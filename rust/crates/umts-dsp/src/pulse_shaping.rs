// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;

use crate::DspError;

mod vector;
use vector::PhaseTaps;

const ROLL_OFF: f64 = 0.22;
const HISTORY_BLOCK_CHIPS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SamplesPerChip {
    Two,
    Four,
    Eight,
}

impl SamplesPerChip {
    pub fn get(self) -> usize {
        match self {
            Self::Two => 2,
            Self::Four => 4,
            Self::Eight => 8,
        }
    }
}

// TS 25.104 6.8.1, with t expressed in chip durations and the removable poles resolved.
fn root_raised_cosine(t: f64) -> f64 {
    let pi = std::f64::consts::PI;
    if t.abs() < f64::EPSILON {
        return 1.0 + ROLL_OFF * (4.0 / pi - 1.0);
    }
    let scaled = 4.0 * ROLL_OFF * t;
    if (scaled.abs() - 1.0).abs() < 8.0 * f64::EPSILON {
        let phase = pi / (4.0 * ROLL_OFF);
        return ROLL_OFF
            * std::f64::consts::FRAC_1_SQRT_2
            * ((1.0 + 2.0 / pi) * phase.sin() + (1.0 - 2.0 / pi) * phase.cos());
    }
    ((pi * t * (1.0 - ROLL_OFF)).sin() + scaled * (pi * t * (1.0 + ROLL_OFF)).cos())
        / (pi * t * (1.0 - scaled * scaled))
}

pub(crate) fn pulse_taps(
    samples_per_chip: SamplesPerChip,
    span_chips: usize,
) -> Result<Vec<f64>, DspError> {
    if span_chips == 0 || !span_chips.is_multiple_of(2) {
        return Err(DspError::InvalidFilter);
    }
    let rate = samples_per_chip.get();
    let order = span_chips
        .checked_mul(rate)
        .ok_or(DspError::InvalidLength)?;
    let length = order.checked_add(1).ok_or(DspError::InvalidLength)?;
    let mut taps = Vec::new();
    taps.try_reserve_exact(length)
        .map_err(|_| DspError::Capacity)?;
    taps.extend(
        (0..length).map(|i| root_raised_cosine((i as f64 - (order / 2) as f64) / rate as f64)),
    );
    let energy: f64 = taps.iter().map(|value| value * value).sum();
    let scale = (rate as f64 / energy).sqrt();
    for tap in &mut taps {
        *tap *= scale;
    }
    Ok(taps)
}

pub fn maximum_component_gain(
    samples_per_chip: SamplesPerChip,
    span_chips: usize,
) -> Result<f64, DspError> {
    let taps = pulse_taps(samples_per_chip, span_chips)?;
    Ok((0..samples_per_chip.get())
        .map(|phase| {
            taps.iter()
                .skip(phase)
                .step_by(samples_per_chip.get())
                .map(|tap| f64::from(*tap as f32).abs())
                .sum::<f64>()
        })
        .fold(0.0, f64::max))
}

pub struct PulseShaper {
    samples_per_chip: usize,
    delay_samples: usize,
    polyphase: PhaseTaps,
    #[cfg(test)]
    phases: Vec<Vec<f32>>,
    history: Vec<Complex32>,
}

impl PulseShaper {
    // Even span gives an integer-chip group delay. Sum(h^2)=samples_per_chip
    // preserves mean sample power for uncorrelated unit-power input chips.
    pub fn new(samples_per_chip: SamplesPerChip, span_chips: usize) -> Result<Self, DspError> {
        let prototype = pulse_taps(samples_per_chip, span_chips)?;
        let samples_per_chip = samples_per_chip.get();
        let length = prototype.len();
        let delay_samples = (length - 1) / 2;
        let history_length = length.div_ceil(samples_per_chip);
        let mut phases = Vec::with_capacity(samples_per_chip);
        for phase in 0..samples_per_chip {
            let mut taps = Vec::new();
            taps.try_reserve_exact(history_length)
                .map_err(|_| DspError::Capacity)?;
            for tap in 0..history_length {
                taps.push(
                    (prototype
                        .get(tap * samples_per_chip + phase)
                        .copied()
                        .unwrap_or(0.0)) as f32,
                );
            }
            phases.push(taps);
        }
        let history_storage = history_length
            .checked_add(HISTORY_BLOCK_CHIPS)
            .ok_or(DspError::InvalidLength)?;
        let mut history = Vec::new();
        history
            .try_reserve_exact(history_storage)
            .map_err(|_| DspError::Capacity)?;
        history.resize(history_storage, Complex32::default());
        Ok(Self {
            samples_per_chip,
            delay_samples,
            polyphase: PhaseTaps::new(&phases),
            #[cfg(test)]
            phases,
            history,
        })
    }

    pub fn group_delay_samples(&self) -> usize {
        self.delay_samples
    }

