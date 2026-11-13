// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use num_complex::Complex32;
use std::{num::NonZeroU64, os::unix::fs::PermissionsExt, sync::Arc};
use umts_management::{
    bridge::{self, Controller, Driver, Snapshot, State},
    proto::get_operation_request::Selector,
};
use umts_types::time::SampleRate;

struct Rig {
    owner: Owner,
    recorders: [live::Recorder; 2],
    controller: Controller,
    driver: Driver,
    state: Arc<State>,
    directory: tempfile::TempDir,
}

#[test]
fn manifest_redacts_subscriber_identity_and_preserves_protocol_values() {
    let directory = tempfile::tempdir().unwrap();
    let snapshot = Snapshot {
        status: proto::StatusSnapshot {
            identity: Some(proto::SnapshotIdentity {
                epoch: u64::MAX,
                ..Default::default()
            }),
            phase: i32::MAX,
            ..Default::default()
        },
        ues: vec![proto::Ue {
            reference: Some(proto::UeRef {
                epoch: u64::MAX,
                slot: 0,
                generation: u64::MAX,
            }),
            imsi: Some("001010000000001".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    write_manifest(directory.path(), &snapshot, &serde_json::json!({})).unwrap();
    let bytes = fs::read(directory.path().join("capture.json")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["status"]["identity"]["epoch"], u64::MAX.to_string());
    assert_eq!(value["status"]["phase"], i32::MAX);
    assert_eq!(
        value["ues"][0]["reference"]["generation"],
        u64::MAX.to_string()
    );
    assert!(value["ues"][0].get("imsi").is_none());
    assert!(
        !String::from_utf8(bytes)
            .unwrap()
            .contains("001010000000001")
    );
}

impl Rig {
    fn new(disk_mib: u64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        private_root(directory.path()).unwrap();
        let geometry = live::Geometry {
            direction: live::Direction::Receive,
            epoch: 42,
            sample_rate: SampleRate::new(1000, 1).unwrap(),
            tick_rate: NonZeroU64::new(4000).unwrap(),
        };
        let (rx_port, rx_recorder) = live::channel(geometry);
        let (tx_port, tx_recorder) = live::channel(live::Geometry {
            direction: live::Direction::Transmit,
            ..geometry
        });
        let (controller, driver, state) = bridge::channel(Snapshot {
            status: proto::StatusSnapshot {
                identity: Some(proto::SnapshotIdentity {
                    epoch: 42,
                    ..Default::default()
                }),
                phase: proto::Phase::Running.into(),
                radio_ready: true,
                ..Default::default()
            },
            ..Default::default()
        });
        let owner = Owner {
            root: directory.path().to_owned(),
            disk_budget: disk_mib * MIB,
            cell_context: serde_json::json!({}),
            ports: Some([rx_port, tx_port]),
            pending_ports: None,
            active: [None, None],
            history: VecDeque::new(),
            sink: controller.completion_sink(),
        };
        Self {
            owner,
            recorders: [rx_recorder, tx_recorder],
            controller,
            driver,
            state,
            directory,
        }
    }

    fn command(&mut self, action: Action) -> proto::Operation {
        let operation = self
            .state
            .submit(
                proto::CommandIdentity {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    expected_instance_id: self.controller.instance_id().into(),
                    expected_epoch: 42,
                },
                action,
                Duration::from_secs(1),
            )
            .unwrap();
        self.owner
            .command(
                self.controller.try_next().unwrap(),
                (*self.state.snapshot()).clone(),
            )
            .unwrap();
        self.driver.poll().unwrap();
        operation
    }

    fn start(&mut self, direction: proto::CaptureDirection, duration_ms: u32) -> proto::Operation {
        self.command(Action::StartCapture {
            direction,
            duration_ms,
            maximum_mib: 1,
        })
    }

    fn outcome(&self, operation: &proto::Operation) -> proto::Operation {
        self.state
            .operation(&Selector::OperationId(operation.id.clone()))
            .unwrap()
    }
}

#[test]
fn quota_rejection_does_not_allocate_a_capture_or_create_files() {
    let mut rig = Rig::new(1);
    let operation = rig.start(proto::CaptureDirection::Rx, 10);
    assert_eq!(
        rig.outcome(&operation).error_code,
        i32::from(Code::ResourceExhausted)
    );
    assert!(rig.state.captures().unwrap().is_empty());
    assert_eq!(fs::read_dir(rig.directory.path()).unwrap().count(), 0);
}

#[test]
fn concurrent_directions_and_manual_stop_keep_distinct_operation_custody() {
    let mut rig = Rig::new(64);
    let receive = rig.start(proto::CaptureDirection::Rx, 1000);
    let transmit = rig.start(proto::CaptureDirection::Tx, 2);
    let denied = rig.start(proto::CaptureDirection::Rx, 1000);
    assert_eq!(
        rig.outcome(&denied).error_code,
        i32::from(Code::ResourceExhausted)
    );
    let values = [Complex32::new(0.25, -0.5); 4];
    rig.recorders[0].record_receive(100, 400, 1, &values, false);
    let id = rig.outcome(&receive).capture_id;
    let stop = rig.command(Action::StopCapture {
        capture_id: id.clone(),
    });
    assert_eq!(
        rig.outcome(&stop).phase,
        i32::from(proto::OperationPhase::Running)
    );
    rig.recorders[0].poll();
    rig.recorders[1].record_transmit(100, 400, 1, &values, Some(4), false);
    rig.owner.collect().unwrap();
    rig.driver.poll().unwrap();
    for operation in [&receive, &transmit, &stop] {
        let outcome = rig.outcome(operation);
        assert_eq!(
            outcome.phase,
            i32::from(proto::OperationPhase::Succeeded),
            "{}",
            outcome.error_message
        );
    }
    let captures = rig.state.captures().unwrap();
    let row = captures.iter().find(|row| row.id == id).unwrap();
    assert!(row.stopped_early);
    assert_eq!(row.samples, 4);
    assert!(Path::new(&row.directory).join("checksums.json").exists());
    let next = rig.start(proto::CaptureDirection::Rx, 1);
    rig.recorders[0].record_receive(104, 416, 2, &values, false);
    rig.owner.collect().unwrap();
    rig.driver.poll().unwrap();
    assert_eq!(
        rig.outcome(&next).phase,
        i32::from(proto::OperationPhase::Succeeded)
    );
}

#[test]
fn finalize_collision_is_terminal_and_does_not_overwrite_an_existing_file() {
    let mut rig = Rig::new(64);
    let operation = rig.start(proto::CaptureDirection::Rx, 1);
    let row = rig.state.captures().unwrap().pop().unwrap();
    let path = Path::new(&row.directory).join("raw-rx-0.bin");
    fs::write(&path, b"preserve this file").unwrap();
    rig.recorders[0].record_receive(100, 400, 1, &[Complex32::new(0.25, -0.5); 4], false);
    rig.owner.collect().unwrap();
    rig.driver.poll().unwrap();
    assert_eq!(
        rig.outcome(&operation).error_code,
        i32::from(Code::DataLoss)
    );
    assert_eq!(fs::read(path).unwrap(), b"preserve this file");
    assert_eq!(
        rig.state.captures().unwrap()[0].phase,
        i32::from(proto::CapturePhase::Failed)
    );
}

#[test]
fn invalid_capture_data_fails_without_stopping_the_recorder() {
    let mut rig = Rig::new(64);
    let invalid = rig.start(proto::CaptureDirection::Rx, 1);
    rig.recorders[0].record_receive(100, 400, 1, &[Complex32::new(f32::NAN, 0.0)], false);
    rig.owner.collect().unwrap();
    rig.driver.poll().unwrap();
    assert_eq!(rig.outcome(&invalid).error_code, i32::from(Code::DataLoss));
    let valid = rig.start(proto::CaptureDirection::Rx, 1);
    rig.recorders[0].record_receive(104, 416, 2, &[Complex32::new(0.25, -0.5)], false);
    rig.owner.collect().unwrap();
    rig.driver.poll().unwrap();
    assert_eq!(
        rig.outcome(&valid).phase,
        i32::from(proto::OperationPhase::Succeeded)
    );
}
