// SPDX-License-Identifier: AGPL-3.0-or-later

use super::SetupAfterRelease;
use crate::rab_handoff::{PendingRabAssignment, SetupAssignment};
use std::ops::Deref;

pub(super) enum SetupSource {
    Fresh(PendingRabAssignment),
    AfterRelease(SetupAfterRelease),
}

impl SetupSource {
    pub(super) fn pending(&self) -> &PendingRabAssignment {
        match self {
            Self::Fresh(pending) => pending,
            Self::AfterRelease(released) => released.pending(),
        }
    }

    pub(super) fn selection(&self) -> Option<SetupAssignment<'_>> {
        match self {
            Self::Fresh(pending) => pending.setup_selection(),
            Self::AfterRelease(released) => Some(SetupAssignment::after_release(released)),
        }
    }

    pub(super) fn into_pending(self) -> PendingRabAssignment {
        match self {
            Self::Fresh(pending) => pending,
            Self::AfterRelease(released) => released.into_pending(),
        }
    }
}

impl Deref for SetupSource {
    type Target = PendingRabAssignment;

    fn deref(&self) -> &Self::Target {
        self.pending()
    }
}
