// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    convolutional::{self, CodeRate, TerminatedDecoder},
    crc::TransportCrc,
    interleaving::{Interleaver, Rel99Tti},
};

// TS 25.302 table A.1, FDD BCH. TS 25.212 4.2.13.6 permits one TB and one TrCH.
pub const PAYLOAD_BITS: usize = 246;
const CRC_BITS: usize = 16;
const BLOCK_BITS: usize = PAYLOAD_BITS + CRC_BITS;
pub const FRAME_BITS: usize = 270;
pub const TTI_BITS: usize = 2 * FRAME_BITS;

struct BroadcastInterleaving {
    first: Interleaver,
    second: Interleaver,
}

impl BroadcastInterleaving {
    fn new() -> Result<Self, DspError> {
        Ok(Self {
            first: Interleaver::first(Rel99Tti::Ms20, TTI_BITS)?,
            second: Interleaver::second(FRAME_BITS)?,
        })
    }
}

pub struct BroadcastEncoder {
    interleaving: BroadcastInterleaving,
}

impl BroadcastEncoder {
    pub fn new() -> Result<Self, DspError> {
        Ok(Self {
            interleaving: BroadcastInterleaving::new()?,
        })
    }

    // Two consecutive P-CCPCH radio frames, before QPSK mapping and spreading.
    pub fn encode(
        &self,
        payload: &[Bit; PAYLOAD_BITS],
        output: &mut [Bit; TTI_BITS],
    ) -> Result<(), DspError> {
        let mut block = [Bit::Zero; BLOCK_BITS];
        block[..PAYLOAD_BITS].copy_from_slice(payload);
        TransportCrc::Crc16.write(payload, &mut block[PAYLOAD_BITS..])?;
        let mut coded = [Bit::Zero; TTI_BITS];
        convolutional::encode(CodeRate::Half, &block, &mut coded)?;
        let mut frames = [Bit::Zero; TTI_BITS];
        self.interleaving.first.interleave(&coded, &mut frames)?;
        for (input, output) in frames
            .chunks_exact(FRAME_BITS)
            .zip(output.chunks_exact_mut(FRAME_BITS))
        {
            self.interleaving.second.interleave(input, output)?;
        }
        Ok(())
    }
}

pub struct BroadcastDecoder {
    interleaving: BroadcastInterleaving,
    decoder: TerminatedDecoder,
}

impl BroadcastDecoder {
    pub fn new() -> Result<Self, DspError> {
        Ok(Self {
            interleaving: BroadcastInterleaving::new()?,
            decoder: TerminatedDecoder::new(CodeRate::Half),
        })
    }

