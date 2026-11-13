// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn at(index: u64) -> SampleInstant {
    SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn tone(time: f64, frequency: f64) -> Complex32 {
    let angle = std::f64::consts::TAU * time * frequency;
    Complex32::new(angle.cos() as f32, angle.sin() as f32)
}

#[test]
fn parallel_outputs_preserve_scalar_bits_for_every_phase_and_block_remainder() {
    let sampler = FractionalSampler::new();
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for gain in [1e-12_f32, 1.0, 1e6] {
            let input: Vec<_> = (0..192)
                .map(|i| {
                    Complex32::new(
                        ((i * 17 % 101) as f32 - 50.0) * gain,
                        ((i * 29 % 97) as f32 - 48.0) * gain,
                    )
                })
                .collect();
            let mut storage = crate::sample_validation::FiniteSampleBuffer::zeroed(input.len());
            storage
                .write(0, FiniteSamples::new(&input).unwrap())
                .unwrap();
            for phase in 0..PHASES {
                for length in 1..=17 {
                    let mut output = [Complex32::default(); 17];
                    sampler
                        .write_chips(
                            at(0),
                            &input,
                            SamplePosition::new(at(RADIUS as u64), phase as f64 / PHASES as f64)
                                .unwrap(),
                            rate,
                            &mut output[..length],
                        )
                        .unwrap();
                    let mut reused = [Complex32::default(); 17];
                    sampler
                        .write_validated_chips(
                            at(0),
                            storage.window(0..input.len()).unwrap(),
                            SamplePosition::new(at(RADIUS as u64), phase as f64 / PHASES as f64)
                                .unwrap(),
                            rate,
                            &mut reused[..length],
                        )
                        .unwrap();
                    for (a, b) in output[..length].iter().zip(&reused[..length]) {
                        assert_eq!(a.re.to_bits(), b.re.to_bits());
                        assert_eq!(a.im.to_bits(), b.im.to_bits());
                    }
                    for (index, actual) in output[..length].iter().enumerate() {
                        let start = index * rate.get();
                        let expected = input[start..start + TAPS]
                            .iter()
                            .zip(&sampler.weights[phase])
                            .fold(Complex64::default(), |sum, (value, weight)| {
                                sum + Complex64::new(f64::from(value.re), f64::from(value.im))
                                    * weight
                            });
                        assert_eq!(actual.re.to_bits(), (expected.re as f32).to_bits());
                        assert_eq!(actual.im.to_bits(), (expected.im as f32).to_bits());
                    }
                }
            }
        }
    }
}

#[test]
fn owned_windows_preserve_prefix_amplitude_rejection_and_unused_tail_scope() {
    let sampler = FractionalSampler::new();
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for fraction in [0.0, 0.2, 0.5, 0.9999] {
            for index in [0, RADIUS, TAPS + 19 * rate.get() - 1, 255] {
                let mut input = vec![Complex32::new(0.2, -0.3); 256];
                input[index] = Complex32::new(f32::MAX, -f32::MAX);
                let mut buffer = crate::sample_validation::FiniteSampleBuffer::zeroed(input.len());
                buffer
                    .write(0, FiniteSamples::new(&input).unwrap())
                    .unwrap();
                let evidence = buffer.window(0..input.len()).unwrap();
                assert_eq!(evidence.component_bound(), None);
                let first = SamplePosition::new(at(RADIUS as u64), fraction).unwrap();
                let sentinel = Complex32::new(13.0, -17.0);
                let mut raw = [sentinel; 4];
                let mut reused = raw;
                let expected = sampler.write_prefix_chips(at(0), &input, first, rate, 20, &mut raw);
                let actual = sampler.write_validated_prefix_chips(
                    at(0),
                    evidence,
                    first,
                    rate,
                    20,
                    &mut reused,
                );
                assert_eq!(actual, expected);
                for (a, b) in raw.iter().zip(reused) {
                    assert_eq!(a.re.to_bits(), b.re.to_bits());
                    assert_eq!(a.im.to_bits(), b.im.to_bits());
                }
                if expected.is_err() {
                    assert_eq!(raw, [sentinel; 4]);
                }
            }
        }
    }
}

