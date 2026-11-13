// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::{ReceiveIdentity, ReceivePlanStatus},
    serving::Description,
};
use anyhow::{Context, Result, anyhow, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};
use tonic::Code;
use umts_config::Config;
use umts_management::{
    bridge::{self, Action, Completion, Controller, Snapshot},
    proto,
    server::{ManagementServer, Settings},
};

const COMMANDS_PER_CYCLE: usize = 4;
const COMPLETION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const FINAL_STATUS_GRACE: Duration = Duration::from_secs(1);
const COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(10);

mod captures;

pub struct Management {
    controller: Controller,
    state: Arc<bridge::State>,
    server: ManagementServer,
    captures: captures::Captures,
    pending_capture_ports: Option<[crate::radio_io::capture::live::Port; 2]>,
    snapshot: Snapshot,
    completions: VecDeque<Completion>,
    shutdown_operations: Vec<String>,
    shutdown: bool,
    worker_failure_reported: bool,
    last_published: Instant,
}

fn binary_digest() -> Result<String> {
    let mut file = std::fs::File::open(std::env::current_exe()?)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hash.update(&buffer[..length]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl Management {
    pub fn start(config: &Config, source: &[u8], description: Description) -> Result<Option<Self>> {
        let settings = &config.management.grpc;
        if !settings.enabled {
            return Ok(None);
        }
        let digest = format!("{:x}", Sha256::digest(source));
        let snapshot = Snapshot {
            status: proto::StatusSnapshot {
                identity: Some(proto::SnapshotIdentity {
                    epoch: description.epoch.get(),
                    ..Default::default()
                }),
                phase: proto::Phase::Starting.into(),
                build_version: env!("CARGO_PKG_VERSION").into(),
                build_commit: env!("HNB_BUILD_COMMIT").into(),
                binary_sha256: binary_digest().context("hash serving executable")?,
                configuration_sha256: digest.clone(),
                ..Default::default()
            },
            configuration: proto::Configuration {
                plmn: config.cell.plmn.clone(),
                lac: config.cell.lac.into(),
                rac: config.cell.rac.into(),
                cell_id: config.cell.cell_id,
                uarfcn_dl: config.radio.uarfcn_dl,
                uarfcn_ul: config.radio.uarfcn_ul,
                configured_tx_gain_db: config.radio.tx_gain_db,
                configured_rx_gain_db: config.radio.rx_gain_db,
                hsdpa_enabled: config.hspa.downlink.enabled,
                hsupa_enabled: config.hspa.uplink.enabled,
                backend: config.radio.backend.clone(),
                configuration_sha256: digest,
                configured_downlink_hz: description.downlink_hz as f64,
                configured_uplink_hz: description.uplink_hz as f64,
                ..Default::default()
            },
            ..Default::default()
        };
        let (controller, driver, state) = bridge::channel(snapshot.clone());
        let server = ManagementServer::spawn(
            Settings {
                socket_path: settings.socket_path.clone(),
                administrator_socket_path: settings.administrator_socket_path.clone(),
                tcp_address: settings
                    .tcp_address
                    .as_deref()
                    .map(str::parse)
                    .transpose()?,
                tls_certificate: settings.tls_certificate.clone(),
                tls_key: settings.tls_key.clone(),
                tls_client_ca: settings.tls_client_ca.clone(),
                administrator_certificates_sha256: settings
                    .administrator_certificates_sha256
                    .clone(),
            },
            driver,
            Arc::clone(&state),
        )?;
        let captures = captures::Captures::start(
            settings.capture_directory.clone(),
            settings.maximum_capture_disk_mib,
            controller.completion_sink(),
            serde_json::json!({
                "primary_scrambling_code": config.cell.physical.downlink_scrambling_code,
                "prach": config.cell.access.prach,
                "cpich_tx_dbm": config.cell.advertised_power.cpich_tx_dbm,
                "uplink_interference_dbm": config.cell.advertised_power.uplink_interference_dbm,
            }),
        )?;
        tracing::info!(
            "Management ready: read={} admin={} instance={}",
            settings.socket_path.display(),
            settings.administrator_socket_path.display(),
            controller.instance_id()
        );
        Ok(Some(Self {
            controller,
            state,
            server,
            captures,
            pending_capture_ports: None,
            snapshot,
            completions: VecDeque::with_capacity(bridge::MAXIMUM_OPERATIONS),
            shutdown_operations: Vec::with_capacity(bridge::MAXIMUM_OPERATIONS),
            shutdown: false,
            worker_failure_reported: false,
            last_published: Instant::now(),
        }))
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown
    }

    fn drain_completions(&mut self) -> bool {
        while let Some(completion) = self.completions.pop_front() {
            if let Err(completion) = self.controller.try_complete(completion) {
                self.completions.push_front(completion);
                return false;
            }
        }
        true
    }

    pub fn service(&mut self) {
        if let Some(ports) = self.pending_capture_ports.take()
            && let Err(ports) = self.captures.attach(ports)
        {
            self.pending_capture_ports = Some(*ports);
        }
        if self.server.is_finished() {
            if !self.worker_failure_reported {
                tracing::error!(
                    "Management worker stopped unexpectedly. Radio ownership is unchanged."
                );
                self.worker_failure_reported = true;
            }
            return;
        }
        if !self.drain_completions() {
            return;
        }
        for _ in 0..COMMANDS_PER_CYCLE {
            let Some(command) = self.controller.try_next() else {
                break;
            };
            let epoch = self
                .snapshot
                .status
                .identity
                .as_ref()
                .map_or(0, |identity| identity.epoch);
            let rejection = if command.expires <= Instant::now() {
                Some((
                    Code::DeadlineExceeded,
                    "command expired before execution".into(),
                ))
            } else if command.expected_epoch != epoch {
                Some((
                    Code::FailedPrecondition,
                    "radio epoch changed before execution".into(),
                ))
            } else {
                None
            };
            if let Some(error) = rejection {
                self.completions.push_back(Completion::Finished {
                    operation_id: command.operation_id,
                    result: Err(error),
                });
            } else {
                match &command.action {
                    Action::Shutdown => {
                        self.shutdown = true;
                        self.completions.push_back(Completion::Running {
                            operation_id: command.operation_id.clone(),
                            capture_id: String::new(),
                        });
                        self.shutdown_operations.push(command.operation_id);
                    }
                    Action::StartCapture { .. } | Action::StopCapture { .. } => {
                        if let Err((code, command)) = self
                            .captures
                            .dispatch(command, Box::new((*self.state.snapshot()).clone()))
                        {
                            self.completions.push_back(Completion::Finished {
                                operation_id: command.operation_id,
                                result: Err((code, "capture I/O command queue unavailable".into())),
                            });
                        }
                    }
                }
            }
            if !self.drain_completions() {
                break;
            }
        }
    }

    pub fn update(&mut self, mut snapshot: Snapshot) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_published).as_secs_f64();
        for ue in &mut snapshot.ues {
            if let Some(previous) = self.snapshot.ues.iter().find(|previous| {
                previous.reference == ue.reference && previous.configuration == ue.configuration
            }) {
                ue.power.clone_from(&previous.power);
                // The base serving snapshot does not own receive telemetry.
                // Retain the last observation only for this exact receiver, as for power, until
                // receive() supplies a new one.
                if ue.reference.is_some()
                    && let Some(counters) = &previous.counters
                {
                    let values = &mut ue.counters.get_or_insert_default().values;
                    for value in counters
                        .values
                        .iter()
                        .filter(|value| value.name.starts_with("rx.dch."))
                    {
                        if !values.iter().any(|current| current.name == value.name) {
                            values.push(value.clone());
                        }
                    }
                }
                ue.downlink_payload_bps =
                    rate(previous, ue, "hsdpa.acknowledged_mac_bits", elapsed);
                ue.uplink_payload_bps =
                    rate(previous, ue, "hsupa.decoded_mac_payload_bits", elapsed);
            }
        }
        snapshot
            .status
            .build_version
            .clone_from(&self.snapshot.status.build_version);
        snapshot
            .status
            .build_commit
            .clone_from(&self.snapshot.status.build_commit);
        snapshot
            .status
            .binary_sha256
            .clone_from(&self.snapshot.status.binary_sha256);
        snapshot
            .status
            .configuration_sha256
            .clone_from(&self.snapshot.status.configuration_sha256);
        let tick_rate = snapshot.configuration.device_tick_rate_hz;
        snapshot.configuration = self.snapshot.configuration.clone();
        snapshot.configuration.device_tick_rate_hz = tick_rate;
        self.snapshot = snapshot;
        self.last_published = now;
    }

    pub fn attach_capture_ports(&mut self, ports: [crate::radio_io::capture::live::Port; 2]) {
        self.snapshot.configuration.applied_sample_rate_hz =
            Some(ports[0].geometry().sample_rate.hz());
        self.pending_capture_ports = Some(ports);
        self.service();
    }

    pub fn receive(&mut self, identity: ReceiveIdentity, status: &ReceivePlanStatus) {
        if let Some(ue) = self.snapshot.ues.iter_mut().find(|ue| {
            ue.reference.is_some_and(|reference| {
                reference.epoch == identity.ue.epoch.get()
                    && reference.slot == identity.ue.slot
                    && reference.generation == identity.ue.generation.get()
            }) && ue.configuration == identity.configuration.get()
        }) {
            crate::serving::receive_metrics(ue, status);
        }
    }

    pub fn publish_received(&mut self) -> Result<()> {
        self.controller
            .publish(self.snapshot.clone())
            .map_err(|error| anyhow!(error))
    }

    pub fn phase(&mut self, phase: proto::Phase, epoch: u64, failure: &str) -> Result<()> {
        if self
            .snapshot
            .status
            .identity
            .as_ref()
            .is_some_and(|identity| identity.epoch != epoch)
        {
            self.snapshot.radio_counters = proto::Counters::default();
            self.snapshot.status.counters = None;
        }
        self.snapshot.status.phase = phase.into();
        self.snapshot.status.radio_ready = false;
        self.snapshot.status.core_connected = false;
        self.snapshot.status.failure_reason = failure.into();
        self.snapshot
            .status
            .identity
            .as_mut()
            .expect("management identity")
            .epoch = epoch;
        if matches!(
            phase,
            proto::Phase::Starting
                | proto::Phase::Recovering
                | proto::Phase::Stopped
                | proto::Phase::Failed
        ) {
            self.snapshot.ues.clear();
            self.snapshot.status.registered_ues = 0;
            self.snapshot.status.active_bearers = 0;
            self.snapshot.status.bearer_snapshot_complete = false;
            self.snapshot.configuration.applied_sample_rate_hz = None;
            self.snapshot.configuration.device_tick_rate_hz = None;
        }
        self.publish_received()
    }

    pub fn finish(&mut self, result: &Result<()>) -> Result<()> {
        self.controller.close_commands();
        for _ in 0..bridge::COMMAND_CAPACITY {
            let Some(command) = self.controller.try_next() else {
                break;
            };
            self.completions.push_back(Completion::Finished {
                operation_id: command.operation_id,
                result: Err((
                    Code::Unavailable,
                    "serving stopped before command execution".into(),
                )),
            });
        }
        let capture_result = self.captures.finish();
        let epoch = self
            .snapshot
            .status
            .identity
            .as_ref()
            .map_or(0, |identity| identity.epoch);
        let mut failure = result
            .as_ref()
            .err()
            .map(|error| format!("{error:#}"))
            .unwrap_or_default();
        if let Err(error) = &capture_result {
            if !failure.is_empty() {
                failure.push_str(". ");
            }
            failure.push_str(&format!("capture shutdown failed: {error:#}"));
        }
        let succeeded = result.is_ok() && capture_result.is_ok();
        self.phase(
            if succeeded {
                proto::Phase::Stopped
            } else {
                proto::Phase::Failed
            },
            epoch,
            &failure,
        )?;
        for operation_id in self.shutdown_operations.drain(..) {
            self.completions.push_back(Completion::Finished {
                operation_id,
                result: if succeeded {
                    Ok(())
                } else {
                    Err((Code::Internal, failure.clone()))
                },
            });
        }
        let until = Instant::now() + COMPLETION_DRAIN_TIMEOUT;
        while !self.drain_completions() && Instant::now() < until && !self.server.is_finished() {
            std::thread::sleep(COMPLETION_POLL_INTERVAL);
        }
        ensure!(
            self.completions.is_empty(),
            "management outcomes retained but server unavailable"
        );
        while self.state.status()?.pending_operations != 0
            && Instant::now() < until
            && !self.server.is_finished()
        {
            std::thread::sleep(COMPLETION_POLL_INTERVAL);
        }
        ensure!(
            self.state.status()?.pending_operations == 0,
            "management outcomes were not consumed before shutdown"
        );
        if self.shutdown {
            std::thread::sleep(FINAL_STATUS_GRACE);
        }
        self.server.join()?;
        capture_result
    }
}

