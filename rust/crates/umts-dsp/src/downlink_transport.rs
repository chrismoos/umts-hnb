// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::{Bit, TxBit},
    convolutional::CodeRate,
    crc::TransportCrc,
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
    transport_blocks::{BlockEncoder, BlockFormat, BlockPlan},
};

const MAX_COMMON_BLOCK_BITS: usize = 4968;
const MAX_COMMON_BLOCKS: usize = 512;
const MAX_COMMON_TRANSPORTS: usize = 8;
const MAX_SECONDARY_FRAME_BITS: usize = 19080;

#[derive(Clone, Copy, Debug)]
pub struct ConvolutionalFormat {
    pub block_bits: usize,
    pub blocks: usize,
    pub crc: TransportCrc,
    pub rate: CodeRate,
    pub interval: Rel99Tti,
    pub rate_matched_bits: usize,
}

pub struct ConvolutionalDownlink {
    format: ConvolutionalFormat,
    payload_bits: usize,
    encoder: BlockEncoder,
    matching: RateMatching,
    first: Interleaver,
    coded: Vec<Bit>,
    matched: Vec<Bit>,
}

impl ConvolutionalDownlink {
    // TS 25.212 Figure 2, flexible-position TrCH through first interleaving.
    // rate_matched_bits must come from TFCS allocation, not the current packet's free space.
    pub fn new(format: ConvolutionalFormat) -> Result<Self, DspError> {
        let frames = format.interval.frames();
        if format.block_bits == 0
            || format.block_bits > MAX_COMMON_BLOCK_BITS
            || format.blocks > MAX_COMMON_BLOCKS
            || !format.rate_matched_bits.is_multiple_of(frames)
            || format.rate_matched_bits / frames > MAX_SECONDARY_FRAME_BITS
            || (format.blocks == 0) != (format.rate_matched_bits == 0)
        {
            return Err(DspError::InvalidLength);
        }
        let payload_bits = format.block_bits * format.blocks;
        let plan = BlockPlan::new(
            BlockFormat {
                block_bits: format.block_bits,
                blocks: format.blocks,
                crc: format.crc,
                rate: format.rate,
            },
            std::num::NonZeroUsize::MAX,
        )?;
        let matching = RateMatching::downlink_convolutional_flexible(
            plan.coded_bits(),
            format.rate_matched_bits,
        )?;
        let first = Interleaver::first(format.interval, format.rate_matched_bits)?;
        Ok(Self {
            format,
            payload_bits,
            encoder: BlockEncoder::new(plan),
            matching,
            first,
            coded: vec![Bit::Zero; plan.coded_bits()],
            matched: vec![Bit::Zero; format.rate_matched_bits],
        })
    }

    pub fn frame_bits(&self) -> usize {
        self.format.rate_matched_bits / self.format.interval.frames()
    }

    pub fn encode(&mut self, payload: &[Bit], frames: &mut [Bit]) -> Result<(), DspError> {
        if payload.len() != self.payload_bits || frames.len() != self.format.rate_matched_bits {
            return Err(DspError::InvalidLength);
        }
        self.encoder.encode(payload, &mut self.coded)?;
        self.matching.apply(&self.coded, &mut self.matched)?;
        self.first.interleave(&self.matched, frames)
    }
}

pub struct FlexibleCommonFrame {
    second: Interleaver,
    scratch: Vec<TxBit>,
}

impl FlexibleCommonFrame {
    pub fn new(data_bits: usize) -> Result<Self, DspError> {
        if data_bits == 0 || data_bits > MAX_SECONDARY_FRAME_BITS {
            return Err(DspError::InvalidLength);
        }
        Ok(Self {
            second: Interleaver::second(data_bits)?,
            scratch: vec![TxBit::Dtx; data_bits],
        })
    }

