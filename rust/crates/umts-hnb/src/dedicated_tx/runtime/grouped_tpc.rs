// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Bit, CHIPS_PER_SLOT, TpcEstimate};

pub(crate) const MAXIMUM_HOLD_GROUPS: u8 = 8;
const SLOTS_PER_GROUP: u64 = 5;
const GROUPS_PER_FRAME: u64 = 3;

pub(super) struct GroupedTpc {
    hold_groups: u8,
    group: Option<u64>,
    command: Option<Bit>,
}

impl GroupedTpc {
    pub(super) fn new(hold_groups: u8) -> Self {
        Self {
            hold_groups,
            group: None,
            command: None,
        }
    }

    // TS 25.214 5.1.2.2.3.1 aligns independent five-slot sets to this UE's DPCH frames.
    pub(super) fn take(
        &mut self,
        elapsed_slots: u64,
        anchor_cfn: u8,
        start: super::ChipInstant,
        estimate: Option<TpcEstimate>,
        counters: &mut super::TpcEstimateCounters,
    ) -> Bit {
        let group = u64::from(anchor_cfn) * GROUPS_PER_FRAME + elapsed_slots / SLOTS_PER_GROUP;
        let position = elapsed_slots % SLOTS_PER_GROUP;
        if self.group != Some(group) {
            self.group = Some(group);
            let cadence = u64::from(self.hold_groups) + 1;
            let control_group = group % cadence == u64::from(self.hold_groups);
            self.command = None;
            let groups = &mut counters.groups;
            if position != 0 {
                groups.partial_start_hold = groups.partial_start_hold.saturating_add(1);
            } else if !control_group {
                groups.cadence_hold = groups.cadence_hold.saturating_add(1);
            } else if let Some(estimate) = estimate {
                if estimate.observed_at().epoch != start.epoch
                    || estimate.observed_at().index > start.index
                    || estimate.valid_until().index <= start.index
                {
                    groups.stale_hold = groups.stale_hold.saturating_add(1);
                } else {
                    // TS 25.214 5.1.2.2.3.1 encodes one correction as five equal commands.
                    // Freshness gates the decision. An admitted group must finish unchanged.
                    let command = estimate.command();
                    self.command = Some(command);
                    match command {
                        Bit::One => groups.up = groups.up.saturating_add(1),
                        Bit::Zero => groups.down = groups.down.saturating_add(1),
                    }
                    groups.maximum_decision_age_chips = groups
                        .maximum_decision_age_chips
                        .max(start.index - estimate.observed_at().index);
                    if start
                        .index
                        .checked_add(SLOTS_PER_GROUP * CHIPS_PER_SLOT)
                        .is_some_and(|end| end > estimate.valid_until().index)
                    {
                        groups.latched_beyond_measurement_window =
                            groups.latched_beyond_measurement_window.saturating_add(1);
                    }
                }
            } else {
                groups.missing_hold = groups.missing_hold.saturating_add(1);
            }
            if self.command.is_some() {
                counters.algorithm2_control_groups =
                    counters.algorithm2_control_groups.saturating_add(1);
            } else {
                counters.algorithm2_hold_groups = counters.algorithm2_hold_groups.saturating_add(1);
                if estimate.is_none() {
                    counters.algorithm2_stale_hold_groups =
                        counters.algorithm2_stale_hold_groups.saturating_add(1);
                }
            }
        }
        self.command.unwrap_or_else(|| {
            counters.algorithm2_hold_slots = counters.algorithm2_hold_slots.saturating_add(1);
            Bit::from(elapsed_slots.is_multiple_of(2))
        })
    }
}
