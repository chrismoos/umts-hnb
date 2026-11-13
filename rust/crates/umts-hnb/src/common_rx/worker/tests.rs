// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_rx::tests::pipeline,
    radio_io::{RxSettings, TimedRx},
    test_support::count,
};
use num_complex::Complex32;
use sdr_hal::{Radio, RadioRx, RxReadResult, error::Error, memory::MemoryRadio};
use std::{sync::atomic::AtomicUsize, time::Instant};
use umts_dsp::pulse_shaping::SamplesPerChip;

#[derive(Clone, Copy)]
enum Fault {
    Read,
    ReadAndStop,
    IdleAndStop,
    Panic,
    Wait,
}

#[derive(Default)]
struct Probe {
    activations: AtomicUsize,
    entered: AtomicBool,
    release: AtomicBool,
    drops: AtomicUsize,
    stops: AtomicUsize,
}

struct FaultRx {
    inner: Box<dyn RadioRx>,
    fault: Fault,
    probe: Arc<Probe>,
}
impl RadioRx for FaultRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        self.inner.rx_sample_rate_hz()
    }
    fn tick_rate(&self) -> u64 {
        self.inner.tick_rate()
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.inner.get_hardware_time()
    }
    fn rx_activate(&mut self, tick: Option<u64>) -> Result<(), Error> {
        self.probe.activations.fetch_add(1, Ordering::AcqRel);
        self.inner.rx_activate(tick)
    }
    fn rx_read(&mut self, _: &mut [Complex32], _: i64) -> Result<RxReadResult, Error> {
        self.probe.entered.store(true, Ordering::Release);
        match self.fault {
            Fault::IdleAndStop => {
                return Ok(RxReadResult {
                    samples_read: 0,
                    time_ticks: self.inner.get_hardware_time()?,
                    overflow: false,
                });
            }
            Fault::Panic => panic!("RX read panic"),
            Fault::Wait => {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !self.probe.release.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "bounded test read deadline");
                    thread::yield_now();
                }
            }
            Fault::Read | Fault::ReadAndStop => {}
        }
        Err("RX read failure".into())
    }
    fn stop(&mut self) -> Result<(), Error> {
        self.probe.stops.fetch_add(1, Ordering::AcqRel);
        if matches!(self.fault, Fault::ReadAndStop | Fault::IdleAndStop) {
            return Err("RX stop failure".into());
        }
        self.inner.stop()
    }
}

#[test]
fn staged_rx_cancels_before_hal_activation_and_returns_original_owner_once() {
    let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
    let probe = fault(&mut runtime, Fault::Read);
    let gate = StartGate::default();
    let mut worker = RxWorker::spawn_staged(
        runtime,
        StopToken::default(),
        None,
        Duration::from_micros(100),
        gate.clone(),
    )
    .unwrap();
    assert!(matches!(
        worker.join(Duration::ZERO),
        Err(JoinError::Deadline)
    ));
    assert_eq!(probe.activations.load(Ordering::Acquire), 0);
    assert!(!probe.entered.load(Ordering::Acquire));
    worker.request_stop();
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.exit, WorkerExit::Stopped);
    assert_eq!(report.shutdown, HalStopOutcome::NotAttempted);
    gate.release();
    assert_eq!(probe.activations.load(Ordering::Acquire), 0);
    assert_eq!(probe.stops.load(Ordering::Acquire), 0);
    let retired = worker.take_retired_runtime().unwrap();
    assert!(worker.take_retired_runtime().is_none());
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    drop(retired);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn staged_rx_activates_only_after_the_shared_gate_is_released() {
    let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
    let probe = fault(&mut runtime, Fault::Read);
    let gate = StartGate::default();
    let mut worker = RxWorker::spawn_staged(
        runtime,
        StopToken::default(),
        None,
        Duration::from_micros(100),
        gate.clone(),
    )
    .unwrap();
    assert!(matches!(
        worker.join(Duration::ZERO),
        Err(JoinError::Deadline)
    ));
    assert_eq!(probe.activations.load(Ordering::Acquire), 0);
    gate.release();
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert!(matches!(report.exit, WorkerExit::Failed(_)));
    assert_eq!(report.shutdown, HalStopOutcome::Succeeded);
    assert_eq!(probe.activations.load(Ordering::Acquire), 1);
    assert!(probe.entered.load(Ordering::Acquire));
    assert_eq!(probe.stops.load(Ordering::Acquire), 1);
    assert!(worker.take_retired_runtime().is_some());
}
impl Drop for FaultRx {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::AcqRel);
    }
}

fn fault(runtime: &mut CommonRx, fault: Fault) -> Arc<Probe> {
    let (mut radio, _peer) = MemoryRadio::new(1);
    radio
        .setup_rx(0, "", 7_680_000.0, 5_000_000.0, None)
        .unwrap();
    let (_tx, rx) = Box::new(radio).split().unwrap();
    let probe = Arc::new(Probe::default());
    runtime.receiver = TimedRx::new(
        Box::new(FaultRx {
            inner: rx.unwrap(),
            fault,
            probe: probe.clone(),
        }),
        runtime.receiver.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(997),
            timeout_us: 1000,
        },
    )
    .unwrap();
    probe
}

