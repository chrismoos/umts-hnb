// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
#[cfg(test)]
use num_complex::Complex64;
use sdr_hal::dsp::PhasorNco;
use umts_types::time::{SampleInstant, SampleRate};

use crate::DspError;

const MAX_CARRIERS: usize = 2;
const PHASE_MODULUS: f64 = 18_446_744_073_709_551_616.0;
// Half the exact limit, so f32 rounding never admits a block the exact check rejects.
const FAST_AMPLITUDE_BOUND: f32 = f32::MAX / (2 * MAX_CARRIERS) as f32;

#[derive(Clone, Copy, Debug)]
pub struct CarrierMixConfig {
    pub offset_hz: f64,
    pub occupied_bandwidth_hz: f64,
    pub power_fraction: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Oscillator {
    increment: u64,
    amplitude: f64,
}

#[derive(Clone, Debug)]
pub struct CarrierMixer {
    origin: SampleInstant,
    oscillators: [Oscillator; MAX_CARRIERS],
    count: usize,
}

impl CarrierMixer {
    pub fn new(
        rate: SampleRate,
        origin: SampleInstant,
        carriers: &[CarrierMixConfig],
    ) -> Result<Self, DspError> {
        if carriers.is_empty() || carriers.len() > MAX_CARRIERS {
            return Err(DspError::InvalidLength);
        }
        let mut oscillators = [Oscillator::default(); MAX_CARRIERS];
        let mut total_power = 0.0;
        for (oscillator, carrier) in oscillators.iter_mut().zip(carriers) {
            if !carrier.offset_hz.is_finite()
                || !carrier.occupied_bandwidth_hz.is_finite()
                || carrier.occupied_bandwidth_hz <= 0.0
                || carrier.offset_hz.abs() + carrier.occupied_bandwidth_hz / 2.0 >= rate.hz() / 2.0
            {
                return Err(DspError::InvalidFrequency);
            }
            if !carrier.power_fraction.is_finite() || !(0.0..=1.0).contains(&carrier.power_fraction)
            {
                return Err(DspError::InvalidAmplitude);
            }
            total_power += carrier.power_fraction;
            let turns = carrier.offset_hz / rate.hz();
            let magnitude = (turns.abs() * PHASE_MODULUS).round() as u64;
            oscillator.increment = if turns < 0.0 {
                magnitude.wrapping_neg()
            } else {
                magnitude
            };
            oscillator.amplitude = carrier.power_fraction.sqrt();
        }
        if total_power <= 0.0 || total_power > 1.0 {
            return Err(DspError::InvalidAmplitude);
        }
        if carriers.len() == MAX_CARRIERS
            && (carriers[0].offset_hz - carriers[1].offset_hz).abs()
                < (carriers[0].occupied_bandwidth_hz + carriers[1].occupied_bandwidth_hz) / 2.0
        {
            return Err(DspError::InvalidFrequency);
        }
        Ok(Self {
            origin,
            oscillators,
            count: carriers.len(),
        })
    }

