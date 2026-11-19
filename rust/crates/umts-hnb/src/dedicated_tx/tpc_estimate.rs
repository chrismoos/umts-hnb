// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ReceiveIdentity, TransmitError};
use umts_dsp::bits::Bit;
use umts_types::time::{CHIPS_PER_SLOT, ChipInstant};

pub(crate) const ESTIMATE_SLOTS: usize = 64;
const FAR_ABOVE_TARGET: f64 = 6.3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TpcEstimate {
    identity: ReceiveIdentity,
    valid_until: ChipInstant,
    observed_at: ChipInstant,
    sir_linear: f64,
    target_linear: f64,
}

impl TpcEstimate {
    pub fn new(
        identity: ReceiveIdentity,
        valid_until: ChipInstant,
        observed_at: ChipInstant,
        sir_linear: f64,
        target_linear: f64,
    ) -> Result<Self, TransmitError> {
        if [valid_until.epoch, observed_at.epoch]
            .iter()
            .any(|epoch| *epoch != identity.ue.epoch)
        {
            return Err(TransmitError::Identity);
        }
        if valid_until.index <= observed_at.index
            || valid_until.index - observed_at.index > ESTIMATE_SLOTS as u64 * CHIPS_PER_SLOT
        {
            return Err(TransmitError::Time);
        }
        if !sir_linear.is_finite()
            || sir_linear <= 0.0
            || !target_linear.is_finite()
            || target_linear <= 0.0
        {
            return Err(TransmitError::Payload);
        }
        Ok(Self {
            identity,
            valid_until,
            observed_at,
            sir_linear,
            target_linear,
        })
    }
    pub fn identity(self) -> ReceiveIdentity {
        self.identity
    }
    pub fn valid_until(self) -> ChipInstant {
        self.valid_until
    }
    pub fn observed_at(self) -> ChipInstant {
        self.observed_at
    }
    pub fn sir_linear(self) -> f64 {
        self.sir_linear
    }
    pub fn far_above_target(self) -> bool {
        self.sir_linear >= self.target_linear * FAR_ABOVE_TARGET
    }
    // TS 25.214 5.1.2.2.1. The estimate is held only within its original validity interval.
    pub fn command(self) -> Bit {
        Bit::from(self.sir_linear < self.target_linear)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TpcGroupCounters {
    pub up: u64,
    pub down: u64,
    pub cadence_hold: u64,
    pub missing_hold: u64,
    pub partial_start_hold: u64,
    pub stale_hold: u64,
    pub latched_beyond_measurement_window: u64,
    pub maximum_decision_age_chips: u64,
}

impl TpcGroupCounters {
    fn add(&mut self, other: Self) {
        self.up = self.up.saturating_add(other.up);
        self.down = self.down.saturating_add(other.down);
        self.cadence_hold = self.cadence_hold.saturating_add(other.cadence_hold);
        self.missing_hold = self.missing_hold.saturating_add(other.missing_hold);
        self.partial_start_hold = self
            .partial_start_hold
            .saturating_add(other.partial_start_hold);
        self.stale_hold = self.stale_hold.saturating_add(other.stale_hold);
        self.latched_beyond_measurement_window = self
            .latched_beyond_measurement_window
            .saturating_add(other.latched_beyond_measurement_window);
        self.maximum_decision_age_chips = self
            .maximum_decision_age_chips
            .max(other.maximum_decision_age_chips);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TpcEstimateCounters {
    pub groups: TpcGroupCounters,
    pub installed: u64,
    pub activated: u64,
    pub held_slots: u64,
    pub superseded: u64,
    pub maximum_observation_age_chips: u64,
    pub expired_slots: u64,
    pub last_expiry: Option<TpcExpiryEvidence>,
    pub last_rejection: Option<TpcRejectionEvidence>,
    pub algorithm2_control_groups: u64,
    pub algorithm2_hold_groups: u64,
    pub algorithm2_stale_hold_groups: u64,
    pub algorithm2_hold_slots: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpcRejectionEvidence {
    pub identity: ReceiveIdentity,
    pub observed_at: ChipInstant,
    pub frontier: ChipInstant,
    pub previous: Option<ChipInstant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpcEstimateTiming {
    pub observed_at: ChipInstant,
    pub valid_until: ChipInstant,
}
impl From<TpcEstimate> for TpcEstimateTiming {
    fn from(estimate: TpcEstimate) -> Self {
        Self {
            observed_at: estimate.observed_at,
            valid_until: estimate.valid_until,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpcExpiryEvidence {
    pub identity: ReceiveIdentity,
    pub requested: ChipInstant,
    pub active: TpcEstimateTiming,
    pub latest: Option<TpcEstimateTiming>,
    pub pending_updates: usize,
}
impl TpcEstimateCounters {
    pub(super) fn add(&mut self, other: Self) {
        self.groups.add(other.groups);
        if let Some(rejection) = other.last_rejection
            && self
                .last_rejection
                .is_none_or(|old| old.frontier.index <= rejection.frontier.index)
        {
            self.last_rejection = Some(rejection);
        }
        self.installed = self.installed.saturating_add(other.installed);
        self.activated = self.activated.saturating_add(other.activated);
        self.held_slots = self.held_slots.saturating_add(other.held_slots);
        self.superseded = self.superseded.saturating_add(other.superseded);
        self.maximum_observation_age_chips = self
            .maximum_observation_age_chips
            .max(other.maximum_observation_age_chips);
        self.expired_slots = self.expired_slots.saturating_add(other.expired_slots);
        self.algorithm2_control_groups = self
            .algorithm2_control_groups
            .saturating_add(other.algorithm2_control_groups);
        self.algorithm2_hold_groups = self
            .algorithm2_hold_groups
            .saturating_add(other.algorithm2_hold_groups);
        self.algorithm2_stale_hold_groups = self
            .algorithm2_stale_hold_groups
            .saturating_add(other.algorithm2_stale_hold_groups);
        self.algorithm2_hold_slots = self
            .algorithm2_hold_slots
            .saturating_add(other.algorithm2_hold_slots);
        if let Some(expiry) = other.last_expiry
            && self
                .last_expiry
                .is_none_or(|old| old.requested.index <= expiry.requested.index)
        {
            self.last_expiry = Some(expiry);
        }
    }
}

pub(super) struct EstimateLane {
    updates: [Option<TpcEstimate>; ESTIMATE_SLOTS],
    head: usize,
    len: usize,
    active: Option<TpcEstimate>,
    latest: Option<TpcEstimate>,
    explicit: bool,
    pub counters: TpcEstimateCounters,
}
impl Default for EstimateLane {
    fn default() -> Self {
        Self {
            updates: [None; ESTIMATE_SLOTS],
            head: 0,
            len: 0,
            active: None,
            latest: None,
            explicit: false,
            counters: TpcEstimateCounters::default(),
        }
    }
}
impl EstimateLane {
    pub fn has_measurement(&self) -> bool {
        self.latest.is_some()
    }
    pub fn admit_explicit(&mut self) -> Result<(), TransmitError> {
        if self.latest.is_some() {
            return Err(TransmitError::Conflict);
        }
        self.explicit = true;
        Ok(())
    }
    pub fn stage(&mut self, estimate: TpcEstimate, now: ChipInstant) -> Result<(), TransmitError> {
        if self.explicit {
            return Err(TransmitError::Conflict);
        }
        let horizon = now
            .index
            .checked_add(ESTIMATE_SLOTS as u64 * CHIPS_PER_SLOT)
            .ok_or(TransmitError::Time)?;
        if estimate.observed_at.index > horizon {
            self.record_rejection(estimate, now);
            return Err(TransmitError::FeedbackFuture);
        }
        if let Some(old) = self.latest
            && estimate.observed_at.index <= old.observed_at.index
        {
            self.record_rejection(estimate, now);
            return Err(TransmitError::FeedbackOrder);
        }
        if now.index >= estimate.valid_until.index {
            return Err(TransmitError::FeedbackLate);
        }
        if estimate.observed_at.index <= now.index {
            self.counters.superseded = self.counters.superseded.saturating_add(self.len as u64);
            self.head = 0;
            self.len = 0;
        } else if self.len == ESTIMATE_SLOTS {
            return Err(TransmitError::Capacity);
        }
        let tail = (self.head + self.len) % ESTIMATE_SLOTS;
        self.updates[tail] = Some(estimate);
        self.len += 1;
        self.latest = Some(estimate);
        self.counters.installed = self.counters.installed.saturating_add(1);
        Ok(())
    }

    fn record_rejection(&mut self, estimate: TpcEstimate, now: ChipInstant) {
        self.counters.last_rejection = Some(TpcRejectionEvidence {
            identity: estimate.identity,
            observed_at: estimate.observed_at,
            frontier: now,
            previous: self.latest.map(|old| old.observed_at),
        });
    }
    pub fn take(&mut self, start: ChipInstant) -> Option<TpcEstimate> {
        if self
            .latest
            .is_some_and(|latest| latest.observed_at.epoch != start.epoch)
        {
            return None;
        }
        let mut update = None;
        while self.len != 0 {
            let estimate = self.updates[self.head].expect("occupied measurement queue");
            if estimate.observed_at.index > start.index {
                break;
            }
            self.updates[self.head] = None;
            self.head = (self.head + 1) % ESTIMATE_SLOTS;
            self.len -= 1;
            if update.replace(estimate).is_some() {
                self.counters.superseded = self.counters.superseded.saturating_add(1);
            }
        }
        let estimate = update.or(self.active)?;
        if start.epoch != estimate.observed_at.epoch
            || start.index < estimate.observed_at.index
            || start.index >= estimate.valid_until.index
        {
            self.counters.expired_slots = self.counters.expired_slots.saturating_add(1);
            self.counters.last_expiry = Some(TpcExpiryEvidence {
                identity: estimate.identity,
                requested: start,
                active: self.active.unwrap_or(estimate).into(),
                latest: self.latest.map(Into::into),
                pending_updates: self.len,
            });
            return None;
        }
        if update.is_none() {
            self.counters.held_slots = self.counters.held_slots.saturating_add(1);
        } else {
            self.active = Some(estimate);
            self.counters.activated = self.counters.activated.saturating_add(1);
        }
        self.counters.maximum_observation_age_chips = self
            .counters
            .maximum_observation_age_chips
            .max(start.index - estimate.observed_at.index);
        Some(estimate)
    }
}
