// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::setup::SetupSource;
use super::*;
use crate::{
    core_link::CoreLink,
    packet_rabs::PacketRabs,
    rab_handoff::PendingRabAssignment,
    rel99_admission::{
        CircuitAssignment, CircuitPlayoutTime, CircuitSettings, InitialCircuitError,
        InitialPacketError, PacketAssignment, RabAdmissionError, RabOutcomeIds,
        inventory::{PreparedRabInventory, RabInventory},
    },
    rel99_planner::RabPlan,
    voice_bearers::VoiceTraffic,
};
use umts_core::{Gateway, Transport};
use umts_iuh::{
    Domain,
    ranap::rab::{Cause, SetupOrModify},
    session::ControlTime,
};
use umts_user_plane::packet::BoundDatagrams;

pub(super) enum RequestMedia {
    Packet(PacketAssignment),
    Circuit(CircuitAssignment),
}

impl RequestMedia {
    pub(super) fn new(
        selection: crate::rab_handoff::SetupAssignment<'_>,
        plans: &[RabPlan],
    ) -> Result<Self, RabAdmissionError> {
        match selection.pending().request().domain() {
            Domain::Ps => PacketAssignment::new(selection)
                .map(Self::Packet)
                .map_err(RabAdmissionError::Packet),
            Domain::Cs => CircuitAssignment::new(selection, plans.to_vec())
                .map(Self::Circuit)
                .map_err(RabAdmissionError::Circuit),
        }
    }

    fn outcomes(&self) -> RabOutcomeIds {
        match self {
            Self::Packet(state) => state.outcome_ids(),
            Self::Circuit(state) => state.outcome_ids(),
        }
    }
}

enum InventoryState {
    Awaiting(PreparedRabInventory),
    Recorded,
}

#[must_use = "Retain request authority and media owners until exact retirement"]
pub struct AdditionalAssignment {
    source: SetupSource,
    inventory: InventoryState,
    pub(super) media: RequestMedia,
}

#[derive(Debug)]
pub enum AdditionalMediaError {
    Admission(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdmissionError,
    ),
    Packet(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        InitialPacketError,
    ),
    Circuit(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        InitialCircuitError,
    ),
    Inventory(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::rel99_admission::InventoryError,
    ),
}

impl AdditionalAssignment {
    // Media preparation is gated on Committed, so only unpromoted rollback may discard this idle media state.
    pub(in crate::rel99_admission) fn into_pending(self) -> PendingRabAssignment {
        self.source.into_pending()
    }
    pub(super) fn new(
        source: SetupSource,
        inventory: PreparedRabInventory,
        media: RequestMedia,
    ) -> Self {
        Self {
            source,
            inventory: InventoryState::Awaiting(inventory),
            media,
        }
    }

