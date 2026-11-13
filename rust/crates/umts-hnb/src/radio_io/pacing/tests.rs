// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    radio_io::*,
    test_support::{count, source},
};
use num_complex::Complex32;
use sdr_hal::{
    RadioRx, RadioTx, RxReadResult, Schedule, TxMetadata,
    error::{Error, ErrorKind},
};
use std::{
    collections::VecDeque,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use umts_runtime::worker::{JoinError, WorkerExit};

#[derive(Default)]
struct RadioState {
    now: AtomicU64,
    queries: AtomicU64,
    clock_error: AtomicBool,
    activation_error: AtomicBool,
    hold_write: AtomicBool,
    hold_activation: AtomicBool,
    write_entered: AtomicBool,
    active: AtomicBool,
    activations: AtomicU64,
    underflows: AtomicU64,
    late_packets: AtomicU64,
    health_queries: AtomicU64,
    health_delay_ticks: AtomicU64,
    health_queries_at_write: AtomicU64,
    stopped: AtomicBool,
    writes: Mutex<Vec<(u64, usize)>>,
    zero_sample_calls: AtomicU64,
}

#[test]
fn uncertain_wait_is_conservative_and_capped() {
    let rate = std::num::NonZeroU64::new(1_000_000).unwrap();
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            2_000,
            u64::MAX,
            uncertain_bounds(1_000, 1_000),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::from_micros(100)
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1_050,
            u64::MAX,
            uncertain_bounds(1_000, 1_000),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::from_micros(50)
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            999,
            u64::MAX,
            uncertain_bounds(1_000, 1_000),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::ZERO
    );
}

#[test]
fn uncertain_wait_floors_sub_tick_time_without_overwaiting() {
    let rate = std::num::NonZeroU64::new(30_720_000).unwrap();
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1,
            u64::MAX,
            uncertain_bounds(0, 0),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::from_nanos(32)
    );
}

#[test]
fn uncertain_wait_handles_due_zero_budget_and_extreme_clock_rates() {
    let rate = std::num::NonZeroU64::MIN;
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            50,
            u64::MAX,
            uncertain_bounds(50, 50),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::ZERO
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            u64::MAX,
            u64::MAX,
            uncertain_bounds(0, 0),
            rate,
            Duration::ZERO,
        )
        .unwrap(),
        Duration::ZERO
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            u64::MAX,
            u64::MAX,
            uncertain_bounds(0, 0),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::from_micros(100)
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1,
            u64::MAX,
            uncertain_bounds(0, 0),
            std::num::NonZeroU64::new(u64::MAX).unwrap(),
            Duration::from_micros(100)
        ),
        Ok(Duration::ZERO)
    );
}

fn uncertain_bounds(earliest: u64, latest: u64) -> umts_radio::clock_model::ClockBounds {
    umts_radio::clock_model::ClockBounds { earliest, latest }
}

#[test]
fn uncertain_wait_straddling_readiness_keeps_a_positive_sleep() {
    let rate = std::num::NonZeroU64::new(1_000_000).unwrap();
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1_667,
            2_900,
            uncertain_bounds(1_666, 2_000),
            rate,
            Duration::from_micros(100),
        )
        .unwrap(),
        Duration::from_micros(1)
    );
}

#[test]
fn uncertain_wait_caps_at_the_upper_bound_deadline() {
    let rate = std::num::NonZeroU64::new(1_000_000).unwrap();
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1_667,
            2_900,
            uncertain_bounds(1_000, 2_899),
            rate,
            Duration::from_millis(10),
        )
        .unwrap(),
        Duration::from_micros(1)
    );
}

#[test]
fn uncertain_wait_is_zero_at_either_ready_or_deadline_equality() {
    let rate = std::num::NonZeroU64::new(1_000_000).unwrap();
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1_667,
            2_900,
            uncertain_bounds(1_667, 2_900),
            rate,
            Duration::from_millis(10),
        )
        .unwrap(),
        Duration::ZERO
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1_667,
            2_900,
            uncertain_bounds(1_000, 2_900),
            rate,
            Duration::from_millis(10),
        )
        .unwrap(),
        Duration::ZERO
    );
}

#[test]
fn uncertain_wait_rejects_invalid_or_overflowing_inputs_closed() {
    let rate = std::num::NonZeroU64::new(1_000_000).unwrap();
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1,
            2,
            uncertain_bounds(3, 2),
            rate,
            Duration::from_millis(1),
        ),
        Err(super::UncertainWaitError::InvalidBounds)
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            3,
            2,
            uncertain_bounds(1, 1),
            rate,
            Duration::from_millis(1),
        ),
        Err(super::UncertainWaitError::InvalidWindow)
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            1,
            2,
            uncertain_bounds(1, 3),
            rate,
            Duration::from_millis(1),
        ),
        Err(super::UncertainWaitError::DeadlinePassed)
    );
    assert_eq!(
        super::bounded_uncertain_wait_duration(
            u64::MAX,
            u64::MAX,
            uncertain_bounds(0, 0),
            std::num::NonZeroU64::MIN,
            Duration::MAX,
        ),
        Err(super::UncertainWaitError::Overflow)
    );
}

#[test]
fn submission_wait_duration_refreshes_device_time_and_respects_cap() {
    let (_preparation, mut pump, shared) = pair(1_000, 3_000, 0, 1_000_000, vec![]);
    pump.start().unwrap();
    assert_eq!(
        pump.wait_duration(1_050, Duration::from_micros(100))
            .unwrap(),
        Duration::from_micros(50)
    );
    shared.now.store(1_100, Ordering::Release);
    assert_eq!(
        pump.wait_duration(1_050, Duration::from_micros(100))
            .unwrap(),
        Duration::ZERO
    );
}

#[test]
fn expired_deadline_wait_repolls_immediately() {
    let rate = std::num::NonZeroU64::new(1_000_000).unwrap();
    assert_eq!(
        crate::radio_io::deadline_wait_duration(
            1_350,
            1_400,
            uncertain_bounds(1_000, 1_401),
            rate,
            Duration::from_millis(1),
        )
        .unwrap(),
        Duration::ZERO
    );
}

#[test]
fn preparation_progress_preserves_wait_target_and_published_bounds() {
    let (mut preparation, _pump, _shared) = pair(1_000, 8_000, 0, 1_000_000, vec![]);
    match preparation.prepare_progress().unwrap() {
        PreparationProgress::Waiting {
            until_tick,
            deadline_tick,
            bounds,
        } => {
            assert!(until_tick > bounds.latest);
            assert!(deadline_tick > until_tick);
            assert_eq!(bounds.earliest, 1_000);
            assert_eq!(bounds.latest, 1_000);
        }
        progress => panic!("unexpected preparation progress: {progress:?}"),
    }
}

