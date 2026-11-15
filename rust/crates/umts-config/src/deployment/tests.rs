// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::NetworkOperationMode;

const DEFAULTS: &str = include_str!("../../../../../config/defaults.yaml");
const OPERATOR: &str =
    include_str!("../../../../../rust/test-support/fixtures/config/operator.yaml");
const BASELINE: &str =
    include_str!("../../../../../rust/test-support/fixtures/config/split-baseline.yaml");

#[test]
fn soapy_operator_does_not_require_native_uhd_arguments() {
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    operator["radio"]
        .as_mapping_mut()
        .unwrap()
        .remove("uhd_args");
    let missing_uhd = serde_yaml::to_string(&operator).unwrap();
    assert!(
        Deployment::parse(DEFAULTS, &missing_uhd)
            .is_err_and(|error| error.to_string().contains("radio.uhd_args"))
    );
    operator["radio"]["backend"] = Value::from("soapy");
    operator["radio"]["soapy_args"] = Value::from("driver=uhd");
    let loaded = Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
    assert_eq!(loaded.config.radio.soapy_args, "driver=uhd");
    assert_eq!(loaded.config.radio.uhd_args, "");
    assert_eq!(loaded.config.radio.tx_lo_offset_hz, 0);
}

fn unified_reference() -> Config {
    let mut reference: Config = serde_yaml::from_str(BASELINE).unwrap();
    reference.rel99.profile = Some(crate::Rel99Profile::Standard);
    reference.power_control.uplink.sir_ceiling_db = 13.0;
    reference.hspa.uplink.initial_serving_grant = Some(31);
    reference.hspa.uplink.serving_grant = 31;
    assert_eq!(
        reference.hspa.downlink.measurement_power_offset_db,
        Some(12.0)
    );
    reference.hspa.downlink.measurement_power_offset_db = None;
    let planner = &mut reference.rel99.physical.planner;
    assert_eq!(planner.services.packet[0].downlink.maximum_blocks, 0);
    assert_eq!(planner.physical_candidates.len(), 2);
    planner.services.packet[0].downlink.maximum_blocks = 3;
    planner.services.packet[0].downlink.rlc_bits = 2040;
    planner.services.packet[0]
        .am
        .as_mut()
        .unwrap()
        .network
        .status_prohibit_ms = Some(100);
    planner.services.packet[0]
        .am
        .as_mut()
        .unwrap()
        .buffers
        .queued_sdus = 16;
    let mut packet = planner.physical_candidates.last().unwrap().clone();
    packet.downlink_sf = 8;
    packet.downlink_pilot_bits = 16;
    planner.physical_candidates.push(packet);
    reference.runtime.tx_batch_slots = 1;
    reference.runtime.maximum_submission_us = 1425;
    reference.runtime.maximum_preparation_us = 1525;
    reference.runtime.controller_idle_wait_us = Some(100);
    reference.runtime.media.maximum_voice_bytes = 268_435_456;
    reference.hspa.downlink.qam64_enabled = true;
    reference.cell.packet_modes = Some(vec![
        crate::PacketMode::Rel99,
        crate::PacketMode::Hspa,
        crate::PacketMode::HspaPlus,
    ]);
    reference.radio.tx_analog_bandwidth_hz = 10_000_000;
    reference.radio.tx_lo_offset_hz = 2_500_000;
    let scheduling = reference
        .hspa
        .downlink
        .resources
        .scheduling
        .as_mut()
        .unwrap();
    scheduling.maximum_cell_bytes = 3_221_225_472;
    scheduling.maximum_coded_bits = 150_000;
    scheduling.maximum_plan_bytes = 300_000_000;
    scheduling.maximum_ue_bytes = 640_000_000;
    let allocation = reference
        .power_control
        .downlink
        .allocation
        .as_mut()
        .unwrap();
    allocation.broadcast_sync_percent = 2.0;
    allocation.carrier_rms = 0.293_906_9;
    let hsupa = reference.hspa.uplink.resources.as_mut().unwrap();
    hsupa.maximum_cell_receive_bytes = 4_294_967_296;
    hsupa.receive.packet_capacity = 257;
    reference
}

#[test]
fn split_configuration_matches_unified_packet_and_derived_cqi_defaults() {
    let original = unified_reference();
    original.validate().unwrap();
    let loaded = Deployment::parse(DEFAULTS, OPERATOR).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded.config).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert_eq!(loaded.effective, serde_yaml::to_value(&original).unwrap());
}

