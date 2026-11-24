// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::common_control::{ControlDecision, ControlIngress, ControlSdu};
use umts_rrc::establishment::{RejectionReason, build_rejection};
use umts_rrc::mobility::{
    CellUpdate, ConfirmTarget, UpdateCause, build_signalling_failure_release,
    build_unknown_context_release, cell_update_confirm_message, inspect_cell_update,
};
use umts_rrc::nas::NasPublishError;

// TS 25.331 10.3.3.50: the UE retries after this many seconds (8.1.3.9), bounded by N300.
const CONGESTION_WAIT_TIME_SECONDS: u8 = 5;

#[derive(Debug, PartialEq, Eq)]
pub enum CommonIngressWork {
    ConnectionRequested(UeKey),
    ConnectionReleased { key: UeKey, update: CellUpdate },
    UpdateConfirmed { key: UeKey, update: CellUpdate },
    UraUpdateConfirmed { key: UeKey },
    ConnectionRejected { wait_time_seconds: u8 },
    UnknownContextReleaseQueued { urnti: u32 },
    Rejected(SignallingError),
    Deferred,
}

#[derive(Debug)]
pub struct CommonIngressStep {
    pub transport: umts_dsp::prach_message::DecodedPrachMessage,
    pub message: CommonMessageKind,
    pub work: CommonIngressWork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommonMessageKind {
    ConnectionRequest,
    CellUpdate,
    Other,
    Invalid,
}

impl CommonSignalling {
    // The input is the original CRC-valid RRC SDU, not a re-encoded message or unused transport-block filler.
    #[cfg(test)]
    pub fn release_failed_rlc_update(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        bytes: &[u8],
        bit_length: usize,
        transaction: TransactionId,
    ) -> Result<CellUpdate, SignallingError> {
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        let decoded = umts_rrc_codec::decode_uplink_common(bytes, bit_length)
            .map_err(SignallingError::Codec)?;
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        let update = inspect_cell_update(context.setup.assigned(), &decoded)
            .map_err(SignallingError::Mobility)?;
        if !update.signalling_rlc_error && !update.user_rlc_error {
            return Err(SignallingError::Message);
        }
        self.release_decoded_update(key, now, bytes, bit_length, &decoded, transaction)
    }

    // The input is the original CRC-valid RRC SDU, not a re-encoded message.
    #[cfg(test)]
    pub fn handle_cell_update(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        bytes: &[u8],
        bit_length: usize,
        transaction: TransactionId,
    ) -> Result<CommonIngressWork, SignallingError> {
        let decoded = umts_rrc_codec::decode_uplink_common(bytes, bit_length)
            .map_err(SignallingError::Codec)?;
        self.handle_decoded_cell_update(key, now, bytes, bit_length, &decoded, transaction)
    }

