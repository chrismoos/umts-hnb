// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, bits::Bit};

pub const CODEWORD_BITS: usize = 32;
const INFORMATION_BITS: u32 = 10;
pub const UPLINK_CODE_BITS: usize = 30;
const MAX_COMBINATIONS: usize = 1 << INFORMATION_BITS;
const TRANSFORM_MIN_CANDIDATES: usize = 256;
const FLOAT_EXPONENT_SHIFT: u32 = f32::MANTISSA_DIGITS - 1;
const FLOAT_EXPONENT_MASK: u32 = 0xff;
const EXACT_EXPONENT_SPAN: u32 = f64::MANTISSA_DIGITS
    - f32::MANTISSA_DIGITS
    - UPLINK_CODE_BITS.next_power_of_two().trailing_zeros();

// TS 25.212 Table 8. Bit n in a row is M_i,n, with a0 the TFCI's LSB.
const BASIS_ROWS: [u16; CODEWORD_BITS] = [
    0b0000100001,
    0b0001100010,
    0b1000100011,
    0b1101100100,
    0b1000100101,
    0b0100100110,
    0b0010100111,
    0b0110101000,
    0b0111101001,
    0b1101101010,
    0b1100101011,
    0b0110101100,
    0b1010101101,
    0b1001101110,
    0b1111101111,
    0b0011110001,
    0b1011110010,
    0b0101110011,
    0b1110110100,
    0b1010110101,
    0b1100110110,
    0b1110110111,
    0b0010111000,
    0b1011111001,
    0b0101111010,
    0b1001111011,
    0b0100111100,
    0b0011111101,
    0b0111111110,
    0b1111111111,
    0b0000100000,
    0b0001110000,
];

