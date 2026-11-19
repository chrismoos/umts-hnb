// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{PreparedControlSearch, ReceiveError, ReceiveIdentity};
use crate::hsupa_rx::TrackedReceiver;

pub struct HsupaAttachment {
    pub(super) receiver: Box<TrackedReceiver>,
    pub(super) control: Option<Box<PreparedControlSearch>>,
    pub(super) selection: Option<Box<super::PreparedDchSelection>>,
}

impl From<Box<TrackedReceiver>> for HsupaAttachment {
    fn from(receiver: Box<TrackedReceiver>) -> Self {
        Self {
            receiver,
            control: None,
            selection: None,
        }
    }
}

impl HsupaAttachment {
    /// Called only before publication or after RX returns rejected ownership.
    pub(crate) fn discard_initial_suffixes(&mut self) -> bool {
        if self.receiver.is_started() {
            return false;
        }
        let Some(control) = self.control.as_mut() else {
            return false;
        };
        if control
            .startup
            .as_ref()
            .is_some_and(|suffix| suffix.automatic && !suffix.attempted)
        {
            control.startup = None;
            true
        } else {
            false
        }
    }

    pub(crate) fn discard_collection_buffer(&mut self) -> bool {
        self.receiver.discard_collection_buffer()
    }

    /// Prepare all eligible initial TTI starts before publishing to RX.
    pub fn prepare_initial_suffixes(&mut self) -> Result<(), ReceiveError> {
        if self.receiver.is_started()
            || self.receiver.interval() != umts_types::hsupa::Tti::TwoMilliseconds
            || self.receiver.maximum_dpdch() != 0
        {
            return Err(ReceiveError::Configuration);
        }
        let origin = self.receiver.anchor().time;
        self.control
            .as_mut()
            .ok_or(ReceiveError::Configuration)?
            .prepare_initial_suffixes(origin)
    }

    /// Admit one initial partial-frame candidate before publication. The hint
    /// supplies known timing geometry, never evidence of acquisition or CRC.
    #[cfg(test)]
    pub fn prepare_startup_suffix(
        &mut self,
        frame: umts_types::time::ChipInstant,
        slot: u8,
    ) -> Result<(), ReceiveError> {
        let anchor = self.receiver.anchor().time;
        if self.receiver.is_started()
            || self.receiver.interval() != umts_types::hsupa::Tti::TwoMilliseconds
            || frame.epoch != anchor.epoch
            || frame
                .index
                .checked_sub(anchor.index)
                .is_none_or(|n| !n.is_multiple_of(umts_types::time::CHIPS_PER_FRAME))
            || frame
                .index
                .checked_add(umts_types::time::CHIPS_PER_FRAME)
                .is_none()
        {
            return Err(ReceiveError::Configuration);
        }
        self.control
            .as_mut()
            .ok_or(ReceiveError::Configuration)?
            .prepare_startup_suffix(frame, slot)
    }

    pub fn edch_only(
        receiver: Box<TrackedReceiver>,
        control: Box<PreparedControlSearch>,
    ) -> Result<Self, Self> {
        let candidate = Self {
            receiver,
            control: Some(control),
            selection: None,
        };
        if candidate.receiver.maximum_dpdch() != 0 || candidate.receiver.is_started() {
            return Err(candidate);
        }
        Ok(candidate)
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.receiver.identity()
    }
    pub fn coexistence(
        receiver: Box<TrackedReceiver>,
        selection: Box<super::PreparedDchSelection>,
    ) -> Self {
        Self {
            receiver,
            control: None,
            selection: Some(selection),
        }
    }

    pub fn receiver(&self) -> &TrackedReceiver {
        &self.receiver
    }

    pub fn retained_bytes(&self) -> Result<usize, ReceiveError> {
        self.receiver
            .retained_bytes()
            .map_err(ReceiveError::Hsupa)?
            .checked_add(
                self.control
                    .as_ref()
                    .map_or(0, |control| control.retained_bytes()),
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    self.selection
                        .as_ref()
                        .map_or(0, |selection| selection.retained_bytes()),
                )
            })
            .ok_or(ReceiveError::Capacity)
    }
}