    fn handle_decoded_cell_update(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        bytes: &[u8],
        bit_length: usize,
        decoded: &DecodedUplinkCommon,
        transaction: TransactionId,
    ) -> Result<CommonIngressWork, SignallingError> {
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        let update = inspect_cell_update(context.setup.assigned(), decoded)
            .map_err(SignallingError::Mobility)?;
        tracing::debug!(
            "CELL UPDATE: slot={} generation={} cause={:?} dedicated={} parked={:?} rlc_errors=({}, {})",
            key.slot,
            key.generation,
            update.cause,
            context.dedicated_route.is_some(),
            context.dormancy.parked,
            update.signalling_rlc_error,
            update.user_rlc_error,
        );
        // TS 25.331 8.3.1.5: an unrecoverable RLC error is answered with RRC CONNECTION RELEASE
        // on the CCCH. A UE that left CELL_DCH gets the same answer because its dedicated
        // bearers cannot be re-established on common channels here.
        if update.signalling_rlc_error || update.user_rlc_error || context.dedicated_route.is_some()
        {
            let update =
                self.release_decoded_update(key, now, bytes, bit_length, decoded, transaction)?;
            return Ok(CommonIngressWork::ConnectionReleased { key, update });
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.dormancy.defers(update.cause) {
            // A repeated cell update replaces the stored one, so the confirm answers the latest.
            let first = context.dormancy.deferred.is_none();
            context.dormancy.deferred = Some(super::dormancy::DeferredUpdate {
                bytes: bytes.to_vec(),
                bit_length,
                received: now,
            });
            context.dormancy.wake |= first;
            tracing::info!(
                "Cell update from a parked UE: slot={} generation={} cause={:?} answer deferred until its channel is prepared",
                key.slot,
                key.generation,
                update.cause
            );
            return Ok(CommonIngressWork::UpdateConfirmed { key, update });
        }
        self.confirm_decoded_update(key, bytes, bit_length, decoded, update)?;
        super::log_common("CELL UPDATE confirmed", key, now, bytes, bit_length);
        Ok(CommonIngressWork::UpdateConfirmed { key, update })
    }

    /// Sends the confirm of a deferred cell update. Returns false when none is stored.
    pub(crate) fn send_deferred_confirm(&mut self, key: UeKey) -> Result<bool, SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let Some(deferred) = context.dormancy.deferred.take() else {
            return Ok(false);
        };
        let assigned = context.setup.assigned();
        let outcome = umts_rrc_codec::decode_uplink_common(&deferred.bytes, deferred.bit_length)
            .map_err(SignallingError::Codec)
            .and_then(|decoded| {
                let update =
                    inspect_cell_update(assigned, &decoded).map_err(SignallingError::Mobility)?;
                self.confirm_decoded_update(
                    key,
                    &deferred.bytes,
                    deferred.bit_length,
                    &decoded,
                    update,
                )
            });
        match outcome {
            Ok(()) => Ok(true),
            Err(error) => {
                if super::scheduling::backpressure(&error)
                    && let Some(context) = self.registry.get_mut(key)
                {
                    context.dormancy.deferred = Some(deferred);
                }
                Err(error)
            }
        }
    }

    fn confirm_decoded_update(
        &mut self,
        key: UeKey,
        bytes: &[u8],
        bit_length: usize,
        decoded: &DecodedUplinkCommon,
        update: CellUpdate,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let assigned = context.setup.assigned();
        let paged = std::mem::take(&mut context.dormancy.paged);
        let target = context
            .dormancy
            .confirm_target(update.cause, assigned.crnti(), paged)
            .unwrap_or(ConfirmTarget::Fach {
                // TS 25.331 8.3.1.2: a cell reselection clears the UE's C-RNTI, so that update
                // re-assigns it and 8.3.1.7 then expects UTRAN MOBILITY INFORMATION CONFIRM.
                new_crnti: (update.cause == UpdateCause::CellReselection)
                    .then_some(assigned.crnti()),
            });
        if !matches!(target, ConfirmTarget::Fach { .. }) {
            tracing::info!(
                "Cell update from a parked UE: slot={} generation={} cause={:?} answer={target:?}",
                key.slot,
                key.generation,
                update.cause
            );
        }
        let transaction = context
            .transactions
            .next(transactions::MessageType::CellUpdateConfirm);
        let message =
            cell_update_confirm_message(transaction, target).map_err(SignallingError::Mobility)?;
        publish_update_confirm(
            context,
            key,
            bytes,
            bit_length,
            decoded,
            &message,
            Some(update.starts),
        )?;
        context
            .transactions
            .admitted(transactions::MessageType::CellUpdateConfirm, transaction);
        context.activity = context.activity.saturating_add(1);
        Ok(())
    }

