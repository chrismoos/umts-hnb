// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::radio_io::WallTimeSummary;
use std::time::{Duration, Instant};

const HISTORY: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
pub struct RxCycleTiming {
    pub read: Option<crate::radio_io::RxReadTiming>,
    pub input_next_sample: Option<u64>,
    pub started_ns: u128,
    pub completed_ns: u128,
    pub inter_cycle_ns: u128,
    pub read_filter_ns: u128,
    pub dedicated_feed_ns: u128,
    pub access_search_ns: u128,
    pub dedicated_service_ns: u128,
    pub dedicated_service_cpu_ns: Option<u128>,
    pub access_decode_ns: u128,
    pub clock_sample_ns: u128,
    pub sampled_slots: u64,
    pub decode_state_updates: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RxTimingSummary {
    pub dedicated_progress_without_input: u64,
    pub cycle: WallTimeSummary,
    pub read_filter: WallTimeSummary,
    pub dedicated_feed: WallTimeSummary,
    pub access_search: WallTimeSummary,
    pub dedicated_service: WallTimeSummary,
    pub access_decode: WallTimeSummary,
    pub clock_sample: WallTimeSummary,
    pub slowest_cycle: Option<RxCycleTiming>,
    pub slowest_dedicated_service: Option<RxCycleTiming>,
    pub largest_dedicated_cpu: Option<RxCycleTiming>,
    pub dedicated_cpu_samples: u64,
    pub dedicated_cpu_total_ns: u128,
    pub history: [Option<RxCycleTiming>; HISTORY],
    pub next_index: usize,
    pub frozen_on_capture_request: bool,
}

impl RxTimingSummary {
    fn retain_peaks(&mut self, cycle: RxCycleTiming) {
        if let Some(cpu_ns) = cycle.dedicated_service_cpu_ns {
            self.dedicated_cpu_samples = self.dedicated_cpu_samples.saturating_add(1);
            self.dedicated_cpu_total_ns = self.dedicated_cpu_total_ns.saturating_add(cpu_ns);
            if self.largest_dedicated_cpu.is_none_or(|previous| {
                previous
                    .dedicated_service_cpu_ns
                    .is_none_or(|previous| cpu_ns > previous)
            }) {
                self.largest_dedicated_cpu = Some(cycle);
            }
        }
        if cycle.input_next_sample.is_none()
            && (cycle.sampled_slots != 0 || cycle.decode_state_updates != 0)
        {
            self.dedicated_progress_without_input =
                self.dedicated_progress_without_input.saturating_add(1);
        }
        if self.slowest_cycle.is_none_or(|previous| {
            cycle.completed_ns - cycle.started_ns > previous.completed_ns - previous.started_ns
        }) {
            self.slowest_cycle = Some(cycle);
        }
        if self
            .slowest_dedicated_service
            .is_none_or(|previous| cycle.dedicated_service_ns > previous.dedicated_service_ns)
        {
            self.slowest_dedicated_service = Some(cycle);
        }
    }
}

pub(super) struct RxTimings {
    origin: Instant,
    previous_end: Instant,
    pub summary: RxTimingSummary,
}

impl RxTimings {
    pub fn record(&mut self, mut cycle: RxCycleTiming, started: Instant, freeze: bool) {
        let completed = Instant::now();
        cycle.started_ns = started.duration_since(self.origin).as_nanos();
        cycle.completed_ns = completed.duration_since(self.origin).as_nanos();
        cycle.inter_cycle_ns = started
            .saturating_duration_since(self.previous_end)
            .as_nanos();
        self.previous_end = completed;
        self.summary.retain_peaks(cycle);
        self.summary.cycle.record(completed.duration_since(started));
        for (summary, ns) in [
            (&mut self.summary.read_filter, cycle.read_filter_ns),
            (&mut self.summary.dedicated_feed, cycle.dedicated_feed_ns),
            (&mut self.summary.access_search, cycle.access_search_ns),
            (
                &mut self.summary.dedicated_service,
                cycle.dedicated_service_ns,
            ),
            (&mut self.summary.access_decode, cycle.access_decode_ns),
            (&mut self.summary.clock_sample, cycle.clock_sample_ns),
        ] {
            summary.record(Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)));
        }
        if !self.summary.frozen_on_capture_request {
            self.summary.history[self.summary.next_index] = Some(cycle);
            self.summary.next_index = (self.summary.next_index + 1) % HISTORY;
            self.summary.frozen_on_capture_request = freeze;
        }
    }
}

