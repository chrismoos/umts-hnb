// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::hsupa_rx::SchedulingReport;
use umts_types::time::{CHIPS_PER_SECOND, ChipInstant};

const DATA_SILENCE_CHIPS: u64 = CHIPS_PER_SECOND / 2;
const FRESH_SCHEDULING_CHIPS: u64 = CHIPS_PER_SECOND / 5;
const RETRY_INTERVAL_CHIPS: u64 = CHIPS_PER_SECOND;
const MAX_ATTEMPTS: u8 = 3;

#[derive(Default)]
pub(super) struct Recovery {
    data: Option<ChipInstant>,
    scheduling: Option<SchedulingReport>,
    last_attempt: Option<ChipInstant>,
    attempts: u8,
}

impl Recovery {
    pub(super) fn observe(
        &mut self,
        data: Option<ChipInstant>,
        scheduling: Option<SchedulingReport>,
    ) {
        if data != self.data {
            self.data = data;
            self.attempts = 0;
        }
        self.scheduling = scheduling;
    }

    // TS 25.321 11.8.1.3.1 permits repeating the same primary allocation.
    // Scheduling information alone does not prove that the UE lost its grant.
    pub(super) fn due(&self, now: ChipInstant) -> bool {
        let (Some(data), Some(scheduling)) = (self.data, self.scheduling) else {
            return false;
        };
        self.attempts < MAX_ATTEMPTS
            && scheduling.information.total_buffer_index != 0
            && age(now, data).is_some_and(|age| age >= DATA_SILENCE_CHIPS)
            && age(now, scheduling.start).is_some_and(|age| age <= FRESH_SCHEDULING_CHIPS)
            && self
                .last_attempt
                .is_none_or(|last| age(now, last).is_some_and(|age| age >= RETRY_INTERVAL_CHIPS))
    }

    pub(super) fn has_fresh_demand(&self, now: ChipInstant) -> bool {
        self.scheduling.is_some_and(|report| {
            report.information.total_buffer_index != 0
                && age(now, report.start).is_some_and(|age| age <= FRESH_SCHEDULING_CHIPS)
        })
    }

    pub(super) fn queued(&mut self, at: ChipInstant) {
        self.last_attempt = Some(at);
        self.attempts = self.attempts.saturating_add(1);
    }
}

fn age(now: ChipInstant, previous: ChipInstant) -> Option<u64> {
    (now.epoch == previous.epoch)
        .then(|| now.index.checked_sub(previous.index))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;
    use umts_mac::hsupa::scheduling::SchedulingInformation;

    fn demand(index: u64, buffered: bool) -> SchedulingReport {
        SchedulingReport {
            start: at(index),
            information: SchedulingInformation {
                power_headroom_index: 31,
                total_buffer_index: u8::from(buffered),
                priority_buffer_index: 0,
                highest_priority_logical_channel: 1,
            },
        }
    }

    #[test]
    fn refresh_requires_fresh_demand_and_silence_after_data() {
        let mut recovery = Recovery::default();
        let now = DATA_SILENCE_CHIPS;
        recovery.observe(None, Some(demand(now, true)));
        assert!(!recovery.due(at(now)));
        recovery.observe(Some(at(0)), Some(demand(now, false)));
        assert!(!recovery.due(at(now)));
        recovery.observe(Some(at(0)), Some(demand(0, true)));
        assert!(!recovery.due(at(now)));
        recovery.observe(Some(at(1)), Some(demand(now, true)));
        assert!(!recovery.due(at(now)));
        recovery.observe(Some(at(0)), Some(demand(now, true)));
        assert!(recovery.due(at(now)));
        assert!(!recovery.due(at(now - 1)));
    }

    #[test]
    fn refresh_budget_and_cooldown_are_per_owner_and_rearmed_only_by_data() {
        let mut recovery = Recovery::default();
        for attempt in 0..MAX_ATTEMPTS {
            let now = DATA_SILENCE_CHIPS + u64::from(attempt) * RETRY_INTERVAL_CHIPS;
            recovery.observe(Some(at(0)), Some(demand(now, true)));
            assert!(recovery.due(at(now)));
            recovery.queued(at(now));
            assert!(!recovery.due(at(now)));
        }
        let now = DATA_SILENCE_CHIPS + u64::from(MAX_ATTEMPTS) * RETRY_INTERVAL_CHIPS;
        recovery.observe(Some(at(0)), Some(demand(now, true)));
        assert!(!recovery.due(at(now)));
        let mut peer = Recovery::default();
        peer.observe(Some(at(0)), Some(demand(now, true)));
        assert!(peer.due(at(now)));
        recovery.observe(Some(at(now)), Some(demand(now, true)));
        assert!(!recovery.due(at(now)));
        let later = now + DATA_SILENCE_CHIPS;
        recovery.observe(Some(at(now)), Some(demand(later, true)));
        assert!(recovery.due(at(later)));
    }
}
