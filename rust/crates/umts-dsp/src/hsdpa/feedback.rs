// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, bits::Bit};

const CQI_BITS: usize = 20;
const ACK_BITS: usize = 10;
const CQI_WORDS: usize = 32;
const CQI_BASIS: [u8; CQI_BITS] = [
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 16, 16, 16, 16, 16,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cqi(u8);

impl Cqi {
    pub fn new(value: u8) -> Result<Self, DspError> {
        if value > 30 {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    // TS 25.212 Table 15A uses the binary representation of CQI+1, LSB first.
    pub fn encode(self) -> [Bit; CQI_BITS] {
        std::array::from_fn(|index| {
            Bit::from(
                !((self.0 + 1) & CQI_BASIS[index])
                    .count_ones()
                    .is_multiple_of(2),
            )
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarqAck {
    Ack,
    Nack,
    Preamble,
    Postamble,
}

impl HarqAck {
    pub fn encode(self) -> [Bit; ACK_BITS] {
        // TS 25.212 Table 15, not the MIMO or dual-cell codebook.
        std::array::from_fn(|index| {
            Bit::from(match self {
                Self::Ack => true,
                Self::Nack => false,
                Self::Preamble => index % 3 == 2,
                Self::Postamble => index % 3 == 1,
            })
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FeedbackEvidence<T> {
    pub candidate: Option<T>,
    pub normalized_correlation: f64,
    pub normalized_margin: f64,
    pub mean_soft_power: f64,
}

// Soft values are coherent real projections with positive values representing zero.
// These results are evidence, not a power-calibrated DTX or HARQ admission decision.
pub fn decode_harq_ack(input: &[f32; ACK_BITS]) -> Result<FeedbackEvidence<HarqAck>, DspError> {
    let words = [
        HarqAck::Ack,
        HarqAck::Nack,
        HarqAck::Preamble,
        HarqAck::Postamble,
    ];
    let codewords = words.map(HarqAck::encode);
    let ranked = rank(input, words.len(), |index, bit| codewords[index][bit])?;
    Ok(FeedbackEvidence {
        candidate: ranked.candidate.map(|index| words[index]),
        normalized_correlation: ranked.normalized_correlation,
        normalized_margin: ranked.normalized_margin,
        mean_soft_power: ranked.mean_soft_power,
    })
}

pub fn decode_cqi(input: &[f32; CQI_BITS]) -> Result<FeedbackEvidence<Cqi>, DspError> {
    // Retain the reserved all-zero information word as a competing hypothesis.
    if let Some(ranked) = rank_cqi_exact(input) {
        return Ok(cqi_evidence(ranked));
    }
    let ranked = rank(input, CQI_WORDS, |word, bit| {
        Bit::from(
            !((word as u8) & CQI_BASIS[bit])
                .count_ones()
                .is_multiple_of(2),
        )
    })?;
    Ok(cqi_evidence(ranked))
}

fn cqi_evidence(ranked: FeedbackEvidence<usize>) -> FeedbackEvidence<Cqi> {
    FeedbackEvidence {
        candidate: ranked
            .candidate
            .and_then(|word| word.checked_sub(1))
            .map(|value| Cqi(value as u8)),
        normalized_correlation: ranked.normalized_correlation,
        normalized_margin: ranked.normalized_margin,
        mean_soft_power: ranked.mean_soft_power,
    }
}

fn rank_cqi_exact(input: &[f32; CQI_BITS]) -> Option<FeedbackEvidence<usize>> {
    let mut minimum = u32::MAX;
    let mut maximum = 0;
    for value in input {
        let bits = value.to_bits() & 0x7fff_ffff;
        if bits != 0 {
            let exponent = ((bits >> 23) & 255).max(1);
            minimum = minimum.min(exponent);
            maximum = maximum.max(exponent);
        }
    }
    // Within 20 exponent steps and 20 terms every intermediate sum fits in 49 bits, so f64
    // butterflies are exact.
    // Wider ranges, erasures and nonfinite input keep ordered ranking.
    if maximum == 255 || minimum == u32::MAX || maximum - minimum > 20 {
        return None;
    }
    let power = input
        .iter()
        .map(|&value| f64::from(value).powi(2))
        .sum::<f64>();
    let mut scores = [0.0_f64; CQI_WORDS];
    for (&value, basis) in input.iter().zip(CQI_BASIS) {
        scores[usize::from(basis)] += f64::from(value);
    }
    let mut width = 1;
    while width < CQI_WORDS {
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
    let normalization = (power * CQI_BITS as f64).sqrt();
    Some(FeedbackEvidence {
        candidate: (best > second).then_some(winner),
        normalized_correlation: best / normalization,
        normalized_margin: (best - second) / normalization,
        mean_soft_power: power / CQI_BITS as f64,
    })
}

pub(super) fn rank<const N: usize>(
    input: &[f32; N],
    words: usize,
    bit: impl Fn(usize, usize) -> Bit,
) -> Result<FeedbackEvidence<usize>, DspError> {
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
    let mut best = f64::NEG_INFINITY;
    let mut second = f64::NEG_INFINITY;
    let mut candidate = 0;
    for word in 0..words {
        let score = input
            .iter()
            .enumerate()
            .map(|(index, &value)| {
                f64::from(value)
                    * if bit(word, index) == Bit::Zero {
                        1.0
                    } else {
                        -1.0
                    }
            })
            .sum::<f64>();
        if score > best {
            second = best;
            best = score;
            candidate = word;
        } else if score > second {
            second = score;
        }
    }
    let normalization = (power * N as f64).sqrt();
    Ok(FeedbackEvidence {
        candidate: (best > second).then_some(candidate),
        normalized_correlation: best / normalization,
        normalized_margin: (best - second) / normalization,
        mean_soft_power: power / N as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordered_cqi(input: &[f32; CQI_BITS]) -> FeedbackEvidence<Cqi> {
        cqi_evidence(
            rank(input, CQI_WORDS, |word, bit| {
                Bit::from(
                    !((word as u8) & CQI_BASIS[bit])
                        .count_ones()
                        .is_multiple_of(2),
                )
            })
            .unwrap(),
        )
    }

    fn assert_same_cqi(input: &[f32; CQI_BITS]) {
        let actual = decode_cqi(input).unwrap();
        let expected = ordered_cqi(input);
        assert_eq!(actual.candidate, expected.candidate);
        assert_eq!(
            actual.normalized_correlation.to_bits(),
            expected.normalized_correlation.to_bits()
        );
        assert_eq!(
            actual.normalized_margin.to_bits(),
            expected.normalized_margin.to_bits()
        );
        assert_eq!(
            actual.mean_soft_power.to_bits(),
            expected.mean_soft_power.to_bits()
        );
    }

    #[test]
    fn cqi_transform_matches_ordered_scores_and_range_fallback_exactly() {
        let mut state = 0x8d31_2b47_u32;
        for exponent in [0, 1, 2, 100, 127, 230, 254] {
            for spread in [0, 1, 20, 21, 24, 80] {
                for _ in 0..64 {
                    let input = std::array::from_fn(|i| {
                        state ^= state << 13;
                        state ^= state >> 17;
                        state ^= state << 5;
                        let e = (exponent + i as u32 % (spread + 1)).min(254);
                        f32::from_bits((state & 0x807f_ffff) | (e << 23))
                    });
                    assert_same_cqi(&input);
                }
            }
        }
        for word in 0..CQI_WORDS {
            for gain in [f32::from_bits(1), f32::MIN_POSITIVE, 0.001, 1.0, f32::MAX] {
                let input = std::array::from_fn(|i| {
                    if ((word as u8) & CQI_BASIS[i]).count_ones().is_multiple_of(2) {
                        gain
                    } else {
                        -gain
                    }
                });
                assert_same_cqi(&input);
            }
        }
        for value in [0.0, -0.0] {
            assert_same_cqi(&[value; CQI_BITS]);
        }
        for index in 0..CQI_BITS {
            let mut input = [0.0; CQI_BITS];
            input[index] = 1.0;
            assert_same_cqi(&input);
            input[index] = f32::INFINITY;
            assert!(decode_cqi(&input).is_err());
            input[index] = f32::NAN;
            assert!(decode_cqi(&input).is_err());
        }
        let mut boundary = [1.0; CQI_BITS];
        boundary[0] = 2.0_f32.powi(20);
        assert!(rank_cqi_exact(&boundary).is_some());
        boundary[0] = 2.0_f32.powi(21);
        assert!(rank_cqi_exact(&boundary).is_none());
    }

    #[test]
    fn cqi_basis_and_reserved_word_are_explicit() {
        let cqi_zero = Cqi::new(0).unwrap().encode();
        assert_eq!(
            cqi_zero,
            std::array::from_fn(|index| { Bit::from(index < 15 && index % 2 == 0) })
        );
        assert_eq!(Cqi::new(15).unwrap().encode(), [Bit::One; CQI_BITS]);
        assert!(decode_cqi(&[1.0; CQI_BITS]).unwrap().candidate.is_none());
        assert!(Cqi::new(31).is_err());
    }

    #[test]
    fn coherent_cqi_and_ack_evidence_is_gain_invariant() {
        for gain in [0.001_f32, 1.0, 1000.0] {
            for value in 0..=30 {
                let cqi = Cqi::new(value).unwrap();
                let soft = cqi
                    .encode()
                    .map(|bit| if bit == Bit::Zero { gain } else { -gain });
                let evidence = decode_cqi(&soft).unwrap();
                assert_eq!(evidence.candidate, Some(cqi));
                assert!((evidence.normalized_correlation - 1.0).abs() < 1e-12);
                assert!(evidence.normalized_margin > 0.0);
            }
            for ack in [
                HarqAck::Ack,
                HarqAck::Nack,
                HarqAck::Preamble,
                HarqAck::Postamble,
            ] {
                let soft = ack
                    .encode()
                    .map(|bit| if bit == Bit::Zero { gain } else { -gain });
                let evidence = decode_harq_ack(&soft).unwrap();
                assert_eq!(evidence.candidate, Some(ack));
                assert!((evidence.normalized_correlation - 1.0).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn zero_or_invalid_evidence_never_produces_an_ack() {
        assert!(
            decode_harq_ack(&[0.0; ACK_BITS])
                .unwrap()
                .candidate
                .is_none()
        );
        assert!(decode_cqi(&[0.0; CQI_BITS]).unwrap().candidate.is_none());
        assert!(decode_harq_ack(&[f32::NAN; ACK_BITS]).is_err());
        assert!(decode_cqi(&[f32::INFINITY; CQI_BITS]).is_err());
    }
}
