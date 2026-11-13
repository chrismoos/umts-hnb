// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, interleaving::Rel99Tti};

mod turbo;

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Puncture,
    Repeat,
}

#[derive(Clone, Copy, Debug)]
pub struct Parameters {
    pub initial: u64,
    pub plus: u64,
    pub minus: u64,
}

pub struct RateMatching {
    input_length: usize,
    source: Vec<usize>,
}

impl RateMatching {
    // TS 25.212 4.2.7.2.1.3 uses the maximum TFS input, not the current format, for fixed-position matching.
    pub fn downlink_convolutional_fixed(
        input: usize,
        maximum: usize,
        delta: i64,
        maximum_output: std::num::NonZeroUsize,
    ) -> Result<Self, DspError> {
        if maximum == 0
            || input > maximum
            || delta < 0 && delta.unsigned_abs() as u128 > maximum as u128
        {
            return Err(DspError::InvalidRateMatching);
        }
        let change = (input as u128 * u128::from(delta.unsigned_abs())).div_ceil(maximum as u128);
        let change = usize::try_from(change).map_err(|_| DspError::Capacity)?;
        let output = if delta < 0 {
            input.checked_sub(change)
        } else {
            input.checked_add(change)
        }
        .filter(|output| *output <= maximum_output.get())
        .ok_or(DspError::Capacity)?;
        let plus = u64::try_from(maximum)
            .ok()
            .and_then(|bits| bits.checked_mul(2))
            .ok_or(DspError::Capacity)?;
        let minus = delta
            .unsigned_abs()
            .checked_mul(2)
            .ok_or(DspError::Capacity)?;
        Self::new(
            input,
            output,
            if delta < 0 {
                Operation::Puncture
            } else {
                Operation::Repeat
            },
            Parameters {
                initial: 1,
                plus,
                minus,
            },
        )
    }

    pub fn output_bits(&self) -> usize {
        self.source.len()
    }
    // TS 25.212 4.2.7.1.2.1 changes the initial error across frames of one uplink TTI.
    pub fn uplink_convolutional(
        input_length: usize,
        output_length: usize,
        interval: Rel99Tti,
        frame: usize,
    ) -> Result<Self, DspError> {
        let frames = interval.frames();
        if frame >= frames {
            return Err(DspError::InvalidLength);
        }
        if input_length == 0 {
            return Self::downlink_convolutional_flexible(input_length, output_length);
        }
        let n = input_length as i128;
        let delta = output_length as i128 - n;
        let remainder = delta.rem_euclid(n);
        let q = if remainder != 0 && 2 * remainder <= n {
            (n + remainder - 1) / remainder
        } else {
            -(n / (n - remainder))
        };
        let mut a = q.unsigned_abs();
        let mut b = frames as u128;
        while b != 0 {
            let r = a % b;
            a = b;
            b = r;
        }
        let scaled_q = q * frames as i128 + if q % 2 == 0 { a as i128 } else { 0 };
        let mut s = [0_u128; 8];
        for x in 0..frames {
            let value = (x as i128 * scaled_q)
                .div_euclid(frames as i128)
                .unsigned_abs();
            s[(value % frames as u128) as usize] = value / frames as u128;
        }
        let column = interval.permutation()[frame];
        let plus = u64::try_from(input_length)
            .ok()
            .and_then(|n| n.checked_mul(2))
            .ok_or(DspError::InvalidLength)?;
        let minus = u64::try_from(input_length.abs_diff(output_length))
            .ok()
            .and_then(|n| n.checked_mul(2))
            .ok_or(DspError::InvalidLength)?;
        let initial = ((s[column] * delta.unsigned_abs()) % (input_length as u128) * 2 + 1)
            % u128::from(plus);
        Self::new(
            input_length,
            output_length,
            if delta < 0 {
                Operation::Puncture
            } else {
                Operation::Repeat
            },
            Parameters {
                initial: initial as u64,
                plus,
                minus,
            },
        )
    }

    // TS 25.212 4.2.7.2.2.2. Allocation of bits across TrCHs is a separate step.
    pub fn downlink_convolutional_flexible(
        input_length: usize,
        output_length: usize,
    ) -> Result<Self, DspError> {
        let plus = u64::try_from(input_length)
            .ok()
            .and_then(|length| length.checked_mul(2))
            .ok_or(DspError::InvalidLength)?;
        let minus = u64::try_from(input_length.abs_diff(output_length))
            .ok()
            .and_then(|delta| delta.checked_mul(2))
            .ok_or(DspError::InvalidLength)?;
        if input_length == 0 {
            return if output_length == 0 {
                Ok(Self {
                    input_length,
                    source: Vec::new(),
                })
            } else {
                Err(DspError::InvalidLength)
            };
        }
        Self::new(
            input_length,
            output_length,
            if output_length < input_length {
                Operation::Puncture
            } else {
                Operation::Repeat
            },
            Parameters {
                initial: 1,
                plus,
                minus,
            },
        )
    }

