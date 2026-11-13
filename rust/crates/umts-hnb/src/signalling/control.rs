// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rlc::acknowledged::{ReceiveTime, SduReceipt, SduTransmission};
use umts_rrc::dedicated::{
    DchDirection, DedicatedError, RadioBearerSetupProcedure, Rel99Rlc, RetainedBearer,
    SetupPublicationError, SetupResponse,
};
use umts_rrc_codec::{DecodedUplinkDedicated, decode_downlink_dedicated};
use umts_rrc_schema::{class_definitions::DLDCCHMessageType, information_elements as ie};
use umts_security::{ciphering::BearerCiphers, contexts::KeyOrigin};
use umts_types::CnDomain;

mod additional;
mod counters;
mod hspa;
mod kind;
mod publication;
mod release;
use publication::PublicationProtection;
#[cfg(test)]
pub(super) mod tests;
pub use additional::{AcceptedAdditionalSetup, AdditionalSetupCommitError};
pub use hspa::{
    AcceptedActivation, AcceptedHsdpaActivation, AcceptedHsupaActivation, HspaCommitError,
    HspaConfiguration,
};
pub use kind::ControlMessageKind;
pub use release::{AcceptedBearerRelease, BearerReleaseCommitError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlProtection {
    Unprotected,
    Integrity { origin: KeyOrigin, count: u32 },
}
pub struct ReceivedControl<'a> {
    pub(super) key: UeKey,
    pub(super) bearer: SignallingBearer,
    pub(super) time: ReceiveTime,
    pub(super) message: &'a DecodedUplinkDedicated,
    pub(super) protection: ControlProtection,
}
pub struct AcceptedUserSetup {
    key: UeKey,
    configuration: std::sync::Arc<umts_rrc::dedicated::Rel99Configuration>,
    hsdpa: Option<std::sync::Arc<umts_rrc::hsdpa::InitialConfiguration>>,
    time: ReceiveTime,
    counters: umts_rrc::dedicated::SetupCounters,
    prior_starts: umts_rrc::establishment::DomainStarts,
}
impl AcceptedUserSetup {
    pub fn prior_starts(&self) -> umts_rrc::establishment::DomainStarts {
        self.prior_starts
    }
    pub fn key(&self) -> UeKey {
        self.key
    }
    pub fn configuration(&self) -> &std::sync::Arc<umts_rrc::dedicated::Rel99Configuration> {
        &self.configuration
    }
    pub fn initial_hsdpa(&self) -> Option<&std::sync::Arc<umts_rrc::hsdpa::InitialConfiguration>> {
        self.hsdpa.as_ref()
    }
    pub fn time(&self) -> ReceiveTime {
        self.time
    }
    pub fn counters(&self) -> umts_rrc::dedicated::SetupCounters {
        self.counters
    }
}
impl<'a> ReceivedControl<'a> {
    pub fn bearer_setup_response_with<T>(
        &self,
        procedure: &mut RadioBearerSetupProcedure,
        security: &mut ControlSecurity<'_>,
        admit: impl FnOnce(AcceptedUserSetup, &mut BearerCiphers) -> Result<T, SignallingError>,
    ) -> Result<(SetupResponse, Option<T>), SignallingError> {
        let pending = security.change.as_ref().ok_or(SignallingError::Phase)?;
        pending.require_setup()?;
        let expected = pending.expected.ok_or(SignallingError::Mapping)?;
        if self.bearer != SignallingBearer::ControlAm
            || self.key != procedure.key()
            || expected.ue != self.key
            || procedure.activation().is_some()
            || pending.transaction != procedure.transaction()
            || procedure.receipt() != Some(pending.receipt)
            || !pending.protection.matches(self.protection)
        {
            return Err(SignallingError::Mapping);
        }
        if matches!(
            self.message.value.message,
            ULDCCHMessageType::radioBearerSetupComplete(_)
        ) && self.time.provenance()
            != umts_rlc::acknowledged::ReceiveProvenance::Path(expected.configuration)
        {
            return Err(SignallingError::Mapping);
        }
        match security
            .control
            .transmission(self.key, pending.receipt)
            .map_err(SignallingError::Am)?
        {
            SduTransmission::InFlight {
                delimited: true, ..
            }
            | SduTransmission::Acknowledged => (),
            _ => return Err(SignallingError::Phase),
        }
        let configuration = procedure.configuration().clone();
        let hsdpa = procedure.initial_hsdpa().cloned();
        let prior_starts = pending.prior_starts;
        let domains = counters::setup_domains(procedure.rabs());
        let result = procedure
            .response_with(self.key, self.time, self.message, |counters| {
                security.admit_starts_with(
                    self.key,
                    self.protection,
                    counters::setup_starts(counters, domains),
                    |ciphers| {
                        admit(
                            AcceptedUserSetup {
                                key: self.key,
                                configuration,
                                hsdpa,
                                time: self.time,
                                counters,
                                prior_starts,
                            },
                            ciphers,
                        )
                    },
                )
            })
            .map_err(|error| match error {
                umts_rrc::dedicated::SetupResponseError::Procedure(error) => {
                    SignallingError::Dedicated(error)
                }
                umts_rrc::dedicated::SetupResponseError::Admission(error) => error,
            })?;
        security
            .change
            .as_mut()
            .expect("retained setup admission")
            .completion_admitted = procedure.phase() == umts_rrc::dedicated::SetupPhase::Complete;
        Ok(result)
    }
    pub fn key(&self) -> UeKey {
        self.key
    }
    #[cfg(test)]
    pub fn bearer(&self) -> SignallingBearer {
        self.bearer
    }
    pub fn time(&self) -> ReceiveTime {
        self.time
    }
    pub fn message(&self) -> &'a DecodedUplinkDedicated {
        self.message
    }
    pub fn unsupported(&self) -> SignallingError {
        SignallingError::UnsupportedControl((&self.message.value.message).into())
    }
    #[cfg(test)]
    pub fn protection(&self) -> ControlProtection {
        self.protection
    }
    #[cfg(test)]
    pub fn accepted_user_setup(
        &self,
        procedure: &RadioBearerSetupProcedure,
        security: &ControlSecurity<'_>,
    ) -> Result<AcceptedUserSetup, SignallingError> {
        let pending = security.change.as_ref().ok_or(SignallingError::Phase)?;
        pending.require_setup()?;
        let expected = pending.expected.ok_or(SignallingError::Mapping)?;
        let ULDCCHMessageType::radioBearerSetupComplete(complete) = &self.message.value.message
        else {
            return Err(SignallingError::Mapping);
        };
        if self.bearer != SignallingBearer::ControlAm
            || self.key != procedure.key()
            || expected.ue != self.key
            || self.time.provenance()
                != umts_rlc::acknowledged::ReceiveProvenance::Path(expected.configuration)
            || !pending.completion_admitted
            || pending.transaction != procedure.transaction()
            || procedure.receipt() != Some(pending.receipt)
            || procedure.phase() != umts_rrc::dedicated::SetupPhase::Complete
            || procedure.completion() != Some(complete)
            || !pending.protection.matches(self.protection)
        {
            return Err(SignallingError::Phase);
        }
        // Initial Now setup and scheduled existing-DCH changes have distinct counter constructions.
        if procedure.activation().is_some() {
            return Err(SignallingError::ConfigurationFailure);
        }
        match security
            .control
            .transmission(self.key, pending.receipt)
            .map_err(SignallingError::Am)?
        {
            SduTransmission::InFlight {
                delimited: true, ..
            }
            | SduTransmission::Acknowledged => (),
            _ => return Err(SignallingError::Phase),
        }
        Ok(AcceptedUserSetup {
            key: self.key,
            configuration: procedure.configuration().clone(),
            hsdpa: procedure.initial_hsdpa().cloned(),
            time: self.time,
            counters: procedure
                .completion_counters()
                .ok_or(SignallingError::Phase)?,
            prior_starts: pending.prior_starts,
        })
    }
    pub fn bearer_setup_response(
        &self,
        procedure: &mut RadioBearerSetupProcedure,
        security: &mut ControlSecurity<'_>,
    ) -> Result<SetupResponse, SignallingError> {
        // TS 25.331 8.2.2.4 transmits setup completion on uplink DCCH using AM RLC.
        if self.bearer != SignallingBearer::ControlAm {
            return Err(SignallingError::Mapping);
        }
        let pending = security.change.as_ref().ok_or(SignallingError::Phase)?;
        pending.require_setup()?;
        if pending.transaction != procedure.transaction()
            || procedure.receipt() != Some(pending.receipt)
            || !pending.protection.matches(self.protection)
        {
            return Err(SignallingError::Dedicated(DedicatedError::Transaction));
        }
        // TS 25.331 8.2.2.4 sends successful CELL_DCH setup completion using the new configuration.
        if matches!(
            self.message.value.message,
            ULDCCHMessageType::radioBearerSetupComplete(_)
        ) && pending.expected.is_some_and(|expected| {
            expected.ue != self.key
                || self.time.provenance()
                    != umts_rlc::acknowledged::ReceiveProvenance::Path(expected.configuration)
        }) {
            return Err(SignallingError::Mapping);
        }
        match security
            .control
            .transmission(self.key, pending.receipt)
            .map_err(SignallingError::Am)?
        {
            SduTransmission::InFlight {
                delimited: true, ..
            }
            | SduTransmission::Acknowledged => (),
            _ => return Err(SignallingError::Phase),
        }
        let domains = counters::setup_domains(procedure.rabs());
        let (response, _) = procedure
            .response_with(self.key, self.time, self.message, |counters| {
                security.admit_starts_with(
                    self.key,
                    self.protection,
                    counters::setup_starts(counters, domains),
                    |_| Ok(()),
                )
            })
            .map_err(|error| match error {
                umts_rrc::dedicated::SetupResponseError::Procedure(error) => {
                    SignallingError::Dedicated(error)
                }
                umts_rrc::dedicated::SetupResponseError::Admission(error) => error,
            })?;
        security
            .change
            .as_mut()
            .expect("retained setup admission")
            .completion_admitted = procedure.phase() == umts_rrc::dedicated::SetupPhase::Complete;
        Ok(response)
    }
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct ControlSecurity<'a> {
    router: &'a mut umts_rrc::nas::NasRouter,
    route: Option<crate::dedicated_rx::ReceiveIdentity>,
    established: &'a ConnectionSecurity,
    ciphers: &'a mut BearerCiphers,
    control: &'a CommonAmBearer,
    change: &'a mut Option<BearerChangeAdmission>,
    cs: Option<KeyOrigin>,
    ps: Option<KeyOrigin>,
}
pub(super) struct BearerChangeAdmission {
    protection: PublicationProtection,
    prior_starts: umts_rrc::establishment::DomainStarts,
    transaction: TransactionId,
    receipt: SduReceipt,
    completion_admitted: bool,
    expected: Option<crate::dedicated_rx::ReceiveIdentity>,
    kind: BearerChangeKind,
}
enum BearerChangeKind {
    Setup,
    AdditionalSetup(additional::AdditionalAdmission),
    Release(release::ReleaseAdmission),
    Hspa(hspa::HspaAdmission),
}
impl BearerChangeAdmission {
    pub(super) fn delivery_receipt(&self) -> SduReceipt {
        self.receipt
    }