#[test]
fn stopped_before_activation_returns_original_runtime_once_and_caches_join() {
    let (_tx, _pump, _port, runtime, _ingress, _peer) = pipeline(2);
    let epoch = runtime.receiver.epoch();
    let stop = StopToken::default();
    stop.request_stop();
    let mut worker = RxWorker::spawn(runtime, stop, None, Duration::from_micros(100)).unwrap();
    assert!(worker.take_retired_runtime().is_none());
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.exit, WorkerExit::Stopped);
    assert_eq!(report.shutdown, HalStopOutcome::NotAttempted);
    assert!(report.statistics.is_some());
    assert!(!worker.is_ready());
    let runtime = worker.take_retired_runtime().unwrap();
    assert_eq!(runtime.receiver.epoch(), epoch);
    assert!(worker.take_retired_runtime().is_none());
    let repeated = worker.join(Duration::ZERO).unwrap();
    assert_eq!(repeated.exit, report.exit);
    assert_eq!(repeated.shutdown, report.shutdown);
    assert!(repeated.statistics.is_some());
}

#[test]
fn panic_returns_active_runtime_without_inventing_stop_or_dropping_it_on_worker() {
    let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
    let probe = fault(&mut runtime, Fault::Panic);
    let mut worker = RxWorker::spawn(
        runtime,
        StopToken::default(),
        None,
        Duration::from_micros(100),
    )
    .unwrap();
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.exit, WorkerExit::Panicked);
    assert_eq!(report.shutdown, HalStopOutcome::NotAttempted);
    assert_eq!(probe.stops.load(Ordering::Acquire), 0);
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    assert!(!worker.is_ready());
    let mut runtime = worker.take_retired_runtime().unwrap();
    runtime.stop().unwrap();
    assert_eq!(probe.stops.load(Ordering::Acquire), 1);
    drop(runtime);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn run_failure_and_actual_stop_result_remain_independent_with_original_owner() {
    for mode in [Fault::Read, Fault::ReadAndStop] {
        let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
        let probe = fault(&mut runtime, mode);
        let mut worker = RxWorker::spawn(
            runtime,
            StopToken::default(),
            None,
            Duration::from_micros(100),
        )
        .unwrap();
        let report = worker.join(Duration::from_secs(1)).unwrap();
        assert!(
            matches!(&report.exit, WorkerExit::Failed(error) if error.contains("RX read failure"))
        );
        match mode {
            Fault::Read => assert_eq!(report.shutdown, HalStopOutcome::Succeeded),
            Fault::ReadAndStop => assert!(
                matches!(&report.shutdown, HalStopOutcome::Failed(error) if error.contains("RX stop failure"))
            ),
            _ => unreachable!(),
        }
        assert_eq!(probe.stops.load(Ordering::Acquire), 1);
        assert_eq!(probe.drops.load(Ordering::Acquire), 0);
        let runtime = worker.take_retired_runtime().unwrap();
        assert!(runtime.started);
        drop(runtime);
        assert_eq!(probe.drops.load(Ordering::Acquire), 1);
    }
}

#[test]
fn join_deadline_retains_live_runtime_until_actual_worker_returns_it() {
    let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
    let probe = fault(&mut runtime, Fault::Wait);
    let mut worker = RxWorker::spawn(
        runtime,
        StopToken::default(),
        None,
        Duration::from_micros(100),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !probe.entered.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "worker did not enter actual read"
        );
        thread::yield_now();
    }
    assert!(matches!(
        worker.join(Duration::ZERO),
        Err(JoinError::Deadline)
    ));
    assert!(worker.take_retired_runtime().is_none());
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    probe.release.store(true, Ordering::Release);
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert!(matches!(report.exit, WorkerExit::Failed(_)));
    assert_eq!(report.shutdown, HalStopOutcome::Succeeded);
    drop(worker.take_retired_runtime().unwrap());
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn successful_run_does_not_hide_failed_explicit_stop_of_an_active_runtime() {
    let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
    let probe = fault(&mut runtime, Fault::IdleAndStop);
    let stop = StopToken::default();
    let mut worker = RxWorker::spawn(runtime, stop, None, Duration::from_micros(100)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !probe.entered.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "worker did not activate and enter real read"
        );
        thread::yield_now();
    }
    worker.request_stop();
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.exit, WorkerExit::Stopped);
    assert!(
        matches!(&report.shutdown, HalStopOutcome::Failed(error) if error.contains("RX stop failure"))
    );
    assert_eq!(probe.stops.load(Ordering::Acquire), 1);
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    let runtime = worker.take_retired_runtime().unwrap();
    assert!(runtime.started);
    drop(runtime);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn gated_cancellation_retains_preactivated_runtime_for_explicit_stop_retry() {
    let (_tx, _pump, _port, mut runtime, _ingress, _peer) = pipeline(2);
    let probe = fault(&mut runtime, Fault::ReadAndStop);
    runtime.start(None).unwrap();
    let stop = StopToken::default();
    stop.request_stop();
    let mut worker = RxWorker::spawn(runtime, stop, None, Duration::from_micros(100)).unwrap();
    let report = worker.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.exit, WorkerExit::Stopped);
    assert_eq!(report.shutdown, HalStopOutcome::NotAttempted);
    assert_eq!(probe.stops.load(Ordering::Acquire), 0);
    let mut runtime = worker.take_retired_runtime().unwrap();
    assert!(runtime.started);
    assert!(runtime.stop_for_retirement().is_err());
    assert_eq!(probe.stops.load(Ordering::Acquire), 1);
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    drop(runtime);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}
