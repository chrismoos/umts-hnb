// SPDX-License-Identifier: AGPL-3.0-or-later

mod activation;
pub mod candidate_admission;
mod grants;
mod receive;
pub mod receive_preparation;
mod restoration;
mod retirement;
pub use restoration::{Restoration, RestorationPhase};
mod timing;
pub(crate) use activation::MAX_DEDICATED_FLOWS;
pub use activation::{build_coexistence_activation, build_edch_only_activation};
pub use grants::{GrantCounters, GrantProgress, Grants};
#[cfg(test)]
pub use receive::SessionAssemblyError;
pub use receive::{ReceiveInputs, ReceiveResources, ReceiveSettings, RejectedReceiveSession};
mod preparation;
pub use preparation::{Preparation, PreparationInputs, PreparationPhase};

use crate::{
    dedicated_rx::{ReceiveIdentity, ReceiveOwner},
    signalling::{
        AcceptedHsupaActivation, CommonSignalling, ControlSecurity, HspaCommitError,
        ReceivedControl, SignallingError,
    },
};
use std::sync::Arc;
use umts_rrc::{
    dedicated::{
        DedicatedError, ReconfigurationCounters, ReconfigurationPhase as ActivationPhase,
        ReconfigurationResponse as ActivationResponse,
    },
    hsupa::ActivationProcedure,
};
use umts_security::ciphering::BearerCiphers;
use umts_types::time::ChipInstant;

enum Admission {
    Pending,
    Accepted(Box<AcceptedHsupaActivation>),
    Committed(Box<AcceptedHsupaActivation>),
    Active(Box<AcceptedHsupaActivation>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Restoring,
    Restored,
    Preparing,
    Prepared,
    AwaitingComplete,
    Accepted,
    AwaitingCommandAck,
    Active,
    Cancelling,
    Cancelled,
    Retiring,
    Retired,
    RecoveryRequired,
}

pub struct Session {
    preparation: Preparation,
    procedure: Box<ActivationProcedure>,
    admission: Admission,
    ingress: Option<crate::hsupa_rx::Ingress>,
    grants: Option<Grants>,
    retirement: Option<retirement::Retirement>,
    restoration: Option<Restoration>,
}

#[cfg(test)]
pub struct RejectedSession {
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub preparation: Preparation,
    #[cfg_attr(
        test,
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub procedure: Box<ActivationProcedure>,
}

impl Session {
    pub fn ingress_counters(&self) -> Option<crate::hsupa_rx::IngressCounters> {
        self.ingress.as_ref().map(|ingress| ingress.counters())
    }

    #[cfg(test)]
    pub fn new(
        preparation: Preparation,
        procedure: Box<ActivationProcedure>,
    ) -> Result<Self, Box<RejectedSession>> {
        if preparation.identity().ue != procedure.key()
            || !Arc::ptr_eq(preparation.configuration(), procedure.construction())
            || procedure.phase() != ActivationPhase::Prepared
            || preparation.phase() == PreparationPhase::RecoveryRequired
        {
            return Err(Box::new(RejectedSession {
                preparation,
                procedure,
            }));
        }
        Ok(Self {
            preparation,
            procedure,
            admission: Admission::Pending,
            ingress: None,
            grants: None,
            retirement: None,
            restoration: None,
        })
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.preparation.identity()
    }
    pub fn procedure(&self) -> &ActivationProcedure {
        &self.procedure
    }
    pub fn preparation(&mut self) -> &mut Preparation {
        &mut self.preparation
    }

    pub fn phase(&self) -> Phase {
        if let Some(retirement) = &self.retirement {
            if self.restoration.is_some() && retirement.phase() == Phase::Retired {
                return Phase::Restored;
            }
            return retirement.phase();
        }
        if let Some(restoration) = &self.restoration {
            return if restoration.phase() == RestorationPhase::RecoveryRequired {
                Phase::RecoveryRequired
            } else {
                Phase::Restoring
            };
        }
        if self.preparation.phase() == PreparationPhase::RecoveryRequired {
            return Phase::RecoveryRequired;
        }
        match self.procedure.phase() {
            ActivationPhase::FailureReported | ActivationPhase::RecoveryRequired => {
                Phase::RecoveryRequired
            }
            ActivationPhase::AbortedBeforePublication => {
                if self.preparation.phase() == PreparationPhase::Cancelled {
                    Phase::Cancelled
                } else {
                    Phase::Cancelling
                }
            }
            _ => match self.admission {
                Admission::Accepted(_) => Phase::Accepted,
                Admission::Committed(_) => Phase::AwaitingCommandAck,
                Admission::Active(_) => Phase::Active,
                Admission::Pending => match self.procedure.phase() {
                    ActivationPhase::Prepared
                        if self.preparation.phase() == PreparationPhase::Ready =>
                    {
                        Phase::Prepared
                    }
                    ActivationPhase::Prepared => Phase::Preparing,
                    _ => Phase::AwaitingComplete,
                },
            },
        }
    }

