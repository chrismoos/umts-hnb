// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use std::sync::Arc;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::{
    dedicated::{
        ActivationMessage, ActivationProcedure, ReconfigurationCounters, Rel99Configuration,
    },
    hsdpa::{
        ActivationPhase, ActivationResponse, InitialActivation, PublicationError, ResponseError,
    },
    hsupa::{DchRestoration, UplinkActivation},
};

mod sealed {
    pub trait Configuration {}
    impl Configuration for umts_rrc::hsdpa::InitialActivation {}
    impl Configuration for umts_rrc::hsupa::UplinkActivation {}
    impl Configuration for umts_rrc::hsupa::DchRestoration {}
}

pub enum RetainedHspaConfiguration {
    Downlink(Arc<InitialActivation>),
    Uplink(Arc<UplinkActivation>),
    RestoredDch(Arc<DchRestoration>),
}

pub trait HspaConfiguration: ActivationMessage + sealed::Configuration {
    fn dedicated(&self) -> &Arc<Rel99Configuration>;
    fn retain(configuration: Arc<Self>) -> RetainedHspaConfiguration;
    fn matches(configuration: &Arc<Self>, retained: &RetainedHspaConfiguration) -> bool;
}

impl HspaConfiguration for InitialActivation {
    fn dedicated(&self) -> &Arc<Rel99Configuration> {
        self.dedicated()
    }
    fn retain(configuration: Arc<Self>) -> RetainedHspaConfiguration {
        RetainedHspaConfiguration::Downlink(configuration)
    }
    fn matches(configuration: &Arc<Self>, retained: &RetainedHspaConfiguration) -> bool {
        matches!(retained, RetainedHspaConfiguration::Downlink(other) if Arc::ptr_eq(configuration, other))
    }
}

impl HspaConfiguration for UplinkActivation {
    fn dedicated(&self) -> &Arc<Rel99Configuration> {
        self.dedicated()
    }
    fn retain(configuration: Arc<Self>) -> RetainedHspaConfiguration {
        RetainedHspaConfiguration::Uplink(configuration)
    }
    fn matches(configuration: &Arc<Self>, retained: &RetainedHspaConfiguration) -> bool {
        matches!(retained, RetainedHspaConfiguration::Uplink(other) if Arc::ptr_eq(configuration, other))
    }
}

impl HspaConfiguration for DchRestoration {
    fn dedicated(&self) -> &Arc<Rel99Configuration> {
        self.dedicated()
    }

    fn retain(configuration: Arc<Self>) -> RetainedHspaConfiguration {
        RetainedHspaConfiguration::RestoredDch(configuration)
    }

    fn matches(configuration: &Arc<Self>, retained: &RetainedHspaConfiguration) -> bool {
        matches!(retained, RetainedHspaConfiguration::RestoredDch(other) if Arc::ptr_eq(configuration, other))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HspaCommitError<E> {
    Signalling(SignallingError),
    Commit(E),
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct AcceptedActivation<C: HspaConfiguration> {
    identity: ReceiveIdentity,
    configuration: Arc<C>,
    protection: PublicationProtection,
    transaction: TransactionId,
    receipt: SduReceipt,
    counters: ReconfigurationCounters,
    time: ReceiveTime,
}

pub type AcceptedHsdpaActivation = AcceptedActivation<InitialActivation>;
pub type AcceptedHsupaActivation = AcceptedActivation<UplinkActivation>;
pub type ActivationAdmission<C, T> = (ActivationResponse, Option<(AcceptedActivation<C>, T)>);

impl<C: HspaConfiguration> AcceptedActivation<C> {
    #[cfg(test)]
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    #[cfg(test)]
    pub fn configuration(&self) -> &Arc<C> {
        &self.configuration
    }
    #[cfg(test)]
    pub fn time(&self) -> ReceiveTime {
        self.time
    }
}

pub(super) struct HspaAdmission {
    identity: ReceiveIdentity,
    configuration: RetainedHspaConfiguration,
    committed: bool,
}

fn pending_for<'a, C: HspaConfiguration>(
    pending: &'a BearerChangeAdmission,
    procedure: &ActivationProcedure<C>,
) -> Result<&'a HspaAdmission, SignallingError> {
    let BearerChangeKind::Hspa(activation) = &pending.kind else {
        return Err(SignallingError::Phase);
    };
    if procedure.key() != activation.identity.ue
        || pending.expected != Some(activation.identity)
        || pending.transaction != procedure.transaction()
        || procedure.receipt() != Some(pending.receipt)
        || !C::matches(procedure.construction(), &activation.configuration)
    {
        return Err(SignallingError::Mapping);
    }
    Ok(activation)
}

fn accepted_matches<C: HspaConfiguration>(
    pending: &BearerChangeAdmission,
    accepted: &AcceptedActivation<C>,
) -> bool {
    let BearerChangeKind::Hspa(activation) = &pending.kind else {
        return false;
    };
    pending.completion_admitted
        && pending.expected == Some(accepted.identity)
        && pending.protection == accepted.protection
        && pending.transaction == accepted.transaction
        && pending.receipt == accepted.receipt
        && activation.identity == accepted.identity
        && C::matches(&accepted.configuration, &activation.configuration)
}

impl ControlSecurity<'_> {
    pub fn can_route_hspa_activation<C: HspaConfiguration>(
        &self,
        procedure: &ActivationProcedure<C>,
    ) -> bool {
        match self.change.as_ref() {
            Some(pending) => pending_for(pending, procedure).is_ok(),
            None => procedure.phase() == ActivationPhase::Complete,
        }
    }
}

