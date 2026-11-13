// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_control::ControlIngress,
    common_rx::{AccessRxSettings, RachQueueLimits, prepare_common_rx},
    common_tx::ControlPorts,
    core_link::tests::{Mock, ready},
    radio_io::{RxSettings, TimedRx, TxSettings, TxTiming, prepare_tx},
    signalling::tests::limits,
    test_support::{at, count, source},
};
use num_complex::Complex32;
use sdr_hal::{
    RadioRx, RadioTx, RxReadResult, TxMetadata, TxRadioHealth,
    error::{Error, ErrorKind},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};
use umts_dsp::pulse_shaping::SamplesPerChip;
use umts_rlc::transparent::TmLimits;
use umts_runtime::worker::WorkerExit;

const ORIGIN: u64 = 10_000_000;

mod retirement;

#[derive(Default)]
struct State {
    tick: AtomicU64,
    writes: AtomicU64,
    health_queries: AtomicU64,
    sequence_errors: AtomicU64,
    underflows: AtomicU64,
    tx_active: AtomicBool,
    rx_active: AtomicBool,
    hold_activation: AtomicBool,
    hold_write: AtomicBool,
    hold_read: AtomicBool,
    read_entered: AtomicBool,
    write_entered: AtomicBool,
    bad_clock: AtomicBool,
    bad_rx: AtomicBool,
    fail_tx_stop: AtomicBool,
    fail_rx_stop: AtomicBool,
    tx_stop_calls: AtomicU64,
    rx_stop_calls: AtomicU64,
    tx_drops: AtomicU64,
    rx_drops: AtomicU64,
}

fn until(mut condition: impl FnMut() -> bool) {
    let begin = Instant::now();
    while !condition() {
        assert!(
            begin.elapsed() < Duration::from_secs(2),
            "worker observation deadline"
        );
        thread::sleep(Duration::from_micros(100));
    }
}

struct Tx(Arc<State>);
impl RadioTx for Tx {
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        if self.0.bad_clock.load(Ordering::Acquire) {
            Err(Error::new(ErrorKind::Disconnected, "injected clock loss"))
        } else {
            Ok(self.0.tick.load(Ordering::Acquire))
        }
    }
    fn transmit(&mut self, _: &[Complex32]) -> Result<(), Error> {
        panic!("untimed TX")
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        assert!(matches!(metadata.schedule, sdr_hal::Schedule::At(_)));
        self.0.write_entered.store(true, Ordering::Release);
        until(|| !self.0.hold_write.load(Ordering::Acquire));
        self.0.writes.fetch_add(1, Ordering::Release);
        Ok(samples.len())
    }
    fn enable_transmit(&mut self, active: bool) -> Result<(), Error> {
        if active {
            until(|| !self.0.hold_activation.load(Ordering::Acquire));
        } else {
            self.0.tx_stop_calls.fetch_add(1, Ordering::AcqRel);
            if self.0.fail_tx_stop.load(Ordering::Acquire) {
                return Err(Error::new(ErrorKind::Backend, "injected TX stop failure"));
            }
        }
        self.0.tx_active.store(active, Ordering::Release);
        Ok(())
    }
    fn tx_health(&mut self) -> Result<TxRadioHealth, Error> {
        self.0.health_queries.fetch_add(1, Ordering::Release);
        Ok(TxRadioHealth {
            sequence_errors: self.0.sequence_errors.load(Ordering::Acquire),
            underflows: self.0.underflows.load(Ordering::Acquire),
            ..TxRadioHealth::default()
        })
    }
}
impl Drop for Tx {
    fn drop(&mut self) {
        self.0.tx_drops.fetch_add(1, Ordering::AcqRel);
    }
}

