// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use wide::f32x8;

use crate::{DspError, pulse_shaping::SamplesPerChip};

const FILTER_SPAN_CHIPS: usize = 128;
const FILTER_STAGES: usize = 2;
const CUTOFF_CHIP_RATE: f64 = 0.64;
const FILTERED_CORRECTION_GAIN: f64 = 0.8;
const KAISER_BETA: f64 = 7.0;
const BESSEL_TERMS: usize = 24;
const CHIPS_PER_SLOT: usize = umts_types::time::CHIPS_PER_SLOT as usize;
const CORRECTIONS_PER_CHIP_RATE: usize = 64;
const MAXIMUM_RELATIVE_ERROR: f64 = 0.05;
// A relative error only rejects when the correction itself is this large in
// full-scale units, so one legitimate correction in a quiet slot passes.
const MINIMUM_REJECTED_ERROR_RMS: f64 = 0.02;
const VECTOR_SAMPLES: usize = 4;
const QUIET_GROUP_SAMPLES: usize = 32;
const ADJACENT_CARRIER_OFFSET_HZ: f64 = 2_500_000.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct CrestReport {
    pub corrections: usize,
    pub limited_components: usize,
    pub work_limit_exceeded: bool,
    pub relative_error: f64,
    pub error_rms: f64,
}

impl CrestReport {
    pub fn rejected(self) -> bool {
        self.work_limit_exceeded
            || (self.relative_error > MAXIMUM_RELATIVE_ERROR
                && self.error_rms > MINIMUM_REJECTED_ERROR_RMS)
    }
}

fn bessel_i0(value: f64) -> f64 {
    let square = value * value / 4.0;
    let mut term = 1.0;
    let mut sum = term;
    for number in 1..=BESSEL_TERMS {
        term *= square / (number * number) as f64;
        sum += term;
    }
    sum
}

