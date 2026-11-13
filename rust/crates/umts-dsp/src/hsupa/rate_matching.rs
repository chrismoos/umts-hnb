// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;

use crate::{
    DspError,
    rate_matching::{Operation, Parameters, RateMatching},
};

const STREAMS: usize = 3;
const RV_COUNT: u8 = 4;
const R_MAX: u128 = 2;

pub struct HarqRateMatching {
    coded_bits: usize,
    source: Vec<usize>,
}

impl HarqRateMatching {
    pub(super) fn heap_bytes(&self) -> usize {
        self.source.capacity() * std::mem::size_of::<usize>()
    }
    pub fn new(
        coded_bits: NonZeroUsize,
        physical_bits: NonZeroUsize,
        rv: u8,
        maximum_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        let input = coded_bits.get();
        let output = physical_bits.get();
        if input > maximum_bits.get() || output > maximum_bits.get() {
            return Err(DspError::Capacity);
        }
        if !input.is_multiple_of(STREAMS) || rv >= RV_COUNT {
            return Err(DspError::InvalidRateMatching);
        }
        let width = input / STREAMS;
        let repetition = output > input;
        // TS 25.212 Table 15D maps RV 0..3 to (s,r)=(1,0),(0,0),(1,1),(0,1).
        let systematic_priority = rv.is_multiple_of(2);
        let version = u128::from(rv / 2);
        let systematic = if repetition {
            output / STREAMS
        } else if systematic_priority {
            width.min(output)
        } else {
            output.saturating_sub(2 * width)
        };
        let parity = output - systematic;
        let targets = [systematic, parity / 2, parity.div_ceil(2)];
        let mut lanes = [Vec::new(), Vec::new(), Vec::new()];
        let mut puncturing = [Parameters {
            initial: 1,
            plus: 1,
            minus: 0,
        }; STREAMS];
        for (lane, target) in targets.into_iter().enumerate() {
            let factor = if lane == 1 { 2 } else { 1 };
            let plus = width.checked_mul(factor).ok_or(DspError::Capacity)? as u128;
            let phase = if repetition {
                (u128::from(systematic_priority) + 2 * version) * plus / (2 * R_MAX)
            } else {
                version * plus / R_MAX
            };
            let initial = (width as i128 - phase as i128 - 1).rem_euclid(plus as i128) + 1;
            let parameters = Parameters {
                initial: initial as u64,
                plus: u64::try_from(plus).map_err(|_| DspError::Capacity)?,
                minus: u64::try_from(
                    width
                        .abs_diff(target)
                        .checked_mul(factor)
                        .ok_or(DspError::Capacity)?,
                )
                .map_err(|_| DspError::Capacity)?,
            };
            if repetition {
                let plan = RateMatching::new(width, target, Operation::Repeat, parameters)?;
                let indices: Vec<_> = (0..width).map(|index| STREAMS * index + lane).collect();
                lanes[lane].resize(target, 0);
                plan.apply(&indices, &mut lanes[lane])?;
            } else {
                if parameters.initial > parameters.plus || parameters.minus > parameters.plus {
                    return Err(DspError::InvalidRateMatching);
                }
                puncturing[lane] = parameters;
            }
        }
        let mut source = Vec::with_capacity(output);
        if repetition {
            for position in 0..*targets.iter().max().unwrap() {
                for lane in &lanes {
                    if let Some(&index) = lane.get(position) {
                        source.push(index);
                    }
                }
            }
        } else {
            // TS 25.212 4.8.4.4 removes punctured positions after inverse separation.
            // The HSDPA rectangular collector is not applicable to E-DCH.
            // The three lane recurrences in coded-bit order yield the inverse separation already sorted.
            let mut errors = puncturing.map(|parameters| i128::from(parameters.initial));
            let mut counts = [0; STREAMS];
            for position in 0..width {
                for (lane, (error, parameters)) in errors.iter_mut().zip(&puncturing).enumerate() {
                    *error -= i128::from(parameters.minus);
                    if *error <= 0 {
                        *error += i128::from(parameters.plus);
                    } else {
                        source.push(STREAMS * position + lane);
                        counts[lane] += 1;
                    }
                }
            }
            if counts != targets {
                return Err(DspError::InvalidRateMatching);
            }
        }
        if source.len() != output {
            return Err(DspError::InvalidRateMatching);
        }
        Ok(Self {
            coded_bits: input,
            source,
        })
    }