struct Rx {
    shared: Arc<State>,
}
impl RadioRx for Rx {
    fn rx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn tick_rate(&self) -> u64 {
        1_000_000_000
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.shared.tick.load(Ordering::Acquire))
    }
    fn rx_read(&mut self, _: &mut [Complex32], timeout: i64) -> Result<RxReadResult, Error> {
        self.shared.read_entered.store(true, Ordering::Release);
        until(|| !self.shared.hold_read.load(Ordering::Acquire));
        if self.shared.bad_rx.load(Ordering::Acquire) {
            return Err(Error::new(ErrorKind::Disconnected, "injected RX loss"));
        }
        thread::sleep(Duration::from_micros(timeout as u64));
        Ok(RxReadResult {
            samples_read: 0,
            time_ticks: 0,
            overflow: false,
        })
    }
    fn rx_activate(&mut self, _: Option<u64>) -> Result<(), Error> {
        self.shared.rx_active.store(true, Ordering::Release);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.shared.rx_stop_calls.fetch_add(1, Ordering::AcqRel);
        if self.shared.fail_rx_stop.load(Ordering::Acquire) {
            return Err(Error::new(ErrorKind::Backend, "injected RX stop failure"));
        }
        self.shared.rx_active.store(false, Ordering::Release);
        Ok(())
    }
}
impl Drop for Rx {
    fn drop(&mut self) {
        self.shared.rx_drops.fetch_add(1, Ordering::AcqRel);
    }
}

struct Parts {
    preparation: TxPreparation,
    pump: TxPump,
    rx: CommonRx,
    controller: CommonController<Mock>,
    shared: Arc<State>,
}

fn settings() -> EpochSettings {
    EpochSettings {
        idle_wait: Duration::from_micros(100),
        startup_timeout: Duration::from_secs(1),
        clock_stall_timeout: Duration::from_millis(100),
        control_origin: ControlTime(10),
        rx_activation_tick: None,
        health: TxHealthSettings {
            poll_interval: Duration::from_millis(1),
            maximum_underflows: 1,
            maximum_late_packets: 1,
            maximum_unknown_events: 0,
        },
    }
}

fn parts() -> Parts {
    parts_with_timing(TxTiming::DeviceClock(umts_radio::pacing::LeadDurations {
        minimum_submission: Duration::from_micros(100),
        maximum_submission: Duration::from_millis(2),
        maximum_preparation: Duration::from_millis(4),
    }))
}

fn parts_with_timing(timing: TxTiming) -> Parts {
    let shared = Arc::new(State::default());
    let (mut preparation, pump) = prepare_tx(
        source(false),
        Box::new(Tx(shared.clone())),
        TxSettings {
            timing,
            ..TxSettings::offline(ORIGIN, count(4), count(1), count(3))
        },
    )
    .unwrap();
    let ControlPorts {
        transport,
        acquisition,
    } = preparation
        .control_ports(count(4), count(1), count(4), count(2))
        .unwrap();
    let receiver = TimedRx::new(
        Box::new(Rx {
            shared: shared.clone(),
        }),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(997),
            timeout_us: 1000,
        },
    )
    .unwrap();
    let (rx, ingress) = prepare_common_rx(
        receiver,
        acquisition,
        SamplesPerChip::Two,
        AccessRxSettings {
            maximum_channels: count(2),
            minimum_coherent_fraction: 0.2,
            minimum_tx_lead_chips: 0,
            messages: crate::access_message::MessageLimits {
                capacity: count(2),
                maximum_payload_bits: count(512),
                maximum_formats: count(2),
                maximum_buffer_bytes: 4 * 1024 * 1024,
            },
        },
        RachQueueLimits {
            capacity: count(4),
            maximum_transport_bits: count(512),
            maximum_buffer_bytes: 4096,
        },
    )
    .unwrap();
    let mut signalling = CommonSignalling::new(&transport, 1, 1, limits()).unwrap();
    let (core, _) = ready(&mut signalling);
    let controller = CommonController::new(
        signalling,
        transport,
        ingress,
        ControlIngress::new(
            at(0).epoch,
            TmLimits {
                maximum_pdus: count(2),
                maximum_pdu_bits: count(254),
                maximum_sdu_bits: count(254),
            },
            count(2),
        )
        .unwrap(),
        core,
        crate::controller::ControllerSettings {
            maximum_ingress_age_chips: std::num::NonZeroU64::new(3_840_000).unwrap(),
            preparation_guard_chips: std::num::NonZeroU64::new(5120).unwrap(),
            maximum_future_frames: count(2),
            fach_scan_budget: count(25),
            retirement_scan_budget: count(4),
        },
    )
    .unwrap();
    Parts {
        preparation,
        pump,
        rx,
        controller,
        shared,
    }
}