pub(super) fn elapsed(started: Option<Instant>) -> u128 {
    started.map_or(0, |started| started.elapsed().as_nanos())
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

pub(super) fn thread_cpu_elapsed(started: Duration) -> Option<u128> {
    thread_cpu_time()?
        .checked_sub(started)
        .map(|elapsed| elapsed.as_nanos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_peak_retains_its_own_wall_time_and_work_instead_of_the_wall_peak() {
        let mut summary = RxTimingSummary::default();
        let mostly_waiting = RxCycleTiming {
            input_next_sample: Some(100),
            dedicated_service_ns: 1000,
            dedicated_service_cpu_ns: Some(10),
            decode_state_updates: 8,
            ..RxCycleTiming::default()
        };
        let mostly_computing = RxCycleTiming {
            input_next_sample: Some(200),
            dedicated_service_ns: 100,
            dedicated_service_cpu_ns: Some(90),
            decode_state_updates: 64,
            ..RxCycleTiming::default()
        };
        summary.retain_peaks(mostly_waiting);
        summary.retain_peaks(mostly_computing);
        summary.retain_peaks(RxCycleTiming::default());
        let wall = summary.slowest_dedicated_service.unwrap();
        assert_eq!(wall.input_next_sample, Some(100));
        assert_eq!(wall.dedicated_service_cpu_ns, Some(10));
        let cpu = summary.largest_dedicated_cpu.unwrap();
        assert_eq!(cpu.input_next_sample, Some(200));
        assert_eq!(cpu.dedicated_service_ns, 100);
        assert_eq!(cpu.dedicated_service_cpu_ns, Some(90));
        assert_eq!(cpu.decode_state_updates, 64);
        assert_eq!(summary.dedicated_cpu_samples, 2);
        assert_eq!(summary.dedicated_cpu_total_ns, 100);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_thread_cpu_clock_is_available_and_monotonic() {
        let first = thread_cpu_time().unwrap();
        let second = thread_cpu_time().unwrap();
        assert!(second >= first);
        assert!(thread_cpu_elapsed(first).is_some());
    }

    #[test]
    fn peak_snapshots_retain_complete_events_instead_of_combining_stage_maxima() {
        let mut summary = RxTimingSummary::default();
        let slow_cycle = RxCycleTiming {
            input_next_sample: Some(100),
            started_ns: 10,
            completed_ns: 110,
            dedicated_service_ns: 20,
            read_filter_ns: 70,
            decode_state_updates: 4,
            ..RxCycleTiming::default()
        };
        let slow_service = RxCycleTiming {
            input_next_sample: Some(200),
            started_ns: 200,
            completed_ns: 280,
            dedicated_service_ns: 60,
            read_filter_ns: 10,
            decode_state_updates: 8,
            ..RxCycleTiming::default()
        };
        summary.retain_peaks(slow_cycle);
        summary.retain_peaks(slow_service);
        summary.retain_peaks(RxCycleTiming::default());
        let cycle = summary.slowest_cycle.unwrap();
        assert_eq!(cycle.input_next_sample, Some(100));
        assert_eq!(cycle.read_filter_ns, 70);
        assert_eq!(cycle.dedicated_service_ns, 20);
        assert_eq!(cycle.decode_state_updates, 4);
        let service = summary.slowest_dedicated_service.unwrap();
        assert_eq!(service.input_next_sample, Some(200));
        assert_eq!(service.read_filter_ns, 10);
        assert_eq!(service.completed_ns - service.started_ns, 80);
        assert_eq!(service.decode_state_updates, 8);
    }

    #[test]
    fn counts_only_dedicated_progress_without_a_radio_block() {
        let mut summary = RxTimingSummary::default();
        for cycle in [
            RxCycleTiming::default(),
            RxCycleTiming {
                sampled_slots: 1,
                ..RxCycleTiming::default()
            },
            RxCycleTiming {
                decode_state_updates: 8,
                ..RxCycleTiming::default()
            },
            RxCycleTiming {
                input_next_sample: Some(5120),
                decode_state_updates: 8,
                ..RxCycleTiming::default()
            },
        ] {
            summary.retain_peaks(cycle);
        }
        assert_eq!(summary.dedicated_progress_without_input, 2);
    }
}