    pub fn publish(
        &mut self,
        radio: &mut CommonSignalling,
        now: ChipInstant,
    ) -> Result<(), SignallingError> {
        if self.phase() != Phase::Prepared || self.ingress.is_none() || self.grants.is_none() {
            return Err(SignallingError::Phase);
        }
        let identity = self.identity();
        radio.publish_hspa_activation_on(&mut self.procedure, identity, now)?;
        Ok(())
    }

    pub fn response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
        admit: impl FnOnce(ReconfigurationCounters, &mut BearerCiphers) -> Result<(), SignallingError>,
    ) -> Result<ActivationResponse, SignallingError> {
        if let Some(restoration) = &mut self.restoration {
            return restoration.response(received, security);
        }
        if self.retirement.is_some() || self.preparation.phase() != PreparationPhase::Ready {
            return Err(SignallingError::Phase);
        }
        match &self.admission {
            Admission::Accepted(accepted)
            | Admission::Committed(accepted)
            | Admission::Active(accepted) => {
                received.hspa_activation_duplicate(&mut self.procedure, accepted)?;
                Ok(ActivationResponse::Duplicate)
            }
            Admission::Pending => {
                let (response, accepted) =
                    received.hspa_activation_response_with(&mut self.procedure, security, admit)?;
                if let Some((accepted, ())) = accepted {
                    self.admission = Admission::Accepted(Box::new(accepted));
                }
                Ok(response)
            }
        }
    }

    pub fn commit<T, E>(
        &mut self,
        radio: &mut CommonSignalling,
        commit: impl FnOnce(&mut BearerCiphers) -> Result<T, E>,
    ) -> Result<T, HspaCommitError<E>> {
        if self.phase() != Phase::Accepted {
            return Err(HspaCommitError::Signalling(SignallingError::Phase));
        }
        let Admission::Accepted(accepted) = &self.admission else {
            unreachable!()
        };
        let result = radio.commit_hspa_activation_with(accepted, commit)?;
        let Admission::Accepted(accepted) =
            std::mem::replace(&mut self.admission, Admission::Pending)
        else {
            unreachable!()
        };
        self.admission = Admission::Committed(accepted);
        Ok(result)
    }

    pub fn finish_command(&mut self, radio: &mut CommonSignalling) -> Result<(), SignallingError> {
        if self.phase() != Phase::AwaitingCommandAck {
            return Err(SignallingError::Phase);
        }
        let Admission::Committed(accepted) = &self.admission else {
            unreachable!()
        };
        radio.release_hspa_activation_receipt(accepted)?;
        let Admission::Committed(accepted) =
            std::mem::replace(&mut self.admission, Admission::Pending)
        else {
            unreachable!()
        };
        self.admission = Admission::Active(accepted);
        Ok(())
    }

    pub fn expire(
        &mut self,
        radio: &mut CommonSignalling,
        now: ChipInstant,
    ) -> Result<bool, SignallingError> {
        if self.retirement.is_some() || self.restoration.is_some() {
            return Ok(false);
        }
        let changed = self
            .procedure
            .expire(now)
            .map_err(SignallingError::Dedicated)?;
        if self.procedure.phase() == ActivationPhase::AbortedBeforePublication {
            self.preparation.begin_cancellation();
        }
        if matches!(
            self.procedure.phase(),
            ActivationPhase::FailureReported | ActivationPhase::RecoveryRequired
        ) {
            radio.require_hspa_activation_recovery(&self.procedure)?;
        }
        Ok(changed)
    }

    pub fn abort_unpublished(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        if self.procedure.receipt().is_some() {
            return Err(DedicatedError::Phase);
        }
        self.procedure.local_failure(now)?;
        self.preparation.begin_cancellation();
        Ok(())
    }