    /// TS 25.331 8.3.1: a UE in URA_PCH reports a periodic update or a URA it cannot confirm.
    /// It stays in URA_PCH with the URA identity this cell broadcasts.
    fn confirm_ura_update(
        &mut self,
        key: UeKey,
        bytes: &[u8],
        bit_length: usize,
        decoded: &DecodedUplinkCommon,
    ) -> Result<(), SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let update = umts_rrc::mobility::inspect_ura_update(context.setup.assigned(), decoded)
            .map_err(SignallingError::Mobility)?;
        let Some(state @ umts_rrc::mobility::ParkedState::UraPch { .. }) = context.dormancy.parked
        else {
            return Err(SignallingError::Phase);
        };
        let transaction = context
            .transactions
            .next(transactions::MessageType::UraUpdateConfirm);
        let message = umts_rrc::mobility::ura_update_confirm_message(
            transaction,
            state,
            context.dormancy.drx_coefficient,
        )
        .map_err(SignallingError::Mobility)?;
        publish_update_confirm(context, key, bytes, bit_length, decoded, &message, None)?;
        context
            .transactions
            .admitted(transactions::MessageType::UraUpdateConfirm, transaction);
        context.dormancy.updates = context.dormancy.updates.saturating_add(1);
        tracing::info!(
            "URA update confirmed: slot={} generation={} cause={:?}",
            key.slot,
            key.generation,
            update.cause
        );
        Ok(())
    }

    /// TS 25.331 8.1.4.2: the release goes on the CCCH, because integrity state is unknown.
    pub(crate) fn release_unanswered_security(
        &mut self,
        key: UeKey,
        transaction: TransactionId,
    ) -> Result<bool, SignallingError> {
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        if context.dedicated_route.is_some() || context.dormancy.parked.is_some() {
            return Ok(false);
        }
        let release = build_signalling_failure_release(context.setup.assigned(), transaction)
            .map_err(SignallingError::Mobility)?;
        let core_context = match context.registration {
            Registration::Active(id) => Some(id),
            _ => None,
        };
        let connections = context
            .router
            .as_ref()
            .map_or([None; 2], NasRouter::connections);
        let pending = context.registration == Registration::Pending;
        let rb0 = &mut self.rb0;
        self.core
            .push_radio_failure_with(key, core_context, pending, connections, || {
                rb0.enqueue(
                    Bits::new(&release.bytes, release.bit_length)
                        .map_err(|_| SignallingError::Message)?,
                )
                .map_err(SignallingError::Common)
            })?;
        self.retire_admitted(key);
        tracing::warn!(
            "Connection released after an unanswered security mode command: slot={} generation={}",
            key.slot,
            key.generation
        );
        Ok(true)
    }

    fn release_decoded_update(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        bytes: &[u8],
        bit_length: usize,
        decoded: &DecodedUplinkCommon,
        transaction: TransactionId,
    ) -> Result<CellUpdate, SignallingError> {
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        let update = inspect_cell_update(context.setup.assigned(), decoded)
            .map_err(SignallingError::Mobility)?;
        let release = build_signalling_failure_release(context.setup.assigned(), transaction)
            .map_err(SignallingError::Mobility)?;
        let core_context = match context.registration {
            Registration::Active(id) => Some(id),
            _ => None,
        };
        let connections = context
            .router
            .as_ref()
            .map_or([None; 2], NasRouter::connections);
        let core = &mut self.core;
        let rb0 = &mut self.rb0;
        context.protection.admit_common_with(
            key,
            bytes,
            bit_length,
            decoded.value.integrity_check_info.is_some(),
            || {
                core.push_radio_failure_with(
                    key,
                    core_context,
                    context.registration == Registration::Pending,
                    connections,
                    || {
                        rb0.enqueue(
                            Bits::new(&release.bytes, release.bit_length)
                                .map_err(|_| SignallingError::Message)?,
                        )
                        .map_err(SignallingError::Common)
                    },
                )
            },
        )?;
        self.retire_admitted(key);
        Ok(update)
    }

