// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    crc::TransportCrc,
    turbo::{TurboBlockFormat, TurboBlockPlan, TurboInterleaver},
};
use std::{num::NonZeroUsize, sync::Arc};
use umts_types::bits::Bits;

const CRC_BITS: usize = 24;
const UNMASKED_CRC_BITS: usize = 8;
const IDENTITY_BITS: usize = 16;
const SCRAMBLER_PERIOD: usize = (1 << u16::BITS) - 1;
static SCRAMBLING_SEQUENCE: [Bit; SCRAMBLER_PERIOD] = scrambling_sequence();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrcMethod {
    Method1,
    Method2 { h_rnti: u16 },
}

pub struct TransportEncoder {
    payload_bits: NonZeroUsize,
    coded_bits: usize,
    packed: Vec<u8>,
    plan: TurboBlockPlan,
    interleaver: Arc<TurboInterleaver>,
    block: Vec<Bit>,
}

impl TransportEncoder {
    /// Requested retained storage excludes allocator overhead and construction scratch.
    pub fn required_storage_bytes(
        payload_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<usize, DspError> {
        let joined = payload_bits
            .get()
            .checked_add(CRC_BITS)
            .ok_or(DspError::Capacity)?;
        let plan = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: joined,
                blocks: 1,
                crc: TransportCrc::None,
            },
            maximum_coded_bits,
        )?;
        plan.code_block_bits()
            .checked_mul(std::mem::size_of::<Bit>())
            .and_then(|bytes| bytes.checked_add(payload_bits.get().div_ceil(8)))
            .and_then(|bytes| {
                plan.code_block_bits()
                    .checked_mul(std::mem::size_of::<usize>())
                    .and_then(|indices| bytes.checked_add(indices))
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    std::mem::size_of::<TurboInterleaver>() + 2 * std::mem::size_of::<usize>(),
                )
            })
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or(DspError::Capacity)
    }

    pub fn new(
        payload_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        let joined = payload_bits
            .get()
            .checked_add(CRC_BITS)
            .ok_or(DspError::Capacity)?;
        let plan = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: joined,
                blocks: 1,
                crc: TransportCrc::None,
            },
            maximum_coded_bits,
        )?;
        Ok(Self {
            payload_bits,
            coded_bits: plan.coded_bits(),
            packed: vec![0; payload_bits.get().div_ceil(8)],
            interleaver: plan.interleaver()?.ok_or(DspError::InvalidCode)?,
            block: vec![Bit::Zero; plan.code_block_bits()],
            plan,
        })
    }

    pub fn coded_bits(&self) -> usize {
        self.coded_bits
    }

    // TS 25.212 4.5.1–4.5.3 applies scrambling before segmentation and turbo coding.
    pub fn encode(
        &mut self,
        input: &[Bit],
        method: CrcMethod,
        scramble: bool,
        output: &mut [Bit],
    ) -> Result<(), DspError> {
        if input.len() != self.payload_bits.get() || output.len() != self.coded_bits {
            return Err(DspError::InvalidLength);
        }
        self.packed.fill(0);
        for (index, bit) in input.iter().enumerate() {
            self.packed[index / 8] |= (*bit as u8) << (7 - index % 8);
        }
        self.encode_payload(method, scramble, output)
    }

    /// Encode an MSB-first payload view without expanding or copying a full bit vector.
    pub fn encode_packed(
        &mut self,
        input: Bits<'_>,
        method: CrcMethod,
        scramble: bool,
        output: &mut [Bit],
    ) -> Result<(), DspError> {
        if input.len() != self.payload_bits.get() || output.len() != self.coded_bits {
            return Err(DspError::InvalidLength);
        }
        input
            .copy_to(&mut self.packed)
            .map_err(|_| DspError::InvalidLength)?;
        self.encode_payload(method, scramble, output)
    }

    fn encode_payload(
        &mut self,
        method: CrcMethod,
        scramble: bool,
        output: &mut [Bit],
    ) -> Result<(), DspError> {
        let payload = self.payload_bits.get();
        let remainder = TransportCrc::Crc24.remainder_bytes(&self.packed, payload)?;
        let mut parity: [Bit; CRC_BITS] =
            std::array::from_fn(|index| Bit::from(remainder & (1 << index) != 0));
        if let CrcMethod::Method2 { h_rnti } = method {
            for (index, bit) in parity[UNMASKED_CRC_BITS..].iter_mut().enumerate() {
                *bit =
                    Bit::from((*bit as u16 ^ ((h_rnti >> (IDENTITY_BITS - 1 - index)) & 1)) != 0);
            }
        }
        let width = self.plan.code_block_bits();
        let mut offset = 0;
        for (index, coded) in output
            .chunks_exact_mut(crate::turbo::coded_bits(width)?)
            .enumerate()
        {
            let filler = if index == 0 {
                self.plan.filler_bits()
            } else {
                0
            };
            let count = width - filler;
            self.block[..filler].fill(Bit::Zero);
            let block = &mut self.block[filler..];
            let data = payload.saturating_sub(offset).min(count);
            unpack(&self.packed, offset, &mut block[..data]);
            if data < count {
                let first = offset + data - payload;
                block[data..].copy_from_slice(&parity[first..first + count - data]);
            }
            if scramble {
                scramble_from(block, offset % SCRAMBLER_PERIOD);
            }
            crate::turbo::encode(&self.interleaver, &self.block, coded)?;
            offset += count;
        }
        Ok(())
    }
}

