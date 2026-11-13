// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_rx::ReceiveIdentity,
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitEvent},
    hsupa_tx::{Command, CommandOutcome, Completion, Control, ControlId, Error},
};
use std::num::NonZeroU64;
use umts_dsp::hsupa::{AbsoluteGrant, GRANT_FRAME_OFFSET_CHIPS, GrantScope};
use umts_types::{hsupa::Tti, time::ChipInstant};

mod recovery;
use recovery::Recovery;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
enum Pending {
    Command {
        id: TransmitCommandId,
        grant: AbsoluteGrant,
        end: ChipInstant,
    },
    Air {
        id: ControlId,
        grant: AbsoluteGrant,
        end: ChipInstant,
    },
}

// TS 25.212 Table 16B indexes absolute grants 0 to 31.
const MAX_GRANT_INDEX: u8 = 31;
const GRANT_TRANSMISSIONS: u8 = 4;
const GRANT_REPEAT_INTERVAL_CHIPS: u64 = umts_types::time::CHIPS_PER_SECOND / 50;
const UNHAPPY_TTIS_PER_STEP: u32 = 8;
const IDLE_CHIPS: u64 = 200 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const FEEDBACK_MAX_AGE_TTIS: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantProgress {
    Quiesced,
    Pending,
    Submitted,
    Queued,
    Backpressure,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GrantCounters {
    pub queued: u64,
    pub admitted: u64,
    pub rejected: u64,
    pub submitted: u64,
    pub lost: u64,
    pub recovery_queued: u64,
    pub periodic_queued: u64,
    pub repeated_queued: u64,
    pub raised: u64,
    pub lowered: u64,
}

pub struct Grants {
    identity: ReceiveIdentity,
    interval: Tti,
    activation: ChipInstant,
    amplitude: f32,
    desired: AbsoluteGrant,
    submitted: Option<AbsoluteGrant>,
    pending: Option<Pending>,
    failure: Option<Error>,
    counters: GrantCounters,
    quiescing: Option<ChipInstant>,
    recovery: Recovery,
    refresh_period: Option<NonZeroU64>,
    last_submitted_end: Option<ChipInstant>,
    grant_transmissions: u8,
    floor: u8,
    ceiling: u8,
    adjusted_streak: u32,
    last_feedback: Option<ChipInstant>,
    last_adjustment: Option<ChipInstant>,
}

impl Grants {
    pub fn new(
        identity: ReceiveIdentity,
        interval: Tti,
        activation: ChipInstant,
        amplitude: f32,
        value: u8,
    ) -> Result<Self, Error> {
        if activation.epoch != identity.ue.epoch {
            return Err(Error::Identity);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(Error::Amplitude);
        }
        let desired =
            AbsoluteGrant::new(value, GrantScope::AllProcesses).map_err(|_| Error::Conflict)?;
        Ok(Self {
            identity,
            interval,
            activation,
            amplitude,
            desired,
            floor: value,
            ceiling: value,
            adjusted_streak: 0,
            last_feedback: None,
            last_adjustment: None,
            submitted: None,
            pending: None,
            failure: None,
            counters: GrantCounters::default(),
            quiescing: None,
            recovery: Recovery::default(),
            refresh_period: None,
            last_submitted_end: None,
            grant_transmissions: 0,
        })
    }

    pub(super) fn with_refresh_period(mut self, period: Option<NonZeroU64>) -> Result<Self, Error> {
        if period.is_some_and(|period| period.get() < self.interval.chips()) {
            return Err(Error::Time);
        }
        self.refresh_period = period;
        Ok(self)
    }

    fn periodic_refresh_due(&self, now: ChipInstant) -> bool {
        let (Some(period), Some(previous)) = (self.refresh_period, self.last_submitted_end) else {
            return false;
        };
        previous.epoch == now.epoch
            && now
                .index
                .checked_sub(previous.index)
                .is_some_and(|age| age >= period.get())
            && self.recovery.has_fresh_demand(now)
    }

    // TS 25.321 11.8.1.3.1 permits repeating an allocation without changing its value.
    // Driver submission does not prove that the UE decoded the unacknowledged E-AGCH.
    fn repeat_due(&self, now: ChipInstant) -> bool {
        self.grant_transmissions < GRANT_TRANSMISSIONS
            && self.last_submitted_end.is_some_and(|previous| {
                previous.epoch == now.epoch
                    && now
                        .index
                        .checked_sub(previous.index)
                        .is_some_and(|age| age >= GRANT_REPEAT_INTERVAL_CHIPS)
            })
    }

    pub fn counters(&self) -> GrantCounters {
        self.counters
    }

    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn observe_uplink(&mut self, now: ChipInstant, activity: crate::hsupa_rx::GrantActivity) {
        self.recovery.observe(activity.data, activity.scheduling);
        self.adapt(now, activity);
    }

    /// Grant adaptation between `floor` (the initial grant) and `ceiling` (the configured
    /// maximum). With equal bounds the grant stays fixed.
    pub(super) fn with_bounds(mut self, floor: u8, ceiling: u8) -> Result<Self, Error> {
        if floor > ceiling || ceiling > MAX_GRANT_INDEX {
            return Err(Error::Conflict);
        }
        self.floor = floor;
        self.ceiling = ceiling;
        Ok(self)
    }

    // TS 25.321 11.8.1.5: unhappy already indicates buffered demand and power headroom.
    fn adapt(&mut self, now: ChipInstant, activity: crate::hsupa_rx::GrantActivity) {
        if self.floor == self.ceiling
            || now.epoch != self.identity.ue.epoch
            || self.pending.is_some()
            || self.quiescing.is_some()
        {
            return;
        }
        let value = self.desired.value();
        let age = |at: ChipInstant| {
            (at.epoch == now.epoch)
                .then(|| now.index.checked_sub(at.index))
                .flatten()
        };
        let scheduling = activity
            .scheduling
            .filter(|report| age(report.start).is_some_and(|age| age < IDLE_CHIPS));
        let happy = activity.happy.filter(|(at, _)| {
            age(*at).is_some_and(|age| age <= self.interval.chips() * FEEDBACK_MAX_AGE_TTIS)
        });
        let fresh_unhappy = happy.is_some_and(|(_, happy)| !happy);
        let new_feedback = happy
            .is_some_and(|(at, _)| self.last_feedback.is_none_or(|last| at.index > last.index));
        if new_feedback {
            self.last_feedback = happy.map(|(at, _)| at);
        }
        if activity.unhappy_streak < self.adjusted_streak {
            self.adjusted_streak = 0;
        }
        if value < self.ceiling
            && new_feedback
            && fresh_unhappy
            && happy.is_some_and(|(at, _)| {
                self.last_adjustment
                    .is_none_or(|last| at.index > last.index)
                    && scheduling.is_none_or(|report| {
                        report.start.index < at.index || report.information.total_buffer_index != 0
                    })
            })
            && activity.unhappy_streak.saturating_sub(self.adjusted_streak) >= UNHAPPY_TTIS_PER_STEP
            && let Ok(grant) = AbsoluteGrant::new(value + 1, GrantScope::AllProcesses)
        {
            self.desired = grant;
            self.adjusted_streak = activity.unhappy_streak;
            self.last_adjustment = Some(now);
            self.counters.raised = self.counters.raised.saturating_add(1);
            return;
        }
        let idle =
            age(activity.data.unwrap_or(self.activation)).is_some_and(|age| age >= IDLE_CHIPS);
        let settled = age(self.last_adjustment.unwrap_or(self.activation))
            .is_some_and(|age| age >= IDLE_CHIPS);
        if value > self.floor
            && idle
            && settled
            && !fresh_unhappy
            && scheduling.is_none_or(|report| report.information.total_buffer_index == 0)
            && let Ok(grant) = AbsoluteGrant::new(value - 1, GrantScope::AllProcesses)
        {
            self.desired = grant;
            self.adjusted_streak = activity.unhappy_streak;
            self.last_adjustment = Some(now);
            self.counters.lowered = self.counters.lowered.saturating_add(1);
        }
    }

    #[cfg(test)]
    pub fn request(&mut self, value: u8) -> Result<(), Error> {
        if self.quiescing.is_some() {
            return Err(Error::Unavailable);
        }
        let grant =
            AbsoluteGrant::new(value, GrantScope::AllProcesses).map_err(|_| Error::Conflict)?;
        self.desired = grant;
        Ok(())
    }

    pub fn quiesce_before(&mut self, boundary: ChipInstant) -> Result<(), Error> {
        if boundary.epoch != self.identity.ue.epoch {
            return Err(Error::Identity);
        }
        if boundary.index <= self.activation.index {
            return Err(Error::Time);
        }
        if let Some(previous) = self.quiescing {
            return if previous == boundary {
                Ok(())
            } else {
                Err(Error::Conflict)
            };
        }
        if self.pending.is_some_and(|pending| {
            let end = match pending {
                Pending::Command { end, .. } | Pending::Air { end, .. } => end,
            };
            end.index > boundary.index
        }) {
            return Err(Error::Time);
        }
        self.quiescing = Some(boundary);
        Ok(())
    }

    pub fn service(
        &mut self,
        now: ChipInstant,
        next_transmit: ChipInstant,
        control: &mut impl Control,
    ) -> Result<GrantProgress, Error> {
        if now.epoch != self.identity.ue.epoch
            || next_transmit.epoch != now.epoch
            || control.epoch() != now.epoch
        {
            return Err(Error::Identity);
        }
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !control.available() {
            return Err(Error::Unavailable);
        }
        if self.pending.is_some() {
            return Ok(GrantProgress::Pending);
        }
        if self.quiescing.is_some() {
            return Ok(GrantProgress::Quiesced);
        }
        let periodic = self.periodic_refresh_due(now);
        let repeated = self.repeat_due(now);
        let refresh = self.submitted == Some(self.desired)
            && self.desired.value() != 0
            && (self.recovery.due(now) || periodic || repeated);
        if self.submitted == Some(self.desired) && !refresh {
            return Ok(GrantProgress::Submitted);
        }
        let earliest = next_transmit
            .index
            .max(now.index)
            .max(self.activation.index)
            .max(GRANT_FRAME_OFFSET_CHIPS);
        let chips = self.interval.chips();
        let remainder = (earliest - GRANT_FRAME_OFFSET_CHIPS) % chips;
        let index = earliest
            .checked_add(if remainder == 0 { 0 } else { chips - remainder })
            .ok_or(Error::Time)?;
        let start = ChipInstant {
            index,
            epoch: now.epoch,
        };
        let end = ChipInstant {
            index: index.checked_add(chips).ok_or(Error::Time)?,
            ..start
        };
        let command = Command::Grant {
            identity: self.identity,
            start,
            grant: self.desired,
            amplitude: self.amplitude,
        };
        let Ok(id) = control.queue_hsupa(command) else {
            return Ok(GrantProgress::Backpressure);
        };
        self.pending = Some(Pending::Command {
            id,
            grant: self.desired,
            end,
        });
        if refresh {
            if repeated {
                self.counters.repeated_queued = self.counters.repeated_queued.saturating_add(1);
            } else if periodic {
                self.counters.periodic_queued = self.counters.periodic_queued.saturating_add(1);
            } else {
                self.recovery.queued(start);
                self.counters.recovery_queued = self.counters.recovery_queued.saturating_add(1);
            }
        }
        self.counters.queued = self.counters.queued.saturating_add(1);
        Ok(GrantProgress::Queued)
    }

    pub fn route_command(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        let Some(Pending::Command { id, grant, end }) = self.pending else {
            return Some(event);
        };
        if event.id != id {
            return Some(event);
        }
        let TransmitCommandOutcome::Hsupa(CommandOutcome::Queued { identity, result }) =
            &event.outcome
        else {
            return Some(event);
        };
        if *identity != self.identity {
            return Some(event);
        }
        match *result {
            Ok(id) => {
                self.pending = Some(Pending::Air { id, grant, end });
                self.counters.admitted = self.counters.admitted.saturating_add(1);
            }
            Err(error) => {
                self.pending = None;
                self.counters.rejected = self.counters.rejected.saturating_add(1);
                if !matches!(error, Error::Capacity | Error::Conflict | Error::Time) {
                    self.failure = Some(error);
                }
            }
        }
        None
    }

    pub fn accept_completion(&mut self, completion: Completion) -> Result<(), Completion> {
        let Some(Pending::Air { id, grant, end }) = self.pending else {
            return Err(completion);
        };
        if completion.id != id
            || completion.identity != self.identity
            || completion.end != end
            || completion.feedback_tti.is_some()
        {
            return Err(completion);
        }
        self.pending = None;
        if completion.submitted {
            self.grant_transmissions = if self.submitted == Some(grant) {
                self.grant_transmissions
                    .saturating_add(1)
                    .min(GRANT_TRANSMISSIONS)
            } else {
                1
            };
            self.submitted = Some(grant);
            self.last_submitted_end = Some(end);
            self.counters.submitted = self.counters.submitted.saturating_add(1);
        } else {
            // A partially transmitted replacement cannot establish which grant the UE retained.
            self.submitted = None;
            self.counters.lost = self.counters.lost.saturating_add(1);
        }
        Ok(())
    }
}