    pub fn decode(
        &mut self,
        input: &[f32; TTI_BITS],
    ) -> Result<Option<[Bit; PAYLOAD_BITS]>, DspError> {
        let mut frames = [0.0; TTI_BITS];
        for (input, output) in input
            .chunks_exact(FRAME_BITS)
            .zip(frames.chunks_exact_mut(FRAME_BITS))
        {
            self.interleaving.second.deinterleave(input, output)?;
        }
        let mut coded = [0.0; TTI_BITS];
        self.interleaving.first.deinterleave(&frames, &mut coded)?;
        let mut block = [Bit::Zero; BLOCK_BITS];
        self.decoder.decode(&coded, &mut block)?;
        if !TransportCrc::Crc16.verify(&block[..PAYLOAD_BITS], &block[PAYLOAD_BITS..])? {
            return Ok(None);
        }
        let mut payload = [Bit::Zero; PAYLOAD_BITS];
        payload.copy_from_slice(&block[..PAYLOAD_BITS]);
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn independent_reference(payload: &[Bit; PAYLOAD_BITS], corrupt_crc: bool) -> [Bit; TTI_BITS] {
        let mut dividend = [0_u8; BLOCK_BITS];
        for (out, &bit) in dividend.iter_mut().zip(payload) {
            *out = bit as u8;
        }
        let polynomial = 0x11021_u32;
        for index in 0..PAYLOAD_BITS {
            if dividend[index] != 0 {
                for offset in 0..=CRC_BITS {
                    dividend[index + offset] ^= ((polynomial >> (CRC_BITS - offset)) & 1) as u8;
                }
            }
        }
        let mut block = vec![Bit::Zero; BLOCK_BITS + 8];
        block[..PAYLOAD_BITS].copy_from_slice(payload);
        for offset in 0..CRC_BITS {
            block[PAYLOAD_BITS + offset] = Bit::from(dividend[BLOCK_BITS - 1 - offset] != 0);
        }
        if corrupt_crc {
            block[PAYLOAD_BITS] = Bit::from(block[PAYLOAD_BITS] == Bit::Zero);
        }
        let mut coded = [Bit::Zero; TTI_BITS];
        for time in 0..block.len() {
            for (branch, polynomial) in [0o561_u16, 0o753].iter().enumerate() {
                let mut parity = false;
                for delay in 0..=8 {
                    if time >= delay && polynomial & (1 << (8 - delay)) != 0 {
                        parity ^= block[time - delay] == Bit::One;
                    }
                }
                coded[2 * time + branch] = Bit::from(parity);
            }
        }
        let columns = [
            0, 20, 10, 5, 15, 25, 3, 13, 23, 8, 18, 28, 1, 11, 21, 6, 16, 26, 4, 14, 24, 19, 9, 29,
            12, 2, 7, 22, 27, 17,
        ];
        let mut result = [Bit::Zero; TTI_BITS];
        for frame in 0..2 {
            for (column_position, &column) in columns.iter().enumerate() {
                for row in 0..9 {
                    result[frame * FRAME_BITS + column_position * 9 + row] =
                        coded[2 * (row * 30 + column) + frame];
                }
            }
        }
        result
    }

    #[test]
    fn broadcast_transport_matches_independent_crc_polynomial_and_frame_mapping() {
        let encoder = BroadcastEncoder::new().unwrap();
        let mut decoder = BroadcastDecoder::new().unwrap();
        for seed in [0, 1, 7, 19] {
            let payload = std::array::from_fn(|i| Bit::from((i * seed + i / 5) % 7 < 3));
            let expected = independent_reference(&payload, false);
            let mut actual = [Bit::Zero; TTI_BITS];
            encoder.encode(&payload, &mut actual).unwrap();
            assert_eq!(actual, expected);
            let evidence = expected.map(|bit| if bit == Bit::Zero { 2.0 } else { -2.0 });
            assert_eq!(decoder.decode(&evidence).unwrap(), Some(payload));
        }
    }

    #[test]
    fn bad_crc_is_not_delivered_even_with_perfect_codeword_likelihoods() {
        let payload = [Bit::One; PAYLOAD_BITS];
        let evidence = independent_reference(&payload, true)
            .map(|bit| if bit == Bit::Zero { 10.0 } else { -10.0 });
        assert_eq!(
            BroadcastDecoder::new().unwrap().decode(&evidence).unwrap(),
            None
        );
        assert_eq!(
            BroadcastDecoder::new()
                .unwrap()
                .decode(&[f32::NAN; TTI_BITS]),
            Err(DspError::NonFiniteEvidence)
        );
    }

    #[test]
    fn sparse_erasures_and_sign_errors_recover_the_same_broadcast_payload() {
        let payload = std::array::from_fn(|i| Bit::from((i * 13 + i / 3) % 11 < 5));
        let coded = independent_reference(&payload, false);
        let mut evidence = coded.map(|bit| if bit == Bit::Zero { 3.0 } else { -3.0 });
        for (index, value) in evidence.iter_mut().enumerate() {
            if index % 43 == 0 {
                *value = 0.0;
            }
            if index % 97 == 0 {
                *value = -*value;
            }
        }
        assert_eq!(
            BroadcastDecoder::new().unwrap().decode(&evidence).unwrap(),
            Some(payload)
        );
    }
}