    pub fn service_cancellation(
        &mut self,
        receive: &mut crate::dedicated_rx::DedicatedReceivePort,
        transmit: &mut impl crate::hsupa_tx::Control,
        air_now: ChipInstant,
    ) {
        if self.procedure.receipt().is_none()
            && self.procedure.phase() == ActivationPhase::AbortedBeforePublication
        {
            self.preparation
                .service_cancellation(receive, transmit, air_now);
        }
    }

    #[cfg(test)]
    pub fn into_cancelled(
        self,
    ) -> Result<
        (
            crate::dedicated_rx::HsupaAttachment,
            crate::radio_resources::HsupaUserLease,
        ),
        Box<Self>,
    > {
        if self.phase() != Phase::Cancelled {
            return Err(Box::new(self));
        }
        match self.preparation.into_cancelled() {
            Ok(resources) => Ok(resources),
            Err(preparation) => Err(Box::new(Self {
                preparation: *preparation,
                procedure: self.procedure,
                admission: self.admission,
                ingress: self.ingress,
                grants: self.grants,
                retirement: self.retirement,
                restoration: self.restoration,
            })),
        }
    }

    pub fn route_command(
        &mut self,
        event: crate::dedicated_tx::TransmitEvent,
    ) -> Option<crate::dedicated_tx::TransmitEvent> {
        let event = match &mut self.retirement {
            Some(retirement) => retirement.route(event)?,
            None => event,
        };
        let event = self.preparation.route_command(event)?;
        let event = match &mut self.grants {
            Some(grants) => grants.route_command(event)?,
            None => event,
        };
        match &mut self.ingress {
            Some(ingress) => ingress.route_command(event),
            None => Some(event),
        }
    }

    pub fn accept_completion(
        &mut self,
        completion: crate::hsupa_tx::Completion,
    ) -> Result<(), crate::hsupa_tx::Completion> {
        let completion = match &mut self.grants {
            Some(grants) => match grants.accept_completion(completion) {
                Ok(()) => return Ok(()),
                Err(completion) => completion,
            },
            None => completion,
        };
        match &mut self.ingress {
            Some(ingress) => ingress.accept_completion(completion),
            None => Err(completion),
        }
    }

    pub fn uplink_anchor(&self) -> crate::dedicated_rx::FrameAnchor {
        crate::dedicated_rx::FrameAnchor {
            time: self.preparation.receive_activation(),
            cfn: self.procedure.construction().activation().cfn(),
        }
    }

    pub fn can_route_response(&self, transaction: u8, security: &ControlSecurity<'_>) -> bool {
        match &self.restoration {
            Some(restoration) => {
                restoration.procedure().receipt().is_some()
                    && restoration.procedure().transaction().get() == transaction
                    && security.can_route_hspa_activation(restoration.procedure())
            }
            None => {
                self.procedure.receipt().is_some()
                    && self.procedure.transaction().get() == transaction
                    && security.can_route_hspa_activation(&self.procedure)
            }
        }
    }

    pub fn begin_restoration(&mut self, restoration: Restoration) -> Result<(), Box<Restoration>> {
        if self.phase() != Phase::Active
            || restoration.identity() != self.identity()
            || !Arc::ptr_eq(
                restoration.procedure().construction().previous(),
                self.procedure.construction(),
            )
            || timing::receive_activation(
                restoration.procedure().construction().activation(),
                crate::dedicated_rx::FrameAnchor {
                    time: self.preparation.receive_activation(),
                    cfn: self.procedure.construction().activation().cfn(),
                },
            )
            .ok()
                != Some(restoration.receive_boundary())
            || self.grants.as_mut().is_none_or(|grants| {
                grants
                    .quiesce_before(restoration.procedure().construction().activation().time())
                    .is_err()
            })
        {
            return Err(Box::new(restoration));
        }
        self.restoration = Some(restoration);
        Ok(())
    }