#[test]
fn ready_publication_preserves_wake_token_before_submission_park() {
    let (mut preparation, _pump, _shared) = pair(0, 3_000, 0, 1_000_000, vec![]);
    preparation.enable_timing();
    let wake = Arc::new(OnceLock::new());
    preparation.submission_wake = Some(wake.clone());
    let registered = Arc::new(AtomicBool::new(false));
    let publication = Arc::new(AtomicBool::new(false));
    let park_entered = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(AtomicBool::new(false));
    let worker_registered = registered.clone();
    let worker_publication = publication.clone();
    let worker_park_entered = park_entered.clone();
    let worker_completed = completed.clone();
    let worker = thread::spawn(move || {
        wake.set(thread::current()).unwrap();
        worker_registered.store(true, Ordering::Release);
        while !worker_publication.load(Ordering::Acquire) {
            thread::yield_now();
        }
        worker_park_entered.store(true, Ordering::Release);
        thread::park();
        worker_completed.store(true, Ordering::Release);
    });

    const MAX_YIELDS: usize = 100_000;
    let mut registered_seen = false;
    for _ in 0..MAX_YIELDS {
        if registered.load(Ordering::Acquire) {
            registered_seen = true;
            break;
        }
        thread::yield_now();
    }

    let published = if registered_seen {
        preparation.prepare_one().map(|progress| progress.is_some())
    } else {
        Ok(false)
    };
    publication.store(true, Ordering::Release);

    let mut park_seen = false;
    for _ in 0..MAX_YIELDS {
        if park_entered.load(Ordering::Acquire) {
            park_seen = true;
            break;
        }
        thread::yield_now();
    }

    let mut woke = false;
    for _ in 0..MAX_YIELDS {
        if completed.load(Ordering::Acquire) {
            woke = true;
            break;
        }
        thread::yield_now();
    }
    if !woke {
        worker.thread().unpark();
    }
    assert!(worker.join().is_ok());
    assert_eq!(published.ok(), Some(true));
    assert!(registered_seen);
    assert!(park_seen);
    assert!(woke, "publication did not preserve the wake token");
    let history = preparation.timings().unwrap().publication_completions;
    assert_eq!(history.count, 1);
    let event = history.records[0].unwrap();
    assert!(event.wake_registered);
    assert_eq!(event.first_sample.index, 0);
    assert!(event.next_sample.index > event.first_sample.index);
    let unpark_started = event.unpark_started_ns.unwrap();
    assert!(unpark_started >= event.enqueue_returned_ns);
    assert!(event.unpark_returned_ns.unwrap() >= unpark_started);
}

struct ClockedTx {
    shared: Arc<RadioState>,
    calibration: i64,
    rate: u64,
    outcomes: VecDeque<Result<usize, Error>>,
}

fn wait_while(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while condition() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "bounded worker observation timed out"
        );
        thread::sleep(Duration::from_micros(100));
    }
}

impl RadioTx for ClockedTx {
    fn supports_timed_tx(&self) -> bool {
        true
    }
    fn tx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn rx_to_tx_delay_ns(&self) -> Option<i64> {
        Some(self.calibration)
    }
    fn tick_rate(&self) -> u64 {
        self.rate
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.shared.queries.fetch_add(1, Ordering::AcqRel);
        if self.shared.clock_error.load(Ordering::Acquire) {
            Err(Error::new(ErrorKind::Disconnected, "injected clock loss"))
        } else {
            Ok(self.shared.now.load(Ordering::Acquire))
        }
    }
    fn transmit(&mut self, _: &[Complex32]) -> Result<(), Error> {
        panic!("untimed TX");
    }
    fn enable_transmit(&mut self, active: bool) -> Result<(), Error> {
        if active {
            self.shared.activations.fetch_add(1, Ordering::AcqRel);
            wait_while(|| self.shared.hold_activation.load(Ordering::Acquire));
            if self.shared.activation_error.load(Ordering::Acquire) {
                return Err(Error::new(
                    ErrorKind::Disconnected,
                    "injected activation loss",
                ));
            }
        } else {
            self.shared.stopped.store(true, Ordering::Release);
        }
        self.shared.active.store(active, Ordering::Release);
        Ok(())
    }
    fn tx_health(&mut self) -> Result<sdr_hal::TxRadioHealth, Error> {
        self.shared.health_queries.fetch_add(1, Ordering::AcqRel);
        self.shared.now.fetch_add(
            self.shared.health_delay_ticks.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        Ok(sdr_hal::TxRadioHealth {
            underflows: self.shared.underflows.load(Ordering::Acquire),
            late_packets: self.shared.late_packets.load(Ordering::Acquire),
            ..Default::default()
        })
    }
    fn write(&mut self, samples: &[Complex32], metadata: TxMetadata) -> Result<usize, Error> {
        assert!(self.shared.active.load(Ordering::Acquire));
        let Schedule::At(time) = metadata.schedule else {
            panic!("untimed TX")
        };
        self.shared.write_entered.store(true, Ordering::Release);
        self.shared.health_queries_at_write.store(
            self.shared.health_queries.load(Ordering::Acquire),
            Ordering::Release,
        );
        if samples.iter().all(|sample| *sample == Complex32::default()) {
            self.shared.zero_sample_calls.fetch_add(1, Ordering::AcqRel);
        }
        wait_while(|| self.shared.hold_write.load(Ordering::Acquire));
        self.shared
            .writes
            .lock()
            .unwrap()
            .push((time.ticks(), samples.len()));
        self.outcomes.pop_front().unwrap_or(Ok(samples.len()))
    }
}

struct ClockOnlyRx(Arc<RadioState>);

impl RadioRx for ClockOnlyRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn tick_rate(&self) -> u64 {
        1_000_000
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        Ok(self.0.now.load(Ordering::Acquire))
    }
    fn rx_read(&mut self, _: &mut [Complex32], _: i64) -> Result<RxReadResult, Error> {
        panic!("clock-only receiver must not read samples")
    }
    fn rx_activate(&mut self, _: Option<u64>) -> Result<(), Error> {
        panic!("clock-only receiver must not activate")
    }
    fn stop(&mut self) -> Result<(), Error> {
        panic!("clock-only receiver must not activate")
    }
}

struct ReleaseHolds(Arc<RadioState>);
impl Drop for ReleaseHolds {
    fn drop(&mut self) {
        self.0.hold_write.store(false, Ordering::Release);
        self.0.hold_activation.store(false, Ordering::Release);
    }
}

fn limits() -> umts_radio::pacing::LeadDurations {
    umts_radio::pacing::LeadDurations {
        minimum_submission: Duration::from_micros(100),
        maximum_submission: Duration::from_millis(2),
        maximum_preparation: Duration::from_millis(4),
    }
}

#[test]
fn aligned_two_slot_batch_is_submitted_about_one_millisecond_before_first_sample() {
    let shared = Arc::new(RadioState::default());
    let origin = 307_200;
    let tick_rate = 30_720_000;
    let tx = ClockedTx {
        shared: shared.clone(),
        calibration: 0,
        rate: tick_rate,
        outcomes: VecDeque::new(),
    };
    let (mut preparation, mut pump) = prepare_tx_batched(
        source(false),
        Box::new(tx),
        TxSettings {
            timing: TxTiming::DeviceClock(LeadDurations {
                minimum_submission: Duration::from_micros(100),
                maximum_submission: Duration::from_micros(2334),
                maximum_preparation: Duration::from_micros(2434),
            }),
            ..TxSettings::offline(origin, count(2), count(1), count(3))
        },
        TxBatch::TwoSlots,
    )
    .unwrap();
    pump.start().unwrap();
    let PreparationProgress::Waiting { until_tick, .. } = preparation.prepare_progress().unwrap()
    else {
        panic!("preparation must wait for its batch lead")
    };
    shared.now.store(until_tick, Ordering::Release);
    pump.cycle().unwrap();
    let Some(window) = preparation.prepare_one().unwrap() else {
        panic!("aligned batch ready")
    };
    assert_eq!(window.next_sample.index - window.first_sample.index, 10240);
    let PumpProgress::Waiting { until_tick, .. } = pump.cycle().unwrap() else {
        panic!("submission has its own later release gate")
    };
    let lead_ns = (origin - until_tick) * 1_000_000_000 / tick_rate;
    assert!((1_000_000..1_001_000).contains(&lead_ns));
    shared.now.store(until_tick, Ordering::Release);
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert_eq!(*shared.writes.lock().unwrap(), [(origin, 10240)]);
}

fn pair(
    now: u64,
    origin: u64,
    calibration: i64,
    rate: u64,
    outcomes: Vec<Result<usize, Error>>,
) -> (TxPreparation, TxPump, Arc<RadioState>) {
    pair_with_source(source(false), now, origin, calibration, rate, outcomes)
}

