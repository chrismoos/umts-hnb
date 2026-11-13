// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CipherUse, Configuration, CountAnchor, Error, Position, Scope, UpdateError,
    timeline::ResetConfiguration, timeline::validate_bearer,
};
use crate::{
    contexts::{CipherContext, KeyOrigin},
    counters::CipherMode,
};
use umts_types::{CnDomain, Direction, UeKey};

const USER_CONFIGURATIONS: usize = 2;
const SIGNALLING_CONFIGURATIONS: usize = 3;
const UNPROTECTED_INTERVALS: usize = 1;
const CN_DOMAINS: usize = 2;
const COUNTER_INTERVALS: usize = USER_CONFIGURATIONS;
const MAX_INTERVALS: usize = SIGNALLING_CONFIGURATIONS + UNPROTECTED_INTERVALS + COUNTER_INTERVALS;

pub(super) enum PendingActivation {
    KeepBoundary,
    ReselectBoundary,
}

#[derive(Clone)]
struct Epoch {
    configuration: Configuration,
    anchor: CountAnchor,
}

pub(super) struct Selected<'a> {
    configuration: &'a Configuration,
    scope: Scope,
    pub(super) count: u32,
}

impl Selected<'_> {
    pub(super) fn usage(&self) -> CipherUse {
        CipherUse::Data {
            origin: self.configuration.origin(),
            count: self.count,
        }
    }

    pub(super) fn apply(
        &self,
        bearer: u8,
        direction: Direction,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<(), Error> {
        validate_bearer(self.scope, bearer)?;
        if let Configuration::Configured(context) = self.configuration {
            context
                .apply(
                    context.origin(),
                    self.count,
                    bearer - 1,
                    direction,
                    bytes,
                    bits,
                )
                .map_err(Error::Algorithm)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct Epochs {
    ue: UeKey,
    scope: Scope,
    mode: CipherMode,
    epochs: [Option<Epoch>; MAX_INTERVALS],
    length: usize,
    retained_floor: Position,
    highest_generation: [u64; CN_DOMAINS],
}

impl Epochs {
    pub(super) fn new(
        ue: UeKey,
        scope: Scope,
        anchor: CountAnchor,
        initial: Configuration,
    ) -> Result<Self, Error> {
        if anchor.stopped() && initial.origin().is_some() {
            return Err(Error::Mode);
        }
        let mut highest_generation = [0; CN_DOMAINS];
        if let Some(origin) = initial.origin() {
            validate_origin(ue, scope, origin)?;
            highest_generation[domain_index(origin.domain)] = origin.generation.get();
        }
        let mut epochs = std::array::from_fn(|_| None);
        let retained_floor = anchor.position();
        let mode = anchor.mode();
        epochs[0] = Some(Epoch {
            configuration: initial,
            anchor,
        });
        Ok(Self {
            ue,
            scope,
            mode,
            epochs,
            length: 1,
            retained_floor,
            highest_generation,
        })
    }

    pub(super) fn len(&self) -> usize {
        let mut seen = [None; MAX_INTERVALS];
        let mut count = 0;
        for origin in self.origins().flatten() {
            if !seen[..count].contains(&Some(origin)) {
                seen[count] = Some(origin);
                count += 1;
            }
        }
        count
    }

    pub(super) fn origins(&self) -> impl Iterator<Item = Option<KeyOrigin>> + '_ {
        self.epochs[..self.length]
            .iter()
            .flatten()
            .map(|epoch| epoch.configuration.origin())
    }

    pub(super) fn selected(&self, key: UeKey, position: Position) -> Result<Selected<'_>, Error> {
        if key != self.ue {
            return Err(Error::Owner);
        }
        let raw = position.get(self.mode)?;
        if raw < self.retained_floor.get(self.mode)? {
            return Err(Error::Position);
        }
        let epoch = self.epochs[..self.length]
            .iter()
            .flatten()
            .rev()
            .find(|epoch| {
                epoch
                    .anchor
                    .position()
                    .get(self.mode)
                    .is_ok_and(|at| at <= raw)
            })
            .ok_or(Error::Position)?;
        Ok(Selected {
            configuration: &epoch.configuration,
            scope: self.scope,
            count: epoch.anchor.count_at(position)?,
        })
    }
    pub(super) fn pending_activation(
        &self,
        key: UeKey,
        next: Position,
    ) -> Result<Option<Position>, Error> {
        if key != self.ue {
            return Err(Error::Owner);
        }
        if next.get(self.mode)? < self.retained_floor.get(self.mode)? {
            return Err(Error::Position);
        }
        let at = self.epochs[self.length - 1]
            .as_ref()
            .ok_or(Error::Position)?
            .anchor
            .position();
        Ok((at.get(self.mode)? > next.get(self.mode)?).then_some(at))
    }

    pub(super) fn reset_configuration(
        &self,
        key: UeKey,
        next: Position,
    ) -> Result<ResetConfiguration, Error> {
        if key != self.ue {
            return Err(Error::Owner);
        }
        if self.mode != CipherMode::Acknowledged {
            return Err(Error::Mode);
        }
        let raw = next.get(self.mode)?;
        if raw < self.retained_floor.get(self.mode)? {
            return Err(Error::Position);
        }
        let latest = self.epochs[self.length - 1]
            .as_ref()
            .ok_or(Error::Position)?;
        let pending = latest.anchor.position().get(self.mode)? > raw;
        // TS 25.331 8.1.12.5 initializes the pending configuration at reset, not at the skipped wire SN.
        let initial_count = if pending {
            Some(
                latest.anchor.count_at(latest.anchor.position())? & !super::count::AM_SEQUENCE_MASK,
            )
        } else {
            None
        };
        Ok(ResetConfiguration {
            origin: latest.configuration.origin(),
            initial_count,
        })
    }

    pub(super) fn schedule_with<T, E>(
        &mut self,
        context: CipherContext,
        anchor: CountAnchor,
        first_new: Position,
        pending: PendingActivation,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        let fail = UpdateError::Cipher;
        let origin = context.origin();
        validate_origin(self.ue, self.scope, origin).map_err(fail)?;
        if anchor.mode() != self.mode || anchor.stopped() {
            return Err(fail(Error::Mode));
        }
        let first_new = first_new.get(self.mode).map_err(fail)?;
        let at = anchor.position().get(self.mode).map_err(fail)?;
        if first_new < self.retained_floor.get(self.mode).map_err(fail)? || at < first_new {
            return Err(fail(Error::Position));
        }
        let domain = domain_index(origin.domain);
        if origin.generation.get() <= self.highest_generation[domain] {
            return Err(fail(Error::ReusedGeneration));
        }
        let previous = self.epochs[self.length - 1]
            .as_ref()
            .ok_or(fail(Error::Position))?;
        let previous_at = previous.anchor.position().get(self.mode).map_err(fail)?;
        let reselect_pending =
            matches!(pending, PendingActivation::ReselectBoundary) && previous_at > first_new;
        if at < previous_at && !reselect_pending {
            return Err(fail(Error::Position));
        }
        let replace = at == previous_at || (reselect_pending && at < previous_at);
        // A receiver's contiguous floor cannot prove that earlier-key PDUs are not in flight.
        if previous_at > first_new && !replace && !reselect_pending {
            return Err(fail(Error::PendingActivation));
        }
        let limit = match self.scope {
            Scope::Signalling => SIGNALLING_CONFIGURATIONS,
            Scope::User(_) => USER_CONFIGURATIONS,
        };
        let removes_origin = replace
            && previous.configuration.origin().is_some_and(|origin| {
                self.origins()
                    .filter(|entry| *entry == Some(origin))
                    .count()
                    == 1
            });
        let configurations = self.len() + 1 - usize::from(removes_origin);
        if configurations > limit || (!replace && self.length == MAX_INTERVALS) {
            return Err(fail(Error::Capacity));
        }
        if !replace && at != 0 {
            let last_old = match self.mode {
                CipherMode::Transparent => Position::Frame(at - 1),
                _ => Position::Sequence(at - 1),
            };
            previous.anchor.count_at(last_old).map_err(fail)?;
        }
        let result = publish().map_err(UpdateError::Publication)?;
        let slot = if replace {
            self.length - 1
        } else {
            self.length
        };
        self.epochs[slot] = Some(Epoch {
            configuration: Configuration::Configured(context),
            anchor,
        });
        self.length += usize::from(!replace);
        self.highest_generation[domain] = origin.generation.get();
        Ok(result)
    }

    pub(super) fn matches_anchor(
        &self,
        key: UeKey,
        anchor: CountAnchor,
        origin: Option<KeyOrigin>,
    ) -> Result<bool, Error> {
        let selected = self.selected(key, anchor.position())?;
        let at = anchor.position().get(self.mode)?;
        let epoch = self.epochs[..self.length]
            .iter()
            .flatten()
            .rev()
            .find(|epoch| {
                epoch
                    .anchor
                    .position()
                    .get(self.mode)
                    .is_ok_and(|value| value <= at)
            })
            .ok_or(Error::Position)?;
        Ok(selected.configuration.origin() == origin
            && epoch.anchor.agrees_with(anchor, anchor.position())?)
    }

    pub(super) fn activate_counter_with<T, E>(
        &mut self,
        key: UeKey,
        origin: Option<KeyOrigin>,
        anchor: CountAnchor,
        first_frame: u64,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        let fail = UpdateError::Cipher;
        if key != self.ue {
            return Err(fail(Error::Owner));
        }
        if self.mode != CipherMode::Transparent || anchor.mode() != self.mode {
            return Err(fail(Error::Mode));
        }
        let at = anchor.position().get(self.mode).map_err(fail)?;
        let previous = self.epochs[self.length - 1]
            .as_ref()
            .ok_or(fail(Error::Position))?;
        let previous_at = previous.anchor.position().get(self.mode).map_err(fail)?;
        if previous.configuration.origin() != origin {
            return Err(fail(Error::Owner));
        }
        if (origin.is_some() && !anchor.running()) || (origin.is_none() && !anchor.stopped()) {
            return Err(fail(Error::Mode));
        }
        if previous.anchor.running() {
            return Err(fail(Error::Mode));
        }
        if first_frame < self.retained_floor.get(self.mode).map_err(fail)?
            || at < first_frame
            || at <= previous_at
        {
            return Err(fail(Error::Position));
        }
        if previous_at > first_frame {
            return Err(fail(Error::PendingActivation));
        }
        if self.length == MAX_INTERVALS {
            return Err(fail(Error::Capacity));
        }
        let configuration = previous.configuration.clone();
        let result = publish().map_err(UpdateError::Publication)?;
        self.epochs[self.length] = Some(Epoch {
            configuration,
            anchor,
        });
        self.length += 1;
        Ok(result)
    }

    pub(super) fn retire_before(&mut self, key: UeKey, floor: Position) -> Result<(), Error> {
        if key != self.ue {
            return Err(Error::Owner);
        }
        let raw = floor.get(self.mode)?;
        if raw < self.retained_floor.get(self.mode)? {
            return Err(Error::Position);
        }
        while self.length > 1 {
            let second = self.epochs[1].as_ref().ok_or(Error::Position)?;
            if second.anchor.position().get(self.mode)? > raw {
                break;
            }
            for index in 0..self.length - 1 {
                self.epochs[index] = self.epochs[index + 1].take()
            }
            self.length -= 1;
            self.epochs[self.length] = None;
        }
        self.retained_floor = floor;
        Ok(())
    }

    pub(super) fn reset_with<T, E>(
        &mut self,
        key: UeKey,
        origin: Option<KeyOrigin>,
        anchor: CountAnchor,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        let fail = UpdateError::Cipher;
        if key != self.ue {
            return Err(fail(Error::Owner));
        }
        if self.mode != CipherMode::Acknowledged || anchor.mode() != self.mode {
            return Err(fail(Error::Mode));
        }
        let raw = anchor.position().get(self.mode).map_err(fail)?;
        if raw < self.retained_floor.get(self.mode).map_err(fail)?
            || raw & u64::from(super::count::AM_SEQUENCE_MASK) != 0
        {
            return Err(fail(Error::Position));
        }
        let configuration = self.epochs[..self.length]
            .iter()
            .flatten()
            .find(|epoch| epoch.configuration.origin() == origin)
            .map(|epoch| epoch.configuration.clone())
            .ok_or(fail(Error::Owner))?;
        let result = publish().map_err(UpdateError::Publication)?;
        self.epochs = std::array::from_fn(|_| None);
        self.epochs[0] = Some(Epoch {
            configuration,
            anchor,
        });
        self.length = 1;
        self.retained_floor = anchor.position();
        Ok(result)
    }
}

fn validate_origin(ue: UeKey, scope: Scope, origin: KeyOrigin) -> Result<(), Error> {
    if origin.ue != ue || matches!(scope, Scope::User(domain) if domain != origin.domain) {
        return Err(Error::Owner);
    }
    Ok(())
}

fn domain_index(domain: CnDomain) -> usize {
    match domain {
        CnDomain::Cs => 0,
        CnDomain::Ps => 1,
    }
}
