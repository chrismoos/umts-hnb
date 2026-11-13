// SPDX-License-Identifier: AGPL-3.0-or-later

use super::timing::{PreparationTimings, SubmissionTimings, WaitResolution};
use super::{
    ClockEvidence, HalStopOutcome, PreparationError, PreparationProgress, PumpError, PumpProgress,
    TxHealthSnapshot, TxPreparation, TxPump, WaitSchedule,
};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use umts_runtime::worker::{JoinError, StartGate, StopToken, Worker, WorkerExit};

const MAX_IDLE_WAIT: Duration = Duration::from_millis(1);
const FINAL_GATE_POLL_QUANTUM: Duration = Duration::from_micros(5);
const MINIMUM_BLOCKING_WAIT: Duration = Duration::from_micros(1);
const QUEUE_WAKE_MARGIN: Duration = Duration::from_micros(250);
const SPIN_HINTS_PER_CLOCK_READ: usize = 16;

#[derive(Clone, Copy)]
enum WaitPolicy {
    Blocking,
    Submission { spin: bool },
}

mod diagnostics;
use diagnostics::{WorkerTracker, measured_call};

mod pacer;
use pacer::SleepMargin;

fn wait_request(
    schedule: Option<WaitSchedule>,
    idle_wait: Duration,
    pacer: &SleepMargin,
) -> (Duration, bool) {
    let Some(schedule) = schedule else {
        return (idle_wait, false);
    };
    let early = pacer.sleep_duration(schedule.duration).min(idle_wait);
    if early.is_zero() {
        (
            schedule
                .duration
                .min(idle_wait)
                .min(FINAL_GATE_POLL_QUANTUM),
            true,
        )
    } else {
        (early, false)
    }
}

fn preparation_wait_duration(
    schedule: Option<WaitSchedule>,
    idle_wait: Duration,
    pacer: &SleepMargin,
) -> Duration {
    let Some(schedule) =
        schedule.filter(|value| value.evidence.source != super::WaitClockSource::Published)
    else {
        return idle_wait.max(MINIMUM_BLOCKING_WAIT);
    };
    let permitted = schedule.duration.min(MAX_IDLE_WAIT);
    let early = pacer.sleep_duration(permitted);
    if early >= MINIMUM_BLOCKING_WAIT {
        early
    } else {
        permitted.min(idle_wait).max(MINIMUM_BLOCKING_WAIT)
    }
}

fn wait_with_evidence<E>(
    tracker: &mut Option<WorkerTracker>,
    schedule: Option<WaitSchedule>,
    idle_wait: Duration,
    pacer: &mut SleepMargin,
    policy: WaitPolicy,
    wait: fn(Duration),
    mut clock: impl FnMut() -> Result<ClockEvidence, E>,
) -> Result<(), E> {
    let (requested, cooperative, spin_wait) = match (policy, schedule) {
        (WaitPolicy::Blocking, schedule) => (
            preparation_wait_duration(schedule, idle_wait, pacer),
            false,
            false,
        ),
        (WaitPolicy::Submission { spin: true }, None) => {
            // A fixed spin interval can delay a new publication past its deadline.
            (Duration::ZERO, false, true)
        }
        (WaitPolicy::Submission { spin: true }, Some(schedule)) => {
            let permitted = schedule.duration.min(idle_wait);
            if permitted <= FINAL_GATE_POLL_QUANTUM {
                (permitted, false, true)
            } else {
                (permitted - FINAL_GATE_POLL_QUANTUM, false, false)
            }
        }
        (WaitPolicy::Submission { spin: false }, Some(_)) => {
            let (duration, cooperative) = wait_request(schedule, idle_wait, pacer);
            (duration, cooperative, false)
        }
        _ => (
            schedule
                .map_or(idle_wait, |value| value.duration.min(idle_wait))
                .max(MINIMUM_BLOCKING_WAIT),
            false,
            false,
        ),
    };
    let pre = match (&tracker, schedule) {
        (Some(_), Some(value)) => Some(value.evidence),
        (Some(_), None) => Some(clock()?),
        (None, _) => None,
    };
    if spin_wait && requested.is_zero() && tracker.is_none() {
        std::hint::spin_loop();
        return Ok(());
    }
    let started = Instant::now();
    let recorded = match (tracker.as_mut(), pre) {
        (Some(tracker), Some(pre)) => tracker.begin_wait(pre, schedule, requested, started),
        _ => false,
    };
    if recorded && let Some(tracker) = tracker.as_mut() {
        tracker.record_policy(cooperative && !spin_wait, spin_wait);
    }
    if spin_wait {
        std::hint::spin_loop();
        while started.elapsed() < requested {
            for _ in 0..SPIN_HINTS_PER_CLOCK_READ {
                std::hint::spin_loop();
            }
        }
    } else if cooperative {
        loop {
            thread::yield_now();
            if started.elapsed() >= requested {
                break;
            }
        }
    } else if requested.is_zero() {
        thread::yield_now();
    } else {
        wait(requested);
    }
    let woke = Instant::now();
    let actual = woke.saturating_duration_since(started);
    if schedule.is_some() {
        pacer.observe(requested, actual);
    }
    let Some(tracker) = tracker else {
        return Ok(());
    };
    let post = clock();
    let sampled = Instant::now();
    tracker.record_idle(requested, actual);
    if recorded {
        tracker.record_wake(woke, actual, post.as_ref().ok().copied(), sampled);
    }
    if post.is_err() {
        tracker.finish_wait(WaitResolution::ClockFailed);
    }
    post.map(|_| ())
}

fn idle(tracker: &mut Option<WorkerTracker>, requested: Duration) -> Duration {
    if let Some(tracker) = tracker {
        let started = Instant::now();
        thread::sleep(requested);
        let actual = started.elapsed();
        tracker.record_idle(requested, actual);
        actual
    } else {
        thread::sleep(requested);
        requested
    }
}