    pub fn service_common_one(
        &mut self,
        source: &dyn CommonTxTarget,
        ingress: &mut ControlIngress,
        transaction: TransactionId,
    ) -> Option<CommonIngressStep> {
        let outcome = ingress.process_one_with(|sdu| {
            let transport = sdu.transport();
            let message = match sdu.message().as_ref().map(|decoded| &decoded.value.message) {
                Ok(ULCCCHMessageType::rrcConnectionRequest(_)) => {
                    CommonMessageKind::ConnectionRequest
                }
                Ok(ULCCCHMessageType::cellUpdate(_)) => CommonMessageKind::CellUpdate,
                Ok(_) => CommonMessageKind::Other,
                Err(_) => CommonMessageKind::Invalid,
            };
            match self.dispatch_common(source, sdu, transaction) {
                Ok(work) => ControlDecision::Consume(CommonIngressStep {
                    transport,
                    message,
                    work,
                }),
                Err(SignallingError::Capacity)
                | Err(SignallingError::Common(CommonControlTxError::Capacity))
                | Err(SignallingError::Common(CommonControlTxError::Rlc(
                    umts_rlc::unacknowledged::UmError::Capacity,
                )))
                | Err(SignallingError::Um(CommonUmError::Rlc(
                    umts_rlc::unacknowledged::UmError::Capacity,
                ))) => ControlDecision::Retry(CommonIngressStep {
                    transport,
                    message,
                    work: CommonIngressWork::Deferred,
                }),
                Err(error) => ControlDecision::Consume(CommonIngressStep {
                    transport,
                    message,
                    work: CommonIngressWork::Rejected(error),
                }),
            }
        })?;
        Some(match outcome {
            ControlDecision::Consume(step) | ControlDecision::Retry(step) => step,
        })
    }

    fn dispatch_common(
        &mut self,
        source: &dyn CommonTxTarget,
        sdu: ControlSdu<'_>,
        transaction: TransactionId,
    ) -> Result<CommonIngressWork, SignallingError> {
        self.rb0
            .validate_source(source, sdu.transport().start)
            .map_err(SignallingError::Common)?;
        let decoded = sdu
            .message()
            .as_ref()
            .map_err(|error| SignallingError::Codec(*error))?;
        match &decoded.value.message {
            ULCCCHMessageType::rrcConnectionRequest(request) => {
                // TS 25.331 8.1.3.3 does not call for rejecting on the protocol error indicator,
                // and a zero-wait rejection turns each retry into a failed location update.
                if request.protocol_error_indicator
                    == umts_rrc_schema::information_elements::ProtocolErrorIndicator::errorOccurred
                {
                    tracing::warn!(
                        "RRC connection request after protocol error: identity={}",
                        identity_kind(&request.initial_ue_identity)
                    );
                }
                let requested = self.request(source, sdu.transport().start, decoded);
                match requested {
                    Ok(key) => {
                        super::log_common(
                            "RRC connection request",
                            key,
                            sdu.transport().start,
                            sdu.bytes(),
                            sdu.bit_length(),
                        );
                        Ok(CommonIngressWork::ConnectionRequested(key))
                    }
                    // TS 25.331 8.1.3.4: when no context can be admitted the UE is told to wait
                    // rather than left to exhaust N300 against silence.
                    Err(SignallingError::Capacity) => {
                        let rejection = build_rejection(
                            &request.initial_ue_identity,
                            transaction,
                            RejectionReason::Congestion,
                            CONGESTION_WAIT_TIME_SECONDS,
                        )
                        .map_err(|error| {
                            SignallingError::Procedure(
                                umts_rrc::establishment::ProcedureError::MessageConstruction(error),
                            )
                        })?;
                        self.rb0
                            .enqueue(
                                Bits::new(&rejection.bytes, rejection.bit_length)
                                    .map_err(|_| SignallingError::Message)?,
                            )
                            .map_err(SignallingError::Common)?;
                        Ok(CommonIngressWork::ConnectionRejected {
                            wait_time_seconds: CONGESTION_WAIT_TIME_SECONDS,
                        })
                    }
                    Err(error) => Err(error),
                }
            }
            ULCCCHMessageType::cellUpdate(_) => {
                let urnti = umts_rrc::mobility::cell_update_urnti(decoded)
                    .map_err(SignallingError::Mobility)?;
                let Some(key) = self.key_for_urnti(urnti) else {
                    let release = build_unknown_context_release(urnti, transaction)
                        .map_err(SignallingError::Mobility)?;
                    self.rb0
                        .enqueue(
                            Bits::new(&release.bytes, release.bit_length)
                                .map_err(|_| SignallingError::Message)?,
                        )
                        .map_err(SignallingError::Common)?;
                    return Ok(CommonIngressWork::UnknownContextReleaseQueued { urnti });
                };
                self.handle_decoded_cell_update(
                    key,
                    sdu.transport().start,
                    sdu.bytes(),
                    sdu.bit_length(),
                    decoded,
                    transaction,
                )
            }
            ULCCCHMessageType::uraUpdate(_) => {
                let urnti = umts_rrc::mobility::ura_update_urnti(decoded)
                    .map_err(SignallingError::Mobility)?;
                let Some(key) = self.key_for_urnti(urnti) else {
                    let release = build_unknown_context_release(urnti, transaction)
                        .map_err(SignallingError::Mobility)?;
                    self.rb0
                        .enqueue(
                            Bits::new(&release.bytes, release.bit_length)
                                .map_err(|_| SignallingError::Message)?,
                        )
                        .map_err(SignallingError::Common)?;
                    return Ok(CommonIngressWork::UnknownContextReleaseQueued { urnti });
                };
                self.confirm_ura_update(key, sdu.bytes(), sdu.bit_length(), decoded)?;
                Ok(CommonIngressWork::UraUpdateConfirmed { key })
            }
            _ => Err(SignallingError::Message),
        }
    }
}