#[test]
fn urgent_queue_polling_is_bounded_by_the_next_batch_deadline() {
    let (_preparation, mut pump, shared) = pair(1_000, 2_000, 0, 1_000_000, vec![]);
    pump.start().unwrap();
    let margin = Duration::from_micros(250);
    assert!(!pump.urgent_queue_wait(margin).unwrap());
    let first = pump.submission.tick_at(pump.next_expected_sample).unwrap();
    let deadline = first - pump.pacing.as_ref().unwrap().budget.minimum_ticks();
    shared.now.store(deadline - 250, Ordering::Release);
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Idle));
    let queries = shared.queries.load(Ordering::Acquire);
    assert!(pump.urgent_queue_wait(margin).unwrap());
    assert_eq!(shared.queries.load(Ordering::Acquire), queries);
    shared.now.store(deadline, Ordering::Release);
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Idle));
    assert!(pump.urgent_queue_wait(margin).unwrap());
    shared.now.store(deadline + 1, Ordering::Release);
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Idle));
    assert!(!pump.urgent_queue_wait(margin).unwrap());
    pump.pacing = None;
    assert!(!pump.urgent_queue_wait(margin).unwrap());
}

fn pair_with_source(
    source: CommonTx,
    now: u64,
    origin: u64,
    calibration: i64,
    rate: u64,
    outcomes: Vec<Result<usize, Error>>,
) -> (TxPreparation, TxPump, Arc<RadioState>) {
    let shared = Arc::new(RadioState::default());
    shared.now.store(now, Ordering::Release);
    let tx = ClockedTx {
        shared: shared.clone(),
        calibration,
        rate,
        outcomes: outcomes.into(),
    };
    let (preparation, submission) = prepare_tx(
        source,
        Box::new(tx),
        TxSettings {
            timing: TxTiming::DeviceClock(limits()),
            ..TxSettings::offline(origin, count(4), count(1), count(3))
        },
    )
    .unwrap();
    (preparation, submission, shared)
}

#[test]
fn amplitude_rejection_writes_timed_silence_without_acknowledging_data_and_recycles_buffers() {
    let mut source = source(false);
    source.set_component_peak_limit(0.0001).unwrap();
    let progress = source.submission_progress();
    let (mut preparation, mut pump, shared) =
        pair_with_source(source, 1000, 2000, 0, 1_000_000, vec![]);
    preparation.pacing = None;
    pump.pacing = None;
    assert_eq!(preparation.component_peak_limit, Some(0.0001));
    pump.start().unwrap();
    let origin = preparation.source().next_chip();
    for index in 0..8 {
        let window = preparation.prepare_one().unwrap().unwrap();
        let receipt = progress
            .receipt(
                window.first_sample,
                window.next_sample,
                preparation.source().next_chip(),
            )
            .unwrap();
        assert!(matches!(
            pump.cycle().unwrap(),
            PumpProgress::Dropped {
                cause: DropCause::Amplitude,
                deadline: None,
                ..
            }
        ));
        assert!(!receipt.viable());
        assert!(!receipt.submitted());
        assert_eq!(pump.next_expected_sample, window.next_sample);
        assert!(progress.available());
        assert_eq!(preparation.source().next_chip().epoch, origin.epoch);
        assert_eq!(
            preparation.source().next_chip().index,
            origin.index + (index + 1) * CHIPS_PER_SLOT
        );
        assert!(!preparation.stop_token().is_stopping());
    }
    assert_eq!(shared.writes.lock().unwrap().len(), 8);
    assert_eq!(shared.zero_sample_calls.load(Ordering::Acquire), 8);
    assert_eq!(pump.recovery().amplitude_dropped, 8);
    assert_eq!(pump.recovery().quality_muted_batches, 8);
    assert_eq!(pump.recovery().quality_muted_samples, 8 * 5120);
    assert_eq!(pump.recovery().dropped_samples, 8 * 5120);
    preparation.component_peak_limit = Some(0.9);
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert_eq!(shared.writes.lock().unwrap().len(), 9);
    assert_eq!(shared.zero_sample_calls.load(Ordering::Acquire), 8);
    assert_eq!(pump.recovery().amplitude_dropped, 8);
}

#[test]
fn component_budget_never_admits_nonfinite_samples_even_after_a_finite_exceedance() {
    assert_eq!(
        exceeds_component_limit(&[Complex32::new(0.5, -0.5)], 0.5),
        Ok(false)
    );
    assert_eq!(
        exceeds_component_limit(&[Complex32::new(0.51, 0.0)], 0.5),
        Ok(true)
    );
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(
            exceeds_component_limit(
                &[Complex32::new(1.0, 0.0), Complex32::new(0.0, invalid)],
                0.5
            ),
            Err(CommonTxError::Dsp(DspError::NonFiniteEvidence))
        );
    }
}

#[test]
fn crest_quality_rejection_loses_the_exact_window_while_preserving_stream_continuity() {
    let mut source = source(false);
    source.set_component_peak_limit(0.0001).unwrap();
    source.enable_crest_reduction(SamplesPerChip::Two).unwrap();
    let (mut preparation, mut pump, shared) =
        pair_with_source(source, 1000, 2000, 0, 1_000_000, vec![]);
    preparation.pacing = None;
    pump.pacing = None;
    pump.start().unwrap();
    for _ in 0..8 {
        let window = preparation.prepare_one().unwrap().unwrap();
        assert_eq!(
            pump.cycle().unwrap(),
            PumpProgress::Dropped {
                window,
                deadline: None,
                cause: DropCause::Amplitude,
            }
        );
        assert!(!preparation.stop_token().is_stopping());
    }
    assert_eq!(shared.writes.lock().unwrap().len(), 8);
    assert_eq!(shared.zero_sample_calls.load(Ordering::Acquire), 8);
    assert_eq!(pump.recovery().amplitude_dropped, 8);
    assert_eq!(pump.recovery().quality_muted_samples, 8 * 5120);
}

#[test]
fn partial_silence_never_acknowledges_any_rejected_payload_prefix() {
    let mut source = source(false);
    source.set_component_peak_limit(0.0001).unwrap();
    let progress = source.submission_progress();
    let (mut preparation, mut pump, shared) =
        pair_with_source(source, 1000, 2000, 0, 1_000_000, vec![Ok(256)]);
    preparation.pacing = None;
    pump.pacing = None;
    pump.start().unwrap();
    let window = preparation.prepare_one().unwrap().unwrap();
    let receipt = progress
        .receipt(
            window.first_sample,
            window.next_sample,
            preparation.source().next_chip(),
        )
        .unwrap();
    assert!(matches!(
        pump.cycle().unwrap(),
        PumpProgress::Partial { accepted: 256, .. }
    ));
    assert!(!receipt.submitted());
    assert!(matches!(
        pump.cycle().unwrap(),
        PumpProgress::Dropped {
            cause: DropCause::Amplitude,
            ..
        }
    ));
    assert!(!receipt.viable());
    assert!(!receipt.submitted());
    assert_eq!(shared.zero_sample_calls.load(Ordering::Acquire), 2);
    assert_eq!(pump.recovery().quality_muted_samples, 5120);
    assert_eq!(pump.recovery().dropped_samples, 5120);
}

#[test]
fn expired_quality_muted_window_is_lost_without_writing_late_silence() {
    let mut source = source(false);
    source.set_component_peak_limit(0.0001).unwrap();
    let (mut preparation, mut pump, shared) =
        pair_with_source(source, 1000, 2000, 0, 1_000_000, vec![]);
    preparation.pacing = None;
    pump.start().unwrap();
    preparation.prepare_one().unwrap().unwrap();
    shared.now.store(2001, Ordering::Release);
    assert!(matches!(
        pump.cycle().unwrap(),
        PumpProgress::Dropped {
            cause: DropCause::Submission,
            ..
        }
    ));
    assert!(shared.writes.lock().unwrap().is_empty());
    assert_eq!(pump.recovery().amplitude_dropped, 1);
    assert_eq!(pump.recovery().submission_late, 1);
    assert_eq!(pump.recovery().quality_muted_samples, 0);
    assert_eq!(pump.recovery().dropped_samples, 5120);
}

