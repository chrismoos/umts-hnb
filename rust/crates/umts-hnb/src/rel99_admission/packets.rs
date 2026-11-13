// SPDX-License-Identifier: AGPL-3.0-or-later

use super::inventory::RabOutcomeIds;
use super::{AdmissionPhase, Rel99Admission, RetiredAdmission};
use crate::{
    core_link::CoreLink,
    dedicated_controller::DedicatedController,
    dedicated_rx::ReceiveIdentity,
    packet_rabs::{self, PacketRabs, RadioBinding, Ticket},
    signalling::AcceptedAdditionalSetup,
};
use std::sync::Arc;
use umts_core::{Gateway, Transport};
use umts_iuh::{
    Domain,
    ranap::rab::{Cause, Failed, Setup},
    session::{ControlTime, RabAuthorization, RabOperation, RabOutcome, RabRequest},
};
use umts_rrc::dedicated::{RadioBearerSetupProcedure, SetupPhase};
use umts_user_plane::packet::BoundDatagrams;

#[derive(Debug)]
pub enum InitialPacketError {
    Phase,
    Identity,
    Capacity,
    Inventory(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        super::inventory::InventoryError,
    ),
    Packet {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        rab: u8,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        error: packet_rabs::Error,
    },
    Core {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        error: umts_core::Error,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketProgress {
    pub items: usize,
    pub prepared: usize,
    pub bound: usize,
    pub recorded: usize,
    pub retired: usize,
    pub superseded: usize,
}

#[derive(Clone, Copy)]
pub enum PacketRadio<'a> {
    InitialPrepared {
        identity: ReceiveIdentity,
        plan: &'a Arc<crate::rel99_plan::CompiledRel99>,
        procedure: &'a RadioBearerSetupProcedure,
    },
    AdditionalAccepted {
        plan: &'a Arc<crate::rel99_plan::CompiledRel99>,
        accepted: &'a AcceptedAdditionalSetup,
    },
}

#[derive(Clone, Copy)]
enum Disposition {
    Unreported,
    Recorded,
    Superseded,
}
enum Stage {
    Unprepared(u8),
    Prepared(Ticket),
    Bound { ticket: Ticket, setup: Setup },
    Recorded(Ticket),
    Retired { rab: u8, disposition: Disposition },
}
impl Stage {
    fn rab(&self) -> u8 {
        match self {
            Self::Unprepared(rab) | Self::Retired { rab, .. } => *rab,
            Self::Prepared(ticket) | Self::Bound { ticket, .. } | Self::Recorded(ticket) => {
                ticket.key().rab
            }
        }
    }
    fn ticket(&self) -> Option<Ticket> {
        match self {
            Self::Prepared(ticket) | Self::Bound { ticket, .. } | Self::Recorded(ticket) => {
                Some(*ticket)
            }
            Self::Unprepared(_) | Self::Retired { .. } => None,
        }
    }
}

#[must_use = "Retain packet attempts until exact transport cleanup or successful outcome recording"]
pub struct PacketAssignment {
    request: RabRequest,
    stages: Vec<Stage>,
}
impl PacketAssignment {
    pub(super) fn finish_radio_release(
        &mut self,
        removed: &[umts_user_plane::packet::RouteHandle],
    ) {
        for stage in &mut self.stages {
            let Some(ticket) = stage.ticket() else {
                continue;
            };
            if !removed.iter().any(|handle| handle.key() == ticket.key()) {
                continue;
            }
            let disposition = if matches!(stage, Stage::Recorded(_)) {
                Disposition::Recorded
            } else {
                Disposition::Superseded
            };
            *stage = Stage::Retired {
                rab: ticket.key().rab,
                disposition,
            };
        }
    }
    pub(super) fn new(
        selection: crate::rab_handoff::SetupAssignment<'_>,
    ) -> Result<Self, InitialPacketError> {
        let pending = selection.pending();
        if pending.request().domain() != Domain::Ps || pending.assignment().setup.is_empty() {
            return Err(InitialPacketError::Identity);
        }
        let mut stages = Vec::new();
        stages
            .try_reserve_exact(pending.assignment().setup.len())
            .map_err(|_| InitialPacketError::Capacity)?;
        stages.extend(
            pending
                .assignment()
                .setup
                .iter()
                .map(|item| Stage::Unprepared(item.first.id.get())),
        );
        Ok(Self {
            request: pending.request(),
            stages,
        })
    }

