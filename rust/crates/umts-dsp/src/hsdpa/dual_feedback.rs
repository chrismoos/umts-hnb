// SPDX-License-Identifier: AGPL-3.0-or-later

use super::feedback::{Cqi, FeedbackEvidence, rank};
use crate::{DspError, bits::Bit};

const ACK_BITS: usize = 10;
const CQI_BITS: usize = 20;
const CQI_WIDTH: usize = 5;
const CQI_MASK: usize = (1 << CQI_WIDTH) - 1;
const COMPOSITE_WORDS: usize = 1 << (2 * CQI_WIDTH);
// TS 25.212 Table 15C, columns Mi,0 through Mi,9, with Mi,0 in the LSB.
const BASIS: [usize; CQI_BITS] = [
    0b0000000001,
    0b0000000010,
    0b0000001000,
    0b0000010000,
    0b0000100000,
    0b0010000000,
    0b0100000000,
    0b1000000000,
    0b0111000101,
    0b1110001011,
    0b1100010110,
    0b1000101101,
    0b0001011011,
    0b0010110111,
    0b0101101110,
    0b1011011100,
    0b0110111000,
    0b1101110001,
    0b1011100010,
    0b1111111111,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellAck {
    Ack,
    Nack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DualHarqAck {
    Primary(CellAck),
    Secondary(CellAck),
    Both {
        primary: CellAck,
        secondary: CellAck,
    },
    Preamble,
    Postamble,
}

const ACK_WORDS: [DualHarqAck; 10] = [
    DualHarqAck::Primary(CellAck::Ack),
    DualHarqAck::Primary(CellAck::Nack),
    DualHarqAck::Secondary(CellAck::Ack),
    DualHarqAck::Secondary(CellAck::Nack),
    DualHarqAck::Both {
        primary: CellAck::Ack,
        secondary: CellAck::Ack,
    },
    DualHarqAck::Both {
        primary: CellAck::Ack,
        secondary: CellAck::Nack,
    },
    DualHarqAck::Both {
        primary: CellAck::Nack,
        secondary: CellAck::Ack,
    },
    DualHarqAck::Both {
        primary: CellAck::Nack,
        secondary: CellAck::Nack,
    },
    DualHarqAck::Preamble,
    DualHarqAck::Postamble,
];

impl DualHarqAck {
    pub fn cells(self) -> [Option<CellAck>; 2] {
        match self {
            Self::Primary(ack) => [Some(ack), None],
            Self::Secondary(ack) => [None, Some(ack)],
            Self::Both { primary, secondary } => [Some(primary), Some(secondary)],
            Self::Preamble | Self::Postamble => [None, None],
        }
    }

    pub fn encode(self) -> [Bit; ACK_BITS] {
        // TS 25.212 Table 15C.1, transmitted left to right.
        let word: u16 = match self {
            Self::Primary(CellAck::Ack) => 0b1111111111,
            Self::Primary(CellAck::Nack) => 0b0000000000,
            Self::Secondary(CellAck::Ack) => 0b1111100000,
            Self::Secondary(CellAck::Nack) => 0b0000011111,
            Self::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Ack,
            } => 0b1010101010,
            Self::Both {
                primary: CellAck::Ack,
                secondary: CellAck::Nack,
            } => 0b1100110011,
            Self::Both {
                primary: CellAck::Nack,
                secondary: CellAck::Ack,
            } => 0b0011001100,
            Self::Both {
                primary: CellAck::Nack,
                secondary: CellAck::Nack,
            } => 0b0101010101,
            Self::Preamble => 0b0010010010,
            Self::Postamble => 0b0100100100,
        };
        std::array::from_fn(|index| Bit::from(word & (1 << (ACK_BITS - 1 - index)) != 0))
    }
}

// Call only while the UE uses the active dual-cell codebook. Absence of a cell
// in a decoded word means no detected transport block, not a NACK for that cell.
pub fn decode_harq_ack_dual(
    input: &[f32; ACK_BITS],
) -> Result<FeedbackEvidence<DualHarqAck>, DspError> {
    let codewords = ACK_WORDS.map(DualHarqAck::encode);
    let ranked = rank(input, ACK_WORDS.len(), |word, bit| codewords[word][bit])?;
    Ok(FeedbackEvidence {
        candidate: ranked.candidate.map(|word| ACK_WORDS[word]),
        normalized_correlation: ranked.normalized_correlation,
        normalized_margin: ranked.normalized_margin,
        mean_soft_power: ranked.mean_soft_power,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompositeCqi {
    pub primary: Cqi,
    pub secondary: Cqi,
}

impl CompositeCqi {
    pub fn encode(self) -> [Bit; CQI_BITS] {
        let word = usize::from(self.primary.value() + 1)
            | (usize::from(self.secondary.value() + 1) << CQI_WIDTH);
        std::array::from_fn(|index| {
            Bit::from(!(word & BASIS[index]).count_ones().is_multiple_of(2))
        })
    }
}

pub fn decode_cqi_composite(
    input: &[f32; CQI_BITS],
) -> Result<FeedbackEvidence<CompositeCqi>, DspError> {
    if input.iter().any(|value| !value.is_finite()) {
        return Err(DspError::NonFiniteEvidence);
    }
    let power = input
        .iter()
        .map(|&value| f64::from(value).powi(2))
        .sum::<f64>();
    if power == 0.0 {
        return Ok(FeedbackEvidence {
            candidate: None,
            normalized_correlation: 0.0,
            normalized_margin: 0.0,
            mean_soft_power: 0.0,
        });
    }
    // Walsh transform computes all code correlations in bounded O(10 * 1024) work.
    let mut scores = [0.0_f64; COMPOSITE_WORDS];
    for (&value, basis) in input.iter().zip(BASIS) {
        scores[basis] += f64::from(value);
    }
    let mut width = 1;
    while width < COMPOSITE_WORDS {
        for group in scores.chunks_exact_mut(width * 2) {
            let (left, right) = group.split_at_mut(width);
            for (a, b) in left.iter_mut().zip(right) {
                let sum = *a + *b;
                *b = *a - *b;
                *a = sum;
            }
        }
        width *= 2;
    }
    let mut best = f64::NEG_INFINITY;
    let mut second = f64::NEG_INFINITY;
    let mut winner = 0;
    for (word, score) in scores.into_iter().enumerate() {
        if score > best {
            second = best;
            best = score;
            winner = word;
        } else if score > second {
            second = score;
        }
    }
    // Reserved constituent zero words remain competing hypotheses.
    let primary = winner & CQI_MASK;
    let secondary = winner >> CQI_WIDTH;
    let candidate = if best > second && primary != 0 && secondary != 0 {
        Some(CompositeCqi {
            primary: Cqi::new((primary - 1) as u8)?,
            secondary: Cqi::new((secondary - 1) as u8)?,
        })
    } else {
        None
    };
    let normalization = (power * CQI_BITS as f64).sqrt();
    Ok(FeedbackEvidence {
        candidate,
        normalized_correlation: best / normalization,
        normalized_margin: (best - second) / normalization,
        mean_soft_power: power / CQI_BITS as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_cqi_pair_matches_table_15c_columns_zero_and_five() {
        let cqi = CompositeCqi {
            primary: Cqi::new(0).unwrap(),
            secondary: Cqi::new(0).unwrap(),
        };
        let expected = [1, 0, 0, 0, 1, 0, 0, 0, 1, 1, 0, 0, 1, 0, 1, 0, 1, 0, 1, 0]
            .map(|bit| Bit::from(bit != 0));
        assert_eq!(cqi.encode(), expected);
    }

    #[test]
    fn all_dual_ack_words_and_missing_cell_evidence() {
        for word in ACK_WORDS {
            let soft = word
                .encode()
                .map(|bit| if bit == Bit::Zero { 1.0 } else { -1.0 });
            assert_eq!(decode_harq_ack_dual(&soft).unwrap().candidate, Some(word));
        }
        assert_eq!(
            DualHarqAck::Secondary(CellAck::Ack).cells(),
            [None, Some(CellAck::Ack)]
        );
        assert_eq!(
            decode_harq_ack_dual(&[0.0; ACK_BITS]).unwrap().candidate,
            None
        );
        assert!(decode_harq_ack_dual(&[f32::NAN; ACK_BITS]).is_err());
    }

    #[test]
    fn every_composite_cqi_and_reserved_zero() {
        for primary in 0..=30 {
            for secondary in 0..=30 {
                let cqi = CompositeCqi {
                    primary: Cqi::new(primary).unwrap(),
                    secondary: Cqi::new(secondary).unwrap(),
                };
                let soft = cqi
                    .encode()
                    .map(|bit| if bit == Bit::Zero { 0.25 } else { -0.25 });
                assert_eq!(decode_cqi_composite(&soft).unwrap().candidate, Some(cqi));
            }
        }
        assert_eq!(
            decode_cqi_composite(&[1.0; CQI_BITS]).unwrap().candidate,
            None
        );
        assert_eq!(
            decode_cqi_composite(&[0.0; CQI_BITS]).unwrap().candidate,
            None
        );
        assert!(decode_cqi_composite(&[f32::INFINITY; CQI_BITS]).is_err());
    }
}