#[test]
fn dedicated_feedback_binds_the_actual_preparation_lane_clock_and_lead_budget() {
    fn attach(preparation: &mut TxPreparation) -> crate::dedicated_tx::DedicatedTransmitPorts {
        preparation
            .dedicated_ports(
                count(2),
                count(4),
                count(2_000_000),
                crate::dedicated_tx::TransmitRuntimeLimits {
                    command_capacity: count(2),
                    commands_per_step: count(1),
                    feedback_capacity: count(2),
                    feedback_per_step: count(1),
                    maximum_frame_payload_bits: count(100_000),
                    maximum_inflight_retained_bits: count(2_000_000),
                },
            )
            .unwrap()
    }
    let (mut first, _first_pump, _) = pair(0, 3000, 0, 1_000_000, vec![]);
    let (mut second, _second_pump, _) = pair(0, 3000, 0, 1_000_000, vec![]);
    let first_ports = attach(&mut first);
    let second_ports = attach(&mut second);
    let returned = match first.dedicated_feedback(second_ports.feedback) {
        Ok(_) => panic!("foreign feedback lane paired with a different preparation owner"),
        Err(port) => port,
    };
    assert!(returned.available());
    assert_eq!(returned.outstanding(), 0);
    let source = first
        .dedicated_feedback(first_ports.feedback)
        .unwrap_or_else(|_| panic!("paired feedback"));
    assert_eq!(source.origin(), first.air_origin());
    assert_eq!(source.minimum_lead_chips(), 15_360);
    assert!(first_ports.control.available());
    assert!(second_ports.control.available());
}

#[test]
fn tx_pacing_limits_preparation_and_full_slot_submission_without_spending_retry_budget() {
    let (mut preparation, mut pump, shared) = pair(0, 3000, 0, 1_000_000, vec![]);
    assert!(preparation.prepare_one().unwrap().is_some());
    assert!(preparation.prepare_one().unwrap().is_none());
    pump.start().unwrap();
    shared.now.store(1000, Ordering::Release);
    for _ in 0..8 {
        assert_eq!(
            pump.cycle().unwrap(),
            PumpProgress::Waiting {
                until_tick: 1668,
                deadline_tick: 2_900,
            }
        );
        assert_eq!(pump.pending.as_ref().unwrap().attempts, 0);
    }
    assert!(preparation.prepare_one().unwrap().is_some());
    assert!(shared.writes.lock().unwrap().is_empty());
    shared.now.store(1668, Ordering::Release);
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    assert_eq!(*shared.writes.lock().unwrap(), vec![(3000, 5120)]);
}

#[test]
fn paired_preparation_extrapolates_time_without_submission_or_new_hal_queries() {
    use crate::radio_io::sampled_clock::HostClock;
    use umts_dsp::pulse_shaping::SamplesPerChip;
    use umts_radio::clock_model::{ClockError, ClockSettings};

    let (mut preparation, mut pump, shared) = pair(1_000, 8_000, 0, 1_000_000, vec![]);
    let mut receiver = TimedRx::new(
        Box::new(ClockOnlyRx(shared.clone())),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(5120),
            timeout_us: 1000,
        },
    )
    .unwrap();
    let host = shared.clone();
    pump.use_sampled_clock_pair_with_host(
        &mut preparation,
        &mut receiver,
        ClockSamplingSettings {
            interval: Duration::from_millis(50),
            model: ClockSettings {
                maximum_age: Some(Duration::from_millis(200)),
                maximum_query: Duration::from_micros(100),
                maximum_drift_ppm: 0,
            },
        },
        HostClock::Controlled(Arc::new(move || {
            Duration::from_micros(host.now.load(Ordering::Acquire))
        })),
    )
    .unwrap();
    let queries = shared.queries.load(Ordering::Acquire);
    shared.now.store(1_100, Ordering::Release);
    assert!(matches!(
        preparation.prepare_progress().unwrap(),
        PreparationProgress::Waiting {
            bounds: umts_radio::clock_model::ClockBounds {
                earliest: 1_099,
                latest: 1_101
            },
            ..
        }
    ));
    let schedule = preparation
        .wait_schedule(1_151, 1_151, Duration::from_micros(100))
        .unwrap();
    assert_eq!(schedule.evidence.source, WaitClockSource::FreshSampled);
    assert_eq!(schedule.duration, Duration::from_micros(50));
    shared.now.store(1_200, Ordering::Release);
    assert_eq!(
        preparation
            .wait_schedule(1_151, 1_151, Duration::from_micros(100))
            .unwrap()
            .duration,
        Duration::ZERO
    );
    assert_eq!(shared.queries.load(Ordering::Acquire), queries);
    shared.now.store(201_001, Ordering::Release);
    assert!(matches!(
        preparation.prepare_progress(),
        Err(PreparationError::ClockModel(ClockError::Stale))
    ));
    assert_eq!(
        pump.sampled_clock.as_mut().unwrap().read(),
        Err(ClockError::Discontinuity)
    );
}

struct PairedClockFixture {
    preparation: TxPreparation,
    pump: TxPump,
    receiver: TimedRx,
    shared: Arc<RadioState>,
    host: crate::radio_io::sampled_clock::HostClock,
    settings: ClockSamplingSettings,
}

impl PairedClockFixture {
    fn new() -> Self {
        let (preparation, pump, shared) = pair(1_000, 8_000, 0, 1_000_000, vec![]);
        let receiver = TimedRx::new(
            Box::new(ClockOnlyRx(shared.clone())),
            preparation.air_origin(),
            RxSettings {
                rate: umts_dsp::pulse_shaping::SamplesPerChip::Two,
                span_chips: 32,
                phase: 0,
                maximum_samples: count(5120),
                timeout_us: 1000,
            },
        )
        .unwrap();
        let time = shared.clone();
        Self {
            preparation,
            pump,
            receiver,
            shared,
            host: crate::radio_io::sampled_clock::HostClock::Controlled(Arc::new(move || {
                Duration::from_micros(time.now.load(Ordering::Acquire))
            })),
            settings: ClockSamplingSettings {
                interval: Duration::from_millis(50),
                model: umts_radio::clock_model::ClockSettings {
                    maximum_age: Some(Duration::from_millis(200)),
                    maximum_query: Duration::from_micros(100),
                    maximum_drift_ppm: 0,
                },
            },
        }
    }

    fn attach(&mut self) -> Result<(), PumpError> {
        self.pump.use_sampled_clock_pair_with_host(
            &mut self.preparation,
            &mut self.receiver,
            self.settings,
            self.host.clone(),
        )
    }

    fn publication(&self) -> umts_radio::clock_model::ClockBounds {
        self.preparation
            .pacing
            .as_ref()
            .unwrap()
            .observed
            .read_bounds()
            .unwrap()
    }

    fn assert_unattached(&self, original: umts_radio::clock_model::ClockBounds) {
        assert!(self.preparation.sampled_clock.is_none());
        assert!(self.pump.sampled_clock.is_none());
        assert!(self.receiver.clock_sampler.is_none());
        assert_eq!(self.publication(), original);
        assert!(
            self.preparation
                .pacing
                .as_ref()
                .unwrap()
                .observed
                .same_clock(&self.pump.pacing.as_ref().unwrap().observed)
        );
    }
}

