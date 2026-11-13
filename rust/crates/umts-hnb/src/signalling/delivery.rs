// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
const FAILURE_RELEASE_RETRIES: u8 = 4;

#[derive(Clone, Copy)]
pub(super) struct Reservation {
    identity: crate::dedicated_rx::ReceiveIdentity,
    aborting: bool,
}

impl CommonSignalling {
    pub(crate) fn reserve_transition_delivery(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(identity.ue)
            .ok_or(SignallingError::Origin)?;
        if context.dedicated_route != Some(identity) || context.bearer_change.is_none() {
            return Err(SignallingError::Phase);
        }
        context.transition_reservation = Some(Reservation {
            identity,
            aborting: false,
        });
        Ok(())
    }

    pub(crate) fn reserved_signalling(
        &self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Option<SignallingBearer> {
        use umts_rlc::acknowledged::SduTransmission;
        let context = self.registry.get(identity.ue)?;
        let reservation = context.transition_reservation?;
        if reservation.identity != identity {
            return None;
        }
        if reservation.aborting {
            return Some(SignallingBearer::Unacknowledged);
        }
        let change = context.bearer_change.as_ref()?;
        (!matches!(
            context
                .control
                .transmission(identity.ue, change.delivery_receipt()),
            Ok(SduTransmission::Acknowledged)
        ))
        .then_some(SignallingBearer::ControlAm)
    }

    pub(crate) fn abort_transition_delivery(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(identity.ue)
            .ok_or(SignallingError::Origin)?;
        let reservation = context
            .transition_reservation
            .as_mut()
            .ok_or(SignallingError::Phase)?;
        if reservation.identity != identity {
            return Err(SignallingError::Mapping);
        }
        reservation.aborting = true;
        super::lifecycle::queue_rrc_release(
            context,
            identity.ue,
            umts_rrc::mobility::ReleaseCompleteRetries::new(FAILURE_RELEASE_RETRIES)
                .expect("valid release retries"),
            true,
        )
    }

    pub(crate) fn transition_release_complete(&self, key: UeKey) -> bool {
        self.registry
            .get(key)
            .is_some_and(|context| context.idle_release.is_some_and(|release| release.complete))
    }
}

#[cfg(test)]
mod tests;
