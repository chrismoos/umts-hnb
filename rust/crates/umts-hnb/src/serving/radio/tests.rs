// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn config() -> umts_config::Config {
    serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/serving.yaml"
    )))
    .unwrap()
}

#[test]
fn secondary_carrier_moves_only_tx_to_the_midpoint() {
    let mut config = config();
    config.radio.uarfcn_dl = 10637;
    config.radio.uarfcn_ul = 9687;
    config.radio.tx_samples_per_chip = 4;
    config.radio.tx_analog_bandwidth_hz = 10_000_000;
    config.cell.secondary_carrier.enabled = true;
    config.cell.secondary_carrier.uarfcn_dl = Some(10662);
    let plan = radio_plan(&config).unwrap();
    assert_eq!(plan.downlink_hz, 2_129_900_000);
    assert_eq!(plan.uplink_hz, 1_937_400_000);
    assert_eq!(plan.hal.tx_sample_rate_hz, 15_360_000);
    assert_eq!(plan.hal.rx_sample_rate_hz, Some(7_680_000));
    assert_eq!(plan.hal.rx_bandwidth_hz, Some(5_000_000));
    config.cell.secondary_carrier.uarfcn_dl = Some(10612);
    assert_eq!(radio_plan(&config).unwrap().downlink_hz, 2_124_900_000);
    config.radio.tx_samples_per_chip = 2;
    assert!(radio_plan(&config).is_err());
    config.radio.tx_samples_per_chip = 4;
    config.radio.tx_analog_bandwidth_hz = 5_000_000;
    assert!(radio_plan(&config).is_err());
    config.cell.secondary_carrier.enabled = false;
    assert_eq!(radio_plan(&config).unwrap().downlink_hz, 2_127_400_000);
}

#[test]
fn uhd_preserves_float_gains_args_and_explicit_stream_geometry() {
    let mut config = config();
    config.radio.uhd_args = "type=b200,serial=example,master_clock_rate=30.72e6".into();
    config.radio.tx_gain_db = 65.125;
    config.radio.rx_gain_db = 23.75;
    let plan = radio_plan(&config).unwrap();
    assert_eq!(
        (plan.downlink_hz, plan.uplink_hz),
        (880_000_000, 835_000_000)
    );
    assert_eq!(plan.rx_phase, 0);
    assert_eq!(plan.hal.tx_sample_rate_hz, SAMPLE_RATE_HZ);
    assert_eq!(plan.hal.rx_sample_rate_hz, Some(SAMPLE_RATE_HZ));
    assert_eq!(plan.hal.tx_bandwidth_hz, Some(ANALOG_BANDWIDTH_HZ));
    assert_eq!(plan.hal.rx_bandwidth_hz, Some(ANALOG_BANDWIDTH_HZ));
    assert!(matches!(
        plan.hal.tx_rate_policy,
        RatePolicy::NearestWholeHz
    ));
    assert!(matches!(
        plan.hal.rx_rate_policy,
        RatePolicy::NearestWholeHz
    ));
    match plan.hal.device {
        DeviceConfig::Uhd {
            device,
            channel,
            antenna,
            tx_gain_db,
            master_clock_rate,
            software_tx_frequency_correction,
            rx_antenna,
            rx_gain_db,
            rx_reference_dbm,
            rx_to_tx_delay_ns,
            rx_max_gain_reference_dbm,
            tx_max_gain_power_estimate_dbm,
            ..
        } => {
            assert_eq!(device, config.radio.uhd_args);
            assert_eq!(channel, 0);
            assert_eq!(antenna, "TX/RX");
            assert_eq!(rx_antenna.as_deref(), Some("RX2"));
            assert_eq!(master_clock_rate, Some(30_720_000));
            assert!(software_tx_frequency_correction);
            assert_eq!(tx_gain_db, 65.125);
            assert_eq!(rx_gain_db, Some(23.75));
            assert_eq!(rx_reference_dbm, None);
            assert_eq!(rx_to_tx_delay_ns, None);
            assert_eq!(rx_max_gain_reference_dbm, None);
            assert_eq!(tx_max_gain_power_estimate_dbm, None);
        }
        _ => panic!("expected UHD plan"),
    }
    config.radio.tx_antenna = Some("TX/RX".into());
    config.radio.rx_antenna = Some("TX/RX".into());
    config.radio.tx_analog_bandwidth_hz = 15_360_000;
    config.radio.tx_lo_offset_hz = 2_500_000;
    match radio_plan(&config).unwrap().hal.device {
        DeviceConfig::Uhd {
            antenna,
            rx_antenna,
            software_tx_frequency_correction,
            ..
        } => {
            assert_eq!(
                (antenna.as_str(), rx_antenna.as_deref()),
                ("TX/RX", Some("TX/RX"))
            );
            assert!(!software_tx_frequency_correction);
        }
        _ => panic!("expected UHD plan"),
    }
}

