// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use wide::f32x8;

use crate::DspError;
use std::ops::Range;

/// An immutable finite sample batch. Construction validates the whole batch
/// before it can enter owned storage. Slicing preserves that evidence.
#[derive(Clone, Copy)]
pub struct FiniteSamples<'a> {
    samples: &'a [Complex32],
    unit_bounded: bool,
}

impl<'a> FiniteSamples<'a> {
    pub fn new(samples: &'a [Complex32]) -> Result<Self, DspError> {
        Ok(Self {
            samples,
            unit_bounded: !exceeds_component_limit(samples, 1.0)?,
        })
    }

    #[inline]
    pub fn samples(self) -> &'a [Complex32] {
        self.samples
    }

    #[inline]
    pub fn component_bound(self) -> Option<f32> {
        self.unit_bounded.then_some(1.0)
    }

    #[inline]
    pub fn range(self, range: Range<usize>) -> Option<Self> {
        Some(Self {
            samples: self.samples.get(range)?,
            ..self
        })
    }
}

/// Owns finite samples without exposing mutable slices that could invalidate
/// their provenance. Values above one remain valid and make the sampler
/// revalidate amplitudes. One is not an RF acceptance threshold.
pub struct FiniteSampleBuffer {
    samples: Vec<Complex32>,
    unit_bounded: bool,
}

impl FiniteSampleBuffer {
    pub fn zeroed(length: usize) -> Self {
        Self {
            samples: vec![Complex32::default(); length],
            unit_bounded: true,
        }
    }

    #[inline]
    pub fn samples(&self) -> &[Complex32] {
        &self.samples
    }

    #[inline]
    pub fn write(&mut self, offset: usize, input: FiniteSamples<'_>) -> Result<(), DspError> {
        let end = offset
            .checked_add(input.samples.len())
            .ok_or(DspError::InvalidLength)?;
        let destination = self
            .samples
            .get_mut(offset..end)
            .ok_or(DspError::InvalidLength)?;
        destination.copy_from_slice(input.samples);
        self.unit_bounded &= input.unit_bounded;
        Ok(())
    }

    pub fn copy_within(&mut self, range: Range<usize>, destination: usize) -> Result<(), DspError> {
        let count = range
            .end
            .checked_sub(range.start)
            .ok_or(DspError::InvalidLength)?;
        if range.end > self.samples.len()
            || destination
                .checked_add(count)
                .is_none_or(|end| end > self.samples.len())
        {
            return Err(DspError::InvalidLength);
        }
        self.samples.copy_within(range, destination);
        Ok(())
    }

    #[inline]
    pub fn window(&self, range: Range<usize>) -> Option<FiniteSamples<'_>> {
        Some(FiniteSamples {
            samples: self.samples.get(range)?,
            unit_bounded: self.unit_bounded,
        })
    }
}

pub fn exceeds_component_limit(samples: &[Complex32], limit: f32) -> Result<bool, DspError> {
    let limit = f32x8::splat(limit);
    let mut invalid = f32x8::ZERO;
    let mut exceeded = f32x8::ZERO;
    let mut blocks = samples.chunks_exact(4);
    for block in &mut blocks {
        let values = f32x8::new([
            block[0].re,
            block[0].im,
            block[1].re,
            block[1].im,
            block[2].re,
            block[2].im,
            block[3].re,
            block[3].im,
        ]);
        invalid |= !values.is_finite();
        exceeded |= values.abs().simd_gt(limit);
    }
    if invalid.any() {
        return Err(DspError::NonFiniteEvidence);
    }
    let mut rejected = exceeded.any();
    let limit = limit.to_array()[0];
    for sample in blocks.remainder() {
        if !sample.re.is_finite() || !sample.im.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        rejected |= sample.re.abs() > limit || sample.im.abs() > limit;
    }
    Ok(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_finite_windows_preserve_proof_and_fail_writes_atomically() {
        let mut buffer = FiniteSampleBuffer::zeroed(16);
        let bounded = [Complex32::new(1.0, -1.0), Complex32::new(0.0, -0.0)];
        buffer
            .write(3, FiniteSamples::new(&bounded).unwrap())
            .unwrap();
        buffer.copy_within(3..5, 10).unwrap();
        assert_eq!(&buffer.samples()[10..12], &bounded);
        assert_eq!(buffer.window(10..12).unwrap().component_bound(), Some(1.0));
        let large = [Complex32::new(f32::MAX, -f32::MAX)];
        let large = FiniteSamples::new(&large).unwrap();
        let saved = buffer.samples().to_vec();
        for offset in [16, usize::MAX] {
            assert_eq!(buffer.write(offset, large), Err(DspError::InvalidLength));
            assert_eq!(buffer.samples(), saved);
            assert_eq!(buffer.window(0..16).unwrap().component_bound(), Some(1.0));
        }
        for (range, destination) in [
            (Range { start: 2, end: 1 }, 0),
            (0..17, 0),
            (0..2, 15),
            (0..1, usize::MAX),
        ] {
            assert_eq!(
                buffer.copy_within(range, destination),
                Err(DspError::InvalidLength)
            );
            assert_eq!(buffer.samples(), saved);
        }
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(matches!(
                FiniteSamples::new(&[Complex32::new(invalid, 0.0)]),
                Err(DspError::NonFiniteEvidence)
            ));
        }
        buffer.write(0, large).unwrap();
        assert_eq!(buffer.window(0..16).unwrap().component_bound(), None);
        // Conservative provenance remains valid even when an outlier is later
        // overwritten. It may cost a scan but cannot skip a required rejection.
        buffer
            .write(0, FiniteSamples::new(&bounded).unwrap())
            .unwrap();
        assert_eq!(buffer.window(0..16).unwrap().component_bound(), None);
        assert!(buffer.window(16..17).is_none());
    }

    #[test]
    fn vector_validation_matches_scalar_across_blocks_and_tails() {
        let slot_samples = umts_types::time::CHIPS_PER_SLOT as usize
            * crate::pulse_shaping::SamplesPerChip::Two.get();
        for length in (0..33).chain([slot_samples - 1, slot_samples, slot_samples + 1]) {
            let samples: Vec<_> = (0..length)
                .map(|index| {
                    Complex32::new((index % 17) as f32 / 16.0, -(index as f32 % 11.0) / 8.0)
                })
                .collect();
            for limit in [0.0, 0.5, 1.0, f32::MAX] {
                let expected = samples
                    .iter()
                    .any(|sample| sample.re.abs() > limit || sample.im.abs() > limit);
                assert_eq!(exceeds_component_limit(&samples, limit), Ok(expected));
            }
        }
    }

    #[test]
    fn nonfinite_components_override_peak_rejection_in_every_lane_and_tail() {
        for length in 1..18 {
            for index in 0..length {
                for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                    for imaginary in [false, true] {
                        let mut samples = vec![Complex32::new(2.0, -2.0); length];
                        if imaginary {
                            samples[index].im = value;
                        } else {
                            samples[index].re = value;
                        }
                        assert_eq!(
                            exceeds_component_limit(&samples, 1.0),
                            Err(DspError::NonFiniteEvidence)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn component_boundary_and_signed_zero_are_not_rejected() {
        let samples = [Complex32::new(0.5, -0.5), Complex32::new(0.0, -0.0)];
        assert_eq!(exceeds_component_limit(&samples, 0.5), Ok(false));
        let limit = f32::from_bits(0.5_f32.to_bits() - 1);
        assert_eq!(exceeds_component_limit(&samples, limit), Ok(true));
    }
}