const BYTE_BITS: usize = u8::BITS as usize;
static UNPACKED_BYTES: [[Bit; BYTE_BITS]; 1 << BYTE_BITS] = unpacked_bytes();

const fn unpacked_bytes() -> [[Bit; BYTE_BITS]; 1 << BYTE_BITS] {
    let mut table = [[Bit::Zero; BYTE_BITS]; 1 << BYTE_BITS];
    let mut value = 0;
    while value < table.len() {
        let mut bit = 0;
        while bit < BYTE_BITS {
            if value & (1 << (BYTE_BITS - 1 - bit)) != 0 {
                table[value][bit] = Bit::One;
            }
            bit += 1;
        }
        value += 1;
    }
    table
}

fn unpack(packed: &[u8], start: usize, output: &mut [Bit]) {
    let lead = ((BYTE_BITS - start % BYTE_BITS) % BYTE_BITS).min(output.len());
    let (head, body) = output.split_at_mut(lead);
    for (index, bit) in head.iter_mut().enumerate() {
        let source = start + index;
        let mask = 1 << (BYTE_BITS - 1 - source % BYTE_BITS);
        *bit = Bit::from(packed[source / BYTE_BITS] & mask != 0);
    }
    let first = (start + lead) / BYTE_BITS;
    let whole = body.len() / BYTE_BITS;
    let (body, tail) = body.split_at_mut(whole * BYTE_BITS);
    for (bits, &byte) in body
        .chunks_exact_mut(BYTE_BITS)
        .zip(&packed[first..first + whole])
    {
        bits.copy_from_slice(&UNPACKED_BYTES[usize::from(byte)]);
    }
    if !tail.is_empty() {
        let bits = &UNPACKED_BYTES[usize::from(packed[first + whole])];
        tail.copy_from_slice(&bits[..tail.len()]);
    }
}

fn scramble_from(mut bits: &mut [Bit], mut phase: usize) {
    while !bits.is_empty() {
        let run = (SCRAMBLER_PERIOD - phase).min(bits.len());
        let (head, tail) = std::mem::take(&mut bits).split_at_mut(run);
        for (bit, &mask) in head
            .iter_mut()
            .zip(&SCRAMBLING_SEQUENCE[phase..phase + run])
        {
            *bit = Bit::from((*bit as u8 ^ mask as u8) != 0);
        }
        bits = tail;
        phase = 0;
    }
}

// TS 25.212 4.5.1a seeds y'(1)=1 and taps delays 11, 13, 14, 16.
const fn scrambling_sequence() -> [Bit; SCRAMBLER_PERIOD] {
    let mut sequence = [Bit::Zero; SCRAMBLER_PERIOD];
    let mut history: u16 = 0;
    let mut index = 0;
    while index < sequence.len() {
        let next = if index == 0 {
            1
        } else {
            ((history >> 10) ^ (history >> 12) ^ (history >> 13) ^ (history >> 15)) & 1
        };
        sequence[index] = if next == 0 { Bit::Zero } else { Bit::One };
        history = (history << 1) | next;
        index += 1;
    }
    sequence
}