    pub fn request(&self) -> umts_iuh::session::RabRequest {
        self.source.pending().request()
    }
    pub(in crate::rel99_admission) fn pending(&self) -> &PendingRabAssignment {
        self.source.pending()
    }
    pub(super) fn selection(&self) -> crate::rab_handoff::SetupAssignment<'_> {
        self.source.selection().expect("owned setup source")
    }

    pub fn outcomes_recorded(&self) -> bool {
        matches!(self.inventory, InventoryState::Recorded)
    }
    pub(in crate::rel99_admission) fn circuits_mut(&mut self) -> Option<&mut CircuitAssignment> {
        match &mut self.media {
            RequestMedia::Circuit(state) => Some(state),
            RequestMedia::Packet(_) => None,
        }
    }

    pub(in crate::rel99_admission) fn finish_radio_release(
        &mut self,
        released: &[crate::dedicated_bearers::BearerId],
        removed_packets: &[umts_user_plane::packet::RouteHandle],
    ) {
        match &mut self.media {
            RequestMedia::Packet(state) => state.finish_radio_release(removed_packets),
            RequestMedia::Circuit(state) => state.finish_radio_release(released),
        }
    }

    pub(in crate::rel99_admission) fn retired(&self, voices: &VoiceTraffic) -> bool {
        match &self.media {
            RequestMedia::Packet(state) => {
                let progress = state.progress();
                progress.retired == progress.items
            }
            RequestMedia::Circuit(state) => {
                let progress = state.progress(voices);
                progress.retired == progress.items
            }
        }
    }

    pub(super) fn promote<C: Transport>(
        &mut self,
        inventory: &mut RabInventory,
        gateway: &Gateway<C>,
    ) -> Result<bool, AdditionalMediaError> {
        if self.outcomes_recorded() {
            return Ok(true);
        }
        // A superseded request has no media and no response. Its bearers exist at the UE,
        // so they are recorded for the Iu release to remove.
        let outcomes = if gateway.rab_superseded(self.source.pending().request()) {
            let mut outcomes = RabOutcomeIds::default();
            for item in &self.source.pending().assignment().setup {
                outcomes.record_resolved(item.first.id.get(), true);
            }
            outcomes
        } else {
            self.media.outcomes()
        };
        if outcomes.resolved().len() != self.source.pending().assignment().setup.len() {
            return Ok(false);
        }
        let InventoryState::Awaiting(prepared) =
            std::mem::replace(&mut self.inventory, InventoryState::Recorded)
        else {
            unreachable!("unrecorded additional inventory")
        };
        if let Err((error, prepared)) =
            inventory.commit_addition_with_current_connection(prepared, &outcomes, |request| {
                gateway.check_rab_connection(request).is_ok()
            })
        {
            self.inventory = InventoryState::Awaiting(prepared);
            return Err(AdditionalMediaError::Inventory(error));
        }
        Ok(true)
    }

    pub(crate) fn finish_failure<C: Transport, D: BoundDatagrams>(
        &mut self,
        core: &mut CoreLink<C>,
        packets: &mut PacketRabs<D>,
        voices: &mut VoiceTraffic,
        now: ControlTime,
        cause: Cause,
    ) -> Result<(), AdditionalMediaError> {
        match &mut self.media {
            RequestMedia::Packet(state) => {
                state
                    .retire(packets)
                    .map_err(AdditionalMediaError::Packet)?;
                if core.gateway().owns_rab_request(state.request()) {
                    state
                        .finish_failure(core, packets, now, cause)
                        .map_err(AdditionalMediaError::Packet)?;
                }
            }
            RequestMedia::Circuit(state) => {
                state
                    .retire(voices)
                    .map_err(AdditionalMediaError::Circuit)?;
                if core.gateway().owns_rab_request(state.request()) {
                    state
                        .finish_failure(core, voices, now, cause)
                        .map_err(AdditionalMediaError::Circuit)?;
                }
            }
        }
        Ok(())
    }
}

pub struct AdditionalMediaServices<'a, C: Transport, D: BoundDatagrams> {
    pub core: &'a mut CoreLink<C>,
    pub receive: &'a mut DedicatedController,
    pub packets: &'a mut PacketRabs<D>,
    pub voices: &'a mut VoiceTraffic,
    pub now: ControlTime,
    pub circuit_time: CircuitPlayoutTime,
    pub circuit_budget: NonZeroUsize,
}

pub struct RetainedCircuitServices<'a, C: Transport> {
    pub gateway: &'a Gateway<C>,
    pub voices: &'a mut VoiceTraffic,
    pub receive: &'a mut DedicatedController,
    pub time: CircuitPlayoutTime,
    pub budget: NonZeroUsize,
}

impl Rel99Admission {
    pub fn retained_additional_assignments(
        &self,
        identity: ReceiveIdentity,
    ) -> Option<&[AdditionalAssignment]> {
        Some(&self.entry(identity)?.additional_assignments)
    }

    pub fn additional_assignment(
        &self,
        identity: ReceiveIdentity,
    ) -> Option<&AdditionalAssignment> {
        self.entry(identity)?
            .lifecycle
            .addition()?
            .assignment
            .as_ref()
    }