fn spawn(parts: Parts, settings: EpochSettings) -> (CommonEpoch<Mock>, Arc<State>) {
    let Parts {
        preparation,
        pump,
        rx,
        controller,
        shared,
    } = parts;
    (
        CommonEpoch::spawn(preparation, pump, rx, controller, settings).unwrap(),
        shared,
    )
}

fn advance_to_air_origin(epoch: &mut CommonEpoch<Mock>, state: &State) {
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    state.tick.store(7_000_000, Ordering::Release);
    state.tick.store(8_700_000, Ordering::Release);
    until(|| state.writes.load(Ordering::Acquire) >= 1);
    state.tick.store(9_400_000, Ordering::Release);
    until(|| state.writes.load(Ordering::Acquire) >= 2);
    state
        .tick
        .store(epoch.clock.origin().tick, Ordering::Release);
    until(|| {
        epoch
            .clock
            .read()
            .is_ok_and(|value| value.tick == epoch.clock.origin().tick)
    });
}

fn running(epoch: &mut CommonEpoch<Mock>, state: &State) {
    advance_to_air_origin(epoch, state);
    let work = epoch
        .step(epoch.started + Duration::from_millis(11), |_, _| {
            Err::<(), _>(())
        })
        .unwrap();
    assert!(work.is_some());
    assert_eq!(epoch.phase(), EpochPhase::Running);
}

#[test]
fn controller_mutation_is_scoped_to_running_and_receives_only_a_committed_step_time() {
    let (mut epoch, state) = spawn(parts(), settings());
    let mut calls = 0;
    assert_eq!(
        epoch.with_controller(|_, _| {
            calls += 1;
        }),
        Err(EpochMutationError::Phase(EpochPhase::Starting))
    );
    assert_eq!(calls, 0);
    assert_eq!(epoch.phase(), EpochPhase::Starting);
    running(&mut epoch, &state);
    let expected = epoch.last_controller_time.unwrap();
    let observed = epoch
        .with_controller(|controller, time| {
            calls += 1;
            assert_eq!(controller.epoch(), time.radio.epoch);
            (time.radio, time.security.control, time.security.monotonic)
        })
        .unwrap();
    assert_eq!(
        observed,
        (
            expected.radio,
            ControlTime(21),
            epoch.started + Duration::from_millis(11)
        )
    );
    assert_eq!(calls, 1);
    epoch.request_stop();
    assert_eq!(
        epoch.with_controller(|_, _| {
            calls += 1;
        }),
        Err(EpochMutationError::Phase(EpochPhase::Stopping))
    );
    epoch.join(Duration::from_secs(1)).unwrap();
    assert_eq!(
        epoch.with_controller(|_, _| {
            calls += 1;
        }),
        Err(EpochMutationError::Phase(EpochPhase::Stopped))
    );
    assert_eq!(calls, 1);
}

