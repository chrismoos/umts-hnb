// SPDX-License-Identifier: AGPL-3.0-or-later

use super::inventory::{InventoryError, RabInventory};
use super::setup::SetupSource;
use super::{AdmissionError, Rel99Admission, SetupAfterRelease};
use crate::{
    dedicated_controller::DedicatedController,
    dedicated_rx::ReceiveIdentity,
    rab_handoff::PendingRabAssignment,
    radio_resources::{AllocatedRadio, RadioResources},
    rel99_planner::{
        ConfiguredPolicy, InitialOwnerSettings, OwnerBuildError, PreparedInitial, RabPlan,
        SelectionError,
    },
    signalling::CommonSignalling,
};
use umts_core::{Gateway, Transport};
use umts_iuh::Domain;
use umts_types::CnDomain;

pub(super) fn authorization<C: Transport>(
    core: &Gateway<C>,
    request: umts_iuh::session::RabRequest,
    rab: u8,
) -> Result<Option<umts_iuh::session::RabAuthorization<'_>>, umts_core::Error> {
    match core.authorize_rab(request, rab, umts_iuh::session::RabOperation::SetupOrModify) {
        Ok(proof) => Ok(Some(proof)),
        Err(
            umts_core::Error::State
            | umts_core::Error::Session(
                umts_iuh::Error::Identity | umts_iuh::Error::Origin | umts_iuh::Error::State,
            ),
        ) => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub enum RabAdmissionError {
    Construction,
    Hsdpa(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains catalog construction context for diagnostics."
            )
        )]
        String,
    ),
    Authority(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_core::Error,
    ),
    Selection(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        SelectionError,
    ),
    Planning(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::rel99_planner::Error,
    ),
    OwnerSettings(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        OwnerBuildError,
    ),
    Owners(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        OwnerBuildError,
    ),
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
        super::InitialPacketError,
    ),
    Circuit(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        super::InitialCircuitError,
    ),
    Inventory(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        InventoryError,
    ),
}

#[must_use]
pub struct RejectedRabAssignment {
    pub error: RabAdmissionError,
    pub pending: PendingRabAssignment,
    pub allocation: Option<AllocatedRadio>,
    pub rollback_error: Option<crate::radio_resources::Error>,
}

#[must_use = "Retain the core request and unpublished allocation until admission or cancellation"]
pub struct PreparedRabAssignment {
    source: SetupSource,
    prepared: PreparedInitial,
    packet: Option<super::PacketAssignment>,
    inventory: RabInventory,
}

impl PreparedRabAssignment {
    pub fn radio(&self) -> &PreparedInitial {
        &self.prepared
    }

    pub(crate) fn reject(
        self: Box<Self>,
        resources: &mut RadioResources,
        error: RabAdmissionError,
    ) -> Box<RejectedRabAssignment> {
        let Self {
            source, prepared, ..
        } = *self;
        reject(resources, source, Some(prepared.allocation), error)
    }
}

pub struct InitialRabHost<'a, T: Transport> {
    pub gateway: &'a Gateway<T>,
    pub policy: &'a ConfiguredPolicy,
    pub radio: &'a mut CommonSignalling,
    pub resources: &'a mut RadioResources,
    pub receive: &'a DedicatedController,
    pub admission: &'a mut Rel99Admission,
}

