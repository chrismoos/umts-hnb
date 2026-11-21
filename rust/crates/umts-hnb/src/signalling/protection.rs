// SPDX-License-Identifier: AGPL-3.0-or-later

pub(super) mod cipher;
#[cfg(test)]
mod retention_tests;

use super::{
    Context, CoreOutbox, SignallingError, am_admission_error, ingress::IngressView, nas_error,
};
use crate::{
    common_am::CommonAmError,
    connection_security::{self, SecurityTime},
};
use umts_rlc::acknowledged::{ReceiveCoverage, SduReceipt, SduTransmission};
use umts_rrc::{
    establishment::DomainStarts,
    integrity::{IntegritySchedule, ScheduleError, ScheduleFailure, StreamError},
    nas::SignallingBearer,
    security::{ExchangeError, SignallingSet},
};
use umts_security::{
    ciphering::{
        BearerCipherConfiguration, BearerCiphers, Configuration, CountAnchor, Position, Scope,
    },
    contexts::KeyOrigin,
    counters::Start,
};
use umts_types::{CnDomain, UeKey, bits::Bits, time::ChipInstant};

const RETAINED_INTEGRITY_CONTEXTS: usize = 3;
const RETAINED_CIPHER_CONTEXTS: usize = 3;
const CONNECTION_BEARERS: usize = 32;