    fn require_setup(&self) -> Result<(), SignallingError> {
        if matches!(self.kind, BearerChangeKind::Setup) {
            Ok(())
        } else {
            Err(SignallingError::Phase)
        }
    }
}
impl ControlSecurity<'_> {
    #[cfg(test)]
    pub fn starts(&self) -> umts_rrc::establishment::DomainStarts {
        self.router.starts()
    }
    pub fn user_cipher(
        &self,
        domain: CnDomain,
    ) -> Result<Option<crate::connection_security::UserCipher>, SignallingError> {
        self.established
            .user_cipher(domain.into())
            .map_err(|_| SignallingError::ConfigurationFailure)
    }
    #[cfg(test)]
    pub fn active_origin(&self, domain: CnDomain) -> Option<KeyOrigin> {
        match domain {
            CnDomain::Cs => self.cs,
            CnDomain::Ps => self.ps,
        }
    }
}
impl<'a> ControlSecurity<'a> {
    pub(super) fn new(
        security: &'a ConnectionSecurity,
        ciphers: &'a mut BearerCiphers,
        router: &'a mut umts_rrc::nas::NasRouter,
        control: &'a CommonAmBearer,
        change: &'a mut Option<BearerChangeAdmission>,
        route: Option<crate::dedicated_rx::ReceiveIdentity>,
    ) -> Self {
        Self {
            router,
            route,
            established: security,
            ciphers,
            control,
            change,
            cs: security.active_origin(umts_iuh::Domain::Cs),
            ps: security.active_origin(umts_iuh::Domain::Ps),
        }
    }
}
pub(super) fn external(message: &ULDCCHMessageType) -> bool {
    !matches!(
        message,
        ULDCCHMessageType::rrcConnectionSetupComplete(_)
            | ULDCCHMessageType::securityModeComplete(_)
            | ULDCCHMessageType::securityModeFailure(_)
            | ULDCCHMessageType::ueCapabilityInformation(_)
            | ULDCCHMessageType::initialDirectTransfer(_)
            | ULDCCHMessageType::uplinkDirectTransfer(_)
    )
}

