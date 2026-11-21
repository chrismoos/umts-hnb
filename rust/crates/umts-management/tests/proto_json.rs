// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_management::json::{from_value, to_value};
use umts_management::proto::{Configuration, Phase, SnapshotIdentity, StatusSnapshot, Ue};

#[test]
fn inter_frequency_measurement_json_preserves_zero_and_absent_frequency() {
    use umts_management::proto::{InterFrequencyCellMeasurement, MeasurementReport, RrcReports};
    let report = RrcReports {
        last_secondary_measurement: Some(MeasurementReport {
            identity: 14,
            inter_frequency_cells: vec![
                InterFrequencyCellMeasurement {
                    uarfcn_dl: Some(0),
                    ..Default::default()
                },
                InterFrequencyCellMeasurement {
                    uarfcn_dl: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    let value = to_value(&report).unwrap();
    assert_eq!(
        value["lastSecondaryMeasurement"]["interFrequencyCells"][0]["uarfcnDl"],
        0
    );
    assert!(
        value["lastSecondaryMeasurement"]["interFrequencyCells"][1]
            .get("uarfcnDl")
            .is_none()
    );
    assert_eq!(from_value::<RrcReports>(value).unwrap(), report);
}

#[test]
fn rrc_reports_preserve_optional_zero_identifiers_and_protocol_cause() {
    use umts_management::proto::{RrcProtocolError, RrcReports, RrcStatusReport};
    let ue = Ue {
        rrc_reports: Some(RrcReports {
            status_count: 1,
            last_status: Some(RrcStatusReport {
                cause: RrcProtocolError::ReceiverState.into(),
                rejected_message_type: Some(0),
                transaction: Some(0),
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let json = to_value(&ue).unwrap();
    assert_eq!(
        json["rrcReports"]["lastStatus"]["cause"],
        "RRC_PROTOCOL_ERROR_RECEIVER_STATE"
    );
    assert_eq!(json["rrcReports"]["lastStatus"]["transaction"], 0);
    assert_eq!(json["rrcReports"]["lastStatus"]["rejectedMessageType"], 0);
    assert_eq!(from_value::<Ue>(json).unwrap(), ue);
}

#[test]
fn identifiers_and_timestamps_round_trip_without_javascript_precision_loss() {
    let identity = SnapshotIdentity {
        instance_id: "instance".into(),
        sequence: u64::MAX,
        epoch: 2_557_346_559_598_295_823,
        uptime_ms: u64::MAX,
        observed_unix_ms: i64::MIN,
    };
    let json = to_value(&identity).unwrap();
    assert_eq!(json["sequence"], u64::MAX.to_string());
    assert_eq!(json["epoch"], "2557346559598295823");
    assert_eq!(json["uptimeMs"], u64::MAX.to_string());
    assert_eq!(json["observedUnixMs"], i64::MIN.to_string());
    assert_eq!(from_value::<SnapshotIdentity>(json).unwrap(), identity);
}

#[test]
fn enum_names_and_optional_zero_values_follow_proto_json() {
    let status = StatusSnapshot {
        phase: Phase::Running.into(),
        ..Default::default()
    };
    let json = to_value(&status).unwrap();
    assert_eq!(json["phase"], "PHASE_RUNNING");
    assert!(json.get("identity").is_none());
    assert_eq!(from_value::<StatusSnapshot>(json).unwrap(), status);
    let ue = Ue {
        downlink_scrambling_code: Some(0),
        ..Default::default()
    };
    let json = to_value(&ue).unwrap();
    assert_eq!(json["downlinkScramblingCode"], 0);
    assert!(json.get("cqi").is_none());
    assert_eq!(from_value::<Ue>(json).unwrap(), ue);
}

#[test]
fn parser_accepts_proto_names_numeric_integers_and_unknown_enum_values() {
    let config: Configuration = from_value(serde_json::json!({
        "device_tick_rate_hz": 30_720_000,
        "configured_tx_gain_db": 75
    }))
    .unwrap();
    assert_eq!(config.device_tick_rate_hz, Some(30_720_000));
    assert_eq!(config.configured_tx_gain_db, 75.0);
    let status: StatusSnapshot = from_value(serde_json::json!({"phase": i32::MAX})).unwrap();
    assert_eq!(status.phase, i32::MAX);
    let json = to_value(&status).unwrap();
    assert_eq!(json["phase"], i32::MAX);
    assert_eq!(from_value::<StatusSnapshot>(json).unwrap(), status);
}

#[test]
fn dual_carrier_snapshot_keeps_primary_zero_cqi_and_secondary_absence_distinct() {
    use umts_management::proto::{Harq, HsdpaCarrier, HsdpaCarrierRole, HsdpaCarrierState};
    let ue = Ue {
        hsdpa_carriers: vec![
            HsdpaCarrier {
                role: HsdpaCarrierRole::Primary.into(),
                state: HsdpaCarrierState::Active.into(),
                cqi: Some(0),
                feedback: Some(Harq {
                    ack: 7,
                    ..Default::default()
                }),
                ..Default::default()
            },
            HsdpaCarrier {
                role: HsdpaCarrierRole::Secondary.into(),
                state: HsdpaCarrierState::Configured.into(),
                cqi: None,
                maximum_new_transport_bits: 42192,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let json = to_value(&ue).unwrap();
    assert_eq!(json["hsdpaCarriers"][0]["cqi"], 0);
    assert!(json["hsdpaCarriers"][1].get("cqi").is_none());
    assert_eq!(json["hsdpaCarriers"][1]["maximumNewTransportBits"], "42192");
    assert_eq!(from_value::<Ue>(json).unwrap(), ue);
}

#[test]
fn parked_rrc_states_keep_their_numbers_and_names() {
    use umts_management::proto::RrcState;
    for (state, number, name) in [
        (RrcState::CellPch, 6, "RRC_STATE_CELL_PCH"),
        (RrcState::UraPch, 7, "RRC_STATE_URA_PCH"),
    ] {
        assert_eq!(i32::from(state), number);
        assert_eq!(state.as_str_name(), name);
        assert_eq!(RrcState::try_from(number), Ok(state));
        let ue = Ue {
            rrc_state: state.into(),
            ..Default::default()
        };
        let json = to_value(&ue).unwrap();
        assert_eq!(json["rrcState"], name);
        assert_eq!(from_value::<Ue>(json).unwrap(), ue);
    }
    for (state, number) in [
        (RrcState::Unspecified, 0),
        (RrcState::Connecting, 1),
        (RrcState::CellFach, 2),
        (RrcState::CellDch, 3),
        (RrcState::Releasing, 4),
        (RrcState::Failed, 5),
    ] {
        assert_eq!(i32::from(state), number);
    }
}
