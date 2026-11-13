// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{controller::ControllerError, radio_io::TerminalTransmitDrain};

struct TrackedMedia {
    inner: MemoryBoundDatagrams,
    state: Arc<State>,
}
impl TrackedMedia {
    fn new(config: &Config, state: &Arc<State>) -> Self {
        assert_eq!(state.media_owners.fetch_add(1, Ordering::AcqRel), 0);
        Self {
            inner: packet_transport(config),
            state: state.clone(),
        }
    }
}
impl Datagrams for TrackedMedia {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.inner.receive(output)
    }
    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool> {
        self.inner.send(bytes, peer)
    }
}
impl BoundDatagrams for TrackedMedia {
    fn local_address(&self) -> io::Result<SocketAddr> {
        self.inner.local_address()
    }
}
impl Drop for TrackedMedia {
    fn drop(&mut self) {
        assert_eq!(self.state.media_owners.fetch_sub(1, Ordering::AcqRel), 1);
    }
}

fn vacant(state: &State) {
    state.inactive();
    assert_eq!(state.core_owners.load(Ordering::Acquire), 0);
    assert_eq!(state.media_owners.load(Ordering::Acquire), 0);
}

fn run(
    prepared: PreparedServing,
    config: &Config,
    state: &Arc<State>,
) -> ServingRuntime<Core, TrackedMedia> {
    vacant(state);
    let before_writes = state.writes.load(Ordering::Acquire);
    let before_activations = state.tx_activations.load(Ordering::Acquire);
    let origin = state.clock_ns.load(Ordering::Acquire)
        + u64::from(config.runtime.startup_lead_ms) * NANOSECONDS_PER_MILLISECOND;
    let clock = state.clone();
    let prepared = prepared
        .with_test_clock(move || Duration::from_nanos(clock.clock_ns.load(Ordering::Acquire)));
    let (tx, rx) = handles(state, Geometry::default());
    let mut runtime = prepared
        .spawn_with_packet_transport(
            tx,
            rx,
            Core::new(state, false),
            TrackedMedia::new(config, state),
        )
        .unwrap();
    let began = Instant::now();
    while state.tx_activations.load(Ordering::Acquire) == before_activations
        || state.rx_activations.load(Ordering::Acquire) == before_activations
    {
        assert!(began.elapsed() < WAIT_LIMIT, "real HAL activation deadline");
        thread::sleep(POLL_WAIT);
    }
    state
        .clock_ns
        .store(origin - FIRST_WINDOW_LEAD_NS, Ordering::Release);
    while state.writes.load(Ordering::Acquire) == before_writes {
        assert!(
            began.elapsed() < WAIT_LIMIT,
            "real HAL timed-write deadline"
        );
        thread::sleep(POLL_WAIT);
    }
    let actual = state.last_scheduled_tick.load(Ordering::Acquire);
    assert_eq!(actual, origin);
    state
        .clock_ns
        .store(actual + AFTER_FIRST_START_NS, Ordering::Release);
    while runtime.phase() != EpochPhase::Running {
        runtime.step(Instant::now()).unwrap();
        assert!(
            began.elapsed() < WAIT_LIMIT,
            "real serving readiness deadline"
        );
        thread::sleep(POLL_WAIT);
    }
    while runtime.core_phase() != Some(umts_core::Phase::Serving) {
        runtime.step(Instant::now()).unwrap();
        assert!(began.elapsed() < WAIT_LIMIT, "HNB-GW registration deadline");
        thread::sleep(POLL_WAIT);
    }
    assert_eq!(state.handles.load(Ordering::Acquire), 2);
    assert_eq!(state.core_owners.load(Ordering::Acquire), 1);
    assert_eq!(state.media_owners.load(Ordering::Acquire), 1);
    runtime
}

fn retire(
    mut runtime: ServingRuntime<Core, TrackedMedia>,
    state: &State,
    failed: bool,
) -> RetiredServing {
    runtime.request_stop();
    let joined = runtime.join().unwrap();
    assert_eq!(joined.tx.submission, WorkerExit::Stopped);
    if failed {
        assert!(
            matches!(&joined.rx.exit, WorkerExit::Failed(error) if error.contains("injected serving RX failure"))
        );
        assert!(joined.failure.is_some());
    } else {
        assert_eq!(joined.rx.exit, WorkerExit::Stopped);
        assert!(joined.failure.is_none());
    }
    assert_eq!(state.handles.load(Ordering::Acquire), 2);
    assert_eq!(state.media_owners.load(Ordering::Acquire), 1);
    let retained = runtime
        .try_finish_shutdown()
        .expect_err("actual queued IQ has not elapsed");
    let crate::epoch::EpochRetirementError::TailPending(TerminalTransmitDrain::Waiting {
        tail,
        observed_tick,
    }) = &retained.error
    else {
        panic!("joined epoch must retain its actual future terminal tail");
    };
    assert!(tail.tick() > *observed_tick);
    assert_eq!(state.handles.load(Ordering::Acquire), 2);
    assert_eq!(state.media_owners.load(Ordering::Acquire), 1);
    state.clock_ns.store(tail.tick(), Ordering::Release);
    let retired = (*retained.into_runtime()).try_finish_shutdown().unwrap();
    assert_eq!(
        retired.report().tx.shutdown,
        crate::radio_io::HalStopOutcome::Succeeded
    );
    assert_eq!(
        retired.report().rx.shutdown,
        crate::radio_io::HalStopOutcome::Succeeded
    );
    vacant(state);
    retired
}