impl ReceivedControl<'_> {
    pub fn hspa_activation_response_with<C: HspaConfiguration, T>(
        &self,
        procedure: &mut ActivationProcedure<C>,
        security: &mut ControlSecurity<'_>,
        admit: impl FnOnce(ReconfigurationCounters, &mut BearerCiphers) -> Result<T, SignallingError>,
    ) -> Result<ActivationAdmission<C, T>, SignallingError> {
        let pending = security.change.as_ref().ok_or(SignallingError::Phase)?;
        let activation = pending_for(pending, procedure)?;
        if self.bearer != SignallingBearer::ControlAm
            || self.key != activation.identity.ue
            || security.route != Some(activation.identity)
            || self.time.provenance() != ReceiveProvenance::Path(activation.identity.configuration)
            || !pending.protection.matches(self.protection)
        {
            return Err(SignallingError::Mapping);
        }
        if !matches!(
            security
                .control
                .transmission(self.key, pending.receipt)
                .map_err(SignallingError::Am)?,
            SduTransmission::InFlight {
                delimited: true,
                ..
            } | SduTransmission::Acknowledged
        ) {
            return Err(SignallingError::Phase);
        }
        let identity = activation.identity;
        let configuration = procedure.construction().clone();
        let protection = pending.protection;
        let transaction = pending.transaction;
        let receipt = pending.receipt;
        let result = procedure
            .response_with(self.key, self.time, self.message, |counters| {
                let admitted = security.admit_starts_with(
                    self.key,
                    self.protection,
                    counters.domain_starts(),
                    |ciphers| admit(counters, ciphers),
                )?;
                Ok((
                    AcceptedActivation {
                        identity,
                        configuration,
                        protection,
                        transaction,
                        receipt,
                        counters,
                        time: self.time,
                    },
                    admitted,
                ))
            })
            .map_err(|error| match error {
                ResponseError::Procedure(error) => SignallingError::Dedicated(error),
                ResponseError::Admission(error) => error,
            })?;
        security
            .change
            .as_mut()
            .expect("retained activation admission")
            .completion_admitted = procedure.phase() == ActivationPhase::Complete;
        Ok(result)
    }

    pub fn hspa_activation_duplicate<C: HspaConfiguration>(
        &self,
        procedure: &mut ActivationProcedure<C>,
        accepted: &AcceptedActivation<C>,
    ) -> Result<(), SignallingError> {
        if self.key != accepted.identity.ue
            || self.bearer != SignallingBearer::ControlAm
            || self.time.provenance() != ReceiveProvenance::Path(accepted.identity.configuration)
            || !accepted.protection.matches(self.protection)
            || procedure.key() != self.key
            || procedure.transaction() != accepted.transaction
            || procedure.receipt() != Some(accepted.receipt)
            || !Arc::ptr_eq(procedure.construction(), &accepted.configuration)
            || procedure.phase() != ActivationPhase::Complete
        {
            return Err(SignallingError::Mapping);
        }
        match procedure.response_with(self.key, self.time, self.message, |_| {
            Err::<(), _>(SignallingError::Phase)
        }) {
            Ok((ActivationResponse::Duplicate, None)) => Ok(()),
            _ => Err(SignallingError::Mapping),
        }
    }
}