#[test]
fn shared_stop_refuses_mutation_before_the_callback_and_keeps_the_terminal_failure() {
    let (mut epoch, state) = spawn(parts(), settings());
    running(&mut epoch, &state);
    epoch.stop.request_stop();
    let mut called = false;
    assert_eq!(
        epoch.with_controller(|_, _| {
            called = true;
        }),
        Err(EpochMutationError::Failure(EpochFailure::WorkerStopped))
    );
    assert!(!called);
    assert_eq!(epoch.phase(), EpochPhase::Failed);
    assert!(epoch.controller().is_none());
    assert_eq!(
        epoch.with_controller(|_, _| {
            called = true;
        }),
        Err(EpochMutationError::Failure(EpochFailure::WorkerStopped))
    );
    assert!(!called);
    epoch.join(Duration::from_secs(1)).unwrap();
}

#[test]
fn frame_policy_uses_the_same_air_origin_and_time_gates_with_actual_admission_owners() {
    use crate::{
        dedicated_controller::{DedicatedController, DedicatedControllerLimits},
        dedicated_rx::{ReceiveResources, SampleRuntimeLimits, ShardLimits},
        dedicated_tx::TransmitRuntimeLimits,
        rel99_admission::{AdmissionLimits, ClosurePolicy, Rel99Admission},
    };
    use std::num::NonZeroU64;
    let mut parts = parts();
    let receive = parts
        .rx
        .configure_dedicated(SampleRuntimeLimits {
            shard: ShardLimits {
                maximum_ues: count(2),
                maximum_sessions_per_step: count(2),
                resources: ReceiveResources {
                    physical_sample_bytes: 8_000_000,
                    coded_bits: 1_000_000,
                    output_bytes: 1_000_000,
                    demultiplexed_bits: 1_000_000,
                    channels: 8,
                },
            },
            history_samples: count(100_000),
            maximum_shared_bytes: 4_000_000,
            command_capacity: count(2),
            commands_per_step: count(1),
            output_slots_per_ue: count(1),
            maximum_output_bits: count(5000),
            maximum_output_bytes: 10_000,
        })
        .unwrap();
    parts
        .controller
        .attach_dedicated(DedicatedController::new(
            receive,
            DedicatedControllerLimits {
                receipt_visits: count(2),
                service_visits: count(2),
                maximum_bearer_bytes: 2_000_000,
            },
        ))
        .ok()
        .unwrap();
    let ports = parts
        .preparation
        .dedicated_ports(
            count(2),
            count(4),
            count(2_000_000),
            TransmitRuntimeLimits {
                command_capacity: count(8),
                commands_per_step: count(2),
                feedback_capacity: count(2),
                feedback_per_step: count(2),
                maximum_frame_payload_bits: count(10_000),
                maximum_inflight_retained_bits: count(2_000_000),
            },
        )
        .unwrap();
    let _feedback = ports.feedback;
    parts
        .controller
        .attach_admission(
            Rel99Admission::new(
                ports.control,
                count(2),
                AdmissionLimits {
                    visits: count(2),
                    command_results: count(2),
                    prefill_frames: count(4),
                    maximum_lead_frames: count(8),
                    health_interval_chips: NonZeroU64::new(1_000_000).unwrap(),
                },
            )
            .ok()
            .unwrap(),
        )
        .ok()
        .unwrap();
    let (mut epoch, state) = spawn(parts, settings());
    let mut policy = ClosurePolicy {
        select: |_: crate::rel99_admission::FrameOpportunity| -> umts_dsp::tfci::Tfci {
            panic!("no dedicated UE admitted")
        },
        fill: |_: &mut crate::dedicated_bearers::DownlinkFrame<'_>,
               _: &mut CommonSignalling|
         -> Result<(), crate::dedicated_bearers::FrameBuildError> {
            panic!("no dedicated UE admitted")
        },
    };
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    let waiting = epoch
        .step_with_frame_policy(
            epoch.started,
            |_, _| Err::<(), _>(()),
            |_| false,
            |_, _| Err(crate::signalling::SignallingError::Message),
            &mut policy,
        )
        .unwrap();
    assert!(waiting.is_none());
    assert!(epoch.read_clock().unwrap().chip.is_none());
    assert!(epoch.last_controller_time.is_none());
    advance_to_air_origin(&mut epoch, &state);
    let now = epoch.started + Duration::from_millis(11);
    let work = epoch
        .step_with_frame_policy(
            now,
            |_, _| Err::<(), _>(()),
            |_| false,
            |_, _| Err(crate::signalling::SignallingError::Message),
            &mut policy,
        )
        .unwrap();
    assert!(work.unwrap().admission.unwrap().is_ok());
    assert_eq!(epoch.phase(), EpochPhase::Running);
    assert_eq!(
        epoch
            .with_controller(|_, time| time.security.monotonic)
            .unwrap(),
        now
    );
    assert!(matches!(
        epoch.step_with_frame_policy(
            epoch.started,
            |_, _| Err::<(), _>(()),
            |_| false,
            |_, _| Err(crate::signalling::SignallingError::Message),
            &mut policy
        ),
        Err(EpochFailure::Time)
    ));
    assert_eq!(
        epoch.with_controller(|_, _| ()),
        Err(EpochMutationError::Failure(EpochFailure::Time))
    );
    epoch.join(Duration::from_secs(1)).unwrap();
}