    pub(crate) fn prepare<C: Transport, D: BoundDatagrams>(
        &mut self,
        selection: crate::rab_handoff::SetupAssignment<'_>,
        radio: PacketRadio<'_>,
        core: &Gateway<C>,
        packets: &mut PacketRabs<D>,
    ) -> Result<PacketProgress, InitialPacketError> {
        let pending = selection.pending();
        if self.request != pending.request()
            || pending.request().domain() != Domain::Ps
            || pending.assignment().setup.is_empty()
            || self.stages.len() != pending.assignment().setup.len()
        {
            return Err(InitialPacketError::Identity);
        }
        let rab = self
            .stages
            .first()
            .ok_or(InitialPacketError::Identity)?
            .rab();
        core.check_rab_connection(self.request)
            .map_err(|error| InitialPacketError::Core { rab, error })?;
        for (stage, item) in self.stages.iter_mut().zip(&pending.assignment().setup) {
            if !matches!(stage, Stage::Unprepared(_)) {
                continue;
            }
            let rab = stage.rab();
            let authorization = core
                .authorize_rab(self.request, rab, RabOperation::SetupOrModify)
                .map_err(|error| InitialPacketError::Core { rab, error })?;
            let binding = match radio {
                PacketRadio::InitialPrepared {
                    identity,
                    plan,
                    procedure,
                } => RadioBinding::from_procedure(identity, (*plan).clone(), procedure, rab),
                PacketRadio::AdditionalAccepted { plan, accepted } => {
                    RadioBinding::from_additional_setup(plan, accepted, rab)
                }
            }
            .map_err(|error| InitialPacketError::Packet { rab, error })?;
            let ticket = packets
                .prepare(authorization, item, binding)
                .map_err(|error| InitialPacketError::Packet { rab, error })?;
            *stage = Stage::Prepared(ticket);
        }
        Ok(self.progress())
    }

    pub(crate) fn complete<C: Transport, D: BoundDatagrams>(
        &mut self,
        core: &mut CoreLink<C>,
        packets: &mut PacketRabs<D>,
        receive: &mut DedicatedController,
        now: ControlTime,
    ) -> Result<PacketProgress, InitialPacketError> {
        if !core.gateway().owns_rab_request(self.request) {
            return Err(InitialPacketError::Identity);
        }
        if self.stages.iter().any(|stage| {
            matches!(
                stage,
                Stage::Unprepared(_)
                    | Stage::Retired {
                        disposition: Disposition::Unreported,
                        ..
                    }
            )
        }) {
            return Err(InitialPacketError::Phase);
        }
        for stage in &mut self.stages {
            let rab = stage.rab();
            if matches!(stage, Stage::Prepared(_) | Stage::Bound { .. })
                && authorization(core.gateway(), self.request, rab)?.is_none()
            {
                let ticket = stage.ticket().expect("unfinished packet attempt");
                packets
                    .retire_attempt(ticket)
                    .map_err(|error| InitialPacketError::Packet { rab, error })?;
                *stage = Stage::Retired {
                    rab,
                    disposition: Disposition::Superseded,
                };
                continue;
            }
            if let Stage::Prepared(ticket) = stage {
                let authorization = core
                    .gateway()
                    .authorize_rab(self.request, rab, RabOperation::SetupOrModify)
                    .map_err(|error| InitialPacketError::Core { rab, error })?;
                let setup = packets
                    .complete(*ticket, authorization, receive)
                    .map_err(|error| InitialPacketError::Packet { rab, error })?;
                *stage = Stage::Bound {
                    ticket: *ticket,
                    setup,
                };
            }
            if let Stage::Bound { ticket, setup } = stage {
                core.gateway()
                    .authorize_rab(self.request, rab, RabOperation::SetupOrModify)
                    .map_err(|error| InitialPacketError::Core { rab, error })?;
                core.gateway_mut()
                    .record_rab_outcome(now, self.request, RabOutcome::Setup(setup.clone()))
                    .map_err(|error| InitialPacketError::Core { rab, error })?;
                *stage = Stage::Recorded(*ticket);
            }
        }
        Ok(self.progress())
    }