#[test]
fn actual_startup_pair_publishes_identical_seed_bounds() {
    let mut fixture = PairedClockFixture::new();
    fixture.attach().unwrap();
    let preparation = fixture
        .preparation
        .sampled_clock
        .as_mut()
        .unwrap()
        .read()
        .unwrap();
    let submission = fixture.pump.sampled_clock.as_mut().unwrap().read().unwrap();
    assert_eq!(preparation, submission);
    assert_eq!(preparation.latest, fixture.publication().latest);
    assert!(fixture.publication().earliest >= preparation.earliest);
    assert_eq!(preparation, uncertain_bounds(999, 1_001));
}

#[test]
fn attach_rejects_origin_tick_rate_and_active_owners_without_partial_state() {
    for case in 0..4 {
        let mut fixture = PairedClockFixture::new();
        let original = fixture.publication();
        let queries = fixture.shared.queries.load(Ordering::Acquire);
        match case {
            0 => fixture.receiver.air_origin.tick += 1,
            1 => fixture.receiver.air_origin.tick_rate = std::num::NonZeroU64::MIN,
            2 => fixture.receiver.active = true,
            3 => fixture.pump.start().unwrap(),
            _ => unreachable!(),
        }
        let result = fixture.attach();
        if case == 2 {
            fixture.receiver.active = false;
        }
        if case == 3 {
            fixture.pump.stop().unwrap();
        }
        assert!(matches!(result, Err(PumpError::ClockSamplingState)));
        fixture.assert_unattached(original);
        assert_eq!(fixture.shared.queries.load(Ordering::Acquire), queries);
    }
}

#[test]
fn startup_query_and_model_failures_leave_no_installed_pair_or_new_publication() {
    for model_failure in [false, true] {
        let mut fixture = PairedClockFixture::new();
        let original = fixture.publication();
        if model_failure {
            let receiver = Arc::new(RadioState::default());
            receiver.now.store(999, Ordering::Release);
            fixture.receiver.rx = Some(Box::new(ClockOnlyRx(receiver)));
            assert!(matches!(
                fixture.attach(),
                Err(PumpError::ClockModel(
                    umts_radio::clock_model::ClockError::TickReversed
                ))
            ));
        } else {
            fixture.shared.clock_error.store(true, Ordering::Release);
            assert!(matches!(fixture.attach(), Err(PumpError::DriverClock(_))));
        }
        fixture.assert_unattached(original);
    }
}

#[test]
fn receiver_tick_rate_mismatch_is_rejected_before_attachment() {
    let fixture = PairedClockFixture::new();
    let original = fixture.publication();
    let mut origin = fixture.preparation.air_origin();
    origin.tick_rate = std::num::NonZeroU64::MIN;
    assert!(matches!(
        TimedRx::new(
            Box::new(ClockOnlyRx(fixture.shared.clone())),
            origin,
            RxSettings {
                rate: umts_dsp::pulse_shaping::SamplesPerChip::Two,
                span_chips: 32,
                phase: 0,
                maximum_samples: count(5120),
                timeout_us: 1000
            }
        ),
        Err(RxError::Time(umts_types::time::TimeError::InvalidRate))
    ));
    fixture.assert_unattached(original);
}

#[test]
fn newer_fanout_observation_controls_preparation_despite_frozen_publication() {
    let mut fixture = PairedClockFixture::new();
    fixture.attach().unwrap();
    let frozen = fixture.publication();
    fixture.shared.now.store(51_000, Ordering::Release);
    fixture.receiver.sample_clock().unwrap();
    assert_eq!(fixture.publication(), frozen);
    assert_eq!(fixture.receiver.counters.clock_queries, 9);
    assert!(matches!(
        fixture.preparation.prepare_progress(),
        Ok(PreparationProgress::Skipped {
            deadline: DeadlineFailure {
                device_tick: 51_001,
                ..
            },
            ..
        })
    ));
    assert!(matches!(
        fixture.pump.ready.try_recv().unwrap().late,
        Some(DeadlineFailure {
            device_tick: 51_001,
            ..
        })
    ));
    assert_eq!(fixture.publication(), frozen);
}

#[test]
fn preparation_wait_refreshes_both_bounds_and_rechecks_gate_after_virtual_wake() {
    let mut fixture = PairedClockFixture::new();
    fixture.attach().unwrap();
    let PreparationProgress::Waiting {
        until_tick,
        deadline_tick,
        ..
    } = fixture.preparation.prepare_progress().unwrap()
    else {
        panic!("expected wait");
    };
    let before = fixture
        .preparation
        .wait_schedule(until_tick, deadline_tick, Duration::from_micros(100))
        .unwrap();
    assert_eq!(before.evidence.bounds, uncertain_bounds(999, 1_001));
    assert_eq!(before.duration, Duration::from_micros(100));
    fixture.shared.now.store(until_tick + 1, Ordering::Release);
    let after = fixture
        .preparation
        .wait_schedule(until_tick, deadline_tick, Duration::from_micros(100))
        .unwrap();
    assert_eq!(after.duration, Duration::ZERO);
    assert!(after.evidence.bounds.earliest >= until_tick);
    assert!(matches!(
        fixture.preparation.prepare_progress().unwrap(),
        PreparationProgress::Prepared(_)
    ));
}

#[test]
fn paired_clock_attachment_rejects_foreign_owners_without_partial_installation() {
    use crate::radio_io::sampled_clock::HostClock;
    use umts_dsp::pulse_shaping::SamplesPerChip;
    use umts_radio::clock_model::{ClockError, ClockSettings};

    let (mut preparation, mut pump, shared) = pair(1_000, 8_000, 0, 1_000_000, vec![]);
    let (mut foreign, _foreign_pump, _) = pair(1_000, 8_000, 0, 1_000_000, vec![]);
    let queries = shared.queries.load(Ordering::Acquire);
    let mut receiver = TimedRx::new(
        Box::new(ClockOnlyRx(shared.clone())),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(5120),
            timeout_us: 1000,
        },
    )
    .unwrap();
    let settings = ClockSamplingSettings {
        interval: Duration::from_millis(50),
        model: ClockSettings {
            maximum_age: Some(Duration::from_millis(200)),
            maximum_query: Duration::from_micros(100),
            maximum_drift_ppm: 0,
        },
    };
    let host = shared.clone();
    let host = HostClock::Controlled(Arc::new(move || {
        Duration::from_micros(host.now.load(Ordering::Acquire))
    }));
    assert!(matches!(
        pump.use_sampled_clock_pair_with_host(&mut foreign, &mut receiver, settings, host.clone()),
        Err(PumpError::ClockSamplingState)
    ));
    assert!(foreign.sampled_clock.is_none());
    assert!(preparation.sampled_clock.is_none());
    assert!(pump.sampled_clock.is_none());
    assert!(receiver.clock_sampler.is_none());
    assert_eq!(shared.queries.load(Ordering::Acquire), queries);
    pump.use_sampled_clock_pair_with_host(&mut preparation, &mut receiver, settings, host.clone())
        .unwrap();
    assert!(matches!(
        pump.use_sampled_clock_pair_with_host(&mut preparation, &mut receiver, settings, host),
        Err(PumpError::ClockSamplingState)
    ));
    assert!(preparation.sampled_clock.as_mut().unwrap().read().is_ok());
    drop(preparation);
    assert_eq!(
        pump.sampled_clock.as_mut().unwrap().read(),
        Err(ClockError::Discontinuity)
    );
}