#[cfg(test)]
fn scramble_bits(bits: &mut [Bit]) {
    for block in bits.chunks_mut(SCRAMBLER_PERIOD) {
        for (bit, &mask) in block.iter_mut().zip(&SCRAMBLING_SEQUENCE) {
            *bit = Bit::from((*bit as u8 ^ mask as u8) != 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_scramble(bits: &mut [Bit]) {
        let mut history = 0u16;
        for (index, bit) in bits.iter_mut().enumerate() {
            let next = if index == 0 {
                1
            } else {
                ((history >> 10) ^ (history >> 12) ^ (history >> 13) ^ (history >> 15)) & 1
            };
            *bit = Bit::from((*bit as u16 ^ next) != 0);
            history = (history << 1) | next;
        }
    }

    #[test]
    fn packed_offsets_tails_segmentation_and_scrambling_match_generic_reference() {
        for length in [1, 7, 8, 9, 137, 5090, 5091, 20251, 27952, 65536] {
            let maximum = NonZeroUsize::new(250_000).unwrap();
            let mut encoder =
                TransportEncoder::new(NonZeroUsize::new(length).unwrap(), maximum).unwrap();
            let packed_pointer = encoder.packed.as_ptr();
            let block_pointer = encoder.block.as_ptr();
            let plan = TurboBlockPlan::new(
                TurboBlockFormat {
                    block_bits: length + CRC_BITS,
                    blocks: 1,
                    crc: TransportCrc::None,
                },
                maximum,
            )
            .unwrap();
            let mut reference = crate::turbo::TurboBlockEncoder::new(plan).unwrap();
            for offset in 0..8 {
                let bytes: Vec<u8> = (0..(length + offset).div_ceil(8))
                    .map(|i| (i * 73 + 0x9d) as u8)
                    .collect();
                let view = Bits::new(&bytes, length + offset)
                    .unwrap()
                    .subspan(offset..offset + length)
                    .unwrap();
                let input: Vec<_> = (0..length)
                    .map(|i| Bit::from(view.bit(i).unwrap()))
                    .collect();
                for method in [
                    CrcMethod::Method1,
                    CrcMethod::Method2 { h_rnti: 0 },
                    CrcMethod::Method2 { h_rnti: 0xa35c },
                    CrcMethod::Method2 { h_rnti: u16::MAX },
                ] {
                    for scramble in [false, true] {
                        let mut joined = input.clone();
                        let mut parity = [Bit::Zero; CRC_BITS];
                        TransportCrc::Crc24.write(&input, &mut parity).unwrap();
                        if let CrcMethod::Method2 { h_rnti } = method {
                            for i in 0..IDENTITY_BITS {
                                parity[UNMASKED_CRC_BITS + i] = Bit::from(
                                    (parity[UNMASKED_CRC_BITS + i] as u16
                                        ^ ((h_rnti >> (IDENTITY_BITS - 1 - i)) & 1))
                                        != 0,
                                );
                            }
                        }
                        joined.extend_from_slice(&parity);
                        if scramble {
                            reference_scramble(&mut joined);
                        }
                        let mut expected = vec![Bit::Zero; encoder.coded_bits()];
                        reference.encode(&joined, &mut expected).unwrap();
                        let mut actual = vec![Bit::One; expected.len()];
                        encoder
                            .encode_packed(view, method, scramble, &mut actual)
                            .unwrap();
                        assert_eq!(
                            actual, expected,
                            "length={length} offset={offset} method={method:?} scramble={scramble}"
                        );
                        assert_eq!(encoder.packed.as_ptr(), packed_pointer);
                        assert_eq!(encoder.block.as_ptr(), block_pointer);
                    }
                }
            }
            let charged = TransportEncoder::required_storage_bytes(
                NonZeroUsize::new(length).unwrap(),
                maximum,
            )
            .unwrap();
            assert_eq!(
                charged,
                std::mem::size_of::<TransportEncoder>()
                    + encoder.packed.len()
                    + encoder.block.len() * std::mem::size_of::<Bit>()
                    + encoder.interleaver.len() * std::mem::size_of::<usize>()
                    + std::mem::size_of::<TurboInterleaver>()
                    + 2 * std::mem::size_of::<usize>()
            );
        }
    }

    #[test]
    fn packed_invalid_lengths_do_not_modify_coded_output() {
        let mut encoder = TransportEncoder::new(
            NonZeroUsize::new(137).unwrap(),
            NonZeroUsize::new(1000).unwrap(),
        )
        .unwrap();
        let bytes = [0xff; 18];
        let mut output = vec![Bit::One; encoder.coded_bits()];
        assert_eq!(
            encoder.encode_packed(
                Bits::new(&bytes, 136).unwrap(),
                CrcMethod::Method1,
                true,
                &mut output
            ),
            Err(DspError::InvalidLength)
        );
        assert!(output.iter().all(|bit| *bit == Bit::One));
        let n = output.len() - 1;
        assert_eq!(
            encoder.encode_packed(
                Bits::new(&bytes, 137).unwrap(),
                CrcMethod::Method1,
                true,
                &mut output[..n]
            ),
            Err(DspError::InvalidLength)
        );
        assert!(output.iter().all(|bit| *bit == Bit::One));
    }

    #[test]
    fn shared_scrambling_sequence_matches_recurrence_across_period_boundaries() {
        let input: Vec<_> = (0..SCRAMBLER_PERIOD * 2 + 33)
            .map(|i| Bit::from((i * 17 + i / 7) % 5 < 2))
            .collect();
        let mut expected = input.clone();
        reference_scramble(&mut expected);
        for length in [
            0,
            1,
            137,
            20275,
            27976,
            SCRAMBLER_PERIOD - 1,
            SCRAMBLER_PERIOD,
            SCRAMBLER_PERIOD + 1,
            input.len(),
        ] {
            let mut actual = input[..length].to_vec();
            scramble_bits(&mut actual);
            assert_eq!(actual, expected[..length]);
            scramble_bits(&mut actual);
            assert_eq!(actual, input[..length]);
        }
    }

    #[test]
    fn complete_transport_output_preserves_crc_methods_and_scrambling_bypass() {
        for length in [137, 20251, 27952] {
            let input: Vec<_> = (0..length).map(|i| Bit::from(i % 3 == 1)).collect();
            let mut encoder = TransportEncoder::new(
                NonZeroUsize::new(length).unwrap(),
                NonZeroUsize::new(100_000).unwrap(),
            )
            .unwrap();
            for method in [CrcMethod::Method1, CrcMethod::Method2 { h_rnti: 0xa35c }] {
                for scramble in [false, true] {
                    let mut joined = vec![Bit::Zero; length + CRC_BITS];
                    joined[..length].copy_from_slice(&input);
                    TransportCrc::Crc24
                        .write(&input, &mut joined[length..])
                        .unwrap();
                    if let CrcMethod::Method2 { h_rnti } = method {
                        for index in 0..IDENTITY_BITS {
                            let bit = &mut joined[length + UNMASKED_CRC_BITS + index];
                            *bit = Bit::from(
                                (*bit as u16 ^ (h_rnti >> (IDENTITY_BITS - 1 - index)) & 1) != 0,
                            );
                        }
                    }
                    if scramble {
                        reference_scramble(&mut joined);
                    }
                    let mut actual = vec![Bit::Zero; encoder.coded_bits()];
                    let mut expected = actual.clone();
                    encoder
                        .encode(&input, method, scramble, &mut actual)
                        .unwrap();
                    let plan = TurboBlockPlan::new(
                        TurboBlockFormat {
                            block_bits: joined.len(),
                            blocks: 1,
                            crc: TransportCrc::None,
                        },
                        NonZeroUsize::new(100_000).unwrap(),
                    )
                    .unwrap();
                    crate::turbo::TurboBlockEncoder::new(plan)
                        .unwrap()
                        .encode(&joined, &mut expected)
                        .unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}