#[test]
fn exact_pipeline_owners_are_checked_before_any_radio_activation() {
    for wrong_rx in [false, true] {
        let first = parts();
        let second = parts();
        let a = first.shared.clone();
        let b = second.shared.clone();
        let result = if wrong_rx {
            CommonEpoch::spawn(
                first.preparation,
                first.pump,
                second.rx,
                first.controller,
                settings(),
            )
        } else {
            CommonEpoch::spawn(
                first.preparation,
                second.pump,
                first.rx,
                first.controller,
                settings(),
            )
        };
        assert!(matches!(result, Err(EpochStartError::Owners)));
        assert!(!a.tx_active.load(Ordering::Acquire) && !a.rx_active.load(Ordering::Acquire));
        assert!(!b.tx_active.load(Ordering::Acquire) && !b.rx_active.load(Ordering::Acquire));
    }
}

#[test]
fn common_epoch_gates_air_origin_runs_owned_controller_and_closes_before_join() {
    use umts_dsp::{
        prach::{PrachCode, PreambleSignature},
        prach_message::DecodedPrachMessage,
        tfci::{Tfci, TfciDecision},
    };
    use umts_mac::{CommonHeader, FddCommonChannel, encode_common};
    use umts_types::bits::Bits;
    let mut parts = parts();
    let request = crate::signalling::tests::request(0);
    let encoded = rasn::uper::encode(request.value.as_ref()).unwrap();
    let mut mac = [0; 32];
    encode_common(
        FddCommonChannel::Rach,
        CommonHeader::CommonControl,
        Bits::new(&encoded, request.consumed_bits).unwrap(),
        &mut mac,
    )
    .unwrap();
    parts.rx.inject_transport(
        DecodedPrachMessage {
            start: at(0),
            code: PrachCode::new(7504).unwrap(),
            signature: PreambleSignature::new(13).unwrap(),
            tfci: TfciDecision {
                tfci: Tfci::new(0).unwrap(),
                normalized_score: 1.0,
                normalized_margin: None,
            },
            payload_bits: 256,
        },
        256,
        1,
        &mac,
    );
    let (mut epoch, state) = spawn(parts, settings());
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    let first = epoch.step(epoch.started, |_, _| Err::<(), _>(())).unwrap();
    assert!(first.is_none());
    assert!(epoch.read_clock().unwrap().chip.is_none());
    running(&mut epoch, &state);
    assert_eq!(epoch.controller().unwrap().radio().counters().created, 1);
    epoch.request_stop();
    assert!(epoch.controller().is_none());
    assert_eq!(epoch.phase(), EpochPhase::Stopping);
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert!(report.failure.is_none());
    assert_eq!(report.tx.preparation, WorkerExit::Stopped);
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert_eq!(report.rx.exit, WorkerExit::Stopped);
    assert!(report.tx.health.is_some() && report.rx.statistics.is_some());
    assert!(!state.tx_active.load(Ordering::Acquire) && !state.rx_active.load(Ordering::Acquire));
    assert_eq!(epoch.phase(), EpochPhase::Stopped);
    assert!(matches!(
        epoch.step(epoch.started + Duration::from_secs(1), |_, _| Err::<(), _>(
            ()
        )),
        Err(EpochFailure::Stopped)
    ));
    assert_eq!(epoch.join(Duration::ZERO).unwrap().tx, report.tx);
}