    pub fn output_length(&self, chips: usize) -> Result<usize, DspError> {
        chips
            .checked_mul(self.samples_per_chip)
            .ok_or(DspError::InvalidLength)
    }

    // A reset discards waveform history. Use only at an explicit radio-epoch boundary.
    pub fn reset(&mut self) {
        self.history.fill(Complex32::default());
    }

    pub fn process(
        &mut self,
        chips: &[Complex32],
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if output.len() != self.output_length(chips.len())? {
            return Err(DspError::InvalidLength);
        }
        crate::sample_validation::exceeds_component_limit(chips, f32::MAX)?;
        let history_length = self.history.len() - HISTORY_BLOCK_CHIPS;
        for (chips, samples) in chips
            .chunks(HISTORY_BLOCK_CHIPS)
            .zip(output.chunks_mut(HISTORY_BLOCK_CHIPS * self.samples_per_chip))
        {
            let start = HISTORY_BLOCK_CHIPS - chips.len();
            for (destination, &chip) in self.history[start..HISTORY_BLOCK_CHIPS]
                .iter_mut()
                .zip(chips.iter().rev())
            {
                *destination = chip;
            }
            self.polyphase.synthesize_block(
                &self.history[start..HISTORY_BLOCK_CHIPS + history_length],
                samples,
            );
            self.history
                .copy_within(start..start + history_length, HISTORY_BLOCK_CHIPS);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_process(shaper: &mut PulseShaper, chips: &[Complex32], output: &mut [Complex32]) {
        let history = &mut shaper.history[HISTORY_BLOCK_CHIPS..];
        for (&chip, samples) in chips
            .iter()
            .zip(output.chunks_exact_mut(shaper.samples_per_chip))
        {
            history.rotate_right(1);
            history[0] = chip;
            for (sample, taps) in samples.iter_mut().zip(&shaper.phases) {
                *sample = history
                    .iter()
                    .zip(taps)
                    .fold(Complex32::default(), |sum, (&value, &tap)| {
                        sum + value * tap
                    });
            }
        }
    }

    #[test]
    fn phase_lanes_preserve_scalar_bits() {
        const BLOCK_CHIPS: usize = 2560;
        const BLOCKS: usize = 48;
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            for span in [16, 32] {
                let mut scalar = PulseShaper::new(rate, span).unwrap();
                let mut vector = PulseShaper::new(rate, span).unwrap();
                let input: Vec<_> = (0..BLOCK_CHIPS)
                    .map(|i| {
                        let scale = [1e-12_f32, 1.0, 1e12][i % 3];
                        Complex32::new(
                            ((i * 17 % 251) as f32 - 125.5) * scale,
                            ((i * 23 % 127) as f32 - 63.5) * scale,
                        )
                    })
                    .collect();
                let mut expected = vec![Complex32::default(); rate.get() * BLOCK_CHIPS];
                let mut actual = expected.clone();
                for block in 0..BLOCKS {
                    scalar_process(&mut scalar, &input, &mut expected);
                    vector.process(&input, &mut actual).unwrap();
                    for (index, (a, b)) in actual.iter().zip(&expected).enumerate() {
                        assert_eq!(
                            (a.re.to_bits(), a.im.to_bits()),
                            (b.re.to_bits(), b.im.to_bits()),
                            "sps={} span={span} block={block} sample={index}",
                            rate.get()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn removable_poles_have_finite_symmetric_limits() {
        for pole in [0.0, 1.0 / (4.0 * ROLL_OFF), -1.0 / (4.0 * ROLL_OFF)] {
            let exact = root_raised_cosine(pole);
            for delta in [-1e-6, 1e-6] {
                assert!((root_raised_cosine(pole + delta) - exact).abs() < 2e-6);
            }
            assert_eq!(exact, root_raised_cosine(-pole));
        }
    }

    #[test]
    fn polyphase_output_matches_direct_zero_insertion_convolution() {
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            let mut shaper = PulseShaper::new(rate, 16).unwrap();
            let input: Vec<_> = (0..100)
                .map(|i| Complex32::new((i % 7) as f32 - 3.0, (i % 5) as f32 - 2.0))
                .collect();
            let mut actual = vec![Complex32::default(); shaper.output_length(input.len()).unwrap()];
            shaper.process(&input, &mut actual).unwrap();
            let sps = rate.get();
            for (time, &sample) in actual.iter().enumerate() {
                let mut expected = Complex32::default();
                for (i, &chip) in input.iter().enumerate() {
                    if time >= i * sps {
                        let tap = time - i * sps;
                        if let Some(&coefficient) = shaper.phases[tap % sps].get(tap / sps) {
                            expected += chip * coefficient;
                        }
                    }
                }
                assert!((sample - expected).norm() < 3e-6);
            }
        }
    }

    #[test]
    fn arbitrary_block_boundaries_preserve_identical_samples() {
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            for span in [2, 16, 32, 128] {
                check_block_boundaries(rate, span);
            }
        }
    }

    #[test]
    fn parallel_accumulators_preserve_scalar_bits_for_fractional_input() {
        const MULTIPLIER: u32 = 1664525;
        const INCREMENT: u32 = 1013904223;
        const HISTORY_BLOCKS: usize = 16;
        const TAIL_CHIPS: usize = 3;
        let mut seed = 0x8ab4_7921_u32;
        let input: Vec<_> = (0..HISTORY_BLOCK_CHIPS * HISTORY_BLOCKS + TAIL_CHIPS)
            .map(|index| {
                let mut next = || {
                    seed = seed.wrapping_mul(MULTIPLIER).wrapping_add(INCREMENT);
                    (seed as i32 as f32) / i32::MAX as f32
                };
                let scale = if index % 3 == 0 { 8.0 } else { 0.125 };
                Complex32::new(next() * scale, next() * scale)
            })
            .collect();
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            let mut parallel = PulseShaper::new(rate, 32).unwrap();
            let mut scalar = PulseShaper::new(rate, 32).unwrap();
            let mut actual = vec![Complex32::default(); input.len() * rate.get()];
            let mut expected = actual.clone();
            parallel.process(&input, &mut actual).unwrap();
            scalar_process(&mut scalar, &input, &mut expected);
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(actual.re.to_bits(), expected.re.to_bits());
                assert_eq!(actual.im.to_bits(), expected.im.to_bits());
            }
        }
    }

    fn check_block_boundaries(rate: SamplesPerChip, span: usize) {
        let input: Vec<_> = (0..400)
            .map(|i| Complex32::new((i % 3) as f32, (i % 7) as f32))
            .collect();
        let mut full = PulseShaper::new(rate, span).unwrap();
        let mut split = PulseShaper::new(rate, span).unwrap();
        let mut scalar = PulseShaper::new(rate, span).unwrap();
        let mut expected = vec![Complex32::default(); input.len() * rate.get()];
        let mut actual = expected.clone();
        full.process(&input, &mut expected).unwrap();
        scalar_process(&mut scalar, &input, &mut actual);
        assert_eq!(actual, expected);
        let mut start = 0;
        for count in [0, 1, 14, 0, 257, 128] {
            split
                .process(
                    &input[start..start + count],
                    &mut actual[rate.get() * start..rate.get() * (start + count)],
                )
                .unwrap();
            start += count;
        }
        assert_eq!(start, input.len());
        assert_eq!(actual, expected);
        split.reset();
        split.process(&input, &mut actual).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(full.group_delay_samples(), span * rate.get() / 2);
    }

    #[test]
    fn normalized_pulse_has_nyquist_matched_response_and_expected_bandwidth() {
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            let shaper = PulseShaper::new(rate, 32).unwrap();
            let sps = rate.get();
            let taps: Vec<f64> = (0..32 * sps + 1)
                .map(|i| f64::from(shaper.phases[i % sps][i / sps]))
                .collect();
            let energy: f64 = taps.iter().map(|value| value * value).sum();
            assert!((energy - sps as f64).abs() < 1e-6);
            for offset in 1..=12 {
                let response: f64 = taps
                    .iter()
                    .zip(&taps[offset * sps..])
                    .map(|(a, b)| a * b)
                    .sum();
                assert!((response / energy).abs() < 0.001);
            }
            let response = |cycles_per_chip: f64| {
                let sum = taps.iter().enumerate().fold(
                    num_complex::Complex64::default(),
                    |sum, (index, &tap)| {
                        let phase =
                            -std::f64::consts::TAU * cycles_per_chip * index as f64 / sps as f64;
                        sum + num_complex::Complex64::from_polar(tap, phase)
                    },
                );
                sum.norm() / sps as f64
            };
            assert!((response(0.1) - 1.0).abs() < 0.005);
            assert!((response(0.5) - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.005);
            assert!(response(0.7) < 0.005);
        }
    }

    #[test]
    fn invalid_calls_do_not_change_filter_history() {
        assert!(matches!(
            PulseShaper::new(SamplesPerChip::Two, 0),
            Err(DspError::InvalidFilter)
        ));
        assert!(matches!(
            PulseShaper::new(SamplesPerChip::Two, 3),
            Err(DspError::InvalidFilter)
        ));
        let mut shaper = PulseShaper::new(SamplesPerChip::Two, 16).unwrap();
        let mut output = [Complex32::new(9.0, 9.0); 2];
        assert_eq!(
            shaper.process(&[Complex32::new(f32::NAN, 0.0)], &mut output),
            Err(DspError::NonFiniteEvidence)
        );
        assert_eq!(
            shaper.process(&[Complex32::default()], &mut []),
            Err(DspError::InvalidLength)
        );
        assert_eq!(output, [Complex32::new(9.0, 9.0); 2]);
        shaper
            .process(&[Complex32::default()], &mut output)
            .unwrap();
        assert_eq!(output, [Complex32::default(); 2]);
    }
}