#[test]
fn operator_overrides_defaults_and_optional_null_is_preserved() {
    let operator = format!(
        "{OPERATOR}\nhspa:\n  downlink:\n    feedback:\n      cqi_interval_ms: 4\nhspa:\n  uplink:\n    scheduled: true\n"
    );
    assert!(Deployment::parse(DEFAULTS, &operator).is_err());
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    insert(
        &mut operator,
        "power_control.uplink.sir_ceiling_db",
        Value::from(11),
    )
    .unwrap();
    insert(
        &mut operator,
        "cell.measurements.ue_measurement_report_count",
        Value::Null,
    )
    .unwrap();
    insert(
        &mut operator,
        "hspa.downlink.feedback.cqi_interval_ms",
        Value::from(4),
    )
    .unwrap();
    let mut defaults: Value = serde_yaml::from_str(DEFAULTS).unwrap();
    defaults["cell"]["measurements"]["ue_measurement_report_count"] = Value::from(32);
    insert(&mut operator, "runtime.spin_wait", Value::Bool(false)).unwrap();
    insert(
        &mut operator,
        "rrc.inactivity_release_seconds",
        Value::from(0),
    )
    .unwrap();
    let loaded = Deployment::parse(
        &serde_yaml::to_string(&defaults).unwrap(),
        &serde_yaml::to_string(&operator).unwrap(),
    )
    .unwrap();
    assert_eq!(loaded.config.hspa.downlink.feedback.cqi_interval_ms, 4);
    assert_eq!(loaded.config.power_control.uplink.sir_ceiling_db, 11.0);
    assert_eq!(
        loaded.config.cell.measurements.ue_measurement_report_count,
        None
    );
    assert!(!loaded.config.runtime.spin_wait);
    assert_eq!(loaded.config.rrc.inactivity_release_seconds, 0);
}

#[test]
fn effective_serialization_is_independent_of_operator_key_order() {
    let value: Value = serde_yaml::from_str(OPERATOR).unwrap();
    let reordered = Value::Mapping(
        value
            .as_mapping()
            .unwrap()
            .iter()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    let original = Deployment::parse(DEFAULTS, OPERATOR).unwrap();
    let other = Deployment::parse(DEFAULTS, &serde_yaml::to_string(&reordered).unwrap()).unwrap();
    assert_eq!(
        serde_yaml::to_string(&original.effective).unwrap(),
        serde_yaml::to_string(&other.effective).unwrap()
    );
}

#[test]
fn lists_replace_instead_of_concatenating() {
    let mut defaults: Value = serde_yaml::from_str("a: [1, 2]\nb: {x: 3, y: 4}").unwrap();
    merge(
        &mut defaults,
        serde_yaml::from_str("a: []\nb: {x: 7}").unwrap(),
    );
    assert_eq!(
        defaults,
        serde_yaml::from_str::<Value>("a: []\nb: {x: 7, y: 4}").unwrap()
    );
}

#[test]
fn reject_unknown_duplicate_mistyped_and_removed_fields_before_merge() {
    for extra in [
        "typo: true",
        "radio: {hspa_enable: true}",
        "receiver: {turbo_iterations: wrong}",
        "runtime: null",
        "rrc: {inactivity_release_seconds: null}",
        "rrc: {typo: 1}",
        "runtime: {maximum_ues: 1, maximum_ues: 2}",
    ] {
        assert!(
            Deployment::parse(DEFAULTS, &format!("{OPERATOR}\n{extra}\n")).is_err(),
            "{extra}"
        );
    }
    assert!(Deployment::parse(&format!("{DEFAULTS}\nrrc: {{typo: 1}}\n"), OPERATOR).is_err());
}

#[test]
fn missing_operator_identity_and_missing_files_fail_closed() {
    assert!(Deployment::parse(DEFAULTS, "{}").is_err());
    assert!(
        Deployment::load(
            Path::new("/nonexistent/umts-operator.yaml"),
            Path::new("/nonexistent/umts-defaults.yaml")
        )
        .is_err()
    );
    let mut invalid: Value = serde_yaml::from_str(DEFAULTS).unwrap();
    insert(&mut invalid, "cell.plmn", Value::String("001-01".into())).unwrap();
    assert!(Deployment::parse(&serde_yaml::to_string(&invalid).unwrap(), OPERATOR).is_err());
}

#[test]
fn example_has_documented_b200_radio_and_path_settings() {
    let example = include_str!("../../../../../config/example.yaml");
    let mut loaded = Deployment::parse(DEFAULTS, example).unwrap().config;
    let mut reference = unified_reference();
    reference.radio.tx_gain_db = 78.0;
    reference.radio.tx_antenna = Some("TX/RX".into());
    reference.radio.rx_antenna = Some("RX2".into());
    reference.runtime.maximum_ues = 16;
    reference.core.hnbgw.address = "127.0.0.1".into();
    loaded.cell.broadcast_state_file = reference.cell.broadcast_state_file.clone();
    assert_eq!(
        serde_json::to_value(loaded).unwrap(),
        serde_json::to_value(reference).unwrap()
    );
}

#[test]
fn rel99_only_mode_disables_hspa_without_replacing_packet_defaults() {
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    operator["cell"]["packet_modes"] = serde_yaml::from_str("[rel99]").unwrap();
    let loaded = Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
    assert!(!loaded.config.hspa.downlink.enabled);
    assert!(!loaded.config.hspa.uplink.enabled);
    assert_eq!(
        loaded.config.rel99.physical.planner.services.packet[0]
            .downlink
            .rlc_bits,
        2040
    );
}

#[test]
fn named_network_modes_and_numeric_operator_overrides_are_equivalent() {
    for (name, number, expected) in [
        ("combined-paging", 1, NetworkOperationMode::CombinedPaging),
        ("separate-paging", 2, NetworkOperationMode::SeparatePaging),
    ] {
        let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
        insert(&mut operator, "cell.nmo", Value::from(name)).unwrap();
        let named =
            Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
        operator["cell"]["nmo"] = Value::from(number);
        let numeric =
            Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
        assert_eq!(named.config.cell.nmo, expected);
        assert_eq!(named.effective, numeric.effective);
    }
    for value in ["3", "unknown", "null"] {
        let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
        insert(
            &mut operator,
            "cell.nmo",
            serde_yaml::from_str(value).unwrap(),
        )
        .unwrap();
        assert!(Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).is_err());
    }
}

