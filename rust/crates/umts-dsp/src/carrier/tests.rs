// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;
use umts_types::RadioEpoch;

use super::*;

fn at(index: u64) -> SampleInstant {
    SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

#[test]
fn double_precision_rotation_preserves_scalar_samples_and_state() {
    let rate = SampleRate::new(7_680_000, 1).unwrap();
    for hz in [0.0, 1e-6, -1734.5, 3_000_000.0] {
        let mut actual =
            CarrierCorrector::new(rate, CarrierOffsetHz::new(hz).unwrap(), -0.8, at(91)).unwrap();
        let mut reference =
            CarrierCorrector::new(rate, CarrierOffsetHz::new(hz).unwrap(), -0.8, at(91)).unwrap();
        for count in [0, 1, 254, 1, 1, 257, 5120, 5119] {
            let mut samples: Vec<_> = (0..count)
                .map(|i| {
                    let gain = [1e-100, 1.0, 1e100][i % 3];
                    Complex64::new((i % 17) as f64 * gain, -(i as f64 % 13.0) * gain)
                })
                .collect();
            let expected: Vec<_> = samples
                .iter()
                .map(|&value| reference.rotate(value))
                .collect();
            actual.process64(actual.next_input(), &mut samples).unwrap();
            for (a, b) in samples.iter().zip(expected) {
                assert_eq!(
                    (a.re.to_bits(), a.im.to_bits()),
                    (b.re.to_bits(), b.im.to_bits())
                );
            }
            assert_eq!(actual.phase, reference.phase);
            assert_eq!(actual.until_anchor, reference.until_anchor);
            assert_eq!(
                (
                    actual.oscillator.re.to_bits(),
                    actual.oscillator.im.to_bits()
                ),
                (
                    reference.oscillator.re.to_bits(),
                    reference.oscillator.im.to_bits()
                )
            );
            let offset = CarrierOffsetHz::new(-hz).unwrap();
            actual.set_offset(offset).unwrap();
            reference.set_offset(offset).unwrap();
        }
    }
}

#[test]
fn signed_offsets_match_direct_trigonometry_across_wrap_and_large_clock_origins() {
    for hz in [0.0, 1e-6, -1e-6, 1734.5, -3210.75, 3_000_000.0] {
        let rate = SampleRate::new(7_680_000, 1).unwrap();
        let mut corrector = CarrierCorrector::new(
            rate,
            CarrierOffsetHz::new(hz).unwrap(),
            -0.8,
            at(u64::MAX - 100_000),
        )
        .unwrap();
        let original = Complex32::new(0.7, -0.3);
        let mut input = vec![original; 80_000];
        corrector
            .process(corrector.next_input(), &mut input)
            .unwrap();
        for (index, &actual) in input.iter().enumerate() {
            let expected = Complex64::new(f64::from(original.re), f64::from(original.im))
                * Complex64::from_polar(
                    1.0,
                    -0.8 - std::f64::consts::TAU * hz * index as f64 / rate.hz(),
                );
            assert!(
                (Complex64::new(f64::from(actual.re), f64::from(actual.im)) - expected).norm()
                    < 1e-7,
                "hz={hz} index={index}"
            );
        }
    }
}

#[test]
fn streaming_partitions_are_bit_exact_and_frequency_updates_preserve_phase() {
    let rate = SampleRate::new(15_360_000, 1).unwrap();
    let mut full =
        CarrierCorrector::new(rate, CarrierOffsetHz::new(1250.0).unwrap(), 0.7, at(91)).unwrap();
    let mut split =
        CarrierCorrector::new(rate, CarrierOffsetHz::new(1250.0).unwrap(), 0.7, at(91)).unwrap();
    let mut expected = vec![Complex32::new(1.0, 0.0); 10_000];
    let mut actual = expected.clone();
    full.process(at(91), &mut expected).unwrap();
    let mut offset = 0;
    for size in [0, 1, 254, 0, 1, 1, 777, 8966] {
        split
            .process(at(91 + offset as u64), &mut actual[offset..offset + size])
            .unwrap();
        offset += size;
    }
    assert_eq!(offset, actual.len());
    assert_eq!(actual, expected);
    split
        .set_offset(CarrierOffsetHz::new(-2400.0).unwrap())
        .unwrap();
    let mut following = [Complex32::new(1.0, 0.0); 1000];
    split.process(split.next_input(), &mut following).unwrap();
    for (index, value) in following.iter().enumerate() {
        let phase =
            0.7 - std::f64::consts::TAU * (1250.0 * 10_000.0 - 2400.0 * index as f64) / rate.hz();
        assert!((*value - Complex32::from_polar(1.0, phase as f32)).norm() < 1e-6);
    }
}

#[test]
fn gain_and_received_frequency_are_corrected_without_amplitude_control() {
    let rate = SampleRate::new(30_720_000, 1).unwrap();
    for gain in [1e-20, 1e-3, 1.0, 100.0] {
        let mut input: Vec<_> = (0..30_720)
            .map(|index| {
                let phase = 0.3 + std::f64::consts::TAU * 2017.0 * index as f64 / rate.hz();
                let value = Complex64::from_polar(gain, phase);
                Complex32::new(value.re as f32, value.im as f32)
            })
            .collect();
        let mut corrector =
            CarrierCorrector::new(rate, CarrierOffsetHz::new(2017.0).unwrap(), -0.3, at(0))
                .unwrap();
        corrector.process(at(0), &mut input).unwrap();
        for value in input {
            assert!(
                (Complex64::new(f64::from(value.re), f64::from(value.im)) / gain
                    - Complex64::new(1.0, 0.0))
                .norm()
                    < 1e-7
            );
        }
    }
}

#[test]
fn invalid_calls_and_clock_gaps_do_not_change_samples_or_phase() {
    let rate = SampleRate::new(7_680_000, 1).unwrap();
    assert!(CarrierOffsetHz::new(f64::NAN).is_err());
    assert!(
        CarrierCorrector::new(rate, CarrierOffsetHz::new(3_840_000.0).unwrap(), 0.0, at(0))
            .is_err()
    );
    assert!(
        CarrierCorrector::new(
            rate,
            CarrierOffsetHz::new(0.0).unwrap(),
            f64::INFINITY,
            at(0)
        )
        .is_err()
    );
    let mut corrector =
        CarrierCorrector::new(rate, CarrierOffsetHz::new(1200.0).unwrap(), 0.3, at(0)).unwrap();
    let mut samples = [Complex32::new(1.0, 0.0); 100];
    assert_eq!(
        corrector.process(at(1), &mut samples),
        Err(DspError::DiscontinuousTime)
    );
    assert_eq!(samples, [Complex32::new(1.0, 0.0); 100]);
    samples[99].re = f32::INFINITY;
    assert_eq!(
        corrector.process(at(0), &mut samples),
        Err(DspError::NonFiniteEvidence)
    );
    assert_eq!(samples[0], Complex32::new(1.0, 0.0));
    samples[99] = Complex32::new(f32::MAX, f32::MAX);
    assert_eq!(
        corrector.process(at(0), &mut samples),
        Err(DspError::InvalidAmplitude)
    );
    assert!(
        corrector
            .set_offset(CarrierOffsetHz::new(4e6).unwrap())
            .is_err()
    );
    assert_eq!(corrector.offset().hz(), 1200.0);
    assert!(corrector.reset(at(100), f64::NAN).is_err());
    assert_eq!(corrector.next_input(), at(0));
    samples.fill(Complex32::new(1.0, 0.0));
    corrector.process(at(0), &mut samples).unwrap();
    assert!((samples[0] - Complex32::from_polar(1.0, 0.3)).norm() < 1e-7);
    let new_origin = SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 500,
    };
    assert_eq!(
        corrector.process(new_origin, &mut samples),
        Err(DspError::DiscontinuousTime)
    );
    corrector.reset(new_origin, 0.0).unwrap();
    let before_reset_processing = samples[0];
    corrector.process(new_origin, &mut samples).unwrap();
    assert_eq!(samples[0], before_reset_processing);
    corrector.reset(at(u64::MAX - 1), 0.0).unwrap();
    assert_eq!(
        corrector.process(corrector.next_input(), &mut samples),
        Err(DspError::InvalidLength)
    );
}

