// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode};
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn at(index: u64) -> SampleInstant {
    SampleInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn code() -> UplinkDpchCode {
    UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap()
}

fn reference(format: UplinkControlFormat, slot: u8) -> Vec<Complex64> {
    let count = format.pilots().bits();
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    UplinkLongSequence::new(
        UplinkScramblingCode::new(0x123456).unwrap(),
        u32::from(slot) * CHIPS_PER_SLOT as u32,
    )
    .unwrap()
    .write(&mut scrambling);
    scrambling[..count * 256]
        .iter()
        .enumerate()
        .map(|(i, chip)| {
            let bit = (format.pilots().word(slot).unwrap() >> (count - 1 - i / 256)) & 1;
            Complex64::new(-f64::from(chip.im), f64::from(chip.re))
                * if bit == 0 { 1.0 } else { -1.0 }
        })
        .collect()
}

#[test]
fn interpolated_pilot_measurement_preserves_gain_and_carrier_invariance() {
    for number in 0..=4 {
        let format = UplinkControlFormat::new(number).unwrap();
        let slot = number * 3;
        let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, slot, 2, 17).unwrap());
        let mut search = DedicatedTimingSearch::new(plan);
        for gain in [0.001, 1.0, 100.0] {
            for hz in [-1200.0, 0.0, 1200.0] {
                let chips: Vec<_> = reference(format, slot)
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let phase =
                            std::f64::consts::TAU * hz * index as f64 / CHIPS_PER_SECOND as f64;
                        let value = value * Complex64::from_polar(gain, phase);
                        Complex32::new(value.re as f32, value.im as f32)
                    })
                    .collect();
                let coherence = search
                    .interpolated_pilot_coherence(
                        at(137),
                        &chips,
                        CarrierOffsetHz::new(hz).unwrap(),
                    )
                    .unwrap();
                assert!(coherence > 0.99999999);
                assert!(
                    search
                        .interpolated_pilot_coherence(
                            at(137),
                            &chips[..chips.len() - 1],
                            CarrierOffsetHz::new(hz).unwrap()
                        )
                        .is_err()
                );
            }
        }
        let mut zeros = vec![Complex32::default(); reference(format, slot).len()];
        let zero = CarrierOffsetHz::new(0.0).unwrap();
        assert_eq!(
            search
                .interpolated_pilot_coherence(at(0), &zeros, zero)
                .unwrap(),
            0.0
        );
        zeros[0].im = f32::NAN;
        assert!(
            search
                .interpolated_pilot_coherence(at(0), &zeros, zero)
                .is_err()
        );
    }
}

#[test]
fn following_slot_confirmation_uses_its_own_pilot_word_and_scrambling() {
    for number in 0..=4 {
        let format = UplinkControlFormat::new(number).unwrap();
        for slot in [0, 14] {
            let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, slot, 2, 17).unwrap());
            let mut search = DedicatedTimingSearch::new(plan);
            let next = (slot + 1) % 15;
            for gain in [0.001, 1.0, 100.0] {
                let chips: Vec<_> = reference(format, next)
                    .into_iter()
                    .map(|v| Complex32::new((v.re * gain) as f32, (v.im * gain) as f32))
                    .collect();
                let offset = CarrierOffsetHz::new(0.0).unwrap();
                assert!(
                    search
                        .following_pilot_coherence(at(0), &chips, offset)
                        .unwrap()
                        > 0.99999999
                );
                assert!(
                    search
                        .interpolated_pilot_coherence(at(0), &chips, offset)
                        .unwrap()
                        < 0.9
                );
                assert!(
                    search
                        .following_pilot_coherence(at(0), &chips[..chips.len() - 1], offset)
                        .is_err()
                );
            }
        }
    }
}

#[test]
fn third_slot_confirmation_preserves_slot_words_and_frame_wrap() {
    for number in 0..=4 {
        let format = UplinkControlFormat::new(number).unwrap();
        for slot in [0, 13, 14] {
            let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, slot, 2, 17).unwrap());
            let mut search = DedicatedTimingSearch::new(plan);
            for gain in [0.001, 1.0, 100.0] {
                let chips: Vec<_> = reference(format, (slot + 2) % 15)
                    .into_iter()
                    .map(|v| Complex32::new((v.re * gain) as f32, (v.im * gain) as f32))
                    .collect();
                let offset = CarrierOffsetHz::new(0.0).unwrap();
                assert!(search.third_pilot_coherence(at(0), &chips, offset).unwrap() > 0.99999999);
                assert!(
                    search
                        .third_pilot_coherence(at(0), &chips[..chips.len() - 1], offset)
                        .is_err()
                );
            }
        }
    }
}