#[test]
fn fractional_chips_recover_bandlimited_tones_at_all_supported_rates_and_gains() {
    let sampler = FractionalSampler::new();
    // Peak complex error budgets for the nine-tap Hann kernel across the occupied band.
    for (rate, maximum_error) in [
        (SamplesPerChip::Two, 0.017),
        (SamplesPerChip::Four, 0.008),
        (SamplesPerChip::Eight, 0.005),
    ] {
        for gain in [1e-12_f32, 1.0, 1e6] {
            for chip_frequency in [-0.61, -0.5, -0.25, 0.0, 0.25, 0.5, 0.61] {
                let frequency = chip_frequency / rate.get() as f64;
                let input: Vec<_> = (0..512).map(|i| tone(i as f64, frequency) * gain).collect();
                for fraction in (0..PHASES)
                    .map(|phase| phase as f64 / PHASES as f64)
                    .chain([0.173, 0.837, 0.9999])
                {
                    let first = SamplePosition::new(at(1032), fraction).unwrap();
                    let mut output = [Complex32::default(); 40];
                    let actual = sampler
                        .write_chips(at(1000), &input, first, rate, &mut output)
                        .unwrap();
                    for (chip, value) in output.iter().enumerate() {
                        let time = (actual.sample().index - 1000) as f64
                            + actual.fraction()
                            + (chip * rate.get()) as f64;
                        let error = (*value / gain - tone(time, frequency)).norm();
                        assert!(
                            error < maximum_error,
                            "rate {} phase {fraction} frequency {chip_frequency} gain {gain}: error {error}",
                            rate.get()
                        );
                    }
                    let displacement = (actual.sample().index - first.sample().index) as f64
                        + actual.fraction()
                        - fraction;
                    assert!(displacement.abs() <= 0.5 / PHASES as f64);
                }
            }
        }
    }
}

#[test]
fn interpolation_preserves_dc_and_large_integer_timestamps_without_allocating_output() {
    let sampler = FractionalSampler::new();
    let start = at((1_u64 << 60) + 37);
    let value = Complex32::new(0.21, -0.37);
    let input = [value; 128];
    for fraction in [0.0, 0.25, 0.6, 0.9999] {
        let first = SamplePosition::new(at(start.index + 17), fraction).unwrap();
        let mut output = [Complex32::default(); 8];
        let actual = sampler
            .write_chips(start, &input, first, SamplesPerChip::Eight, &mut output)
            .unwrap();
        assert_eq!(
            actual.sample().index,
            first.sample().index + u64::from(fraction > 0.9995)
        );
        for sample in output {
            assert!((sample - value).norm() < 1e-7);
        }
    }
    assert!(sampler.storage_bytes() < 300_000);
}

