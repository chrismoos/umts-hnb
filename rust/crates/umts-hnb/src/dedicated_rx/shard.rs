// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    DedicatedReceiveSession, FrameAnchor, ReceiveError, ReceiveIdentity, ReceiveResources,
};
use std::num::NonZeroUsize;
use umts_types::RadioEpoch;

pub(super) const MAX_SHARD_UES: usize = 4096;
pub(super) const CONFIGURATIONS_PER_UE: usize = 2;

#[derive(Clone, Copy)]
pub(super) enum PhysicalChange {
    Grow(usize),
    Shrink(usize),
}
impl PhysicalChange {
    fn apply(self, bytes: usize) -> Option<usize> {
        match self {
            Self::Grow(add) => bytes.checked_add(add),
            Self::Shrink(remove) => bytes.checked_sub(remove),
        }
    }
}

pub trait ReceiveOwner {
    fn session(&self) -> &DedicatedReceiveSession;
    fn session_mut(&mut self) -> &mut DedicatedReceiveSession;
    fn service_enabled(&self) -> bool {
        true
    }
    fn last_consumed_frame(&self) -> Option<umts_types::time::ChipInstant> {
        self.session().last
    }
    fn resources(&self) -> ReceiveResources {
        self.session().resources()
    }
}
impl ReceiveOwner for DedicatedReceiveSession {
    fn session(&self) -> &DedicatedReceiveSession {
        self
    }
    fn session_mut(&mut self) -> &mut DedicatedReceiveSession {
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ShardLimits {
    pub maximum_ues: NonZeroUsize,
    pub maximum_sessions_per_step: NonZeroUsize,
    pub resources: ReceiveResources,
}
struct Entry<T> {
    last: Option<ReceiveIdentity>,
    boundary: Option<RetiredBoundary>,
    retired_last: Option<umts_types::time::ChipInstant>,
    owners: [Option<Owner<T>>; CONFIGURATIONS_PER_UE],
}
struct Owner<T> {
    session: Box<T>,
    failure_reported: bool,
}
#[derive(Clone, Copy)]
struct RetiredBoundary {
    anchor: FrameAnchor,
    last: Option<umts_types::time::ChipInstant>,
    longest: usize,
}
pub struct DedicatedRxShard<T: ReceiveOwner = DedicatedReceiveSession> {
    epoch: RadioEpoch,
    limits: ShardLimits,
    entries: Vec<Entry<T>>,
    used: ReceiveResources,
    cursor: usize,
}
impl<T: ReceiveOwner> DedicatedRxShard<T> {
    pub fn new(epoch: RadioEpoch, limits: ShardLimits) -> Result<Self, ReceiveError> {
        if limits.maximum_ues.get() > MAX_SHARD_UES {
            return Err(ReceiveError::Capacity);
        }
        Ok(Self {
            epoch,
            limits,
            entries: (0..limits.maximum_ues.get())
                .map(|_| Entry {
                    last: None,
                    boundary: None,
                    retired_last: None,
                    owners: std::array::from_fn(|_| None),
                })
                .collect(),
            used: ReceiveResources::default(),
            cursor: 0,
        })
    }
    #[cfg(test)]
    pub fn used(&self) -> ReceiveResources {
        self.used
    }

    pub(super) fn owners(&self) -> impl Iterator<Item = &T> {
        self.entries
            .iter()
            .flat_map(|entry| entry.owners.iter().flatten())
            .map(|owner| owner.session.as_ref())
    }

    // The callback must leave the owner unchanged on failure and apply exactly this charge on success.
    pub(super) fn change_physical_with(
        &mut self,
        identity: ReceiveIdentity,
        change: PhysicalChange,
        apply: impl FnOnce(&mut T) -> Result<(), ReceiveError>,
    ) -> Result<(), ReceiveError> {
        let total = change
            .apply(self.used.physical_sample_bytes)
            .filter(|total| *total <= self.limits.resources.physical_sample_bytes)
            .ok_or(ReceiveError::Capacity)?;
        let owner = self.session_mut(identity).ok_or(ReceiveError::Identity)?;
        let before = owner.resources();
        let expected = ReceiveResources {
            physical_sample_bytes: change
                .apply(before.physical_sample_bytes)
                .ok_or(ReceiveError::Capacity)?,
            ..before
        };
        let result = apply(owner);
        assert_eq!(
            owner.resources(),
            if result.is_ok() { expected } else { before },
            "transactional receive resource growth"
        );
        result?;
        self.used.physical_sample_bytes = total;
        Ok(())
    }
    pub(super) fn owner_at_index_mut(&mut self, index: usize) -> Option<&mut T> {
        self.entries.get_mut(index / CONFIGURATIONS_PER_UE)?.owners[index % CONFIGURATIONS_PER_UE]
            .as_mut()
            .map(|owner| owner.session.as_mut())
    }
    pub fn install(&mut self, session: Box<T>) -> Result<(), (ReceiveError, Box<T>)> {
        self.admit(None, session)
    }
    pub fn stage_replacement(
        &mut self,
        previous: ReceiveIdentity,
        session: Box<T>,
    ) -> Result<(), (ReceiveError, Box<T>)> {
        self.admit(Some(previous), session)
    }
    fn admit(
        &mut self,
        previous: Option<ReceiveIdentity>,
        session: Box<T>,
    ) -> Result<(), (ReceiveError, Box<T>)> {
        let identity = session.session().identity();
        if identity.ue.epoch != self.epoch {
            return Err((ReceiveError::Identity, session));
        }
        let Some(entry) = self.entries.get_mut(identity.ue.slot as usize) else {
            return Err((ReceiveError::Capacity, session));
        };
        if let Some(previous) = previous {
            if identity.ue != previous.ue {
                return Err((ReceiveError::Identity, session));
            }
            let Some(old) = entry
                .owners
                .iter()
                .flatten()
                .find(|owner| owner.session.session().identity() == previous)
            else {
                return Err((ReceiveError::Identity, session));
            };
            if old.session.session().failed || old.session.session().retired {
                return Err((ReceiveError::Failed, session));
            }
            let mut previous_boundary = boundary(old.session.as_ref());
            previous_boundary.last = latest(previous_boundary.last, entry.retired_last);
            if !valid_replacement(previous_boundary, session.session()) {
                return Err((ReceiveError::Time, session));
            }
        } else if entry.owners.iter().any(Option::is_some) {
            return Err((ReceiveError::Occupied, session));
        }
        let Some(index) = entry.owners.iter().position(Option::is_none) else {
            return Err((ReceiveError::Occupied, session));
        };
        if session.session().retired || session.session().failed {
            return Err((ReceiveError::Retired, session));
        }
        if entry.last.is_some_and(|last| {
            identity.ue.generation < last.ue.generation
                || identity.ue.generation == last.ue.generation
                    && identity.configuration <= last.configuration
        }) {
            return Err((ReceiveError::Identity, session));
        }
        if previous.is_none()
            && entry.last.is_some_and(|last| last.ue == identity.ue)
            && entry
                .boundary
                .is_some_and(|old| !valid_replacement(old, session.session()))
        {
            return Err((ReceiveError::Time, session));
        }
        let Some(used) =
            add(self.used, session.resources()).filter(|used| fits(*used, self.limits.resources))
        else {
            let error = budget_error(self.used, session.resources(), self.limits.resources);
            return Err((error, session));
        };
        self.used = used;
        if entry.last.is_none_or(|last| last.ue != identity.ue) {
            entry.retired_last = None;
            entry.boundary = None;
        }
        entry.last = Some(identity);
        entry.owners[index] = Some(Owner {
            session,
            failure_reported: false,
        });
        Ok(())
    }
    /// Replacing an admitted owner in place invalidates resource/generation accounting. Use explicit admission/removal instead.
    pub fn session_mut(&mut self, identity: ReceiveIdentity) -> Option<&mut T> {
        let entry = self.entries.get_mut(identity.ue.slot as usize)?;
        entry
            .owners
            .iter_mut()
            .flatten()
            .find(|owner| owner.session.session().identity() == identity)
            .map(|owner| owner.session.as_mut())
    }
    pub fn remove(&mut self, identity: ReceiveIdentity) -> Option<Box<T>> {
        let entry = self.entries.get_mut(identity.ue.slot as usize)?;
        let index = entry.owners.iter().position(|owner| {
            owner
                .as_ref()
                .is_some_and(|owner| owner.session.session().identity() == identity)
        })?;
        let session = entry.owners[index].take().expect("validated owner").session;
        let mut removed = boundary(session.as_ref());
        entry.retired_last = latest(entry.retired_last, removed.last);
        removed.last = entry.retired_last;
        entry.boundary = Some(removed);
        let resources = session.resources();
        self.used.physical_sample_bytes -= resources.physical_sample_bytes;
        self.used.coded_bits -= resources.coded_bits;
        self.used.output_bytes -= resources.output_bytes;
        self.used.demultiplexed_bits -= resources.demultiplexed_bits;
        self.used.channels -= resources.channels;
        Some(session)
    }
    pub(super) fn mark_failed(&mut self, identity: ReceiveIdentity) -> bool {
        let Some(entry) = self.entries.get_mut(identity.ue.slot as usize) else {
            return false;
        };
        let Some(owner) = entry
            .owners
            .iter_mut()
            .flatten()
            .find(|owner| owner.session.session().identity() == identity)
        else {
            return false;
        };
        let fresh = !owner.failure_reported;
        owner.failure_reported = true;
        fresh
    }
    #[cfg(test)]
    pub fn service(
        &mut self,
        observe: impl FnMut(ReceiveIdentity, Result<usize, ReceiveError>),
    ) -> usize {
        self.service_with(|owner| owner.session_mut().service(), observe)
    }
    pub(super) fn service_with<R>(
        &mut self,
        mut operate: impl FnMut(&mut T) -> Result<R, ReceiveError>,
        mut observe: impl FnMut(ReceiveIdentity, Result<R, ReceiveError>),
    ) -> usize {
        let mut serviced = 0;
        let maximum = self.limits.maximum_sessions_per_step.get();
        let owners = self.entries.len() * CONFIGURATIONS_PER_UE;
        for _ in 0..maximum.saturating_mul(CONFIGURATIONS_PER_UE).min(owners) {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % owners;
            let Some(owner) = &mut self.entries[index / CONFIGURATIONS_PER_UE].owners
                [index % CONFIGURATIONS_PER_UE]
            else {
                continue;
            };
            if owner.failure_reported || !owner.session.service_enabled() {
                continue;
            }
            let result = operate(&mut owner.session);
            owner.failure_reported = result.is_err();
            observe(owner.session.session().identity(), result);
            serviced += 1;
            if serviced == maximum {
                break;
            }
        }
        serviced
    }
}

fn latest(
    first: Option<umts_types::time::ChipInstant>,
    second: Option<umts_types::time::ChipInstant>,
) -> Option<umts_types::time::ChipInstant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(if first.index >= second.index {
            first
        } else {
            second
        }),
        (None, value) | (value, None) => value,
    }
}

