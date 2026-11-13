// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;
use umts_types::RadioEpoch;

use super::*;
use crate::{
    carrier::{CarrierCorrector, CarrierOffsetHz},
    prach::PreambleProbe,
};
use umts_types::time::SampleInstant;

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

#[test]
fn fft_matches_direct_correlation_at_every_delay_and_signature() {
    let code = PrachCode::new(7504).unwrap();
    for span in [0..256, 257..769] {
        for delays in [1, 17, 256] {
            let plan =
                Arc::new(PreambleSearchPlan::new(code, u16::MAX, span.clone(), delays).unwrap());
            let mut search = PreambleTimingSearch::new(plan.clone());
            let input: Vec<_> = (0..plan.input_length())
                .map(|i| Complex32::new((i % 7) as f32 - 3.0, (i % 11) as f32 - 5.0))
                .collect();
            let window = search.search(at(10_000), &input).unwrap();
            for signature in 0..16 {
                let signature = PreambleSignature::new(signature).unwrap();
                let mut reference = vec![Complex32::default(); PREAMBLE_CHIPS];
                code.write_preamble(signature, &mut reference).unwrap();
                for delay in 0..delays {
                    let mut direct = Complex64::default();
                    let mut energy = 0.0;
                    for (&value, &reference) in input[delay..delay + span.len()]
                        .iter()
                        .zip(&reference[span.clone()])
                    {
                        let value = Complex64::new(f64::from(value.re), f64::from(value.im));
                        direct += value
                            * Complex64::new(f64::from(reference.re), -f64::from(reference.im));
                        energy += value.norm_sqr();
                    }
                    direct /= span.len() as f64;
                    let actual = window.correlation(delay, signature).unwrap().unwrap();
                    assert_eq!(actual.correlation_start, at(10_000 + delay as u64));
                    assert_eq!(actual.preamble_offset, span.start);
                    assert!((actual.amplitude - direct).norm() < 1e-12);
                    assert!(
                        (actual.coherent_fraction - direct.norm_sqr() * span.len() as f64 / energy)
                            .abs()
                            < 1e-12
                    );
                }
            }
        }
    }
}

#[test]
fn fft_length_is_the_smallest_supported_smooth_size_without_truncation() {
    for required in 256..=PREAMBLE_CHIPS + MAX_SEARCH_DELAYS - 1 {
        let length = smooth_fft_length(required);
        assert!(length >= required);
        assert!(length <= required.next_power_of_two());
        let mut remaining = length;
        for radix in [2, 3, 5] {
            while remaining.is_multiple_of(radix) {
                remaining /= radix;
            }
        }
        assert_eq!(remaining, 1);
        if required > 256 && length != smooth_fft_length(required - 1) {
            assert_eq!(smooth_fft_length(required - 1), required - 1);
        }
    }
    assert_eq!(smooth_fft_length(4195), 4320);
    assert_eq!(smooth_fft_length(9215), 9216);
}

#[test]
fn full_preambles_at_search_edges_and_simultaneous_signatures_keep_gain() {
    let code = PrachCode::new(8191).unwrap();
    let plan = Arc::new(
        PreambleSearchPlan::new(code, u16::MAX, 0..PREAMBLE_CHIPS, MAX_SEARCH_DELAYS).unwrap(),
    );
    let mut first = PreambleTimingSearch::new(plan.clone());
    let mut second = PreambleTimingSearch::new(plan.clone());
    let mut preamble = vec![Complex32::default(); PREAMBLE_CHIPS];
    code.write_preamble(PreambleSignature::new(13).unwrap(), &mut preamble)
        .unwrap();
    for delay in [0, MAX_SEARCH_DELAYS - 1] {
        for gain in [1e-20_f32, 1.0, 1e20] {
            let mut input = vec![Complex32::default(); plan.input_length()];
            for (value, &chip) in input[delay..delay + PREAMBLE_CHIPS]
                .iter_mut()
                .zip(&preamble)
            {
                *value = chip * Complex32::new(gain, -gain * 0.5);
            }
            let window = first.search(at(20_000), &input).unwrap();
            let best = window
                .strongest(PreambleSignature::new(13).unwrap())
                .unwrap();
            assert_eq!(best.correlation_start, at(20_000 + delay as u64));
            assert!((best.coherent_fraction - 1.0).abs() < 1e-6);
            assert!((best.amplitude / f64::from(gain) - Complex64::new(1.0, -0.5)).norm() < 1e-6);
            let other = second
                .search(at(30_000), &input)
                .unwrap()
                .strongest(PreambleSignature::new(13).unwrap())
                .unwrap();
            assert_eq!(other.coherent_fraction, best.coherent_fraction);
        }
    }
    let mut input = vec![Complex32::default(); plan.input_length()];
    for (signature, gain) in [(3, 0.25), (13, 1.0)] {
        code.write_preamble(PreambleSignature::new(signature).unwrap(), &mut preamble)
            .unwrap();
        for (value, &chip) in input[57..57 + PREAMBLE_CHIPS].iter_mut().zip(&preamble) {
            *value += chip * gain;
        }
    }
    let window = first.search(at(20_000), &input).unwrap();
    for (signature, expected) in [(3, 0.25_f64), (13, 1.0)] {
        let best = window
            .strongest(PreambleSignature::new(signature).unwrap())
            .unwrap();
        assert_eq!(best.correlation_start, at(20_057));
        assert!((best.amplitude - Complex64::new(expected, 0.0)).norm() < 1e-6);
    }
}