#[test]
fn broadcast_state_requires_the_new_operator_key_without_the_old_alias() {
    let loaded = Deployment::parse(DEFAULTS, OPERATOR).unwrap();
    assert_eq!(
        loaded.config.cell.broadcast_state_file,
        std::env::current_dir()
            .unwrap()
            .join(".runtime/cell-broadcast.pb")
    );
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    let old_path = operator["cell"]
        .as_mapping_mut()
        .unwrap()
        .remove(Value::from("broadcast_state_file"))
        .unwrap();
    operator["cell"]["si_state_file"] = old_path;
    assert!(Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).is_err());
}

#[test]
fn runtime_paths_resolve_from_launch_directory_without_creating_files() {
    let cwd = std::env::current_dir().unwrap();
    let loaded = Deployment::parse(DEFAULTS, OPERATOR).unwrap();
    assert_eq!(
        loaded.config.cell.broadcast_state_file,
        cwd.join(".runtime/cell-broadcast.pb")
    );
    assert_eq!(
        loaded.config.management.grpc.socket_path,
        cwd.join(".runtime/management.sock")
    );
    assert_eq!(
        loaded.config.management.grpc.administrator_socket_path,
        cwd.join(".runtime/admin.sock")
    );
    assert_eq!(
        loaded.config.management.grpc.capture_directory,
        cwd.join(".runtime/captures")
    );

    let unused = std::env::temp_dir().join(format!("umts-preflight-paths-{}", std::process::id()));
    assert!(!unused.exists());
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    operator["cell"]["broadcast_state_file"] =
        Value::from(unused.join("cell.pb").to_str().unwrap());
    for (key, name) in [
        ("socket_path", "read.sock"),
        ("administrator_socket_path", "admin.sock"),
        ("capture_directory", "captures"),
    ] {
        insert(
            &mut operator,
            &format!("management.grpc.{key}"),
            Value::from(unused.join(name).to_str().unwrap()),
        )
        .unwrap();
    }
    let loaded = Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
    assert_eq!(
        loaded.config.cell.broadcast_state_file,
        unused.join("cell.pb")
    );
    assert_eq!(
        loaded.config.management.grpc.socket_path,
        unused.join("read.sock")
    );
    assert_eq!(
        loaded.config.management.grpc.administrator_socket_path,
        unused.join("admin.sock")
    );
    assert_eq!(
        loaded.config.management.grpc.capture_directory,
        unused.join("captures")
    );
    assert!(!unused.exists());
}

#[test]
fn removed_control_socket_and_empty_runtime_paths_are_rejected() {
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    operator["cell"]["broadcast_state_file"] = Value::from("");
    assert!(Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).is_err());
    let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
    insert(
        &mut operator,
        "management.control_socket_path",
        Value::from("unused.sock"),
    )
    .unwrap();
    assert!(Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).is_err());
}

#[test]
fn multi_gib_catalog_budgets_preserve_unsigned_values_through_deployment() {
    let path = "hspa.downlink.resources.scheduling.maximum_cell_bytes";
    for limit in [
        2_147_483_648_u64,
        2_684_354_560,
        3_221_225_472,
        u64::from(u32::MAX),
    ] {
        let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
        insert(&mut operator, path, Value::from(limit)).unwrap();
        let loaded =
            Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).unwrap();
        assert_eq!(
            u64::from(
                loaded
                    .config
                    .hspa
                    .downlink
                    .resources
                    .scheduling
                    .as_ref()
                    .unwrap()
                    .maximum_cell_bytes
            ),
            limit
        );
        assert_eq!(
            lookup(&loaded.effective, path).unwrap().as_u64(),
            Some(limit)
        );
    }
    for limit in [0_u64, u64::from(u32::MAX) + 1] {
        let mut operator: Value = serde_yaml::from_str(OPERATOR).unwrap();
        insert(&mut operator, path, Value::from(limit)).unwrap();
        assert!(
            Deployment::parse(DEFAULTS, &serde_yaml::to_string(&operator).unwrap()).is_err(),
            "{limit}"
        );
    }
}