impl CommonSignalling {
    pub fn next_hspa_activation_transaction(
        &self,
        key: UeKey,
    ) -> Result<TransactionId, SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        Ok(context
            .transactions
            .next(transactions::MessageType::RadioBearerReconfiguration))
    }

    /// The matching RX feedback owner, TX code lease and timed bearer cutover must be reserved first.
    pub fn publish_hspa_activation_on<C: HspaConfiguration>(
        &mut self,
        procedure: &mut ActivationProcedure<C>,
        identity: ReceiveIdentity,
        now: ChipInstant,
    ) -> Result<SduReceipt, SignallingError> {
        let key = procedure.key();
        if identity.ue != key {
            return Err(SignallingError::Origin);
        }
        self.validate_retained_configuration(key, procedure.construction().dedicated())?;
        if self.ingress.pending(key, SignallingBearer::ControlAm) {
            return Err(SignallingError::Capacity);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.dedicated_route != Some(identity) {
            return Err(SignallingError::Mapping);
        }
        if context.bearer_change.is_some()
            || context.dormancy.parking()
            || context.protection.bearer_reconfiguration_block().is_some()
            || context.protection.integrity.pending_origin().is_some()
            || [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
                .into_iter()
                .any(|domain| context.security.pending_origin(domain).is_some())
            || context.capabilities.phase() == capabilities::ExchangePhase::AwaitingInformation
        {
            return Err(SignallingError::Capacity);
        }
        let transaction = context
            .transactions
            .next(transactions::MessageType::RadioBearerReconfiguration);
        if procedure.transaction() != transaction {
            return Err(SignallingError::Dedicated(DedicatedError::Transaction));
        }
        let protection = context
            .protection
            .integrity
            .downlink_origin(key, SignallingBearer::ControlAm.identity())
            .map(PublicationProtection::from)
            .map_err(SignallingError::Integrity)?;
        let prior_starts = context
            .router
            .as_ref()
            .ok_or(SignallingError::Phase)?
            .starts();
        let control = &mut context.control;
        procedure
            .publish_with(now, |encoded| {
                let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                    .map_err(SignallingError::Codec)?;
                if !matches!(
                    decoded.value.message,
                    DLDCCHMessageType::radioBearerReconfiguration(_)
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
                PublicationError::Procedure(error) => SignallingError::Dedicated(error),
                PublicationError::Publication(error) => error,
            })?;
        let receipt = procedure.receipt().expect("admitted activation receipt");
        tracing::debug!(
            "HSPA reconfiguration encoding: identity={identity:?} construction={} at={now:?} bits={} bytes={:02x?}",
            std::any::type_name::<C>(),
            procedure.encoded().bit_length,
            procedure.encoded().bytes,
        );
        context.transactions.admitted(
            transactions::MessageType::RadioBearerReconfiguration,
            transaction,
        );
        context.bearer_change = Some(BearerChangeAdmission {
            protection,
            prior_starts,
            transaction,
            receipt,
            completion_admitted: false,
            expected: Some(identity),
            kind: BearerChangeKind::Hspa(HspaAdmission {
                identity,
                configuration: C::retain(procedure.construction().clone()),
                committed: false,
            }),
        });
        self.refresh_control_status(key);
        Ok(receipt)
    }

    pub fn commit_hspa_activation_with<C: HspaConfiguration, T, E>(
        &mut self,
        accepted: &AcceptedActivation<C>,
        commit: impl FnOnce(&mut BearerCiphers) -> Result<T, E>,
    ) -> Result<T, HspaCommitError<E>> {
        let fail = HspaCommitError::Signalling;
        let context = self
            .registry
            .get_mut(accepted.identity.ue)
            .ok_or_else(|| fail(SignallingError::Origin))?;
        let pending = context
            .bearer_change
            .as_mut()
            .ok_or_else(|| fail(SignallingError::Phase))?;
        if !accepted_matches(pending, accepted)
            || context.dedicated_route != Some(accepted.identity)
        {
            return Err(fail(SignallingError::Mapping));
        }
        let BearerChangeKind::Hspa(activation) = &mut pending.kind else {
            return Err(fail(SignallingError::Phase));
        };
        if activation.committed {
            return Err(fail(SignallingError::Phase));
        }
        let result = commit(&mut context.protection.ciphers).map_err(HspaCommitError::Commit)?;
        activation.committed = true;
        Ok(result)
    }

    pub fn release_hspa_activation_receipt<C: HspaConfiguration>(
        &mut self,
        accepted: &AcceptedActivation<C>,
    ) -> Result<(), SignallingError> {
        let key = accepted.identity.ue;
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let pending = context
            .bearer_change
            .as_ref()
            .ok_or(SignallingError::Phase)?;
        if !accepted_matches(pending, accepted)
            || context.dedicated_route != Some(accepted.identity)
            || !matches!(&pending.kind, BearerChangeKind::Hspa(activation) if activation.committed)
        {
            return Err(SignallingError::Mapping);
        }
        if context
            .control
            .transmission(key, accepted.receipt)
            .map_err(SignallingError::Am)?
            != SduTransmission::Acknowledged
        {
            return Err(SignallingError::Capacity);
        }
        context
            .control
            .release_receipt(key, accepted.receipt)
            .map_err(SignallingError::Am)?;
        context.bearer_change = None;
        self.refresh_control_status(key);
        Ok(())
    }

    /// Recover an established HS-DSCH route without a pending activation transaction.
    pub(crate) fn require_active_hsdpa_recovery(
        &mut self,
        identity: ReceiveIdentity,
        construction: &Arc<InitialActivation>,
        bank: &crate::dedicated_bearers::DedicatedBearers,
    ) -> Result<(), SignallingError> {
        if bank.identity() != identity {
            return Err(SignallingError::Mapping);
        }
        let route = bank
            .hsdpa_route(identity)
            .map_err(|_| SignallingError::Mapping)?;
        if !route.is_some_and(|route| Arc::ptr_eq(route, construction)) {
            return Err(SignallingError::Mapping);
        }
        let clock = bank
            .downlink_clock(identity)
            .map_err(|_| SignallingError::Mapping)?;
        for mapping in construction.mappings() {
            let bearer = crate::dedicated_bearers::BearerId::new(mapping.bearer)
                .map_err(|_| SignallingError::Mapping)?;
            if !bank
                .has_active_hsdpa_bearer(identity, bearer)
                .map_err(|_| SignallingError::Mapping)?
            {
                continue;
            }
            bank.hsdpa_grant(identity, bearer, clock.anchor.time)
                .map_err(|_| SignallingError::Mapping)?;
        }
        let context = self
            .registry
            .get_mut(identity.ue)
            .ok_or(SignallingError::Origin)?;
        if context.dedicated_route != Some(identity) {
            return Err(SignallingError::Mapping);
        }
        // An uncertain HS-SCCH order also makes the primary ACK codebook uncertain.
        // Recover this connection through the existing protected release path.
        context.protection.require_recovery(None);
        self.refresh_control_status(identity.ue);
        Ok(())
    }

    pub fn require_hspa_activation_recovery<C: HspaConfiguration>(
        &mut self,
        procedure: &ActivationProcedure<C>,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(procedure.key())
            .ok_or(SignallingError::Origin)?;
        let pending = context
            .bearer_change
            .as_ref()
            .ok_or(SignallingError::Phase)?;
        pending_for(pending, procedure)?;
        if !matches!(
            procedure.phase(),
            ActivationPhase::FailureReported | ActivationPhase::RecoveryRequired
        ) {
            return Err(SignallingError::Phase);
        }
        if let Some(failure) = procedure.failure() {
            super::log_bearer_failure(
                "RADIO BEARER RECONFIGURATION FAILURE",
                procedure.key(),
                &failure.failure_cause,
            );
        }
        context.protection.require_recovery(None);
        self.refresh_control_status(procedure.key());
        Ok(())
    }
}