    pub fn coded_bits(&self) -> usize {
        self.coded_bits
    }
    pub fn physical_bits(&self) -> usize {
        self.source.len()
    }

    pub fn apply<T: Copy>(&self, input: &[T], output: &mut [T]) -> Result<(), DspError> {
        if input.len() != self.coded_bits || output.len() != self.source.len() {
            return Err(DspError::InvalidLength);
        }
        for (value, &source) in output.iter_mut().zip(&self.source) {
            *value = input[source];
        }
        Ok(())
    }

    // The destination is a single transmission's evidence, not the HARQ owner's accumulated state.
    pub fn unmatch(&self, input: &[f32], output: &mut [f32]) -> Result<(), DspError> {
        if input.len() != self.source.len() || output.len() != self.coded_bits {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        output.fill(0.0);
        if input.len() <= self.coded_bits {
            // Puncturing and identity mappings hit each destination at most once. Adding to zero
            // maps negative zero to positive zero as the accumulating path does.
            for (&value, &index) in input.iter().zip(&self.source) {
                output[index] = 0.0 + value;
            }
            return Ok(());
        }
        for (&value, &index) in input.iter().zip(&self.source) {
            let sum = output[index] + value;
            if !sum.is_finite() {
                output.fill(0.0);
                return Err(DspError::NonFiniteEvidence);
            }
            output[index] = sum;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmatching_preserves_error_atomicity_and_repetition_overflow_cleanup() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        for physical in [4, 12, 24] {
            let plan = HarqRateMatching::new(nz(12), nz(physical), 0, nz(100)).unwrap();
            let mut input = vec![1.0; physical];
            let mut output = [17.0; 12];
            input[physical - 1] = f32::NAN;
            assert_eq!(
                plan.unmatch(&input, &mut output),
                Err(DspError::NonFiniteEvidence)
            );
            assert_eq!(output, [17.0; 12]);
            assert_eq!(
                plan.unmatch(&input[..physical - 1], &mut output),
                Err(DspError::InvalidLength)
            );
            assert_eq!(output, [17.0; 12]);
            input.fill(f32::MAX);
            if physical > 12 {
                assert_eq!(
                    plan.unmatch(&input, &mut output),
                    Err(DspError::NonFiniteEvidence)
                );
                assert_eq!(output.map(f32::to_bits), [0; 12]);
            } else {
                plan.unmatch(&input, &mut output).unwrap();
                assert_eq!(output.iter().filter(|v| **v == f32::MAX).count(), physical);
            }
        }
    }

    #[test]
    fn systematic_priority_and_repetition_have_distinct_collectors() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let systematic = HarqRateMatching::new(nz(12), nz(4), 0, nz(100)).unwrap();
        assert_eq!(systematic.source, [0, 3, 6, 9]);
        let parity = HarqRateMatching::new(nz(12), nz(8), 1, nz(100)).unwrap();
        assert_eq!(parity.source, [1, 2, 4, 5, 7, 8, 10, 11]);
        for rv in 0..RV_COUNT {
            let unchanged = HarqRateMatching::new(nz(12), nz(12), rv, nz(100)).unwrap();
            assert_eq!(unchanged.source, (0..12).collect::<Vec<_>>());
            let repeated = HarqRateMatching::new(nz(12), nz(24), rv, nz(100)).unwrap();
            let mut output = [0.0; 12];
            repeated.unmatch(&[1.0; 24], &mut output).unwrap();
            assert_eq!(output, [2.0; 12]);
        }
    }
}

#[cfg(test)]
mod ordered_construction_tests {
    use super::*;

