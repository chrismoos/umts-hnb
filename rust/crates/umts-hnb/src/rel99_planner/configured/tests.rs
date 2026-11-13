// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn config() -> Config {
    serde_yaml::from_str(include_str!(
        "../../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap()
}

#[test]
fn canonical_reference_yaml_compiles_an_owned_policy_without_opening_radio() {
    let config = config();
    let compiled = ConfiguredPolicy::new(&config).unwrap();
    let policy = compiled.policy();
    assert_eq!(policy.physical.len(), 1);
    assert_eq!(policy.signalling.interval, TransportInterval::Ms40);
    assert_eq!(policy.signalling.um_uplink_bits, 144);
    assert_eq!(policy.signalling.um_downlink_bits, 336);
    assert_eq!(policy.maximum_combinations.get(), 1024);
    assert_eq!(policy.maximum_total_coded_bits.get(), 500_000);
    assert_eq!(policy.maximum_user_storage_bytes.get(), 16_000_000);
    assert_eq!(policy.transport.maximum_coded_bits.get(), 500_000);
    assert_eq!(policy.transport.maximum_mapping_entries.get(), 1_000_000);
    assert_eq!(policy.uplink_gains.control, 15);
    assert_eq!(policy.uplink_gains.data, 15);
    assert_eq!(
        policy.physical[0].primary_scrambling,
        config.cell.physical.downlink_scrambling_code
    );
    assert_eq!(
        policy.physical[0].uplink_scrambling,
        config.rel99.physical.uplink_scrambling_code_base
    );
    assert_eq!(
        policy.physical[0].maximum_uplink_dbm,
        config.rrc.max_allowed_ul_tx_power_dbm
    );
}

#[test]
fn policy_retains_its_snapshot_after_source_config_changes() {
    let mut config = config();
    let compiled = ConfiguredPolicy::new(&config).unwrap();
    config.rel99.physical.planner.uplink_data_gain = 3;
    config.rel99.physical.planner.physical_candidates.clear();
    config
        .rel99
        .physical
        .planner
        .budgets
        .maximum_user_storage_bytes = 1;
    let policy = compiled.policy();
    assert_eq!(policy.uplink_gains.data, 15);
    assert_eq!(policy.physical.len(), 1);
    assert_eq!(policy.maximum_user_storage_bytes.get(), 16_000_000);
}

#[test]
fn candidate_order_and_existing_cell_settings_are_preserved() {
    let mut config = config();
    config.cell.physical.downlink_scrambling_code = 37;
    config.rel99.physical.uplink_scrambling_code_base = 1234;
    config.rel99.physical.initial_ul_puncturing_limit_percent = 88;
    config.rrc.max_allowed_ul_tx_power_dbm = 17;
    let first = &mut config.rel99.physical.planner.physical_candidates[0];
    first.downlink_sf = 64;
    first.downlink_pilot_bits = 8;
    first.downlink_code_count = 2;
    first.tpc_algorithm = R99TpcAlgorithm::Algorithm2;
    let mut second = first.clone();
    second.downlink_sf = 16;
    second.downlink_pilot_bits = 16;
    second.tpc_algorithm = R99TpcAlgorithm::Algorithm1;
    second.tpc_step_db = 2;
    config
        .rel99
        .physical
        .planner
        .physical_candidates
        .push(second);
    let compiled = ConfiguredPolicy::new(&config).unwrap();
    let physical = compiled.policy().physical;
    assert_eq!(
        physical.iter().map(|p| p.downlink_sf).collect::<Vec<_>>(),
        [64, 16]
    );
    assert_eq!(physical[0].power_control, UplinkPowerControl::Algorithm2);
    assert_eq!(
        physical[1].power_control,
        UplinkPowerControl::Algorithm1 { step_db: 2 }
    );
    for p in physical {
        assert_eq!(p.primary_scrambling, 37);
        assert_eq!(p.uplink_scrambling, 1234);
        assert_eq!(p.puncturing_percent, 88);
        assert_eq!(p.maximum_uplink_dbm, 17);
        assert_eq!(p.downlink_codes, [0, 1]);
    }
}

#[test]
fn all_normal_dpch_offsets_preserve_the_specified_modulo_relation() {
    let mut config = config();
    for doff in 0..=599 {
        config.rel99.physical.planner.physical_candidates[0].default_offset_512_chips = doff;
        let compiled = ConfiguredPolicy::new(&config).unwrap();
        let physical = &compiled.policy().physical[0];
        assert_eq!(
            u32::from(physical.frame_offset_256_chips) * 256,
            u32::from(doff) * 512 % 38_400
        );
        physical.validate().unwrap();
    }
}

#[test]
fn every_supported_coding_crc_and_tti_compiles_without_narrowing() {
    let mut config = config();
    for kind in [
        R99CodingKind::ConvolutionalHalf,
        R99CodingKind::ConvolutionalThird,
        R99CodingKind::Turbo,
    ] {
        for crc in [0, 8, 12, 16, 24] {
            for tti in [10, 20, 40, 80] {
                let signalling = &mut config.rel99.physical.planner.signalling;
                signalling.uplink.kind = kind;
                signalling.downlink.kind = kind;
                signalling.uplink.crc_bits = crc;
                signalling.downlink.crc_bits = crc;
                signalling.interval_ms = tti;
                let compiled = ConfiguredPolicy::new(&config).unwrap();
                assert_eq!(
                    compiled.policy().signalling.interval,
                    interval(tti).unwrap()
                );
                let actual = match compiled.policy().signalling.uplink.crc {
                    CrcSize::None => 0,
                    CrcSize::Bits8 => 8,
                    CrcSize::Bits12 => 12,
                    CrcSize::Bits16 => 16,
                    CrcSize::Bits24 => 24,
                };
                assert_eq!(actual, crc);
            }
        }
    }
}

#[test]
fn invalid_protocol_values_and_zero_budgets_fail_policy_compilation() {
    for change in 0..10 {
        let mut config = config();
        let planner = &mut config.rel99.physical.planner;
        match change {
            0 => planner.signalling.uplink.crc_bits = 10,
            1 => planner.signalling.interval_ms = 30,
            2 => planner.signalling.um_uplink_bits = 17,
            3 => planner.signalling.um_downlink_bits = 16,
            4 => planner.physical_candidates[0].downlink_pilot_bits = 2,
            5 => planner.physical_candidates[0].dpcch_offset_db = -7,
            6 => planner.budgets.maximum_total_coded_bits = 0,
            7 => planner.budgets.maximum_user_storage_bytes = 0,
            8 => planner.budgets.maximum_mapping_entries = 0,
            _ => planner.physical_candidates.clear(),
        }
        assert!(
            ConfiguredPolicy::new(&config).is_err(),
            "accepted invalid case {change}"
        );
    }
}

#[test]
fn decoder_budget_is_checked_before_any_plan_is_constructed() {
    let mut config = config();
    for iterations in [0, 256, u32::MAX] {
        config.receiver.turbo_iterations = iterations;
        assert!(ConfiguredPolicy::new(&config).is_err());
    }
}