fn transform_scores(input: &[f32; UPLINK_CODE_BITS]) -> Option<[f64; MAX_COMBINATIONS]> {
    let mut minimum = u32::MAX;
    let mut maximum = 0;
    for &value in input {
        if value != 0.0 {
            let exponent = ((value.to_bits() >> FLOAT_EXPONENT_SHIFT) & FLOAT_EXPONENT_MASK).max(1);
            minimum = minimum.min(exponent);
            maximum = maximum.max(exponent);
        }
    }
    // Every partial sum must fit exactly in f64, otherwise retain sequential scalar rounding.
    if maximum.saturating_sub(minimum) > EXACT_EXPONENT_SPAN {
        return None;
    }
    let mut scores = [0.0; MAX_COMBINATIONS];
    for (&value, &row) in input.iter().zip(&BASIS_ROWS) {
        scores[usize::from(row)] += f64::from(value);
    }
    let mut half = 1;
    while half < MAX_COMBINATIONS {
        for block in scores.chunks_exact_mut(2 * half) {
            let (left, right) = block.split_at_mut(half);
            for (left, right) in left.iter_mut().zip(right) {
                let sum = *left + *right;
                *right = *left - *right;
                *left = sum;
            }
        }
        half *= 2;
    }
    Some(scores)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tfci(u16);

impl Tfci {
    pub fn new(index: u16) -> Result<Self, DspError> {
        if index >= 1 << INFORMATION_BITS {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(index))
    }

    pub fn index(self) -> u16 {
        self.0
    }

    pub fn encode(self) -> [Bit; CODEWORD_BITS] {
        BASIS_ROWS.map(|row| Bit::from(!(row & self.0).count_ones().is_multiple_of(2)))
    }
}

pub struct TfciDecoder {
    candidates: Vec<(Tfci, [f32; UPLINK_CODE_BITS])>,
}

#[derive(Clone, Copy, Debug)]
pub struct TfciDecision {
    pub tfci: Tfci,
    pub normalized_score: f64,
    pub normalized_margin: Option<f64>,
}

impl TfciDecoder {
    pub fn storage_bytes(&self) -> usize {
        size_of::<Self>() + self.heap_bytes()
    }
    pub(crate) fn heap_bytes(&self) -> usize {
        self.candidates.capacity() * std::mem::size_of::<(Tfci, [f32; UPLINK_CODE_BITS])>()
    }
    pub fn new(allowed: &[Tfci]) -> Result<Self, DspError> {
        if allowed.is_empty() || allowed.len() > MAX_COMBINATIONS {
            return Err(DspError::InvalidCode);
        }
        let mut seen = [false; MAX_COMBINATIONS];
        let mut candidates = Vec::with_capacity(allowed.len());
        for tfci in allowed {
            let index = usize::from(tfci.index());
            if seen[index] {
                return Err(DspError::InvalidCode);
            }
            seen[index] = true;
            let word = tfci.encode();
            candidates.push((
                *tfci,
                std::array::from_fn(|bit| if word[bit] == Bit::Zero { 1.0 } else { -1.0 }),
            ));
        }
        Ok(Self { candidates })
    }

    // TS 25.212 4.3.5.1 transmits only b0..b29 on uplink. Candidates are restricted to the configured TFCS.
    // Scores rank those candidates and do not establish access admission. Exact ties and silence have no decision.
    pub fn decode(
        &self,
        input: &[f32; UPLINK_CODE_BITS],
    ) -> Result<Option<TfciDecision>, DspError> {
        self.decode_preferring(input, &[], 0.0)
    }

    /// Returns a `preferred` TFCI scoring within `tolerance` of the winner, keeping the winner's
    /// margin.
    pub fn decode_preferring(
        &self,
        input: &[f32; UPLINK_CODE_BITS],
        preferred: &[Tfci],
        tolerance: f64,
    ) -> Result<Option<TfciDecision>, DspError> {
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        let energy = input
            .iter()
            .map(|value| f64::from(*value).powi(2))
            .sum::<f64>();
        if energy == 0.0 {
            return Ok(None);
        }
        let mut best = None;
        let mut second = f64::NEG_INFINITY;
        let mut preferred_best: Option<(Tfci, f64)> = None;
        let scores = (self.candidates.len() >= TRANSFORM_MIN_CANDIDATES)
            .then(|| transform_scores(input))
            .flatten();
        for (tfci, word) in &self.candidates {
            let score = scores.as_ref().map_or_else(
                || {
                    word.iter()
                        .zip(input)
                        .map(|(sign, value)| f64::from(*sign) * f64::from(*value))
                        .sum::<f64>()
                },
                |scores| scores[usize::from(tfci.index())],
            );
            let score = if score == 0.0 { 0.0 } else { score };
            if preferred.contains(tfci) && preferred_best.is_none_or(|(_, old)| score > old) {
                preferred_best = Some((*tfci, score));
            }
            if best.is_none_or(|(_, old)| score > old) {
                second = best.map_or(f64::NEG_INFINITY, |(_, score)| score);
                best = Some((*tfci, score));
            } else if score > second {
                second = score;
            }
        }
        let (tfci, score) = best.unwrap();
        if score == second {
            return Ok(None);
        }
        let normalization = (UPLINK_CODE_BITS as f64 * energy).sqrt();
        let margin = second.is_finite().then(|| (score - second) / normalization);
        if let Some((candidate, candidate_score)) = preferred_best
            && candidate != tfci
            && tolerance > 0.0
            && (score - candidate_score) / normalization <= tolerance
        {
            return Ok(Some(TfciDecision {
                tfci: candidate,
                normalized_score: candidate_score / normalization,
                normalized_margin: margin,
            }));
        }
        Ok(Some(TfciDecision {
            tfci,
            normalized_score: score / normalization,
            normalized_margin: margin,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_preserves_every_scalar_score_across_exact_exponent_span() {
        for exponent in [0, 1, 24, 120, 230] {
            for seed in 0..16_u32 {
                let input = std::array::from_fn(|index| {
                    let mantissa = (seed.wrapping_mul(7919) ^ (index as u32).wrapping_mul(104729))
                        & ((1 << FLOAT_EXPONENT_SHIFT) - 1);
                    let sign = ((index as u32 + seed) & 1) << 31;
                    f32::from_bits(
                        sign | ((exponent + index as u32 % 25) << FLOAT_EXPONENT_SHIFT) | mantissa,
                    )
                });
                let scores = transform_scores(&input).unwrap();
                for (index, &score) in scores.iter().enumerate() {
                    let word = Tfci::new(index as u16).unwrap().encode();
                    let expected: f64 = input
                        .iter()
                        .zip(word)
                        .map(|(&value, bit)| {
                            if bit == Bit::Zero {
                                f64::from(value)
                            } else {
                                -f64::from(value)
                            }
                        })
                        .sum();
                    assert_eq!(
                        score, expected,
                        "exponent={exponent} seed={seed} word={index}"
                    );
                }
            }
        }
        let mut wide = [1.0; UPLINK_CODE_BITS];
        wide[0] = f32::MIN_POSITIVE;
        assert!(transform_scores(&wide).is_none());
        wide[0] = f32::MAX;
        assert!(transform_scores(&wide).is_none());
    }

    #[test]
    fn transformed_codebook_preserves_preference_and_ties() {
        let allowed: Vec<_> = (0..MAX_COMBINATIONS as u16)
            .rev()
            .map(|v| Tfci::new(v).unwrap())
            .collect();
        let decoder = TfciDecoder::new(&allowed).unwrap();
        let winner = Tfci::new(3).unwrap();
        let preferred = Tfci::new(5).unwrap();
        let input = std::array::from_fn(|i| {
            if winner.encode()[i] == Bit::Zero {
                1.0
            } else {
                -1.0
            }
        });
        assert_eq!(decoder.decode(&input).unwrap().unwrap().tfci, winner);
        assert_eq!(
            decoder
                .decode_preferring(&input, &[preferred], 2.0)
                .unwrap()
                .unwrap()
                .tfci,
            preferred
        );
        assert_eq!(
            decoder
                .decode_preferring(&input, &[preferred], 0.0)
                .unwrap()
                .unwrap()
                .tfci,
            winner
        );
        let mut tie = [0.0; UPLINK_CODE_BITS];
        tie[0] = 1.0;
        assert!(decoder.decode(&tie).unwrap().is_none());
        assert!(decoder.decode(&[0.0; UPLINK_CODE_BITS]).unwrap().is_none());
    }

    #[test]
    fn preferred_codeword_wins_only_within_tolerance() {
        let allowed: Vec<_> = (0..8).map(|index| Tfci::new(index).unwrap()).collect();
        let decoder = TfciDecoder::new(&allowed).unwrap();
        let word = allowed[3].encode();
        // Blend codeword 3 with a little of codeword 5 so 3 wins by a small normalized margin.
        let other = allowed[5].encode();
        let mut input = [0.0f32; UPLINK_CODE_BITS];
        for ((value, a), b) in input.iter_mut().zip(&word).zip(&other) {
            let sign = |bit: &Bit| if *bit == Bit::Zero { 1.0f32 } else { -1.0 };
            *value = sign(a) + 0.6 * sign(b);
        }
        let plain = decoder.decode(&input).unwrap().unwrap();
        assert_eq!(plain.tfci.index(), 3);
        let margin = plain.normalized_margin.unwrap();
        let preferred = [allowed[5]];
        let aided = decoder
            .decode_preferring(&input, &preferred, margin * 1.5)
            .unwrap()
            .unwrap();
        assert_eq!(aided.tfci.index(), 5);
        assert_eq!(aided.normalized_margin, plain.normalized_margin);
        let strict = decoder
            .decode_preferring(&input, &preferred, margin * 0.5)
            .unwrap()
            .unwrap();
        assert_eq!(strict.tfci.index(), 3);
        let unrelated = [allowed[0]];
        let far = decoder
            .decode_preferring(&input, &unrelated, 0.05)
            .unwrap()
            .unwrap();
        assert_eq!(far.tfci.index(), 3);
    }

    #[test]
    fn soft_uplink_decoder_is_restricted_to_configured_combinations() {
        let allowed = [0, 1, 3, 45, 1023].map(|index| Tfci::new(index).unwrap());
        let decoder = TfciDecoder::new(&allowed).unwrap();
        for tfci in allowed {
            for scale in [1e-20_f32, 0.1, 1e20] {
                let word = tfci.encode();
                let input = std::array::from_fn(|bit| {
                    if bit % 11 == 0 {
                        0.0
                    } else {
                        (if word[bit] == Bit::Zero { 1.0 } else { -1.0 }) * scale
                    }
                });
                let decision = decoder.decode(&input).unwrap().unwrap();
                assert_eq!(decision.tfci, tfci);
                assert!(decision.normalized_score > 0.9);
                assert!(decision.normalized_margin.unwrap() > 0.1);
            }
        }
        assert!(decoder.decode(&[0.0; UPLINK_CODE_BITS]).unwrap().is_none());
        assert!(decoder.decode(&[f32::NAN; UPLINK_CODE_BITS]).is_err());
        assert!(TfciDecoder::new(&[]).is_err());
        assert!(TfciDecoder::new(&[allowed[0], allowed[0]]).is_err());
        let singleton = TfciDecoder::new(&[allowed[0]]).unwrap();
        let decision = singleton.decode(&[1.0; UPLINK_CODE_BITS]).unwrap().unwrap();
        assert!(decision.normalized_margin.is_none());
    }

    #[test]
    fn ambiguous_soft_tfci_has_no_arbitrary_winner() {
        let choices = [Tfci::new(0).unwrap(), Tfci::new(1).unwrap()];
        let decoder = TfciDecoder::new(&choices).unwrap();
        let a = choices[0].encode();
        let b = choices[1].encode();
        let input = std::array::from_fn(|bit| {
            if a[bit] == b[bit] {
                if a[bit] == Bit::Zero { 1.0 } else { -1.0 }
            } else {
                0.0
            }
        });
        assert!(decoder.decode(&input).unwrap().is_none());
    }

    fn bits(text: &str) -> Vec<Bit> {
        text.bytes().map(|byte| Bit::from(byte == b'1')).collect()
    }

    #[test]
    fn wire_basis_columns_match_the_spec_table() {
        let columns = [
            "10101010101010110101010101010100",
            "01100110011001101100110011001100",
            "00011110000111100011110000111100",
            "00000001111111100000001111111100",
            "00000000000000011111111111111101",
            "11111111111111111111111111111111",
            "01010000110001111100000111011101",
            "00000011100110111011011100011100",
            "00010101111100100110110010101100",
            "00111000011011101011110101000100",
        ];
        for (column, expected) in columns.iter().enumerate() {
            assert_eq!(
                Tfci::new(1 << column).unwrap().encode().as_slice(),
                bits(expected)
            );
        }
        assert!(Tfci::new(1 << INFORMATION_BITS).is_err());
    }

    #[test]
    fn full_codebook_is_linear_and_has_minimum_distance_twelve() {
        let words: Vec<_> = (0..1 << INFORMATION_BITS)
            .map(|index| {
                Tfci::new(index)
                    .unwrap()
                    .encode()
                    .iter()
                    .enumerate()
                    .fold(0_u32, |word, (bit, value)| word | ((*value as u32) << bit))
            })
            .collect();
        assert_eq!(words[0], 0);
        assert_eq!(
            words[1..].iter().map(|word| word.count_ones()).min(),
            Some(12)
        );
        for index in 0..words.len() {
            assert_eq!(words[index] ^ words[0x2a5], words[index ^ 0x2a5]);
        }
    }
}