    // Independent oracle: use the general lane matcher and an explicitly sorted
    // inverse separation, rather than the constructor's interleaved recurrence.
    fn reference(input: usize, output: usize, rv: u8) -> Vec<usize> {
        let width = input / STREAMS;
        let repetition = output > input;
        let systematic_priority = rv.is_multiple_of(2);
        let systematic = if repetition {
            output / STREAMS
        } else if systematic_priority {
            width.min(output)
        } else {
            output.saturating_sub(2 * width)
        };
        let parity = output - systematic;
        let targets = [systematic, parity / 2, parity.div_ceil(2)];
        let mut lanes = [Vec::new(), Vec::new(), Vec::new()];
        for (lane, target) in targets.into_iter().enumerate() {
            let factor = if lane == 1 { 2 } else { 1 };
            let plus = (width * factor) as u128;
            let version = u128::from(rv / 2);
            let phase = if repetition {
                (u128::from(systematic_priority) + 2 * version) * plus / (2 * R_MAX)
            } else {
                version * plus / R_MAX
            };
            let initial = (width as i128 - phase as i128 - 1).rem_euclid(plus as i128) + 1;
            let matching = RateMatching::new(
                width,
                target,
                if repetition {
                    Operation::Repeat
                } else {
                    Operation::Puncture
                },
                Parameters {
                    initial: initial as u64,
                    plus: plus as u64,
                    minus: (width.abs_diff(target) * factor) as u64,
                },
            )
            .unwrap();
            let indices: Vec<_> = (0..width).map(|index| STREAMS * index + lane).collect();
            lanes[lane].resize(target, 0);
            matching.apply(&indices, &mut lanes[lane]).unwrap();
        }
        let mut source = Vec::new();
        if repetition {
            for position in 0..*targets.iter().max().unwrap() {
                for lane in &lanes {
                    if let Some(&index) = lane.get(position) {
                        source.push(index);
                    }
                }
            }
        } else {
            source.extend(lanes.into_iter().flatten());
            source.sort_unstable();
        }
        source
    }
    fn compare(input: usize, output: usize, rv: u8) {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let actual =
            HarqRateMatching::new(nz(input), nz(output), rv, nz(input.max(output))).unwrap();
        assert_eq!(
            actual.source,
            reference(input, output, rv),
            "coded={input} physical={output} rv={rv}"
        );
        assert!(actual.source.iter().all(|&index| index < input));
        let evidence: Vec<_> = (0..output)
            .map(|index| {
                [
                    0.0,
                    -0.0,
                    1.0,
                    -1.0,
                    f32::from_bits(1),
                    -f32::from_bits(1),
                    0.25,
                ][index % 7]
            })
            .collect();
        let mut expected = vec![0.0_f32; input];
        for (&value, &index) in evidence.iter().zip(&actual.source) {
            expected[index] += value;
        }
        let mut restored = vec![17.0; input];
        actual.unmatch(&evidence, &mut restored).unwrap();
        assert_eq!(
            restored.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        if output <= input {
            assert!(actual.source.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
    #[test]
    fn ordered_puncturing_matches_general_lane_oracle() {
        for width in 1..=32 {
            for output in 1..=6 * width + 3 {
                for rv in 0..RV_COUNT {
                    compare(STREAMS * width, output, rv);
                }
            }
        }
        for width in [97, 512, 3569, 10719] {
            for output in [
                1,
                width - 1,
                width,
                width + 1,
                2 * width - 1,
                2 * width,
                2 * width + 1,
                3 * width - 1,
                3 * width,
                3 * width + 1,
                5 * width,
            ] {
                for rv in 0..RV_COUNT {
                    compare(STREAMS * width, output, rv);
                }
            }
        }
    }
}