#[test]
fn sampled_clock_submits_without_per_cycle_tx_queries_and_rejects_stale_time() {
    use crate::radio_io::sampled_clock::HostClock;
    use umts_dsp::pulse_shaping::SamplesPerChip;
    use umts_radio::clock_model::{ClockError, ClockSettings};

    let (mut preparation, mut pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    let mut receiver = TimedRx::new(
        Box::new(ClockOnlyRx(shared.clone())),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(5120),
            timeout_us: 1000,
        },
    )
    .unwrap();
    let host = shared.clone();
    pump.use_sampled_clock_pair_with_host(
        &mut preparation,
        &mut receiver,
        ClockSamplingSettings {
            interval: Duration::from_millis(50),
            model: ClockSettings {
                maximum_age: Some(Duration::from_secs(1)),
                maximum_query: Duration::from_micros(100),
                maximum_drift_ppm: 0,
            },
        },
        HostClock::Controlled(Arc::new(move || {
            Duration::from_micros(host.now.load(Ordering::Acquire))
        })),
    )
    .unwrap();
    let startup_queries = shared.queries.load(Ordering::Acquire);
    assert!(startup_queries > 0);
    preparation.enable_timing();
    pump.enable_timing();
    assert!(preparation.prepare_one().unwrap().is_some());
    pump.start().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    assert_eq!(*shared.writes.lock().unwrap(), vec![(2000, 5120)]);
    shared.now.store(1100, Ordering::Release);
    for _ in 0..8 {
        assert_eq!(pump.cycle().unwrap(), PumpProgress::Idle);
        assert_eq!(shared.queries.load(Ordering::Acquire), startup_queries);
    }
    assert_eq!(pump.timings().unwrap().clock_query.calls, 0);
    assert_eq!(pump.timings().unwrap().write.calls, 1);
    assert_eq!(preparation.timings().unwrap().synthesis.calls, 1);
    let completed = preparation
        .timings()
        .unwrap()
        .publication_completions
        .records[0]
        .unwrap();
    assert!(!completed.wake_registered);
    assert_eq!(completed.unpark_started_ns, None);
    assert_eq!(completed.unpark_returned_ns, None);
    let publication = pump.timings().unwrap().last_publication.unwrap();
    assert_eq!(publication.first_tick, 2000);
    assert_eq!(publication.first_sample.index, 0);
    assert!(publication.deadline_tick.is_some());
    assert_eq!(publication.enqueue_started_from_wait_start, None);
    assert_eq!(publication.dequeued_from_wait_wake, None);
    shared.now.store(1_001_001, Ordering::Release);
    assert!(matches!(
        pump.cycle(),
        Err(PumpError::ClockModel(ClockError::Stale))
    ));
    assert_eq!(shared.queries.load(Ordering::Acquire), startup_queries);
    assert_eq!(*shared.writes.lock().unwrap(), vec![(2000, 5120)]);
    assert_eq!(pump.timings().unwrap().clock_query.calls, 0);
    assert_eq!(pump.timings().unwrap().write.calls, 1);
    assert!(preparation.stop_token().is_stopping());
}

#[test]
fn tx_pacing_checks_the_remaining_prefix_again_before_a_partial_write_retry() {
    let (mut preparation, mut pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![Ok(1000)]);
    preparation.enable_timing();
    pump.enable_timing();
    preparation.prepare_one().unwrap().unwrap();
    pump.start().unwrap();
    assert_eq!(
        pump.cycle().unwrap(),
        PumpProgress::Partial {
            accepted: 1000,
            total: 5120
        }
    );
    shared.now.store(2031, Ordering::Release);
    assert!(matches!(
        pump.cycle(),
        Ok(PumpProgress::Dropped {
            deadline: Some(DeadlineFailure {
                first_tick: 2130,
                shortfall_ticks: 1,
                ..
            }),
            ..
        })
    ));
    assert_eq!(shared.writes.lock().unwrap().len(), 1);
    assert!(pump.pending.is_none());
    let dropped = pump.dropped_publication.take().unwrap();
    assert_eq!(dropped.evidence.first_sample.index, 1000);
    assert_eq!(dropped.evidence.first_tick, 2130);
    assert!(dropped.dequeued >= dropped.enqueued);
    assert!(!preparation.stop_token().is_stopping());
    assert!(preparation.prepare_one().unwrap().is_some());
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    assert_eq!(*shared.writes.lock().unwrap(), [(2000, 5120), (2667, 5120)]);
}

#[test]
fn tx_pacing_rejects_clock_reset_or_query_loss_before_any_write() {
    for reset in [false, true] {
        let (mut preparation, mut pump, shared) = pair(1000, 3000, 0, 1_000_000, vec![]);
        preparation.prepare_one().unwrap().unwrap();
        pump.start().unwrap();
        shared.now.store(1100, Ordering::Release);
        assert!(matches!(pump.cycle(), Ok(PumpProgress::Waiting { .. })));
        if reset {
            shared.now.store(1099, Ordering::Release);
        } else {
            shared.clock_error.store(true, Ordering::Release);
        }
        assert!(matches!(
            pump.cycle(),
            Err(PumpError::Clock(ClockFailure::Reversed { .. }) | PumpError::DriverClock(_))
        ));
        assert!(preparation.stop_token().is_stopping());
        assert!(shared.writes.lock().unwrap().is_empty());
    }
}

#[test]
fn tx_pacing_uses_calibrated_sample_time_not_pulse_delayed_air_origin() {
    for calibration in [-1000, 1000] {
        let (mut preparation, mut pump, shared) =
            pair(70_000, 100_000, calibration, 30_720_000, vec![]);
        let expected = (100_000_i64 + calibration.signum() * 31) as u64;
        assert_eq!(preparation.air_origin().tick, expected + 128);
        preparation.prepare_one().unwrap().unwrap();
        pump.start().unwrap();
        assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
        assert_eq!(*shared.writes.lock().unwrap(), vec![(expected, 5120)]);
    }
}

#[test]
fn tx_pacing_commits_a_known_complete_write_without_an_empty_retry() {
    let (mut preparation, mut pump, shared) = pair(
        1000,
        2000,
        0,
        1_000_000,
        vec![Err(
            Error::new(ErrorKind::Timeout, "all samples accepted").with_accepted_samples(5120)
        )],
    );
    preparation.prepare_one().unwrap().unwrap();
    pump.start().unwrap();
    assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
    assert!(pump.pending.is_none());
    assert_eq!(shared.writes.lock().unwrap().len(), 1);
    assert!(preparation.prepare_one().unwrap().is_some());
}