    pub fn service_additional_media<C: Transport, D: BoundDatagrams>(
        &mut self,
        identity: ReceiveIdentity,
        services: AdditionalMediaServices<'_, C, D>,
        mut settings: impl FnMut(
            &RabPlan,
            &SetupOrModify,
        ) -> Result<CircuitSettings, InitialCircuitError>,
    ) -> Result<bool, AdditionalMediaError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(AdditionalMediaError::Admission(AdmissionError::Identity))?;
        if entry.lifecycle.phase() != AdmissionPhase::Active {
            return Err(AdditionalMediaError::Admission(AdmissionError::Phase));
        }
        let addition = entry
            .lifecycle
            .addition_mut()
            .ok_or(AdditionalMediaError::Admission(AdmissionError::Phase))?;
        if addition.phase != AdditionPhase::Committed {
            return Err(AdditionalMediaError::Admission(AdmissionError::Phase));
        }
        let accepted = addition
            .accepted
            .as_ref()
            .ok_or(AdditionalMediaError::Admission(AdmissionError::Phase))?;
        let assignment = addition
            .assignment
            .as_mut()
            .ok_or(AdditionalMediaError::Admission(AdmissionError::Phase))?;
        let plan = entry
            .pool
            .as_ref()
            .ok_or(AdditionalMediaError::Admission(AdmissionError::Phase))?
            .plan();
        let earliest = entry
            .frame
            .as_ref()
            .map(|frame| frame.start.index.checked_add(CHIPS_PER_FRAME))
            .unwrap_or(Some(entry.next_frame.index))
            .ok_or(AdditionalMediaError::Admission(AdmissionError::Time))?;
        if services
            .core
            .gateway()
            .rab_superseded(assignment.source.pending().request())
        {
            return assignment.promote(
                entry
                    .inventory
                    .as_mut()
                    .ok_or(AdditionalMediaError::Admission(AdmissionError::Phase))?,
                services.core.gateway(),
            );
        }
        match &mut assignment.media {
            RequestMedia::Packet(state) => {
                if state.needs_preparation() {
                    state
                        .prepare(
                            assignment.source.selection().expect("owned setup source"),
                            crate::rel99_admission::packets::PacketRadio::AdditionalAccepted {
                                plan,
                                accepted,
                            },
                            services.core.gateway(),
                            services.packets,
                        )
                        .map_err(AdditionalMediaError::Packet)?;
                }
                state
                    .complete(
                        services.core,
                        services.packets,
                        services.receive,
                        services.now,
                    )
                    .map_err(AdditionalMediaError::Packet)?;
            }
            RequestMedia::Circuit(state) => {
                if state.needs_preparation() {
                    state
                        .prepare(
                            crate::rel99_admission::circuits::CircuitPreparation {
                                pending: assignment.source.selection().expect("owned setup source"),
                                identity,
                                plan,
                                gateway: services.core.gateway(),
                            },
                            services.voices,
                            &mut settings,
                        )
                        .map_err(AdditionalMediaError::Circuit)?;
                }
                state
                    .service(
                        crate::rel99_admission::circuits::CircuitService {
                            identity,
                            gateway: services.core.gateway(),
                            receive: services.receive,
                            time: crate::rel99_admission::circuits::CircuitIngressTime::Playout(
                                services.circuit_time,
                            ),
                            earliest_playout: ChipInstant {
                                epoch: identity.ue.epoch,
                                index: earliest,
                            },
                            radio_active: true,
                        },
                        services.voices,
                        services.circuit_budget,
                    )
                    .map_err(AdditionalMediaError::Circuit)?;
                state
                    .complete(
                        identity,
                        services.core,
                        services.voices,
                        services.receive,
                        services.now,
                    )
                    .map_err(AdditionalMediaError::Circuit)?;
            }
        }
        assignment.promote(
            entry
                .inventory
                .as_mut()
                .ok_or(AdditionalMediaError::Admission(AdmissionError::Phase))?,
            services.core.gateway(),
        )
    }

    pub fn service_retained_additional_circuits<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        services: RetainedCircuitServices<'_, C>,
    ) -> Result<usize, InitialCircuitError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(InitialCircuitError::Identity)?;
        if entry.lifecycle.phase() != AdmissionPhase::Active {
            return Err(InitialCircuitError::Phase);
        }
        let earliest = entry
            .frame
            .as_ref()
            .map(|frame| frame.start.index.checked_add(CHIPS_PER_FRAME))
            .unwrap_or(Some(entry.next_frame.index))
            .ok_or(InitialCircuitError::Configuration)?;
        let mut serviced = 0;
        let length = entry.additional_assignments.len();
        for _ in 0..services.budget.get().min(length) {
            let index = entry.additional_media_cursor % length;
            entry.additional_media_cursor = (index + 1) % length;
            let assignment = &mut entry.additional_assignments[index];
            if let RequestMedia::Circuit(state) = &mut assignment.media {
                state.service(
                    crate::rel99_admission::circuits::CircuitService {
                        identity,
                        gateway: services.gateway,
                        receive: services.receive,
                        time: crate::rel99_admission::circuits::CircuitIngressTime::Playout(
                            services.time,
                        ),
                        earliest_playout: ChipInstant {
                            epoch: identity.ue.epoch,
                            index: earliest,
                        },
                        radio_active: true,
                    },
                    services.voices,
                    NonZeroUsize::MIN,
                )?;
                serviced += 1;
            }
        }
        Ok(serviced)
    }
}

impl RetiredAdmission {
    pub fn finish_additional_media_failure<C: Transport, D: BoundDatagrams>(
        &mut self,
        core: &mut CoreLink<C>,
        packets: &mut PacketRabs<D>,
        voices: &mut VoiceTraffic,
        now: ControlTime,
        cause: Cause,
    ) -> Result<(), AdditionalMediaError> {
        for assignment in &mut self.additional_assignments {
            assignment.finish_failure(core, packets, voices, now, cause.clone())?;
        }
        Ok(())
    }
}