fn taps(rate: usize) -> Vec<f32> {
    let order = FILTER_SPAN_CHIPS * rate;
    let delay = order / 2;
    let cutoff = CUTOFF_CHIP_RATE / rate as f64;
    let window_scale = bessel_i0(KAISER_BETA);
    let mut taps: Vec<f64> = (0..=order)
        .map(|index| {
            let offset = index as f64 - delay as f64;
            let position = offset / delay as f64;
            let window =
                bessel_i0(KAISER_BETA * (1.0 - position * position).max(0.0).sqrt()) / window_scale;
            let argument = 2.0 * std::f64::consts::PI * cutoff * offset;
            let sinc = if index == delay {
                1.0
            } else {
                argument.sin() / argument
            };
            2.0 * cutoff * sinc * window
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter_mut().for_each(|tap| *tap /= sum);
    taps.into_iter()
        .map(|tap| (tap * FILTERED_CORRECTION_GAIN) as f32)
        .collect()
}

struct Stage {
    delay: Vec<Complex32>,
    delay_cursor: usize,
    corrections: Vec<Complex32>,
    correction_cursor: usize,
    remaining: usize,
}

impl Stage {
    fn new(length: usize) -> Self {
        Self {
            delay: vec![Complex32::default(); length / 2],
            delay_cursor: 0,
            corrections: vec![Complex32::default(); length],
            correction_cursor: 0,
            remaining: 0,
        }
    }

    fn sample(
        &mut self,
        sample: Complex32,
        taps: &[f32],
        limit: f32,
        report: &mut CrestReport,
    ) -> Complex32 {
        let error = sample - limit_envelope(sample, limit);
        if error != Complex32::default() {
            if self.remaining == 0 {
                report.work_limit_exceeded = true;
            } else {
                self.remaining -= 1;
                report.corrections += 1;
                let (before, after) = self.corrections.split_at_mut(self.correction_cursor);
                let (first, last) = taps.split_at(after.len());
                for (correction, &tap) in after.iter_mut().zip(first) {
                    *correction += error * tap;
                }
                for (correction, &tap) in before.iter_mut().zip(last) {
                    *correction += error * tap;
                }
            }
        }
        let output = self.delay[self.delay_cursor] - self.corrections[self.correction_cursor];
        self.delay[self.delay_cursor] = sample;
        self.corrections[self.correction_cursor] = Complex32::default();
        self.delay_cursor += 1;
        if self.delay_cursor == self.delay.len() {
            self.delay_cursor = 0;
        }
        self.correction_cursor += 1;
        if self.correction_cursor == taps.len() {
            self.correction_cursor = 0;
        }
        output
    }

    fn delay_only(&mut self, mut samples: &mut [Complex32]) {
        self.correction_cursor = (self.correction_cursor + samples.len() % self.corrections.len())
            % self.corrections.len();
        while !samples.is_empty() {
            let count = samples.len().min(self.delay.len() - self.delay_cursor);
            let (block, rest) = samples.split_at_mut(count);
            block.swap_with_slice(&mut self.delay[self.delay_cursor..self.delay_cursor + count]);
            self.delay_cursor = (self.delay_cursor + count) % self.delay.len();
            samples = rest;
        }
    }

    fn process(
        &mut self,
        mut samples: &mut [Complex32],
        taps: &[f32],
        limit: f32,
        report: &mut CrestReport,
    ) {
        while !samples.is_empty() {
            let available = samples
                .len()
                .min(self.delay.len() - self.delay_cursor)
                .min(self.corrections.len() - self.correction_cursor)
                .min(QUIET_GROUP_SAMPLES);
            let quiet = samples[..available]
                .chunks_exact(VECTOR_SAMPLES)
                .take_while(|block| {
                    block
                        .iter()
                        .all(|sample| sample.norm_sqr() <= limit * limit)
                })
                .count()
                * VECTOR_SAMPLES;
            if quiet == 0 {
                samples[0] = self.sample(samples[0], taps, limit, report);
                samples = &mut samples[1..];
                continue;
            }
            let (block, rest) = samples.split_at_mut(quiet);
            samples = rest;
            let delay_end = self.delay_cursor + quiet;
            let correction_end = self.correction_cursor + quiet;
            for ((block, delay), corrections) in block
                .chunks_exact_mut(VECTOR_SAMPLES)
                .zip(self.delay[self.delay_cursor..delay_end].chunks_exact_mut(VECTOR_SAMPLES))
                .zip(
                    self.corrections[self.correction_cursor..correction_end]
                        .chunks_exact_mut(VECTOR_SAMPLES),
                )
            {
                let output = (packed(delay) - packed(corrections)).to_array();
                delay.copy_from_slice(block);
                corrections.fill(Complex32::default());
                for (sample, pair) in block.iter_mut().zip(output.chunks_exact(2)) {
                    *sample = Complex32::new(pair[0], pair[1]);
                }
            }
            self.delay_cursor = if delay_end == self.delay.len() {
                0
            } else {
                delay_end
            };
            self.correction_cursor = if correction_end == self.corrections.len() {
                0
            } else {
                correction_end
            };
        }
    }
}

fn packed(samples: &[Complex32]) -> f32x8 {
    f32x8::new([
        samples[0].re,
        samples[0].im,
        samples[1].re,
        samples[1].im,
        samples[2].re,
        samples[2].im,
        samples[3].re,
        samples[3].im,
    ])
}

fn limit_envelope(sample: Complex32, limit: f32) -> Complex32 {
    if sample.norm_sqr() <= limit * limit {
        return sample;
    }
    let scale = f64::from(limit) / f64::from(sample.re).hypot(f64::from(sample.im));
    Complex32::new(
        ((f64::from(sample.re) * scale) as f32).clamp(-limit, limit),
        ((f64::from(sample.im) * scale) as f32).clamp(-limit, limit),
    )
}

pub struct CrestReducer {
    taps: Vec<f32>,
    stages: [Stage; FILTER_STAGES],
    reference: Vec<Complex32>,
    comparison: Vec<Complex32>,
    reference_cursor: usize,
    slot_samples: usize,
    slot_cursor: usize,
    correction_budget: usize,
    component_limit: f32,
    settling_samples: usize,
}

impl CrestReducer {
    pub fn for_dual_carrier(component_limit: f32) -> Result<Self, DspError> {
        let mut reducer = Self::new(SamplesPerChip::Four, component_limit)?;
        let sample_rate =
            umts_types::time::CHIPS_PER_SECOND as f64 * SamplesPerChip::Four.get() as f64;
        let center = (reducer.taps.len() / 2) as f64;
        // A real cosine shifts the correction filter to both carrier passbands.
        for (index, tap) in reducer.taps.iter_mut().enumerate() {
            let phase =
                std::f64::consts::TAU * ADJACENT_CARRIER_OFFSET_HZ * (index as f64 - center)
                    / sample_rate;
            *tap *= (2.0 * phase.cos()) as f32;
        }
        Ok(reducer)
    }

    pub fn new(rate: SamplesPerChip, component_limit: f32) -> Result<Self, DspError> {
        if !component_limit.is_finite()
            || !(0.0..=1.0).contains(&component_limit)
            || component_limit == 0.0
        {
            return Err(DspError::InvalidAmplitude);
        }
        let rate = rate.get();
        let taps = taps(rate);
        let delay = FILTER_STAGES * (taps.len() / 2);
        Ok(Self {
            stages: std::array::from_fn(|_| Stage::new(taps.len())),
            taps,
            reference: vec![Complex32::default(); delay],
            comparison: vec![Complex32::default(); CHIPS_PER_SLOT * rate],
            reference_cursor: 0,
            slot_samples: CHIPS_PER_SLOT * rate,
            slot_cursor: 0,
            correction_budget: CORRECTIONS_PER_CHIP_RATE * rate,
            component_limit,
            settling_samples: 0,
        })
    }

    // Include this delay once in the air origin and every emission receipt.
    pub fn group_delay_samples(&self) -> usize {
        self.reference.len()
    }

    // Rejected output must not reach the radio. State still advances on the exact sample grid.
    pub fn process(&mut self, samples: &mut [Complex32]) -> Result<CrestReport, DspError> {
        self.process_inner::<true>(samples)
    }

    fn process_inner<const VECTOR: bool>(
        &mut self,
        samples: &mut [Complex32],
    ) -> Result<CrestReport, DspError> {
        let exceeds_limit =
            crate::sample_validation::exceeds_component_limit(samples, self.component_limit)?
                || samples
                    .iter()
                    .any(|sample| sample.norm_sqr() > self.component_limit * self.component_limit);
        if self.settling_samples == 0 && !exceeds_limit {
            self.delay_only(samples);
            return Ok(CrestReport::default());
        }
        let count = samples.len();
        let settling = self.settling_samples;
        // Each nonlinear stage can extend a correction by one complete FIR support.
        self.settling_samples = FILTER_STAGES * self.taps.len();
        let mut report = CrestReport::default();
        let mut error_power = 0.0;
        let mut reference_power = 0.0;
        let mut remaining = samples;
        while !remaining.is_empty() {
            if self.slot_cursor == 0 {
                for stage in &mut self.stages {
                    stage.remaining = self.correction_budget;
                }
            }
            let group = remaining
                .len()
                .min(if VECTOR { self.comparison.len() } else { 1 })
                .min(self.slot_samples - self.slot_cursor);
            let (block, rest) = remaining.split_at_mut(group);
            remaining = rest;
            let references = &mut self.comparison[..group];
            let mut offset = 0;
            while offset < group {
                let count = (group - offset).min(self.reference.len() - self.reference_cursor);
                let end = self.reference_cursor + count;
                references[offset..offset + count]
                    .copy_from_slice(&self.reference[self.reference_cursor..end]);
                self.reference[self.reference_cursor..end]
                    .copy_from_slice(&block[offset..offset + count]);
                self.reference_cursor = end;
                if self.reference_cursor == self.reference.len() {
                    self.reference_cursor = 0;
                }
                offset += count;
            }
            for stage in &mut self.stages {
                stage.process(block, &self.taps, self.component_limit, &mut report);
            }
            for (sample, reference) in block.iter_mut().zip(references.iter().copied()) {
                let reduced = *sample;
                if !reduced.re.is_finite() || !reduced.im.is_finite() {
                    return Err(DspError::NonFiniteEvidence);
                }
                let output = limit_envelope(reduced, self.component_limit);
                report.limited_components +=
                    usize::from(output.re != reduced.re) + usize::from(output.im != reduced.im);
                let error = output - reference;
                error_power += f64::from(error.re).powi(2) + f64::from(error.im).powi(2);
                reference_power +=
                    f64::from(reference.re).powi(2) + f64::from(reference.im).powi(2);
                *sample = output;
            }
            self.slot_cursor += group;
            if self.slot_cursor == self.slot_samples {
                self.slot_cursor = 0;
            }
        }
        report.relative_error = if reference_power > 0.0 {
            (error_power / reference_power).sqrt()
        } else if error_power > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
        report.error_rms = if count > 0 {
            (error_power / count as f64).sqrt()
        } else {
            0.0
        };
        if report.corrections == 0 && report.limited_components == 0 && !report.work_limit_exceeded
        {
            self.settling_samples = settling.saturating_sub(count);
        }
        Ok(report)
    }

    fn delay_only(&mut self, samples: &mut [Complex32]) {
        if samples.is_empty() {
            return;
        }
        let resets_budget =
            self.slot_cursor == 0 || samples.len() > self.slot_samples - self.slot_cursor;
        let mut input = &samples[..];
        while !input.is_empty() {
            let count = input
                .len()
                .min(self.reference.len() - self.reference_cursor);
            self.reference[self.reference_cursor..self.reference_cursor + count]
                .copy_from_slice(&input[..count]);
            self.reference_cursor = (self.reference_cursor + count) % self.reference.len();
            input = &input[count..];
        }
        for stage in &mut self.stages {
            if resets_budget {
                stage.remaining = self.correction_budget;
            }
            stage.delay_only(samples);
        }
        self.slot_cursor =
            (self.slot_cursor + samples.len() % self.slot_samples) % self.slot_samples;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_carrier_corrections_cover_both_carriers_with_equal_delay() {
        let dual = CrestReducer::for_dual_carrier(0.8).unwrap();
        let single = CrestReducer::new(SamplesPerChip::Four, 0.8).unwrap();
        assert_eq!(dual.group_delay_samples(), single.group_delay_samples());
        let response = |frequency: f64| {
            let rate = umts_types::time::CHIPS_PER_SECOND as f64 * 4.0;
            let center = dual.taps.len() as f64 / 2.0 - 0.5;
            dual.taps
                .iter()
                .enumerate()
                .map(|(index, tap)| {
                    f64::from(*tap)
                        * (std::f64::consts::TAU * frequency * (index as f64 - center) / rate).cos()
                })
                .sum::<f64>()
        };
        for frequency in [-4_000_000.0, -2_500_000.0, 2_500_000.0, 4_000_000.0] {
            assert!((response(frequency) - FILTERED_CORRECTION_GAIN).abs() < 0.02);
        }
        assert!(response(6_000_000.0).abs() < 0.01);
    }

    #[test]
    fn dual_carrier_quiet_signal_is_only_delayed() {
        let mut reducer = CrestReducer::for_dual_carrier(0.8).unwrap();
        let delay = reducer.group_delay_samples();
        let input: Vec<_> = (0..delay * 3)
            .map(|index| {
                let phase = std::f32::consts::TAU * 2_500_000.0 * index as f32 / 15_360_000.0;
                Complex32::new(phase.cos(), phase.sin()) * 0.1
            })
            .collect();
        let mut output = input.clone();
        let report = reducer.process(&mut output).unwrap();
        assert_eq!(report.corrections, 0);
        assert_eq!(&output[delay..], &input[..input.len() - delay]);
    }

    fn sample_bits(samples: &[Complex32]) -> Vec<(u32, u32)> {
        samples
            .iter()
            .map(|sample| (sample.re.to_bits(), sample.im.to_bits()))
            .collect()
    }

    fn assert_filter_state(actual: &CrestReducer, reference: &CrestReducer) {
        assert_eq!(
            sample_bits(&actual.reference),
            sample_bits(&reference.reference)
        );
        assert_eq!(actual.reference_cursor, reference.reference_cursor);
        assert_eq!(actual.slot_cursor, reference.slot_cursor);
        for (actual, reference) in actual.stages.iter().zip(&reference.stages) {
            assert_eq!(sample_bits(&actual.delay), sample_bits(&reference.delay));
            assert_eq!(
                sample_bits(&actual.corrections),
                sample_bits(&reference.corrections)
            );
            assert_eq!(actual.delay_cursor, reference.delay_cursor);
            assert_eq!(actual.correction_cursor, reference.correction_cursor);
            assert_eq!(actual.remaining, reference.remaining);
        }
    }

    #[test]
    fn envelope_limit_preserves_phase_and_bounds_large_finite_values() {
        for input in [
            Complex32::new(0.7, -0.7),
            Complex32::new(f32::MAX, -f32::MAX),
        ] {
            let output = limit_envelope(input, 0.8);
            assert!(output.re.is_finite() && output.im.is_finite());
            assert!((output.norm() - 0.8).abs() < 1e-6);
            assert!((output.re + output.im).abs() < 1e-6);
            assert!(output.re > 0.0 && output.im < 0.0);
        }
        let quiet = Complex32::new(0.2, -0.3);
        assert_eq!(limit_envelope(quiet, 0.8), quiet);
    }

    #[test]
    fn diagonal_peak_does_not_bypass_settled_pipeline() {
        let mut reducer = CrestReducer::new(SamplesPerChip::Two, 0.8).unwrap();
        let mut samples = vec![Complex32::new(0.1, 0.1); CHIPS_PER_SLOT * 2];
        samples[100] = Complex32::new(0.7, 0.7);
        let report = reducer.process(&mut samples).unwrap();
        assert!(report.corrections > 0);
        assert!(samples.iter().all(|sample| sample.norm() <= 0.800001));
    }

    #[test]
    fn settled_blocks_match_scalar_pipeline_through_clipping_and_wraps() {
        for rate in [
            SamplesPerChip::Two,
            SamplesPerChip::Four,
            SamplesPerChip::Eight,
        ] {
            let mut actual = CrestReducer::new(rate, 0.8).unwrap();
            let mut reference = CrestReducer::new(rate, 0.8).unwrap();
            let mut position = 0;
            for length in [0, 1, 73, 5120, 6000, 17, 513, 8192, 258, 0, 4096] {
                let mut input: Vec<_> = (position..position + length)
                    .map(|index| match index {
                        31 | 5119 | 5120 | 5300 | 15_000 => Complex32::new(1.3, -1.1),
                        700..=1100 => Complex32::new(2.0, -2.0),
                        100..=101 => Complex32::new(-0.0, 0.0),
                        _ => Complex32::new((index as f32).sin() * 0.2, 0.3),
                    })
                    .collect();
                let mut expected = input.clone();
                reference.settling_samples = usize::MAX;
                let report = actual.process(&mut input).unwrap();
                let expected_report = reference.process_inner::<false>(&mut expected).unwrap();
                assert_eq!(sample_bits(&input), sample_bits(&expected));
                assert_eq!(report.corrections, expected_report.corrections);
                assert_eq!(
                    report.limited_components,
                    expected_report.limited_components
                );
                assert_eq!(
                    report.work_limit_exceeded,
                    expected_report.work_limit_exceeded
                );
                assert_eq!(
                    report.relative_error.to_bits(),
                    expected_report.relative_error.to_bits()
                );
                assert_filter_state(&actual, &reference);
                position += length;
            }
            let mut input = vec![Complex32::new(0.1, -0.2); 6000];
            for _ in 0..3 {
                let mut expected = input.clone();
                reference.settling_samples = usize::MAX;
                actual.process(&mut input).unwrap();
                reference.process_inner::<false>(&mut expected).unwrap();
                assert_eq!(sample_bits(&input), sample_bits(&expected));
                assert_filter_state(&actual, &reference);
            }
            assert_eq!(actual.settling_samples, 0);
        }
    }

    #[test]
    fn contiguous_correction_updates_match_scalar_ring() {
        let taps = taps(2);
        for cursor in [0, 1, taps.len() / 2, taps.len() - 1] {
            let mut stage = Stage::new(taps.len());
            stage.correction_cursor = cursor;
            stage.remaining = 1;
            let error = Complex32::new(1.3, -1.2) - limit_envelope(Complex32::new(1.3, -1.2), 0.8);
            let mut expected = stage.corrections.clone();
            for (offset, &tap) in taps.iter().enumerate() {
                let index = (cursor + offset) % taps.len();
                expected[index] += error * tap;
            }
            expected[cursor] = Complex32::default();
            stage.sample(
                Complex32::new(1.3, -1.2),
                &taps,
                0.8,
                &mut CrestReport::default(),
            );
            assert_eq!(sample_bits(&stage.corrections), sample_bits(&expected));
        }
    }

    #[test]
    fn inactive_is_exact_delayed_identity_and_chunks_preserve_state() {
        let mut whole = CrestReducer::new(SamplesPerChip::Two, 0.9).unwrap();
        let mut pieces = CrestReducer::new(SamplesPerChip::Two, 0.9).unwrap();
        let input: Vec<_> = (0..20_000)
            .map(|i| Complex32::new((i as f32).sin() * 0.2, (i as f32).cos() * 0.3))
            .collect();
        let mut output = input.clone();
        assert!(!whole.process(&mut output).unwrap().rejected());
        let mut chunked = input.clone();
        for block in chunked.chunks_mut(73) {
            pieces.process(block).unwrap();
        }
        assert_eq!(output, chunked);
        let delay = whole.group_delay_samples();
        assert_eq!(&output[delay..], &input[..input.len() - delay]);
        assert!(
            output[..delay]
                .iter()
                .all(|sample| *sample == Complex32::default())
        );
    }

    #[test]
    fn relative_error_alone_does_not_reject_a_small_correction() {
        let quiet = CrestReport {
            corrections: 1,
            limited_components: 4,
            work_limit_exceeded: false,
            relative_error: 0.068,
            error_rms: 0.0105,
        };
        assert!(!quiet.rejected());
        let loud = CrestReport {
            error_rms: 0.03,
            ..quiet
        };
        assert!(loud.rejected());
        let exhausted = CrestReport {
            work_limit_exceeded: true,
            relative_error: 0.0,
            error_rms: 0.0,
            ..quiet
        };
        assert!(exhausted.rejected());
    }

    #[test]
    fn distorted_chunks_match_and_work_exhaustion_is_rejected() {
        let mut whole = CrestReducer::new(SamplesPerChip::Two, 0.8).unwrap();
        let mut pieces = CrestReducer::new(SamplesPerChip::Two, 0.8).unwrap();
        let mut output = vec![Complex32::new(0.1, -0.2); 20_000];
        for index in (100..output.len()).step_by(337) {
            output[index] = Complex32::new(1.2, -1.1);
        }
        let mut chunked = output.clone();
        assert!(whole.process(&mut output).unwrap().corrections > 0);
        for block in chunked.chunks_mut(73) {
            pieces.process(block).unwrap();
        }
        assert_eq!(output, chunked);
        assert!(
            output
                .iter()
                .all(|sample| sample.re.abs() <= 0.8 && sample.im.abs() <= 0.8)
        );
        let mut overload = vec![Complex32::new(2.0, -2.0); CHIPS_PER_SLOT * 2];
        let mut overloaded = CrestReducer::new(SamplesPerChip::Two, 0.8).unwrap();
        let report = overloaded.process(&mut overload).unwrap();
        assert!(report.rejected());
        assert!(report.work_limit_exceeded);
        assert!(report.corrections <= FILTER_STAGES * overloaded.correction_budget);
    }

    #[test]
    fn nonfinite_input_does_not_mutate_filter_state() {
        let mut reducer = CrestReducer::new(SamplesPerChip::Two, 0.9).unwrap();
        assert_eq!(
            reducer
                .process(&mut [Complex32::new(f32::NAN, 0.0)])
                .unwrap_err(),
            DspError::NonFiniteEvidence
        );
        let mut input = vec![Complex32::new(0.1, 0.2); 1000];
        assert!(!reducer.process(&mut input).unwrap().rejected());
        assert!(
            input[..reducer.group_delay_samples()]
                .iter()
                .all(|sample| *sample == Complex32::default())
        );
    }
}