#[test]
fn phase_slope_then_carrier_correction_restores_coherent_search() {
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(13).unwrap();
    let mut preamble = vec![Complex32::default(); PREAMBLE_CHIPS];
    code.write_preamble(signature, &mut preamble).unwrap();
    let rate = umts_types::time::SampleRate::new(3_840_000, 1).unwrap();
    for hz in [-3100.0, 2100.0] {
        let mut received: Vec<_> = preamble
            .iter()
            .enumerate()
            .map(|(i, value)| {
                value
                    * Complex32::from_polar(
                        0.05,
                        (std::f64::consts::TAU * hz * i as f64 / rate.hz()) as f32,
                    )
            })
            .collect();
        let probe = PreambleProbe::new(code, 1 << 13).unwrap();
        let result = probe.correlate(&received).unwrap()[13].unwrap();
        assert!((result.phase_slope_hz.unwrap() - hz).abs() < 0.01);
        let mut corrector = CarrierCorrector::new(
            rate,
            CarrierOffsetHz::new(result.phase_slope_hz.unwrap()).unwrap(),
            0.0,
            SampleInstant {
                epoch: at(0).epoch,
                index: 0,
            },
        )
        .unwrap();
        corrector
            .process(corrector.next_input(), &mut received)
            .unwrap();
        let plan = Arc::new(PreambleSearchPlan::new(code, 1 << 13, 0..PREAMBLE_CHIPS, 1).unwrap());
        let mut search = PreambleTimingSearch::new(plan);
        let best = search
            .search(at(0), &received)
            .unwrap()
            .strongest(signature)
            .unwrap();
        assert!(best.coherent_fraction > 0.99999);
    }
}

#[test]
fn residual_frequency_loss_matches_the_analytic_coherent_response() {
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(13).unwrap();
    let plan = Arc::new(PreambleSearchPlan::new(code, 1 << 13, 0..PREAMBLE_CHIPS, 32).unwrap());
    let mut search = PreambleTimingSearch::new(plan.clone());
    let mut preamble = vec![Complex32::default(); PREAMBLE_CHIPS];
    code.write_preamble(signature, &mut preamble).unwrap();
    for hz in [-210.0_f64, -85.0, 0.0, 85.0, 210.0] {
        let mut input = vec![Complex32::default(); plan.input_length()];
        for (i, (value, &chip)) in input[13..13 + PREAMBLE_CHIPS]
            .iter_mut()
            .zip(&preamble)
            .enumerate()
        {
            let phase = std::f64::consts::TAU * hz * i as f64 / 3_840_000.0;
            let rotated = Complex64::new(f64::from(chip.re), f64::from(chip.im))
                * Complex64::from_polar(0.1, phase);
            *value = Complex32::new(rotated.re as f32, rotated.im as f32);
        }
        let best = search
            .search(at(100), &input)
            .unwrap()
            .strongest(signature)
            .unwrap();
        assert_eq!(best.correlation_start, at(113));
        let half_step = std::f64::consts::PI * hz / 3_840_000.0;
        let expected = if hz == 0.0 {
            1.0
        } else {
            ((PREAMBLE_CHIPS as f64 * half_step).sin() / (PREAMBLE_CHIPS as f64 * half_step.sin()))
                .powi(2)
        };
        assert!((best.coherent_fraction - expected).abs() < 1e-6);
    }
}

