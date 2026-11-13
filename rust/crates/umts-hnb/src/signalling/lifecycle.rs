// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleBlocker {
    Establishment,
    BearerChange,
    Protection,
    Capability,
    Security,
    Registration,
    DeferredNas,
    ControlTransmit,
    ControlReceive,
    NasTransmit,
    NasReceive,
    UmTransmit,
    CoreOutbox,
    Ingress,
    Assignment,
    Circuit,
    Radio,
    UserData,
}

impl IdleBlocker {
    pub fn counter_name(self) -> &'static str {
        match self {
            Self::Establishment => "inactivity.blocked.establishment",
            Self::BearerChange => "inactivity.blocked.bearer_change",
            Self::Protection => "inactivity.blocked.protection",
            Self::Capability => "inactivity.blocked.capability",
            Self::Security => "inactivity.blocked.security",
            Self::Registration => "inactivity.blocked.registration",
            Self::DeferredNas => "inactivity.blocked.deferred_nas",
            Self::ControlTransmit => "inactivity.blocked.control_transmit",
            Self::ControlReceive => "inactivity.blocked.control_receive",
            Self::NasTransmit => "inactivity.blocked.nas_transmit",
            Self::NasReceive => "inactivity.blocked.nas_receive",
            Self::UmTransmit => "inactivity.blocked.um_transmit",
            Self::CoreOutbox => "inactivity.blocked.core_outbox",
            Self::Ingress => "inactivity.blocked.ingress",
            Self::Assignment => "inactivity.blocked.assignment",
            Self::Circuit => "inactivity.blocked.circuit",
            Self::Radio => "inactivity.blocked.radio",
            Self::UserData => "inactivity.blocked.user_data",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct IdleRelease {
    transaction: TransactionId,
    pub complete: bool,
    coverage: Option<umts_rlc::acknowledged::ReceiveCoverage>,
}

#[derive(Clone, Copy)]
pub(crate) struct IdleCandidate {
    pub key: UeKey,
    pub route: Option<crate::dedicated_rx::ReceiveIdentity>,
    pub activity: u64,
    pub blocker: Option<IdleBlocker>,
    pub connections: [Option<CoreConnection>; 2],
    pub release_complete: Option<bool>,
    pub release_queued: bool,
    pub release_after_core_disconnect: bool,
}

pub(super) fn release_complete(
    release: &mut Option<IdleRelease>,
    dedicated: bool,
    bearer: SignallingBearer,
    message: &umts_rrc_codec::DecodedUplinkDedicated,
    key: UeKey,
    coverage: Option<umts_rlc::acknowledged::ReceiveCoverage>,
    control: &mut CommonAmBearer,
) -> Result<bool, SignallingError> {
    let ULDCCHMessageType::rrcConnectionReleaseComplete(message) = &message.value.message else {
        return Ok(false);
    };
    let pending = release.as_mut().ok_or(SignallingError::Phase)?;
    let expected = if dedicated {
        SignallingBearer::Unacknowledged
    } else {
        SignallingBearer::ControlAm
    };
    if bearer != expected || message.rrc_transaction_identifier.0 != pending.transaction.get() {
        return Err(SignallingError::Message);
    }
    if !dedicated {
        let coverage = coverage.ok_or(SignallingError::Mapping)?;
        control
            .request_status_confirmation(key, coverage)
            .map_err(SignallingError::Am)?;
        pending.coverage = Some(coverage);
    }
    if message.error_indication.is_some() && !pending.complete {
        tracing::warn!(
            "RRC release completion reports a protocol error: key={key:?} transaction={:?}",
            pending.transaction
        );
    }
    pending.complete = true;
    Ok(true)
}

fn pending_am_transmit(bearer: &CommonAmBearer) -> bool {
    let positions = bearer.window_positions();
    bearer.pending_new_data() || positions.oldest_transmit != positions.next_transmit
}

impl CommonSignalling {
    pub(crate) fn idle_candidate(&self, slot: u32, now: ChipInstant) -> Option<IdleCandidate> {
        let (key, context) = self.registry.at_slot(slot)?;
        let blocker = [
            (
                context.setup.phase() != EstablishmentPhase::Established,
                IdleBlocker::Establishment,
            ),
            (context.bearer_change.is_some(), IdleBlocker::BearerChange),
            (context.protection.blocks_status(), IdleBlocker::Protection),
            (
                context.capabilities.pending_transaction().is_some(),
                IdleBlocker::Capability,
            ),
            (
                [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
                    .into_iter()
                    .any(|domain| {
                        context.security.phase(domain) != connection_security::Phase::Idle
                    }),
                IdleBlocker::Security,
            ),
            (
                matches!(
                    context.registration,
                    Registration::Pending | Registration::Identifying
                ),
                IdleBlocker::Registration,
            ),
            (!context.deferred.is_empty(), IdleBlocker::DeferredNas),
            (
                pending_am_transmit(&context.control),
                IdleBlocker::ControlTransmit,
            ),
            (
                context.control.pending_receive_data(),
                IdleBlocker::ControlReceive,
            ),
            (
                pending_am_transmit(&context.nas_bearer),
                IdleBlocker::NasTransmit,
            ),
            (
                context.nas_bearer.pending_receive_data(),
                IdleBlocker::NasReceive,
            ),
            (context.um.publication_ready(), IdleBlocker::UmTransmit),
            (self.core.pending_for(key), IdleBlocker::CoreOutbox),
            (
                [
                    SignallingBearer::Unacknowledged,
                    SignallingBearer::ControlAm,
                    SignallingBearer::NasAm,
                ]
                .into_iter()
                .any(|bearer| self.ingress.pending(key, bearer)),
                IdleBlocker::Ingress,
            ),
        ]
        .into_iter()
        .find_map(|(pending, reason)| pending.then_some(reason));
        Some(IdleCandidate {
            key,
            release_after_core_disconnect: context.release_after_core_disconnect,
            route: context.dedicated_route,
            activity: context.activity,
            blocker,
            connections: context
                .router
                .as_ref()
                .map_or([None; 2], NasRouter::connections),
            release_complete: context.idle_release.map(|release| {
                release.complete
                    && (context.dedicated_route.is_some()
                        || release.coverage.is_some_and(|coverage| {
                            context
                                .control
                                .status_submission(key)
                                .ok()
                                .flatten()
                                .is_some_and(|status| {
                                    status.coverage.covers(coverage)
                                        && status.transport.submitted()
                                        && now.epoch == status.transport.air_end().epoch
                                        && now.index >= status.transport.air_end().index
                                })
                        }))
            }),
            release_queued: context.um.publication_ready(),
        })
    }

    pub(crate) fn request_inactivity_release(
        &mut self,
        connection: CoreConnection,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get(connection.key.ue)
            .ok_or(SignallingError::Origin)?;
        if context
            .router
            .as_ref()
            .and_then(|router| router.connection(connection.key.domain))
            != Some(connection)
        {
            return Err(SignallingError::Origin);
        }
        self.core.push_inactivity_release(connection)
    }

    pub(crate) fn release_idle_rrc(
        &mut self,
        key: UeKey,
        retries: umts_rrc::mobility::ReleaseCompleteRetries,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.idle_release.is_some() {
            return Ok(());
        }
        if context
            .router
            .as_ref()
            .is_some_and(|router| router.connections().iter().any(Option::is_some))
        {
            return Err(SignallingError::Phase);
        }
        queue_rrc_release(context, key, retries, false)
    }
}

pub(super) fn queue_rrc_release(
    context: &mut Context,
    key: UeKey,
    retries: umts_rrc::mobility::ReleaseCompleteRetries,
    failure: bool,
) -> Result<(), SignallingError> {
    if context.idle_release.is_some() {
        return Ok(());
    }
    let transaction = context
        .transactions
        .next(transactions::MessageType::RrcConnectionRelease);
    let state = if context.dedicated_route.is_some() {
        umts_rrc::mobility::ReleaseConnectionState::CellDch(retries)
    } else {
        umts_rrc::mobility::ReleaseConnectionState::CellFach
    };
    let message = if failure {
        umts_rrc::mobility::build_transition_failure_release(transaction, state)
    } else {
        umts_rrc::mobility::build_inactivity_release(transaction, state)
    };
    let um = &mut context.um;
    // TS 25.331 8.1.4.2 requires UM RLC and the existing DCCH security schedule.
    context
        .protection
        .integrity
        .publish_with(
            key,
            SignallingBearer::Unacknowledged.identity(),
            &message,
            |encoded| {
                um.enqueue(
                    key,
                    Bits::new(&encoded.bytes, encoded.bit_length)
                        .map_err(|_| SignallingError::Message)?,
                )
                .map_err(SignallingError::Um)
            },
        )
        .map_err(|error| protection::schedule_error(error, |error| error))?;
    context
        .transactions
        .admitted(transactions::MessageType::RrcConnectionRelease, transaction);
    context.idle_release = Some(IdleRelease {
        transaction,
        complete: false,
        coverage: None,
    });
    Ok(())
}

impl CommonSignalling {
    pub(crate) fn retire_idle(&mut self, key: UeKey) -> Result<(), SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        if context.idle_release.is_none()
            || context
                .router
                .as_ref()
                .is_some_and(|router| router.connections().iter().any(Option::is_some))
        {
            return Err(SignallingError::Phase);
        }
        self.core.push_rrc_release(key)?;
        self.retire_admitted(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        signalling::tests::{controller, establish},
        test_support::{at, source},
    };

    #[test]
    fn release_publication_is_per_ue_and_requires_closed_iu_connections() {
        let source = source(false);
        let mut radio = controller(&source);
        let first = establish(&mut radio, &source, 0);
        let second = establish(&mut radio, &source, 1);
        let retries = umts_rrc::mobility::ReleaseCompleteRetries::new(1).unwrap();
        radio.release_idle_rrc(first, retries).unwrap();
        assert_eq!(
            radio
                .idle_candidate(first.slot, at(0))
                .unwrap()
                .release_complete,
            Some(false)
        );
        assert_eq!(
            radio
                .idle_candidate(second.slot, at(0))
                .unwrap()
                .release_complete,
            None
        );
        assert!(radio.registry.get(first).unwrap().um.publication_ready());
        assert!(!radio.registry.get(second).unwrap().um.publication_ready());
        let queued = radio.registry.get(first).unwrap().um.transmit_position();
        radio.release_idle_rrc(first, retries).unwrap();
        assert_eq!(
            radio.registry.get(first).unwrap().um.transmit_position(),
            queued
        );
        assert_eq!(radio.phase(second), Some(EstablishmentPhase::Established));
    }
}