impl<T: Transport> InitialRabHost<'_, T> {
    #[cfg(test)]
    pub fn begin(
        &mut self,
        pending: PendingRabAssignment,
        settings: InitialOwnerSettings,
    ) -> Result<ReceiveIdentity, Box<RejectedRabAssignment>> {
        self.begin_with_settings(pending, |_| Ok(settings))
    }

    #[cfg(test)]
    pub fn begin_with_settings(
        &mut self,
        pending: PendingRabAssignment,
        make_settings: impl FnOnce(&PreparedInitial) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedRabAssignment>> {
        let domain = match pending.request().domain() {
            Domain::Cs => CnDomain::Cs,
            Domain::Ps => CnDomain::Ps,
        };
        if let Err(error) = self.policy.validate_rel99_packet_setup(domain) {
            return Err(Box::new(RejectedRabAssignment {
                error: RabAdmissionError::Hsdpa(error.to_string()),
                pending,
                allocation: None,
                rollback_error: None,
            }));
        }
        let prepared = self.prepare(pending)?;
        self.begin_prepared(prepared, make_settings)
    }

    pub fn prepare(
        &mut self,
        pending: PendingRabAssignment,
    ) -> Result<Box<PreparedRabAssignment>, Box<RejectedRabAssignment>> {
        self.prepare_source(SetupSource::Fresh(pending))
    }

    pub fn begin_after_release_with_settings(
        &mut self,
        released: SetupAfterRelease,
        make_settings: impl FnOnce(&PreparedInitial) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedRabAssignment>> {
        let domain = match released.request().domain() {
            Domain::Cs => CnDomain::Cs,
            Domain::Ps => CnDomain::Ps,
        };
        if let Err(error) = self.policy.validate_rel99_packet_setup(domain) {
            return Err(Box::new(RejectedRabAssignment {
                error: RabAdmissionError::Hsdpa(error.to_string()),
                pending: released.into_pending(),
                allocation: None,
                rollback_error: None,
            }));
        }
        let prepared = self.prepare_source(SetupSource::AfterRelease(released))?;
        self.begin_prepared(prepared, make_settings)
    }

    fn prepare_source(
        &mut self,
        source: SetupSource,
    ) -> Result<Box<PreparedRabAssignment>, Box<RejectedRabAssignment>> {
        let result = (|| {
            let pending = source.pending();
            let assignment = pending.assignment();
            let selection = source.selection().ok_or(RabAdmissionError::Construction)?;
            // The Session commits authority after event publication, never inside that callback.
            let _authority = selection
                .authorize_items(self.gateway)
                .map_err(RabAdmissionError::Authority)?;
            let domain = match pending.request().domain() {
                Domain::Cs => CnDomain::Cs,
                Domain::Ps => CnDomain::Ps,
            };
            let requests = assignment
                .setup
                .iter()
                .map(|item| (domain, item))
                .collect::<Vec<_>>();
            let selected = self
                .policy
                .services()
                .select(&requests)
                .map_err(RabAdmissionError::Selection)?;
            let packet = if pending.request().domain() == Domain::Ps {
                Some(super::PacketAssignment::new(selection).map_err(RabAdmissionError::Packet)?)
            } else {
                None
            };
            let planned = self
                .policy
                .plan_initial_with_resources(
                    pending.request().key(),
                    self.radio,
                    selected.rabs(),
                    self.resources,
                )
                .map_err(RabAdmissionError::Planning)?;
            let inventory = RabInventory::initial(pending.request(), planned.rabs())
                .map_err(RabAdmissionError::Inventory)?;
            let prepared = planned
                .allocate(pending.request().key(), self.radio, self.resources)
                .map_err(RabAdmissionError::Planning)?;
            let physical = prepared.allocation.plan().configuration().physical();
            let uplink = prepared.allocation.plan().configuration().uplink();
            tracing::debug!(
                "Dedicated radio allocation prepared: key={:?} sf={} codes={:?} secondary={:?} uplink_scrambling={} uplink_transports={:?} uplink_ctfc={:?} downlink_transports={:?} downlink_ctfc={:?}",
                pending.request().key(),
                physical.downlink_sf,
                physical.downlink_codes,
                physical.secondary_scrambling,
                physical.uplink_scrambling,
                uplink.transports(),
                uplink.ctfc_values(),
                prepared
                    .allocation
                    .plan()
                    .configuration()
                    .downlink()
                    .transports(),
                prepared
                    .allocation
                    .plan()
                    .configuration()
                    .downlink()
                    .ctfc_values(),
            );
            Ok((prepared, packet, inventory))
        })();
        match result {
            Ok((prepared, packet, inventory)) => Ok(Box::new(PreparedRabAssignment {
                source,
                prepared,
                packet,
                inventory,
            })),
            Err(error) => Err(self.reject(source, None, error)),
        }
    }

    pub fn begin_prepared(
        &mut self,
        request: Box<PreparedRabAssignment>,
        make_settings: impl FnOnce(&PreparedInitial) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReceiveIdentity, Box<RejectedRabAssignment>> {
        let authority = request
            .source
            .selection()
            .ok_or(RabAdmissionError::Construction)
            .and_then(|selection| {
                selection
                    .authorize_items(self.gateway)
                    .map_err(RabAdmissionError::Authority)
            });
        if let Err(error) = authority {
            return Err(self.cancel_prepared(request, error));
        }
        let settings = match make_settings(request.radio()) {
            Ok(settings) => settings,
            Err(error) => {
                return Err(self.cancel_prepared(request, RabAdmissionError::OwnerSettings(error)));
            }
        };
        let PreparedRabAssignment {
            source,
            mut prepared,
            packet,
            inventory,
        } = *request;
        let mut allocation = None;
        let mut circuit = None;
        let result = (|| {
            if source.pending().request().domain() == Domain::Cs {
                match super::CircuitAssignment::new(
                    source.selection().expect("authorized setup selection"),
                    std::mem::take(&mut prepared.rabs),
                ) {
                    Ok(circuits) => circuit = Some(circuits),
                    Err(error) => {
                        allocation = Some(prepared.allocation);
                        return Err(RabAdmissionError::Circuit(error));
                    }
                }
            }
            let mut built = match prepared.build_owners(self.radio, &self.admission.port, settings)
            {
                Ok(built) => built,
                Err(error) => {
                    allocation = Some(prepared.allocation);
                    return Err(RabAdmissionError::Owners(error));
                }
            };
            let feedback = match self
                .admission
                .prepare_hsdpa_feedback(built.identity(), |capacity, budget| {
                    built.configure_hsdpa_feedback(capacity, budget)
                }) {
                Ok(feedback) => feedback,
                Err(error) => {
                    allocation = Some(prepared.allocation);
                    return Err(RabAdmissionError::Owners(
                        crate::rel99_planner::OwnerBuildError::Receive(error),
                    ));
                }
            };
            match built.begin(prepared.allocation, self.admission, self.receive) {
                Ok(identity) => {
                    self.admission.commit_hsdpa_feedback(feedback);
                    Ok(identity)
                }
                Err(rejected) => {
                    allocation = Some(rejected.allocation);
                    Err(RabAdmissionError::Admission(rejected.error))
                }
            }
        })();
        match result {
            Ok(identity) => {
                let entry = self
                    .admission
                    .entry_mut(identity)
                    .expect("accepted initial admission");
                entry.inventory = Some(inventory);
                entry.rab_assignment = Some(source);
                entry.packet_assignment = packet;
                entry.circuit_assignment = circuit;
                Ok(identity)
            }
            Err(error) => Err(self.reject(source, allocation, error)),
        }
    }

    pub fn cancel_prepared(
        &mut self,
        request: Box<PreparedRabAssignment>,
        error: RabAdmissionError,
    ) -> Box<RejectedRabAssignment> {
        request.reject(self.resources, error)
    }

    fn reject(
        &mut self,
        source: SetupSource,
        allocation: Option<AllocatedRadio>,
        error: RabAdmissionError,
    ) -> Box<RejectedRabAssignment> {
        reject(self.resources, source, allocation, error)
    }
}

fn reject(
    resources: &mut RadioResources,
    source: SetupSource,
    mut allocation: Option<AllocatedRadio>,
    error: RabAdmissionError,
) -> Box<RejectedRabAssignment> {
    let mut rollback_error = None;
    if let Some(candidate) = allocation.take()
        && let Err(rejected) = resources.cancel(candidate)
    {
        rollback_error = Some(rejected.error);
        allocation = Some(rejected.allocation);
    }
    Box::new(RejectedRabAssignment {
        error,
        pending: source.into_pending(),
        allocation,
        rollback_error,
    })
}

impl Rel99Admission {
    pub fn rab_assignment(&self, identity: ReceiveIdentity) -> Option<&PendingRabAssignment> {
        self.entry(identity)?
            .rab_assignment
            .as_ref()
            .map(SetupSource::pending)
    }

    pub fn active_rabs(&self, identity: ReceiveIdentity) -> Option<Vec<RabPlan>> {
        let entry = self.entry(identity)?;
        if entry.lifecycle.phase() != super::AdmissionPhase::Active || entry.failure.is_some() {
            return None;
        }
        entry
            .inventory
            .as_ref()
            .and_then(|inventory| (!inventory.initial_pending()).then(|| inventory.active_plans()))
    }

    #[cfg(test)]
    pub(crate) fn record_radio_fixture_addition(
        &mut self,
        identity: ReceiveIdentity,
        request: umts_iuh::session::RabRequest,
        plans: &[RabPlan],
    ) {
        let inventory = self
            .entry_mut(identity)
            .unwrap()
            .inventory
            .as_mut()
            .unwrap();
        let prepared = inventory.prepare_addition(request, plans).unwrap();
        let mut outcomes = super::RabOutcomeIds::default();
        for plan in plans {
            outcomes.record_resolved(plan.rab, true);
        }
        inventory.commit_addition(prepared, &outcomes).unwrap();
    }

    pub fn additional_request_ready(&self, identity: ReceiveIdentity) -> bool {
        self.active_radio(identity).is_ok()
            && self.entry(identity).is_some_and(|entry| {
                entry
                    .inventory
                    .as_ref()
                    .is_some_and(|inventory| !inventory.initial_pending())
            })
    }
}