impl CommonSignalling {
    pub fn dedicated_route(&self, key: UeKey) -> Option<crate::dedicated_rx::ReceiveIdentity> {
        self.registry.get(key)?.dedicated_route
    }
    pub(crate) fn owns_frame_pool(&self, pool: &crate::dedicated_bearers::FramePool) -> bool {
        let identity = pool.identity();
        self.registry
            .get(identity.ue)
            .is_some_and(|context| context.frame_pools.contains(&Some(identity.configuration)))
            && pool.admitted_by(&self.ledger)
    }
    pub(crate) fn select_dedicated_route(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(identity.ue)
            .ok_or(SignallingError::Origin)?;
        if context.bearer_change.as_ref().is_none_or(|setup| {
            !matches!(setup.kind, BearerChangeKind::Setup)
                || setup.expected != Some(identity)
                || !setup.completion_admitted
        }) {
            return Err(SignallingError::Phase);
        }
        if context
            .dedicated_route
            .is_some_and(|previous| previous != identity)
        {
            return Err(SignallingError::Phase);
        }
        context.dedicated_route = Some(identity);
        Ok(())
    }
    pub(crate) fn require_dedicated_recovery(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(identity.ue)
            .ok_or(SignallingError::Origin)?;
        if context.dedicated_route != Some(identity) {
            return Err(SignallingError::Phase);
        }
        context.protection.require_recovery(None);
        self.refresh_control_status(identity.ue);
        Ok(())
    }
    pub(crate) fn retire_failed_radio(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Result<bool, SignallingError> {
        let Some(context) = self.registry.get(identity.ue) else {
            return Ok(false);
        };
        let owns_failure = context.dedicated_route == Some(identity)
            || context
                .bearer_change
                .as_ref()
                .is_some_and(|change| change.expected == Some(identity));
        if !owns_failure || !context.protection.blocks_status() {
            return Err(SignallingError::Phase);
        }
        self.retire(identity.ue)?;
        Ok(true)
    }
    pub fn allocate_frame_pool(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
        plan: std::sync::Arc<crate::rel99_plan::CompiledRel99>,
        port: &crate::dedicated_tx::DedicatedTransmitPort,
        limits: crate::dedicated_bearers::FramePoolLimits,
    ) -> Result<crate::dedicated_bearers::FramePool, SignallingError> {
        let context = self
            .registry
            .get(identity.ue)
            .ok_or(SignallingError::Origin)?;
        if context.setup.phase() != EstablishmentPhase::Established {
            return Err(SignallingError::Phase);
        }
        if context.frame_pools.contains(&Some(identity.configuration)) {
            return Err(SignallingError::Phase);
        }
        let slot = context
            .frame_pools
            .iter()
            .position(Option::is_none)
            .ok_or(SignallingError::Capacity)?;
        let pool = crate::dedicated_bearers::FramePool::new(
            identity,
            plan,
            port,
            limits,
            &mut self.ledger,
        )
        .map_err(SignallingError::FramePool)?;
        self.registry
            .get_mut(identity.ue)
            .expect("admitted pool owner")
            .frame_pools[slot] = Some(identity.configuration);
        Ok(pool)
    }
    pub fn retire_frame_pool(
        &mut self,
        pool: crate::dedicated_bearers::FramePool,
    ) -> Result<(), crate::dedicated_bearers::RejectedFramePool> {
        let identity = pool.identity();
        if let Some(context) = self.registry.get_mut(identity.ue) {
            context.control.release_submitted_status_emission();
            context.nas_bearer.release_submitted_status_emission();
        }
        pool.retire(&mut self.ledger)?;
        if let Some(context) = self.registry.get_mut(identity.ue)
            && let Some(slot) = context
                .frame_pools
                .iter()
                .position(|configuration| *configuration == Some(identity.configuration))
        {
            context.frame_pools[slot] = None;
        }
        Ok(())
    }
    pub fn signalling_endpoints(
        &self,
        key: UeKey,
    ) -> Result<Vec<crate::dedicated_bearers::BearerEndpoint>, SignallingError> {
        use crate::dedicated_bearers::{BearerEndpoint, BearerId, SignallingRoute};
        use umts_security::counters::CipherMode;
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        if context.setup.phase() != EstablishmentPhase::Established {
            return Err(SignallingError::Phase);
        }
        let (receive_bytes, transmit_minimum, transmit_maximum, maximum_sdu_bytes) =
            context.um.payload_limits();
        let mut endpoints = Vec::with_capacity(3);
        endpoints.push(
            BearerEndpoint::signalling(
                key,
                BearerId::new(SignallingBearer::Unacknowledged.identity()).expect("SRB1"),
                SignallingRoute {
                    mode: CipherMode::Unacknowledged,
                    receive_bytes,
                    transmit_minimum,
                    transmit_maximum,
                },
                maximum_sdu_bytes,
            )
            .map_err(|_| SignallingError::ConfigurationFailure)?,
        );
        for (bearer, entity) in [
            (SignallingBearer::ControlAm, &context.control),
            (SignallingBearer::NasAm, &context.nas_bearer),
        ] {
            let configuration = entity.configuration();
            endpoints.push(
                BearerEndpoint::signalling(
                    key,
                    BearerId::new(bearer.identity()).expect("AM SRB"),
                    SignallingRoute {
                        mode: CipherMode::Acknowledged,
                        receive_bytes: configuration.receive_pdu_bytes.get(),
                        transmit_minimum: configuration.transmit_pdu_bytes.get(),
                        transmit_maximum: configuration.transmit_pdu_bytes.get(),
                    },
                    self.limits.maximum_sdu_bytes.get(),
                )
                .map_err(|_| SignallingError::ConfigurationFailure)?,
            );
        }
        Ok(endpoints)
    }
    pub fn retained_signalling(&self, key: UeKey) -> Result<Vec<RetainedBearer>, SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        if context.setup.phase() != EstablishmentPhase::Established {
            return Err(SignallingError::Phase);
        }
        let ie::RLCInfoChoice::rlc_Info(info) = SignallingProfile.acknowledged_rlc() else {
            unreachable!("normal signaling profile");
        };
        let Some(ie::ULRLCMode::ul_AM_RLC_Mode(uplink)) = info.ul_rlc_mode else {
            unreachable!("normal uplink AM profile");
        };
        let Some(ie::DLRLCMode::dl_AM_RLC_Mode(downlink)) = info.dl_rlc_mode else {
            unreachable!("normal downlink AM profile");
        };
        let mut retained = vec![RetainedBearer {
            id: 1,
            domain: None,
            rlc: Rel99Rlc::Unacknowledged,
            uplink_pdu_bits: 0,
            downlink_pdu_bits: 0,
        }];
        for (id, entity) in [(2, &context.control), (3, &context.nas_bearer)] {
            let configuration = entity.configuration();
            let expected = SignallingProfile.network_am_configuration(
                configuration.transmit_pdu_bytes,
                configuration.receive_pdu_bytes,
                configuration.initial_tx_hfn,
                configuration.initial_rx_hfn,
            );
            if configuration != expected {
                return Err(SignallingError::ConfigurationFailure);
            }
            let bits = |bytes: NonZeroUsize| {
                u16::try_from(bytes.get() * BITS_PER_BYTE).map_err(|_| SignallingError::Capacity)
            };
            retained.push(RetainedBearer {
                id,
                domain: None,
                rlc: Rel99Rlc::Acknowledged {
                    uplink: uplink.clone(),
                    downlink: downlink.clone(),
                },
                uplink_pdu_bits: bits(configuration.receive_pdu_bytes)?,
                downlink_pdu_bits: bits(configuration.transmit_pdu_bytes)?,
            });
        }
        Ok(retained)
    }
    pub fn next_bearer_setup_transaction(
        &self,
        key: UeKey,
    ) -> Result<TransactionId, SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        Ok(context
            .transactions
            .next(transactions::MessageType::RadioBearerSetup))
    }
    /// The coordinator must prepare the real new links before publication and retain this receipt through recovery.
    #[cfg(test)]
    pub fn publish_bearer_setup(
        &mut self,
        procedure: &mut RadioBearerSetupProcedure,
        now: ChipInstant,
    ) -> Result<SduReceipt, SignallingError> {
        self.publish_bearer_setup_bound(procedure, None, now)
    }
    pub fn publish_bearer_setup_on(
        &mut self,
        procedure: &mut RadioBearerSetupProcedure,
        expected: crate::dedicated_rx::ReceiveIdentity,
        now: ChipInstant,
    ) -> Result<SduReceipt, SignallingError> {
        if expected.ue != procedure.key() {
            return Err(SignallingError::Origin);
        }
        self.publish_bearer_setup_bound(procedure, Some(expected), now)
    }
    /// Diagnostic flags do not reserve queue capacity or authorize publication.
    pub fn bearer_setup_readiness(&self, key: UeKey) -> Option<SetupPublicationReadiness> {
        let context = self.registry.get(key)?;
        Some(SetupPublicationReadiness {
            ingress_pending: self.ingress.pending(key, SignallingBearer::ControlAm),
            bearer_change_pending: context.bearer_change.is_some()
                || context.idle_release.is_some(),
            protection_blocks_reconfiguration: context
                .protection
                .bearer_reconfiguration_block()
                .is_some(),
            protection_block: context.protection.bearer_reconfiguration_block(),
            integrity_pending: context.protection.integrity.pending_origin().is_some(),
            cs_cipher_pending: context
                .security
                .pending_origin(umts_iuh::Domain::Cs)
                .is_some(),
            ps_cipher_pending: context
                .security
                .pending_origin(umts_iuh::Domain::Ps)
                .is_some(),
            capabilities_pending: context.capabilities.phase()
                == capabilities::ExchangePhase::AwaitingInformation,
        })
    }
    pub(crate) fn validate_retained_configuration(
        &self,
        key: UeKey,
        configuration: &umts_rrc::dedicated::Rel99Configuration,
    ) -> Result<(), SignallingError> {
        let retained = self.retained_signalling(key)?;
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        let (receive_bytes, transmit_minimum, transmit_maximum, _) = context.um.payload_limits();
        for previous in &retained {
            let target = configuration
                .bearers()
                .iter()
                .find(|bearer| bearer.id == previous.id)
                .ok_or(SignallingError::Mapping)?;
            if target.rlc != previous.rlc || target.domain != previous.domain {
                return Err(SignallingError::ConfigurationFailure);
            }
            if previous.id == 1 {
                for (direction, minimum, maximum) in [
                    (DchDirection::Uplink, 1, receive_bytes),
                    (DchDirection::Downlink, transmit_minimum, transmit_maximum),
                ] {
                    let tfcs = match direction {
                        DchDirection::Uplink => configuration.uplink(),
                        DchDirection::Downlink => configuration.downlink(),
                    };
                    for mapping in target.mappings(direction) {
                        let transport = tfcs
                            .transport(mapping.transport)
                            .ok_or(SignallingError::Mapping)?;
                        if transport.formats().any(|format| {
                            format.blocks != 0
                                && (!format.rlc_bits.is_multiple_of(BITS_PER_BYTE as u16)
                                    || !(minimum..=maximum)
                                        .contains(&(usize::from(format.rlc_bits) / BITS_PER_BYTE)))
                        }) {
                            return Err(SignallingError::ConfigurationFailure);
                        }
                    }
                }
                continue;
            }
            for (direction, size) in [
                (DchDirection::Uplink, previous.uplink_pdu_bits),
                (DchDirection::Downlink, previous.downlink_pdu_bits),
            ] {
                let tfcs = match direction {
                    DchDirection::Uplink => configuration.uplink(),
                    DchDirection::Downlink => configuration.downlink(),
                };
                for mapping in target.mappings(direction) {
                    let transport = tfcs
                        .transports()
                        .iter()
                        .find(|transport| transport.id == mapping.transport)
                        .ok_or(SignallingError::Mapping)?;
                    if transport.formats().any(|format| {
                        format.blocks != 0 && format.rlc_bits != 0 && format.rlc_bits != size
                    }) {
                        return Err(SignallingError::ConfigurationFailure);
                    }
                }
            }
        }
        Ok(())
    }
    fn publish_bearer_setup_bound(
        &mut self,
        procedure: &mut RadioBearerSetupProcedure,
        expected: Option<crate::dedicated_rx::ReceiveIdentity>,
        now: ChipInstant,
    ) -> Result<SduReceipt, SignallingError> {
        let key = procedure.key();
        self.validate_retained_configuration(key, procedure.configuration())?;
        if self.ingress.pending(key, SignallingBearer::ControlAm) {
            return Err(SignallingError::Capacity);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.bearer_change.is_some() {
            return Err(SignallingError::Capacity);
        }
        let prior_starts = context
            .router
            .as_ref()
            .ok_or(SignallingError::Phase)?
            .starts();
        let transaction = context
            .transactions
            .next(transactions::MessageType::RadioBearerSetup);
        if transaction != procedure.transaction() {
            return Err(SignallingError::Dedicated(DedicatedError::Transaction));
        }
        if context.protection.bearer_reconfiguration_block().is_some()
            || context.protection.integrity.pending_origin().is_some()
            || [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
                .into_iter()
                .any(|domain| context.security.pending_origin(domain).is_some())
            || context.capabilities.phase() == capabilities::ExchangePhase::AwaitingInformation
        {
            return Err(SignallingError::Capacity);
        }
        let protection = context
            .protection
            .integrity
            .downlink_origin(key, SignallingBearer::ControlAm.identity())
            .map(PublicationProtection::from)
            .map_err(SignallingError::Integrity)?;
        let control = &mut context.control;
        procedure
            .publish_with(now, |encoded| {
                let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                    .map_err(SignallingError::Codec)?;
                if !matches!(
                    decoded.value.message,
                    DLDCCHMessageType::radioBearerSetup(_)
                ) || decoded.value.integrity_check_info.is_some()
                {
                    return Err(SignallingError::Message);
                }
                context
                    .protection
                    .integrity
                    .publish_with(
                        key,
                        SignallingBearer::ControlAm.identity(),
                        &decoded.value,
                        |protected| {
                            control
                                .enqueue_tracked(
                                    key,
                                    Bits::new(&protected.bytes, protected.bit_length)
                                        .map_err(|_| SignallingError::Message)?,
                                )
                                .map_err(am_admission_error)
                        },
                    )
                    .map_err(|error| protection::schedule_error(error, |error| error))
            })
            .map_err(|error| match error {
                SetupPublicationError::Procedure(error) => SignallingError::Dedicated(error),
                SetupPublicationError::Publication(error) => error,
            })?;
        context
            .transactions
            .admitted(transactions::MessageType::RadioBearerSetup, transaction);
        context
            .reports
            .describe_configuration(procedure.configuration());
        context.bearer_change = Some(BearerChangeAdmission {
            protection,
            prior_starts,
            transaction,
            receipt: procedure.receipt().expect("admitted setup receipt"),
            completion_admitted: false,
            expected,
            kind: BearerChangeKind::Setup,
        });
        self.refresh_control_status(key);
        Ok(procedure.receipt().expect("admitted setup receipt"))
    }
    pub fn bearer_setup_transmission(
        &self,
        key: UeKey,
        receipt: SduReceipt,
    ) -> Result<SduTransmission, SignallingError> {
        self.registry
            .get(key)
            .ok_or(SignallingError::Origin)?
            .control
            .transmission(key, receipt)
            .map_err(SignallingError::Am)
    }
    pub fn release_bearer_setup_receipt(
        &mut self,
        key: UeKey,
        receipt: SduReceipt,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        context
            .bearer_change
            .as_ref()
            .ok_or(SignallingError::Phase)?
            .require_setup()?;
        if context
            .bearer_change
            .as_ref()
            .is_none_or(|setup| setup.receipt != receipt || !setup.completion_admitted)
        {
            return Err(SignallingError::Phase);
        }
        if context
            .control
            .transmission(key, receipt)
            .map_err(SignallingError::Am)?
            != SduTransmission::Acknowledged
        {
            return Err(SignallingError::Capacity);
        }
        context
            .control
            .release_receipt(key, receipt)
            .map_err(SignallingError::Am)?;
        context.bearer_change = None;
        Ok(())
    }
    pub fn require_bearer_setup_recovery(
        &mut self,
        procedure: &RadioBearerSetupProcedure,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(procedure.key())
            .ok_or(SignallingError::Origin)?;
        let admitted = context
            .bearer_change
            .as_ref()
            .ok_or(SignallingError::Phase)?;
        admitted.require_setup()?;
        if procedure.receipt() != Some(admitted.receipt)
            || procedure.transaction() != admitted.transaction
        {
            return Err(SignallingError::Dedicated(DedicatedError::Transaction));
        }
        if !matches!(
            procedure.phase(),
            umts_rrc::dedicated::SetupPhase::FailureReported
                | umts_rrc::dedicated::SetupPhase::RecoveryRequired
        ) {
            return Err(SignallingError::Phase);
        }
        // TS 25.331 8.2.2.10: the failure cause is what the UE could not apply. The new
        // configuration is deleted and the core is told through the failed RAB list.
        if let Some(failure) = procedure.failure() {
            log_bearer_failure(
                "RADIO BEARER SETUP FAILURE",
                procedure.key(),
                &failure.failure_cause,
            );
        }
        context.protection.require_recovery(None);
        self.refresh_control_status(procedure.key());
        Ok(())
    }
}

pub(super) fn log_bearer_failure(
    label: &str,
    key: UeKey,
    cause: &umts_rrc_schema::information_elements::FailureCauseWithProtErr,
) {
    tracing::info!("{label}: ue={key:?} cause={cause:?}");
}
