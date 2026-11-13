// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(crate) struct ControlAmForecast {
    pub wire_bits: usize,
    pub pdu_bytes: std::num::NonZeroUsize,
    pub am: umts_rlc::acknowledged::DeliveryForecast,
}
pub(crate) type AdditionalSetupForecast = ControlAmForecast;
use crate::dedicated_rx::ReceiveIdentity;
use std::sync::Arc;
use umts_rlc::acknowledged::ReceiveProvenance;
use umts_rrc::dedicated::{
    NewRab, Rel99Configuration, ScheduledActivation, SetupCounters, SetupPhase,
};

pub struct AcceptedAdditionalSetup {
    key: UeKey,
    previous_identity: ReceiveIdentity,
    identity: ReceiveIdentity,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    activation: ScheduledActivation,
    counters: SetupCounters,
    prior_starts: umts_rrc::establishment::DomainStarts,
    time: ReceiveTime,
    rabs: Arc<[NewRab]>,
    transaction: TransactionId,
    receipt: SduReceipt,
    protection: PublicationProtection,
}

impl AcceptedAdditionalSetup {
    pub fn prior_starts(&self) -> umts_rrc::establishment::DomainStarts {
        self.prior_starts
    }
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
    pub fn counters(&self) -> SetupCounters {
        self.counters
    }
    pub fn time(&self) -> ReceiveTime {
        self.time
    }
    pub fn rabs(&self) -> &[NewRab] {
        &self.rabs
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AdditionalSetupCommitError<E> {
    Signalling(SignallingError),
    Commit(E),
}

pub(super) struct AdditionalAdmission {
    previous_identity: ReceiveIdentity,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    activation: ScheduledActivation,
    rabs: Arc<[NewRab]>,
}

impl ReceivedControl<'_> {
    pub(crate) fn additional_setup_duplicate(
        &self,
        procedure: &mut RadioBearerSetupProcedure,
        accepted: &AcceptedAdditionalSetup,
    ) -> Result<(), SignallingError> {
        if self.key != accepted.key
            || procedure.key() != accepted.key
            || procedure.phase() != SetupPhase::Complete
            || procedure.transaction() != accepted.transaction
            || procedure.receipt() != Some(accepted.receipt)
            || procedure.activation() != Some(accepted.activation)
            || !Arc::ptr_eq(procedure.configuration(), &accepted.configuration)
            || procedure
                .previous()
                .is_none_or(|previous| !Arc::ptr_eq(previous, &accepted.previous))
            || procedure.rabs() != accepted.rabs.as_ref()
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
                umts_rrc::dedicated::SetupResponseError::Procedure(error) => {
                    SignallingError::Dedicated(error)
                }
                umts_rrc::dedicated::SetupResponseError::Admission(error) => error,
            })?;
        if response != SetupResponse::Duplicate {
            return Err(SignallingError::Phase);
        }
        Ok(())
    }
    pub fn additional_setup_response_with<T>(
        &self,
        procedure: &mut RadioBearerSetupProcedure,
        security: &mut ControlSecurity<'_>,
        admit: impl FnOnce(&AcceptedAdditionalSetup, &mut BearerCiphers) -> Result<T, SignallingError>,
    ) -> Result<(SetupResponse, Option<(AcceptedAdditionalSetup, T)>), SignallingError> {
        let pending = security.change.as_ref().ok_or(SignallingError::Phase)?;
        let BearerChangeKind::AdditionalSetup(additional) = &pending.kind else {
            return Err(SignallingError::Phase);
        };
        let identity = pending.expected.ok_or(SignallingError::Mapping)?;
        if self.key != procedure.key()
            || identity.ue != self.key
            || additional.previous_identity.ue != self.key
            || self.bearer != SignallingBearer::ControlAm
            || pending.transaction != procedure.transaction()
            || procedure.receipt() != Some(pending.receipt)
            || procedure
                .previous()
                .is_none_or(|previous| !Arc::ptr_eq(previous, &additional.previous))
            || !Arc::ptr_eq(procedure.configuration(), &additional.configuration)
            || procedure.activation() != Some(additional.activation)
            || (security.route != Some(additional.previous_identity)
                && !(procedure.phase() == SetupPhase::Complete && security.route == Some(identity)))
            || !pending.protection.matches(self.protection)
        {
            return Err(SignallingError::Mapping);
        }
        let is_complete = matches!(
            self.message.value.message,
            ULDCCHMessageType::radioBearerSetupComplete(_)
        );
        // TS 25.331 8.2.2.4 sends successful CELL_DCH completion on the new configuration.
        if (is_complete
            && self.time.provenance() != ReceiveProvenance::Path(identity.configuration))
            || (!is_complete
                && self.time.provenance() != ReceiveProvenance::Path(identity.configuration)
                && self.time.provenance()
                    != ReceiveProvenance::Path(additional.previous_identity.configuration))
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
        let key = self.key;
        let previous_identity = additional.previous_identity;
        let previous = additional.previous.clone();
        let configuration = additional.configuration.clone();
        let activation = additional.activation;
        let rabs = additional.rabs.clone();
        let protection = pending.protection;
        let transaction = pending.transaction;
        let receipt = pending.receipt;
        let prior_starts = pending.prior_starts;
        let domains = counters::setup_domains(&rabs);
        let result = procedure
            .response_with(key, self.time, self.message, |counters| {
                let accepted = AcceptedAdditionalSetup {
                    key,
                    previous_identity,
                    identity,
                    previous,
                    configuration,
                    activation,
                    counters,
                    prior_starts,
                    time: self.time,
                    rabs,
                    transaction,
                    receipt,
                    protection,
                };
                let admitted = security.admit_starts_with(
                    key,
                    self.protection,
                    counters::setup_starts(counters, domains),
                    |ciphers| admit(&accepted, ciphers),
                )?;
                Ok((accepted, admitted))
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
            .expect("retained additional setup")
            .completion_admitted = procedure.phase() == SetupPhase::Complete;
        Ok(result)
    }
}

impl CommonSignalling {
    pub(crate) fn additional_setup_ready(&self, previous: ReceiveIdentity) -> bool {
        self.check_additional_setup(previous).is_ok()
            && self.registry.get(previous.ue).is_some_and(|context| {
                let window = context.control.window_positions();
                !context.control.pending_new_data()
                    && window.oldest_transmit == window.next_transmit
            })
    }

    fn check_additional_setup(&self, previous: ReceiveIdentity) -> Result<(), SignallingError> {
        self.check_control_am_forecast(previous)
    }

    fn check_control_am_forecast(&self, previous: ReceiveIdentity) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get(previous.ue)
            .ok_or(SignallingError::Origin)?;
        if context.dedicated_route != Some(previous) {
            return Err(SignallingError::Mapping);
        }
        if self
            .ingress
            .pending(previous.ue, SignallingBearer::ControlAm)
            || context.bearer_change.is_some()
            || context.idle_release.is_some()
            || context.protection.bearer_reconfiguration_block().is_some()
            || context.protection.integrity.pending_origin().is_some()
            || [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
                .into_iter()
                .any(|domain| context.security.pending_origin(domain).is_some())
            || context.capabilities.phase() == capabilities::ExchangePhase::AwaitingInformation
        {
            return Err(SignallingError::Capacity);
        }
        Ok(())
    }

    pub(crate) fn control_am_forecast(
        &self,
        previous: ReceiveIdentity,
        encoded_bits: usize,
    ) -> Result<ControlAmForecast, SignallingError> {
        self.check_control_am_forecast(previous)?;
        self.control_am_forecast_inner(previous, encoded_bits)
    }

    pub(crate) fn additional_setup_forecast(
        &self,
        previous: ReceiveIdentity,
        encoded_bits: usize,
    ) -> Result<AdditionalSetupForecast, SignallingError> {
        self.check_additional_setup(previous)?;
        self.control_am_forecast_inner(previous, encoded_bits)
    }

    fn control_am_forecast_inner(
        &self,
        previous: ReceiveIdentity,
        encoded_bits: usize,
    ) -> Result<ControlAmForecast, SignallingError> {
        let key = previous.ue;
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        let protection = context
            .protection
            .integrity
            .downlink_origin(key, SignallingBearer::ControlAm.identity())
            .map(PublicationProtection::from)
            .map_err(SignallingError::Integrity)?;
        let wire_bits = protection.wire_bits(encoded_bits)?;
        let am = context
            .control
            .forecast_delivery(key, wire_bits.div_ceil(umts_types::bits::BITS_PER_BYTE))
            .map_err(SignallingError::Am)?;
        Ok(ControlAmForecast {
            wire_bits,
            pdu_bytes: context.control.transmit_pdu_bytes(),
            am,
        })
    }

    pub fn publish_additional_setup(
        &mut self,
        procedure: &mut RadioBearerSetupProcedure,
        previous_identity: ReceiveIdentity,
        identity: ReceiveIdentity,
        now: ChipInstant,
    ) -> Result<SduReceipt, SignallingError> {
        let key = procedure.key();
        if previous_identity.ue != key
            || identity.ue != key
            || identity.configuration <= previous_identity.configuration
        {
            return Err(SignallingError::Origin);
        }
        let previous = procedure.previous().ok_or(SignallingError::Phase)?.clone();
        let activation = procedure.activation().ok_or(SignallingError::Phase)?;
        let rabs: Arc<[NewRab]> = procedure.rabs().into();
        if rabs.is_empty() {
            return Err(SignallingError::Mapping);
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
            .next(transactions::MessageType::RadioBearerSetup);
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
        let receipt = procedure
            .receipt()
            .expect("admitted additional setup receipt");
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
            receipt,
            completion_admitted: false,
            expected: Some(identity),
            kind: BearerChangeKind::AdditionalSetup(AdditionalAdmission {
                previous_identity,
                previous,
                configuration: procedure.configuration().clone(),
                activation,
                rabs,
            }),
        });
        self.refresh_control_status(key);
        Ok(receipt)
    }

    /// The callback must commit the actual accepted RX/TX mapping before the signaling route changes.
    pub fn commit_additional_setup_with<T, E>(
        &mut self,
        accepted: &AcceptedAdditionalSetup,
        commit: impl FnOnce(&mut BearerCiphers) -> Result<T, E>,
    ) -> Result<T, AdditionalSetupCommitError<E>> {
        let fail = AdditionalSetupCommitError::Signalling;
        let context = self
            .registry
            .get_mut(accepted.key)
            .ok_or_else(|| fail(SignallingError::Origin))?;
        let pending = context
            .bearer_change
            .as_ref()
            .ok_or_else(|| fail(SignallingError::Phase))?;
        validate_accepted(pending, accepted).map_err(fail)?;
        if context.dedicated_route != Some(accepted.previous_identity) {
            return Err(fail(SignallingError::Mapping));
        }
        let result =
            commit(&mut context.protection.ciphers).map_err(AdditionalSetupCommitError::Commit)?;
        context.dedicated_route = Some(accepted.identity);
        Ok(result)
    }

    pub fn release_additional_setup_receipt(
        &mut self,
        accepted: &AcceptedAdditionalSetup,
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
        validate_accepted(pending, accepted)?;
        if pending.receipt != receipt || context.dedicated_route != Some(accepted.identity) {
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
}

fn validate_accepted(
    pending: &BearerChangeAdmission,
    accepted: &AcceptedAdditionalSetup,
) -> Result<(), SignallingError> {
    let BearerChangeKind::AdditionalSetup(additional) = &pending.kind else {
        return Err(SignallingError::Phase);
    };
    if !pending.completion_admitted
        || pending.prior_starts != accepted.prior_starts
        || pending.transaction != accepted.transaction
        || pending.receipt != accepted.receipt
        || pending.expected != Some(accepted.identity)
        || additional.previous_identity != accepted.previous_identity
        || !Arc::ptr_eq(&additional.previous, &accepted.previous)
        || !Arc::ptr_eq(&additional.configuration, &accepted.configuration)
        || !Arc::ptr_eq(&additional.rabs, &accepted.rabs)
        || additional.activation != accepted.activation
        || pending.protection != accepted.protection
    {
        return Err(SignallingError::Mapping);
    }
    Ok(())
}