#[test]
fn fft_matches_direct_complex_correlation_and_strided_energy_at_every_delay() {
    for rate in [1, 2, 4, 8] {
        for number in 0..=4 {
            let format = UplinkControlFormat::new(number).unwrap();
            let slot = number * 3;
            let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, slot, rate, 37).unwrap());
            let input: Vec<_> = (0..plan.input_length())
                .map(|i| {
                    Complex32::new(((i * 19) % 29) as f32 - 14.0, ((i * 7) % 31) as f32 - 15.0)
                })
                .collect();
            let mut search = DedicatedTimingSearch::new(plan);
            let window = search
                .search(at(137), &input, CarrierOffsetHz::new(0.0).unwrap())
                .unwrap();
            let reference = reference(format, slot);
            let reference_energy: f64 = reference.iter().map(|v| v.norm_sqr()).sum();
            assert!((reference_energy - 2.0 * reference.len() as f64).abs() < 1e-10);
            for delay in 0..37 {
                let mut dot = Complex64::default();
                let mut energy = 0.0;
                for (chip, reference) in reference.iter().enumerate() {
                    let value = input[delay + chip * rate];
                    let value = Complex64::new(f64::from(value.re), f64::from(value.im));
                    dot += value * reference.conj();
                    energy += value.norm_sqr();
                }
                let actual = window.at(delay).unwrap();
                let pilot_energy: f64 = reference
                    .chunks_exact(CONTROL_SF)
                    .enumerate()
                    .map(|(symbol, chips)| {
                        chips
                            .iter()
                            .enumerate()
                            .map(|(chip, reference)| {
                                let value = input[delay + (symbol * CONTROL_SF + chip) * rate];
                                Complex64::new(f64::from(value.re), f64::from(value.im))
                                    * reference.conj()
                            })
                            .sum::<Complex64>()
                            .norm_sqr()
                    })
                    .sum();
                let expected = dot.norm_sqr() / (format.pilots().bits() as f64 * pilot_energy);
                assert!((actual.despread_coherent_fraction - expected).abs() < 1e-12);
                assert!((actual.amplitude - dot / reference_energy).norm() < 1e-10);
                assert!(
                    (actual.coherent_fraction - dot.norm_sqr() / (energy * reference_energy)).abs()
                        < 1e-12
                );
                assert_eq!(actual.position.sample(), at(137 + delay as u64));
            }
            assert!(window.at(37).is_none());
        }
    }
}

#[test]
fn edges_sample_phases_cfo_and_gain_preserve_the_configured_pilot_peak() {
    let format = UplinkControlFormat::new(0).unwrap();
    let reference = reference(format, 7);
    for rate in [1, 2, 4, 8] {
        let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, 7, rate, 31).unwrap());
        let mut search = DedicatedTimingSearch::new(plan.clone());
        for delay in [0, 1, 7, 14, 30] {
            for gain in [1e-12, 0.5, 1e12] {
                let channel = Complex64::from_polar(gain, 0.713);
                let frequency = 1400.0;
                let mut input = vec![Complex32::default(); plan.input_length()];
                for (chip, value) in reference.iter().enumerate() {
                    let sample = delay + chip * rate;
                    let value = value
                        * channel
                        * Complex64::from_polar(
                            1.0,
                            std::f64::consts::TAU * frequency * sample as f64
                                / (CHIPS_PER_SECOND as f64 * rate as f64),
                        );
                    input[sample] = Complex32::new(value.re as f32, value.im as f32);
                }
                let peak = search
                    .search(at(999), &input, CarrierOffsetHz::new(frequency).unwrap())
                    .unwrap()
                    .strongest();
                assert_eq!(peak.position.sample(), at(999 + delay as u64));
                assert!(peak.coherent_fraction > 0.99999999);
                assert!(peak.despread_coherent_fraction > 0.99999999);
                assert!((peak.amplitude / channel - Complex64::new(1.0, 0.0)).norm() < 1e-7);
                assert!(peak.fractional_offset_samples.abs() <= 0.5);
            }
        }
    }
}

