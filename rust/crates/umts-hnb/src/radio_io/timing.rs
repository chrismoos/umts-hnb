// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{TxHealthSnapshot, WaitClockSource};
use std::time::Duration;
use umts_radio::clock_model::ClockBounds;
use umts_types::time::SampleInstant;

#[derive(Clone, Copy, Debug, Default)]
pub struct RxReadTiming {
    pub radio_read_ns: u128,
    pub coordinate_capture_ns: u128,
    pub filter_ns: u128,
    pub decimation_ns: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOffset {
    BeforeNs(u128),
    AtOrAfterNs(u128),
}

impl HostOffset {
    pub(crate) fn between(value: std::time::Instant, reference: std::time::Instant) -> Self {
        match value.checked_duration_since(reference) {
            Some(delta) => Self::AtOrAfterNs(delta.as_nanos()),
            None => Self::BeforeNs(reference.duration_since(value).as_nanos()),
        }
    }
}

/// Enqueue starts before publication. Successful enqueue precedes the producer's unpark call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationEvidence {
    pub first_sample: SampleInstant,
    pub first_tick: u64,
    pub deadline_tick: Option<u64>,
    pub enqueue_started_to_dequeue_ns: u128,
    pub enqueue_started_from_wait_start: Option<HostOffset>,
    pub dequeued_from_wait_wake: Option<HostOffset>,
}

/// Offsets share the slot's enqueue-start Instant. Enqueue returns after publication, not atomically with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationCompletionEvidence {
    pub first_sample: SampleInstant,
    pub next_sample: SampleInstant,
    pub enqueue_returned_ns: u128,
    pub unpark_started_ns: Option<u128>,
    pub unpark_returned_ns: Option<u128>,
    pub wake_registered: bool,
}

pub const PUBLICATION_HISTORY_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicationCompletionHistory {
    pub records: [Option<PublicationCompletionEvidence>; PUBLICATION_HISTORY_CAPACITY],
    pub count: usize,
    pub next_index: usize,
    pub overwritten: u64,
}

impl PublicationCompletionHistory {
    pub(crate) fn push(&mut self, evidence: PublicationCompletionEvidence) {
        self.records[self.next_index] = Some(evidence);
        self.next_index = (self.next_index + 1) % PUBLICATION_HISTORY_CAPACITY;
        if self.count < PUBLICATION_HISTORY_CAPACITY {
            self.count += 1;
        } else {
            self.overwritten = self.overwritten.saturating_add(1);
        }
    }
}

/// Elapsed wall time includes waits and scheduling delays, not just CPU execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WallTimeSummary {
    pub calls: u64,
    pub total_ns: u128,
    pub maximum_ns: u128,
}

impl WallTimeSummary {
    pub fn record(&mut self, duration: Duration) {
        let nanoseconds = duration.as_nanos();
        self.calls = self.calls.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(nanoseconds);
        self.maximum_ns = self.maximum_ns.max(nanoseconds);
    }
}