    pub fn request(&self) -> RabRequest {
        self.request
    }
    pub fn progress(&self) -> PacketProgress {
        let mut result = PacketProgress {
            items: self.stages.len(),
            ..PacketProgress::default()
        };
        for stage in &self.stages {
            match stage {
                Stage::Unprepared(_) => (),
                Stage::Prepared(_) => result.prepared += 1,
                Stage::Bound { .. } => result.bound += 1,
                Stage::Recorded(_) => result.recorded += 1,
                Stage::Retired { disposition, .. } => {
                    result.retired += 1;
                    match disposition {
                        Disposition::Recorded => result.recorded += 1,
                        Disposition::Superseded => result.superseded += 1,
                        Disposition::Unreported => (),
                    }
                }
            }
        }
        result
    }

    pub(crate) fn outcome_ids(&self) -> RabOutcomeIds {
        let mut outcomes = RabOutcomeIds::default();
        for stage in &self.stages {
            match stage {
                Stage::Recorded(ticket) => outcomes.record_resolved(ticket.key().rab, true),
                Stage::Retired {
                    rab,
                    disposition: Disposition::Recorded | Disposition::Superseded,
                } => outcomes.record_resolved(*rab, false),
                Stage::Retired { .. } => (),
                Stage::Unprepared(_) | Stage::Prepared(_) | Stage::Bound { .. } => (),
            }
        }
        outcomes
    }
    pub(super) fn prepared(&self) -> bool {
        self.stages.iter().all(|stage| {
            matches!(
                stage,
                Stage::Prepared(_) | Stage::Bound { .. } | Stage::Recorded(_)
            )
        })
    }
    pub(crate) fn needs_preparation(&self) -> bool {
        self.stages
            .iter()
            .any(|stage| matches!(stage, Stage::Unprepared(_)))
    }
    pub fn retire<D: BoundDatagrams>(
        &mut self,
        packets: &mut PacketRabs<D>,
    ) -> Result<usize, InitialPacketError> {
        let mut retired = 0;
        for stage in &mut self.stages {
            let rab = stage.rab();
            let disposition = match stage {
                Stage::Recorded(_) => Disposition::Recorded,
                Stage::Retired { disposition, .. } => *disposition,
                _ => Disposition::Unreported,
            };
            if let Some(ticket) = stage.ticket() {
                retired += usize::from(
                    packets
                        .retire_attempt(ticket)
                        .map_err(|error| InitialPacketError::Packet { rab, error })?,
                );
            }
            *stage = Stage::Retired { rab, disposition };
        }
        Ok(retired)
    }

    pub(crate) fn finish_failure<C: Transport, D: BoundDatagrams>(
        &mut self,
        core: &mut CoreLink<C>,
        packets: &mut PacketRabs<D>,
        now: ControlTime,
        cause: Cause,
    ) -> Result<PacketProgress, InitialPacketError> {
        if !core.gateway().owns_rab_request(self.request) {
            return Err(InitialPacketError::Identity);
        }
        self.retire(packets)?;
        for stage in &mut self.stages {
            let Stage::Retired { rab, disposition } = stage else {
                unreachable!("retired packet attempt")
            };
            if !matches!(disposition, Disposition::Unreported) {
                continue;
            }
            if authorization(core.gateway(), self.request, *rab)?.is_none() {
                *disposition = Disposition::Superseded;
                continue;
            }
            core.gateway_mut()
                .record_rab_outcome(
                    now,
                    self.request,
                    RabOutcome::SetupFailed(Failed::new(*rab, cause.clone())),
                )
                .map_err(|error| InitialPacketError::Core { rab: *rab, error })?;
            *disposition = Disposition::Recorded;
        }
        Ok(self.progress())
    }
}

impl Rel99Admission {
    pub fn packet_progress(&self, identity: ReceiveIdentity) -> Option<PacketProgress> {
        Some(self.entry(identity)?.packet_assignment.as_ref()?.progress())
    }

