// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use num_complex::Complex32;
use sdr_hal::{RxReadResult, Schedule, TxMetadata, error::Error as HalError};
use std::{
    collections::VecDeque,
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};
use umts_iuh::{
    hnbap,
    wire::{Limits as WireLimits, Protocol},
};
use umts_runtime::worker::WorkerExit;
use umts_sctp::{Metadata, Notification, Receive, SendResult};
use umts_user_plane::packet::Datagrams;

const SAMPLE_RATE: f64 = 7_680_000.0;
const CLOCK_RATE: u64 = 1_000_000_000;
const WAIT_LIMIT: Duration = Duration::from_secs(2);
const POLL_WAIT: Duration = Duration::from_micros(100);
const MAXIMUM_MOCK_READ: Duration = Duration::from_millis(1);
const ASSOCIATION: i32 = 42;
const STREAMS: u16 = 8;
const ACCEPTED_RNC: u16 = 19;
const NANOSECONDS_PER_MILLISECOND: u64 = 1_000_000;
const FIRST_WINDOW_LEAD_NS: u64 = 200_000;
const AFTER_FIRST_START_NS: u64 = 100_000;

mod recovery;

pub(super) fn config() -> Config {
    let mut config: Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    config.radio.backend = "uhd".to_owned();
    config.radio.uhd_args = "master_clock_rate=30.72e6".to_owned();
    config.radio.uhd_rx_timestamp_offset = None;
    config.radio.bladerf_rx_sample_phase = 0;
    config.hspa.downlink.enabled = false;
    config.hspa.uplink.enabled = false;
    config.cell.advertised_power.cpich_tx_dbm = Some(10);
    config.cell.advertised_power.uplink_interference_dbm = Some(-100);
    config.runtime.minimum_submission_us = 100;
    config.runtime.maximum_submission_us = 1000;
    config.runtime.maximum_preparation_us = 1200;
    // The virtual host and device clocks advance exactly together, including the initial seek.
    config.runtime.clock.maximum_age_us = None;
    config.runtime.clock.maximum_drift_ppm = 1;
    config.runtime.startup_timeout_ms = 1000;
    config.runtime.join_timeout_ms = 1000;
    config
}

pub(super) fn epoch() -> RadioEpoch {
    RadioEpoch::new(NonZeroU64::MIN)
}

struct State {
    clock_ns: AtomicU64,
    first_scheduled_tick: AtomicU64,
    last_scheduled_tick: AtomicU64,
    handles: AtomicUsize,
    tx_active: AtomicBool,
    rx_active: AtomicBool,
    tx_activations: AtomicU64,
    rx_activations: AtomicU64,
    tx_stops: AtomicU64,
    rx_stops: AtomicU64,
    writes: AtomicU64,
    samples: AtomicU64,
    reads: AtomicU64,
    registrations: AtomicU64,
    core_acks: AtomicU64,
    core_owners: AtomicUsize,
    media_owners: AtomicUsize,
    fail_read: AtomicBool,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            clock_ns: AtomicU64::new(0),
            first_scheduled_tick: AtomicU64::new(0),
            last_scheduled_tick: AtomicU64::new(0),
            handles: AtomicUsize::new(0),
            tx_active: AtomicBool::new(false),
            rx_active: AtomicBool::new(false),
            tx_activations: AtomicU64::new(0),
            rx_activations: AtomicU64::new(0),
            tx_stops: AtomicU64::new(0),
            rx_stops: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            registrations: AtomicU64::new(0),
            core_acks: AtomicU64::new(0),
            core_owners: AtomicUsize::new(0),
            media_owners: AtomicUsize::new(0),
            fail_read: AtomicBool::new(false),
        })
    }
    fn ticks(&self, rate: u64) -> u64 {
        u64::try_from(
            u128::from(self.clock_ns.load(Ordering::Acquire)) * u128::from(rate)
                / u128::from(CLOCK_RATE),
        )
        .unwrap()
    }
    fn inactive(&self) {
        assert!(!self.tx_active.load(Ordering::Acquire));
        assert!(!self.rx_active.load(Ordering::Acquire));
        assert_eq!(self.handles.load(Ordering::Acquire), 0);
    }
}