#[test]
fn missing_guards_wrong_epochs_and_invalid_samples_do_not_publish_partial_output() {
    let sampler = FractionalSampler::new();
    let mut input = [Complex32::new(1.0, 0.0); 64];
    let mut output = [Complex32::new(9.0, 8.0); 4];
    let saved = output;
    let rate = SamplesPerChip::Eight;
    let last_center = input.len() - 1 - RADIUS - (output.len() - 1) * rate.get();
    for center in [0, RADIUS as u64 - 1, last_center as u64 + 1, u64::MAX] {
        assert!(
            sampler
                .write_chips(
                    at(0),
                    &input,
                    SamplePosition::new(at(center), 0.5).unwrap(),
                    SamplesPerChip::Eight,
                    &mut output
                )
                .is_err()
        );
        assert_eq!(output, saved);
    }
    for center in [RADIUS, last_center] {
        let mut valid = saved;
        sampler
            .write_chips(
                at(0),
                &input,
                SamplePosition::new(at(center as u64), 0.5).unwrap(),
                rate,
                &mut valid,
            )
            .unwrap();
        assert!(
            valid
                .iter()
                .all(|sample| (*sample - input[0]).norm() < 1e-7)
        );
    }
    assert_eq!(
        sampler.write_chips(
            at(0),
            &input,
            SamplePosition::new(at(last_center as u64), 0.9999).unwrap(),
            rate,
            &mut output,
        ),
        Err(DspError::InvalidLength)
    );
    assert_eq!(output, saved);
    let first = SamplePosition::new(at(RADIUS as u64), 0.5).unwrap();
    let last_required = (output.len() - 1) * rate.get() + TAPS - 1;
    input[last_required].im = f32::NAN;
    assert_eq!(
        sampler.write_chips(at(0), &input, first, SamplesPerChip::Eight, &mut output),
        Err(DspError::NonFiniteEvidence)
    );
    assert_eq!(output, saved);
    input[last_required].im = f32::MAX;
    assert_eq!(
        sampler.write_chips(at(0), &input, first, SamplesPerChip::Eight, &mut output),
        Err(DspError::InvalidAmplitude)
    );
    input[last_required].im = 0.0;
    let other = SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 0,
    };
    assert_eq!(
        sampler.write_chips(other, &input, first, SamplesPerChip::Eight, &mut output),
        Err(DspError::DiscontinuousTime)
    );
    sampler
        .write_chips(at(0), &input, first, SamplesPerChip::Eight, &mut output)
        .unwrap();
    for fraction in [-0.1, 1.0, f64::NAN, f64::INFINITY] {
        assert!(SamplePosition::new(at(0), fraction).is_err());
    }
}

#[test]
fn splitting_readonly_input_keeps_the_same_guarded_samples() {
    let sampler = FractionalSampler::new();
    let input: Vec<_> = (0..300).map(|i| tone(i as f64, 0.27)).collect();
    let first = SamplePosition::new(at(60), 0.37).unwrap();
    let mut whole = [Complex32::default(); 40];
    let mut split = whole;
    sampler
        .write_chips(at(0), &input, first, SamplesPerChip::Four, &mut whole)
        .unwrap();
    sampler
        .write_chips(
            at(0),
            &input[..160],
            first,
            SamplesPerChip::Four,
            &mut split[..20],
        )
        .unwrap();
    sampler
        .write_chips(
            at(100),
            &input[100..],
            SamplePosition::new(at(140), 0.37).unwrap(),
            SamplesPerChip::Four,
            &mut split[20..],
        )
        .unwrap();
    assert_eq!(whole, split);
}

#[test]
fn pilot_prefix_preserves_full_interpolation_bits_and_validates_unused_tail() {
    let sampler = FractionalSampler::new();
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        let mut input: Vec<_> = (0..400)
            .map(|i| Complex32::new((i * 17 % 101) as f32 - 50.0, (i * 29 % 97) as f32 - 48.0))
            .collect();
        for phase in 0..PHASES {
            let first =
                SamplePosition::new(at(RADIUS as u64), phase as f64 / PHASES as f64).unwrap();
            let mut whole = [Complex32::default(); 40];
            let position = sampler
                .write_chips(at(0), &input, first, rate, &mut whole)
                .unwrap();
            for length in 1..=17 {
                let mut prefix = [Complex32::default(); 17];
                let actual = sampler
                    .write_prefix_chips(at(0), &input, first, rate, 40, &mut prefix[..length])
                    .unwrap();
                assert_eq!(actual, position);
                for (actual, expected) in prefix[..length].iter().zip(&whole) {
                    assert_eq!(actual.re.to_bits(), expected.re.to_bits());
                    assert_eq!(actual.im.to_bits(), expected.im.to_bits());
                }
            }
        }
        let first = SamplePosition::new(at(RADIUS as u64), 0.5).unwrap();
        let tail = 39 * rate.get() + TAPS - 1;
        let saved = input[tail];
        let sentinel = [Complex32::new(9.0, 8.0); 8];
        for (bad, error) in [
            (f32::NAN, DspError::NonFiniteEvidence),
            (f32::MAX, DspError::InvalidAmplitude),
        ] {
            input[tail].im = bad;
            let mut prefix = sentinel;
            assert_eq!(
                sampler.write_prefix_chips(at(0), &input, first, rate, 40, &mut prefix),
                Err(error)
            );
            assert_eq!(prefix, sentinel);
        }
        input[tail] = saved;
        for validated in [0, 7, usize::MAX] {
            let mut prefix = sentinel;
            assert_eq!(
                sampler.write_prefix_chips(at(0), &input, first, rate, validated, &mut prefix),
                Err(DspError::InvalidLength)
            );
            assert_eq!(prefix, sentinel);
        }
        let mut prefix = sentinel;
        assert_eq!(
            sampler.write_prefix_chips(at(0), &input[..tail], first, rate, 40, &mut prefix),
            Err(DspError::InvalidLength)
        );
        assert_eq!(prefix, sentinel);
    }
}