#[test]
fn mean_input_power_scales_with_input_amplitude() {
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(0).unwrap();
    let plan = Arc::new(PreambleSearchPlan::new(code, 1, 0..256, 1).unwrap());
    let mut search = PreambleTimingSearch::new(plan.clone());

    let first = vec![Complex32::new(1.0, 0.0); plan.input_length()];
    let first = search
        .search(at(0), &first)
        .unwrap()
        .correlation(0, signature)
        .unwrap()
        .unwrap();
    let second = vec![Complex32::new(2.0, 0.0); plan.input_length()];
    let second = search
        .search(at(0), &second)
        .unwrap()
        .correlation(0, signature)
        .unwrap()
        .unwrap();

    assert!((first.mean_input_power - 1.0).abs() < 1e-12);
    assert!((second.mean_input_power - 4.0).abs() < 1e-12);
}

#[test]
fn mean_input_power_uses_the_selected_delay_window() {
    let code = PrachCode::new(7504).unwrap();
    let signature = PreambleSignature::new(0).unwrap();
    let plan = Arc::new(PreambleSearchPlan::new(code, 1, 0..256, 3).unwrap());
    let mut search = PreambleTimingSearch::new(plan.clone());
    let mut input = vec![Complex32::new(1.0, 0.0); plan.input_length()];
    input[256] = Complex32::new(2.0, 0.0);
    input[257] = Complex32::new(3.0, 0.0);
    let window = search.search(at(0), &input).unwrap();

    let first = window
        .correlation(0, signature)
        .unwrap()
        .unwrap()
        .mean_input_power;
    let second = window
        .correlation(1, signature)
        .unwrap()
        .unwrap()
        .mean_input_power;
    let third = window
        .correlation(2, signature)
        .unwrap()
        .unwrap()
        .mean_input_power;

    assert!((first - 1.0).abs() < 1e-12);
    assert!((second - 259.0 / 256.0).abs() < 1e-12);
    assert!((third - 267.0 / 256.0).abs() < 1e-12);
}

#[test]
fn malformed_searches_silence_and_masks_fail_closed_and_recover() {
    let code = PrachCode::new(0).unwrap();
    assert!(PreambleSearchPlan::new(code, 0, 0..4096, 1).is_err());
    for (span, delays) in [
        (0..0, 1),
        (0..255, 1),
        (1..4097, 1),
        (0..256, 0),
        (0..256, MAX_SEARCH_DELAYS + 1),
    ] {
        assert!(PreambleSearchPlan::new(code, 1, span, delays).is_err());
    }
    let plan = Arc::new(PreambleSearchPlan::new(code, 1, 0..4096, 7).unwrap());
    assert_eq!(plan.code(), code);
    assert_eq!(plan.span(), 0..4096);
    assert_eq!(plan.delay_count(), 7);
    let mut search = PreambleTimingSearch::new(plan.clone());
    let mut input = vec![Complex32::default(); plan.input_length()];
    assert!(search.search(at(0), &[]).is_err());
    assert!(search.search(at(u64::MAX - 1), &input).is_err());
    input[1000].re = f32::NAN;
    assert!(search.search(at(0), &input).is_err());
    input[1000].re = 0.0;
    let window = search.search(at(0), &input).unwrap();
    assert!(
        window
            .strongest(PreambleSignature::new(0).unwrap())
            .is_none()
    );
    assert!(
        window
            .correlation(0, PreambleSignature::new(1).unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        window
            .correlation(7, PreambleSignature::new(0).unwrap())
            .is_err()
    );
    code.write_preamble(PreambleSignature::new(0).unwrap(), &mut input[3..3 + 4096])
        .unwrap();
    let window = search.search(at(100), &input).unwrap();
    assert_eq!(
        window
            .strongest(PreambleSignature::new(0).unwrap())
            .unwrap()
            .correlation_start,
        at(103)
    );
}