    // TS 25.212 4.2.7.5. UL/DL and turbo/convolutional parameter planners differ.
    // The expected length bounds setup allocation and rejects inconsistent plans.
    pub fn new(
        input_length: usize,
        output_length: usize,
        operation: Operation,
        parameters: Parameters,
    ) -> Result<Self, DspError> {
        if parameters.plus == 0
            || parameters.initial > parameters.plus
            || matches!(operation, Operation::Puncture) && parameters.minus > parameters.plus
        {
            return Err(DspError::InvalidRateMatching);
        }
        let mut source = Vec::new();
        source
            .try_reserve_exact(output_length)
            .map_err(|_| DspError::Capacity)?;
        let mut error = i128::from(parameters.initial);
        let plus = i128::from(parameters.plus);
        let minus = i128::from(parameters.minus);
        for index in 0..input_length {
            error -= minus;
            let copies = match operation {
                Operation::Puncture if error <= 0 => {
                    error += plus;
                    0
                }
                Operation::Puncture => 1,
                Operation::Repeat => {
                    let extra = if error <= 0 { (-error) / plus + 1 } else { 0 };
                    error += extra * plus;
                    usize::try_from(extra + 1).map_err(|_| DspError::InvalidLength)?
                }
            };
            if copies > output_length - source.len() {
                return Err(DspError::InvalidRateMatching);
            }
            source.extend(std::iter::repeat_n(index, copies));
        }
        if source.len() != output_length {
            return Err(DspError::InvalidRateMatching);
        }
        Ok(Self {
            input_length,
            source,
        })
    }

    pub fn apply<T: Copy>(&self, input: &[T], output: &mut [T]) -> Result<(), DspError> {
        if input.len() != self.input_length || output.len() != self.source.len() {
            return Err(DspError::InvalidLength);
        }
        for (value, &index) in output.iter_mut().zip(&self.source) {
            *value = input[index];
        }
        Ok(())
    }