/// Inter-call gaps include idle waits, so these summaries must not be added together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerTimings {
    pub inter_call: WallTimeSummary,
    pub idle_wait: WallTimeSummary,
    pub idle_overshoot: WallTimeSummary,
    pub last_wait: Option<WaitEvidence>,
    pub active_wait: Option<WaitEvidence>,
    pub wait_history: WaitHistory,
    pub deadline_wait_history: WaitHistory,
    pub unpaired_deadlines: u64,
    pub last_deadline: Option<super::DeadlineFailure>,
    pub last_deadline_origin: Option<super::DropCause>,
    pub wait_ids_exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    Idle,
    Target,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitResolution {
    WaitingAgain,
    Progress,
    Idle,
    Deadline,
    Stopped,
    ClockFailed,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Host timestamps share one worker-local origin, not the radio tick domain.
pub struct WaitEvidence {
    pub wait_id: u64,
    pub kind: WaitKind,
    pub target_tick: Option<u64>,
    pub deadline_tick: Option<u64>,
    pub pre_earliest: u64,
    pub pre_latest: u64,
    pub pre_model_sequence: Option<u64>,
    pub clock_source: WaitClockSource,
    pub started_host_ns: u128,
    pub requested_ns: u128,
    pub cooperative: bool,
    pub spinning: bool,
    pub actual_ns: u128,
    pub woke_host_ns: Option<u128>,
    pub post_sample_finished_host_ns: Option<u128>,
    pub post_earliest: Option<u64>,
    pub post_latest: Option<u64>,
    pub post_model_sequence: Option<u64>,
    pub post_clock_source: Option<WaitClockSource>,
    pub deadline_device_tick: Option<u64>,
    pub deadline_first_tick: Option<u64>,
    pub deadline_model_sequence: Option<u64>,
    pub deadline_origin: Option<super::DropCause>,
    pub next_deadline_shortfall_ticks: Option<u128>,
    pub resolution: Option<WaitResolution>,
    pub publication: Option<PublicationEvidence>,
}

pub const WAIT_HISTORY_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WaitHistory {
    pub records: [Option<WaitEvidence>; WAIT_HISTORY_CAPACITY],
    pub count: usize,
    pub next_index: usize,
    pub overwritten: u64,
}

impl WaitHistory {
    pub(crate) fn push(&mut self, evidence: WaitEvidence) {
        self.records[self.next_index] = Some(evidence);
        self.next_index = (self.next_index + 1) % WAIT_HISTORY_CAPACITY;
        if self.count < WAIT_HISTORY_CAPACITY {
            self.count += 1;
        } else {
            self.overwritten = self.overwritten.saturating_add(1);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PreparationTimings {
    pub clock_sampling: Option<super::ClockSamplingSnapshot>,
    pub worker: WorkerTimings,
    pub cycle: WallTimeSummary,
    pub synthesis: WallTimeSummary,
    pub published: u64,
    pub publication_completions: PublicationCompletionHistory,
    pub amplitude_failures: AmplitudeFailureHistory,
}

const AMPLITUDE_FAILURE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmplitudeFailure {
    pub first_sample: SampleInstant,
    pub peak_millionths: u64,
    pub limit_millionths: Option<u64>,
    pub relative_error_millionths: u64,
    pub corrections: usize,
    pub limited_components: usize,
    pub work_limit_exceeded: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AmplitudeFailureHistory {
    pub records: [Option<AmplitudeFailure>; AMPLITUDE_FAILURE_CAPACITY],
    pub count: usize,
    pub omitted: u64,
}

impl AmplitudeFailureHistory {
    pub(super) fn record(&mut self, evidence: AmplitudeFailure) {
        if let Some(record) = self.records.get_mut(self.count) {
            *record = Some(evidence);
            self.count += 1;
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubmissionTimings {
    pub clock_sampling: Option<super::ClockSamplingSnapshot>,
    pub worker: WorkerTimings,
    pub cycle: WallTimeSummary,
    pub clock_query: WallTimeSummary,
    pub health_query: WallTimeSummary,
    pub health_changes: HealthChangeHistory,
    pub dropped_windows: DroppedWindowHistory,
    pub ready_queue: WallTimeSummary,
    pub pending: WallTimeSummary,
    pub write: WallTimeSummary,
    pub slow_writes: SlowWriteHistory,
    pub submitted: u64,
    pub last_publication: Option<PublicationEvidence>,
}

const SLOW_WRITE_CAPACITY: usize = 32;
pub(super) const SLOW_WRITE_THRESHOLD: Duration = Duration::from_micros(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlowWrite {
    pub first: SampleInstant,
    pub first_tick: u64,
    pub requested_samples: usize,
    pub accepted_samples: Option<usize>,
    pub failed: bool,
    // Bounds are from the preceding submission gate, not a hardware query at write entry.
    pub gate_bounds: Option<ClockBounds>,
    pub model_sequence: Option<u64>,
    pub wall_ns: u128,
    pub thread_cpu_ns: Option<u128>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlowWriteHistory {
    pub records: [Option<SlowWrite>; SLOW_WRITE_CAPACITY],
    pub count: usize,
    pub omitted: u64,
}

impl SlowWriteHistory {
    pub(super) fn record(&mut self, evidence: SlowWrite) {
        if let Some(record) = self.records.get_mut(self.count) {
            *record = Some(evidence);
            self.count += 1;
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

pub(super) fn thread_cpu_time() -> Option<Duration> {
    #[cfg(target_os = "linux")]
    {
        Duration::try_from(rustix::time::clock_gettime(
            rustix::time::ClockId::ThreadCPUTime,
        ))
        .ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

const DROPPED_WINDOW_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DroppedWindow {
    pub first: SampleInstant,
    pub next: SampleInstant,
    pub cause: super::DropCause,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DroppedWindowHistory {
    pub records: [Option<DroppedWindow>; DROPPED_WINDOW_CAPACITY],
    pub count: usize,
    pub omitted: u64,
}

impl DroppedWindowHistory {
    pub(super) fn record(&mut self, window: DroppedWindow) {
        if let Some(record) = self.records.get_mut(self.count) {
            *record = Some(window);
            self.count += 1;
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }
}

const HEALTH_CHANGE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthObservation {
    // Device-clock bounds at the host poll, not timestamps of asynchronous RF events.
    pub bounds: ClockBounds,
    pub counters: TxHealthSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HealthChange {
    pub previous: Option<HealthObservation>,
    pub current: HealthObservation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HealthChangeHistory {
    pub records: [Option<HealthChange>; HEALTH_CHANGE_CAPACITY],
    pub count: usize,
    pub omitted: u64,
    pub last_observation: Option<HealthObservation>,
}

impl HealthChangeHistory {
    pub(crate) fn observe(&mut self, bounds: ClockBounds, counters: TxHealthSnapshot) {
        let current = HealthObservation { bounds, counters };
        let previous_counters = self
            .last_observation
            .map(|value| value.counters)
            .unwrap_or_default();
        if counters != previous_counters {
            if self.count < HEALTH_CHANGE_CAPACITY {
                self.records[self.count] = Some(HealthChange {
                    previous: self.last_observation,
                    current,
                });
                self.count += 1;
            } else {
                self.omitted = self.omitted.saturating_add(1);
            }
        }
        self.last_observation = Some(current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_windows_preserve_sample_ranges_and_bound_retention() {
        let mut history = DroppedWindowHistory::default();
        let epoch = umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
        for index in 0..DROPPED_WINDOW_CAPACITY + 2 {
            history.record(DroppedWindow {
                first: SampleInstant {
                    epoch,
                    index: index as u64 * 5120 + 100,
                },
                next: SampleInstant {
                    epoch,
                    index: (index as u64 + 1) * 5120,
                },
                cause: super::super::DropCause::Submission,
            });
        }
        assert_eq!(history.count, DROPPED_WINDOW_CAPACITY);
        assert_eq!(history.omitted, 2);
        let first = history.records[0].unwrap();
        assert_eq!(first.first, SampleInstant { epoch, index: 100 });
        assert_eq!(first.next, SampleInstant { epoch, index: 5120 });
        assert_eq!(first.cause, super::super::DropCause::Submission);
        assert_eq!(
            history.records[DROPPED_WINDOW_CAPACITY - 1]
                .unwrap()
                .next
                .index,
            DROPPED_WINDOW_CAPACITY as u64 * 5120
        );
    }

    #[test]
    fn health_changes_keep_initial_events_and_bound_retention() {
        let mut history = HealthChangeHistory::default();
        let bounds = |tick| ClockBounds {
            earliest: tick,
            latest: tick + 1,
        };
        history.observe(bounds(0), TxHealthSnapshot::default());
        history.observe(bounds(10), TxHealthSnapshot::default());
        assert_eq!(history.count, 0);
        for index in 1..=HEALTH_CHANGE_CAPACITY + 2 {
            history.observe(
                bounds(10 + index as u64),
                TxHealthSnapshot {
                    underflows: index as u64,
                    ..TxHealthSnapshot::default()
                },
            );
        }
        assert_eq!(history.count, HEALTH_CHANGE_CAPACITY);
        assert_eq!(history.omitted, 2);
        let first = history.records[0].unwrap();
        assert_eq!(first.previous.unwrap().bounds, bounds(10));
        assert_eq!(first.current.counters.underflows, 1);
        assert_eq!(
            history.last_observation.unwrap().counters.underflows,
            (HEALTH_CHANGE_CAPACITY + 2) as u64
        );
        let mut initial = HealthChangeHistory::default();
        initial.observe(bounds(0), first.current.counters);
        assert_eq!(initial.count, 1);
        assert!(initial.records[0].unwrap().previous.is_none());
    }

    fn publication(index: u64) -> PublicationCompletionEvidence {
        PublicationCompletionEvidence {
            first_sample: SampleInstant {
                epoch: crate::test_support::at(0).epoch,
                index,
            },
            next_sample: SampleInstant {
                epoch: crate::test_support::at(0).epoch,
                index: index + 5120,
            },
            enqueue_returned_ns: 10,
            unpark_started_ns: Some(12),
            unpark_returned_ns: Some(15),
            wake_registered: true,
        }
    }

    #[test]
    fn publication_history_retains_latest_sixteen_exact_sample_owners_in_order() {
        let mut history = PublicationCompletionHistory::default();
        assert_eq!(history.records, [None; PUBLICATION_HISTORY_CAPACITY]);
        for index in 0..PUBLICATION_HISTORY_CAPACITY as u64 + 2 {
            history.push(publication(index));
        }
        assert_eq!(history.count, PUBLICATION_HISTORY_CAPACITY);
        assert_eq!(history.next_index, 2);
        assert_eq!(history.overwritten, 2);
        for offset in 0..PUBLICATION_HISTORY_CAPACITY {
            let index = (history.next_index + offset) % PUBLICATION_HISTORY_CAPACITY;
            assert_eq!(history.records[index], Some(publication(offset as u64 + 2)));
        }
        history.overwritten = u64::MAX;
        history.push(publication(18));
        assert_eq!(history.overwritten, u64::MAX);
        assert_eq!(history.records[2], Some(publication(18)));
    }

    #[test]
    fn publication_snapshots_preserve_optional_wake_intervals_and_full_epoch_geometry() {
        let mut timings = PreparationTimings::default();
        let mut absent = publication(5120);
        absent.wake_registered = false;
        absent.unpark_started_ns = None;
        absent.unpark_returned_ns = None;
        timings.publication_completions.push(absent);
        let snapshot = timings;
        let mut next = publication(5120);
        next.first_sample.epoch =
            umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
        next.next_sample.epoch = next.first_sample.epoch;
        next.unpark_returned_ns = None;
        timings.publication_completions.push(next);
        assert_eq!(snapshot.publication_completions.count, 1);
        assert_eq!(snapshot.publication_completions.records[0], Some(absent));
        assert_eq!(snapshot.publication_completions.records[1], None);
        assert_eq!(timings.publication_completions.records[1], Some(next));
        assert_ne!(absent.first_sample, next.first_sample);
        assert_eq!(next.first_sample.epoch, next.next_sample.epoch);
        assert_eq!(next.next_sample.index - next.first_sample.index, 5120);
        assert!(next.enqueue_returned_ns <= next.unpark_started_ns.unwrap());
        assert!(next.wake_registered);
        assert_eq!(next.unpark_returned_ns, None);
    }

    fn evidence(wait_id: u64) -> WaitEvidence {
        WaitEvidence {
            wait_id,
            kind: WaitKind::Target,
            target_tick: Some(100),
            deadline_tick: Some(110),
            pre_earliest: 90,
            pre_latest: 91,
            pre_model_sequence: Some(7),
            clock_source: WaitClockSource::Published,
            started_host_ns: 1_000,
            requested_ns: 200,
            cooperative: false,
            spinning: false,
            actual_ns: 220,
            woke_host_ns: Some(1_220),
            post_sample_finished_host_ns: Some(1_225),
            post_earliest: Some(99),
            post_latest: Some(101),
            post_model_sequence: Some(8),
            post_clock_source: Some(WaitClockSource::FreshSampled),
            deadline_device_tick: None,
            deadline_first_tick: None,
            deadline_model_sequence: None,
            deadline_origin: None,
            next_deadline_shortfall_ticks: None,
            resolution: Some(WaitResolution::WaitingAgain),
            publication: None,
        }
    }

    #[test]
    fn wait_history_retains_latest_sixteen_records_in_ring_order() {
        let mut history = WaitHistory::default();
        assert_eq!(history.records, [None; WAIT_HISTORY_CAPACITY]);
        for wait_id in 0..WAIT_HISTORY_CAPACITY as u64 {
            history.push(evidence(wait_id));
        }
        assert_eq!(history.count, WAIT_HISTORY_CAPACITY);
        assert_eq!(history.next_index, 0);
        assert_eq!(history.overwritten, 0);
        history.push(evidence(16));
        history.push(evidence(17));
        assert_eq!(history.count, WAIT_HISTORY_CAPACITY);
        assert_eq!(history.next_index, 2);
        assert_eq!(history.overwritten, 2);
        for offset in 0..WAIT_HISTORY_CAPACITY {
            let index = (history.next_index + offset) % WAIT_HISTORY_CAPACITY;
            assert_eq!(history.records[index], Some(evidence(offset as u64 + 2)));
        }
    }

    #[test]
    fn wait_history_overwrite_counter_saturates_without_affecting_retention() {
        let mut history = WaitHistory::default();
        for wait_id in 0..WAIT_HISTORY_CAPACITY as u64 {
            history.push(evidence(wait_id));
        }
        history.overwritten = u64::MAX;
        history.push(evidence(99));
        assert_eq!(history.overwritten, u64::MAX);
        assert_eq!(history.records[0], Some(evidence(99)));
        assert_eq!(history.next_index, 1);
    }

    #[test]
    fn wait_snapshots_preserve_active_and_resolved_records_independently() {
        let completed = evidence(1);
        let mut active = evidence(2);
        active.resolution = None;
        active.woke_host_ns = None;
        active.post_sample_finished_host_ns = None;
        active.post_earliest = None;
        active.post_latest = None;
        active.post_model_sequence = None;
        active.post_clock_source = None;
        let mut timings = WorkerTimings {
            last_wait: Some(completed),
            active_wait: Some(active),
            ..WorkerTimings::default()
        };
        timings.wait_history.push(completed);
        let snapshot = timings;
        timings.active_wait = None;
        timings.wait_history.push(evidence(3));
        timings.wait_ids_exhausted = true;
        assert!(timings.wait_ids_exhausted);
        assert_eq!(snapshot.active_wait, Some(active));
        assert_eq!(snapshot.last_wait, Some(completed));
        assert_eq!(snapshot.wait_history.count, 1);
        assert_eq!(snapshot.wait_history.records[1], None);
        assert!(!snapshot.wait_ids_exhausted);
        assert_eq!(snapshot.active_wait.unwrap().woke_host_ns, None);
    }

    #[test]
    fn exact_elapsed_totals_and_maximum_include_all_recorded_calls() {
        let mut summary = WallTimeSummary::default();
        for duration in [
            Duration::from_nanos(3),
            Duration::from_secs(1),
            Duration::from_nanos(7),
        ] {
            summary.record(duration);
        }
        assert_eq!(
            summary,
            WallTimeSummary {
                calls: 3,
                total_ns: 1_000_000_010,
                maximum_ns: 1_000_000_000,
            }
        );
    }

    #[test]
    fn zero_duration_still_counts_a_call_without_inventing_elapsed_time() {
        let mut summary = WallTimeSummary::default();
        assert_eq!(
            summary,
            WallTimeSummary {
                calls: 0,
                total_ns: 0,
                maximum_ns: 0
            }
        );
        summary.record(Duration::ZERO);
        assert_eq!(
            summary,
            WallTimeSummary {
                calls: 1,
                total_ns: 0,
                maximum_ns: 0
            }
        );
    }

    #[test]
    fn counters_saturate_without_wrapping_or_losing_the_observed_maximum() {
        let mut summary = WallTimeSummary {
            calls: u64::MAX - 1,
            total_ns: u128::MAX - 1,
            maximum_ns: 0,
        };
        summary.record(Duration::from_nanos(5));
        assert_eq!(
            summary,
            WallTimeSummary {
                calls: u64::MAX,
                total_ns: u128::MAX,
                maximum_ns: 5
            }
        );
        summary.record(Duration::from_nanos(10));
        assert_eq!(
            summary,
            WallTimeSummary {
                calls: u64::MAX,
                total_ns: u128::MAX,
                maximum_ns: 10
            }
        );
        summary.record(Duration::ZERO);
        assert_eq!(
            summary,
            WallTimeSummary {
                calls: u64::MAX,
                total_ns: u128::MAX,
                maximum_ns: 10
            }
        );
    }

    #[test]
    fn maximum_duration_is_represented_without_truncation() {
        let mut summary = WallTimeSummary::default();
        summary.record(Duration::MAX);
        assert_eq!(summary.total_ns, Duration::MAX.as_nanos());
        assert_eq!(summary.maximum_ns, Duration::MAX.as_nanos());
        assert!(summary.maximum_ns > u128::from(u64::MAX));
    }

    #[test]
    fn stage_snapshots_are_copy_values_and_keep_each_wall_metric_independent() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<WallTimeSummary>();
        assert_copy::<PublicationCompletionEvidence>();
        assert_copy::<PublicationCompletionHistory>();
        assert_copy::<WaitEvidence>();
        assert_copy::<WaitHistory>();
        assert_copy::<WorkerTimings>();
        assert_copy::<PreparationTimings>();
        assert_copy::<SubmissionTimings>();
        let mut preparation = PreparationTimings::default();
        let snapshot = preparation;
        preparation.synthesis.record(Duration::from_nanos(5));
        assert_eq!(snapshot, PreparationTimings::default());
        assert_eq!(preparation.cycle, WallTimeSummary::default());
        assert_eq!(preparation.published, 0);
        let mut submission = SubmissionTimings::default();
        let snapshot = submission;
        submission.write.record(Duration::from_nanos(7));
        assert_eq!(snapshot, SubmissionTimings::default());
        for untouched in [
            submission.cycle,
            submission.clock_query,
            submission.health_query,
            submission.ready_queue,
            submission.pending,
        ] {
            assert_eq!(untouched, WallTimeSummary::default());
        }
        assert_eq!(submission.write.calls, 1);
        assert_eq!(submission.submitted, 0);
    }
}