fn prepared_with_clock(config: &Config, state: &Arc<State>) -> PreparedServing {
    let state = state.clone();
    PreparedServing::new(config, epoch())
        .unwrap()
        .with_test_clock(move || Duration::from_nanos(state.clock_ns.load(Ordering::Acquire)))
}

struct MemoryBoundDatagrams {
    local: SocketAddr,
    marker: u64,
}

impl Datagrams for MemoryBoundDatagrams {
    fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        Ok(None)
    }
    fn send(&mut self, _: &[u8], _: SocketAddr) -> io::Result<bool> {
        Ok(true)
    }
}

impl BoundDatagrams for MemoryBoundDatagrams {
    fn local_address(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
}

fn packet_transport(config: &Config) -> MemoryBoundDatagrams {
    MemoryBoundDatagrams {
        local: media::MediaPlan::new(config).unwrap().bind_address(),
        marker: 73,
    }
}

#[derive(Clone, Copy)]
struct Geometry {
    tx_rate: f64,
    rx_rate: f64,
    tx_clock: u64,
    rx_clock: u64,
    timed: bool,
}
impl Default for Geometry {
    fn default() -> Self {
        Self {
            tx_rate: SAMPLE_RATE,
            rx_rate: SAMPLE_RATE,
            tx_clock: CLOCK_RATE,
            rx_clock: CLOCK_RATE,
            timed: true,
        }
    }
}
struct Tx {
    state: Arc<State>,
    geometry: Geometry,
}
impl Drop for Tx {
    fn drop(&mut self) {
        self.state.handles.fetch_sub(1, Ordering::AcqRel);
    }
}
impl RadioTx for Tx {
    fn supports_timed_tx(&self) -> bool {
        self.geometry.timed
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        self.geometry.tx_rate
    }
    fn tick_rate(&self) -> u64 {
        self.geometry.tx_clock
    }
    fn get_hardware_time(&self) -> Result<u64, HalError> {
        Ok(self.state.ticks(self.geometry.tx_clock))
    }
    fn transmit(&mut self, _: &[Complex32]) -> Result<(), HalError> {
        panic!("serving must use timed TX")
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, HalError> {
        assert!(self.state.tx_active.load(Ordering::Acquire));
        assert!(matches!(metadata.schedule, Schedule::At(_)));
        let Schedule::At(scheduled) = metadata.schedule else {
            unreachable!("validated timed TX")
        };
        assert!(!samples.is_empty());
        assert!(
            samples
                .iter()
                .all(|sample| sample.re.is_finite() && sample.im.is_finite())
        );
        let _ = self.state.first_scheduled_tick.compare_exchange(
            0,
            scheduled.ticks(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.state
            .last_scheduled_tick
            .store(scheduled.ticks(), Ordering::Release);
        self.state.writes.fetch_add(1, Ordering::AcqRel);
        self.state
            .samples
            .fetch_add(samples.len() as u64, Ordering::AcqRel);
        Ok(samples.len())
    }
    fn enable_transmit(&mut self, active: bool) -> Result<(), HalError> {
        self.state.tx_active.store(active, Ordering::Release);
        if active {
            self.state.tx_activations.fetch_add(1, Ordering::AcqRel);
        } else {
            self.state.tx_stops.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
}
struct Rx {
    state: Arc<State>,
    geometry: Geometry,
}
impl Drop for Rx {
    fn drop(&mut self) {
        self.state.handles.fetch_sub(1, Ordering::AcqRel);
    }
}
impl RadioRx for Rx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.geometry.rx_rate
    }
    fn tick_rate(&self) -> u64 {
        self.geometry.rx_clock
    }
    fn get_hardware_time(&self) -> Result<u64, HalError> {
        Ok(self.state.ticks(self.geometry.rx_clock))
    }
    fn rx_read(&mut self, _: &mut [Complex32], timeout: i64) -> Result<RxReadResult, HalError> {
        assert!(self.state.rx_active.load(Ordering::Acquire));
        assert!(timeout > 0);
        self.state.reads.fetch_add(1, Ordering::AcqRel);
        if self.state.fail_read.load(Ordering::Acquire) {
            return Err(HalError::new(
                sdr_hal::error::ErrorKind::Backend,
                "injected serving RX failure",
            ));
        }
        thread::sleep(Duration::from_micros(timeout as u64).min(MAXIMUM_MOCK_READ));
        Ok(RxReadResult {
            samples_read: 0,
            time_ticks: self.state.ticks(self.geometry.rx_clock),
            overflow: false,
        })
    }
    fn rx_activate(&mut self, _: Option<u64>) -> Result<(), HalError> {
        self.state.rx_active.store(true, Ordering::Release);
        self.state.rx_activations.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), HalError> {
        self.state.rx_active.store(false, Ordering::Release);
        self.state.rx_stops.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}
fn handles(state: &Arc<State>, geometry: Geometry) -> (Box<dyn RadioTx>, Box<dyn RadioRx>) {
    state.handles.fetch_add(2, Ordering::AcqRel);
    (
        Box::new(Tx {
            state: state.clone(),
            geometry,
        }),
        Box::new(Rx {
            state: state.clone(),
            geometry,
        }),
    )
}

enum Input {
    Established,
    Packet(Vec<u8>),
}
struct Core {
    state: Arc<State>,
    connected: bool,
    reject: bool,
    input: VecDeque<Input>,
    held: Option<Input>,
    registered: bool,
}
impl Core {
    fn new(state: &Arc<State>, reject: bool) -> Self {
        state.core_owners.fetch_add(1, Ordering::AcqRel);
        Self {
            state: state.clone(),
            connected: false,
            reject,
            input: VecDeque::new(),
            held: None,
            registered: false,
        }
    }
}
impl Drop for Core {
    fn drop(&mut self) {
        self.state.core_owners.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Transport for Core {
    type Error = &'static str;
    fn complete_connect(&mut self) -> Result<bool, Self::Error> {
        if !self.connected {
            self.connected = true;
            self.input.push_back(Input::Established);
        }
        Ok(true)
    }
    fn receive_once(&mut self) -> Result<Receive<'_>, Self::Error> {
        if self.held.is_none() {
            self.held = self.input.pop_front();
        }
        Ok(match &self.held {
            None => Receive::Idle,
            Some(Input::Established) => Receive::Notification(Notification::Established {
                inbound_streams: STREAMS,
                outbound_streams: STREAMS,
                association: ASSOCIATION,
            }),
            Some(Input::Packet(bytes)) => Receive::Record {
                metadata: Metadata {
                    stream: 0,
                    sequence: 0,
                    ppid: Protocol::Hnbap.ppid().unwrap(),
                    association: ASSOCIATION,
                },
                bytes,
            },
        })
    }
    fn acknowledge_receive(&mut self) -> Result<(), Self::Error> {
        assert!(self.held.take().is_some());
        self.state.core_acks.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn send(&mut self, _: u16, ppid: u32, bytes: &[u8]) -> Result<SendResult, Self::Error> {
        assert_eq!(ppid, Protocol::Hnbap.ppid().unwrap());
        let message = hnbap::Message::decode(bytes, WireLimits::default())
            .unwrap()
            .message;
        // TS 25.469 8.3.1: shutdown writes HNB DE-REGISTER once the HNB was registered.
        if matches!(message, hnbap::Message::HnbDeregistered { .. }) {
            assert!(self.registered);
            return Ok(SendResult::Sent);
        }
        assert!(matches!(message, hnbap::Message::HnbRegister(_)));
        assert!(!self.registered);
        self.registered = true;
        self.state.registrations.fetch_add(1, Ordering::AcqRel);
        let reply = if self.reject {
            hnbap::Message::HnbRejected {
                cause: hnbap::Cause::Radio(hnbap::RadioCause::UnauthorizedHnb),
                backoff_seconds: None,
            }
        } else {
            hnbap::Message::HnbAccepted {
                rnc: ACCEPTED_RNC,
                mux_port: None,
            }
        };
        self.input
            .push_back(Input::Packet(reply.encode(WireLimits::default()).unwrap()));
        Ok(SendResult::Sent)
    }
}

#[test]
fn startup_cancellation_never_registers_or_activates_rf() {
    let state = State::new();
    let prepared = prepared_with_clock(&config(), &state);
    let (tx, rx) = handles(&state, Geometry::default());
    let result = prepared.spawn_with_packet_transport_and_cancellation(
        tx,
        rx,
        Core::new(&state, false),
        packet_transport(&config()),
        || true,
    );
    let error = match result {
        Ok(_) => panic!("canceled startup created RF workers"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("canceled"));
    assert_eq!(state.registrations.load(Ordering::Acquire), 0);
    assert_eq!(state.tx_activations.load(Ordering::Acquire), 0);
    assert_eq!(state.rx_activations.load(Ordering::Acquire), 0);
    state.inactive();
}

#[test]
fn pure_preparation_validates_without_opening_a_radio_or_connecting_core() {
    let mut config = config();
    config.radio.uhd_args = "type=unavailable-test-device,master_clock_rate=30.72e6".to_owned();
    config.core.hnbgw.address = "203.0.113.42".to_owned();
    config.core.gtpu.bind_ip = "203.0.113.43".to_owned();
    config.core.gtpu.advertise_ip = Some("203.0.113.43".to_owned());
    let prepared = PreparedServing::new(&config, epoch()).unwrap();
    assert_eq!(
        prepared.description(),
        Description {
            epoch: epoch(),
            downlink_hz: 877_000_000,
            uplink_hz: 832_000_000,
            core: SocketAddr::new(
                config.core.hnbgw.address.parse().unwrap(),
                config.core.hnbgw.port
            ),
        }
    );
}

#[test]
fn original_factory_runs_actual_bounded_workers_and_registered_core_then_joins_all_handles() {
    factory_runs_with_geometry(Geometry::default());
}

#[test]
fn fractional_applied_rates_cross_the_complete_serving_factory_without_losing_geometry() {
    for rate in [7_680_000.005_983_839, 7_679_999.994_016_161] {
        factory_runs_with_geometry(Geometry {
            tx_rate: rate,
            rx_rate: rate,
            ..Geometry::default()
        });
    }
}

#[test]
fn four_sample_tx_and_two_sample_rx_share_the_air_clock_and_retire_workers() {
    for (tx_rate, rx_rate) in [
        (15_360_000.0, 7_680_000.0),
        (15_360_000.011_967_678, 7_680_000.005_983_839),
    ] {
        let mut config = config();
        config.radio.tx_samples_per_chip = 4;
        factory_runs_with_config(
            Geometry {
                tx_rate,
                rx_rate,
                ..Geometry::default()
            },
            config,
        );
    }
}

fn factory_runs_with_geometry(geometry: Geometry) {
    factory_runs_with_config(geometry, config());
}

#[test]
fn hsdpa_factory_attaches_shared_channels_and_retires_actual_workers() {
    factory_runs_with_config(Geometry::default(), hsdpa_config());
}

#[test]
fn hsupa_factory_attaches_shared_channels_with_either_downlink_and_retires_workers() {
    for hsdpa in [false, true] {
        let mut config = if hsdpa { hsdpa_config() } else { config() };
        config.hspa.uplink.enabled = true;
        config.runtime.maximum_ues = 2;
        config.hspa.uplink.resources = super::hsupa::tests::config().hspa.uplink.resources;
        factory_runs_with_config(Geometry::default(), config);
    }
}

fn hsdpa_config() -> Config {
    let mut config = config();
    config.hspa.downlink.enabled = true;
    config.hspa.downlink.resources.scheduling = Some(
        serde_yaml::from_str(
            r#"
maximum_formats: 1024
maximum_coded_bits: 100000
maximum_plan_bytes: 16000000
maximum_buffer_bytes: 2000000
maximum_encoder_bytes: 1000000
maximum_ue_bytes: 16000000
maximum_cell_bytes: 32000000
maximum_cqi_age_chips: 38400
selection_budget: 1024
control_amplitude: 0.1
acknowledgement: &evidence
  minimum_correlation: 0.5
  minimum_margin: 0.1
  minimum_pilot_fraction: 0.5
  maximum_relative_disturbance: 1.0
  minimum_relative_soft_power: 0.01
cqi: *evidence
"#,
        )
        .unwrap(),
    );
    config
}

fn factory_runs_with_config(geometry: Geometry, config: Config) {
    let state = State::new();
    let origin = u64::from(config.runtime.startup_lead_ms) * NANOSECONDS_PER_MILLISECOND;
    let prepared = prepared_with_clock(&config, &state);
    let (tx, rx) = handles(&state, geometry);
    let mut runtime = prepared
        .spawn_with_packet_transport(tx, rx, Core::new(&state, false), packet_transport(&config))
        .unwrap();
    // RF starts before the HNB-GW registration, which the controller drives once running.
    assert_eq!(state.registrations.load(Ordering::Acquire), 0);
    let began = Instant::now();
    while state.tx_activations.load(Ordering::Acquire) == 0
        || state.rx_activations.load(Ordering::Acquire) == 0
        || state.reads.load(Ordering::Acquire) == 0
    {
        if began.elapsed() >= WAIT_LIMIT {
            runtime.request_stop();
            let report = runtime.join();
            panic!("actual HAL activation deadline: {report:?}");
        }
        thread::sleep(POLL_WAIT);
    }
    // This fixture controls device time and does not qualify real-time scheduling.
    state
        .clock_ns
        .store(origin - FIRST_WINDOW_LEAD_NS, Ordering::Release);
    while state.writes.load(Ordering::Acquire) == 0 {
        if began.elapsed() >= WAIT_LIMIT {
            runtime.request_stop();
            let report = runtime.join();
            panic!("actual HAL write deadline: {report:?}");
        }
        thread::sleep(POLL_WAIT);
    }
    let first = state.first_scheduled_tick.load(Ordering::Acquire);
    assert_eq!(first, origin);
    state
        .clock_ns
        .store(first + AFTER_FIRST_START_NS, Ordering::Release);
    while runtime.phase() != EpochPhase::Running {
        let result = runtime.step(Instant::now());
        if result.is_err() || began.elapsed() >= WAIT_LIMIT {
            runtime.request_stop();
            let report = runtime.join();
            panic!(
                "serving readiness failed: step={:?}, phase={:?}, join={:?}",
                result.err(),
                runtime.phase(),
                report
            );
        }
        thread::sleep(POLL_WAIT);
    }
    while runtime.core_phase() != Some(umts_core::Phase::Serving) {
        runtime.step(Instant::now()).unwrap();
        assert!(began.elapsed() < WAIT_LIMIT, "HNB-GW registration deadline");
        thread::sleep(POLL_WAIT);
    }
    assert_eq!(runtime.rnc(), Some(ACCEPTED_RNC));
    let controller = runtime.epoch.controller().unwrap();
    assert!(controller.admission().is_some());
    assert_eq!(
        controller.admission().unwrap().capacity(),
        config.runtime.maximum_ues as usize
    );
    assert_eq!(
        runtime.feedback_lead_chips(),
        runtime
            .air_origin()
            .chip_rate
            .ticks_to_samples_ceil(
                u64::from(config.runtime.maximum_preparation_us),
                NonZeroU64::new(1_000_000).unwrap()
            )
            .unwrap()
    );
    runtime.step(Instant::now()).unwrap();
    assert_eq!(runtime.counters().failed_admission, 0);
    assert_eq!(runtime.counters().failed_dedicated, 0);
    assert_eq!(runtime.registered_ues(), 0);
    let management = runtime.management_snapshot();
    assert_eq!(
        management.status.identity.as_ref().unwrap().epoch,
        epoch().get()
    );
    assert_eq!(
        management.status.phase,
        i32::from(umts_management::proto::Phase::Running)
    );
    assert!(management.status.core_connected);
    assert!(management.status.radio_ready);
    assert!(management.status.bearer_snapshot_complete);
    assert!(management.ues.is_empty());
    assert_eq!(
        management.configuration.device_tick_rate_hz,
        Some(CLOCK_RATE)
    );
    assert_eq!(runtime.traffic.packets_mut().io_mut().marker, 73);
    runtime.traffic.packets_mut().io_mut().marker = 91;
    assert_eq!(runtime.traffic.packets_mut().io_mut().marker, 91);
    assert_eq!(runtime.traffic.voices_mut().reserved_bytes(), 0);
    assert_eq!(
        runtime.radio_allocations(),
        usize::from(config.hspa.downlink.enabled) + usize::from(config.hspa.uplink.enabled)
    );
    assert_eq!(
        runtime.hsdpa_codes().is_some(),
        config.hspa.downlink.enabled
    );
    assert!(runtime.take_ready_assignment().unwrap().is_none());
    assert!(runtime.service_packet_network().unwrap().is_ok());
    assert_eq!(runtime.bearers.network_failures, 0);
    assert!(!runtime.has_pending_assignment());
    assert!(runtime.take_assignment().unwrap().is_none());
    assert_eq!(state.registrations.load(Ordering::Acquire), 1);
    assert!(state.core_acks.load(Ordering::Acquire) >= 2);
    assert_eq!(state.tx_activations.load(Ordering::Acquire), 1);
    assert_eq!(state.rx_activations.load(Ordering::Acquire), 1);
    assert!(state.samples.load(Ordering::Acquire) > 0);
    assert!(state.reads.load(Ordering::Acquire) > 0);
    runtime.request_stop();
    assert_eq!(runtime.phase(), EpochPhase::Stopping);
    let report = runtime.join().unwrap();
    assert!(report.failure.is_none());
    assert_eq!(report.tx.preparation, WorkerExit::Stopped);
    assert_eq!(report.tx.submission, WorkerExit::Stopped);
    assert_eq!(report.rx.exit, WorkerExit::Stopped);
    assert_eq!(runtime.phase(), EpochPhase::Stopped);
    assert!(matches!(
        runtime.take_ready_assignment(),
        Err(AssignmentReadinessError::Epoch(_))
    ));
    assert!(runtime.service_packet_network().is_err());
    assert!(state.tx_stops.load(Ordering::Acquire) > 0);
    assert!(state.rx_stops.load(Ordering::Acquire) > 0);
    assert!(runtime.step(Instant::now()).is_err());
    assert_eq!(state.handles.load(Ordering::Acquire), 2);
    let retained = runtime
        .try_finish_shutdown()
        .expect_err("a joined timed tail has not elapsed");
    let crate::epoch::EpochRetirementError::TailPending(
        crate::radio_io::TerminalTransmitDrain::Waiting { tail, .. },
    ) = &retained.error
    else {
        panic!("retirement must retain the actual scheduled TX tail");
    };
    assert_eq!(state.handles.load(Ordering::Acquire), 2);
    let tail_ns = u64::try_from(
        (u128::from(tail.tick()) * u128::from(CLOCK_RATE)).div_ceil(u128::from(geometry.tx_clock)),
    )
    .unwrap();
    state.clock_ns.store(tail_ns, Ordering::Release);
    let report = (*retained.into_runtime()).try_finish_shutdown().unwrap();
    assert_eq!(
        report.report().tx.shutdown,
        crate::radio_io::HalStopOutcome::Succeeded
    );
    assert_eq!(
        report.report().rx.shutdown,
        crate::radio_io::HalStopOutcome::Succeeded
    );
    state.inactive();
}

#[test]
fn dedicated_budget_errors_are_rejected_before_core_registration_or_rf_activation() {
    let state = State::new();
    for invalid in 0..3 {
        let mut config = config();
        let limits = &mut config.receiver.dedicated;
        match invalid {
            0 => limits.maximum_shared_bytes = 1,
            1 => limits.maximum_output_bytes = 1,
            _ => limits.maximum_transmit_bits = 1,
        }
        assert!(PreparedServing::new(&config, epoch()).is_err());
    }
    state.inactive();
    assert_eq!(state.registrations.load(Ordering::Acquire), 0);
}

#[test]
fn invalid_hal_rate_clock_or_timing_support_rejects_before_stream_activation() {
    let valid = Geometry::default();
    for geometry in [
        Geometry {
            tx_rate: SAMPLE_RATE / 2.0,
            ..valid
        },
        Geometry {
            rx_rate: SAMPLE_RATE / 2.0,
            ..valid
        },
        Geometry {
            rx_clock: CLOCK_RATE / 2,
            ..valid
        },
        Geometry {
            tx_clock: 0,
            ..valid
        },
        Geometry {
            timed: false,
            ..valid
        },
    ] {
        let state = State::new();
        let prepared = prepared_with_clock(&config(), &state);
        let (tx, rx) = handles(&state, geometry);
        assert!(
            prepared
                .spawn_with_packet_transport(
                    tx,
                    rx,
                    Core::new(&state, false),
                    packet_transport(&config())
                )
                .is_err()
        );
        assert_eq!(state.tx_activations.load(Ordering::Acquire), 0);
        assert_eq!(state.rx_activations.load(Ordering::Acquire), 0);
        state.inactive();
    }
}

#[test]
fn radio_startup_failures_are_not_retried_as_core_failures() {
    let state = State::new();
    let prepared = prepared_with_clock(&config(), &state);
    let (tx, rx) = handles(
        &state,
        Geometry {
            timed: false,
            ..Geometry::default()
        },
    );
    let error = match prepared.spawn_with_packet_transport(
        tx,
        rx,
        Core::new(&state, false),
        packet_transport(&config()),
    ) {
        Ok(_) => panic!("startup must fail"),
        Err(error) => error,
    };
    assert!(!core_unavailable(&error), "{error:#}");
    assert_eq!(state.registrations.load(Ordering::Acquire), 0);
    assert_eq!(state.tx_activations.load(Ordering::Acquire), 0);
    state.inactive();
}

#[test]
fn wrong_bound_packet_endpoint_rejects_before_registration_or_radio_activation() {
    let state = State::new();
    let config = config();
    let prepared = prepared_with_clock(&config, &state);
    let mut datagrams = packet_transport(&config);
    datagrams
        .local
        .set_port(datagrams.local.port().checked_add(1).unwrap());
    let (tx, rx) = handles(&state, Geometry::default());
    assert!(
        prepared
            .spawn_with_packet_transport(tx, rx, Core::new(&state, false), datagrams)
            .is_err()
    );
    assert_eq!(state.registrations.load(Ordering::Acquire), 0);
    assert_eq!(state.tx_activations.load(Ordering::Acquire), 0);
    assert_eq!(state.rx_activations.load(Ordering::Acquire), 0);
    state.inactive();
}

#[test]
fn unsupported_or_incomplete_serving_configuration_fails_closed() {
    for variant in 0..5 {
        let mut config = config();
        match variant {
            0 => config.cell.advertised_power.cpich_tx_dbm = None,
            1 => config.cell.advertised_power.uplink_interference_dbm = None,
            2 => config.hspa.downlink.enabled = true,
            3 => {
                config.hspa.downlink.enabled = true;
                config.hspa.uplink.enabled = true;
            }
            4 => config.radio.uarfcn_ul += 1,
            _ => unreachable!(),
        }
        assert!(PreparedServing::new(&config, epoch()).is_err());
    }
}