    pub fn prepare_packets<C: Transport, D: BoundDatagrams>(
        &mut self,
        identity: ReceiveIdentity,
        core: &Gateway<C>,
        packets: &mut PacketRabs<D>,
    ) -> Result<PacketProgress, InitialPacketError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(InitialPacketError::Identity)?;
        if matches!(
            entry.lifecycle.phase(),
            AdmissionPhase::Active | AdmissionPhase::Stopping | AdmissionPhase::Retired
        ) || entry.procedure.phase() != SetupPhase::Prepared
        {
            return Err(InitialPacketError::Phase);
        }
        let source = entry
            .rab_assignment
            .as_ref()
            .ok_or(InitialPacketError::Identity)?;
        let pending = source.pending();
        if pending.request().domain() != Domain::Ps {
            return Err(InitialPacketError::Identity);
        }
        let state = entry
            .packet_assignment
            .as_mut()
            .ok_or(InitialPacketError::Phase)?;
        let plan = entry.pool.as_ref().expect("live frame pool").plan().clone();
        state.prepare(
            source.selection().ok_or(InitialPacketError::Identity)?,
            PacketRadio::InitialPrepared {
                identity,
                plan: &plan,
                procedure: &entry.procedure,
            },
            core,
            packets,
        )
    }

    pub fn complete_packets<C: Transport, D: BoundDatagrams>(
        &mut self,
        identity: ReceiveIdentity,
        core: &mut CoreLink<C>,
        packets: &mut PacketRabs<D>,
        receive: &mut DedicatedController,
        now: ControlTime,
    ) -> Result<PacketProgress, InitialPacketError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(InitialPacketError::Identity)?;
        if entry.lifecycle.phase() != AdmissionPhase::Active {
            return Err(InitialPacketError::Phase);
        }
        let state = entry
            .packet_assignment
            .as_mut()
            .ok_or(InitialPacketError::Phase)?;
        let (progress, outcomes, request) = {
            let progress = state.complete(core, packets, receive, now)?;
            let outcomes = state.outcome_ids();
            let request = state.request();
            (progress, outcomes, request)
        };
        if outcomes.resolved().len() == progress.items
            && entry
                .inventory
                .as_ref()
                .is_some_and(|inventory| inventory.initial_pending())
        {
            entry
                .inventory
                .as_mut()
                .expect("checked initial inventory")
                .record_initial_outcomes(request, &outcomes)
                .map_err(InitialPacketError::Inventory)?;
        }
        Ok(progress)
    }
}

impl RetiredAdmission {
    pub fn finish_packet_failure<C: Transport, D: BoundDatagrams>(
        &mut self,
        core: &mut CoreLink<C>,
        packets: &mut PacketRabs<D>,
        now: ControlTime,
        cause: Cause,
    ) -> Result<PacketProgress, InitialPacketError> {
        let state = self
            .packet_assignment
            .as_mut()
            .ok_or(InitialPacketError::Phase)?;
        state.finish_failure(core, packets, now, cause)
    }
}

fn authorization<C: Transport>(
    core: &Gateway<C>,
    request: RabRequest,
    rab: u8,
) -> Result<Option<RabAuthorization<'_>>, InitialPacketError> {
    if !core.owns_rab_request(request) {
        return Err(InitialPacketError::Identity);
    }
    super::rabs::authorization(core, request, rab)
        .map_err(|error| InitialPacketError::Core { rab, error })
}
