// SPDX-License-Identifier: AGPL-3.0-or-later

use super::CommonController;
use crate::{dedicated_rx::ReceiveIdentity, rab_handoff::PendingRabAssignment};
use umts_core::Transport;
use umts_types::CnDomain;

impl<T: Transport> CommonController<T> {
    /// The packet bearers of a dedicated session that can be parked, empty for a UE with
    /// signalling only, or none when the session cannot be parked.
    pub(crate) fn parkable_bearers(&self, identity: ReceiveIdentity) -> Option<Vec<u8>> {
        let admission = self.admission.as_ref()?;
        if !admission.parkable(identity)? {
            return Some(Vec::new());
        }
        let active = admission.committed_radio(identity).ok()?;
        let bearers: Vec<u8> = active
            .plan()
            .configuration()
            .bearers()
            .iter()
            .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
            .map(|bearer| bearer.id)
            .collect();
        (!bearers.is_empty()).then_some(bearers)
    }

    pub(crate) fn park_dedicated(&mut self, identity: ReceiveIdentity, bearers: &[u8]) -> bool {
        let Some(admission) = self.admission.as_mut() else {
            return false;
        };
        if admission.parkable(identity).is_none() {
            return false;
        }
        match self.radio.enter_parked(identity.ue, bearers) {
            Ok(Some(route)) if route == identity => admission.park(identity).is_ok(),
            Ok(_) => false,
            Err(error) => {
                tracing::warn!("Park not entered: identity={identity:?} error={error:?}");
                false
            }
        }
    }

    pub(crate) fn resumable_parked(&self, pending: &PendingRabAssignment) -> bool {
        self.core
            .gateway()
            .rab_connection_current(pending.request())
    }

    pub(crate) fn resume_parked(&mut self, pending: &PendingRabAssignment) -> bool {
        let key = pending.request().key();
        match self
            .core
            .gateway_mut()
            .reinstate_rab(pending.request(), pending.assignment())
        {
            Ok(()) => {
                self.radio.wake_started(key);
                true
            }
            Err(error) => {
                tracing::warn!("Parked bearers not resumed: key={key:?} error={error:?}");
                false
            }
        }
    }
}