#[test]
fn tx_pacing_missed_preparation_advances_once_and_continues_in_the_same_epoch() {
    let (mut preparation, mut pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    preparation.enable_timing();
    pump.enable_timing();
    pump.start().unwrap();
    shared.now.store(1901, Ordering::Release);
    assert_eq!(pump.cycle().unwrap(), PumpProgress::Idle);
    let before = preparation.source().next_chip();
    assert!(preparation.prepare_one().unwrap().is_some());
    assert_eq!(
        preparation.source().next_chip().index,
        before.index + CHIPS_PER_SLOT
    );
    assert!(matches!(
        pump.cycle().unwrap(),
        PumpProgress::Dropped {
            cause: DropCause::Preparation,
            ..
        }
    ));
    let publication = pump.dropped_publication.as_ref().unwrap();
    assert_eq!(publication.evidence.first_sample.index, 0);
    assert_eq!(publication.evidence.first_tick, 2000);
    assert!(publication.dequeued >= publication.enqueued);
    assert!(!preparation.stop_token().is_stopping());
    assert!(shared.writes.lock().unwrap().is_empty());
    assert!(preparation.prepare_one().unwrap().is_some());
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert_eq!(pump.recovery().preparation_late, 1);
    assert_eq!(shared.writes.lock().unwrap().len(), 1);
}

#[test]
fn clock_publication_is_single_owner_monotonic_and_permanently_invalidated() {
    let (reader, mut writer) = clock_channel(100).unwrap();
    assert_eq!(reader.read(), Ok(100));
    writer.observe(100).unwrap();
    writer.observe(101).unwrap();
    assert_eq!(
        writer.observe(100),
        Err(ClockFailure::Reversed {
            previous: 101,
            observed: 100
        })
    );
    assert_eq!(reader.read(), Err(ClockFailure::Unavailable));
    assert_eq!(writer.observe(102), Err(ClockFailure::Unavailable));
    let (reader, writer) = clock_channel(100).unwrap();
    drop(writer);
    assert_eq!(reader.read(), Err(ClockFailure::Unavailable));
    assert!(matches!(
        clock_channel(u64::MAX),
        Err(ClockFailure::Exhausted)
    ));
}

#[test]
fn fixed_tx_workers_repeat_start_stop_and_never_run_past_the_device_lead_horizon() {
    for _ in 0..3 {
        let (preparation, pump, shared) = pair(0, 2000, 0, 1_000_000, vec![]);
        let mut workers = TxWorkers::spawn(
            preparation,
            pump,
            Duration::from_micros(50),
            Duration::from_secs(1),
        )
        .unwrap();
        wait_while(|| !workers.is_ready());
        assert!(workers.is_ready());
        assert!(shared.writes.lock().unwrap().is_empty());
        shared.now.store(1000, Ordering::Release);
        wait_while(|| shared.writes.lock().unwrap().is_empty());
        let report = workers.join(Duration::from_secs(1)).unwrap();
        assert_eq!(
            report,
            TxWorkerReport {
                shutdown: HalStopOutcome::Succeeded,
                health: None,
                recovery: TxRecoveryCounters::default(),
                tpc_estimates: None,
                preparation_timings: None,
                submission_timings: None,

                preparation: WorkerExit::Stopped,
                submission: WorkerExit::Stopped
            }
        );
        assert!(!workers.is_ready());
        let retired = workers.take_retired_preparation().unwrap();
        assert!(retired.stop_token().is_stopping());
        assert!(workers.take_retired_preparation().is_none());
        assert_eq!(workers.join(Duration::ZERO).unwrap(), report);
        assert!(shared.stopped.load(Ordering::Acquire));
        assert!(!shared.active.load(Ordering::Acquire));
        assert_eq!(shared.writes.lock().unwrap().len(), 1);
        for &(first, samples) in shared.writes.lock().unwrap().iter() {
            assert_eq!(samples, 5120);
            assert!(first >= 1100 && first + 667 <= 3001);
        }
    }
}

#[test]
fn due_health_poll_cannot_move_a_prepared_write_past_its_deadline() {
    let (mut preparation, mut pump, shared) = pair(1850, 2000, 0, 1_000_000, vec![]);
    pump.monitor_health(TxHealthSettings {
        poll_interval: Duration::from_micros(1),
        maximum_underflows: 0,
        maximum_late_packets: 0,
        maximum_unknown_events: 0,
    })
    .unwrap();
    pump.start().unwrap();
    preparation.prepare_one().unwrap().unwrap();
    shared.health_delay_ticks.store(200, Ordering::Release);
    shared.underflows.store(1, Ordering::Release);

    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert_eq!(shared.writes.lock().unwrap()[0], (2000, 5120));
    assert_eq!(shared.health_queries.load(Ordering::Acquire), 2);
    assert_eq!(shared.health_queries_at_write.load(Ordering::Acquire), 1);
    assert_eq!(shared.now.load(Ordering::Acquire), 2050);
    assert_eq!(pump.health().unwrap().underflows, 1);
    assert_eq!(pump.recovery().health_warnings, 1);
    assert_eq!(pump.recovery().submission_late, 0);
    assert!(!pump.stop.is_stopping());
}

#[test]
fn due_health_poll_waits_for_partial_write_completion() {
    let (mut preparation, mut pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![Ok(1024)]);
    pump.monitor_health(TxHealthSettings {
        poll_interval: Duration::from_micros(1),
        maximum_underflows: 0,
        maximum_late_packets: 0,
        maximum_unknown_events: 0,
    })
    .unwrap();
    pump.start().unwrap();
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(
        pump.cycle().unwrap(),
        PumpProgress::Partial { .. }
    ));
    assert_eq!(shared.health_queries.load(Ordering::Acquire), 1);
    assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
    assert_eq!(shared.health_queries.load(Ordering::Acquire), 2);
    assert_eq!(shared.writes.lock().unwrap().len(), 2);
}

#[test]
fn repeated_underflows_and_late_events_never_stop_the_active_transmit_owner() {
    let (mut preparation, mut pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    pump.monitor_health(TxHealthSettings {
        poll_interval: Duration::from_micros(1),
        maximum_underflows: 0,
        maximum_late_packets: 0,
        maximum_unknown_events: 0,
    })
    .unwrap();
    pump.start().unwrap();
    for index in 0..4 {
        shared.now.store(1000 + index * 667, Ordering::Release);
        shared
            .underflows
            .store((index + 1) * 1000, Ordering::Release);
        shared
            .late_packets
            .store((index + 1) * 1000, Ordering::Release);
        pump.cycle().unwrap();
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        assert!(!preparation.stop_token().is_stopping());
    }
    assert_eq!(shared.activations.load(Ordering::Acquire), 1);
    assert_eq!(pump.recovery().health_warnings, 4);
    assert_eq!(pump.health().unwrap().underflows, 4000);
    assert_eq!(pump.health().unwrap().late_packets, 4000);
}

#[test]
fn real_tx_workers_catch_up_after_missed_slots_without_restart_or_activation_replay() {
    let (preparation, pump, shared) = pair(10_000, 2000, 0, 1_000_000, vec![]);
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_secs(1),
    )
    .unwrap();
    wait_while(|| shared.writes.lock().unwrap().is_empty());
    assert!(!workers.stop_token().is_stopping());
    assert_eq!(shared.activations.load(Ordering::Acquire), 1);
    assert!(
        shared
            .writes
            .lock()
            .unwrap()
            .iter()
            .all(|(tick, _)| *tick >= 10_100)
    );
    let report = workers.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.preparation, WorkerExit::Stopped);
    assert_eq!(report.submission, WorkerExit::Stopped);
    assert!(report.recovery.dropped_batches >= 12);
}

#[test]
fn timing_enabled_worker_join_preserves_snapshots_after_returning_preparation() {
    let (mut preparation, mut pump, shared) = pair(0, 2000, 0, 1_000_000, vec![]);
    preparation.enable_timing();
    pump.enable_timing();
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_secs(1),
    )
    .unwrap();
    wait_while(|| !workers.is_ready());
    shared.now.store(1000, Ordering::Release);
    wait_while(|| shared.writes.lock().unwrap().is_empty());
    let report = workers.join(Duration::from_secs(1)).unwrap();
    assert_eq!(report.preparation, WorkerExit::Stopped);
    assert_eq!(report.submission, WorkerExit::Stopped);
    let preparation = report.preparation_timings.unwrap();
    let submission = report.submission_timings.unwrap();
    assert!(preparation.synthesis.calls >= 1);
    assert!(preparation.cycle.calls >= preparation.synthesis.calls);
    assert!(preparation.published >= 1);
    assert!(submission.write.calls >= 1);
    assert!(submission.submitted >= 1);
    assert!(submission.cycle.calls >= submission.write.calls);
    assert!(preparation.synthesis.total_ns >= preparation.synthesis.maximum_ns);
    assert!(submission.write.total_ns >= submission.write.maximum_ns);
    let retired = workers.take_retired_preparation().unwrap();
    assert_eq!(retired.timings(), Some(preparation));
    drop(retired);
    assert!(workers.take_retired_preparation().is_none());
    assert_eq!(workers.join(Duration::ZERO).unwrap(), report);
}

