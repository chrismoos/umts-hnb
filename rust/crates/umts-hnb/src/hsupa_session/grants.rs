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
        effect: Effect,
    },
    Air {
        id: ControlId,
        grant: AbsoluteGrant,
        end: ChipInstant,
        effect: Effect,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    All,
    Activate(u8),
    Deactivate(u8),
}

// TS 25.212 Table 16B indexes absolute grants 0 to 31.
const MAX_GRANT_INDEX: u8 = 31;
const GRANT_TRANSMISSIONS: u8 = 4;
const GRANT_REPEAT_INTERVAL_CHIPS: u64 = umts_types::time::CHIPS_PER_SECOND / 50;
const UNHAPPY_TTIS_PER_STEP: u32 = 8;
const IDLE_CHIPS: u64 = 200 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const FEEDBACK_MAX_AGE_TTIS: u64 = 8;
const QUALITY_FAILURE_STEP: f64 = 0.05;
const QUALITY_SUCCESS_STEP: f64 = QUALITY_FAILURE_STEP / 19.0;
const QUALITY_MINIMUM_GRANT: u8 = 22;
// TS 25.321 11.8.1.3.1: a per-process INACTIVE grant deactivates its process and an all-process
// grant activates every one.
const INACTIVE_GRANT: u8 = 0;
const PROCESSES: u8 = 8;
const ALL_PROCESSES: u8 = 0xff;
const ALLOCATION_DWELL_CHIPS: u64 = 3 * umts_types::time::CHIPS_PER_SECOND;
const COLLISION_WINDOW_CHIPS: u64 = 4 * umts_types::time::CHIPS_PER_SECOND;
const SHARED_VALUE_INTERVAL_CHIPS: u64 = 2 * umts_types::time::CHIPS_PER_SECOND;
const ALLOCATION_REISSUE_CHIPS: u64 = 3 * umts_types::time::CHIPS_PER_SECOND;
const ALLOCATION_REISSUE_INTERVAL_CHIPS: u64 = 200 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const ALLOCATION_SETTLE_CHIPS: u64 = 600 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const DEMAND_HOLD_CHIPS: u64 = 2 * umts_types::time::CHIPS_PER_SECOND;
const DEMAND_WINDOW_CHIPS: u64 = 300 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const DEMAND_WINDOW_BITS: u64 = 20_000;
// TS 25.321 9.2.5.3.2 total buffer index. Bulk uploads report 31, acknowledgement streams under 18.
const BACKLOG_BUFFER_INDEX: u8 = 24;
const BACKLOG_FRESH_CHIPS: u64 = 300 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const USE_WINDOW_CHIPS: u64 = umts_types::time::CHIPS_PER_SECOND;
const DEMAND_GAP_CHIPS: u64 = 100 * (umts_types::time::CHIPS_PER_SECOND / 1000);
const DEMAND_UNHAPPY_TTIS: u32 = 8;
const DEMAND_SUSTAIN_CHIPS: u64 = umts_types::time::CHIPS_PER_SECOND;
const QUALITY_MAXIMUM_PATIENCE: u8 = 8;
const QUALITY_RAISE_WAIT_CHIPS: u64 = umts_types::time::CHIPS_PER_SECOND;
const QUALITY_CALM_CHIPS: u64 = 16 * umts_types::time::CHIPS_PER_SECOND;

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
    quality_seen: Option<(u64, u64)>,
    quality_credit: f64,
    quality_limit: u8,
    quality_patience: u8,
    raised_since_cut: bool,
    last_cut: Option<ChipInstant>,
    raise_after: Option<ChipInstant>,
    processes: u8,
    active: u8,
    valued: u8,
    last_value_change: Option<ChipInstant>,
    before_cuts: Option<(ChipInstant, u8)>,
    allocation: u8,
    reissue_until: Option<ChipInstant>,
    share_changed: Option<ChipInstant>,
    last_deactivation: Option<ChipInstant>,
    next_deactivation: u8,
    demand_window: Option<(ChipInstant, u64)>,
    use_window: Option<(ChipInstant, u64)>,
    used_bits_per_interval: Option<f64>,
    most_used_bits_per_interval: f64,
    backlog: bool,
    unhappy_demand: Option<ChipInstant>,
    demand_since: Option<ChipInstant>,
    demand_latest: Option<ChipInstant>,
    demand_until: Option<ChipInstant>,
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
            quality_seen: None,
            quality_credit: 0.0,
            quality_limit: value,
            quality_patience: 1,
            raised_since_cut: false,
            last_cut: None,
            raise_after: None,
            processes: ALL_PROCESSES,
            active: ALL_PROCESSES,
            valued: 0,
            last_value_change: None,
            before_cuts: None,
            allocation: ALL_PROCESSES,
            reissue_until: None,
            share_changed: None,
            last_deactivation: None,
            next_deactivation: 0,
            demand_window: None,
            use_window: None,
            used_bits_per_interval: None,
            most_used_bits_per_interval: 0.0,
            backlog: false,
            unhappy_demand: None,
            demand_since: None,
            demand_latest: None,
            demand_until: None,
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

    fn reissue_due(&self, now: ChipInstant) -> bool {
        self.reissue_until
            .is_some_and(|until| until.epoch == now.epoch && now.index < until.index)
            && self.last_submitted_end.is_none_or(|previous| {
                previous.epoch != now.epoch
                    || now.index.saturating_sub(previous.index) >= ALLOCATION_REISSUE_INTERVAL_CHIPS
            })
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
        self.observe_demand(now, activity);
        self.observe_quality(now, activity);
        self.adapt(now, activity);
    }

    fn observe_demand(&mut self, now: ChipInstant, activity: crate::hsupa_rx::GrantActivity) {
        if let Some((at, happy)) = activity.happy {
            self.unhappy_demand =
                (!happy && activity.unhappy_streak >= DEMAND_UNHAPPY_TTIS).then_some(at);
        }
        let unhappy = self.unhappy_demand.is_some_and(|at| {
            at.epoch == now.epoch
                && now
                    .index
                    .checked_sub(at.index)
                    .is_some_and(|age| age <= DEMAND_GAP_CHIPS)
        });
        self.backlog = unhappy
            || activity.scheduling.is_some_and(|report| {
                report.information.total_buffer_index >= BACKLOG_BUFFER_INDEX
                    && report.start.epoch == now.epoch
                    && now
                        .index
                        .checked_sub(report.start.index)
                        .is_some_and(|age| age <= BACKLOG_FRESH_CHIPS)
            });
        match self.use_window {
            Some((start, bits))
                if start.epoch == now.epoch
                    && now.index >= start.index
                    && activity.payload_bits >= bits =>
            {
                let elapsed = now.index - start.index;
                if elapsed >= USE_WINDOW_CHIPS {
                    let intervals = (elapsed / self.interval.chips()) as f64
                        * f64::from(self.allocation.count_ones())
                        / f64::from(PROCESSES);
                    let used = (activity.payload_bits - bits) as f64 / intervals.max(1.0);
                    self.used_bits_per_interval = Some(used);
                    self.most_used_bits_per_interval = self.most_used_bits_per_interval.max(used);
                    self.use_window = Some((now, activity.payload_bits));
                    tracing::debug!(
                        ue = ?self.identity.ue,
                        allocation = self.allocation,
                        used = ?self.used_bits_per_interval,
                        backlog = self.backlog,
                        buffer = ?activity.scheduling.map(|report| report.information.total_buffer_index),
                        headroom = ?activity.scheduling.map(|report| report.information.power_headroom_index),
                        "Uplink share use"
                    );
                }
            }
            _ => self.use_window = Some((now, activity.payload_bits)),
        }
        let mut delivered = false;
        match self.demand_window {
            Some((start, bits))
                if start.epoch == now.epoch
                    && now.index >= start.index
                    && activity.payload_bits >= bits =>
            {
                if now.index - start.index >= DEMAND_WINDOW_CHIPS {
                    delivered = activity.payload_bits - bits >= DEMAND_WINDOW_BITS;
                    self.demand_window = Some((now, activity.payload_bits));
                }
            }
            _ => self.demand_window = Some((now, activity.payload_bits)),
        }
        if (delivered || self.backlog)
            && let Some(index) = now.index.checked_add(DEMAND_HOLD_CHIPS)
        {
            let lapsed = self
                .demand_until
                .is_none_or(|until| until.epoch != now.epoch || now.index > until.index);
            if lapsed {
                self.demand_since = Some(now);
            }
            self.demand_latest = Some(now);
            self.demand_until = Some(ChipInstant { index, ..now });
        }
    }

    pub fn demand(&self, now: ChipInstant) -> bool {
        let sustained = self
            .demand_since
            .zip(self.demand_latest)
            .is_some_and(|(since, latest)| {
                since.epoch == latest.epoch
                    && latest.index.saturating_sub(since.index) >= DEMAND_SUSTAIN_CHIPS
            });
        sustained
            && self
                .demand_until
                .is_some_and(|until| until.epoch == now.epoch && now.index <= until.index)
    }

    pub fn backlog(&self) -> bool {
        self.backlog
    }

    pub fn used_bits_per_interval(&self) -> Option<f64> {
        self.used_bits_per_interval
    }

    pub fn most_used_bits_per_interval(&self) -> f64 {
        self.most_used_bits_per_interval
    }

    pub fn allocation(&self) -> u8 {
        self.allocation
    }

    pub fn allocate(&mut self, now: ChipInstant, processes: u8) -> bool {
        if processes == self.allocation || processes == 0 {
            return false;
        }
        let recent = self.share_changed.is_some_and(|changed| {
            changed.epoch == now.epoch
                && now.index.saturating_sub(changed.index) < ALLOCATION_DWELL_CHIPS
        });
        if recent {
            return false;
        }
        tracing::debug!(ue = ?self.identity.ue, from = self.allocation, to = processes, "Uplink process allocation");
        if let Some((since, value)) = self.before_cuts.take()
            && since.epoch == now.epoch
            && now.index.saturating_sub(since.index) <= COLLISION_WINDOW_CHIPS
            && value > self.desired.value()
            && let Ok(grant) = AbsoluteGrant::new(value, GrantScope::AllProcesses)
        {
            self.desired = grant;
            self.quality_limit = self.quality_limit.max(value).min(self.ceiling);
            self.quality_patience = 1;
            self.raise_after = None;
        }
        self.quality_credit = 0.0;
        self.allocation = processes;
        self.use_window = None;
        self.used_bits_per_interval = None;
        self.share_changed = Some(now);
        self.reissue_until = now
            .index
            .checked_add(ALLOCATION_REISSUE_CHIPS)
            .map(|index| ChipInstant { index, ..now });
        true
    }

    fn observe_quality(&mut self, now: ChipInstant, activity: crate::hsupa_rx::GrantActivity) {
        let totals = (activity.first_decoded, activity.first_failed);
        let Some(previous) = self.quality_seen.replace(totals) else {
            return;
        };
        let settling = self.share_changed.is_some_and(|changed| {
            changed.epoch == now.epoch
                && now
                    .index
                    .checked_sub(changed.index)
                    .is_some_and(|age| age < ALLOCATION_SETTLE_CHIPS)
        });
        if settling {
            return;
        }
        let (Some(decoded), Some(failed)) = (
            totals.0.checked_sub(previous.0),
            totals.1.checked_sub(previous.1),
        ) else {
            self.quality_credit = 0.0;
            return;
        };
        self.quality_credit = (self.quality_credit + decoded as f64 * QUALITY_SUCCESS_STEP
            - failed as f64 * QUALITY_FAILURE_STEP)
            .clamp(-2.0, 1.0);
    }

    fn adapt_quality(&mut self, now: ChipInstant) -> bool {
        let value = self.desired.value();
        let step = if self.quality_credit <= -1.0 {
            self.quality_credit = 0.0;
            if value <= QUALITY_MINIMUM_GRANT.min(self.floor) {
                return false;
            }
            // The value is the grant before this descent and the time is its latest cut.
            let before = self
                .before_cuts
                .filter(|(last, _)| last.epoch == now.epoch)
                .map_or(value, |(_, before)| before.max(value));
            self.before_cuts = Some((now, before));
            self.quality_limit = value - 1;
            self.raise_after = now
                .index
                .checked_add(u64::from(self.quality_patience) * QUALITY_RAISE_WAIT_CHIPS)
                .map(|index| ChipInstant { index, ..now });
            // Only a cut that follows a raise is a failed probe. Further cuts of one descent are not.
            if self.raised_since_cut {
                self.quality_patience = (self.quality_patience * 2).min(QUALITY_MAXIMUM_PATIENCE);
            }
            self.raised_since_cut = false;
            self.last_cut = Some(now);
            self.counters.lowered = self.counters.lowered.saturating_add(1);
            value - 1
        } else if self.quality_credit >= 1.0
            && self
                .raise_after
                .is_none_or(|after| after.epoch != now.epoch || now.index >= after.index)
        {
            self.quality_credit = 0.0;
            if self.last_cut.is_none_or(|cut| {
                cut.epoch != now.epoch || now.index.saturating_sub(cut.index) >= QUALITY_CALM_CHIPS
            }) {
                self.quality_patience = 1;
            }
            self.quality_limit = self.quality_limit.saturating_add(1).min(self.ceiling);
            if value >= self.quality_limit.min(self.floor) {
                return false;
            }
            self.counters.raised = self.counters.raised.saturating_add(1);
            self.raised_since_cut = true;
            self.before_cuts = self.before_cuts.filter(|(_, before)| value + 1 < *before);
            value + 1
        } else {
            return false;
        };
        let Ok(grant) = AbsoluteGrant::new(step, GrantScope::AllProcesses) else {
            return false;
        };
        tracing::debug!(ue = ?self.identity.ue, grant = step, "Grant moved by first-attempt quality");
        self.desired = grant;
        self.last_adjustment = Some(now);
        true
    }

    /// Grant adaptation between `floor` (the initial grant) and `ceiling` (the configured
    pub(super) fn with_bounds(mut self, floor: u8, ceiling: u8) -> Result<Self, Error> {
        if floor > ceiling || ceiling > MAX_GRANT_INDEX {
            return Err(Error::Conflict);
        }
        self.floor = floor;
        self.ceiling = ceiling;
        self.quality_limit = ceiling;
        Ok(self)
    }

    // TS 25.321 11.8.1.5: unhappy already indicates buffered demand and power headroom.
    fn adapt(&mut self, now: ChipInstant, activity: crate::hsupa_rx::GrantActivity) {
        if now.epoch != self.identity.ue.epoch || self.quiescing.is_some() {
            return;
        }
        if self.adapt_quality(now) || self.pending.is_some() || self.floor == self.ceiling {
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
        if value < self.quality_limit
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
        let periodic = self.periodic_refresh_due(now) || self.reissue_due(now);
        let repeated = self.repeat_due(now);
        let shared = self.processes != ALL_PROCESSES;
        let refresh = !shared
            && self.submitted == Some(self.desired)
            && self.desired.value() != 0
            && (self.recovery.due(now) || periodic || repeated);
        let surplus = self.active & !self.processes;
        let missing = self.processes & !self.active;
        if shared && self.submitted.is_some() && self.submitted != Some(self.desired) {
            let paced = self.last_value_change.is_none_or(|previous| {
                previous.epoch != now.epoch
                    || now.index.saturating_sub(previous.index) >= SHARED_VALUE_INTERVAL_CHIPS
            });
            if paced && self.valued != 0 {
                self.valued = 0;
                self.last_value_change = Some(now);
            }
        }
        let unvalued = self.processes & !self.valued;
        #[derive(PartialEq)]
        enum Next {
            All,
            Activate,
            Deactivate,
        }
        let (candidates, next) = if !shared {
            if missing != 0 || self.submitted != Some(self.desired) || refresh {
                (ALL_PROCESSES, Next::All)
            } else {
                return Ok(GrantProgress::Submitted);
            }
        } else if surplus != 0 {
            (surplus, Next::Deactivate)
        } else if unvalued != 0 {
            (unvalued, Next::Activate)
        } else {
            return Ok(GrantProgress::Submitted);
        };
        let deactivate = next == Next::Deactivate;
        let earliest = next_transmit
            .index
            .max(now.index)
            .max(self.activation.index)
            .max(GRANT_FRAME_OFFSET_CHIPS);
        let chips = self.interval.chips();
        let remainder = (earliest - GRANT_FRAME_OFFSET_CHIPS) % chips;
        let boundary = earliest
            .checked_add(if remainder == 0 { 0 } else { chips - remainder })
            .ok_or(Error::Time)?;
        let (grant, effect, index) = if next == Next::All {
            (self.desired, Effect::All, boundary)
        } else {
            let position =
                ((boundary - GRANT_FRAME_OFFSET_CHIPS) / chips % u64::from(PROCESSES)) as u8;
            let wait = (0..PROCESSES)
                .find(|wait| candidates & (1 << ((position + wait) % PROCESSES)) != 0)
                .ok_or(Error::Conflict)?;
            let target = (position + wait) % PROCESSES;
            let index = boundary
                .checked_add(u64::from(wait) * chips)
                .ok_or(Error::Time)?;
            if deactivate {
                let grant = AbsoluteGrant::new(INACTIVE_GRANT, GrantScope::PerProcess)
                    .map_err(|_| Error::Conflict)?;
                (grant, Effect::Deactivate(target), index)
            } else {
                let grant = AbsoluteGrant::new(self.desired.value(), GrantScope::PerProcess)
                    .map_err(|_| Error::Conflict)?;
                (grant, Effect::Activate(target), index)
            }
        };
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
            grant,
            amplitude: self.amplitude,
        };
        let Ok(id) = control.queue_hsupa(command) else {
            return Ok(GrantProgress::Backpressure);
        };
        tracing::debug!(ue = ?self.identity.ue, ?effect, value = grant.value(), start = start.index, position = (start.index - GRANT_FRAME_OFFSET_CHIPS) / chips % u64::from(PROCESSES), "Grant command");
        self.pending = Some(Pending::Command {
            id,
            grant: self.desired,
            end,
            effect,
        });
        if refresh && !deactivate {
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
        let Some(Pending::Command {
            id,
            grant,
            end,
            effect,
        }) = self.pending
        else {
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
                self.pending = Some(Pending::Air {
                    id,
                    grant,
                    end,
                    effect,
                });
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
        let Some(Pending::Air {
            id,
            grant,
            end,
            effect,
        }) = self.pending
        else {
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
        if let Effect::Deactivate(position) = effect {
            if completion.submitted {
                self.active &= !(1 << position);
                self.last_deactivation = Some(end);
                self.next_deactivation = (position + 1) % PROCESSES;
            }
            return Ok(());
        }
        if completion.submitted {
            match effect {
                Effect::All => self.active = ALL_PROCESSES,
                Effect::Activate(position) => {
                    self.active |= 1 << position;
                    self.valued |= 1 << position;
                }
                Effect::Deactivate(_) => (),
            }
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
