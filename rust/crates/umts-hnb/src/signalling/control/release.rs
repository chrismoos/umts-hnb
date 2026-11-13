// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use std::sync::Arc;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::dedicated::{
    PublicationError, RadioBearerReleaseProcedure, ReconfigurationPhase, ReconfigurationResponse,
    Rel99Configuration, ReleaseCounters, ResponseError, ScheduledActivation,
};

#[derive(Debug, PartialEq, Eq)]
pub enum BearerReleaseCommitError<E> {
    Signalling(SignallingError),
    Commit(E),
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct AcceptedBearerRelease {
    protection: PublicationProtection,
    key: UeKey,
    transaction: TransactionId,
    receipt: SduReceipt,
    previous_identity: ReceiveIdentity,
    identity: ReceiveIdentity,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    activation: ScheduledActivation,
    counters: ReleaseCounters,
    time: ReceiveTime,
}
impl AcceptedBearerRelease {
    pub fn key(&self) -> UeKey {
        self.key
    }
    pub fn previous_identity(&self) -> ReceiveIdentity {
        self.previous_identity
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub fn previous(&self) -> &Arc<Rel99Configuration> {
        &self.previous
    }
    pub fn configuration(&self) -> &Arc<Rel99Configuration> {
        &self.configuration
    }
    pub fn activation(&self) -> ScheduledActivation {
        self.activation
    }
    #[cfg(test)]
    pub fn time(&self) -> ReceiveTime {
        self.time
    }
}
pub(super) struct ReleaseAdmission {
    previous_identity: ReceiveIdentity,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    activation: ScheduledActivation,
}
impl ReceivedControl<'_> {
    pub(crate) fn bearer_release_duplicate(
        &self,
        procedure: &mut RadioBearerReleaseProcedure,
        accepted: &AcceptedBearerRelease,
    ) -> Result<(), SignallingError> {
        if self.key != accepted.key
            || procedure.key() != accepted.key
            || procedure.phase() != ReconfigurationPhase::Complete
            || procedure.transaction() != accepted.transaction
            || procedure.receipt() != Some(accepted.receipt)
            || procedure.activation() != accepted.activation
            || !Arc::ptr_eq(procedure.configuration(), &accepted.configuration)
            || !Arc::ptr_eq(procedure.previous(), &accepted.previous)
            || self.bearer != SignallingBearer::ControlAm
            || self.time.provenance() != ReceiveProvenance::Path(accepted.identity.configuration)
            || !accepted.protection.matches(self.protection)
        {
            return Err(SignallingError::Mapping);
        }
        let (response, _): (_, Option<()>) = procedure
            .response_with(self.key, self.time, self.message, |_| {
                Err(SignallingError::Phase)
            })
            .map_err(|error| match error {
                ResponseError::Procedure(error) => SignallingError::Dedicated(error),
                ResponseError::Admission(error) => error,
            })?;
        if response != ReconfigurationResponse::Duplicate {
            return Err(SignallingError::Phase);
        }
        Ok(())
    }
    /// Counter changes must be scheduled by the callback or rejected before it returns success.
    pub fn bearer_release_response_with<T>(
        &self,
        procedure: &mut RadioBearerReleaseProcedure,
        security: &mut ControlSecurity<'_>,
        admit: impl FnOnce(ReleaseCounters, &mut BearerCiphers) -> Result<T, SignallingError>,
    ) -> Result<(ReconfigurationResponse, Option<(AcceptedBearerRelease, T)>), SignallingError>
    {
        let pending = security.change.as_ref().ok_or(SignallingError::Phase)?;
        let BearerChangeKind::Release(release) = &pending.kind else {
            return Err(SignallingError::Phase);
        };
        let expected = pending.expected.ok_or(SignallingError::Mapping)?;
        if self.bearer != SignallingBearer::ControlAm
            || self.key != procedure.key()
            || expected.ue != self.key
            || release.previous_identity.ue != self.key
            || pending.transaction != procedure.transaction()
            || procedure.receipt() != Some(pending.receipt)
            || !Arc::ptr_eq(procedure.previous(), &release.previous)
            || !Arc::ptr_eq(procedure.configuration(), &release.configuration)
            || procedure.activation() != release.activation
            || !pending.protection.matches(self.protection)
        {
            return Err(SignallingError::Mapping);
        }
        // TS 25.331 8.2.2.4 requires ReleaseComplete on the new CELL_DCH configuration.
        if matches!(
            self.message.value.message,
            ULDCCHMessageType::radioBearerReleaseComplete(_)
        ) && self.time.provenance() != ReceiveProvenance::Path(expected.configuration)
        {
            return Err(SignallingError::Mapping);
        }
        if matches!(
            self.message.value.message,
            ULDCCHMessageType::radioBearerReleaseFailure(_)
        ) && self.time.provenance() != ReceiveProvenance::Path(expected.configuration)
            && self.time.provenance()
                != ReceiveProvenance::Path(release.previous_identity.configuration)
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
        let previous_identity = release.previous_identity;
        let previous = release.previous.clone();
        let configuration = release.configuration.clone();
        let activation = release.activation;
        let transaction = pending.transaction;
        let receipt = pending.receipt;
        let protection = pending.protection;
        let result = procedure
            .response_with(self.key, self.time, self.message, |counters| {
                let result = security.admit_starts_with(
                    self.key,
                    self.protection,
                    counters.domain_starts(),
                    |ciphers| admit(counters, ciphers),
                )?;
                Ok((
                    AcceptedBearerRelease {
                        protection,
                        key: self.key,
                        transaction,
                        receipt,
                        previous_identity,
                        identity: expected,
                        previous,
                        configuration,
                        activation,
                        counters,
                        time: self.time,
                    },
                    result,
                ))
            })
            .map_err(|error| match error {
                ResponseError::Procedure(error) => SignallingError::Dedicated(error),
                ResponseError::Admission(error) => error,
            })?;
        security
            .change
            .as_mut()
            .expect("retained release admission")
            .completion_admitted = procedure.phase() == ReconfigurationPhase::Complete;
        Ok(result)
    }
}
impl CommonSignalling {
    pub fn next_bearer_release_transaction(
        &self,
        key: UeKey,
    ) -> Result<TransactionId, SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        Ok(context
            .transactions
            .next(transactions::MessageType::RadioBearerRelease))
    }
    pub fn publish_bearer_release_on(
        &mut self,
        procedure: &mut RadioBearerReleaseProcedure,
        previous_identity: ReceiveIdentity,
        expected: ReceiveIdentity,
        now: ChipInstant,
    ) -> Result<SduReceipt, SignallingError> {
        let key = procedure.key();
        if previous_identity.ue != key
            || expected.ue != key
            || expected.configuration <= previous_identity.configuration
        {
            return Err(SignallingError::Origin);
        }
        self.validate_retained_configuration(key, procedure.configuration())?;
        if self.ingress.pending(key, SignallingBearer::ControlAm) {
            return Err(SignallingError::Capacity);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.dedicated_route != Some(previous_identity) {
            return Err(SignallingError::Mapping);
        }
        if context.bearer_change.is_some()
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
            .next(transactions::MessageType::RadioBearerRelease);
        if procedure.transaction() != transaction {
            return Err(SignallingError::Dedicated(DedicatedError::Transaction));
        }
        let protection = context
            .protection
            .integrity
            .downlink_origin(key, SignallingBearer::ControlAm.identity())
            .map(PublicationProtection::from)
            .map_err(SignallingError::Integrity)?;
        let control = &mut context.control;
        let prior_starts = context
            .router
            .as_ref()
            .ok_or(SignallingError::Phase)?
            .starts();
        procedure
            .publish_with(now, |encoded| {
                let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                    .map_err(SignallingError::Codec)?;
                if !matches!(
                    decoded.value.message,
                    DLDCCHMessageType::radioBearerRelease(_)
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
        let receipt = procedure.receipt().expect("admitted release receipt");
        context
            .transactions
            .admitted(transactions::MessageType::RadioBearerRelease, transaction);
        context
            .reports
            .describe_configuration(procedure.configuration());
        context.bearer_change = Some(BearerChangeAdmission {
            protection,
            prior_starts,
            transaction,
            receipt,
            completion_admitted: false,
            expected: Some(expected),
            kind: BearerChangeKind::Release(ReleaseAdmission {
                previous_identity,
                previous: procedure.previous().clone(),
                configuration: procedure.configuration().clone(),
                activation: procedure.activation(),
            }),
        });
        self.refresh_control_status(key);
        Ok(receipt)
    }
    pub fn release_bearer_release_receipt(
        &mut self,
        accepted: &AcceptedBearerRelease,
        receipt: SduReceipt,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(accepted.key)
            .ok_or(SignallingError::Origin)?;
        let pending = context
            .bearer_change
            .as_ref()
            .ok_or(SignallingError::Phase)?;
        let BearerChangeKind::Release(release) = &pending.kind else {
            return Err(SignallingError::Phase);
        };
        if !pending.completion_admitted
            || pending.protection != accepted.protection
            || pending.receipt != receipt
            || context.dedicated_route != Some(accepted.identity)
            || pending.receipt != accepted.receipt
            || pending.transaction != accepted.transaction
            || pending.expected != Some(accepted.identity)
            || release.previous_identity != accepted.previous_identity
            || !Arc::ptr_eq(&release.previous, &accepted.previous)
            || !Arc::ptr_eq(&release.configuration, &accepted.configuration)
            || release.activation != accepted.activation
        {
            return Err(SignallingError::Mapping);
        }
        if context
            .control
            .transmission(accepted.key, receipt)
            .map_err(SignallingError::Am)?
            != SduTransmission::Acknowledged
        {
            return Err(SignallingError::Capacity);
        }
        context
            .control
            .release_receipt(accepted.key, receipt)
            .map_err(SignallingError::Am)?;
        context.bearer_change = None;
        self.refresh_control_status(accepted.key);
        Ok(())
    }
    #[cfg(test)]
    pub fn require_bearer_release_recovery(
        &mut self,
        procedure: &RadioBearerReleaseProcedure,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(procedure.key())
            .ok_or(SignallingError::Origin)?;
        let pending = context
            .bearer_change
            .as_ref()
            .ok_or(SignallingError::Phase)?;
        let BearerChangeKind::Release(release) = &pending.kind else {
            return Err(SignallingError::Phase);
        };
        if procedure.receipt() != Some(pending.receipt)
            || procedure.transaction() != pending.transaction
            || !Arc::ptr_eq(procedure.previous(), &release.previous)
            || !Arc::ptr_eq(procedure.configuration(), &release.configuration)
            || procedure.activation() != release.activation
        {
            return Err(SignallingError::Mapping);
        }
        if !matches!(
            procedure.phase(),
            ReconfigurationPhase::FailureReported | ReconfigurationPhase::RecoveryRequired
        ) {
            return Err(SignallingError::Phase);
        }
        context.protection.require_recovery(None);
        self.refresh_control_status(procedure.key());
        Ok(())
    }
    pub(crate) fn commit_bearer_release_with<T, E>(
        &mut self,
        accepted: &AcceptedBearerRelease,
        commit: impl FnOnce(&mut BearerCiphers) -> Result<T, E>,
    ) -> Result<T, BearerReleaseCommitError<E>> {
        let fail = BearerReleaseCommitError::Signalling;
        let context = self
            .registry
            .get_mut(accepted.key)
            .ok_or_else(|| fail(SignallingError::Origin))?;
        let pending = context
            .bearer_change
            .as_ref()
            .ok_or_else(|| fail(SignallingError::Phase))?;
        let BearerChangeKind::Release(release) = &pending.kind else {
            return Err(fail(SignallingError::Phase));
        };
        if !pending.completion_admitted
            || pending.protection != accepted.protection
            || pending.expected != Some(accepted.identity)
            || pending.receipt != accepted.receipt
            || pending.transaction != accepted.transaction
            || release.previous_identity != accepted.previous_identity
            || !Arc::ptr_eq(&release.previous, &accepted.previous)
            || !Arc::ptr_eq(&release.configuration, &accepted.configuration)
            || release.activation != accepted.activation
            || context.dedicated_route != Some(accepted.previous_identity)
        {
            return Err(fail(SignallingError::Mapping));
        }
        let committed =
            commit(&mut context.protection.ciphers).map_err(BearerReleaseCommitError::Commit)?;
        context.dedicated_route = Some(accepted.identity);
        Ok(committed)
    }
}