    /// Inputs must already be pulse-shaped at the output sample rate.
    /// The sample timestamp uses the same radio epoch and clock as the origin.
    pub fn mix(
        &self,
        start: SampleInstant,
        inputs: &[&[Complex32]],
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if start.epoch != self.origin.epoch || start.index < self.origin.index {
            return Err(DspError::DiscontinuousTime);
        }
        if inputs.len() != self.count || inputs.iter().any(|input| input.len() != output.len()) {
            return Err(DspError::InvalidLength);
        }
        start
            .index
            .checked_add(output.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        for input in inputs {
            // NaN and infinity fail this comparison, so ordinary samples need no second pass.
            if input.iter().fold(true, |bounded, value| {
                bounded & (value.re.abs() + value.im.abs() < FAST_AMPLITUDE_BOUND)
            }) {
                continue;
            }
            for value in *input {
                if !value.re.is_finite() || !value.im.is_finite() {
                    return Err(DspError::NonFiniteEvidence);
                }
                // This bound keeps every rotation and two-carrier sum representable.
                if f64::from(value.re).abs() + f64::from(value.im).abs()
                    > f64::from(f32::MAX) / MAX_CARRIERS as f64
                {
                    return Err(DspError::InvalidAmplitude);
                }
            }
        }
        if self.count == 1
            && self.oscillators[0].increment == 0
            && self.oscillators[0].amplitude == 1.0
        {
            output.copy_from_slice(inputs[0]);
            return Ok(());
        }
        output.fill(Complex32::default());
        for (oscillator, input) in self.oscillators.iter().zip(inputs) {
            // The start phase comes from the integer timestamp on every call, so gaps never accumulate drift.
            let phase = oscillator
                .increment
                .wrapping_mul(start.index - self.origin.index);
            let mut carrier =
                PhasorNco::with_start_phase(radians(phase), radians(oscillator.increment));
            carrier.rotate_accumulate(input, output, oscillator.amplitude as f32);
        }
        Ok(())
    }
}

/// Signed interpretation keeps a negative offset's small step exact in f64.
fn radians(phase: u64) -> f64 {
    phase as i64 as f64 / PHASE_MODULUS * std::f64::consts::TAU
}

#[cfg(test)]
fn rotation(phase: u64) -> Complex64 {
    Complex64::from_polar(1.0, phase as f64 / PHASE_MODULUS * std::f64::consts::TAU)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use umts_types::RadioEpoch;

    use super::*;

    // Four f32 rotor lanes accumulate rounding over at most 256 updates before reseeding.
    const ROTATION_ERROR: f32 = 1e-4;

    fn at(index: u64) -> SampleInstant {
        SampleInstant {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            index,
        }
    }

    fn config(offset_hz: f64, power_fraction: f64) -> CarrierMixConfig {
        CarrierMixConfig {
            offset_hz,
            occupied_bandwidth_hz: 5_000_000.0,
            power_fraction,
        }
    }

    fn mixer(carriers: &[CarrierMixConfig]) -> CarrierMixer {
        CarrierMixer::new(SampleRate::new(15_360_000, 1).unwrap(), at(0), carriers).unwrap()
    }

    #[test]
    fn zero_offset_preserves_input_bits() {
        let mixer = mixer(&[config(0.0, 1.0)]);
        let input = [Complex32::new(-0.0, 0.3), Complex32::new(-0.7, -0.0)];
        let mut output = [Complex32::default(); 2];
        mixer.mix(at(71), &[&input], &mut output).unwrap();
        for (actual, expected) in output.iter().zip(input) {
            assert_eq!(actual.re.to_bits(), expected.re.to_bits());
            assert_eq!(actual.im.to_bits(), expected.im.to_bits());
        }
    }

    #[test]
    fn timestamps_preserve_phase_across_chunks_and_gaps() {
        let mixer = mixer(&[config(-2_500_000.0, 0.5), config(2_500_000.0, 0.5)]);
        let input = [Complex32::new(0.3, -0.2); 10243];
        let mut full = [Complex32::default(); 10243];
        mixer.mix(at(8192), &[&input, &input], &mut full).unwrap();
        for (offset, count) in [(0, 117), (117, 333), (701, 323), (1009, 1103), (4091, 4106)] {
            let mut part = vec![Complex32::default(); count];
            mixer
                .mix(
                    at(8192 + offset as u64),
                    &[&input[..count], &input[..count]],
                    &mut part,
                )
                .unwrap();
            for (actual, expected) in part.iter().zip(&full[offset..offset + count]) {
                assert!(
                    (*actual - *expected).norm()
                        < 2.0 * ROTATION_ERROR * input[0].norm() * 2.0_f32.sqrt(),
                    "chunk offset {offset}: error {}",
                    (*actual - *expected).norm()
                );
            }
        }
    }

    #[test]
    fn vector_lanes_and_tail_match_direct_sample_rotation() {
        let mixer = mixer(&[config(-2_500_000.0, 0.25), config(2_500_000.0, 0.75)]);
        let first: Vec<_> = (0..10243)
            .map(|index| Complex32::new((index % 519) as f32 / 1000.0, -0.2))
            .collect();
        let second: Vec<_> = first
            .iter()
            .map(|value| Complex32::new(-value.im, value.re))
            .collect();
        let mut output = vec![Complex32::default(); first.len()];
        mixer.mix(at(123), &[&first, &second], &mut output).unwrap();
        for (index, actual) in output.iter().enumerate() {
            let mut expected = Complex32::default();
            let mut magnitude = 0.0;
            for (oscillator, sample) in mixer.oscillators.iter().zip([first[index], second[index]])
            {
                magnitude += sample.norm() * oscillator.amplitude as f32;
                let phase = oscillator.increment.wrapping_mul(123 + index as u64);
                let shifted = Complex64::new(f64::from(sample.re), f64::from(sample.im))
                    * rotation(phase)
                    * oscillator.amplitude;
                expected += Complex32::new(shifted.re as f32, shifted.im as f32);
            }
            assert!(
                (*actual - expected).norm() < ROTATION_ERROR * magnitude,
                "sample {index}: error {}",
                (*actual - expected).norm()
            );
        }
    }

    #[test]
    fn rejects_aliasing_overlap_and_invalid_power() {
        let rate = SampleRate::new(7_680_000, 1).unwrap();
        assert!(CarrierMixer::new(rate, at(0), &[config(2_500_000.0, 1.0)]).is_err());
        let rate = SampleRate::new(15_360_000, 1).unwrap();
        for carriers in [
            [config(0.0, 0.5), config(1_000_000.0, 0.5)],
            [config(-2_500_000.0, 0.7), config(2_500_000.0, 0.7)],
            [config(-2_500_000.0, f64::NAN), config(2_500_000.0, 0.5)],
        ] {
            assert!(CarrierMixer::new(rate, at(0), &carriers).is_err());
        }
    }

    #[test]
    fn positive_offset_rotates_counterclockwise_and_scales_by_amplitude() {
        let mixer = mixer(&[config(3_840_000.0, 0.25)]);
        let input = [Complex32::new(1.0, 0.0); 4];
        let mut output = [Complex32::default(); 4];
        mixer.mix(at(0), &[&input], &mut output).unwrap();
        let expected = [
            Complex32::new(0.5, 0.0),
            Complex32::new(0.0, 0.5),
            Complex32::new(-0.5, 0.0),
            Complex32::new(0.0, -0.5),
        ];
        for (actual, expected) in output.iter().zip(expected) {
            assert!((*actual - expected).norm() < 1e-6);
        }
    }

    #[test]
    fn integer_phase_remains_stable_past_float_timestamp_precision() {
        let mixer = mixer(&[config(3_840_000.0, 1.0)]);
        let input = [Complex32::new(1.0, 0.0); 4];
        let mut early = [Complex32::default(); 4];
        let mut late = [Complex32::default(); 4];
        mixer.mix(at(1), &[&input], &mut early).unwrap();
        mixer
            .mix(at((1_u64 << 60) + 1), &[&input], &mut late)
            .unwrap();
        assert_eq!(early, late);
    }

    #[test]
    fn invalid_input_leaves_output_unchanged() {
        let mixer = mixer(&[config(0.0, 1.0)]);
        let mut output = [Complex32::new(9.0, 4.0)];
        let input = [Complex32::new(f32::NAN, 0.0)];
        assert_eq!(
            mixer.mix(at(0), &[&input], &mut output),
            Err(DspError::NonFiniteEvidence)
        );
        assert_eq!(output, [Complex32::new(9.0, 4.0)]);
    }
}
