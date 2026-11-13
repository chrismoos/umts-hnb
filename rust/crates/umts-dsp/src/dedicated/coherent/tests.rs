// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn random(seed: &mut u32) -> f64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    (f64::from(*seed) + 0.5) / (f64::from(u32::MAX) + 1.0)
}
fn gaussian(seed: &mut u32) -> Complex64 {
    Complex64::from_polar(
        (-2.0 * random(seed).ln()).sqrt(),
        std::f64::consts::TAU * random(seed),
    )
}
#[test]
fn beta_quantization_and_likelihood_sf_noise_and_channel_uncertainty_are_explicit() {
    for control in 1..=15 {
        for data in 0..=15 {
            assert_eq!(
                UplinkGains::new(control, data).is_ok(),
                control.max(data) == 15
            );
        }
    }
    assert!(UplinkGains::new(0, 15).is_err());
    assert!(UplinkGains::new(15, 16).is_err());
    let statistics = DpcchStatistics::new(
        PilotEstimate {
            channel: Complex64::new(1.0, 0.0),
            observed_power: 1.1,
            residual_power: 0.1,
        },
        6,
    )
    .unwrap();
    assert!((statistics.disturbance_symbol_power - 0.12).abs() < 1e-14);
    assert!((statistics.signal_code_power - 0.98).abs() < 1e-14);
    assert!((statistics.disturbance_chip_power - 30.72).abs() < 1e-14);
    assert!((statistics.sir_linear().unwrap() - 0.98 / 0.12).abs() < 1e-12);
    let gain = UplinkGains::new(8, 15).unwrap().ratio();
    let settings = LikelihoodSettings::new(128.0).unwrap();
    let expected = 4.0 * gain * gain / (0.12 * 256.0 / 64.0 + gain * gain * 0.12 / 6.0);
    assert!((statistics.llr(gain, gain, 64, settings) - expected).abs() < 1e-12);
    assert_eq!(statistics.llr(gain, 0.0, 64, settings), 0.0);
    let noiseless = DpcchStatistics::new(
        PilotEstimate {
            channel: Complex64::new(1.0, 0.0),
            observed_power: 1.0,
            residual_power: 0.0,
        },
        6,
    )
    .unwrap();
    assert_eq!(noiseless.llr(-1.0, 1.0, 4, settings), -128.0);
    assert!(noiseless.sir_linear().is_none());
    assert!(
        DpcchStatistics::new(
            PilotEstimate {
                observed_power: 1.0,
                residual_power: f64::MAX,
                ..PilotEstimate::default()
            },
            6
        )
        .is_err()
    );
    let low = DpcchStatistics::new(
        PilotEstimate {
            channel: Complex64::default(),
            observed_power: 0.1,
            residual_power: 0.1,
        },
        2,
    )
    .unwrap();
    assert!(low.signal_code_power < 0.0);
    assert!(low.sir_linear().is_none());
}

#[test]
fn pilot_signal_and_noise_estimators_match_ensemble_power_without_adc_gain_thresholds() {
    let mut reference: Option<(f64, f64)> = None;
    for gain in [1e-12, 1.0, 1e12] {
        let mut seed = 0x1234;
        let mut signal = 0.0;
        let mut noise = 0.0;
        for _ in 0..20000 {
            let pilots: [Complex64; 6] = std::array::from_fn(|_| {
                (Complex64::new(1.0, 0.3) + gaussian(&mut seed) * 0.4) * gain
            });
            let channel = pilots.iter().sum::<Complex64>() / pilots.len() as f64;
            let estimate = PilotEstimate {
                channel,
                observed_power: pilots.iter().map(|pilot| pilot.norm_sqr()).sum::<f64>() / 6.0,
                residual_power: pilots
                    .iter()
                    .map(|pilot| (*pilot - channel).norm_sqr())
                    .sum::<f64>()
                    / 6.0,
            };
            let statistics = DpcchStatistics::new(estimate, 6).unwrap();
            signal += statistics.signal_code_power / (gain * gain);
            noise += statistics.disturbance_symbol_power / (gain * gain);
        }
        signal /= 20000.0;
        noise /= 20000.0;
        assert!((signal - 1.09).abs() < 0.02);
        assert!((noise - 0.32).abs() < 0.01);
        if let Some((old_signal, old_noise)) = reference {
            assert!((signal - old_signal).abs() < 1e-10);
            assert!((noise - old_noise).abs() < 1e-10);
        }
        reference = Some((signal, noise));
    }
}

#[test]
fn double_carrier_path_is_phase_continuous_across_frequency_changes() {
    let origin = SampleInstant {
        epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
        index: 10000,
    };
    let rate = SampleRate::new(CHIPS_PER_SECOND, 1).unwrap();
    let mut carrier =
        CarrierCorrector::new(rate, CarrierOffsetHz::new(1234.56).unwrap(), 0.3, origin).unwrap();
    let mut phase = 0.3;
    let mut offset = 0;
    for frequency in [1234.56, -777.0, 0.0] {
        carrier
            .set_offset(CarrierOffsetHz::new(frequency).unwrap())
            .unwrap();
        let mut samples = vec![Complex64::new(0.7, -0.2); 2560];
        let start = SampleInstant {
            index: origin.index + offset,
            ..origin
        };
        carrier.process64(start, &mut samples).unwrap();
        for (index, value) in samples.iter().enumerate() {
            let expected = Complex64::new(0.7, -0.2)
                * Complex64::from_polar(
                    1.0,
                    phase
                        - std::f64::consts::TAU * frequency * index as f64
                            / CHIPS_PER_SECOND as f64,
                );
            assert!((*value - expected).norm() < 1e-11);
        }
        phase -= std::f64::consts::TAU * frequency * samples.len() as f64 / CHIPS_PER_SECOND as f64;
        offset += samples.len() as u64;
    }
    let before = carrier.next_input();
    assert!(
        carrier
            .process64(before, &mut [Complex64::new(f64::NAN, 0.0)])
            .is_err()
    );
    assert_eq!(carrier.next_input(), before);
}