fn boundary(owner: &impl ReceiveOwner) -> RetiredBoundary {
    RetiredBoundary {
        anchor: owner.session().anchor,
        last: owner.last_consumed_frame(),
        longest: owner
            .session()
            .channels
            .iter()
            .map(|channel| channel.frames)
            .max()
            .expect("configured transports"),
    }
}

fn valid_replacement(old: RetiredBoundary, new: &DedicatedReceiveSession) -> bool {
    use umts_types::time::CHIPS_PER_FRAME;
    let longest = old.longest.max(
        new.channels
            .iter()
            .map(|channel| channel.frames)
            .max()
            .expect("configured transports"),
    );
    let Some(delta) = new.anchor.time.index.checked_sub(old.anchor.time.index) else {
        return false;
    };
    // TS 25.212 4.2.14 includes removed transports when determining Fmax for the change.
    usize::from(new.anchor.cfn).is_multiple_of(longest)
        && delta.is_multiple_of(CHIPS_PER_FRAME)
        && old
            .anchor
            .cfn
            .wrapping_add(((delta / CHIPS_PER_FRAME) % super::CFN_MODULUS) as u8)
            == new.anchor.cfn
        && old
            .last
            .is_none_or(|last| new.anchor.time.index > last.index)
}

fn add(first: ReceiveResources, second: ReceiveResources) -> Option<ReceiveResources> {
    Some(ReceiveResources {
        physical_sample_bytes: first
            .physical_sample_bytes
            .checked_add(second.physical_sample_bytes)?,
        coded_bits: first.coded_bits.checked_add(second.coded_bits)?,
        output_bytes: first.output_bytes.checked_add(second.output_bytes)?,
        demultiplexed_bits: first
            .demultiplexed_bits
            .checked_add(second.demultiplexed_bits)?,
        channels: first.channels.checked_add(second.channels)?,
    })
}
fn fits(used: ReceiveResources, maximum: ReceiveResources) -> bool {
    used.physical_sample_bytes <= maximum.physical_sample_bytes
        && used.coded_bits <= maximum.coded_bits
        && used.output_bytes <= maximum.output_bytes
        && used.demultiplexed_bits <= maximum.demultiplexed_bits
        && used.channels <= maximum.channels
}