    // TS 25.212 4.2.8/4.2.9.2/4.2.11, one S-CCPCH per CCTrCH.
    // Inputs must follow the signaled PCH/FACH list, not numeric TrCH identity order.
    pub fn compose(&mut self, channels: &[&[Bit]], output: &mut [TxBit]) -> Result<(), DspError> {
        if channels.len() > MAX_COMMON_TRANSPORTS {
            return Err(DspError::InvalidLength);
        }
        let total = channels
            .iter()
            .try_fold(0_usize, |total, channel| total.checked_add(channel.len()))
            .ok_or(DspError::Capacity)?;
        if total > self.scratch.len() || output.len() != self.scratch.len() {
            return Err(DspError::InvalidLength);
        }
        self.scratch.fill(TxBit::Dtx);
        let mut offset = 0;
        for channel in channels {
            for (out, bit) in self.scratch[offset..offset + channel.len()]
                .iter_mut()
                .zip(*channel)
            {
                *out = TxBit::Data(*bit);
            }
            offset += channel.len();
        }
        self.second.interleave(&self.scratch, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(payload: &[Bit], format: ConvolutionalFormat) -> Vec<Bit> {
        let mut joined = Vec::new();
        let width = format.crc.width();
        let polynomial = match format.crc {
            TransportCrc::None => 1,
            TransportCrc::Crc8 => 0x19b,
            TransportCrc::Crc12 => 0x180f_u32,
            TransportCrc::Crc16 => 0x11021,
            TransportCrc::Crc24 => 0x1800063,
        };
        for block in payload.chunks_exact(format.block_bits) {
            let mut division: Vec<_> = block.iter().map(|bit| *bit as u8).collect();
            division.resize(block.len() + width, 0);
            for index in 0..block.len() {
                if division[index] != 0 {
                    for offset in 0..=width {
                        division[index + offset] ^= ((polynomial >> (width - offset)) & 1) as u8;
                    }
                }
            }
            joined.extend_from_slice(block);
            joined.extend(
                division[block.len()..]
                    .iter()
                    .rev()
                    .map(|bit| Bit::from(*bit != 0)),
            );
        }
        if joined.is_empty() {
            return Vec::new();
        }
        let count = joined.len().div_ceil(504);
        let block_bits = joined.len().div_ceil(count);
        let mut padded = vec![Bit::Zero; count * block_bits - joined.len()];
        padded.extend(joined);
        let generators: &[u16] = match format.rate {
            CodeRate::Half => &[0o561, 0o753],
            CodeRate::Third => &[0o557, 0o663, 0o711],
        };
        let mut coded = Vec::new();
        for block in padded.chunks_exact(block_bits) {
            for time in 0..block_bits + 8 {
                for generator in generators {
                    let parity = (0..=8)
                        .filter(|delay| {
                            time >= *delay
                                && time - *delay < block.len()
                                && generator & (1 << (8 - delay)) != 0
                        })
                        .fold(false, |parity, delay| {
                            parity ^ (block[time - delay] == Bit::One)
                        });
                    coded.push(Bit::from(parity));
                }
            }
        }
        let mut matched = Vec::new();
        let mut error = 1_i64;
        let delta =
            (coded.len() as i64 - format.rate_matched_bits as i64).unsigned_abs() as i64 * 2;
        for bit in &coded {
            error -= delta;
            if format.rate_matched_bits < coded.len() {
                if error <= 0 {
                    error += 2 * coded.len() as i64;
                } else {
                    matched.push(*bit);
                }
            } else {
                matched.push(*bit);
                while error <= 0 {
                    matched.push(*bit);
                    error += 2 * coded.len() as i64;
                }
            }
        }
        let columns: &[usize] = match format.interval {
            Rel99Tti::Ms10 => &[0],
            Rel99Tti::Ms20 => &[0, 1],
            Rel99Tti::Ms40 => &[0, 2, 1, 3],
            Rel99Tti::Ms80 => &[0, 4, 2, 6, 1, 5, 3, 7],
        };
        let mut frames = Vec::new();
        for column in columns {
            frames.extend(matched.iter().skip(*column).step_by(columns.len()).copied());
        }
        frames
    }

    #[test]
    fn full_convolutional_chain_matches_independent_spec_construction() {
        for interval in [
            Rel99Tti::Ms10,
            Rel99Tti::Ms20,
            Rel99Tti::Ms40,
            Rel99Tti::Ms80,
        ] {
            for rate in [CodeRate::Half, CodeRate::Third] {
                for crc in [
                    TransportCrc::None,
                    TransportCrc::Crc8,
                    TransportCrc::Crc12,
                    TransportCrc::Crc16,
                    TransportCrc::Crc24,
                ] {
                    for blocks in [0, 1, 2] {
                        let format = ConvolutionalFormat {
                            block_bits: 264,
                            blocks,
                            crc,
                            rate,
                            interval,
                            rate_matched_bits: if blocks == 0 {
                                0
                            } else {
                                570 * interval.frames()
                            },
                        };
                        let payload: Vec<_> = (0..format.block_bits * blocks)
                            .map(|i| Bit::from((i * 7 + i / 5) % 13 < 6))
                            .collect();
                        let mut encoder = ConvolutionalDownlink::new(format).unwrap();
                        let mut actual = vec![Bit::One; format.rate_matched_bits];
                        encoder.encode(&payload, &mut actual).unwrap();
                        assert_eq!(actual, reference(&payload, format));
                        assert_eq!(
                            encoder.frame_bits(),
                            format.rate_matched_bits / interval.frames()
                        );
                        encoder.encode(&payload, &mut actual).unwrap();
                        assert_eq!(actual, reference(&payload, format));
                    }
                }
            }
        }
    }

    #[test]
    fn multiplexing_preserves_signalled_order_and_interleaves_dtx() {
        let paging = [Bit::One, Bit::Zero, Bit::One];
        let forward = [Bit::Zero; 4];
        let mut frame = FlexibleCommonFrame::new(31).unwrap();
        let mut output = vec![TxBit::Data(Bit::One); 31];
        frame.compose(&[&paging, &forward], &mut output).unwrap();
        let mut expected = vec![TxBit::Dtx; 31];
        let indices = [
            0, 30, 20, 10, 5, 15, 25, 3, 13, 23, 8, 18, 28, 1, 11, 21, 6, 16, 26, 4, 14, 24, 19, 9,
            29, 12, 2, 7, 22, 27, 17,
        ];
        let joined: Vec<_> = paging
            .into_iter()
            .chain(forward)
            .map(TxBit::Data)
            .chain(std::iter::repeat(TxBit::Dtx))
            .take(31)
            .collect();
        for (out, index) in expected.iter_mut().zip(indices) {
            *out = joined[index];
        }
        assert_eq!(output, expected);
        frame.compose(&[], &mut output).unwrap();
        assert!(output.iter().all(|bit| *bit == TxBit::Dtx));
        assert!(frame.compose(&[&[Bit::Zero; 32]], &mut output).is_err());
        assert!(output.iter().all(|bit| *bit == TxBit::Dtx));
        assert!(
            frame
                .compose(&[&[] as &[Bit]; MAX_COMMON_TRANSPORTS + 1], &mut output)
                .is_err()
        );
    }

    #[test]
    fn invalid_formats_and_payloads_fail_before_output() {
        let valid = ConvolutionalFormat {
            block_bits: 264,
            blocks: 1,
            crc: TransportCrc::Crc12,
            rate: CodeRate::Half,
            interval: Rel99Tti::Ms20,
            rate_matched_bits: 1140,
        };
        for invalid in [
            ConvolutionalFormat { blocks: 0, ..valid },
            ConvolutionalFormat {
                rate_matched_bits: 1139,
                ..valid
            },
            ConvolutionalFormat {
                rate_matched_bits: usize::MAX,
                ..valid
            },
            ConvolutionalFormat {
                block_bits: usize::MAX,
                ..valid
            },
            ConvolutionalFormat {
                blocks: usize::MAX,
                ..valid
            },
        ] {
            assert!(ConvolutionalDownlink::new(invalid).is_err());
        }
        let mut encoder = ConvolutionalDownlink::new(valid).unwrap();
        let mut output = vec![Bit::One; 1140];
        assert!(encoder.encode(&[Bit::Zero; 263], &mut output).is_err());
        assert!(output.iter().all(|bit| *bit == Bit::One));
    }
}