#[test]
fn vector_validation_matches_scalar_bounds_and_first_error_order() {
    let values = [
        0.0,
        -0.0,
        f32::from_bits(1),
        -f32::from_bits(1),
        1.0,
        f32::from_bits(1.0_f32.to_bits() + 1),
        -f32::MAX,
        f32::MAX,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        f32::from_bits(0x7f80_0001),
        f32::from_bits(0xff80_0001),
    ];
    for maximum in [
        0.0,
        1e-45,
        1.0,
        1.0 + 1e-8,
        1.0 + 1e-7,
        f64::from(f32::MAX) / std::f64::consts::PI,
        f64::from(f32::MAX),
        f64::from(f32::MAX) * 2.0,
    ] {
        for length in 0..=17 {
            let mut input = vec![Complex32::default(); length];
            assert_eq!(
                validate_samples(&input, maximum),
                validate_samples_scalar(&input, maximum)
            );
            for index in 0..length {
                for &re in &values {
                    for &im in &values {
                        input[index] = Complex32::new(re, im);
                        assert_eq!(
                            validate_samples(&input, maximum),
                            validate_samples_scalar(&input, maximum)
                        );
                        input[index] = Complex32::default();
                    }
                }
                input[index].re = f32::MAX;
                for later in index..length {
                    input[later].im = f32::NAN;
                    assert_eq!(
                        validate_samples(&input, maximum),
                        validate_samples_scalar(&input, maximum)
                    );
                    input[later].im = 0.0;
                }
                input[index] = Complex32::default();
            }
        }
    }
}

#[test]
fn larger_outputs_preserve_scalar_bits_across_block_boundaries() {
    let sampler = FractionalSampler::new();
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for gain in [1e-12_f32, 1.0, 1e6] {
            let input: Vec<_> = (0..1024)
                .map(|i| {
                    Complex32::new(
                        ((i * 17 % 101) as f32 - 50.0) * gain,
                        ((i * 29 % 97) as f32 - 48.0) * gain,
                    )
                })
                .collect();
            for fraction in [0.0, 0.173, 0.5, 0.9999] {
                for length in [31, 32, 33, 39, 40, 63, 64, 65, 72] {
                    let mut output = vec![Complex32::default(); length];
                    let actual = sampler
                        .write_chips(
                            at(0),
                            &input,
                            SamplePosition::new(at(RADIUS as u64), fraction).unwrap(),
                            rate,
                            &mut output,
                        )
                        .unwrap();
                    let phase = (actual.fraction() * PHASES as f64).round() as usize;
                    let begin = actual.sample().index as usize - RADIUS;
                    for (index, value) in output.iter().enumerate() {
                        let start = begin + index * rate.get();
                        let expected = input[start..start + TAPS]
                            .iter()
                            .zip(&sampler.weights[phase])
                            .fold(Complex64::default(), |sum, (value, weight)| {
                                sum + Complex64::new(f64::from(value.re), f64::from(value.im))
                                    * weight
                            });
                        assert_eq!(value.re.to_bits(), (expected.re as f32).to_bits());
                        assert_eq!(value.im.to_bits(), (expected.im as f32).to_bits());
                    }
                }
            }
        }
    }
}