#[test]
fn despread_pilot_coherence_survives_stronger_orthogonal_traffic() {
    let format = UplinkControlFormat::new(0).unwrap();
    let reference = reference(format, 0);
    let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, 0, 2, 17).unwrap());
    let mut search = DedicatedTimingSearch::new(plan.clone());
    let delay = 8;
    for gain in [0.001, 1.0, 1000.0] {
        let mut input = vec![Complex32::default(); plan.input_length()];
        for (chip, pilot) in reference.iter().enumerate() {
            let data = if chip % 2 == 0 { 8.0 } else { -8.0 };
            let value = *pilot * gain * (1.0 + data);
            input[delay + chip * 2] = Complex32::new(value.re as f32, value.im as f32);
        }
        let peak = search
            .search(at(0), &input, CarrierOffsetHz::new(0.0).unwrap())
            .unwrap()
            .strongest();
        assert_eq!(peak.position.sample().index, delay as u64);
        assert!(peak.coherent_fraction < 0.02);
        assert!(peak.despread_coherent_fraction > 0.999999);
    }
}

#[test]
fn unrelated_noise_does_not_pass_the_pilot_tracking_probability_check() {
    const MULTIPLIER: u64 = 6364136223846793005;
    const DELAYS: usize = 17;
    let format = UplinkControlFormat::new(0).unwrap();
    let plan = Arc::new(DedicatedTimingPlan::new(&code(), format, 0, 2, DELAYS).unwrap());
    let mut search = DedicatedTimingSearch::new(plan.clone());
    let mut input = vec![Complex32::default(); plan.input_length()];
    let mut state = 1_u64;
    for _ in 0..256 {
        for sample in &mut input {
            let mut next = || {
                state = state.wrapping_mul(MULTIPLIER).wrapping_add(1);
                ((state >> 32) as u32 as f64 / u32::MAX as f64 - 0.5) as f32
            };
            *sample = Complex32::new(next(), next());
        }
        let peak = search
            .search(at(0), &input, CarrierOffsetHz::new(0.0).unwrap())
            .unwrap()
            .strongest();
        let probability = (1.0 - peak.despread_coherent_fraction)
            .powi((format.pilots().bits() - 1) as i32)
            * DELAYS as f64;
        assert!(peak.coherent_fraction < 0.05);
        assert!(probability > 1e-6);
    }
}

#[test]
fn bounded_geometry_and_invalid_retry_preserve_a_usable_workspace() {
    let code = code();
    let format = UplinkControlFormat::new(0).unwrap();
    for (slot, rate, delays) in [(15, 2, 1), (0, 3, 1), (0, 2, 0), (0, 8, 65537)] {
        assert!(DedicatedTimingPlan::new(&code, format, slot, rate, delays).is_err());
    }
    let plan = Arc::new(DedicatedTimingPlan::new(&code, format, 0, 2, 17).unwrap());
    let mut search = DedicatedTimingSearch::new(plan.clone());
    let mut input = vec![Complex32::default(); plan.input_length()];
    let zero = CarrierOffsetHz::new(0.0).unwrap();
    assert_eq!(
        search.search(at(0), &[], zero).err(),
        Some(DspError::InvalidLength)
    );
    input[77].im = f32::NAN;
    assert_eq!(
        search.search(at(0), &input, zero).err(),
        Some(DspError::NonFiniteEvidence)
    );
    input[77].im = 0.0;
    assert_eq!(
        search.search(at(u64::MAX), &input, zero).err(),
        Some(DspError::InvalidLength)
    );
    assert_eq!(
        search
            .search(at(0), &input, CarrierOffsetHz::new(4e6).unwrap())
            .err(),
        Some(DspError::InvalidFrequency)
    );
    let peak = search.search(at(0), &input, zero).unwrap().strongest();
    assert_eq!(peak.coherent_fraction, 0.0);
    assert_eq!(peak.despread_coherent_fraction, 0.0);
    assert_eq!(peak.amplitude, Complex64::default());
    assert!(search.storage_bytes() < 400_000);
}