#[derive(Debug)]
pub enum TxWorkerError {
    NotPaced,
    MismatchedOwners,
    InvalidWait,
    Spawn(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        std::io::Error,
    ),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxWorkerReport {
    pub shutdown: HalStopOutcome,
    pub preparation: WorkerExit,
    pub submission: WorkerExit,
    pub health: Option<TxHealthSnapshot>,
    pub recovery: super::TxRecoveryCounters,
    pub tpc_estimates: Option<crate::dedicated_tx::TpcEstimateCounters>,
    pub preparation_timings: Option<PreparationTimings>,
    pub submission_timings: Option<SubmissionTimings>,
}

#[must_use]
pub struct TxWorkers {
    preparation_telemetry: super::telemetry::Inbox<super::telemetry::Preparation>,
    submission_telemetry: super::telemetry::Inbox<super::telemetry::Submission>,
    shutdown: HalStopOutcome,
    health: Option<TxHealthSnapshot>,
    recovery: super::TxRecoveryCounters,
    tpc_estimates: Option<crate::dedicated_tx::TpcEstimateCounters>,
    preparation_timings: Option<PreparationTimings>,
    submission_timings: Option<SubmissionTimings>,
    stop: StopToken,
    ready: Arc<AtomicBool>,
    preparation: Worker<PreparationState>,
    submission: Worker<SubmissionState>,
    preparation_exit: Option<WorkerExit>,
    submission_exit: Option<WorkerExit>,
    retired_preparation: Option<TxPreparation>,
    retired_submission: Option<TxPump>,
}

struct SubmissionState {
    value: TxPump,
    shutdown: HalStopOutcome,
    worker: Option<WorkerTracker>,
}
impl SubmissionState {
    fn record_deadline(&mut self, failure: super::DeadlineFailure, cause: super::DropCause) {
        let Some(worker) = &mut self.worker else {
            return;
        };
        let value = &mut self.value;
        if let (Some(timings), Some(dropped)) =
            (&mut value.timings, value.dropped_publication.take())
            && dropped.evidence.first_tick == failure.first_tick
        {
            let mut evidence = dropped.evidence;
            worker.record_publication(&mut evidence, dropped.enqueued, dropped.dequeued);
            timings.last_publication = Some(evidence);
        }
        worker.record_deadline(
            failure,
            (cause == super::DropCause::Submission)
                .then(|| {
                    value
                        .sampled_clock
                        .as_ref()
                        .map(|clock| clock.accepted_sequence())
                })
                .flatten(),
            cause,
        );
    }

    fn wait(
        &mut self,
        schedule: Option<WaitSchedule>,
        idle_wait: Duration,
        pacer: &mut SleepMargin,
        spin_wait: bool,
    ) -> Result<(), PumpError> {
        let value = &mut self.value;
        wait_with_evidence(
            &mut self.worker,
            schedule,
            idle_wait,
            pacer,
            WaitPolicy::Submission { spin: spin_wait },
            thread::park_timeout,
            || value.clock_evidence(),
        )
    }
    fn cycle(&mut self) -> Result<PumpProgress, PumpError> {
        let value = &mut self.value;
        measured_call(&mut self.worker, || value.cycle())
    }
    fn submission(&mut self) -> &mut TxPump {
        &mut self.value
    }
    fn stop(&mut self) -> Result<(), String> {
        self.shutdown = HalStopOutcome::Failed("TX shutdown did not complete".to_owned());
        let result = self
            .submission()
            .stop()
            .map_err(|error| format!("TX shutdown: {error}"));
        self.shutdown = match &result {
            Ok(()) => HalStopOutcome::Succeeded,
            Err(error) => HalStopOutcome::Failed(error.clone()),
        };
        result
    }
}
impl SubmissionState {
    fn finish(&mut self) {
        if let Some(worker) = &mut self.worker {
            worker.finish_wait(WaitResolution::Stopped);
        }
        let value = &mut self.value;
        if let Some(timings) = &mut value.timings {
            timings.clock_sampling = value.sampled_clock.as_ref().map(|clock| clock.snapshot());
            if let Some(worker) = &self.worker {
                timings.worker = worker.timings;
            }
        }
    }
}

struct PreparationState {
    value: TxPreparation,
    worker: Option<WorkerTracker>,
}
impl PreparationState {
    fn wait(
        &mut self,
        schedule: Option<WaitSchedule>,
        idle_wait: Duration,
        pacer: &mut SleepMargin,
    ) -> Result<(), PreparationError> {
        let value = &mut self.value;
        wait_with_evidence(
            &mut self.worker,
            schedule,
            idle_wait,
            pacer,
            WaitPolicy::Blocking,
            thread::park_timeout,
            || value.clock_evidence(),
        )
    }
    fn wait_schedule(
        &mut self,
        until_tick: u64,
        deadline_tick: u64,
        maximum: Duration,
    ) -> Result<WaitSchedule, PreparationError> {
        self.value.wait_schedule(until_tick, deadline_tick, maximum)
    }

    fn prepare_progress(&mut self) -> Result<PreparationProgress, PreparationError> {
        let value = &mut self.value;
        measured_call(&mut self.worker, || value.prepare_progress())
    }
}
impl PreparationState {
    fn finish(&mut self) {
        self.value.source().report_dynamic_power();
        if let Some(worker) = &mut self.worker {
            worker.finish_wait(WaitResolution::Stopped);
        }
        let value = &mut self.value;
        if let Some(timings) = &mut value.timings {
            timings.clock_sampling = value.sampled_clock.as_ref().map(|clock| clock.snapshot());
            if let Some(worker) = &self.worker {
                timings.worker = worker.timings;
            }
        }
        value.close_dedicated();
    }
}

impl TxWorkers {
    /// HAL activation, clock queries and writes must have finite waits. This pair does not run control-plane work.
    #[cfg(test)]
    pub fn spawn(
        preparation: TxPreparation,
        submission: TxPump,
        idle_wait: Duration,
        startup_timeout: Duration,
    ) -> Result<Self, TxWorkerError> {
        let gate = StartGate::default();
        let workers = Self::spawn_staged(
            preparation,
            submission,
            idle_wait,
            startup_timeout,
            gate.clone(),
        )?;
        gate.release();
        Ok(workers)
    }

