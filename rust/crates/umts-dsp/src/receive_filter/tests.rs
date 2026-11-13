// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;
use umts_types::RadioEpoch;

use super::*;
use crate::pulse_shaping::PulseShaper;

#[test]
fn vector_kernel_preserves_scalar_bits_across_lengths_and_gains() {
    for length in (1..=257).step_by(2) {
        let taps: Vec<_> = (0..length).map(|i| (i as f32 - 83.0) / 101.0).collect();
        for gain in [0.000001_f32, 1.0, 1000.0] {
            let values: Vec<_> = (0..length)
                .map(|i| {
                    Complex32::new(
                        ((i * 17 % 101) as f32 - 50.0) * gain,
                        ((i * 29 % 97) as f32 - 48.0) * gain,
                    )
                })
                .collect();
            let actual = Coefficients::new(&taps).unwrap().dot(&values, &taps);
            let expected = filter_dot_scalar(&values, &taps);
            assert_eq!(actual.re.to_bits(), expected.re.to_bits());
            assert_eq!(actual.im.to_bits(), expected.im.to_bits());
        }
    }
}

fn at(index: u64) -> SampleInstant {
    SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
        index,
    }
}

#[cfg(target_feature = "avx2")]
#[test]
fn consecutive_outputs_preserve_scalar_bits_and_reverse_history_order() {
    let length = vector::SPECIALIZED_TAPS;
    let outputs = vector::COMPLEX_LANES;
    let taps: Vec<_> = (0..length).map(|i| (i as f32 - 83.0) / 101.0).collect();
    for gain in [0.0_f32, 0.000001, 1.0, 1000.0] {
        let values: Vec<_> = (0..length + outputs - 1)
            .map(|i| {
                Complex32::new(
                    ((i * 17 % 101) as f32 - 50.0) * gain,
                    ((i * 29 % 97) as f32 - 48.0) * gain,
                )
            })
            .collect();
        let actual = Coefficients::consecutive_four(&values, &taps).unwrap();
        for (index, actual) in actual.into_iter().enumerate() {
            let offset = outputs - 1 - index;
            let expected = filter_dot_scalar(&values[offset..offset + taps.len()], &taps);
            assert_eq!(actual.re.to_bits(), expected.re.to_bits());
            assert_eq!(actual.im.to_bits(), expected.im.to_bits());
        }
        assert!(Coefficients::consecutive_four(&values[..values.len() - 1], &taps).is_none());
        assert!(Coefficients::consecutive_four(&values, &taps[..length / 2 + 1]).is_none());
    }
}

#[test]
fn full_rate_preserves_every_decimation_phase_and_chunk_boundary() {
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        let input: Vec<_> = (0..1200)
            .map(|i| Complex32::new((i % 5) as f32 - 2.0, (i % 9) as f32 - 4.0))
            .collect();
        let mut full = ReceiveFilter::full_rate(rate, 16, at(91)).unwrap();
        let mut samples = vec![Complex32::default(); full.output_length(input.len()).unwrap()];
        let window = full.process(at(91), &input, &mut samples).unwrap();
        assert_eq!(window.sample_stride, 1);
        let first = window.first_center.unwrap().index;
        for phase in 0..rate.get() {
            let mut decimated = ReceiveFilter::new(rate, 16, phase, at(91)).unwrap();
            let mut expected =
                vec![Complex32::default(); decimated.output_length(input.len()).unwrap()];
            let selected = decimated.process(at(91), &input, &mut expected).unwrap();
            let offset = (selected.first_center.unwrap().index - first) as usize;
            assert_eq!(
                expected,
                samples[offset..]
                    .iter()
                    .step_by(rate.get())
                    .copied()
                    .collect::<Vec<_>>()
            );
        }
        let mut split = ReceiveFilter::full_rate(rate, 16, at(91)).unwrap();
        let mut actual = Vec::new();
        let mut offset = 0;
        for count in [1, 0, 126, 1, 17, 300, 755] {
            let mut output = vec![Complex32::default(); split.output_length(count).unwrap()];
            let chunk = split
                .process(
                    at(91 + offset as u64),
                    &input[offset..offset + count],
                    &mut output,
                )
                .unwrap();
            if let Some(center) = chunk.first_center {
                assert_eq!(center.index, first + actual.len() as u64);
            }
            actual.extend(output);
            offset += count;
        }
        assert_eq!(actual, samples);
    }
}