#[test]
fn compensated_offset_requires_passband_and_device_clock_margin_without_changing_carriers() {
    let mut config = config();
    config.radio.tx_analog_bandwidth_hz = 15_360_000;
    for offset in [-2_500_000, 0, 2_500_000] {
        config.radio.tx_lo_offset_hz = offset;
        let plan = radio_plan(&config).unwrap();
        assert_eq!(
            (plan.downlink_hz, plan.uplink_hz),
            (880_000_000, 835_000_000)
        );
        assert_eq!(plan.hal.tx_bandwidth_hz, Some(15_360_000));
        assert_eq!(plan.hal.rx_bandwidth_hz, Some(ANALOG_BANDWIDTH_HZ));
        assert_eq!(plan.hal.tx_sample_rate_hz, SAMPLE_RATE_HZ);
    }
    config.radio.tx_analog_bandwidth_hz = 5_000_000;
    config.radio.tx_lo_offset_hz = 2_500_000;
    assert!(radio_plan(&config).is_err());
    config.radio.tx_analog_bandwidth_hz = 15_360_000;
    config.radio.tx_lo_offset_hz = i64::MIN;
    assert!(radio_plan(&config).is_err());
    config.radio.tx_lo_offset_hz = 0;
    config.radio.tx_analog_bandwidth_hz = 56_000_000;
    assert!(radio_plan(&config).is_err());
}

#[test]
fn tx_oversampling_preserves_rx_geometry_and_analog_bandwidth() {
    let mut config = config();
    config.radio.tx_samples_per_chip = 4;
    let plan = radio_plan(&config).unwrap();
    assert_eq!(plan.tx_samples_per_chip, SamplesPerChip::Four);
    assert_eq!(plan.hal.tx_sample_rate_hz, 15_360_000);
    assert_eq!(plan.hal.rx_sample_rate_hz, Some(7_680_000));
    assert_eq!(plan.hal.tx_bandwidth_hz, Some(5_000_000));
    assert_eq!(plan.hal.rx_bandwidth_hz, Some(5_000_000));
    for invalid in [0, 1, 3, 5, 8] {
        config.radio.tx_samples_per_chip = invalid;
        assert!(radio_plan(&config).is_err());
        assert!(config.validate().is_err());
    }
}

#[test]
fn uhd_clock_and_native_offsets_require_unambiguous_configuration() {
    let mut config = config();
    config.radio.uhd_args = "type=b200".into();
    match radio_plan(&config).unwrap().hal.device {
        DeviceConfig::Uhd {
            master_clock_rate, ..
        } => assert_eq!(master_clock_rate, None),
        _ => panic!("expected UHD plan"),
    }
    for args in [
        "master_clock_rate=",
        "master_clock_rate=NaN",
        "master_clock_rate=0",
        "master_clock_rate=30720000.5",
        "master_clock_rate=30720000,master_clock_rate=30720000",
    ] {
        config.radio.uhd_args = args.into();
        assert!(radio_plan(&config).is_err(), "{args}");
    }
    config.radio.uhd_args = "master_clock_rate=30720000".into();
    config.radio.uhd_rx_timestamp_offset = Some(99);
    assert!(radio_plan(&config).is_err());
    config.radio.uhd_rx_timestamp_offset = Some(0);
    assert!(radio_plan(&config).is_ok());
}

#[test]
fn bladerf_preserves_integer_gains_phase_and_stream_settings_without_calibration() {
    let mut config = config();
    config.radio.backend = "bladerf".into();
    config.radio.bladerf_args = "*:serial=example".into();
    let plan = radio_plan(&config).unwrap();
    assert_eq!(
        plan.rx_phase,
        usize::from(config.radio.bladerf_rx_sample_phase)
    );
    match plan.hal.device {
        DeviceConfig::BladeRf {
            device,
            tx_gain_db,
            rx_gain_db,
            num_buffers,
            buffer_size,
            num_transfers,
            rx_reference_dbm,
            rx_to_tx_delay_ns,
            ..
        } => {
            assert_eq!(device, config.radio.bladerf_args);
            assert_eq!(tx_gain_db, 75);
            assert_eq!(rx_gain_db, Some(30));
            assert_eq!(num_buffers, Some(config.radio.bladerf_stream_buffers));
            assert_eq!(
                buffer_size,
                Some(config.radio.bladerf_stream_buffer_samples)
            );
            assert_eq!(num_transfers, Some(config.radio.bladerf_stream_transfers));
            assert_eq!(rx_reference_dbm, None);
            assert_eq!(rx_to_tx_delay_ns, None);
        }
        _ => panic!("expected bladeRF plan"),
    }
    config.radio.tx_antenna = Some("TX1".into());
    config.radio.rx_antenna = Some("RX1".into());
    match radio_plan(&config).unwrap().hal.device {
        DeviceConfig::BladeRf {
            tx_antenna,
            rx_antenna,
            ..
        } => assert_eq!(
            (tx_antenna.as_deref(), rx_antenna.as_deref()),
            (Some("TX1"), Some("RX1"))
        ),
        _ => panic!("expected bladeRF plan"),
    }
    config.radio.tx_gain_db = 65.5;
    assert!(radio_plan(&config).is_err());
    config.radio.tx_gain_db = 65.0;
    config.radio.bladerf_rx_sample_phase = 2;
    assert!(radio_plan(&config).is_err());
    config.radio.bladerf_rx_sample_phase = 0;
    config.radio.bladerf_rx_timestamp_offset = Some(1);
    assert!(radio_plan(&config).is_err());
}

#[test]
fn unsupported_backends_and_nonfinite_gains_never_produce_a_radio_plan() {
    let mut config = config();
    for backend in ["soapy", "lime", "network", "noop", "unknown"] {
        config.radio.backend = backend.into();
        assert!(radio_plan(&config).is_err());
    }
    config.radio.backend = "uhd".into();
    config.radio.rx_gain_db = f64::NAN;
    assert!(radio_plan(&config).is_err());
}