fn rate(previous: &proto::Ue, current: &proto::Ue, name: &str, seconds: f64) -> Option<f64> {
    let read = |ue: &proto::Ue| {
        ue.counters
            .as_ref()?
            .values
            .iter()
            .find(|counter| counter.name == name)
            .map(|counter| counter.value)
    };
    let bits = read(current)?.checked_sub(read(previous)?)?;
    (seconds.is_finite() && seconds > 0.0).then(|| bits as f64 / seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroU64, os::unix::fs::PermissionsExt, sync::Arc};
    use umts_management::proto::get_operation_request::Selector;
    use umts_types::RadioEpoch;

    fn manager() -> (Management, Arc<bridge::State>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: Config = serde_yaml::from_str(include_str!(
            "../../../../rust/test-support/fixtures/config/rel99.yaml"
        ))
        .unwrap();
        config.management.grpc.socket_path = directory.path().join("read.sock");
        config.management.grpc.administrator_socket_path = directory.path().join("admin.sock");
        config.management.grpc.capture_directory = directory.path().join("captures");
        let description = Description {
            epoch: RadioEpoch::new(NonZeroU64::new(42).unwrap()),
            downlink_hz: 880_000_000,
            uplink_hz: 835_000_000,
            core: "127.0.0.1:29169".parse().unwrap(),
        };
        let manager = Management::start(&config, b"configuration fixture", description)
            .unwrap()
            .unwrap();
        let state = Arc::clone(&manager.state);
        (manager, state, directory)
    }

    #[test]
    fn refresh_preserves_receive_counters_only_for_the_same_receiver() {
        let (mut manager, _, _directory) = manager();
        let reference = proto::UeRef {
            epoch: 42,
            slot: 0,
            generation: 1,
        };
        let previous = proto::Ue {
            reference: Some(reference),
            configuration: 3,
            counters: Some(proto::Counters {
                values: vec![
                    proto::Counter {
                        name: "rx.dch.failed_crc".into(),
                        value: 7,
                    },
                    proto::Counter {
                        name: "not_receive_owned".into(),
                        value: 99,
                    },
                ],
            }),
            ..Default::default()
        };
        for (epoch, slot, generation, configuration, preserve) in [
            (42, 0, 1, 3, true),
            (43, 0, 1, 3, false),
            (42, 1, 1, 3, false),
            (42, 0, 2, 3, false),
            (42, 0, 1, 4, false),
        ] {
            manager.snapshot.ues = vec![previous.clone()];
            let mut fresh = manager.snapshot.clone();
            fresh.ues = vec![proto::Ue {
                reference: Some(proto::UeRef {
                    epoch,
                    slot,
                    generation,
                }),
                configuration,
                ..Default::default()
            }];
            manager.update(fresh);
            let values = manager.snapshot.ues[0]
                .counters
                .as_ref()
                .map(|counters| counters.values.as_slice())
                .unwrap_or_default();
            assert_eq!(values.len(), usize::from(preserve));
            if preserve {
                assert_eq!(values[0].name, "rx.dch.failed_crc");
                assert_eq!(values[0].value, 7);
            }
        }
        // A value already present in the fresh snapshot wins. It is neither duplicated nor replaced
        // with an older observation.
        manager.snapshot.ues = vec![previous.clone()];
        let mut fresh = manager.snapshot.clone();
        fresh.ues[0].counters.as_mut().unwrap().values[0].value = 8;
        manager.update(fresh);
        let values = &manager.snapshot.ues[0].counters.as_ref().unwrap().values;
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].value, 8);
    }

    fn command(state: &bridge::State) -> proto::CommandIdentity {
        proto::CommandIdentity {
            request_id: "a076b2b8-38fa-4fc2-b240-d8f5433d5806".into(),
            expected_instance_id: state
                .snapshot()
                .status
                .identity
                .as_ref()
                .unwrap()
                .instance_id
                .clone(),
            expected_epoch: 42,
        }
    }

    fn terminal(state: &bridge::State, operation: &proto::Operation) -> proto::Operation {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let outcome = state
                .operation(&Selector::OperationId(operation.id.clone()))
                .unwrap();
            if matches!(
                proto::OperationPhase::try_from(outcome.phase),
                Ok(proto::OperationPhase::Succeeded | proto::OperationPhase::Failed)
            ) {
                return outcome;
            }
            assert!(Instant::now() < deadline, "operation did not settle");
            std::thread::sleep(COMPLETION_POLL_INTERVAL);
        }
    }

    #[test]
    fn queued_command_rechecks_epoch_at_execution() {
        let (mut manager, state, _directory) = manager();
        manager.snapshot.radio_counters.values.push(proto::Counter {
            name: "old.epoch".into(),
            value: 123,
        });
        let operation = state
            .submit(command(&state), Action::Shutdown, Duration::from_secs(1))
            .unwrap();
        manager.phase(proto::Phase::Starting, 43, "").unwrap();
        assert!(state.snapshot().radio_counters.values.is_empty());
        manager.service();
        let outcome = terminal(&state, &operation);
        assert_eq!(outcome.error_code, i32::from(Code::FailedPrecondition));
        assert!(!manager.shutdown_requested());
        manager.finish(&Ok(())).unwrap();
    }

    #[test]
    fn shutdown_outcome_waits_for_final_retirement_and_closes_admission() {
        let (mut manager, state, _directory) = manager();
        let operation = state
            .submit(command(&state), Action::Shutdown, Duration::from_secs(1))
            .unwrap();
        manager.service();
        assert!(manager.shutdown_requested());
        assert_ne!(
            state
                .operation(&Selector::OperationId(operation.id.clone()))
                .unwrap()
                .phase,
            i32::from(proto::OperationPhase::Succeeded)
        );
        manager.finish(&Ok(())).unwrap();
        assert_eq!(
            terminal(&state, &operation).phase,
            i32::from(proto::OperationPhase::Succeeded)
        );
        let mut next = command(&state);
        next.request_id = "19c694a8-0561-4894-a90d-bc058f0388c2".into();
        assert_eq!(
            state
                .submit(next, Action::Shutdown, Duration::from_secs(1))
                .unwrap_err()
                .code(),
            Code::Unavailable
        );
    }

    #[test]
    fn payload_rates_reject_resets_and_unknown_or_zero_intervals() {
        let previous = proto::Ue {
            counters: Some(proto::Counters {
                values: vec![proto::Counter {
                    name: "bits".into(),
                    value: 100,
                }],
            }),
            ..Default::default()
        };
        let mut current = previous.clone();
        current.counters.as_mut().unwrap().values[0].value = 300;
        assert_eq!(rate(&previous, &current, "bits", 2.0), Some(100.0));
        assert_eq!(rate(&previous, &current, "bits", 0.0), None);
        assert_eq!(rate(&previous, &current, "missing", 2.0), None);
        current.counters.as_mut().unwrap().values[0].value = 50;
        assert_eq!(rate(&previous, &current, "bits", 2.0), None);
    }

    #[test]
    fn capture_commands_record_and_finalize_without_stopping_the_stream() {
        use crate::radio_io::capture::live;
        use num_complex::Complex32;
        use umts_types::time::SampleRate;
        let (mut manager, state, directory) = manager();
        let geometry = live::Geometry {
            direction: live::Direction::Receive,
            epoch: 42,
            sample_rate: SampleRate::new(1000, 1).unwrap(),
            tick_rate: NonZeroU64::new(4000).unwrap(),
        };
        let (receive_port, mut receive) = live::channel(geometry);
        let (transmit_port, _transmit) = live::channel(live::Geometry {
            direction: live::Direction::Transmit,
            ..geometry
        });
        manager.attach_capture_ports([receive_port, transmit_port]);
        let mut snapshot = manager.snapshot.clone();
        snapshot.status.phase = proto::Phase::Running.into();
        snapshot.status.radio_ready = true;
        manager.update(snapshot);
        manager.publish_received().unwrap();
        let operation = state
            .submit(
                command(&state),
                Action::StartCapture {
                    direction: proto::CaptureDirection::Rx,
                    duration_ms: 10,
                    maximum_mib: 1,
                },
                Duration::from_secs(1),
            )
            .unwrap();
        manager.service();
        let until = Instant::now() + Duration::from_secs(2);
        let values = [Complex32::new(0.25, -0.5); 8];
        let mut first = 0;
        loop {
            receive.record_receive(first, first * 4, first, &values, false);
            first += values.len() as u64;
            let outcome = state
                .operation(&Selector::OperationId(operation.id.clone()))
                .unwrap();
            if matches!(
                proto::OperationPhase::try_from(outcome.phase),
                Ok(proto::OperationPhase::Succeeded | proto::OperationPhase::Failed)
            ) {
                assert_eq!(
                    outcome.phase,
                    i32::from(proto::OperationPhase::Succeeded),
                    "{}",
                    outcome.error_message
                );
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(COMPLETION_POLL_INTERVAL);
        }
        let captures = state.captures().unwrap();
        assert_eq!(captures.len(), 1);
        let row = &captures[0];
        assert_eq!(row.phase, i32::from(proto::CapturePhase::Complete));
        assert_eq!(row.samples, 10);
        assert!(!row.stopped_early);
        let path = std::path::Path::new(&row.directory);
        assert!(path.starts_with(directory.path()));
        assert!(path.join("capture.json").exists());
        assert!(path.join("checksums.json").exists());
        assert_eq!(
            &std::fs::read(path.join("raw-rx-0.bin")).unwrap()[..8],
            b"UMTSRX01"
        );
        assert!(!manager.shutdown_requested());
        manager.finish(&Ok(())).unwrap();
    }
}