#[test]
fn double_precision_invalid_tail_is_atomic_and_preserves_error_priority() {
    fn rejected(
        corrector: &mut CarrierCorrector,
        start: SampleInstant,
        samples: &mut [Complex64],
        error: DspError,
    ) {
        let bits = |values: &[Complex64]| {
            values
                .iter()
                .map(|v| (v.re.to_bits(), v.im.to_bits()))
                .collect::<Vec<_>>()
        };
        let before = bits(samples);
        let state = (
            corrector.next,
            corrector.phase,
            corrector.until_anchor,
            corrector.oscillator.re.to_bits(),
            corrector.oscillator.im.to_bits(),
        );
        assert_eq!(corrector.process64(start, samples), Err(error));
        assert_eq!(bits(samples), before);
        assert_eq!(
            (
                corrector.next,
                corrector.phase,
                corrector.until_anchor,
                corrector.oscillator.re.to_bits(),
                corrector.oscillator.im.to_bits()
            ),
            state
        );
    }
    let rate = SampleRate::new(7_680_000, 1).unwrap();
    let mut corrector =
        CarrierCorrector::new(rate, CarrierOffsetHz::new(-1734.5).unwrap(), -0.8, at(91)).unwrap();
    let mut samples = [Complex64::new(1.0, -2.0); 300];
    rejected(
        &mut corrector,
        at(92),
        &mut samples,
        DspError::DiscontinuousTime,
    );
    samples[0].re = f64::MAX;
    samples[299].im = f64::NAN;
    rejected(
        &mut corrector,
        at(91),
        &mut samples,
        DspError::NonFiniteEvidence,
    );
    samples[299].im = 0.0;
    rejected(
        &mut corrector,
        at(91),
        &mut samples,
        DspError::InvalidAmplitude,
    );
    let mut near_limit = CarrierCorrector::new(
        rate,
        CarrierOffsetHz::new(0.0).unwrap(),
        0.0,
        at(u64::MAX - 100),
    )
    .unwrap();
    rejected(
        &mut near_limit,
        at(u64::MAX - 100),
        &mut samples,
        DspError::InvalidLength,
    );
}