#[test]
fn narrow_direct_search_matches_fft_across_formats_rates_cfo_and_gain() {
    for rate in [1, 2, 4, 8] {
        for number in 0..=4 {
            let format = UplinkControlFormat::new(number).unwrap();
            for delays in [1, 3, 4, 16, 17, 18, 37] {
                let plan =
                    Arc::new(DedicatedTimingPlan::new(&code(), format, 14, rate, delays).unwrap());
                let mut search = DedicatedTimingSearch::new(plan.clone());
                let mut fft_search = DedicatedTimingSearch::new(Arc::new(
                    DedicatedTimingPlan::new_inner(&code(), format, 14, rate, delays, true)
                        .unwrap(),
                ));
                for gain in [1e-12, 1.0, 1e12] {
                    for hz in [-1400.0, 0.0, 1400.0] {
                        let input: Vec<_> = (0..plan.input_length())
                            .map(|i| {
                                let noise = Complex64::new(
                                    ((i * 19) % 29) as f64 - 14.0,
                                    ((i * 7) % 31) as f64 - 15.0,
                                );
                                let value = noise
                                    * Complex64::from_polar(
                                        gain,
                                        std::f64::consts::TAU * hz * i as f64
                                            / (CHIPS_PER_SECOND as f64 * rate as f64),
                                    );
                                Complex32::new(value.re as f32, value.im as f32)
                            })
                            .collect();
                        let window = search
                            .search(at(137), &input, CarrierOffsetHz::new(hz).unwrap())
                            .unwrap();
                        let expected: Vec<_> =
                            (0..delays).map(|delay| window.at(delay).unwrap()).collect();
                        let peak = window.strongest();
                        // Rebuild the padded FFT input, including on the wide-search path.
                        let fft_baseline = fft_search
                            .search(at(137), &input, CarrierOffsetHz::new(hz).unwrap())
                            .unwrap();
                        for (delay, expected) in expected.iter().enumerate() {
                            assert_timing_bits_equal(fft_baseline.at(delay).unwrap(), *expected);
                        }
                        assert_timing_bits_equal(fft_baseline.strongest(), peak);
                        fft_search.spectrum.fill(Complex64::default());
                        fft_search.spectrum[..input.len()].copy_from_slice(&fft_search.corrected);
                        fft_search.correlate_fft();
                        let fft = PilotTimingWindow {
                            start: at(137),
                            search: &fft_search,
                        };
                        for (delay, expected) in expected.iter().enumerate() {
                            let actual = fft.at(delay).unwrap();
                            assert_eq!(actual.position, expected.position);
                            assert!((actual.amplitude - expected.amplitude).norm() / gain < 1e-10);
                            assert!(
                                (actual.coherent_fraction - expected.coherent_fraction).abs()
                                    < 1e-12
                            );
                            assert_eq!(
                                actual.despread_coherent_fraction.to_bits(),
                                expected.despread_coherent_fraction.to_bits()
                            );
                        }
                        let actual = fft.strongest();
                        assert_eq!(actual.position, peak.position);
                        assert!(
                            (actual.fractional_offset_samples - peak.fractional_offset_samples)
                                .abs()
                                < 1e-9
                        );
                    }
                }
            }
        }
    }
}

fn assert_timing_bits_equal(actual: PilotTiming, expected: PilotTiming) {
    assert_eq!(actual.position, expected.position);
    let bits = |value: PilotTiming| {
        [
            value.amplitude.re.to_bits(),
            value.amplitude.im.to_bits(),
            value.coherent_fraction.to_bits(),
            value.despread_coherent_fraction.to_bits(),
            value.fractional_offset_samples.to_bits(),
        ]
    };
    assert_eq!(bits(actual), bits(expected));
}

fn timing_work_buffers(search: &DedicatedTimingSearch) -> [(usize, usize); 5] {
    [
        (
            search.spectrum.as_ptr() as usize,
            search.spectrum.capacity(),
        ),
        (search.product.as_ptr() as usize, search.product.capacity()),
        (search.scratch.as_ptr() as usize, search.scratch.capacity()),
        (
            search.energies.as_ptr() as usize,
            search.energies.capacity(),
        ),
        (
            search.corrected.as_ptr() as usize,
            search.corrected.capacity(),
        ),
    ]
}