#[test]
fn frozen_device_clock_and_reversed_host_time_stop_the_whole_epoch_without_reuse() {
    for reverse in [false, true] {
        let (mut epoch, state) = spawn(parts(), settings());
        running(&mut epoch, &state);
        let now = if reverse {
            epoch.started
        } else {
            epoch.started + Duration::from_millis(111)
        };
        let work = epoch.step(now, |_, _| Err::<(), _>(()));
        assert!(
            matches!(work, Err(EpochFailure::Time)) && reverse
                || matches!(work, Err(EpochFailure::ClockStalled { .. })) && !reverse
        );
        assert!(epoch.controller().is_none());
        assert_eq!(epoch.phase(), EpochPhase::Failed);
        let report = epoch.join(Duration::from_secs(1)).unwrap();
        assert!(report.failure.is_some());
        assert!(
            !state.tx_active.load(Ordering::Acquire) && !state.rx_active.load(Ordering::Acquire)
        );
    }
}

#[test]
fn radio_clock_and_rx_loss_propagate_through_shared_stop() {
    for failure in 0..2 {
        let (mut epoch, state) = spawn(parts(), settings());
        running(&mut epoch, &state);
        if failure == 0 {
            state.bad_clock.store(true, Ordering::Release);
        } else {
            state.bad_rx.store(true, Ordering::Release);
        }
        until(|| epoch.stop.is_stopping());
        assert!(matches!(
            epoch.step(
                epoch.started + Duration::from_millis(12),
                |_, _| Err::<(), _>(())
            ),
            Err(EpochFailure::WorkerStopped)
        ));
        let report = epoch.join(Duration::from_secs(1)).unwrap();
        if failure == 1 {
            assert!(matches!(report.rx.exit, WorkerExit::Failed(_)));
        } else {
            assert!(matches!(report.tx.submission, WorkerExit::Failed(_)));
        }
        assert!(
            !state.tx_active.load(Ordering::Acquire) && !state.rx_active.load(Ordering::Acquire)
        );
    }
}

#[test]
fn async_tx_underflow_and_sequence_events_preserve_the_running_epoch() {
    let (mut epoch, state) = spawn(parts(), settings());
    running(&mut epoch, &state);
    let before = state.health_queries.load(Ordering::Acquire);
    state.sequence_errors.store(1, Ordering::Release);
    state.underflows.store(1000, Ordering::Release);
    state.tick.fetch_add(1_100_000, Ordering::Release);
    until(|| state.health_queries.load(Ordering::Acquire) > before);
    assert!(!epoch.stop.is_stopping());
    assert!(state.tx_active.load(Ordering::Acquire));
    assert!(state.rx_active.load(Ordering::Acquire));
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert_eq!(report.tx.preparation, WorkerExit::Stopped);
    assert_eq!(report.rx.exit, WorkerExit::Stopped);
    assert_eq!(report.tx.health.unwrap().underflows, 1000);
    assert_eq!(report.tx.health.unwrap().sequence_errors, 1);
    assert!(report.tx.recovery.health_warnings >= 1);
}

struct Release(Arc<State>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0.hold_activation.store(false, Ordering::Release);
        self.0.hold_write.store(false, Ordering::Release);
        self.0.hold_read.store(false, Ordering::Release);
    }
}