    pub fn service_restoration(
        &mut self,
        radio: &mut CommonSignalling,
        receive: &mut crate::dedicated_rx::DedicatedReceivePort,
        now: ChipInstant,
    ) -> anyhow::Result<()> {
        let identity = self.identity();
        let Some(restoration) = &mut self.restoration else {
            return Ok(());
        };
        let grants_pending = self.grants.as_ref().is_some_and(|grants| grants.pending());
        let (packets_pending, feedback_outstanding, report_pending) =
            self.ingress.as_mut().map_or((false, 0, false), |ingress| {
                (
                    !ingress.packets().is_empty(),
                    ingress.outstanding_feedback(),
                    !ingress.drained()
                        && ingress.packets().is_empty()
                        && ingress.outstanding_feedback() == 0,
                )
            });
        let drained =
            !grants_pending && !packets_pending && feedback_outstanding == 0 && !report_pending;
        if let Err(error) = restoration.service(radio, receive, now, drained) {
            tracing::warn!(
                "HSUPA restoration stopped: identity={identity:?} grants_pending={grants_pending} packets_pending={packets_pending} feedback_outstanding={feedback_outstanding} report_pending={report_pending}: {error:#}"
            );
            return Err(error);
        }
        if let Some(receiver) = restoration.take_detached() {
            self.retirement = Some(retirement::Retirement::restored(
                identity,
                self.procedure.construction().parameters().e_rnti,
                receiver,
            ));
        }
        Ok(())
    }

    // The UE radio context must be released. This is not an E-DCH downgrade on a live connection.
    pub fn begin_ue_release(
        &mut self,
        removed: Box<crate::dedicated_rx::DedicatedSampleReceiver>,
    ) -> Result<(), Box<crate::dedicated_rx::DedicatedSampleReceiver>> {
        self.preparation.poll_receive();
        if self
            .restoration
            .as_mut()
            .is_some_and(|restoration| !restoration.reconcile_for_ue_release())
        {
            return Err(removed);
        }
        let attached = removed.hsupa_attachment();
        let detached = self
            .restoration
            .as_ref()
            .is_some_and(|restoration| restoration.owns_detached());
        if self.retirement.is_some()
            || !self.preparation.ready_for_ue_release()
            || removed.session().identity() != self.identity()
            || !(attached == Some((self.identity(), self.preparation.receive_activation()))
                && !detached
                || attached.is_none() && detached)
        {
            return Err(removed);
        }
        self.retirement = Some(retirement::Retirement::new(
            self.identity(),
            self.procedure.construction().parameters().e_rnti,
            removed,
        ));
        if let Some(component) = self
            .restoration
            .as_mut()
            .and_then(|restoration| restoration.take_for_ue_release())
        {
            self.retirement
                .as_mut()
                .expect("new retirement")
                .retain_component(component);
        }
        Ok(())
    }

    /// Retire a session whose UE radio context was released without handing back its
    /// sample receiver. Nothing remains to recycle on the receive side, so the retirement
    /// only returns the transmit-side state and the E-RNTI lease.
    pub fn retire_orphaned(&mut self) -> bool {
        if self.retirement.is_some() {
            return false;
        }
        self.retirement = Some(retirement::Retirement::orphaned(
            self.identity(),
            self.procedure.construction().parameters().e_rnti,
        ));
        true
    }

    pub fn service_ue_release(
        &mut self,
        now: ChipInstant,
        transmit: &mut impl crate::hsupa_tx::Control,
    ) {
        let Some(retirement) = &mut self.retirement else {
            return;
        };
        let reports_empty = self
            .ingress
            .as_mut()
            .is_none_or(|ingress| ingress.discard_one_for_release());
        let controls_empty = self.grants.as_ref().is_none_or(|grants| !grants.pending())
            && self
                .ingress
                .as_ref()
                .is_none_or(|ingress| ingress.outstanding_feedback() == 0);
        retirement.service(reports_empty, controls_empty, now, transmit);
    }

    pub fn retirement_failure(&self) -> Option<&crate::dedicated_tx::TransmitEvent> {
        self.retirement.as_ref()?.failure()
    }