    // Punctured bits have zero LLR. Repeated independent observations add LLRs.
    pub fn combine_llrs(&self, input: &[f32], output: &mut [f32]) -> Result<(), DspError> {
        if input.len() != self.source.len() || output.len() != self.input_length {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        output.fill(0.0);
        self.combine_values(
            |index| f64::from(input[index]),
            |index, value| {
                let value = value as f32;
                if !value.is_finite() {
                    return Err(DspError::NonFiniteEvidence);
                }
                output[index] = value;
                Ok(())
            },
        )
    }

    pub fn combine_soft(&self, input: &[f64], output: &mut [f64]) -> Result<(), DspError> {
        if input.len() != self.source.len() || output.len() != self.input_length {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        output.fill(0.0);
        self.combine_values(
            |index| input[index],
            |index, value| {
                output[index] = value;
                Ok(())
            },
        )
    }

    fn combine_values(
        &self,
        input: impl Fn(usize) -> f64,
        mut emit: impl FnMut(usize, f64) -> Result<(), DspError>,
    ) -> Result<(), DspError> {
        let mut position = 0;
        while position < self.source.len() {
            let source = self.source[position];
            let mut sum = 0.0_f64;
            while position < self.source.len() && self.source[position] == source {
                sum += input(position);
                position += 1;
            }
            if !sum.is_finite() {
                return Err(DspError::NonFiniteEvidence);
            }
            emit(source, sum)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uplink_frame_phases_match_literal_patterns_and_signed_q_rounding() {
        let input: Vec<_> = (0..8).collect();
        let punctured = [[1, 3, 4, 6, 7], [0, 2, 4, 5, 7]];
        let repeated = [
            [0, 0, 1, 2, 2, 3, 4, 5, 5, 6, 7],
            [0, 1, 1, 2, 3, 3, 4, 5, 6, 6, 7],
            [0, 1, 2, 2, 3, 4, 4, 5, 6, 7, 7],
        ];
        for frame in 0..8 {
            let mut output = [0; 5];
            RateMatching::uplink_convolutional(8, 5, Rel99Tti::Ms80, frame)
                .unwrap()
                .apply(&input, &mut output)
                .unwrap();
            assert_eq!(output, punctured[usize::from((4..7).contains(&frame))]);
            let mut output = [0; 11];
            RateMatching::uplink_convolutional(8, 11, Rel99Tti::Ms80, frame)
                .unwrap()
                .apply(&input, &mut output)
                .unwrap();
            assert_eq!(output, repeated[[0, 1, 2, 0, 1, 2, 0, 1][frame]]);
        }
        assert!(RateMatching::uplink_convolutional(560, 600, Rel99Tti::Ms20, 2).is_err());
        assert!(RateMatching::uplink_convolutional(0, 1, Rel99Tti::Ms10, 0).is_err());
        assert!(RateMatching::uplink_convolutional(usize::MAX, 1, Rel99Tti::Ms10, 0).is_err());
    }

    fn indices(input: usize, output: usize) -> Vec<usize> {
        let plan = RateMatching::downlink_convolutional_flexible(input, output).unwrap();
        let source: Vec<_> = (0..input).collect();
        let mut result = vec![0; output];
        plan.apply(&source, &mut result).unwrap();
        result
    }

    #[test]
    fn downlink_patterns_match_explicit_error_accumulator_vectors() {
        assert_eq!(indices(8, 5), [1, 3, 4, 6, 7]);
        assert_eq!(indices(8, 11), [0, 0, 1, 2, 2, 3, 4, 5, 5, 6, 7]);
        assert_eq!(indices(3, 10), [0, 0, 0, 0, 1, 1, 1, 2, 2, 2]);
        assert_eq!(indices(4, 4), [0, 1, 2, 3]);
        assert!(indices(4, 0).is_empty());
        assert!(indices(0, 0).is_empty());
    }

    #[test]
    fn phase_parameters_change_the_pattern_and_preserve_zero_initial_error() {
        let source: Vec<_> = (0..8).collect();
        let mut output = [0; 5];
        RateMatching::new(
            8,
            5,
            Operation::Puncture,
            Parameters {
                initial: 8,
                plus: 16,
                minus: 6,
            },
        )
        .unwrap()
        .apply(&source, &mut output)
        .unwrap();
        assert_eq!(output, [0, 2, 4, 5, 7]);
        let plan = RateMatching::new(
            2,
            3,
            Operation::Repeat,
            Parameters {
                initial: 0,
                plus: 4,
                minus: 0,
            },
        )
        .unwrap();
        let mut output = [0; 3];
        plan.apply(&[3, 4], &mut output).unwrap();
        assert_eq!(output, [3, 3, 4]);
    }

    #[test]
    fn inverse_inserts_erasures_and_combines_without_averaging() {
        let mut output = [9.0; 8];
        RateMatching::downlink_convolutional_flexible(8, 5)
            .unwrap()
            .combine_llrs(&[1.0, -2.0, 3.0, -4.0, 5.0], &mut output)
            .unwrap();
        assert_eq!(output, [0.0, 1.0, 0.0, -2.0, 3.0, 0.0, -4.0, 5.0]);
        let mut output = [0.0; 3];
        RateMatching::downlink_convolutional_flexible(3, 10)
            .unwrap()
            .combine_llrs(
                &[1.0, 2.0, -1.0, 3.0, -1.0, -2.0, -3.0, 2.0, 3.0, 4.0],
                &mut output,
            )
            .unwrap();
        assert_eq!(output, [5.0, -6.0, 9.0]);
    }

    #[test]
    fn setup_rejects_bad_parameters_lengths_and_unbounded_repetition() {
        for (initial, plus, minus) in [(1, 0, 1), (5, 4, 1), (1, 4, 5)] {
            assert!(
                RateMatching::new(
                    8,
                    5,
                    Operation::Puncture,
                    Parameters {
                        initial,
                        plus,
                        minus
                    }
                )
                .is_err()
            );
        }
        assert!(
            RateMatching::new(
                1,
                1,
                Operation::Repeat,
                Parameters {
                    initial: 1,
                    plus: 1,
                    minus: u64::MAX
                }
            )
            .is_err()
        );
        assert!(RateMatching::downlink_convolutional_flexible(0, 1).is_err());
        assert!(
            RateMatching::new(
                8,
                4,
                Operation::Puncture,
                Parameters {
                    initial: 1,
                    plus: 16,
                    minus: 6
                }
            )
            .is_err()
        );
        let plan = RateMatching::downlink_convolutional_flexible(2, 4).unwrap();
        assert_eq!(plan.apply(&[0], &mut [0; 4]), Err(DspError::InvalidLength));
        assert_eq!(
            plan.combine_llrs(&[f32::NAN; 4], &mut [0.0; 2]),
            Err(DspError::NonFiniteEvidence)
        );
        assert_eq!(
            plan.combine_llrs(&[f32::MAX; 4], &mut [0.0; 2]),
            Err(DspError::NonFiniteEvidence)
        );
    }

    #[test]
    fn precomputed_plans_match_literal_spec_recurrence_across_sizes_and_phases() {
        for length in [1, 2, 3, 8, 81, 244, 512] {
            for delta in [-1_i64, 0, 1, 3, 29, 1024] {
                if delta < 0 && length == 1 {
                    continue;
                }
                for initial in [0, 1, length, 2 * length] {
                    let parameters = Parameters {
                        initial: initial as u64,
                        plus: (2 * length) as u64,
                        minus: 2 * delta.unsigned_abs(),
                    };
                    let operation = if delta < 0 {
                        Operation::Puncture
                    } else {
                        Operation::Repeat
                    };
                    let mut expected = Vec::new();
                    let mut error = i128::from(parameters.initial);
                    for index in 0..length {
                        error -= i128::from(parameters.minus);
                        match operation {
                            Operation::Puncture => {
                                if error <= 0 {
                                    error += i128::from(parameters.plus);
                                } else {
                                    expected.push(index);
                                }
                            }
                            Operation::Repeat => {
                                expected.push(index);
                                while error <= 0 {
                                    expected.push(index);
                                    error += i128::from(parameters.plus);
                                }
                            }
                        }
                    }
                    let plan =
                        RateMatching::new(length, expected.len(), operation, parameters).unwrap();
                    let mut actual = vec![0; expected.len()];
                    plan.apply(&(0..length).collect::<Vec<_>>(), &mut actual)
                        .unwrap();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}
