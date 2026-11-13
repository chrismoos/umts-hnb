// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CipherUse, CountAnchor, Error, Header, Position,
    epochs::{Epochs, PendingActivation},
    header,
};
use crate::{contexts::CipherContext, counters::CipherMode};
use umts_types::{CnDomain, Direction, UeKey};

const MAX_BEARER: u8 = 32;
const FIRST_USER_BEARER: u8 = 5;
const SECURITY_RESPONSE_BEARER: u8 = 2;
const OCTET_BITS: usize = u8::BITS as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Signalling,
    User(CnDomain),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    ue: UeKey,
    scope: Scope,
    bearer: u8,
    mode: CipherMode,
    direction: Direction,
}

impl Binding {
    pub fn new(
        ue: UeKey,
        scope: Scope,
        bearer: u8,
        mode: CipherMode,
        direction: Direction,
    ) -> Result<Self, Error> {
        validate_bearer(scope, bearer)?;
        if mode == CipherMode::Transparent {
            return Err(Error::Mode);
        }
        Ok(Self {
            ue,
            scope,
            bearer,
            mode,
            direction,
        })
    }

    pub fn ue(self) -> UeKey {
        self.ue
    }
    pub fn mode(self) -> CipherMode {
        self.mode
    }
    pub fn direction(self) -> Direction {
        self.direction
    }
    pub fn scope(self) -> Scope {
        self.scope
    }
    pub fn bearer(self) -> u8 {
        self.bearer
    }
}

pub(super) fn validate_bearer(scope: Scope, bearer: u8) -> Result<(), Error> {
    if bearer == 0 || bearer > MAX_BEARER {
        return Err(Error::Bearer);
    }
    match scope {
        Scope::Signalling if bearer >= FIRST_USER_BEARER => Err(Error::Bearer),
        Scope::User(_) if bearer < FIRST_USER_BEARER => Err(Error::Bearer),
        _ => Ok(()),
    }
}

#[derive(Clone, Debug)]
pub enum Configuration {
    Unprotected,
    Configured(CipherContext),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetConfiguration {
    pub origin: Option<crate::contexts::KeyOrigin>,
    /// Some initializes a pending configuration at reset SN zero. None preserves synchronized RLC HFNs.
    pub initial_count: Option<u32>,
}

impl Configuration {
    pub(super) fn origin(&self) -> Option<crate::contexts::KeyOrigin> {
        match self {
            Self::Unprotected => None,
            Self::Configured(context) => Some(context.origin()),
        }
    }
}

#[derive(Clone)]
pub struct CipherTimeline {
    binding: Binding,
    epochs: Epochs,
}

impl CipherTimeline {
    pub fn new(
        binding: Binding,
        anchor: CountAnchor,
        initial: Configuration,
    ) -> Result<Self, Error> {
        if anchor.mode() != binding.mode {
            return Err(Error::Mode);
        }
        let epochs = Epochs::new(binding.ue, binding.scope, anchor, initial)?;
        Ok(Self { binding, epochs })
    }

    pub fn binding(&self) -> Binding {
        self.binding
    }
    pub fn pending_activation(
        &self,
        key: UeKey,
        next: Position,
    ) -> Result<Option<Position>, Error> {
        self.epochs.pending_activation(key, next)
    }
    pub fn configurations(&self) -> usize {
        self.epochs.len()
    }

    pub fn reset_configuration(
        &self,
        key: UeKey,
        next: Position,
    ) -> Result<ResetConfiguration, Error> {
        self.epochs.reset_configuration(key, next)
    }

    pub fn origins(&self) -> impl Iterator<Item = Option<crate::contexts::KeyOrigin>> + '_ {
        self.epochs.origins()
    }

    pub fn selected(&self, key: UeKey, position: Position) -> Result<CipherUse, Error> {
        self.epochs
            .selected(key, position)
            .map(|selected| selected.usage())
    }

    /// `first_new` is the next first-transmission position, not a retransmission or a wrapped wire SN.
    pub fn schedule_with<T, E>(
        &mut self,
        context: CipherContext,
        anchor: CountAnchor,
        first_new: Position,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        // TS 25.331 8.6.3.4 permits a new pending uplink activation time only for SRB2.
        let pending = if self.binding.scope == Scope::Signalling
            && self.binding.bearer == SECURITY_RESPONSE_BEARER
            && self.binding.direction == Direction::Uplink
            && self.binding.mode == CipherMode::Acknowledged
        {
            PendingActivation::ReselectBoundary
        } else {
            PendingActivation::KeepBoundary
        };
        self.epochs
            .schedule_with(context, anchor, first_new, pending, publish)
    }

    /// The RLC owner supplies the oldest position still referenced by its retransmission/receive windows.
    pub fn retire_before(&mut self, key: UeKey, floor: Position) -> Result<(), Error> {
        self.epochs.retire_before(key, floor)
    }

    /// Apply only with an admitted RLC reset and its synchronized HFN. The selected key is explicit.
    pub fn reset_with<T, E>(
        &mut self,
        key: UeKey,
        origin: Option<crate::contexts::KeyOrigin>,
        anchor: CountAnchor,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        self.epochs.reset_with(key, origin, anchor, publish)
    }

    pub fn apply(
        &self,
        key: UeKey,
        position: Position,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<CipherUse, Error> {
        if key != self.binding.ue {
            return Err(Error::Owner);
        }
        let Header::Data {
            sequence,
            clear_octets,
        } = header(self.binding.mode, bytes, bits)?
        else {
            return Ok(CipherUse::Control);
        };
        let selected = self.epochs.selected(key, position)?;
        let mask = (1 << self.binding.mode.sequence_bits()) - 1;
        if sequence.is_some_and(|sequence| u32::from(sequence) != selected.count & mask) {
            return Err(Error::Sequence);
        }
        selected.apply(
            self.binding.bearer,
            self.binding.direction,
            &mut bytes[clear_octets..],
            bits - clear_octets * OCTET_BITS,
        )?;
        Ok(selected.usage())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateError<E> {
    Cipher(Error),
    Publication(E),
}

#[cfg(test)]
mod tests;