    pub(crate) fn spawn_staged(
        mut preparation: TxPreparation,
        submission: TxPump,
        idle_wait: Duration,
        startup_timeout: Duration,
        gate: StartGate,
    ) -> Result<Self, TxWorkerError> {
        if idle_wait.is_zero() || idle_wait > MAX_IDLE_WAIT || startup_timeout.is_zero() {
            return Err(TxWorkerError::InvalidWait);
        }
        let spin_wait = preparation.spin_wait;
        let (mut preparation_metrics, preparation_telemetry) = super::telemetry::channel();
        let (mut submission_metrics, submission_telemetry) = super::telemetry::channel();
        match (&preparation.pacing, &submission.pacing) {
            (Some(first), Some(second)) if first.observed.same_clock(&second.observed) => {}
            (Some(_), Some(_)) => return Err(TxWorkerError::MismatchedOwners),
            _ => return Err(TxWorkerError::NotPaced),
        }
        let submission_wake = Arc::new(OnceLock::new());
        preparation.submission_wake = Some(submission_wake.clone());
        let stop = preparation.stop_token();
        let ready = Arc::new(AtomicBool::new(false));
        let submission_ready = ready.clone();
        let returned_submission = SubmissionState {
            worker: submission.timings().map(|_| WorkerTracker::default()),
            value: submission,
            shutdown: HalStopOutcome::NotAttempted,
        };
        let submission = Worker::spawn_owned_gated_with_role(
            "hnb-tx-submit",
            stop.clone(),
            gate.clone(),
            umts_runtime::scheduling::Role::TxSubmit,
            Box::new(returned_submission),
            move |returned_submission, stop| {
                let run = (|| {
                    if stop.is_stopping() {
                        return Ok(());
                    }
                    submission_wake
                        .set(thread::current())
                        .map_err(|_| "TX submission wake already registered".to_owned())?;
                    returned_submission
                        .submission()
                        .start()
                        .map_err(|error| format!("TX activation: {error}"))?;
                    submission_ready.store(true, Ordering::Release);
                    let mut pacer = SleepMargin::default();
                    let mut counts = super::telemetry::Submission::default();
                    while !stop.is_stopping() {
                        let progress = returned_submission.cycle();
                        match &progress {
                            Ok(PumpProgress::Submitted(_)) => {
                                counts.submitted = counts.submitted.saturating_add(1)
                            }
                            Ok(PumpProgress::Partial { .. }) => {
                                counts.partial = counts.partial.saturating_add(1)
                            }
                            _ => (),
                        }
                        if matches!(
                            &progress,
                            Ok(PumpProgress::Submitted(_)
                                | PumpProgress::Partial { .. }
                                | PumpProgress::Dropped { .. })
                        ) {
                            submission_metrics.publish(Instant::now(), || {
                                let value = &returned_submission.value;
                                super::telemetry::Submission {
                                    health: value.health(),
                                    recovery: value.recovery(),
                                    write: value.timings.as_ref().map(|timings| timings.write),
                                    ready_queue: value
                                        .timings
                                        .as_ref()
                                        .map(|timings| timings.ready_queue),
                                    ..counts
                                }
                            });
                        }
                        match progress {
                            Ok(PumpProgress::Idle) => {
                                if let Some(worker) = &mut returned_submission.worker {
                                    worker.finish_wait(WaitResolution::Idle);
                                }
                                let urgent = spin_wait
                                    && returned_submission
                                        .submission()
                                        .urgent_queue_wait(QUEUE_WAKE_MARGIN)
                                        .map_err(|error| {
                                            format!("TX queue scheduling: {error:?}")
                                        })?;
                                returned_submission
                                    .wait(None, idle_wait, &mut pacer, urgent)
                                    .map_err(|error| format!("TX idle clock: {error:?}"))?;
                            }
                            Ok(PumpProgress::Waiting {
                                until_tick,
                                deadline_tick,
                            }) => {
                                if let Some(worker) = &mut returned_submission.worker {
                                    worker.finish_wait(WaitResolution::WaitingAgain);
                                }
                                let schedule = returned_submission
                                    .submission()
                                    .wait_schedule(until_tick, deadline_tick, Duration::MAX)
                                    .map_err(|error| format!("TX wait scheduling: {error:?}"))?;
                                returned_submission
                                    .wait(Some(schedule), idle_wait, &mut pacer, spin_wait)
                                    .map_err(|error| format!("TX wake clock: {error:?}"))?;
                            }
                            Ok(PumpProgress::Submitted(_) | PumpProgress::Partial { .. }) => {
                                if let Some(worker) = &mut returned_submission.worker {
                                    worker.finish_wait(WaitResolution::Progress);
                                }
                            }
                            Ok(PumpProgress::Dropped {
                                deadline, cause, ..
                            }) => {
                                if let Some(failure) = deadline {
                                    returned_submission.record_deadline(failure, cause);
                                } else if let Some(worker) = &mut returned_submission.worker {
                                    worker.finish_wait(WaitResolution::Progress);
                                }
                            }
                            Err(PumpError::Stopped) if stop.is_stopping() => break,
                            Err(error) => {
                                if let Some(worker) = &mut returned_submission.worker {
                                    worker.finish_wait(WaitResolution::Failed);
                                }
                                return Err(format!("TX submission: {error:?}"));
                            }
                        }
                    }
                    Ok(())
                })();
                submission_ready.store(false, Ordering::Release);
                let _shutdown = returned_submission.stop();
                run
            },
            SubmissionState::finish,
        )
        .map_err(|failure| TxWorkerError::Spawn(failure.error))?;
        let preparation_ready = ready.clone();
        let returned = PreparationState {
            worker: preparation.timings().map(|_| WorkerTracker::default()),
            value: preparation,
        };
        let preparation = Worker::spawn_owned_gated_with_role(
            "hnb-tx-prepare",
            stop.clone(),
            gate,
            umts_runtime::scheduling::Role::TxPrepare,
            Box::new(returned),
            move |returned, stop| {
                if let Some(runtime) = &returned.value.dedicated {
                    runtime
                        .register_preparation_worker()
                        .map_err(|error| format!("TX preparation wake registration: {error:?}"))?;
                }
                let startup = Instant::now();
                let mut pacer = SleepMargin::default();
                let mut prepared_batches = 0_u64;
                while !stop.is_stopping() {
                    if !preparation_ready.load(Ordering::Acquire) {
                        if startup.elapsed() >= startup_timeout {
                            return Err("TX activation deadline exceeded".to_owned());
                        }
                        idle(&mut returned.worker, idle_wait);
                        continue;
                    }
                    match returned.prepare_progress() {
                        Ok(
                            progress @ (PreparationProgress::Prepared(_)
                            | PreparationProgress::Skipped { .. }),
                        ) => {
                            prepared_batches = prepared_batches.saturating_add(1);
                            preparation_metrics.publish(Instant::now(), || {
                                super::telemetry::Preparation {
                                    prepared: prepared_batches,
                                    synthesis: returned
                                        .value
                                        .timings
                                        .as_ref()
                                        .map(|timings| timings.synthesis),
                                }
                            });
                            if let Some(worker) = &mut returned.worker {
                                match progress {
                                    PreparationProgress::Skipped { deadline, .. } => worker
                                        .record_deadline(
                                            deadline,
                                            returned
                                                .value
                                                .sampled_clock
                                                .as_ref()
                                                .map(|clock| clock.accepted_sequence()),
                                            super::DropCause::Preparation,
                                        ),
                                    _ => worker.finish_wait(WaitResolution::Progress),
                                }
                            }
                        }
                        Ok(PreparationProgress::Idle) => {
                            if let Some(worker) = &mut returned.worker {
                                worker.finish_wait(WaitResolution::Idle);
                            }
                            returned
                                .wait(None, idle_wait, &mut pacer)
                                .map_err(|error| format!("TX preparation idle clock: {error:?}"))?;
                        }
                        Ok(PreparationProgress::Waiting {
                            until_tick,
                            deadline_tick,
                            ..
                        }) => {
                            if let Some(worker) = &mut returned.worker {
                                worker.finish_wait(WaitResolution::WaitingAgain);
                            }
                            let schedule = returned
                                .wait_schedule(until_tick, deadline_tick, Duration::MAX)
                                .map_err(|error| {
                                    format!("TX preparation wait scheduling: {error:?}")
                                })?;
                            returned
                                .wait(Some(schedule), idle_wait, &mut pacer)
                                .map_err(|error| format!("TX preparation wake clock: {error:?}"))?;
                        }
                        Err(PreparationError::Stopped) if stop.is_stopping() => break,
                        Err(error) => {
                            if let Some(worker) = &mut returned.worker {
                                worker.finish_wait(WaitResolution::Failed);
                            }
                            return Err(format!("TX preparation: {error:?}"));
                        }
                    }
                }
                Ok(())
            },
            PreparationState::finish,
        )
        .map_err(|failure| TxWorkerError::Spawn(failure.error))?;
        Ok(Self {
            preparation_telemetry,
            submission_telemetry,
            shutdown: HalStopOutcome::NotAttempted,
            health: None,
            recovery: super::TxRecoveryCounters::default(),
            preparation_timings: None,
            submission_timings: None,

            stop,
            ready,
            preparation,
            submission,
            preparation_exit: None,
            submission_exit: None,
            retired_preparation: None,
            tpc_estimates: None,
            retired_submission: None,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.stop.is_stopping()
    }

    pub(crate) fn telemetry(&mut self) -> super::telemetry::Radio {
        super::telemetry::Radio {
            preparation: self.preparation_telemetry.latest(),
            submission: self.submission_telemetry.latest(),
            receive: None,
        }
    }

    #[cfg(test)]
    pub fn stop_token(&self) -> StopToken {
        self.stop.clone()
    }

    pub fn request_stop(&self) {
        self.stop.request_stop();
    }

    pub fn take_retired_preparation(&mut self) -> Option<TxPreparation> {
        self.preparation_exit.as_ref()?;
        self.retired_preparation.take()
    }

    pub(crate) fn take_retired_submission(&mut self) -> Option<TxPump> {
        self.submission_exit.as_ref()?;
        self.retired_submission.take()
    }

    /// A deadline retains both worker handles and any completed result for another join attempt.
    pub fn join(&mut self, timeout: Duration) -> Result<TxWorkerReport, JoinError> {
        self.request_stop();
        let started = Instant::now();
        if self.preparation_exit.is_none() {
            let (exit, state) = self.preparation.join_owned(timeout)?;
            self.preparation_exit = Some(exit);
            self.preparation_timings = state.value.timings();
            self.tpc_estimates = state
                .value
                .dedicated
                .as_ref()
                .map(|runtime| runtime.estimate_counters());
            self.retired_preparation = Some(state.value);
        }
        if self.submission_exit.is_none() {
            let (exit, mut state) = self
                .submission
                .join_owned(timeout.saturating_sub(started.elapsed()))?;
            self.submission_exit = Some(exit);
            self.health = state.value.health();
            self.recovery = state.value.recovery();
            self.submission_timings = state.value.timings();
            self.shutdown = state.shutdown;
            state.value.finish_capture();
            self.retired_submission = Some(state.value);
        }
        Ok(TxWorkerReport {
            shutdown: self.shutdown.clone(),
            health: self.health,
            recovery: self.recovery,
            tpc_estimates: self.tpc_estimates,
            preparation_timings: self.preparation_timings,
            submission_timings: self.submission_timings,

            preparation: self
                .preparation_exit
                .as_ref()
                .expect("joined preparation")
                .clone(),
            submission: self
                .submission_exit
                .as_ref()
                .expect("joined submission")
                .clone(),
        })
    }
}

impl Drop for TxWorkers {
    fn drop(&mut self) {
        self.request_stop();
        // Both workers must finish before either waveform or HAL owner is released.
        let preparation = self.preparation.stop_and_join();
        let submission = self.submission.stop_and_join();
        drop((preparation, submission));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        radio_io::{TxSettings, prepare_tx},
        test_support::{count, source},
    };
    use sdr_hal::{Radio, memory::MemoryRadio};
    use std::sync::Mutex;

    #[test]
    fn worker_gaps_exclude_calls_and_include_idle_time_without_double_counting() {
        let origin = Instant::now();
        let mut tracker = WorkerTracker::default();
        tracker.begin_call(origin);
        assert_eq!(tracker.timings.inter_call.calls, 0);
        tracker.end_call(origin + Duration::from_nanos(10));
        tracker.record_idle(Duration::from_nanos(4), Duration::from_nanos(7));
        tracker.begin_call(origin + Duration::from_nanos(19));
        tracker.end_call(origin + Duration::from_nanos(100));
        tracker.begin_call(origin + Duration::from_nanos(102));
        assert_eq!(tracker.timings.inter_call.calls, 2);
        assert_eq!(tracker.timings.inter_call.total_ns, 11);
        assert_eq!(tracker.timings.inter_call.maximum_ns, 9);
        assert_eq!(tracker.timings.idle_wait.calls, 1);
        assert_eq!(tracker.timings.idle_wait.total_ns, 7);
        assert_eq!(tracker.timings.idle_overshoot.total_ns, 3);
    }

    #[test]
    fn worker_idle_overshoot_saturates_for_early_and_zero_waits() {
        let mut tracker = WorkerTracker::default();
        tracker.record_idle(Duration::from_nanos(10), Duration::from_nanos(3));
        tracker.record_idle(Duration::ZERO, Duration::ZERO);
        tracker.record_idle(Duration::from_nanos(10), Duration::from_nanos(12));
        assert_eq!(tracker.timings.idle_wait.calls, 3);
        assert_eq!(tracker.timings.idle_wait.total_ns, 15);
        assert_eq!(tracker.timings.idle_wait.maximum_ns, 12);
        assert_eq!(tracker.timings.idle_overshoot.calls, 3);
        assert_eq!(tracker.timings.idle_overshoot.total_ns, 2);
        assert_eq!(tracker.timings.idle_overshoot.maximum_ns, 2);
    }

    #[test]
    fn disabled_worker_measurement_preserves_success_and_error_results() {
        let mut tracker = None;
        assert_eq!(measured_call(&mut tracker, || Ok::<_, u8>(7)), Ok(7));
        assert_eq!(measured_call(&mut tracker, || Err::<u8, _>(9)), Err(9));
        assert!(tracker.is_none());
    }

    use super::diagnostics::sample_evidence;

    #[test]
    fn wake_snapshot_and_deadline_keep_distinct_identity_and_observations() {
        let mut tracker = WorkerTracker::default();
        let pre = sample_evidence();
        let started = Instant::now();
        let duration = Duration::from_micros(5);
        let schedule = WaitSchedule {
            duration,
            target_tick: 120,
            deadline_tick: 130,
            evidence: pre,
        };
        assert!(tracker.begin_wait(pre, Some(schedule), duration, started));
        let post = ClockEvidence {
            bounds: umts_radio::clock_model::ClockBounds {
                earliest: 120,
                latest: 130,
            },
            model_sequence: Some(8),
            ..pre
        };
        tracker.record_wake(
            started + Duration::from_micros(750),
            Duration::from_micros(750),
            Some(post),
            started + Duration::from_micros(751),
        );
        let active = tracker.timings.active_wait.unwrap();
        assert_eq!(active.wait_id, 1);
        assert_eq!(active.pre_model_sequence, Some(7));
        assert_eq!(active.post_model_sequence, Some(8));
        assert_eq!(active.woke_host_ns, Some(750_000));
        assert_eq!(active.post_sample_finished_host_ns, Some(751_000));
        assert_eq!(tracker.timings.last_wait, None);
        let failure = super::super::DeadlineFailure {
            device_tick: 140,
            first_tick: 130,
            shortfall_ticks: 10,
        };
        tracker.record_deadline(failure, Some(9), super::super::DropCause::Submission);
        let evidence = tracker.timings.last_wait.unwrap();
        assert_eq!(evidence.target_tick, Some(120));
        assert_eq!(evidence.wait_id, active.wait_id);
        assert_eq!((evidence.pre_earliest, evidence.pre_latest), (100, 110));
        assert_eq!(
            (evidence.requested_ns, evidence.actual_ns),
            (5_000, 750_000)
        );
        assert_eq!(evidence.post_earliest, Some(120));
        assert_eq!(evidence.post_latest, Some(130));
        assert_eq!(evidence.deadline_device_tick, Some(140));
        assert_eq!(evidence.deadline_first_tick, Some(130));
        assert_eq!(evidence.deadline_model_sequence, Some(9));
        assert_eq!(
            evidence.deadline_origin,
            Some(super::super::DropCause::Submission)
        );
        assert_eq!(evidence.resolution, Some(WaitResolution::Deadline));
        assert_eq!(evidence.next_deadline_shortfall_ticks, Some(10));
        tracker.record_deadline(
            super::super::DeadlineFailure {
                shortfall_ticks: 20,
                ..failure
            },
            Some(10),
            super::super::DropCause::Submission,
        );
        assert_eq!(tracker.timings.last_wait, Some(evidence));
        tracker.finish_wait(WaitResolution::Progress);
        tracker.record_deadline(failure, None, super::super::DropCause::Preparation);
        assert_eq!(tracker.timings.last_wait, Some(evidence));
        assert_eq!(tracker.timings.wait_history.count, 1);
        assert_eq!(tracker.timings.unpaired_deadlines, 2);
        assert_eq!(tracker.timings.last_deadline, Some(failure));
        assert_eq!(
            tracker.timings.last_deadline_origin,
            Some(super::super::DropCause::Preparation)
        );
        for _ in 0..super::super::WAIT_HISTORY_CAPACITY + 1 {
            assert!(tracker.begin_wait(pre, None, duration, started));
            tracker.finish_wait(WaitResolution::Progress);
        }
        assert_eq!(tracker.timings.deadline_wait_history.count, 1);
        assert_eq!(
            tracker.timings.deadline_wait_history.records[0],
            Some(evidence)
        );
    }

    #[test]
    fn published_wait_evidence_does_not_claim_a_fresh_clock() {
        let mut tracker = WorkerTracker::default();
        let pre = ClockEvidence {
            source: super::super::WaitClockSource::Published,
            model_sequence: None,
            ..sample_evidence()
        };
        assert!(tracker.begin_wait(pre, None, Duration::ZERO, Instant::now()));
        let evidence = tracker.timings.active_wait.unwrap();
        assert_eq!(
            evidence.clock_source,
            super::super::WaitClockSource::Published
        );
        assert_eq!(evidence.post_latest, None);
        assert_eq!(evidence.next_deadline_shortfall_ticks, None);
        assert_eq!(evidence.pre_model_sequence, None);
        tracker.finish_wait(WaitResolution::Stopped);
        assert!(tracker.timings.last_wait.is_some());
        assert!(tracker.timings.active_wait.is_none());
    }

    #[test]
    fn unresolved_wait_cannot_be_overwritten_and_resolved_history_survives_progress() {
        let mut tracker = WorkerTracker::default();
        let pre = sample_evidence();
        let now = Instant::now();
        assert!(tracker.begin_wait(pre, None, Duration::ZERO, now));
        let active = tracker.timings.active_wait;
        assert!(!tracker.begin_wait(pre, None, Duration::ZERO, now));
        assert_eq!(tracker.timings.active_wait, active);
        tracker.finish_wait(WaitResolution::Progress);
        let resolved = tracker.timings.last_wait;
        tracker.begin_call(now);
        tracker.end_call(now);
        tracker.finish_wait(WaitResolution::Progress);
        assert_eq!(tracker.timings.last_wait, resolved);
        assert!(tracker.begin_wait(pre, None, Duration::ZERO, now));
        assert_eq!(tracker.timings.active_wait.unwrap().wait_id, 2);
        assert_eq!(tracker.timings.last_wait, resolved);
        tracker.finish_wait(WaitResolution::Idle);
        assert_eq!(tracker.timings.wait_history.count, 2);
    }

    #[test]
    fn early_wake_applies_margin_before_poll_cap_and_bounds_final_gate_polling() {
        let mut schedule = WaitSchedule {
            duration: Duration::from_millis(1),
            target_tick: 120,
            deadline_tick: 130,
            evidence: sample_evidence(),
        };
        let pacer = SleepMargin::default();
        let cap = Duration::from_micros(100);
        assert_eq!(wait_request(Some(schedule), cap, &pacer), (cap, false));
        schedule.duration = Duration::from_micros(30);
        assert_eq!(
            wait_request(Some(schedule), cap, &pacer),
            (FINAL_GATE_POLL_QUANTUM, true)
        );
        schedule.duration = Duration::from_micros(1);
        assert_eq!(
            wait_request(Some(schedule), cap, &pacer),
            (schedule.duration, true)
        );
        assert_eq!(wait_request(None, cap, &pacer), (cap, false));
    }

    #[test]
    fn wake_failure_is_explicit_and_disabled_diagnostics_perform_no_clock_read() {
        let schedule = WaitSchedule {
            duration: Duration::ZERO,
            target_tick: 120,
            deadline_tick: 130,
            evidence: sample_evidence(),
        };
        let mut disabled = None;
        assert_eq!(
            wait_with_evidence(
                &mut disabled,
                Some(schedule),
                Duration::ZERO,
                &mut SleepMargin::default(),
                WaitPolicy::Blocking,
                thread::sleep,
                || Err::<ClockEvidence, _>(9)
            ),
            Ok(())
        );
        let mut enabled = Some(WorkerTracker::default());
        assert_eq!(
            wait_with_evidence(
                &mut enabled,
                Some(schedule),
                Duration::ZERO,
                &mut SleepMargin::default(),
                WaitPolicy::Blocking,
                thread::sleep,
                || Err::<ClockEvidence, _>(9)
            ),
            Err(9)
        );
        let timings = &enabled.unwrap().timings;
        let record = timings.last_wait.unwrap();
        assert_eq!(record.resolution, Some(WaitResolution::ClockFailed));
        assert!(record.woke_host_ns.is_some());
        assert_eq!(record.post_latest, None);
        assert_eq!(record.post_model_sequence, None);
        assert!(timings.active_wait.is_none());
    }

    fn panic_wait(_: Duration) {
        panic!("OS wait callback must not run in spin mode");
    }

    static NORMAL_WAIT_CALLED: AtomicBool = AtomicBool::new(false);

    fn recording_wait(_: Duration) {
        NORMAL_WAIT_CALLED.store(true, Ordering::Release);
    }

    #[test]
    fn normal_wait_mode_keeps_using_the_wait_callback() {
        NORMAL_WAIT_CALLED.store(false, Ordering::Release);
        let mut tracker = None;
        let result = wait_with_evidence(
            &mut tracker,
            None,
            Duration::from_micros(1),
            &mut SleepMargin::default(),
            WaitPolicy::Blocking,
            recording_wait,
            || Ok::<_, ()>(sample_evidence()),
        );
        assert_eq!(result, Ok(()));
        assert!(NORMAL_WAIT_CALLED.load(Ordering::Acquire));
    }

    #[test]
    fn submission_idle_blocks_when_not_near_the_publication_deadline() {
        let mut tracker = Some(WorkerTracker::default());
        let mut reads = 0;
        let result = wait_with_evidence(
            &mut tracker,
            None,
            Duration::ZERO,
            &mut SleepMargin::default(),
            WaitPolicy::Submission { spin: false },
            |duration| assert!(duration >= MINIMUM_BLOCKING_WAIT),
            || {
                reads += 1;
                Ok::<_, ()>(sample_evidence())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(reads, 2);
        let timings = tracker.unwrap().timings;
        assert!(timings.last_wait.is_none());
        let evidence = timings.active_wait.unwrap();
        assert!(!evidence.spinning);
        assert!(!evidence.cooperative);
        assert_eq!(evidence.resolution, None);
    }

    #[test]
    fn urgent_submission_idle_polls_without_parking() {
        let mut tracker = Some(WorkerTracker::default());
        wait_with_evidence(
            &mut tracker,
            None,
            Duration::from_micros(100),
            &mut SleepMargin::default(),
            WaitPolicy::Submission { spin: true },
            |_| panic!("urgent publication must not park"),
            || Ok::<_, ()>(sample_evidence()),
        )
        .unwrap();
        let evidence = tracker.unwrap().timings.active_wait.unwrap();
        assert!(evidence.spinning);
        assert_eq!(evidence.requested_ns, 0);
    }

    #[test]
    fn uninstrumented_urgent_idle_does_not_sample_another_clock() {
        wait_with_evidence(
            &mut None,
            None,
            Duration::from_micros(100),
            &mut SleepMargin::default(),
            WaitPolicy::Submission { spin: true },
            |_| panic!("urgent publication must not park"),
            || -> Result<ClockEvidence, ()> { panic!("the next cycle refreshes the clock") },
        )
        .unwrap();
    }

    #[test]
    fn preparation_target_wait_blocks_instead_of_final_gate_polling() {
        for duration in [
            Duration::ZERO,
            Duration::from_micros(1),
            Duration::from_micros(30),
        ] {
            let mut tracker = Some(WorkerTracker::default());
            wait_with_evidence(
                &mut tracker,
                Some(WaitSchedule {
                    duration,
                    target_tick: 120,
                    deadline_tick: 130,
                    evidence: sample_evidence(),
                }),
                Duration::from_micros(100),
                &mut SleepMargin::default(),
                WaitPolicy::Blocking,
                |duration| assert!(duration >= MINIMUM_BLOCKING_WAIT),
                || Ok::<_, ()>(sample_evidence()),
            )
            .unwrap();
            let evidence = tracker.unwrap().timings.active_wait.unwrap();
            assert!(!evidence.spinning);
            assert!(!evidence.cooperative);
            assert_eq!(
                evidence.requested_ns,
                duration.max(MINIMUM_BLOCKING_WAIT).as_nanos()
            );
        }
    }

    #[test]
    fn preparation_uses_fresh_target_but_bounds_stale_publication_and_idle_polling() {
        let idle = Duration::from_micros(100);
        let pacer = SleepMargin::default();
        let mut schedule = WaitSchedule {
            duration: Duration::from_micros(600),
            target_tick: 120,
            deadline_tick: 130,
            evidence: sample_evidence(),
        };
        assert_eq!(
            preparation_wait_duration(Some(schedule), idle, &pacer),
            Duration::from_micros(550)
        );
        schedule.duration = Duration::from_secs(1);
        assert_eq!(
            preparation_wait_duration(Some(schedule), idle, &pacer),
            MAX_IDLE_WAIT - pacer.margin()
        );
        schedule.evidence.source = super::super::WaitClockSource::Published;
        assert_eq!(
            preparation_wait_duration(Some(schedule), idle, &pacer),
            idle
        );
        assert_eq!(preparation_wait_duration(None, idle, &pacer), idle);
    }

    #[test]
    fn submission_blocks_outside_final_spin_quantum() {
        for duration in [Duration::from_micros(21), Duration::from_micros(100)] {
            let mut tracker = Some(WorkerTracker::default());
            wait_with_evidence(
                &mut tracker,
                Some(WaitSchedule {
                    duration,
                    target_tick: 120,
                    deadline_tick: 130,
                    evidence: sample_evidence(),
                }),
                Duration::from_micros(100),
                &mut SleepMargin::default(),
                WaitPolicy::Submission { spin: true },
                |duration| assert!(!duration.is_zero()),
                || Ok::<_, ()>(sample_evidence()),
            )
            .unwrap();
            let evidence = tracker.unwrap().timings.active_wait.unwrap();
            assert!(!evidence.spinning);
            assert!(!evidence.cooperative);
            assert_eq!(
                evidence.requested_ns,
                (duration - FINAL_GATE_POLL_QUANTUM).as_nanos()
            );
        }
    }

    #[test]
    fn spin_wait_skips_os_wait_for_target_and_rereads_after_poll() {
        let schedule = WaitSchedule {
            duration: Duration::from_micros(1),
            target_tick: 120,
            deadline_tick: 130,
            evidence: sample_evidence(),
        };
        let mut tracker = Some(WorkerTracker::default());
        let mut reads = 0;
        let result = wait_with_evidence(
            &mut tracker,
            Some(schedule),
            Duration::from_micros(20),
            &mut SleepMargin::default(),
            WaitPolicy::Submission { spin: true },
            panic_wait,
            || {
                reads += 1;
                Ok::<_, ()>(sample_evidence())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(reads, 1);
        let timings = tracker.unwrap().timings;
        assert!(timings.last_wait.is_none());
        let evidence = timings.active_wait.unwrap();
        assert!(evidence.spinning);
        assert!(!evidence.cooperative);
        assert_eq!(evidence.resolution, None);
    }

    struct DropProbe(Arc<Mutex<Option<thread::ThreadId>>>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(thread::current().id());
        }
    }

    fn submission() -> TxPump {
        let (mut radio, _peer) = MemoryRadio::new(1);
        radio.set_tx_sample_rate(7_680_000.0).unwrap();
        let (tx, _) = Box::new(radio).split().unwrap();
        let (_preparation, pump) = prepare_tx(
            source(false),
            tx,
            TxSettings::offline(1000, count(1), count(1), count(3)),
        )
        .unwrap();
        pump
    }

    struct TrackedTx {
        _drop_probe: DropProbe,
        fail_stop: bool,
        activated: Arc<AtomicBool>,
    }
    impl sdr_hal::RadioTx for TrackedTx {
        fn supports_timed_tx(&self) -> bool {
            true
        }
        fn tx_sample_rate_hz(&self) -> f64 {
            7_680_000.0
        }
        fn tick_rate(&self) -> u64 {
            1_000_000
        }
        fn get_hardware_time(&self) -> Result<u64, sdr_hal::error::Error> {
            Ok(0)
        }
        fn transmit(&mut self, _: &[num_complex::Complex32]) -> Result<(), sdr_hal::error::Error> {
            Ok(())
        }
        fn enable_transmit(&mut self, active: bool) -> Result<(), sdr_hal::error::Error> {
            if active {
                self.activated.store(true, Ordering::Release);
            }
            if !active && self.fail_stop {
                Err(sdr_hal::error::Error::invalid_setting(
                    "injected shutdown failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn tracked_submission(fail_stop: bool) -> (TxPump, Arc<Mutex<Option<thread::ThreadId>>>) {
        let observed = Arc::new(Mutex::new(None));
        let tx = Box::new(TrackedTx {
            _drop_probe: DropProbe(observed.clone()),
            fail_stop,
            activated: Arc::new(AtomicBool::new(false)),
        });
        let (_preparation, pump) = prepare_tx(
            source(false),
            tx,
            TxSettings::offline(1000, count(1), count(1), count(3)),
        )
        .unwrap();
        (pump, observed)
    }

    #[test]
    fn staged_tx_cancellation_returns_both_original_owners_without_hal_activation() {
        let observed = Arc::new(Mutex::new(None));
        let activated = Arc::new(AtomicBool::new(false));
        let tx = Box::new(TrackedTx {
            _drop_probe: DropProbe(observed.clone()),
            fail_stop: false,
            activated: activated.clone(),
        });
        let (preparation, submission) = prepare_tx(
            source(false),
            tx,
            TxSettings {
                timing: crate::radio_io::TxTiming::DeviceClock(umts_radio::pacing::LeadDurations {
                    minimum_submission: Duration::from_micros(100),
                    maximum_submission: Duration::from_millis(2),
                    maximum_preparation: Duration::from_millis(4),
                }),
                ..TxSettings::offline(10_000, count(2), count(1), count(3))
            },
        )
        .unwrap();
        let gate = StartGate::default();
        let mut workers = TxWorkers::spawn_staged(
            preparation,
            submission,
            Duration::from_micros(100),
            Duration::from_secs(1),
            gate.clone(),
        )
        .unwrap();
        assert!(!workers.is_ready());
        assert!(!activated.load(Ordering::Acquire));
        assert!(workers.take_retired_preparation().is_none());
        assert!(workers.take_retired_submission().is_none());
        let report = workers.join(Duration::from_secs(1)).unwrap();
        assert_eq!(report.preparation, WorkerExit::Stopped);
        assert_eq!(report.submission, WorkerExit::Stopped);
        assert_eq!(report.shutdown, HalStopOutcome::NotAttempted);
        gate.release();
        assert!(!activated.load(Ordering::Acquire));
        let preparation = workers.take_retired_preparation().unwrap();
        let submission = workers.take_retired_submission().unwrap();
        assert!(workers.take_retired_preparation().is_none());
        assert!(workers.take_retired_submission().is_none());
        assert!(observed.lock().unwrap().is_none());
        assert_eq!(workers.join(Duration::ZERO).unwrap(), report);
        drop(submission);
        drop(preparation);
        assert_eq!(*observed.lock().unwrap(), Some(thread::current().id()));
    }

    #[test]
    fn group_drop_keeps_preparation_owned_until_submission_finishes() {
        struct PreparationDrop {
            submission_finished: Arc<AtomicBool>,
            observed: Arc<AtomicBool>,
        }
        impl Drop for PreparationDrop {
            fn drop(&mut self) {
                self.observed.store(
                    self.submission_finished.load(Ordering::Acquire),
                    Ordering::Release,
                );
            }
        }
        let finished = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(AtomicBool::new(false));
        let (mut radio, _peer) = MemoryRadio::new(1);
        radio.set_tx_sample_rate(7_680_000.0).unwrap();
        let (tx, _) = Box::new(radio).split().unwrap();
        let (mut preparation, submission) = prepare_tx(
            source(false),
            tx,
            TxSettings {
                timing: crate::radio_io::TxTiming::DeviceClock(umts_radio::pacing::LeadDurations {
                    minimum_submission: Duration::from_micros(100),
                    maximum_submission: Duration::from_millis(2),
                    maximum_preparation: Duration::from_millis(4),
                }),
                ..TxSettings::offline(10_000, count(2), count(1), count(3))
            },
        )
        .unwrap();
        preparation.retirement_probe = Some(Box::new(PreparationDrop {
            submission_finished: finished.clone(),
            observed: observed.clone(),
        }));
        let mut workers = TxWorkers::spawn_staged(
            preparation,
            submission,
            Duration::from_micros(100),
            Duration::from_secs(1),
            StartGate::default(),
        )
        .unwrap();
        let (_, submission) = workers.submission.stop_and_join().unwrap();
        let gate = StartGate::default();
        gate.release();
        let (entered, running) = std::sync::mpsc::sync_channel(1);
        workers.submission = Worker::spawn_owned_gated_with_role(
            "submission-drop-order",
            StopToken::default(),
            gate,
            umts_runtime::scheduling::Role::Background,
            submission,
            move |_, stop| {
                entered.send(()).unwrap();
                while !stop.wait_or_stop(Duration::from_secs(1)) {}
                finished.store(true, Ordering::Release);
                Ok(())
            },
            SubmissionState::finish,
        )
        .unwrap();
        running.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(workers);
        assert!(observed.load(Ordering::Acquire));
    }

    fn state_worker<T: Send + 'static>(
        state: T,
        run: impl FnOnce(&mut T, StopToken) -> Result<(), String> + Send + 'static,
        finish: fn(&mut T),
    ) -> Worker<T> {
        let gate = StartGate::default();
        gate.release();
        Worker::spawn_owned_gated_with_role(
            "owned-radio-test",
            StopToken::default(),
            gate,
            umts_runtime::scheduling::Role::Background,
            Box::new(state),
            run,
            finish,
        )
        .unwrap()
    }

    #[test]
    fn submission_returns_actual_owner_on_success_failure_and_panic() {
        for mode in 0..3 {
            let (mut pump, observed) = tracked_submission(false);
            pump.timings = Some(SubmissionTimings::default());
            let mut tracker = WorkerTracker::default();
            tracker.record_idle(Duration::from_nanos(2), Duration::from_nanos(5));
            let state = SubmissionState {
                worker: Some(tracker),
                value: pump,
                shutdown: HalStopOutcome::NotAttempted,
            };
            let mut worker = state_worker(
                state,
                move |state, _| {
                    assert_eq!(state.value.maximum_attempts, count(3));
                    if mode == 2 {
                        panic!("injected submission panic");
                    }
                    let result = if mode == 1 {
                        Err("injected run failure".to_owned())
                    } else {
                        Ok(())
                    };
                    let _ = state.stop();
                    result
                },
                SubmissionState::finish,
            );
            let (exit, retired) = worker.join_owned(Duration::from_secs(1)).unwrap();
            assert!(observed.lock().unwrap().is_none());
            assert_eq!(retired.value.maximum_attempts, count(3));
            assert_eq!(
                retired.value.timings().unwrap().worker.idle_wait.total_ns,
                5
            );
            assert_eq!(
                retired
                    .value
                    .timings()
                    .unwrap()
                    .worker
                    .idle_overshoot
                    .total_ns,
                3
            );
            assert_eq!(
                retired.shutdown,
                if mode == 2 {
                    HalStopOutcome::NotAttempted
                } else {
                    HalStopOutcome::Succeeded
                }
            );
            assert_eq!(
                exit,
                match mode {
                    0 => WorkerExit::Stopped,
                    1 => WorkerExit::Failed("injected run failure".to_owned()),
                    _ => WorkerExit::Panicked,
                }
            );
            assert!(matches!(
                worker.join_owned(Duration::ZERO),
                Err(JoinError::AlreadyJoined)
            ));
            drop(retired);
            assert_eq!(*observed.lock().unwrap(), Some(thread::current().id()));
        }
    }

    #[test]
    fn failed_hal_stop_is_retained_independently_of_a_successful_run() {
        let (pump, observed) = tracked_submission(true);
        let state = SubmissionState {
            worker: None,
            value: pump,
            shutdown: HalStopOutcome::NotAttempted,
        };
        let mut worker = state_worker(
            state,
            |state, _| {
                assert!(state.stop().is_err());
                Ok(())
            },
            SubmissionState::finish,
        );
        let (exit, retired) = worker.join_owned(Duration::from_secs(1)).unwrap();
        assert_eq!(exit, WorkerExit::Stopped);
        assert!(observed.lock().unwrap().is_none());
        assert!(
            matches!(&retired.shutdown, HalStopOutcome::Failed(error) if error.contains("injected shutdown failure"))
        );
        drop(retired);
        assert_eq!(*observed.lock().unwrap(), Some(thread::current().id()));
    }

    #[test]
    fn submission_join_deadline_retains_handle_and_owner_until_the_worker_returns() {
        let (entered_send, entered_receive) = std::sync::mpsc::sync_channel(1);
        let (release_send, release_receive) = std::sync::mpsc::sync_channel(1);
        let state = SubmissionState {
            worker: None,
            value: submission(),
            shutdown: HalStopOutcome::NotAttempted,
        };
        let mut worker = state_worker(
            state,
            move |state, _| {
                entered_send.send(()).unwrap();
                release_receive
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap();
                state.stop()
            },
            SubmissionState::finish,
        );
        entered_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(matches!(
            worker.join_owned(Duration::ZERO),
            Err(JoinError::Deadline)
        ));
        release_send.send(()).unwrap();
        let (exit, retired) = worker.join_owned(Duration::from_secs(1)).unwrap();
        assert_eq!(exit, WorkerExit::Stopped);
        assert_eq!(retired.shutdown, HalStopOutcome::Succeeded);
        assert_eq!(retired.value.maximum_attempts, count(3));
    }

    #[test]
    fn preparation_returns_heavy_state_to_joining_thread_on_success_and_panic() {
        for panic in [false, true] {
            let (mut radio, _peer) = MemoryRadio::new(1);
            radio.set_tx_sample_rate(7_680_000.0).unwrap();
            let (tx, _) = Box::new(radio).split().unwrap();
            let (mut preparation, _pump) = prepare_tx(
                source(false),
                tx,
                TxSettings::offline(1000, count(1), count(1), count(2)),
            )
            .unwrap();
            let observed = Arc::new(Mutex::new(None));
            preparation.retirement_probe = Some(Box::new(DropProbe(observed.clone())));
            preparation.timings = Some(PreparationTimings::default());
            let mut tracker = WorkerTracker::default();
            tracker.record_idle(Duration::from_nanos(2), Duration::from_nanos(5));
            let state = PreparationState {
                worker: Some(tracker),
                value: preparation,
            };
            let mut worker = state_worker(
                state,
                move |state, _| {
                    let _ = state.value.source().next_chip();
                    if panic {
                        panic!("injected preparation panic");
                    }
                    Ok(())
                },
                PreparationState::finish,
            );
            let (exit, retired) = worker.join_owned(Duration::from_secs(1)).unwrap();
            assert_eq!(
                exit,
                if panic {
                    WorkerExit::Panicked
                } else {
                    WorkerExit::Stopped
                }
            );
            assert!(observed.lock().unwrap().is_none());
            assert_eq!(
                retired.value.timings().unwrap().worker.idle_wait.total_ns,
                5
            );
            assert_eq!(
                retired
                    .value
                    .timings()
                    .unwrap()
                    .worker
                    .idle_overshoot
                    .total_ns,
                3
            );
            drop(retired);
            assert_eq!(*observed.lock().unwrap(), Some(thread::current().id()));
        }
    }
}
