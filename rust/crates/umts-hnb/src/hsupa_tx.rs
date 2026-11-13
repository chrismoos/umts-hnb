// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_tx::CommonTx,
    dedicated_rx::ReceiveIdentity,
    hsupa_rx::FeedbackRequest,
    radio_resources::{HsupaCodeLease, HsupaUserLease},
    tx_evidence::{TransportResolution, TransportSubmission},
};
use num_complex::Complex32;
use std::{
    mem::size_of,
    num::{NonZeroU64, NonZeroUsize},
};
use umts_dsp::{
    bits::Bit,
    hsupa::{AbsoluteGrant, GRANT_BITS, GrantSpreader, Indicator, IndicatorSpreader, Signature},
};
use umts_types::{
    UeKey,
    hsupa::Tti,
    time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant},
};

const USERS: usize = 40;
const MAX_PENDING: usize = 256;

mod commands;
pub(crate) mod control;
pub use control::{Control, ControlPort};
#[cfg(test)]
pub(crate) mod tests;
pub use commands::{Command, CommandOutcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Identity,
    Time,
    Capacity,
    Conflict,
    Unavailable,
    Amplitude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlId(NonZeroU64);

#[derive(Clone, Copy, Debug)]
pub struct Completion {
    pub id: ControlId,
    pub identity: ReceiveIdentity,
    pub feedback_tti: Option<u64>,
    pub end: ChipInstant,
    pub submitted: bool,
}

struct User {
    lease: HsupaUserLease,
    identity: ReceiveIdentity,
    interval: Tti,
    retiring: bool,
    last_end: ChipInstant,
    last_feedback: Option<u64>,
}
enum Waveform {
    Grant([Bit; GRANT_BITS]),
    Feedback {
        signature: Signature,
        indicator: Indicator,
        tti: u64,
    },
}
struct Pending {
    id: ControlId,
    identity: ReceiveIdentity,
    start: ChipInstant,
    end: ChipInstant,
    amplitude: f32,
    waveform: Waveform,
    cancelled: bool,
    submission: TransportSubmission,
    resolution: TransportResolution,
}

pub struct TransmitBank {
    lease: HsupaCodeLease,
    users: [Option<User>; USERS],
    pending: Box<[Option<Pending>]>,
    grant: GrantSpreader,
    indicator: IndicatorSpreader,
    next: ChipInstant,
    serial: Option<NonZeroU64>,
    progress: crate::tx_evidence::Reader,
    rate: usize,
    group_delay: usize,
}

impl TransmitBank {
    pub fn required_storage_bytes(capacity: NonZeroUsize) -> Result<usize, Error> {
        if capacity.get() > MAX_PENDING {
            return Err(Error::Capacity);
        }
        capacity
            .get()
            .checked_mul(size_of::<Option<Pending>>())
            .and_then(|bytes| {
                bytes.checked_add(
                    size_of::<Self>() + 2 * CHIPS_PER_FRAME as usize * size_of::<Complex32>(),
                )
            })
            .ok_or(Error::Capacity)
    }

    pub fn new(
        source: &CommonTx,
        lease: HsupaCodeLease,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
    ) -> Result<Box<Self>, (Error, HsupaCodeLease)> {
        if !Self::required_storage_bytes(capacity).is_ok_and(|bytes| bytes <= budget.get()) {
            return Err((Error::Capacity, lease));
        }
        let primary = source.information().primary_code();
        let grant = GrantSpreader::new(primary, lease.codes().absolute().index())
            .expect("leased E-AGCH code");
        let indicator = IndicatorSpreader::new(primary, lease.codes().indicator().index())
            .expect("leased E-HICH code");
        Ok(Box::new(Self {
            lease,
            users: std::array::from_fn(|_| None),
            pending: (0..capacity.get()).map(|_| None).collect(),
            grant,
            indicator,
            next: source.next_chip(),
            serial: Some(NonZeroU64::MIN),
            progress: source.submission_progress(),
            rate: source.samples_per_slot() / CHIPS_PER_SLOT as usize,
            group_delay: source.group_delay_samples(),
        }))
    }
    pub fn next_chip(&self) -> ChipInstant {
        self.next
    }
    pub(crate) fn matches_progress(&self, progress: &crate::tx_evidence::Reader) -> bool {
        self.progress.paired_with(progress)
    }
    pub(crate) fn paired_with(&self, source: &CommonTx) -> bool {
        self.next == source.next_chip() && self.matches_progress(&source.submission_progress())
    }
    pub fn register(
        &mut self,
        lease: HsupaUserLease,
        identity: ReceiveIdentity,
        interval: Tti,
    ) -> Result<(), (Error, HsupaUserLease)> {
        if !lease.belongs_to(&self.lease)
            || lease.key() != identity.ue
            || identity.ue.epoch != self.next.epoch
            || self
                .users
                .iter()
                .flatten()
                .any(|user| user.identity.ue == identity.ue)
        {
            return Err((Error::Identity, lease));
        }
        let Some(slot) = self.users.iter_mut().find(|slot| slot.is_none()) else {
            return Err((Error::Capacity, lease));
        };
        *slot = Some(User {
            lease,
            identity,
            interval,
            retiring: false,
            last_end: self.next,
            last_feedback: None,
        });
        Ok(())
    }
    fn user(&self, identity: ReceiveIdentity) -> Result<&User, Error> {
        self.users
            .iter()
            .flatten()
            .find(|user| user.identity == identity && !user.retiring)
            .ok_or(Error::Identity)
    }
    pub fn queue_grant(
        &mut self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        grant: AbsoluteGrant,
        amplitude: f32,
    ) -> Result<ControlId, Error> {
        let user = self.user(identity)?;
        if start
            .index
            .checked_sub(umts_dsp::hsupa::GRANT_FRAME_OFFSET_CHIPS)
            .is_none_or(|relative| !relative.is_multiple_of(user.interval.chips()))
        {
            return Err(Error::Time);
        }
        let bits = grant
            .encode(user.lease.e_rnti())
            .expect("validated grant and E-RNTI");
        let duration = user.interval.chips();
        self.enqueue(identity, start, duration, amplitude, Waveform::Grant(bits))
    }
    pub fn queue_feedback(
        &mut self,
        request: FeedbackRequest,
        amplitude: f32,
    ) -> Result<ControlId, Error> {
        let user = self.user(request.identity)?;
        let duration = match user.interval {
            Tti::TwoMilliseconds => 3,
            Tti::TenMilliseconds => 12,
        } * CHIPS_PER_SLOT;
        if request.timing.active_chips != duration
            || request.indicator == Indicator::Silent
            || user
                .last_feedback
                .is_some_and(|last| request.absolute_tti <= last)
        {
            return Err(Error::Time);
        }
        let signature = Signature::new(user.lease.controls().acknowledgement_signature())
            .expect("leased signature");
        let id = self.enqueue(
            request.identity,
            request.timing.start,
            duration,
            amplitude,
            Waveform::Feedback {
                signature,
                indicator: request.indicator,
                tti: request.absolute_tti,
            },
        )?;
        self.users
            .iter_mut()
            .flatten()
            .find(|user| user.identity == request.identity)
            .expect("registered owner")
            .last_feedback = Some(request.absolute_tti);
        Ok(id)
    }
    fn enqueue(
        &mut self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        duration: u64,
        amplitude: f32,
        waveform: Waveform,
    ) -> Result<ControlId, Error> {
        if !self.progress.available() {
            return Err(Error::Unavailable);
        }
        if start.epoch != self.next.epoch
            || start.index < self.next.index
            || !start.index.is_multiple_of(CHIPS_PER_SLOT)
        {
            return Err(Error::Time);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(Error::Amplitude);
        }
        let end = ChipInstant {
            index: start.index.checked_add(duration).ok_or(Error::Time)?,
            ..start
        };
        let is_grant = matches!(waveform, Waveform::Grant(_));
        if self.pending.iter().flatten().any(|pending| {
            !pending.cancelled
                && start.index < pending.end.index
                && pending.start.index < end.index
                && match (&pending.waveform, &waveform) {
                    (Waveform::Grant(_), _) => is_grant,
                    (Waveform::Feedback { .. }, Waveform::Feedback { .. }) => {
                        pending.identity == identity
                    }
                    _ => false,
                }
        }) {
            return Err(Error::Conflict);
        }
        let serial = self.serial.ok_or(Error::Capacity)?;
        let slot = self
            .pending
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Capacity)?;
        let submission = self
            .progress
            .interval_receipt(start, end, self.rate, self.group_delay)
            .ok_or(Error::Time)?;
        let resolution = self
            .progress
            .interval_resolution(start, end, self.rate, self.group_delay)
            .ok_or(Error::Time)?;
        let id = ControlId(serial);
        *slot = Some(Pending {
            id,
            identity,
            start,
            end,
            amplitude,
            waveform,
            cancelled: false,
            submission,
            resolution,
        });
        self.serial = serial.checked_add(1);
        let user = self
            .users
            .iter_mut()
            .flatten()
            .find(|user| user.identity == identity)
            .expect("registered owner");
        user.last_end.index = user.last_end.index.max(end.index);
        Ok(id)
    }
    pub fn mix_slot(&mut self, start: ChipInstant, output: &mut [Complex32]) -> Result<(), Error> {
        if start != self.next || output.len() != CHIPS_PER_SLOT as usize {
            return Err(Error::Time);
        }
        let next = start.index.checked_add(CHIPS_PER_SLOT).ok_or(Error::Time)?;
        for pending in self.pending.iter().flatten().filter(|pending| {
            !pending.cancelled
                && pending.start.index <= start.index
                && start.index < pending.end.index
        }) {
            let slot = ((start.index - pending.start.index) / CHIPS_PER_SLOT) as u8;
            match &pending.waveform {
                Waveform::Grant(bits) => {
                    self.grant
                        .add_slot(bits, start, slot, pending.amplitude, output)
                }
                Waveform::Feedback {
                    signature,
                    indicator,
                    ..
                } => self.indicator.add_slot(
                    start,
                    *signature,
                    slot,
                    *indicator,
                    pending.amplitude,
                    output,
                ),
            }
            .expect("prevalidated HSUPA control geometry");
        }
        self.next.index = next;
        Ok(())
    }
    pub fn take_completed(&mut self) -> Option<Completion> {
        let slot = self.pending.iter_mut().find(|slot| {
            slot.as_ref().is_some_and(|pending| {
                pending.cancelled
                    || (self.next.index >= pending.end.index
                        && (pending.resolution.resolved() || !pending.resolution.viable()))
            })
        })?;
        let pending = slot.take().expect("completed control");
        Some(Completion {
            id: pending.id,
            identity: pending.identity,
            end: pending.end,
            feedback_tti: match pending.waveform {
                Waveform::Feedback { tti, .. } => Some(tti),
                _ => None,
            },
            submitted: !pending.cancelled && pending.submission.submitted(),
        })
    }
    pub fn begin_retirement(&mut self, ue: UeKey) -> Result<(), Error> {
        let user = self
            .users
            .iter_mut()
            .flatten()
            .find(|user| user.identity.ue == ue)
            .ok_or(Error::Identity)?;
        user.retiring = true;
        for pending in
            self.pending.iter_mut().flatten().filter(|pending| {
                pending.identity.ue == ue && pending.start.index >= self.next.index
            })
        {
            pending.cancelled = true;
        }
        Ok(())
    }
    pub fn take_retired(
        &mut self,
        ue: UeKey,
        air_now: ChipInstant,
    ) -> Result<Option<HsupaUserLease>, Error> {
        if air_now.epoch != self.next.epoch {
            return Err(Error::Time);
        }
        let index = self
            .users
            .iter()
            .position(|user| {
                user.as_ref()
                    .is_some_and(|user| user.identity.ue == ue && user.retiring)
            })
            .ok_or(Error::Identity)?;
        if self
            .pending
            .iter()
            .flatten()
            .any(|pending| pending.identity.ue == ue)
            || air_now.index
                < self.users[index]
                    .as_ref()
                    .expect("retiring owner")
                    .last_end
                    .index
        {
            return Ok(None);
        }
        Ok(Some(
            self.users[index].take().expect("retiring owner").lease,
        ))
    }
    #[cfg(test)]
    pub fn release(self: Box<Self>) -> Result<HsupaCodeLease, Box<Self>> {
        if self.users.iter().any(Option::is_some) || self.pending.iter().any(Option::is_some) {
            return Err(self);
        }
        Ok(self.lease)
    }
}