#[test]
fn skipped_frame_preserves_learned_frequency_but_new_epoch_resets_it() {
    let settings = CarrierTrackingSettings {
        initial: CarrierOffsetHz::new(0.0).unwrap(),
        maximum_absolute_hz: 2000.0,
        update_fraction: 0.5,
        minimum_coherence: 0.9,
    };
    let mut tracking = PilotCarrier::new(settings).unwrap();
    let start = ChipInstant {
        epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
        index: 1024,
    };
    tracking.begin(start).unwrap();
    let pilots: Vec<_> = (0..6)
        .map(|index| {
            Complex64::from_polar(
                1.0,
                std::f64::consts::TAU * 1200.0 * index as f64 * 256.0 / CHIPS_PER_SECOND as f64,
            )
        })
        .collect();
    let learned = tracking.update(&pilots).unwrap().next_hz;
    assert!((learned - 600.0).abs() < 1e-9);
    let after_gap = ChipInstant {
        index: start.index + 2 * umts_types::time::CHIPS_PER_FRAME,
        ..start
    };
    tracking.begin(after_gap).unwrap();
    let mut samples: Vec<_> = (0..2560)
        .map(|index| {
            Complex64::from_polar(
                1.0,
                std::f64::consts::TAU * learned * index as f64 / CHIPS_PER_SECOND as f64,
            )
        })
        .collect();
    tracking.correct(after_gap, &mut samples).unwrap();
    assert!(
        samples
            .iter()
            .all(|sample| (*sample - Complex64::new(1.0, 0.0)).norm() < 1e-10)
    );
    assert!(
        (tracking
            .update(&[Complex64::default(); 6])
            .unwrap()
            .applied_hz
            - learned)
            .abs()
            < 1e-9
    );
    tracking
        .begin(ChipInstant {
            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap()),
            ..after_gap
        })
        .unwrap();
    assert_eq!(
        tracking
            .update(&[Complex64::default(); 6])
            .unwrap()
            .applied_hz,
        0.0
    );
}

#[test]
fn confirmed_carrier_restore_survives_a_gap_and_rejects_out_of_range_updates() {
    let mut tracking = PilotCarrier::new(CarrierTrackingSettings {
        initial: CarrierOffsetHz::new(0.0).unwrap(),
        maximum_absolute_hz: 2000.0,
        update_fraction: 0.5,
        minimum_coherence: 0.8,
    })
    .unwrap();
    let mut start = ChipInstant {
        epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
        index: 1024,
    };
    tracking.begin(start).unwrap();
    tracking
        .restore_confirmed_offset(CarrierOffsetHz::new(123.0).unwrap())
        .unwrap();
    assert!(
        tracking
            .restore_confirmed_offset(CarrierOffsetHz::new(2001.0).unwrap())
            .is_err()
    );
    start.index += 2 * umts_types::time::CHIPS_PER_FRAME;
    tracking.begin(start).unwrap();
    let result = tracking.update(&[Complex64::default(); 6]).unwrap();
    assert_eq!(result.applied_hz, 123.0);
    assert_eq!(result.next_hz, 123.0);
}

#[test]
fn pilot_frequency_update_is_next_slot_only_and_gain_invariant() {
    assert!(
        PilotCarrier::new(CarrierTrackingSettings {
            initial: CarrierOffsetHz::new(0.0).unwrap(),
            maximum_absolute_hz: 4000.0,
            update_fraction: 0.5,
            minimum_coherence: 0.9
        })
        .is_err()
    );
    for gain in [1e-12, 1.0, 1e12] {
        let settings = CarrierTrackingSettings {
            initial: CarrierOffsetHz::new(0.0).unwrap(),
            maximum_absolute_hz: 2000.0,
            update_fraction: 0.5,
            minimum_coherence: 0.9,
        };
        let mut tracking = PilotCarrier::new(settings).unwrap();
        let start = ChipInstant {
            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
            index: 1024,
        };
        tracking.begin(start).unwrap();
        let pilots: Vec<_> = (0..6)
            .map(|index| {
                Complex64::from_polar(
                    gain,
                    std::f64::consts::TAU * 1200.0 * index as f64 * 256.0 / CHIPS_PER_SECOND as f64,
                )
            })
            .collect();
        let update = tracking.update(&pilots).unwrap();
        assert_eq!(update.applied_hz, 0.0);
        assert!((update.next_hz - 600.0).abs() < 1e-9);
        assert!(update.updated);
        let missing = tracking.update(&[Complex64::default(); 6]).unwrap();
        assert!(!missing.updated);
        assert!((missing.applied_hz - 600.0).abs() < 1e-9);
    }
}