fn reference_taps(rate: usize, span: usize) -> Vec<f64> {
    let alpha = 0.22;
    let pi = std::f64::consts::PI;
    let mut taps: Vec<_> = (0..=span * rate)
        .map(|index| {
            let t = index as f64 / rate as f64 - span as f64 / 2.0;
            if t == 0.0 {
                1.0 - alpha + 4.0 * alpha / pi
            } else {
                ((pi * (1.0 - alpha) * t).sin() + 4.0 * alpha * t * (pi * (1.0 + alpha) * t).cos())
                    / (pi * t * (1.0 - 16.0 * alpha * alpha * t * t))
            }
        })
        .collect();
    let energy: f64 = taps.iter().map(|value| value * value).sum();
    for tap in &mut taps {
        *tap /= (energy * rate as f64).sqrt();
    }
    taps
}

#[test]
fn every_phase_and_rate_matches_independent_direct_convolution_and_timing() {
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        let taps = reference_taps(rate.get(), 16);
        let input: Vec<_> = (0..601)
            .map(|i| Complex32::new((i % 7) as f32 - 3.0, (i % 11) as f32 - 5.0))
            .collect();
        for phase in 0..rate.get() {
            let mut filter = ReceiveFilter::new(rate, 16, phase, at(137)).unwrap();
            let mut output = vec![Complex32::default(); filter.output_length(input.len()).unwrap()];
            let window = filter.process(at(137), &input, &mut output).unwrap();
            let first = (taps.len() - 1..input.len())
                .find(|i| (137 + i) % rate.get() == phase)
                .unwrap();
            assert_eq!(
                window.first_center,
                Some(at(137 + first as u64 - filter.group_delay_samples() as u64))
            );
            assert_eq!(window.next_input, at(738));
            assert_eq!(window.sample_stride, rate.get());
            for (&actual, time) in output.iter().zip((first..input.len()).step_by(rate.get())) {
                let expected = taps.iter().enumerate().fold(
                    num_complex::Complex64::default(),
                    |sum, (offset, tap)| {
                        let sample = input[time - offset];
                        sum + num_complex::Complex64::new(
                            f64::from(sample.re),
                            f64::from(sample.im),
                        ) * tap
                    },
                );
                let difference =
                    num_complex::Complex64::new(f64::from(actual.re), f64::from(actual.im))
                        - expected;
                assert!(difference.norm() < 2e-6);
            }
        }
    }
}

#[test]
fn arbitrary_input_splits_preserve_samples_and_absolute_centers() {
    let input: Vec<_> = (0..1200)
        .map(|i| Complex32::new((i % 5) as f32, (i % 9) as f32))
        .collect();
    for phase in 0..4 {
        let mut full = ReceiveFilter::new(SamplesPerChip::Four, 32, phase, at(91)).unwrap();
        let mut split = ReceiveFilter::new(SamplesPerChip::Four, 32, phase, at(91)).unwrap();
        let mut expected = vec![Complex32::default(); full.output_length(input.len()).unwrap()];
        let expected_window = full.process(at(91), &input, &mut expected).unwrap();
        let mut actual = Vec::new();
        let mut centers = Vec::new();
        let mut offset = 0;
        for count in [0, 1, 126, 0, 1, 1, 17, 300, 754] {
            let mut output = vec![Complex32::default(); split.output_length(count).unwrap()];
            let window = split
                .process(
                    at(91 + offset as u64),
                    &input[offset..offset + count],
                    &mut output,
                )
                .unwrap();
            if let Some(first) = window.first_center {
                centers.extend((0..output.len()).map(|i| first.index + (i * 4) as u64));
            } else {
                assert!(output.is_empty());
            }
            actual.extend(output);
            offset += count;
        }
        assert_eq!(offset, input.len());
        assert_eq!(actual, expected);
        let first = expected_window.first_center.unwrap().index;
        assert_eq!(
            centers,
            (0..expected.len())
                .map(|i| first + (i * 4) as u64)
                .collect::<Vec<_>>()
        );
        assert_eq!(split.next_input(), full.next_input());
    }
}