pub(super) struct Protection {
    pub integrity: IntegritySchedule,
    pub(super) ciphers: BearerCiphers,
    pub(super) pending_cipher: Option<cipher::PendingCipher>,
    collected_cipher_floors: Option<[u64; 6]>,
    scratch: Vec<u8>,
    delivery: Option<Delivery>,
    pub(super) admission: ControlAdmission,
}
#[derive(Clone, Copy)]
enum Delivery {
    Command {
        origin: KeyOrigin,
        receipt: SduReceipt,
    },
    Complete {
        origin: KeyOrigin,
        receipt: SduReceipt,
        coverage: ReceiveCoverage,
        /// Air end of the first submitted STATUS covering the completion. The peer resumes its
        /// suspended bearers once this acknowledgement reaches it (TS 25.331 8.1.12.3), so any
        /// later uplink on a suspended bearer proves the resumption, whatever STATUS is current.
        acknowledged_at: Option<ChipInstant>,
        peer_progress_observed: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlAdmission {
    Ready,
    RecoveryRequired { rejected: Option<ReceiveCoverage> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BearerProtectionBlock {
    RadioRecovery,
    CipherActivation,
    SecurityCommand,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecurityDeliveryStatus {
    pub origin: KeyOrigin,
    pub command: SduTransmission,
    pub completion: Option<ReceiveCoverage>,
    /// This is lower-queue publication, not RF transmission or peer reception.
    pub completion_status_queued: bool,
    pub completion_status_submitted: bool,
    pub completion_status_acknowledged: bool,
    pub peer_progress_observed: bool,
    pub admission: ControlAdmission,
    pub status_deferral: Option<umts_rlc::acknowledged::StatusDeferral>,
}

impl Protection {
    pub(super) fn downlink_count(
        &self,
        key: UeKey,
        bearer: SignallingBearer,
        position: u64,
    ) -> Result<u32, SignallingError> {
        self.admit_downlink_position(bearer, position)?;
        match self
            .ciphers
            .selected(
                key,
                bearer.identity(),
                umts_types::Direction::Downlink,
                Position::Sequence(position),
            )
            .map_err(SignallingError::Cipher)?
        {
            umts_security::ciphering::CipherUse::Data { count, .. } => Ok(count),
            umts_security::ciphering::CipherUse::Control => Err(SignallingError::Mapping),
        }
    }
    fn admit_downlink_position(
        &self,
        bearer: SignallingBearer,
        position: u64,
    ) -> Result<(), SignallingError> {
        if self
            .pending_cipher
            .as_ref()
            .is_some_and(|pending| !pending.admits_downlink(bearer, position))
        {
            return Err(SignallingError::Capacity);
        }
        Ok(())
    }
    pub(super) fn prepare_um_payload(
        &self,
        key: UeKey,
        publication: umts_rlc::unacknowledged::UmPublication<'_>,
        bytes: &mut [u8],
    ) -> Result<(), SignallingError> {
        self.apply_downlink(
            key,
            SignallingBearer::Unacknowledged,
            publication.position,
            bytes,
        )
    }
    pub(super) fn prepare_am_payload(
        &self,
        key: UeKey,
        bearer: SignallingBearer,
        publication: umts_rlc::acknowledged::Publication<'_>,
        bytes: &mut [u8],
    ) -> Result<(), SignallingError> {
        if matches!(
            publication.kind,
            umts_rlc::acknowledged::PublicationKind::Data { .. }
        ) {
            let position = publication.position.ok_or(SignallingError::Mapping)?;
            self.apply_downlink(key, bearer, position, bytes)?;
        }
        Ok(())
    }
    fn apply_downlink(
        &self,
        key: UeKey,
        bearer: SignallingBearer,
        position: u64,
        bytes: &mut [u8],
    ) -> Result<(), SignallingError> {
        self.admit_downlink_position(bearer, position)?;
        self.ciphers
            .apply(
                key,
                bearer.identity(),
                umts_types::Direction::Downlink,
                Position::Sequence(position),
                bytes,
                bytes.len() * umts_types::bits::BITS_PER_BYTE,
            )
            .map(|_| ())
            .map_err(SignallingError::Cipher)
    }
    pub fn new(key: UeKey, fresh: u32, maximum_sdu_bytes: usize) -> Result<Self, SignallingError> {
        let layout = umts_rrc::security::BearerLayout::new(SignallingSet::Base, &[])
            .map_err(SignallingError::SecurityProcedure)?;
        let capacity =
            std::num::NonZeroUsize::new(CONNECTION_BEARERS).expect("bounded radio bearers");
        let mut ciphers = BearerCiphers::new(key, capacity).map_err(SignallingError::Cipher)?;
        for bearer in 1..layout.signalling().count() as u8 {
            let mode = layout
                .mode(bearer)
                .map_err(SignallingError::SecurityProcedure)?;
            let anchor = CountAnchor::new(mode, Position::Sequence(0), 0)
                .map_err(|error| SignallingError::Cipher(error.into()))?;
            ciphers
                .register_with(
                    BearerCipherConfiguration {
                        bearer,
                        scope: Scope::Signalling,
                        transmit: anchor,
                        receive: anchor,
                        initial: Configuration::Unprotected,
                    },
                    || Ok::<_, std::convert::Infallible>(()),
                )
                .map_err(super::transport::update_error)?;
        }
        Ok(Self {
            integrity: IntegritySchedule::new(key, fresh, SignallingSet::Base),
            ciphers,
            pending_cipher: None,
            collected_cipher_floors: None,
            scratch: vec![0; maximum_sdu_bytes],
            delivery: None,
            admission: ControlAdmission::Ready,
        })
    }
    pub fn buffer_bytes(maximum_sdu_bytes: usize) -> Option<usize> {
        let capacity = std::num::NonZeroUsize::new(CONNECTION_BEARERS)?;
        std::mem::size_of::<Self>()
            .checked_add(maximum_sdu_bytes)?
            .checked_add(BearerCiphers::maximum_storage_bytes(capacity).ok()?)?
            .checked_add(
                RETAINED_INTEGRITY_CONTEXTS
                    * (umts_security::contexts::INTEGRITY_KEY_BYTES
                        + 4 * std::mem::size_of::<usize>()),
            )?
            .checked_add(
                RETAINED_CIPHER_CONTEXTS
                    * (umts_security::contexts::CIPHER_KEY_BYTES
                        + 4 * std::mem::size_of::<usize>()),
            )
    }
    pub fn publish(
        context: &mut Context,
        key: UeKey,
        domain: umts_iuh::Domain,
        now: SecurityTime,
    ) -> Result<(), SignallingError> {
        if context.bearer_change.is_some() {
            return Err(SignallingError::Capacity);
        }
        Self::retire_delivery(context, key)?;
        if context.protection.delivery.is_some() {
            return Err(SignallingError::Phase);
        }
        if context.control.pending_receive_data()
            || context
                .control
                .status_deferral(key)
                .map_err(SignallingError::Am)?
                .is_some()
        {
            return Err(SignallingError::Capacity);
        }
        let start = context
            .router
            .as_ref()
            .ok_or(SignallingError::Phase)?
            .starts()
            .get(domain.into())
            .map(Start::new)
            .transpose()
            .map_err(|_| SignallingError::ConfigurationFailure)?;
        let cipher = context
            .security
            .cipher_plan(domain, start)
            .map_err(|error| connection_error(error, |()| SignallingError::Phase))?;
        if cipher.is_some() && context.control.pending_new_data() {
            return Err(SignallingError::Capacity);
        }
        Self::collect_keys(context, key)?;
        let transmit = [
            context.um.transmit_position(),
            context.control.window_positions().next_transmit,
            context.nas_bearer.window_positions().next_transmit,
        ];
        let origin = context
            .security
            .pending_origin(domain)
            .ok_or(SignallingError::Phase)?;
        let control = &mut context.control;
        let ciphers = &mut context.protection.ciphers;
        let pending = &mut context.protection.pending_cipher;
        if control.key() != key {
            return Err(SignallingError::Origin);
        }
        let receipt = context
            .security
            .publish_scheduled_with(
                domain,
                now,
                start,
                &mut context.protection.integrity,
                |encoded| {
                    let bits = Bits::new(&encoded.bytes, encoded.bit_length)
                        .map_err(|_| SignallingError::Am(CommonAmError::Payload))?;
                    if let Some(plan) = cipher {
                        let span = control
                            .isolated_sdu_range(
                                key,
                                encoded.bit_length.div_ceil(umts_types::bits::BITS_PER_BYTE),
                            )
                            .map_err(am_admission_error)?;
                        let transition =
                            cipher::PendingCipher::prepare(key, plan, ciphers, transmit, span)?;
                        let receipt = transition.publish_with(ciphers, || {
                            control
                                .enqueue_tracked(key, bits)
                                .map_err(am_admission_error)
                        })?;
                        *pending = Some(transition);
                        Ok(receipt)
                    } else {
                        control
                            .enqueue_tracked(key, bits)
                            .map_err(am_admission_error)
                    }
                },
            )
            .map_err(|error| connection_error(error, |error| error))?;
        control
            .require_delivery_admission(key)
            .expect("validated control bearer owner");
        context.protection.delivery = Some(Delivery::Command { origin, receipt });
        Ok(())
    }
    pub fn status(
        context: &Context,
        key: UeKey,
    ) -> Result<Option<SecurityDeliveryStatus>, SignallingError> {
        let Some(delivery) = context.protection.delivery else {
            return Ok(None);
        };
        let (origin, receipt, completion, peer_progress_observed) = match delivery {
            Delivery::Command { origin, receipt } => (origin, receipt, None, false),
            Delivery::Complete {
                origin,
                receipt,
                coverage,
                peer_progress_observed,
                ..
            } => (origin, receipt, Some(coverage), peer_progress_observed),
        };
        let status = context
            .control
            .published_status(key)
            .map_err(SignallingError::Am)?;
        Ok(Some(SecurityDeliveryStatus {
            origin,
            command: context
                .control
                .transmission(key, receipt)
                .map_err(SignallingError::Am)?,
            completion,
            peer_progress_observed,
            completion_status_queued: completion
                .is_some_and(|coverage| status.is_some_and(|status| status.covers(coverage))),
            completion_status_submitted: completion.is_some_and(|coverage| {
                context
                    .control
                    .status_submission(key)
                    .ok()
                    .flatten()
                    .is_some_and(|submission| {
                        submission.coverage.covers(coverage) && submission.transport.submitted()
                    })
            }),
            completion_status_acknowledged: completion.is_some_and(|coverage| {
                context
                    .control
                    .acknowledged_status(key)
                    .ok()
                    .flatten()
                    .is_some_and(|status| status.covers(coverage))
            }),
            admission: context.protection.admission,
            status_deferral: context
                .control
                .status_deferral(key)
                .map_err(SignallingError::Am)?,
        }))
    }
    pub(super) fn delivery_label(&self) -> &'static str {
        match &self.delivery {
            None => "none",
            Some(Delivery::Command { .. }) => "command",
            Some(Delivery::Complete {
                peer_progress_observed: true,
                ..
            }) => "complete-progressed",
            Some(Delivery::Complete { .. }) => "complete-awaiting-progress",
        }
    }
    pub fn blocks_status(&self) -> bool {
        matches!(self.admission, ControlAdmission::RecoveryRequired { .. })
    }
    pub(super) fn blocks_security_reconfiguration(&self) -> bool {
        self.delivery.is_some() || self.pending_cipher.is_some() || self.blocks_status()
    }
    // TS 25.331 8.1.12.3 and 8.1.12.5 permit protected DL messages after authenticated Complete.
    pub(super) fn bearer_reconfiguration_block(&self) -> Option<BearerProtectionBlock> {
        if self.blocks_status() {
            Some(BearerProtectionBlock::RadioRecovery)
        } else if self.pending_cipher.is_some() {
            Some(BearerProtectionBlock::CipherActivation)
        } else if matches!(self.delivery, Some(Delivery::Command { .. })) {
            Some(BearerProtectionBlock::SecurityCommand)
        } else {
            None
        }
    }
    pub(super) fn blocks_reset(&self, bearer: SignallingBearer) -> bool {
        self.pending_cipher.is_some()
            || self.blocks_status()
            || matches!(self.delivery, Some(Delivery::Command { .. }))
            || (bearer == SignallingBearer::ControlAm && self.delivery.is_some())
    }
    /// Remembers when the completion's acknowledgement left the transmitter.
    pub fn observe_acknowledgement(
        context: &mut Context,
        key: UeKey,
    ) -> Result<(), SignallingError> {
        let Some(Delivery::Complete {
            coverage,
            acknowledged_at: acknowledged_at @ None,
            ..
        }) = context.protection.delivery.as_mut()
        else {
            return Ok(());
        };
        if let Some(status) = context
            .control
            .status_submission(key)
            .map_err(SignallingError::Am)?
            && status.coverage.covers(*coverage)
            && status.transport.submitted()
        {
            *acknowledged_at = Some(status.transport.air_end());
        }
        Ok(())
    }
    pub fn retire_delivery(
        context: &mut Context,
        key: UeKey,
    ) -> Result<Option<KeyOrigin>, SignallingError> {
        if context.protection.blocks_status() {
            return Ok(None);
        }
        Self::observe_acknowledgement(context, key)?;
        let Some(Delivery::Complete {
            origin,
            receipt,
            coverage,
            peer_progress_observed,
            ..
        }) = context.protection.delivery
        else {
            return Ok(None);
        };
        let acknowledgement_received = context
            .control
            .acknowledged_status(key)
            .map_err(SignallingError::Am)?
            .is_some_and(|status| status.covers(coverage));
        if !peer_progress_observed && !acknowledgement_received {
            return Ok(None);
        }
        if context
            .control
            .transmission(key, receipt)
            .map_err(SignallingError::Am)?
            != SduTransmission::Acknowledged
        {
            return Ok(None);
        }
        let control = &mut context.control;
        let security = &mut context.security;
        context
            .protection
            .integrity
            .confirm_delivery_with(key, origin, || {
                security
                    .confirm_completion_with(origin, || {
                        control
                            .release_receipt(key, receipt)
                            .map_err(SignallingError::Am)
                    })
                    .map_err(|error| connection_error(error, |error| error))
            })
            .map_err(|error| schedule_error(error, |error| error))?;
        context.protection.delivery = None;
        Ok(Some(origin))
    }
    pub(super) fn collect_keys(
        context: &mut Context,
        key: UeKey,
    ) -> Result<usize, SignallingError> {
        if context.protection.delivery.is_some()
            || context.protection.pending_cipher.is_some()
            || context.protection.blocks_status()
            || context.protection.integrity.pending_origin().is_some()
            || context.protection.integrity.unconfirmed_origin().is_some()
            || context.security.unconfirmed_origin().is_some()
        {
            return Ok(0);
        }
        let control = context.control.window_positions();
        let nas = context.nas_bearer.window_positions();
        // Basic UM may re-admit its last SN, so its receive floor must retain that position.
        let um_receive = context
            .um
            .receive_position()
            .map_err(SignallingError::Um)?
            .saturating_sub(1);
        let floors = [
            context.um.transmit_position(),
            um_receive,
            control.oldest_transmit,
            control.next_receive,
            nas.oldest_transmit,
            nas.next_receive,
        ];
        if context.protection.collected_cipher_floors != Some(floors) {
            let entries = std::array::from_fn::<_, 6, _>(|index| {
                umts_security::ciphering::CipherRetirement::Sequenced {
                    bearer: (index / 2 + 1) as u8,
                    direction: if index.is_multiple_of(2) {
                        umts_types::Direction::Downlink
                    } else {
                        umts_types::Direction::Uplink
                    },
                    floor: Position::Sequence(floors[index]),
                }
            });
            context
                .protection
                .ciphers
                .retire(key, &entries)
                .map_err(SignallingError::Cipher)?;
            context.protection.collected_cipher_floors = Some(floors);
        }
        context
            .protection
            .integrity
            .collect_unused(key)
            .map_err(SignallingError::Integrity)
    }
    pub fn reject_control(&mut self, rejected: Option<ReceiveCoverage>) {
        if self.integrity.configurations() != 0 {
            self.require_recovery(rejected);
        }
    }
    pub(super) fn require_recovery(&mut self, rejected: Option<ReceiveCoverage>) {
        if !self.blocks_status() {
            self.admission = ControlAdmission::RecoveryRequired { rejected };
        }
    }
    pub(super) fn admit_common_with<T>(
        &mut self,
        key: UeKey,
        bytes: &[u8],
        bit_length: usize,
        protected: bool,
        admit: impl FnOnce() -> Result<T, SignallingError>,
    ) -> Result<T, SignallingError> {
        if protected {
            self.integrity
                .receive_common_bits_with(key, bytes, bit_length, &mut self.scratch, |_| admit())
                .map_err(|error| schedule_error(error, |error| error))
        } else if self
            .integrity
            .allows_unprotected_common(key)
            .map_err(SignallingError::Integrity)?
        {
            admit()
        } else {
            Err(SignallingError::Nas(
                umts_rrc::nas::NasError::SecurityRequired,
            ))
        }
    }

    pub(super) fn admit_common_response_with<T>(
        &mut self,
        key: UeKey,
        bytes: &[u8],
        bit_length: usize,
        admit: impl FnOnce(
            &mut umts_rrc::integrity::CompletionAdmission<'_>,
        ) -> Result<T, SignallingError>,
    ) -> Result<T, SignallingError> {
        self.integrity
            .receive_common_response_with(
                key,
                bytes,
                bit_length,
                &mut self.scratch,
                |admission, _| admit(admission),
            )
            .map_err(|error| schedule_error(error, |error| error))
    }
    pub fn receive(
        context: &mut Context,
        core: &mut CoreOutbox,
        entry: &IngressView<'_>,
        now: SecurityTime,
        received_time: umts_rlc::acknowledged::ReceiveTime,
        accept: &mut dyn FnMut(
            super::ReceivedControl<'_>,
            super::ControlSecurity<'_>,
        ) -> Result<(), SignallingError>,
    ) -> Result<UeKey, SignallingError> {
        if context.protection.blocks_status() {
            return Err(SignallingError::Phase);
        }
        let key = entry.key;
        let message = entry
            .message
            .as_ref()
            .map_err(|error| SignallingError::Codec(*error))?;
        let router = context.router.as_mut().ok_or(SignallingError::Phase)?;
        let emergency = context.emergency;
        let security = &mut context.security;
        let delivery = &mut context.protection.delivery;
        let ciphers = &mut context.protection.ciphers;
        let pending_cipher = &mut context.protection.pending_cipher;
        let receive = [
            context.um.receive_position().map_err(SignallingError::Um)?,
            context.control.window_positions().next_receive,
            context.nas_bearer.window_positions().next_receive,
        ];
        let control = &mut context.control;
        let bearer_change = &mut context.bearer_change;
        let dedicated_route = context.dedicated_route;
        let idle_release = &mut context.idle_release;
        let reports = &mut context.reports;
        let dormancy = &mut context.dormancy;
        let mut completed = None;
        let mut confirmed = false;
        context
            .protection
            .integrity
            .receive_with(
                key,
                entry.bearer.identity(),
                entry.bytes,
                &mut context.protection.scratch,
                |admission, accepted| {
                    if super::signalling_release::admit(
                        entry.bearer,
                        message,
                        Some(router),
                        core,
                        &mut dormancy.session_end,
                    )?
                    {
                        return Ok(());
                    }
                    super::log_dedicated(key, &message.value.message);
                    if dormancy.admit(
                        &message.value.message,
                        entry.coverage,
                        received_time.serviced(),
                    ) {
                        if let Some(coverage) = entry.coverage {
                            control
                                .request_status_confirmation(key, coverage)
                                .map_err(SignallingError::Am)?;
                        }
                        return Ok(());
                    }
                    if reports.admit(entry.bearer, &message.value.message)? {
                        if matches!(&message.value.message, umts_rrc_schema::class_definitions::ULDCCHMessageType::measurementReport(_)) {
                            reports.log_measurement(key, entry.time, bearer_change.is_some());
                        }
                        return Ok(());
                    }
                    if super::lifecycle::release_complete(idle_release, dedicated_route.is_some(), entry.bearer, message, key, entry.coverage, control)? {
                        return Ok(());
                    }
                    if super::control::external(&message.value.message) {
                        let result = accept(
                            super::ReceivedControl {
                                key,
                                bearer: entry.bearer,
                                time: received_time,
                                message,
                                protection: super::ControlProtection::Integrity {
                                    origin: accepted.origin(),
                                    count: accepted.count(),
                                },
                            },
                            super::ControlSecurity::new(
                                security,
                                ciphers,
                                router,
                                control,
                                bearer_change,
                                dedicated_route,
                            ),
                        );
                        return reports.consume_unsupported(result);
                    }
                    match entry.bearer {
                        SignallingBearer::ControlAm => {
                            let coverage = entry.coverage.ok_or(SignallingError::Mapping)?;
                            let Some(Delivery::Command { origin, receipt }) = *delivery else {
                                return Err(SignallingError::Phase);
                            };
                            if origin != accepted.origin() {
                                return Err(SignallingError::Origin);
                            }
                            match control
                                .transmission(key, receipt)
                                .map_err(SignallingError::Am)?
                            {
                                SduTransmission::InFlight {
                                    delimited: true, ..
                                }
                                | SduTransmission::Acknowledged => (),
                                _ => return Err(SignallingError::Phase),
                            }
                            security
                                .complete_with(origin.domain.into(), now, accepted, |activation| {
                                    // TS 25.331 8.1.12.3.1 resets the latest START for the new keyset.
                                    let reset_start = activation.new_keys.then_some(0);
                                    let starts = DomainStarts {
                                        cs: if origin.domain == CnDomain::Cs { reset_start } else { None },
                                        ps: if origin.domain == CnDomain::Ps { reset_start } else { None },
                                    };
                                    router.update_starts_with(key, starts, || {
                                        if let Some(pending) = pending_cipher.as_ref() {
                                            pending.complete_with(
                                                key,
                                                ciphers,
                                                activation.completion,
                                                receive,
                                                coverage,
                                                || {
                                                    admission
                                                        .complete(activation.completion)
                                                        .map_err(SignallingError::Integrity)
                                                },
                                            )
                                        } else if activation.completion.cipher().is_some() {
                                            Err(SignallingError::ConfigurationFailure)
                                        } else {
                                            admission
                                                .complete(activation.completion)
                                                .map_err(SignallingError::Integrity)
                                        }
                                    }).map_err(nas_error)
                                })
                                .map_err(|error| connection_error(error, |error| error))?;
                            completed = Some(Delivery::Complete {
                                origin,
                                receipt,
                                coverage,
                                acknowledged_at: None,
                                peer_progress_observed: false,
                            });
                            Ok(())
                        }
                        SignallingBearer::NasAm | SignallingBearer::LowPriorityNasAm => {
                            let status = control
                                .status_submission(key)
                                .map_err(SignallingError::Am)?;
                            let peer_progress_observed = match (*delivery, entry.coverage) {
                                (
                                    Some(Delivery::Complete {
                                        origin,
                                        coverage,
                                        acknowledged_at,
                                        peer_progress_observed,
                                        ..
                                    }),
                                    Some(received),
                                ) => {
                                    let first = received.first_received();
                                    let after = |end: ChipInstant| {
                                        first.epoch == end.epoch && first.index >= end.index
                                    };
                                    let observed = accepted.origin() == origin
                                        && (acknowledged_at.is_some_and(after)
                                            || status.is_some_and(|status| {
                                                status.coverage.covers(coverage)
                                                    && status.transport.submitted()
                                                    && after(status.transport.air_end())
                                            }));
                                    if !observed && !peer_progress_observed {
                                        tracing::debug!(
                                            "NAS uplink arrived before security mode completion was confirmed: key={key:?} origin_match={} acknowledged_at={acknowledged_at:?} first_received={first:?} status={:?}",
                                            accepted.origin() == origin,
                                            status.map(|status| (
                                                status.coverage.covers(coverage),
                                                status.transport.submitted(),
                                                status.transport.air_end()
                                            ))
                                        );
                                    }
                                    observed
                                }
                                _ => false,
                            };
                            router
                                .forward_authenticated_with(accepted, |event| {
                                    core.push_nas(event, emergency)
                                })
                                .map(|_| ())
                                .map_err(nas_error)?;
                            confirmed = peer_progress_observed;
                            Ok(())
                        }
                        _ => Err(SignallingError::Mapping),
                    }
                },
            )
            .map_err(|error| schedule_error(error, |error| error))?;
        if let Some(completed) = completed {
            let Delivery::Complete { coverage, .. } = completed else {
                unreachable!("authenticated security completion");
            };
            context
                .control
                .request_status_confirmation(key, coverage)
                .map_err(SignallingError::Am)?;
            *delivery = Some(completed);
            *pending_cipher = None;
            router.require_security();
            context.capabilities.require_integrity();
        }
        if confirmed
            && let Some(Delivery::Complete {
                peer_progress_observed,
                ..
            }) = delivery.as_mut()
        {
            *peer_progress_observed = true;
        }
        Ok(key)
    }
}

pub(super) fn schedule_error<E>(
    error: ScheduleError<E>,
    downstream: impl FnOnce(E) -> SignallingError,
) -> SignallingError {
    match error {
        ScheduleError::Schedule(ScheduleFailure::Suspended) => SignallingError::Capacity,
        ScheduleError::Schedule(error) => SignallingError::Integrity(error),
        ScheduleError::Downstream(error) | ScheduleError::Stream(StreamError::Consumer(error)) => {
            downstream(error)
        }
        ScheduleError::Stream(StreamError::Duplicate) => SignallingError::Duplicate,
        ScheduleError::Stream(StreamError::Security(error)) => {
            SignallingError::Integrity(ScheduleFailure::Counter(error))
        }
        ScheduleError::Stream(StreamError::Protection(error)) => {
            SignallingError::Integrity(ScheduleFailure::Integrity(error))
        }
    }
}
pub(super) fn connection_error<E>(
    error: connection_security::Error<E>,
    downstream: impl FnOnce(E) -> SignallingError,
) -> SignallingError {
    use connection_security::Error;
    match error {
        Error::Owner | Error::Association => SignallingError::Origin,
        Error::Time => SignallingError::Procedure(umts_rrc::establishment::ProcedureError::Time),
        Error::Phase => SignallingError::Phase,
        Error::Capacity => SignallingError::Capacity,
        Error::Conflict | Error::RadioPlan | Error::MissingStart => {
            SignallingError::ConfigurationFailure
        }
        Error::Command(error) => SignallingError::SecurityProcedure(error),
        Error::Exchange(error) => exchange_error(error, downstream),
        Error::Scheduled(error) => schedule_error(error, |error| exchange_error(error, downstream)),
        Error::Downstream(error) => downstream(error),
    }
}
fn exchange_error<E>(
    error: ExchangeError<E>,
    downstream: impl FnOnce(E) -> SignallingError,
) -> SignallingError {
    match error {
        ExchangeError::Consumer(error)
        | ExchangeError::Protection(StreamError::Consumer(error)) => downstream(error),
        ExchangeError::Protection(StreamError::Duplicate) => SignallingError::Duplicate,
        ExchangeError::Protection(StreamError::Security(error)) => {
            SignallingError::Integrity(ScheduleFailure::Counter(error))
        }
        ExchangeError::Protection(StreamError::Protection(error)) => {
            SignallingError::Integrity(ScheduleFailure::Integrity(error))
        }
        ExchangeError::Message(error) => SignallingError::SecurityProcedure(error),
        ExchangeError::Time | ExchangeError::Expired => {
            SignallingError::Procedure(umts_rrc::establishment::ProcedureError::Time)
        }
        ExchangeError::Phase | ExchangeError::ConflictingCompletion => SignallingError::Phase,
    }
}
