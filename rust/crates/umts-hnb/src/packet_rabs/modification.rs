// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::core_link::CoreLink;
use umts_iuh::session::{ControlTime, RabOutcome};

#[derive(Debug)]
pub(crate) enum ModificationError {
    Packet(Error),
    Core(umts_core::Error),
    Cancellation(Error),
}

pub(crate) struct PacketModification<'a> {
    pub request: RabRequest,
    pub item: &'a SetupOrModify,
    pub identity: ReceiveIdentity,
    pub plan: &'a Arc<CompiledRel99>,
    pub now: ControlTime,
}

impl<T: BoundDatagrams> PacketRabs<T> {
    pub(crate) fn modify_existing<C: umts_core::Transport>(
        &mut self,
        core: &mut CoreLink<C>,
        receive: &mut DedicatedController,
        modification: PacketModification<'_>,
    ) -> Result<(), ModificationError> {
        let PacketModification {
            request,
            item,
            identity,
            plan,
            now,
        } = modification;
        // TS 25.413 8.2.4 rejects this NAS-only transport modification without changing the RAB.
        if item.first.nas_synchronization.is_some()
            && item.first.transport.is_some()
            && item.first.parameters.is_none()
            && item.first.user_plane.is_none()
            && item.first.handover.is_none()
        {
            return Err(ModificationError::Packet(Error::Unsupported));
        }
        let key = RouteKey {
            connection: ConnectionKey {
                ue: request.key(),
                domain: CnDomain::Ps,
            },
            rab: item.first.id.get(),
        };
        let active = self
            .active
            .get(&key)
            .filter(|active| {
                active.ticket.request.same_connection(request) && Arc::ptr_eq(&active.plan, plan)
            })
            .ok_or(ModificationError::Packet(Error::Mapping))?;
        let bearer = self
            .bearers
            .retained_bearer(active.ticket.route, identity, plan)
            .map_err(|error| ModificationError::Packet(Error::Binding(error)))?;
        let binding = RadioBinding {
            identity,
            plan: plan.clone(),
            rab: key.rab,
            bearer,
            nas: active.requested.first.nas_synchronization_value(),
            evidence: RadioEvidence::Completed,
        };
        let authority = core
            .gateway()
            .authorize_rab(request, key.rab, RabOperation::SetupOrModify)
            .map_err(ModificationError::Core)?;
        let ticket = self
            .prepare(authority, item, binding)
            .map_err(ModificationError::Packet)?;
        let outcome = RabOutcome::Setup(self.setup_outcome(ticket));
        // TS 25.413 8.2.2 permits preserving the radio configuration for transport-only changes.
        match core
            .gateway_mut()
            .record_rab_outcome_with(now, request, outcome, |authority| {
                self.complete(ticket, authority, receive)
            }) {
            Ok(_) => Ok(()),
            Err(error) => {
                self.cancel(ticket)
                    .map_err(ModificationError::Cancellation)?;
                Err(match error {
                    umts_core::RabCommitError::Core(error) => ModificationError::Core(error),
                    umts_core::RabCommitError::Commit(error) => ModificationError::Packet(error),
                })
            }
        }
    }
}