#[test]
fn deadline_retains_all_worker_handles_and_repeated_join_preserves_reports() {
    let parts = parts();
    parts.shared.hold_write.store(true, Ordering::Release);
    let (mut epoch, state) = spawn(parts, settings());
    let release = Release(state.clone());
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    state.tick.store(8_700_000, Ordering::Release);
    until(|| state.write_entered.load(Ordering::Acquire));
    assert!(matches!(
        epoch.join(Duration::ZERO),
        Err(EpochJoinError::Tx(JoinError::Deadline))
    ));
    assert!(epoch.controller().is_none());
    assert!(epoch.controller.is_some());
    assert!(matches!(
        epoch.poll_retirement(),
        Err(EpochRetirementError::NotJoined)
    ));
    assert_eq!(state.tx_drops.load(Ordering::Acquire), 0);
    assert_eq!(state.rx_drops.load(Ordering::Acquire), 0);
    drop(release);
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert_eq!(epoch.join(Duration::ZERO).unwrap().tx, report.tx);
}

#[test]
fn startup_origin_and_configuration_deadlines_are_explicit() {
    let first = parts();
    assert!(matches!(
        CommonEpoch::spawn(
            first.preparation,
            first.pump,
            first.rx,
            first.controller,
            EpochSettings {
                idle_wait: Duration::ZERO,
                ..settings()
            }
        ),
        Err(EpochStartError::Configuration)
    ));
    let mut config = settings();
    config.clock_stall_timeout = Duration::from_secs(5);
    let (mut epoch, state) = spawn(parts(), config);
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    assert!(matches!(
        epoch.step(epoch.started + config.startup_timeout, |_, _| Err::<(), _>(
            ()
        )),
        Err(EpochFailure::AirOriginDeadline)
    ));
    epoch.join(Duration::from_secs(1)).unwrap();
    assert!(!state.tx_active.load(Ordering::Acquire));
    let parts = parts();
    parts.shared.hold_activation.store(true, Ordering::Release);
    let (mut epoch, state) = spawn(parts, config);
    let release = Release(state.clone());
    assert!(matches!(
        epoch.step(epoch.started + config.startup_timeout, |_, _| Err::<(), _>(
            ()
        )),
        Err(EpochFailure::StartupDeadline)
    ));
    drop(release);
    epoch.join(Duration::from_secs(1)).unwrap();
}

#[test]
fn unpaced_and_invalid_health_configurations_are_rejected_before_activation() {
    let offline = parts_with_timing(TxTiming::Offline);
    let state = offline.shared.clone();
    assert!(matches!(
        CommonEpoch::spawn(
            offline.preparation,
            offline.pump,
            offline.rx,
            offline.controller,
            settings()
        ),
        Err(EpochStartError::Unpaced)
    ));
    assert!(!state.tx_active.load(Ordering::Acquire) && !state.rx_active.load(Ordering::Acquire));
    let parts = parts();
    let state = parts.shared.clone();
    let mut config = settings();
    config.health.poll_interval = Duration::ZERO;
    assert!(matches!(
        CommonEpoch::spawn(
            parts.preparation,
            parts.pump,
            parts.rx,
            parts.controller,
            config
        ),
        Err(EpochStartError::Health(_))
    ));
    assert_eq!(state.health_queries.load(Ordering::Acquire), 0);
    assert!(!state.tx_active.load(Ordering::Acquire) && !state.rx_active.load(Ordering::Acquire));
}

#[test]
fn control_time_exhaustion_fails_closed_and_drop_stops_both_radio_owners() {
    let (mut epoch, state) = spawn(
        parts(),
        EpochSettings {
            control_origin: ControlTime(u64::MAX),
            ..settings()
        },
    );
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    state.tick.store(8_700_000, Ordering::Release);
    until(|| state.writes.load(Ordering::Acquire) >= 1);
    state.tick.store(9_400_000, Ordering::Release);
    until(|| state.writes.load(Ordering::Acquire) >= 2);
    state
        .tick
        .store(epoch.clock.origin().tick, Ordering::Release);
    until(|| {
        epoch
            .clock
            .read()
            .is_ok_and(|value| value.tick == epoch.clock.origin().tick)
    });
    assert!(matches!(
        epoch.step(
            epoch.started + Duration::from_millis(11),
            |_, _| Err::<(), _>(())
        ),
        Err(EpochFailure::Time)
    ));
    drop(epoch);
    assert!(!state.tx_active.load(Ordering::Acquire) && !state.rx_active.load(Ordering::Acquire));
}