fn budget_error(
    used: ReceiveResources,
    requested: ReceiveResources,
    maximum: ReceiveResources,
) -> ReceiveError {
    use super::ReceiveResource;
    for (resource, used, requested, maximum) in [
        (
            ReceiveResource::PhysicalSampleBytes,
            used.physical_sample_bytes,
            requested.physical_sample_bytes,
            maximum.physical_sample_bytes,
        ),
        (
            ReceiveResource::CodedBits,
            used.coded_bits,
            requested.coded_bits,
            maximum.coded_bits,
        ),
        (
            ReceiveResource::OutputBytes,
            used.output_bytes,
            requested.output_bytes,
            maximum.output_bytes,
        ),
        (
            ReceiveResource::DemultiplexedBits,
            used.demultiplexed_bits,
            requested.demultiplexed_bits,
            maximum.demultiplexed_bits,
        ),
        (
            ReceiveResource::Channels,
            used.channels,
            requested.channels,
            maximum.channels,
        ),
    ] {
        let Some(required) = used.checked_add(requested) else {
            return ReceiveError::ResourceOverflow(resource);
        };
        if required > maximum {
            return ReceiveError::ResourceBudget {
                resource,
                required,
                maximum,
            };
        }
    }
    unreachable!("budget error requires exceeded resource")
}