#[test]
fn prepared_slot_exchange_preserves_work_buffers_and_matches_standalone_searches() {
    for rate in [1, 2, 4, 8] {
        for number in 0..=4 {
            let format = UplinkControlFormat::new(number).unwrap();
            for delays in [1, 3, 4, 17, 18, 37] {
                let base =
                    Arc::new(DedicatedTimingPlan::new(&code(), format, 0, rate, delays).unwrap());
                let mut search = DedicatedTimingSearch::new(base.clone());
                let buffers = timing_work_buffers(&search);
                for slot in 0..15 {
                    let target = Arc::new(
                        DedicatedTimingPlan::new(&code(), format, slot, rate, delays).unwrap(),
                    );
                    let mut standalone = DedicatedTimingSearch::new(target.clone());
                    let mut prepared = target.clone();
                    let bytes = search.storage_bytes() + prepared.storage_bytes();
                    let counts = (Arc::strong_count(&base), Arc::strong_count(&target));
                    search.exchange_plan(&mut prepared).unwrap();
                    assert!(Arc::ptr_eq(&search.plan, &target));
                    assert!(Arc::ptr_eq(&prepared, &base));
                    assert_eq!(search.slot(), slot);
                    assert_eq!(
                        counts,
                        (Arc::strong_count(&base), Arc::strong_count(&target))
                    );
                    assert_eq!(buffers, timing_work_buffers(&search));
                    assert_eq!(bytes, search.storage_bytes() + prepared.storage_bytes());
                    let input: Vec<_> = (0..target.input_length())
                        .map(|i| {
                            Complex32::new(
                                ((i * 19) % 29) as f32 - 14.0,
                                ((i * 7) % 31) as f32 - 15.0,
                            )
                        })
                        .collect();
                    let seed = CarrierOffsetHz::new(317.0).unwrap();
                    let actual = search.search(at(137), &input, seed).unwrap();
                    let expected = standalone.search(at(137), &input, seed).unwrap();
                    for delay in 0..delays {
                        assert_timing_bits_equal(
                            actual.at(delay).unwrap(),
                            expected.at(delay).unwrap(),
                        );
                    }
                    assert_timing_bits_equal(actual.strongest(), expected.strongest());
                    for distance in 0..3 {
                        let chips: Vec<_> = reference(format, (slot + distance) % 15)
                            .iter()
                            .map(|value| Complex32::new(value.re as f32, value.im as f32))
                            .collect();
                        let zero = CarrierOffsetHz::new(0.0).unwrap();
                        let actual = search
                            .pilot_coherence(at(137), &chips, zero, usize::from(distance))
                            .unwrap();
                        assert!(actual > 0.99999999);
                        assert_eq!(
                            actual.to_bits(),
                            standalone
                                .pilot_coherence(at(137), &chips, zero, usize::from(distance))
                                .unwrap()
                                .to_bits()
                        );
                    }
                    search.exchange_plan(&mut prepared).unwrap();
                    assert!(Arc::ptr_eq(&search.plan, &base));
                    assert!(Arc::ptr_eq(&prepared, &target));
                    assert_eq!(buffers, timing_work_buffers(&search));
                }
            }
        }
    }
}

#[test]
fn incompatible_slot_plans_are_rejected_without_changing_active_state() {
    let format = UplinkControlFormat::new(0).unwrap();
    let base = Arc::new(DedicatedTimingPlan::new(&code(), format, 0, 2, 17).unwrap());
    let other = UplinkDpchCode::new(UplinkScramblingCode::new(0x123457).unwrap()).unwrap();
    let candidates = [
        DedicatedTimingPlan::new(&other, format, 12, 2, 17).unwrap(),
        DedicatedTimingPlan::new(&code(), UplinkControlFormat::new(1).unwrap(), 12, 2, 17).unwrap(),
        DedicatedTimingPlan::new(&code(), format, 12, 4, 17).unwrap(),
        DedicatedTimingPlan::new(&code(), format, 12, 2, 18).unwrap(),
        DedicatedTimingPlan::new_inner(&code(), format, 12, 2, 17, true).unwrap(),
    ];
    let mut search = DedicatedTimingSearch::new(base.clone());
    let buffers = timing_work_buffers(&search);
    let input = vec![Complex32::new(0.3, -0.7); base.input_length()];
    let zero = CarrierOffsetHz::new(0.0).unwrap();
    let before = search.search(at(137), &input, zero).unwrap().strongest();
    for candidate in candidates {
        let target = Arc::new(candidate);
        let mut prepared = target.clone();
        assert!(search.exchange_plan(&mut prepared).is_err());
        assert!(Arc::ptr_eq(&search.plan, &base));
        assert!(Arc::ptr_eq(&prepared, &target));
        assert_eq!(buffers, timing_work_buffers(&search));
        assert_timing_bits_equal(
            search.search(at(137), &input, zero).unwrap().strongest(),
            before,
        );
    }
}