#[test]
fn paired_pulses_recover_unit_chip_gain_across_rates_and_amplitudes() {
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for gain in [1e-12_f32, 1.0, 100.0] {
            let mut tx = PulseShaper::new(rate, 32).unwrap();
            let mut rx = ReceiveFilter::new(rate, 32, 0, at(0)).unwrap();
            let mut chips = vec![Complex32::default(); 160];
            chips[64] = Complex32::new(gain, -gain * 0.5);
            let mut samples = vec![Complex32::default(); tx.output_length(chips.len()).unwrap()];
            tx.process(&chips, &mut samples).unwrap();
            let mut actual = vec![Complex32::default(); rx.output_length(samples.len()).unwrap()];
            let window = rx.process(at(0), &samples, &mut actual).unwrap();
            let first = window.first_center.unwrap().index as usize;
            let peak = (64 * rate.get() + tx.group_delay_samples() - first) / rate.get();
            assert!((actual[peak] / gain - Complex32::new(1.0, -0.5)).norm() < 2e-6);
            for offset in 1..=12 {
                assert!(actual[peak - offset].norm() / gain < 0.0012);
                assert!(actual[peak + offset].norm() / gain < 0.0012);
            }
        }
    }
}

#[test]
fn invalid_input_is_atomic_and_gaps_require_explicit_history_reset() {
    assert!(ReceiveFilter::new(SamplesPerChip::Two, 0, 0, at(0)).is_err());
    assert!(ReceiveFilter::new(SamplesPerChip::Two, 3, 0, at(0)).is_err());
    assert!(ReceiveFilter::new(SamplesPerChip::Two, 16, 2, at(0)).is_err());
    let mut filter = ReceiveFilter::new(SamplesPerChip::Two, 16, 0, at(0)).unwrap();
    let input = vec![Complex32::new(1.0, 2.0); 101];
    let mut output = vec![Complex32::new(9.0, 8.0); filter.output_length(input.len()).unwrap()];
    let saved = output.clone();
    let mut invalid = input.clone();
    invalid[100].re = f32::NAN;
    assert_eq!(
        filter.process(at(0), &invalid, &mut output),
        Err(DspError::NonFiniteEvidence)
    );
    assert_eq!(
        filter.process(at(1), &input, &mut output),
        Err(DspError::DiscontinuousTime)
    );
    assert_eq!(
        filter.process(at(0), &input, &mut []),
        Err(DspError::InvalidLength)
    );
    assert_eq!(filter.next_input(), at(0));
    assert_eq!(output, saved);
    filter.process(at(0), &input, &mut output).unwrap();
    assert_eq!(
        filter.process(at(102), &input, &mut output),
        Err(DspError::DiscontinuousTime)
    );
    let new_origin = SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 113,
    };
    assert_eq!(
        filter.process(new_origin, &input, &mut output),
        Err(DspError::DiscontinuousTime)
    );
    filter.reset(new_origin);
    let mut fresh = ReceiveFilter::new(SamplesPerChip::Two, 16, 0, new_origin).unwrap();
    output.resize(
        filter.output_length(input.len()).unwrap(),
        Complex32::default(),
    );
    let mut expected = output.clone();
    assert_eq!(
        filter.process(new_origin, &input, &mut output).unwrap(),
        fresh.process(new_origin, &input, &mut expected).unwrap()
    );
    assert_eq!(output, expected);
    filter.reset(at(u64::MAX - 1));
    assert_eq!(filter.output_length(2), Err(DspError::InvalidLength));
}

