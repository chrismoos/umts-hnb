// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::radio_io::{
    ClockEvidence, WaitSchedule,
    timing::{WaitKind, WaitResolution, WorkerTimings},
};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct WorkerTracker {
    last_call_end: Option<Instant>,
    origin: Option<Instant>,
    next_wait_id: u64,
    pub(super) timings: WorkerTimings,
    wait_started_at: Option<Instant>,
    wait_woke_at: Option<Instant>,
}

impl WorkerTracker {
    pub(super) fn record_policy(&mut self, cooperative: bool, spinning: bool) {
        if let Some(wait) = &mut self.timings.active_wait {
            wait.cooperative = cooperative;
            wait.spinning = spinning;
        }
    }

    pub(super) fn begin_call(&mut self, now: Instant) {
        if let Some(previous) = self.last_call_end.take() {
            self.timings
                .inter_call
                .record(now.saturating_duration_since(previous));
        }
    }

    pub(super) fn end_call(&mut self, now: Instant) {
        self.last_call_end = Some(now);
    }

    pub(super) fn record_idle(&mut self, requested: Duration, actual: Duration) {
        self.timings.idle_wait.record(actual);
        self.timings
            .idle_overshoot
            .record(actual.saturating_sub(requested));
    }

    pub(super) fn begin_wait(
        &mut self,
        evidence: ClockEvidence,
        schedule: Option<WaitSchedule>,
        requested: Duration,
        now: Instant,
    ) -> bool {
        if self.timings.active_wait.is_some() {
            return false;
        }
        let Some(id) = self.next_wait_id.checked_add(1) else {
            self.timings.wait_ids_exhausted = true;
            return false;
        };
        self.next_wait_id = id;
        self.wait_started_at = Some(now);
        self.wait_woke_at = None;
        let origin = *self.origin.get_or_insert(now);
        self.timings.active_wait = Some(crate::radio_io::timing::WaitEvidence {
            wait_id: id,
            kind: if schedule.is_some() {
                WaitKind::Target
            } else {
                WaitKind::Idle
            },
            target_tick: schedule.map(|value| value.target_tick),
            deadline_tick: schedule.map(|value| value.deadline_tick),
            pre_earliest: evidence.bounds.earliest,
            pre_latest: evidence.bounds.latest,
            pre_model_sequence: evidence.model_sequence,
            clock_source: evidence.source,
            started_host_ns: now.saturating_duration_since(origin).as_nanos(),
            requested_ns: requested.as_nanos(),
            cooperative: false,
            spinning: false,
            actual_ns: 0,
            woke_host_ns: None,
            post_sample_finished_host_ns: None,
            post_earliest: None,
            post_latest: None,
            post_model_sequence: None,
            post_clock_source: None,
            deadline_device_tick: None,
            deadline_first_tick: None,
            deadline_model_sequence: None,
            deadline_origin: None,
            next_deadline_shortfall_ticks: None,
            resolution: None,
            publication: None,
        });
        true
    }

    pub(super) fn record_wake(
        &mut self,
        now: Instant,
        actual: Duration,
        post: Option<ClockEvidence>,
        sampled: Instant,
    ) {
        if let Some(wait) = &mut self.timings.active_wait {
            self.wait_woke_at = Some(now);
            wait.actual_ns = actual.as_nanos();
            wait.woke_host_ns = self
                .origin
                .map(|origin| now.saturating_duration_since(origin).as_nanos());
            wait.post_sample_finished_host_ns = self
                .origin
                .map(|origin| sampled.saturating_duration_since(origin).as_nanos());
            if let Some(post) = post {
                wait.post_earliest = Some(post.bounds.earliest);
                wait.post_latest = Some(post.bounds.latest);
                wait.post_model_sequence = post.model_sequence;
                wait.post_clock_source = Some(post.source);
            }
        }
    }

    pub(super) fn record_publication(
        &mut self,
        evidence: &mut crate::radio_io::PublicationEvidence,
        enqueued: Instant,
        dequeued: Instant,
    ) {
        if let Some(wait) = &mut self.timings.active_wait {
            evidence.enqueue_started_from_wait_start = self
                .wait_started_at
                .map(|started| crate::radio_io::HostOffset::between(enqueued, started));
            evidence.dequeued_from_wait_wake = self
                .wait_woke_at
                .map(|woke| crate::radio_io::HostOffset::between(dequeued, woke));
            wait.publication = Some(*evidence);
        }
    }