fn publish_update_confirm(
    context: &mut Context,
    key: UeKey,
    bytes: &[u8],
    bit_length: usize,
    decoded: &DecodedUplinkCommon,
    message: &umts_rrc_schema::class_definitions::DLDCCHMessage,
    starts: Option<umts_rrc::establishment::DomainStarts>,
) -> Result<(), SignallingError> {
    let protected = decoded.value.integrity_check_info.is_some();
    let router = &mut context.router;
    let um = &mut context.um;
    let publish = |encoded: &umts_rrc_codec::EncodedRrc| {
        let mut enqueue = || {
            um.enqueue(
                key,
                Bits::new(&encoded.bytes, encoded.bit_length)
                    .map_err(|_| SignallingError::Message)?,
            )
            .map_err(SignallingError::Um)
        };
        match (router.as_mut(), starts) {
            (Some(router), Some(starts)) => router
                .update_starts_with(key, starts, enqueue)
                .map_err(|error| match error {
                    NasPublishError::Procedure(error) => SignallingError::Nas(error),
                    NasPublishError::Downstream(error) => error,
                }),
            _ => enqueue(),
        }
    };
    let protection = &mut context.protection;
    if protected {
        protection.admit_common_response_with(key, bytes, bit_length, |admission| {
            admission
                .publish_response_with(
                    SignallingBearer::Unacknowledged.identity(),
                    message,
                    publish,
                )
                .map_err(|error| protection::schedule_error(error, |error| error))
        })?;
    } else if protection
        .integrity
        .allows_unprotected_common(key)
        .map_err(SignallingError::Integrity)?
    {
        protection
            .integrity
            .publish_with(
                key,
                SignallingBearer::Unacknowledged.identity(),
                message,
                publish,
            )
            .map_err(|error| protection::schedule_error(error, |error| error))?;
    } else {
        return Err(SignallingError::Nas(
            umts_rrc::nas::NasError::SecurityRequired,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

fn identity_kind(
    identity: &umts_rrc_schema::information_elements::InitialUEIdentity,
) -> &'static str {
    use umts_rrc_schema::information_elements::InitialUEIdentity as Identity;
    match identity {
        Identity::imsi(_) => "imsi",
        Identity::tmsi_and_LAI(_) => "tmsi",
        Identity::p_TMSI_and_RAI(_) => "p-tmsi",
        Identity::imei(_) => "imei",
        _ => "other",
    }
}