#[test]
fn actual_hal_failure_retires_and_restarts_without_owner_overlap_and_rejects_old_time() {
    let state = State::new();
    let mut config = config();
    config.runtime.recovery.maximum_restarts = 1;
    let prepared = PreparedServing::new(&config, epoch()).unwrap();
    let mut old = run(prepared, &config, &state);
    let old_time = old.epoch.with_controller(|_, time| time).unwrap();
    let old_epoch = old_time.radio.epoch;
    state.fail_read.store(true, Ordering::Release);
    let began = Instant::now();
    loop {
        if old.step(Instant::now()).is_err() {
            break;
        }
        assert!(
            began.elapsed() < WAIT_LIMIT,
            "actual HAL failure did not stop its epoch"
        );
        thread::sleep(POLL_WAIT);
    }
    assert_eq!(old.phase(), EpochPhase::Failed);
    let retired = retire(old, &state, true);
    state.fail_read.store(false, Ordering::Release);
    assert_eq!(retired.report().epoch, old_epoch);
    let old_device_time = state.clock_ns.load(Ordering::Acquire);
    let mut budget = RecoveryBudget::new(&config).unwrap();
    let scheduled = Instant::now();
    let pending = match budget.schedule(retired, scheduled) {
        Ok(pending) => pending,
        Err(_) => panic!("one configured restart must accept actual retirement"),
    };
    let ready = pending.ready_at();
    assert_eq!(pending.previous_epoch(), old_epoch);
    assert_eq!(pending.attempt(), 1);
    assert_eq!(budget.attempts(), 1);
    assert_eq!(
        ready,
        scheduled + Duration::from_millis(u64::from(config.runtime.recovery.backoff_ms))
    );
    let rejected = match pending.prepare(&config, ready - Duration::from_nanos(1)) {
        Ok(_) => panic!("restart must preserve the configured delay"),
        Err(rejected) => rejected,
    };
    let pending = rejected.into_pending();
    assert_eq!(pending.ready_at(), ready);
    assert_eq!(pending.previous_epoch(), old_epoch);
    assert_eq!(budget.attempts(), 1);
    vacant(&state);
    let mut invalid = config.clone();
    invalid.hspa.downlink.enabled = true;
    let rejected = match pending.prepare(&invalid, ready) {
        Ok(_) => panic!("unsupported fresh configuration must retain restart ownership"),
        Err(rejected) => rejected,
    };
    let pending = rejected.into_pending();
    assert_eq!(pending.ready_at(), ready);
    assert_eq!(pending.previous_epoch(), old_epoch);
    assert_eq!(budget.attempts(), 1);
    vacant(&state);
    let prepared = match pending.prepare(&config, ready) {
        Ok(prepared) => prepared,
        Err(error) => panic!("ready validated restart should prepare a fresh epoch: {error:?}"),
    };
    let new_epoch = prepared.description().epoch;
    assert_ne!(new_epoch, old_epoch);
    assert_eq!(state.clock_ns.load(Ordering::Acquire), old_device_time);
    vacant(&state);
    let mut fresh = run(prepared, &config, &state);
    assert_eq!(fresh.epoch.epoch(), new_epoch);
    let traffic = &mut fresh.traffic;
    let rejected = fresh
        .epoch
        .with_controller(|controller, _| {
            controller.step_with_duplex_policy(
                old_time,
                |_, _| -> Result<(), ()> {
                    panic!("stale epoch time must be rejected before serving work")
                },
                |_, _| panic!("stale epoch time must not enter control work"),
                traffic,
            )
        })
        .unwrap();
    assert!(matches!(rejected, Err(ControllerError::Epoch)));
    fresh.step(Instant::now()).unwrap();
    assert_eq!(fresh.phase(), EpochPhase::Running);
    assert_eq!(fresh.core_phase(), Some(umts_core::Phase::Serving));
    assert_eq!(state.registrations.load(Ordering::Acquire), 2);
    assert_eq!(state.tx_activations.load(Ordering::Acquire), 2);
    assert_eq!(state.rx_activations.load(Ordering::Acquire), 2);
    let retired = retire(fresh, &state, false);
    assert_eq!(retired.report().epoch, new_epoch);
    let retained = match budget.schedule(retired, ready) {
        Ok(_) => panic!("configured lifetime restart budget must not reset after success"),
        Err(retired) => retired,
    };
    assert_eq!(retained.report().epoch, new_epoch);
    assert_eq!(budget.attempts(), 1);
    vacant(&state);
}