    pub(super) fn record_deadline(
        &mut self,
        failure: crate::radio_io::DeadlineFailure,
        sequence: Option<u64>,
        cause: crate::radio_io::DropCause,
    ) {
        self.timings.last_deadline = Some(failure);
        self.timings.last_deadline_origin = Some(cause);
        if let Some(wait) = &mut self.timings.active_wait {
            wait.deadline_origin = Some(cause);
            wait.deadline_device_tick = Some(failure.device_tick);
            wait.deadline_first_tick = Some(failure.first_tick);
            wait.deadline_model_sequence = sequence;
            wait.next_deadline_shortfall_ticks = Some(failure.shortfall_ticks);
        } else {
            self.timings.unpaired_deadlines = self.timings.unpaired_deadlines.saturating_add(1);
        }
        self.finish_wait(WaitResolution::Deadline);
    }

    pub(super) fn finish_wait(&mut self, resolution: WaitResolution) {
        if let Some(mut wait) = self.timings.active_wait.take() {
            wait.resolution = Some(resolution);
            if resolution == WaitResolution::Deadline {
                self.timings.deadline_wait_history.push(wait);
            }
            self.timings.last_wait = Some(wait);
            self.timings.wait_history.push(wait);
            self.wait_started_at = None;
            self.wait_woke_at = None;
        }
    }
}

pub(super) fn measured_call<T>(tracker: &mut Option<WorkerTracker>, call: impl FnOnce() -> T) -> T {
    if let Some(tracker) = tracker {
        tracker.begin_call(Instant::now());
        let result = call();
        tracker.end_call(Instant::now());
        result
    } else {
        call()
    }
}

#[cfg(test)]
pub(super) fn sample_evidence() -> ClockEvidence {
    ClockEvidence {
        bounds: umts_radio::clock_model::ClockBounds {
            earliest: 100,
            latest: 110,
        },
        source: crate::radio_io::WaitClockSource::FreshSampled,
        model_sequence: Some(7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_publication_is_exactly_bound_to_wait_and_preserves_relative_order() {
        use crate::radio_io::{HostOffset, PublicationEvidence};
        use umts_types::time::SampleInstant;
        let started = Instant::now();
        for (queued, expected) in [
            (
                started - Duration::from_micros(5),
                HostOffset::BeforeNs(5_000),
            ),
            (
                started + Duration::from_micros(10),
                HostOffset::AtOrAfterNs(10_000),
            ),
        ] {
            let mut tracker = WorkerTracker::default();
            assert!(tracker.begin_wait(
                sample_evidence(),
                None,
                Duration::from_micros(100),
                started
            ));
            let woke = started + Duration::from_micros(1400);
            tracker.record_wake(
                woke,
                Duration::from_micros(1400),
                Some(sample_evidence()),
                woke,
            );
            let mut publication = PublicationEvidence {
                first_sample: SampleInstant {
                    epoch: crate::test_support::at(0).epoch,
                    index: 5120,
                },
                first_tick: 5000,
                deadline_tick: Some(4900),
                enqueue_started_to_dequeue_ns: 0,
                enqueue_started_from_wait_start: None,
                dequeued_from_wait_wake: None,
            };
            let dequeued = woke + Duration::from_micros(2);
            publication.enqueue_started_to_dequeue_ns = dequeued.duration_since(queued).as_nanos();
            tracker.record_publication(&mut publication, queued, dequeued);
            tracker.record_deadline(
                crate::radio_io::DeadlineFailure {
                    device_tick: 5100,
                    first_tick: 5000,
                    shortfall_ticks: 200,
                },
                Some(8),
                crate::radio_io::DropCause::Submission,
            );
            let retained = tracker.timings.last_wait.unwrap();
            assert_eq!(retained.publication, Some(publication));
            assert_eq!(publication.enqueue_started_from_wait_start, Some(expected));
            assert_eq!(
                publication.dequeued_from_wait_wake,
                Some(HostOffset::AtOrAfterNs(2_000))
            );
            assert_eq!(retained.deadline_first_tick, Some(publication.first_tick));
            assert_eq!(retained.resolution, Some(WaitResolution::Deadline));
            assert!(tracker.wait_started_at.is_none());
            assert!(tracker.wait_woke_at.is_none());
        }
    }

    #[test]
    fn wait_ids_never_wrap_or_reuse_an_identity() {
        let mut tracker = WorkerTracker {
            next_wait_id: u64::MAX,
            ..WorkerTracker::default()
        };
        assert!(!tracker.begin_wait(sample_evidence(), None, Duration::ZERO, Instant::now()));
        assert!(tracker.timings.wait_ids_exhausted);
        assert!(tracker.timings.active_wait.is_none());
    }
}
