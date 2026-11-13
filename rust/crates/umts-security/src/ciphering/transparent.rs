// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CipherUse, Configuration, CountAnchor, Error, Position, Scope, UpdateError,
    epochs::{Epochs, PendingActivation},
    header,
};
use crate::{
    contexts::{CipherContext, KeyOrigin},
    counters::CipherMode,
};
use umts_types::{CnDomain, Direction, UeKey};

const CFN_ACTIVATION_MULTIPLE: u64 = 8;

// TS 33.102 6.6.4 shares TM COUNT across directions and all TM bearers of one CN domain.
#[derive(Clone)]
pub struct TransparentDomain {
    epochs: Epochs,
}

impl TransparentDomain {
    pub fn new(
        ue: UeKey,
        domain: CnDomain,
        anchor: CountAnchor,
        initial: Configuration,
    ) -> Result<Self, Error> {
        if anchor.mode() != CipherMode::Transparent {
            return Err(Error::Mode);
        }
        Ok(Self {
            epochs: Epochs::new(ue, Scope::User(domain), anchor, initial)?,
        })
    }

    pub fn configurations(&self) -> usize {
        self.epochs.len()
    }

    pub fn origins(&self) -> impl Iterator<Item = Option<crate::contexts::KeyOrigin>> + '_ {
        self.epochs.origins()
    }

    pub fn selected(&self, key: UeKey, frame: u64) -> Result<CipherUse, Error> {
        self.epochs
            .selected(key, Position::Frame(frame))
            .map(|selected| selected.usage())
    }

    pub(super) fn matches_anchor(
        &self,
        key: UeKey,
        anchor: CountAnchor,
        origin: Option<KeyOrigin>,
    ) -> Result<bool, Error> {
        self.epochs.matches_anchor(key, anchor, origin)
    }

    // The authenticated response supplies the START and activation CFN, not a new key generation.
    pub fn activate_counter_with<T, E>(
        &mut self,
        key: UeKey,
        origin: Option<KeyOrigin>,
        anchor: CountAnchor,
        first_frame: u64,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        let frame = anchor
            .position()
            .get(CipherMode::Transparent)
            .map_err(UpdateError::Cipher)?;
        if !frame.is_multiple_of(CFN_ACTIVATION_MULTIPLE) {
            return Err(UpdateError::Cipher(Error::Position));
        }
        self.epochs
            .activate_counter_with(key, origin, anchor, first_frame, publish)
    }

    pub fn schedule_with<T, E>(
        &mut self,
        context: CipherContext,
        anchor: CountAnchor,
        first_frame: u64,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, UpdateError<E>> {
        let frame = anchor
            .position()
            .get(CipherMode::Transparent)
            .map_err(UpdateError::Cipher)?;
        if !frame.is_multiple_of(CFN_ACTIVATION_MULTIPLE) {
            return Err(UpdateError::Cipher(Error::Position));
        }
        self.epochs.schedule_with(
            context,
            anchor,
            Position::Frame(first_frame),
            PendingActivation::KeepBoundary,
            publish,
        )
    }

    pub fn apply(
        &self,
        key: UeKey,
        bearer: u8,
        direction: Direction,
        frame: u64,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<CipherUse, Error> {
        header(CipherMode::Transparent, bytes, bits)?;
        let selected = self.epochs.selected(key, Position::Frame(frame))?;
        selected.apply(bearer, direction, bytes, bits)?;
        Ok(selected.usage())
    }

    pub fn retire_before(&mut self, key: UeKey, frame: u64) -> Result<(), Error> {
        self.epochs.retire_before(key, Position::Frame(frame))
    }
}
