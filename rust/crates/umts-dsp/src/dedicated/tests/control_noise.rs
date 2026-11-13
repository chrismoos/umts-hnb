// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn own_channel_codes_do_not_count_as_symbol_disturbance() {
    for sf in [4, 8, 16, 32, 64, 128, 256] {
        for branch in [Complex64::new(1.0, 0.0), Complex64::new(0.0, 1.0)] {
            let mut code = vec![0.0; sf];
            OvsfCode::new(sf as u16, (sf / 4) as u16)
                .unwrap()
                .write(&mut code)
                .unwrap();
            let samples: Vec<_> = (0..CONTROL_SF)
                .map(|i| {
                    Complex64::new(2.0, 3.0)
                        + branch * f64::from(code[i % sf]) * 100.0
                        + Complex64::new(if i < CONTROL_SF / 2 { 5.0 } else { -5.0 }, 0.0)
                })
                .collect();
            assert!(control_symbol_disturbance(&samples) < 1e-20);
        }
    }
}

#[test]
fn symbol_disturbance_tracks_local_noise_and_squared_gain() {
    for gain in [1e-12, 1.0, 1e12] {
        let samples: Vec<_> = (0..CONTROL_SF)
            .map(|i| {
                let noise = if (i / CONTROL_NOISE_GROUP_CHIPS).is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                Complex64::new(2.0 + noise, 3.0) * gain
            })
            .collect();
        let groups = CONTROL_SF / CONTROL_NOISE_GROUP_CHIPS;
        let expected = gain * gain / (groups - 2) as f64;
        let actual = control_symbol_disturbance(&samples);
        assert!((actual / expected - 1.0).abs() < 1e-12);
    }
}

#[test]
fn hs_feedback_and_edch_codes_do_not_bias_disturbance() {
    for (sf, number) in [
        (256, 1),
        (256, 32),
        (256, 33),
        (256, 64),
        (4, 1),
        (4, 2),
        (4, 3),
        (2, 1),
    ] {
        let mut code = vec![0.0; sf];
        OvsfCode::new(sf as u16, number)
            .unwrap()
            .write(&mut code)
            .unwrap();
        let samples: Vec<_> = (0..CONTROL_SF)
            .map(|i| {
                Complex64::new(3.0, 4.0) + Complex64::new(17.0, -11.0) * f64::from(code[i % sf])
            })
            .collect();
        assert!(
            control_symbol_disturbance(&samples) < 1e-20,
            "sf={sf}, code={number}"
        );
    }
}