#[test]
fn filtered_preambles_preserve_signature_and_power_across_sample_phases() {
    use crate::prach::{PREAMBLE_CHIPS, PrachCode, PreambleProbe, PreambleSignature};
    let code = PrachCode::new(7504).unwrap();
    let probe = PreambleProbe::new(code, u16::MAX).unwrap();
    let mut preamble = vec![Complex32::default(); PREAMBLE_CHIPS];
    code.write_preamble(PreambleSignature::new(13).unwrap(), &mut preamble)
        .unwrap();
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for delay in 0..rate.get() {
            let span = 32;
            let mut chips = vec![Complex32::default(); span];
            chips.extend_from_slice(&preamble);
            chips.resize(chips.len() + span * 2, Complex32::default());
            let mut tx = PulseShaper::new(rate, span).unwrap();
            let mut samples = vec![Complex32::default(); tx.output_length(chips.len()).unwrap()];
            tx.process(&chips, &mut samples).unwrap();
            let mut input = vec![Complex32::default(); delay];
            input.extend(samples);
            let channel = Complex32::new(0.003, -0.004);
            for sample in &mut input {
                *sample *= channel;
            }
            let phase = (137 + delay) % rate.get();
            let mut filter = ReceiveFilter::new(rate, span, phase, at(137)).unwrap();
            let mut output = vec![Complex32::default(); filter.output_length(input.len()).unwrap()];
            let window = filter.process(at(137), &input, &mut output).unwrap();
            let center = 137 + delay + span * rate.get() + tx.group_delay_samples();
            let offset = (center - window.first_center.unwrap().index as usize) / rate.get();
            let result = probe
                .correlate(&output[offset..offset + PREAMBLE_CHIPS])
                .unwrap();
            let best = result
                .iter()
                .flatten()
                .max_by(|a, b| a.coherent_fraction.total_cmp(&b.coherent_fraction))
                .unwrap();
            assert_eq!(best.signature.number(), 13);
            assert!(best.coherent_fraction > 0.9999);
            assert!(
                (best.amplitude
                    / num_complex::Complex64::new(f64::from(channel.re), f64::from(channel.im))
                    - num_complex::Complex64::new(1.0, 0.0))
                .norm()
                    < 0.001
            );
        }
    }
}

#[cfg(target_feature = "avx2")]
#[test]
fn planar_eight_outputs_preserve_scalar_bits_at_every_history_offset() {
    let length = vector::SPECIALIZED_TAPS;
    let taps: Vec<_> = (0..length).map(|i| (i as f32 - 83.0) / 101.0).collect();
    for gain in [0.0_f32, 1e-12, 1.0, 1e6] {
        let values: Vec<_> = (0..length + INPUT_BLOCK_SAMPLES - 1)
            .map(|i| {
                Complex32::new(
                    ((i * 17 % 101) as f32 - 50.0) * gain,
                    ((i * 29 % 97) as f32 - 48.0) * gain,
                )
            })
            .collect();
        let planar = vector::PlanarBlock::new(&values);
        for start in 0..=INPUT_BLOCK_SAMPLES - vector::PlanarBlock::OUTPUTS {
            for (index, actual) in planar.consecutive_eight(start, &taps).iter().enumerate() {
                let offset = start + vector::PlanarBlock::OUTPUTS - 1 - index;
                let expected = filter_dot_scalar(&values[offset..offset + length], &taps);
                assert_eq!(actual.re.to_bits(), expected.re.to_bits());
                assert_eq!(actual.im.to_bits(), expected.im.to_bits());
            }
        }
    }
}

#[test]
fn full_rate_tiny_chunks_and_reset_preserve_samples_near_timestamp_limit() {
    let input: Vec<_> = (0..701)
        .map(|i| Complex32::new((i * 17 % 101) as f32 - 50.0, (i * 29 % 97) as f32 - 48.0))
        .collect();
    for (rate, span) in [
        (SamplesPerChip::Two, 32),
        (SamplesPerChip::Four, 16),
        (SamplesPerChip::Eight, 8),
    ] {
        for origin in [91, u64::MAX - 4096] {
            let mut full = ReceiveFilter::full_rate(rate, span, at(origin)).unwrap();
            let mut expected = vec![Complex32::default(); full.output_length(input.len()).unwrap()];
            full.process(at(origin), &input, &mut expected).unwrap();
            for chunk in 1..=17 {
                let mut split = ReceiveFilter::full_rate(rate, span, at(0)).unwrap();
                let mut warmup =
                    vec![Complex32::default(); split.output_length(input.len()).unwrap()];
                split.process(at(0), &input, &mut warmup).unwrap();
                split.reset(at(origin));
                let mut actual = Vec::new();
                for block in input.chunks(chunk) {
                    let mut output =
                        vec![Complex32::default(); split.output_length(block.len()).unwrap()];
                    split
                        .process(split.next_input(), block, &mut output)
                        .unwrap();
                    actual.extend(output);
                }
                assert_eq!(actual.len(), expected.len());
                for (actual, expected) in actual.iter().zip(&expected) {
                    assert_eq!(actual.re.to_bits(), expected.re.to_bits());
                    assert_eq!(actual.im.to_bits(), expected.im.to_bits());
                }
                assert_eq!(split.next_input(), at(origin + input.len() as u64));
            }
        }
    }
}