    pub(crate) fn release_retired_resources(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<bool, crate::radio_resources::Error> {
        match self.retirement.as_mut() {
            Some(retirement) => retirement.release_resources(resources),
            None => Ok(false),
        }
    }

    pub(crate) fn release_cancelled_resources(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<bool, crate::radio_resources::Error> {
        if self.phase() != Phase::Cancelled {
            return Ok(false);
        }
        self.preparation.release_cancelled_resources(resources)
    }

    pub fn uplink_service_ready(&self, now: ChipInstant) -> bool {
        if matches!(
            self.phase(),
            Phase::AwaitingCommandAck | Phase::Active | Phase::Restoring
        ) {
            return true;
        }
        let configuration = self.procedure.construction();
        // TS 25.331 8.6.3.1 applies the radio configuration at activation, before Complete.
        matches!(self.phase(), Phase::AwaitingComplete | Phase::Accepted)
            && now.epoch == configuration.activation().time().epoch
            && now.index >= configuration.activation().time().index
    }

    pub fn uplink_demand(&self, now: ChipInstant) -> bool {
        self.grants
            .as_ref()
            .is_some_and(|grants| grants.demand(now))
    }

    pub fn uplink_use(&self) -> Option<(bool, Option<f64>, f64, u8)> {
        self.grants.as_ref().map(|grants| {
            (
                grants.backlog(),
                grants.used_bits_per_interval(),
                grants.most_used_bits_per_interval(),
                grants.allocation(),
            )
        })
    }

    pub fn carries_voice(&self) -> bool {
        self.procedure
            .construction()
            .dedicated()
            .bearers()
            .iter()
            .any(|bearer| bearer.domain == Some(umts_types::CnDomain::Cs))
    }

    pub fn allocate_uplink(&mut self, now: ChipInstant, processes: u8) {
        let Some(grants) = self.grants.as_mut() else {
            return;
        };
        if grants.allocate(now, processes) {
            crate::signalling::desire_process_allocation(
                self.identity().ue,
                processes,
                self.procedure.construction(),
            );
        }
    }

    pub fn service_grants(
        &mut self,
        now: ChipInstant,
        next_transmit: ChipInstant,
        transmit: &mut impl crate::hsupa_tx::Control,
    ) -> Result<GrantProgress, crate::hsupa_tx::Error> {
        if !self.uplink_service_ready(now) {
            return Err(crate::hsupa_tx::Error::Unavailable);
        }
        let activity = self
            .ingress
            .as_ref()
            .map(|ingress| ingress.grant_activity());
        let grants = self
            .grants
            .as_mut()
            .ok_or(crate::hsupa_tx::Error::Unavailable)?;
        if let Some(activity) = activity {
            grants.observe_uplink(now, activity);
        }
        grants.service(now, next_transmit, transmit)
    }

    pub fn service_ingress(
        &mut self,
        now: ChipInstant,
        next_transmit: ChipInstant,
        transmit: &mut impl crate::hsupa_tx::Control,
    ) -> Result<crate::hsupa_rx::IngressStep, crate::hsupa_rx::IngressError> {
        if !self.uplink_service_ready(now) {
            return Err(crate::hsupa_rx::IngressError::Configuration);
        }
        self.ingress
            .as_mut()
            .ok_or(crate::hsupa_rx::IngressError::Configuration)?
            .service_one(now, next_transmit, transmit)
    }

    pub fn grant_counters(&self) -> Option<GrantCounters> {
        self.grants.as_ref().map(Grants::counters)
    }

    pub fn service_packets_routed<E>(
        &mut self,
        bank: &mut crate::dedicated_bearers::DedicatedBearers,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: crate::dedicated_bearers::ResetPermission,
        route: impl FnMut(
            crate::dedicated_bearers::SignallingPdu<'_>,
            &mut BearerCiphers,
        ) -> Result<bool, E>,
    ) -> Result<crate::dedicated_bearers::BearerProgress, crate::dedicated_bearers::ServiceError<E>>
    {
        use crate::dedicated_bearers::{BearerError, BearerProgress};
        if !self.uplink_service_ready(now) {
            return Err(BearerError::Phase.into());
        }
        if bank.identity() != self.identity() {
            return Err(BearerError::Owner.into());
        }
        let user_data = matches!(
            self.phase(),
            Phase::AwaitingCommandAck | Phase::Active | Phase::Restoring
        );
        let configuration = self.procedure.construction();
        let ingress = self.ingress.as_mut().ok_or(BearerError::Phase)?;
        let packets = ingress.packets();
        let Some(pdu) = packets
            .peek_matching(now.index, |logical| {
                user_data
                    || configuration.mappings().iter().any(|mapping| {
                        mapping.uplink.logical() == logical
                            && [
                                umts_rrc::nas::SignallingBearer::Unacknowledged,
                                umts_rrc::nas::SignallingBearer::ControlAm,
                                umts_rrc::nas::SignallingBearer::NasAm,
                            ]
                            .iter()
                            .any(|bearer| bearer.identity() == mapping.bearer)
                    })
            })
            .map_err(BearerError::HsupaPacket)?
        else {
            return Ok(BearerProgress::Idle);
        };
        let progress = bank.receive_hsupa_routed(configuration, pdu, now, ciphers, reset, route)?;
        if matches!(progress, BearerProgress::Pdu | BearerProgress::Dropped) {
            assert!(
                packets.release(),
                "accepted HSUPA PDU remains owned until release"
            );
        }
        Ok(progress)
    }
}