#[test]
fn insufficient_work_buffers_reject_slot_exchange_without_consuming_plans() {
    for delays in [17, 37] {
        for buffer in 0..5 {
            let format = UplinkControlFormat::new(1).unwrap();
            let base = Arc::new(DedicatedTimingPlan::new(&code(), format, 0, 2, delays).unwrap());
            let mut search = DedicatedTimingSearch::new(base.clone());
            let target =
                Arc::new(DedicatedTimingPlan::new(&code(), format, 12, 2, delays).unwrap());
            let mut prepared = target.clone();
            match buffer {
                0 if delays == 17 => continue,
                0 => search.spectrum.truncate(search.spectrum.len() - 1),
                1 => search.product.truncate(search.product.len() - 1),
                2 if search.scratch.is_empty() => continue,
                2 => search.scratch.truncate(search.scratch.len() - 1),
                3 => search.energies.truncate(search.energies.len() - 1),
                _ => search.corrected.truncate(search.corrected.len() - 1),
            }
            assert!(search.exchange_plan(&mut prepared).is_err());
            assert!(Arc::ptr_eq(&search.plan, &base));
            assert!(Arc::ptr_eq(&prepared, &target));
        }
    }
}

#[test]
fn oversized_fft_or_corrected_windows_are_not_accepted_as_equal_geometry() {
    for buffer in 0..3 {
        let format = UplinkControlFormat::new(1).unwrap();
        let base = Arc::new(DedicatedTimingPlan::new(&code(), format, 0, 2, 37).unwrap());
        let mut search = DedicatedTimingSearch::new(base.clone());
        let target = Arc::new(DedicatedTimingPlan::new(&code(), format, 12, 2, 37).unwrap());
        let mut prepared = target.clone();
        match buffer {
            0 => search.spectrum.push(Complex64::default()),
            1 => search.product.push(Complex64::default()),
            _ => search.corrected.push(Complex64::default()),
        }
        assert!(search.exchange_plan(&mut prepared).is_err());
        assert!(Arc::ptr_eq(&search.plan, &base));
        assert!(Arc::ptr_eq(&prepared, &target));
    }
}

#[test]
fn direct_timing_plans_omit_fft_resources_at_policy_boundaries() {
    for rate in [1, 2, 4, 8] {
        let limit = if rate == 1 { 3 } else { 17 };
        for number in 0..=4 {
            for delays in [1, limit, limit + 1] {
                let format = UplinkControlFormat::new(number).unwrap();
                let plan =
                    Arc::new(DedicatedTimingPlan::new(&code(), format, 12, rate, delays).unwrap());
                let fft_baseline = Arc::new(
                    DedicatedTimingPlan::new_inner(&code(), format, 12, rate, delays, true)
                        .unwrap(),
                );
                let direct = delays <= limit;
                assert_eq!(plan.fft.is_none(), direct);
                assert_eq!(plan.input_length(), fft_baseline.input_length());
                assert_eq!(plan.energy.to_bits(), fft_baseline.energy.to_bits());
                assert_eq!(plan.pilot_reference, fft_baseline.pilot_reference);
                let search = DedicatedTimingSearch::new(plan.clone());
                let baseline_search = DedicatedTimingSearch::new(fft_baseline);
                if direct {
                    assert!(search.scratch.is_empty());
                    assert!(search.spectrum.is_empty());
                    assert_eq!(search.product.len(), delays);
                    assert!(search.storage_bytes() < baseline_search.storage_bytes());
                } else {
                    assert_eq!(search.storage_bytes(), baseline_search.storage_bytes());
                }
            }
        }
    }
}
