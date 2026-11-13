// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::{Complex32, Complex64};
use umts_types::time::SampleInstant;
use wide::{f32x8, f64x4};

use crate::sample_validation::FiniteSamples;
use crate::{DspError, pulse_shaping::SamplesPerChip};

enum SampleInput<'a> {
    Raw(&'a [Complex32]),
    Finite(FiniteSamples<'a>),
}

const PHASES: usize = 1024;
const RADIUS: usize = 4;
const TAPS: usize = 2 * RADIUS + 1;
const OUTPUT_BLOCK: usize = 8;
const MAX_SAMPLES_PER_CHIP: usize = 8;
const MAX_BLOCK_SAMPLES: usize = TAPS + (OUTPUT_BLOCK - 1) * MAX_SAMPLES_PER_CHIP;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplePosition {
    sample: SampleInstant,
    fraction: f64,
}
impl SamplePosition {
    pub fn new(sample: SampleInstant, fraction: f64) -> Result<Self, DspError> {
        if !fraction.is_finite() || !(0.0..1.0).contains(&fraction) {
            return Err(DspError::InvalidLength);
        }
        Ok(Self { sample, fraction })
    }
    pub fn sample(self) -> SampleInstant {
        self.sample
    }
    pub fn fraction(self) -> f64 {
        self.fraction
    }
}

pub struct FractionalSampler {
    weights: Vec<[f64; TAPS]>,
    absolute_sums: Vec<f64>,
}
impl FractionalSampler {
    /// Real input samples required on each side of an interpolated position.
    pub const GUARD_SAMPLES: usize = RADIUS;

    pub fn new() -> Self {
        let weights: Vec<_> = (0..PHASES)
            .map(|phase| {
                let fraction = phase as f64 / PHASES as f64;
                let mut taps = std::array::from_fn(|index| {
                    let delta = index as f64 - RADIUS as f64 - fraction;
                    let sinc = if delta == 0.0 {
                        1.0
                    } else {
                        let angle = std::f64::consts::PI * delta;
                        angle.sin() / angle
                    };
                    let window = if delta.abs() <= RADIUS as f64 {
                        0.5 * (1.0 + (std::f64::consts::PI * delta / RADIUS as f64).cos())
                    } else {
                        0.0
                    };
                    sinc * window
                });
                let sum: f64 = taps.iter().sum();
                for tap in &mut taps {
                    *tap /= sum;
                }
                taps
            })
            .collect();
        let absolute_sums = weights
            .iter()
            .map(|taps| taps.iter().map(|v| v.abs()).sum())
            .collect();
        Self {
            weights,
            absolute_sums,
        }
    }
    pub fn guard_samples(&self) -> usize {
        Self::GUARD_SAMPLES
    }
    pub fn storage_bytes(&self) -> usize {
        self.weights.len() * std::mem::size_of::<[f64; TAPS]>()
            + self.absolute_sums.len() * std::mem::size_of::<f64>()
    }

    // Both interpolation guards must be real samples from the same continuous epoch.
    // The returned quantized receive position is not the UE's logical CFN/chip anchor.
    pub fn write_chips(
        &self,
        input_start: SampleInstant,
        input: &[Complex32],
        first: SamplePosition,
        rate: SamplesPerChip,
        output: &mut [Complex32],
    ) -> Result<SamplePosition, DspError> {
        self.write_prefix_chips(input_start, input, first, rate, output.len(), output)
    }

    /// Interpolate a prefix while validating the input extent of `validated_chips`.
    /// This preserves full-window guards and evidence rejection when only pilots
    /// are needed. Validation completes before any output is written.
    pub fn write_prefix_chips(
        &self,
        input_start: SampleInstant,
        input: &[Complex32],
        first: SamplePosition,
        rate: SamplesPerChip,
        validated_chips: usize,
        output: &mut [Complex32],
    ) -> Result<SamplePosition, DspError> {
        self.write_prefix_input(
            input_start,
            SampleInput::Raw(input),
            first,
            rate,
            validated_chips,
            output,
        )
    }

    /// Reuse owned finite-history evidence. Timing, extent and guard validation
    /// match write_chips, and samples without a sufficient amplitude bound are revalidated.
    pub fn write_validated_chips(
        &self,
        input_start: SampleInstant,
        input: FiniteSamples<'_>,
        first: SamplePosition,
        rate: SamplesPerChip,
        output: &mut [Complex32],
    ) -> Result<SamplePosition, DspError> {
        self.write_validated_prefix_chips(input_start, input, first, rate, output.len(), output)
    }

    pub fn write_validated_prefix_chips(
        &self,
        input_start: SampleInstant,
        input: FiniteSamples<'_>,
        first: SamplePosition,
        rate: SamplesPerChip,
        validated_chips: usize,
        output: &mut [Complex32],
    ) -> Result<SamplePosition, DspError> {
        self.write_prefix_input(
            input_start,
            SampleInput::Finite(input),
            first,
            rate,
            validated_chips,
            output,
        )
    }

    fn write_prefix_input(
        &self,
        input_start: SampleInstant,
        evidence: SampleInput<'_>,
        first: SamplePosition,
        rate: SamplesPerChip,
        validated_chips: usize,
        output: &mut [Complex32],
    ) -> Result<SamplePosition, DspError> {
        let input = match &evidence {
            SampleInput::Raw(input) => *input,
            SampleInput::Finite(input) => input.samples(),
        };
        if first.sample.epoch != input_start.epoch {
            return Err(DspError::DiscontinuousTime);
        }
        let phase = (first.fraction * PHASES as f64).round() as usize;
        let center = first
            .sample
            .index
            .checked_add(u64::from(phase == PHASES))
            .ok_or(DspError::InvalidLength)?;
        let phase = phase % PHASES;
        let actual = SamplePosition::new(
            SampleInstant {
                index: center,
                ..first.sample
            },
            phase as f64 / PHASES as f64,
        )?;
        let begin = center
            .checked_sub(input_start.index)
            .and_then(|v| usize::try_from(v).ok())
            .and_then(|v| v.checked_sub(RADIUS))
            .ok_or(DspError::InvalidLength)?;
        if output.is_empty() || output.len() > validated_chips {
            return Err(DspError::InvalidLength);
        }
        input_start
            .index
            .checked_add(input.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        let end = (validated_chips - 1)
            .checked_mul(rate.get())
            .and_then(|v| v.checked_add(begin))
            .and_then(|v| v.checked_add(TAPS))
            .filter(|&v| v <= input.len())
            .ok_or(DspError::InvalidLength)?;
        let maximum = f64::from(f32::MAX) / self.absolute_sums[phase];
        let already_bounded = matches!(evidence,
            SampleInput::Finite(input) if input.component_bound().is_some_and(|bound| f64::from(bound) <= maximum));
        if !already_bounded {
            validate_samples(&input[begin..end], maximum)?;
        }
        let count = output.len() / OUTPUT_BLOCK * OUTPUT_BLOCK;
        let (blocks, remainder) = output.split_at_mut(count);
        match rate {
            SamplesPerChip::Two => {
                Self::write_blocks::<2>(&input[begin..end], &self.weights[phase], blocks)
            }
            SamplesPerChip::Four => {
                Self::write_blocks::<4>(&input[begin..end], &self.weights[phase], blocks)
            }
            SamplesPerChip::Eight => {
                Self::write_blocks::<8>(&input[begin..end], &self.weights[phase], blocks)
            }
        }
        let start = begin + count * rate.get();
        for (index, sample) in remainder.iter_mut().enumerate() {
            let start = start + index * rate.get();
            let value = input[start..start + TAPS]
                .iter()
                .zip(&self.weights[phase])
                .fold(Complex64::default(), |sum, (value, weight)| {
                    sum + Complex64::new(f64::from(value.re), f64::from(value.im)) * weight
                });
            *sample = Complex32::new(value.re as f32, value.im as f32);
        }
        Ok(actual)
    }

    fn write_blocks<const STRIDE: usize>(
        input: &[Complex32],
        weights: &[f64; TAPS],
        output: &mut [Complex32],
    ) {
        let mut converted = [Complex64::default(); MAX_BLOCK_SAMPLES];
        let block_samples = TAPS + (OUTPUT_BLOCK - 1) * STRIDE;
        for (index, samples) in output.chunks_exact_mut(OUTPUT_BLOCK).enumerate() {
            let start = index * OUTPUT_BLOCK * STRIDE;
            for (destination, value) in converted[..block_samples]
                .iter_mut()
                .zip(&input[start..start + block_samples])
            {
                *destination = Complex64::new(f64::from(value.re), f64::from(value.im));
            }
            let mut sums = [f64x4::ZERO; OUTPUT_BLOCK / 2];
            for (tap, &weight) in weights.iter().enumerate() {
                let weight = f64x4::splat(weight);
                for (pair, sum) in sums.iter_mut().enumerate() {
                    let first = converted[tap + 2 * pair * STRIDE];
                    let second = converted[tap + (2 * pair + 1) * STRIDE];
                    *sum += f64x4::new([first.re, first.im, second.re, second.im]) * weight;
                }
            }
            for (samples, sum) in samples.chunks_exact_mut(2).zip(sums) {
                let lanes = sum.to_array();
                samples[0] = Complex32::new(lanes[0] as f32, lanes[1] as f32);
                samples[1] = Complex32::new(lanes[2] as f32, lanes[3] as f32);
            }
        }
    }
}
fn validate_samples(input: &[Complex32], maximum: f64) -> Result<(), DspError> {
    // The largest finite f32 at or below the f64 bound accepts exactly the
    // f32 component magnitudes that a comparison against the f64 bound accepts.
    let mut bound = maximum.min(f64::from(f32::MAX)) as f32;
    if f64::from(bound) > maximum {
        bound = f32::from_bits(bound.to_bits() - 1);
    }
    let limit = f32x8::splat(bound);
    let mut chunks = input.chunks_exact(4);
    for chunk in chunks.by_ref() {
        let values = f32x8::new([
            chunk[0].re,
            chunk[0].im,
            chunk[1].re,
            chunk[1].im,
            chunk[2].re,
            chunk[2].im,
            chunk[3].re,
            chunk[3].im,
        ]);
        if !values.abs().simd_le(limit).all() {
            // Preserve per-sample finite-before-amplitude error precedence,
            // including an amplitude error before a later NaN in this block.
            validate_samples_scalar(chunk, maximum)?;
        }
    }
    validate_samples_scalar(chunks.remainder(), maximum)
}

fn validate_samples_scalar(input: &[Complex32], maximum: f64) -> Result<(), DspError> {
    for sample in input {
        if !sample.re.is_finite() || !sample.im.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        if f64::from(sample.re).abs() > maximum || f64::from(sample.im).abs() > maximum {
            return Err(DspError::InvalidAmplitude);
        }
    }
    Ok(())
}

impl Default for FractionalSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