#[test]
fn paced_preparation_services_dedicated_commands_and_returns_closed_state_after_join() {
    use crate::dedicated_tx::{TransmitCommandOutcome, TransmitRuntimeLimits};
    let (mut preparation, pump, _shared) = pair(0, 2000, 0, 1_000_000, vec![]);
    let mut ports = preparation
        .dedicated_ports(
            count(2),
            count(4),
            count(2_000_000),
            TransmitRuntimeLimits {
                command_capacity: count(2),
                commands_per_step: count(1),
                feedback_capacity: count(2),
                feedback_per_step: count(1),
                maximum_frame_payload_bits: count(10_000),
                maximum_inflight_retained_bits: count(2_000_000),
            },
        )
        .unwrap();
    let key = crate::dedicated_rx::ReceiveIdentity {
        ue: umts_types::UeKey {
            epoch: preparation.source().next_chip().epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::MIN,
    };
    let command = ports.control.observe(key).unwrap();
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(workers.take_retired_preparation().is_none());
    let mut event = None;
    wait_while(|| {
        event = ports.control.try_event();
        event.is_none()
    });
    let event = event.unwrap();
    assert_eq!(event.id, command);
    assert!(matches!(
        event.outcome,
        TransmitCommandOutcome::Observed { state: None, .. }
    ));
    workers.join(Duration::from_secs(1)).unwrap();
    assert!(!ports.control.available());
    assert!(!ports.feedback.available());
    let retired = workers.take_retired_preparation().unwrap();
    assert!(retired.stop_token().is_stopping());
    assert!(!retired.dedicated.as_ref().unwrap().available());
}

#[test]
fn tx_workers_activation_failure_cancels_the_other_owner_without_samples() {
    let (preparation, pump, shared) = pair(0, 2000, 0, 1_000_000, vec![]);
    shared.activation_error.store(true, Ordering::Release);
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_secs(1),
    )
    .unwrap();
    wait_while(|| !workers.stop_token().is_stopping());
    let report = workers.join(Duration::from_secs(1)).unwrap();
    assert!(matches!(report.submission, WorkerExit::Failed(_)));
    assert_eq!(report.preparation, WorkerExit::Stopped);
    assert!(shared.writes.lock().unwrap().is_empty());
    assert!(shared.stopped.load(Ordering::Acquire));
}

#[test]
fn tx_workers_join_deadline_keeps_both_owners_for_a_retry() {
    let (preparation, pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    shared.hold_write.store(true, Ordering::Release);
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_secs(1),
    )
    .unwrap();
    let release = ReleaseHolds(shared.clone());
    wait_while(|| !shared.write_entered.load(Ordering::Acquire));
    assert_eq!(workers.join(Duration::ZERO), Err(JoinError::Deadline));
    drop(release);
    let report = workers.join(Duration::from_secs(1)).unwrap();
    assert_eq!(
        report,
        TxWorkerReport {
            shutdown: HalStopOutcome::Succeeded,
            health: None,
            recovery: TxRecoveryCounters::default(),
            tpc_estimates: None,
            preparation_timings: None,
            submission_timings: None,

            preparation: WorkerExit::Stopped,
            submission: WorkerExit::Stopped
        }
    );
    assert!(shared.stopped.load(Ordering::Acquire));
}

#[test]
fn tx_workers_startup_watchdog_cancels_delayed_activation_before_waveform_preparation() {
    let (preparation, pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    shared.hold_activation.store(true, Ordering::Release);
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_millis(5),
    )
    .unwrap();
    let release = ReleaseHolds(shared.clone());
    wait_while(|| !workers.stop_token().is_stopping());
    drop(release);
    let report = workers.join(Duration::from_secs(1)).unwrap();
    assert!(matches!(report.preparation, WorkerExit::Failed(_)));
    assert_eq!(report.submission, WorkerExit::Stopped);
    assert!(shared.writes.lock().unwrap().is_empty());
}

#[test]
fn tx_workers_reject_mixed_pipeline_owners_and_unpaced_mode() {
    let (preparation, other_pump, first) = pair(0, 2000, 0, 1_000_000, vec![]);
    let (other_preparation, pump, second) = pair(0, 2000, 0, 1_000_000, vec![]);
    assert!(matches!(
        TxWorkers::spawn(
            preparation,
            pump,
            Duration::from_micros(50),
            Duration::from_secs(1)
        ),
        Err(TxWorkerError::MismatchedOwners)
    ));
    assert!(other_preparation.stop_token().is_stopping());
    assert!(!first.active.load(Ordering::Acquire));
    assert!(!second.active.load(Ordering::Acquire));
    drop((other_preparation, other_pump));
    let shared = Arc::new(RadioState::default());
    let tx = ClockedTx {
        shared: shared.clone(),
        calibration: 0,
        rate: 1_000_000,
        outcomes: VecDeque::new(),
    };
    let (preparation, pump) = prepare_tx(
        source(false),
        Box::new(tx),
        TxSettings::offline(2000, count(1), count(1), count(2)),
    )
    .unwrap();
    assert!(matches!(
        TxWorkers::spawn(
            preparation,
            pump,
            Duration::from_micros(50),
            Duration::from_secs(1)
        ),
        Err(TxWorkerError::NotPaced)
    ));
    assert!(!shared.active.load(Ordering::Acquire));
}

#[test]
fn tx_pacing_rejects_multiple_inner_writes_and_unavailable_startup_clock_before_activation() {
    for calls in [1, 2] {
        let shared = Arc::new(RadioState::default());
        shared.clock_error.store(true, Ordering::Release);
        let tx = ClockedTx {
            shared: shared.clone(),
            calibration: 0,
            rate: 1_000_000,
            outcomes: VecDeque::new(),
        };
        let result = prepare_tx(
            source(false),
            Box::new(tx),
            TxSettings {
                timing: TxTiming::DeviceClock(limits()),
                ..TxSettings::offline(2000, count(1), count(calls), count(2))
            },
        );
        assert!(matches!(
            result,
            Err(umts_radio::transmit::PrepareError::Driver(_)
                | umts_radio::transmit::PrepareError::Lead(_))
        ));
        assert!(!shared.active.load(Ordering::Acquire));
        assert!(shared.writes.lock().unwrap().is_empty());
    }
    let (mut preparation, mut pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(pump.cycle(), Err(PumpError::NotActive)));
    assert!(preparation.stop_token().is_stopping());
    assert!(shared.writes.lock().unwrap().is_empty());
}

#[test]
fn fixed_tx_workers_accept_control_plans_and_prove_only_the_complete_driver_interval() {
    use crate::common_tx::{CommonTxTarget, PlanStage};
    let (mut preparation, pump, shared) = pair(1000, 2000, 0, 1_000_000, vec![]);
    let mut port = preparation.control_port(count(2), count(1)).unwrap();
    let width = port.information().sib5().secondary_channels()[1].transports()[0]
        .formats
        .groups[0]
        .transport_block_bits as usize;
    let proof = port
        .publish_transport(
            crate::test_support::at(0),
            1,
            &vec![umts_dsp::bits::Bit::One; width],
            &[],
            None,
        )
        .unwrap();
    let mut workers = TxWorkers::spawn(
        preparation,
        pump,
        Duration::from_micros(50),
        Duration::from_secs(1),
    )
    .unwrap();
    for index in 0..16 {
        shared.now.store(1000 + index * 667, Ordering::Release);
        wait_while(|| shared.writes.lock().unwrap().len() <= index as usize);
        assert_eq!(proof.plan_stage(), Some(PlanStage::Committed));
        assert_eq!(proof.submitted(), index == 15);
    }
    assert_eq!(port.next_event().unwrap().stage, PlanStage::Committed);
    assert_eq!(
        workers.join(Duration::from_secs(1)).unwrap(),
        TxWorkerReport {
            shutdown: HalStopOutcome::Succeeded,
            health: None,
            recovery: TxRecoveryCounters::default(),
            tpc_estimates: None,
            preparation_timings: None,
            submission_timings: None,

            preparation: WorkerExit::Stopped,
            submission: WorkerExit::Stopped,
        }
    );
    assert!(!port.available());
    assert!(!proof.submitted());
}