#[test]
fn requested_stop_during_an_rx_read_is_normal_even_when_ingress_closes_mid_cycle() {
    let parts = parts();
    parts.shared.hold_read.store(true, Ordering::Release);
    let (mut epoch, state) = spawn(parts, settings());
    let release = Release(state.clone());
    until(|| state.read_entered.load(Ordering::Acquire));
    epoch.request_stop();
    drop(release);
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.rx.exit, WorkerExit::Stopped);
    assert!(report.failure.is_none());
}

#[test]
fn unexpected_ingress_closure_remains_a_worker_failure() {
    let (mut epoch, _state) = spawn(parts(), settings());
    until(|| epoch.tx.is_ready() && epoch.rx.is_ready());
    epoch.controller.take();
    until(|| epoch.stop.is_stopping());
    assert!(matches!(
        epoch.step(
            epoch.started + Duration::from_millis(1),
            |_, _| Err::<(), _>(())
        ),
        Err(EpochFailure::WorkerStopped)
    ));
    let report = epoch.join(Duration::from_secs(1)).unwrap();
    assert!(matches!(report.rx.exit, WorkerExit::Failed(_)));
}

impl<T: Transport> CommonEpoch<T> {
    pub fn step<U>(
        &mut self,
        now: Instant,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
    ) -> Result<Option<ControllerStep<T::Error, U>>, EpochFailure> {
        self.step_with_control(
            now,
            other_core,
            |_| false,
            |_, _| Err(crate::signalling::SignallingError::Message),
        )
    }
    pub fn step_with_control<U>(
        &mut self,
        now: Instant,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), crate::signalling::SignallingError>,
    ) -> Result<Option<ControllerStep<T::Error, U>>, EpochFailure> {
        if self
            .controller
            .as_ref()
            .is_some_and(|controller| controller.admission().is_some())
        {
            return Err(self.fail(EpochFailure::Controller(ControllerError::Configuration)));
        }
        self.step_with_admission(
            now,
            other_core,
            upper,
            control,
            |opportunity| opportunity.idle,
            |_, _| Ok(()),
        )
    }
    pub fn step_with_admission<U>(
        &mut self,
        now: Instant,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), crate::signalling::SignallingError>,
        select: impl FnMut(crate::rel99_admission::FrameOpportunity) -> umts_dsp::tfci::Tfci,
        fill: impl FnMut(
            &mut crate::dedicated_bearers::DownlinkFrame<'_>,
            &mut CommonSignalling,
        ) -> Result<(), crate::dedicated_bearers::FrameBuildError>,
    ) -> Result<Option<ControllerStep<T::Error, U>>, EpochFailure> {
        self.step_with_frame_policy(
            now,
            other_core,
            upper,
            control,
            &mut crate::rel99_admission::ClosurePolicy { select, fill },
        )
    }

    pub fn step_with_frame_policy<U>(
        &mut self,
        now: Instant,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        upper: impl FnMut(crate::dedicated_bearers::SduDelivery<'_>) -> bool,
        control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), crate::signalling::SignallingError>,
        policy: &mut impl crate::rel99_admission::FramePolicy,
    ) -> Result<Option<ControllerStep<T::Error, U>>, EpochFailure> {
        self.step_with_duplex_policy(
            now,
            other_core,
            control,
            &mut crate::rel99_admission::CallbackDuplexPolicy { policy, upper },
        )
    }
}
