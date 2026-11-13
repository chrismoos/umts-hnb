// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_transport::{FachError, FachTransport, rach_pdu_size},
    common_tx::{CommonTxError, CommonTxTarget},
};
use std::num::NonZeroUsize;
#[cfg(test)]
use umts_mac::CommonPdu;
use umts_mac::{CommonHeader, CommonIdentity, LogicalChannelId, MacError};
#[cfg(test)]
use umts_rlc::acknowledged::AmCounters;
use umts_rlc::acknowledged::{
    AmConfig, AmEntity, AmError, AmLimits, AmReceive, AmState, Delivery, Publication,
    PublicationKind, PublishError, ReceiveError, ReceivePreparation,
};
use umts_types::{
    UeKey,
    bits::{BITS_PER_BYTE, Bits},
    time::ChipInstant,
};

const AM_CLEAR_HEADER_BYTES: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonAmError {
    Origin,
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Mapping,
    Profile,
    Payload,
    Capacity,
    Rlc(AmError),
    Mac(MacError),
    Transmit(CommonTxError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommonAmPreparedError<E> {
    Bearer(CommonAmError),
    Preparation(E),
}
#[derive(Debug, PartialEq, Eq)]
pub enum AmPayloadPublishError<P, A> {
    Bearer(CommonAmError),
    Preparation(P),
    Admission(A),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmPublication {
    pub kind: PublicationKind,
    pub status_coverage: Option<umts_rlc::acknowledged::StatusCoverage>,
}
#[derive(Clone, Debug)]
pub struct StatusSubmission {
    pub coverage: umts_rlc::acknowledged::StatusCoverage,
    pub transport: crate::tx_evidence::TransportSubmission,
}
impl From<FachError> for CommonAmError {
    fn from(error: FachError) -> Self {
        match error {
            FachError::Profile => Self::Profile,
            FachError::Payload => Self::Payload,
            FachError::Mac(error) => Self::Mac(error),
            FachError::Transmit(error) => Self::Transmit(error),
        }
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct CommonAmBearer {
    status_submission: Option<StatusSubmission>,
    key: UeKey,
    identity: u16,
    logical: LogicalChannelId,
    fach: FachTransport,
    rlc: AmEntity,
    rx: Vec<u8>,
    deciphered: Vec<u8>,
    prepared_tx: Vec<u8>,
    rrc: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct CommonAmSettings {
    pub config: AmConfig,
    pub limits: AmLimits,
    pub started_at: ChipInstant,
}

impl CommonAmBearer {
    pub fn preparation_bytes(config: AmConfig) -> Result<usize, CommonAmError> {
        config
            .transmit_pdu_bytes
            .get()
            .checked_add(config.receive_pdu_bytes.get())
            .ok_or(CommonAmError::Capacity)
    }

    pub fn sizes(
        source: &dyn CommonTxTarget,
        channel: usize,
    ) -> Result<(NonZeroUsize, NonZeroUsize), CommonAmError> {
        let header = CommonHeader::Dedicated {
            identity: CommonIdentity::Cell(0),
            channel: LogicalChannelId::new(umts_rrc::nas::SignallingBearer::ControlAm.identity())
                .map_err(CommonAmError::Mac)?,
        };
        Ok((
            FachTransport::pdu_size(source, channel, header)?,
            rach_pdu_size(source, header)?,
        ))
    }
    pub fn new(
        source: &dyn CommonTxTarget,
        channel: usize,
        key: UeKey,
        identity: u16,
        logical: LogicalChannelId,
        settings: CommonAmSettings,
    ) -> Result<Self, CommonAmError> {
        let CommonAmSettings {
            config,
            limits,
            started_at,
        } = settings;
        let mut fach = FachTransport::new(
            source,
            channel,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(identity),
                channel: logical,
            },
        )?;
        if fach.epoch() != key.epoch || started_at.epoch != key.epoch {
            return Err(CommonAmError::Origin);
        }
        fach.bind_ue(key)?;
        if Self::sizes(source, channel)? != (config.transmit_pdu_bytes, config.receive_pdu_bytes) {
            return Err(CommonAmError::Profile);
        }
        Ok(Self {
            status_submission: None,
            key,
            identity,
            logical,
            fach,
            rlc: AmEntity::new(config, limits, started_at).map_err(CommonAmError::Rlc)?,
            rx: vec![0; config.receive_pdu_bytes.get()],
            deciphered: vec![0; config.receive_pdu_bytes.get()],
            prepared_tx: vec![0; config.transmit_pdu_bytes.get()],
            rrc: vec![0; limits.maximum_sdu_bytes.get()],
        })
    }
    pub fn key(&self) -> UeKey {
        self.key
    }
    pub fn transmit_pdu_bytes(&self) -> NonZeroUsize {
        self.rlc.configuration().transmit_pdu_bytes
    }
    pub fn configuration(&self) -> AmConfig {
        self.rlc.configuration()
    }
    #[cfg(test)]
    pub fn counters(&self) -> AmCounters {
        self.rlc.counters()
    }
    pub fn state(&self) -> AmState {
        self.rlc.state()
    }
    pub fn tick(&mut self, now: ChipInstant) -> Result<(), CommonAmError> {
        self.rlc.tick(now).map_err(CommonAmError::Rlc)
    }
    fn origin(&self, key: UeKey) -> Result<(), CommonAmError> {
        if key == self.key {
            Ok(())
        } else {
            Err(CommonAmError::Origin)
        }
    }
    pub fn enqueue(&mut self, key: UeKey, encoded: Bits<'_>) -> Result<(), CommonAmError> {
        self.origin(key)?;
        log_downlink(key, encoded);
        let output = rrc_sdu(&mut self.rrc, encoded)?;
        self.rlc.enqueue(output).map_err(CommonAmError::Rlc)
    }
    pub fn enqueue_tracked(
        &mut self,
        key: UeKey,
        encoded: Bits<'_>,
    ) -> Result<umts_rlc::acknowledged::SduReceipt, CommonAmError> {
        self.origin(key)?;
        let output = rrc_sdu(&mut self.rrc, encoded)?;
        self.rlc.enqueue_tracked(output).map_err(CommonAmError::Rlc)
    }
    pub fn transmission(
        &self,
        key: UeKey,
        receipt: umts_rlc::acknowledged::SduReceipt,
    ) -> Result<umts_rlc::acknowledged::SduTransmission, CommonAmError> {
        self.origin(key)?;
        self.rlc.transmission(receipt).map_err(CommonAmError::Rlc)
    }
    pub fn release_receipt(
        &mut self,
        key: UeKey,
        receipt: umts_rlc::acknowledged::SduReceipt,
    ) -> Result<(), CommonAmError> {
        self.origin(key)?;
        self.rlc
            .release_receipt(receipt)
            .map_err(CommonAmError::Rlc)
    }
    pub fn published_status(
        &self,
        key: UeKey,
    ) -> Result<Option<umts_rlc::acknowledged::StatusCoverage>, CommonAmError> {
        self.origin(key)?;
        Ok(self.rlc.published_status())
    }
    pub fn status_submission(
        &self,
        key: UeKey,
    ) -> Result<Option<&StatusSubmission>, CommonAmError> {
        self.origin(key)?;
        Ok(self
            .status_submission
            .as_ref()
            .filter(|submission| self.rlc.published_status() == Some(submission.coverage)))
    }
    pub fn request_status_confirmation(
        &mut self,
        key: UeKey,
        received: umts_rlc::acknowledged::ReceiveCoverage,
    ) -> Result<(), CommonAmError> {
        self.origin(key)?;
        self.rlc
            .request_status_confirmation(received)
            .map_err(CommonAmError::Rlc)
    }
    pub fn acknowledged_status(
        &self,
        key: UeKey,
    ) -> Result<Option<umts_rlc::acknowledged::StatusCoverage>, CommonAmError> {
        self.origin(key)?;
        Ok(self.rlc.acknowledged_status())
    }
    pub(crate) fn release_submitted_status_emission(&mut self) {
        if let Some(submission) = self.status_submission.as_mut() {
            submission.transport.detach_submitted_emission();
        }
    }
    #[cfg(test)]
    pub fn initiate_reset(&mut self, key: UeKey, now: ChipInstant) -> Result<(), CommonAmError> {
        self.origin(key)?;
        self.rlc.initiate_reset(now).map_err(CommonAmError::Rlc)
    }
    pub fn require_delivery_admission(&mut self, key: UeKey) -> Result<(), CommonAmError> {
        self.origin(key)?;
        self.rlc.require_delivery_admission();
        Ok(())
    }
    pub fn upper_admission_pending(
        &mut self,
        key: UeKey,
        pending: bool,
    ) -> Result<(), CommonAmError> {
        self.origin(key)?;
        self.rlc.upper_admission_pending(pending);
        Ok(())
    }
    pub fn status_deferral(
        &self,
        key: UeKey,
    ) -> Result<Option<umts_rlc::acknowledged::StatusDeferral>, CommonAmError> {
        self.origin(key)?;
        Ok(self.rlc.status_deferral())
    }
    // The caller supplies a MAC PDU from CRC-valid transport after registry lookup.
    #[cfg(test)]
    pub fn receive(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        pdu: CommonPdu<'_>,
    ) -> Result<AmReceive, CommonAmError> {
        self.receive_input(key, pdu)?;
        self.rlc.receive(now, &self.rx).map_err(CommonAmError::Rlc)
    }

    #[cfg(test)]
    fn receive_input(&mut self, key: UeKey, pdu: CommonPdu<'_>) -> Result<(), CommonAmError> {
        self.origin(key)?;
        if pdu.header
            != (CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(self.identity),
                channel: self.logical,
            })
        {
            return Err(CommonAmError::Mapping);
        }
        self.receive_payload(key, pdu.payload)
    }

    fn receive_payload(&mut self, key: UeKey, payload: Bits<'_>) -> Result<(), CommonAmError> {
        self.origin(key)?;
        // TS 25.322 8.2: changing fixed AM PDU size is not a transport-only modification.
        if payload.len() != self.rx.len() * BITS_PER_BYTE {
            return Err(CommonAmError::Payload);
        }
        payload
            .copy_to(&mut self.rx)
            .map_err(|_| CommonAmError::Payload)?;
        Ok(())
    }

    #[cfg(test)]
    pub fn receive_prepared<E>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        pdu: CommonPdu<'_>,
        prepare: impl FnOnce(ReceivePreparation, &mut [u8]) -> Result<(), E>,
    ) -> Result<AmReceive, CommonAmPreparedError<E>> {
        self.receive_prepared_with_reset(key, now, pdu, |request, plain| {
            prepare(request, plain).map(|()| None)
        })
    }
    #[cfg(test)]
    pub fn receive_prepared_with_reset<E>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        pdu: CommonPdu<'_>,
        prepare: impl FnOnce(
            ReceivePreparation,
            &mut [u8],
        ) -> Result<Option<umts_rlc::acknowledged::ResetCounters>, E>,
    ) -> Result<AmReceive, CommonAmPreparedError<E>> {
        self.receive_prepared_timed(
            key,
            umts_rlc::acknowledged::ReceiveTime::simultaneous(now),
            pdu,
            prepare,
        )
    }

    #[cfg(test)]
    pub fn receive_prepared_timed<E>(
        &mut self,
        key: UeKey,
        time: umts_rlc::acknowledged::ReceiveTime,
        pdu: CommonPdu<'_>,
        prepare: impl FnOnce(
            ReceivePreparation,
            &mut [u8],
        ) -> Result<Option<umts_rlc::acknowledged::ResetCounters>, E>,
    ) -> Result<AmReceive, CommonAmPreparedError<E>> {
        self.receive_input(key, pdu)
            .map_err(CommonAmPreparedError::Bearer)?;
        self.receive_staged(time, prepare)
    }

    /// The caller validates MAC mapping and retains the original PDU when admission fails.
    pub fn receive_payload_prepared_timed<E>(
        &mut self,
        key: UeKey,
        time: umts_rlc::acknowledged::ReceiveTime,
        payload: Bits<'_>,
        prepare: impl FnOnce(
            ReceivePreparation,
            &mut [u8],
        ) -> Result<Option<umts_rlc::acknowledged::ResetCounters>, E>,
    ) -> Result<AmReceive, CommonAmPreparedError<E>> {
        self.receive_payload(key, payload)
            .map_err(CommonAmPreparedError::Bearer)?;
        self.receive_staged(time, prepare)
    }

    fn receive_staged<E>(
        &mut self,
        time: umts_rlc::acknowledged::ReceiveTime,
        prepare: impl FnOnce(
            ReceivePreparation,
            &mut [u8],
        ) -> Result<Option<umts_rlc::acknowledged::ResetCounters>, E>,
    ) -> Result<AmReceive, CommonAmPreparedError<E>> {
        let raw = &self.rx;
        let plain = self.deciphered.as_mut_slice();
        self.rlc
            .receive_prepared_timed(time, raw, move |request| {
                plain.copy_from_slice(raw);
                let reset_counters = prepare(request, plain)?;
                Ok(umts_rlc::acknowledged::PreparedReceive {
                    plaintext: &*plain,
                    reset_counters,
                })
            })
            .map_err(|error| match error {
                ReceiveError::Rlc(error) => {
                    CommonAmPreparedError::Bearer(CommonAmError::Rlc(error))
                }
                ReceiveError::Preparation(error) => CommonAmPreparedError::Preparation(error),
            })
    }
    pub fn deliver_with<T, E>(
        &mut self,
        key: UeKey,
        publish: impl FnOnce(Delivery<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        if key != self.key {
            return Err(PublishError::Rlc(AmError::State));
        }
        self.rlc.deliver_with(publish)
    }
    #[cfg(test)]
    pub fn publish_frame(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        start: ChipInstant,
    ) -> Result<Option<PublicationKind>, CommonAmError> {
        self.publish_frame_observed(source, now, start)
            .map(|result| result.map(|published| published.kind))
    }
    /// Metadata describes admitted FACH publication, not on-air completion.
    #[cfg(test)]
    pub fn publish_frame_observed(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        start: ChipInstant,
    ) -> Result<Option<AmPublication>, CommonAmError> {
        self.fach.validate(source, start)?;
        let fach = &mut self.fach;
        let result = self.rlc.publish_next(now, |publication| {
            let transport = fach
                .publish_tracked(source, start, publication.bytes)
                .map_err(CommonAmError::from)?;
            Ok(publication
                .status_coverage
                .map(|coverage| StatusSubmission {
                    coverage,
                    transport,
                }))
        });
        match result {
            Ok(result) => Ok(result.map(|(kind, submission)| {
                let status_coverage = submission.as_ref().map(|submission| submission.coverage);
                if submission.is_some() {
                    self.status_submission = submission;
                }
                AmPublication {
                    kind,
                    status_coverage,
                }
            })),
            Err(PublishError::Rlc(error)) => Err(CommonAmError::Rlc(error)),
            Err(PublishError::Downstream(error)) => Err(error),
        }
    }

    #[cfg(test)]
    pub fn publish_frame_prepared<E>(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        start: ChipInstant,
        prepare: impl FnOnce(Publication<'_>, &mut [u8]) -> Result<(), E>,
    ) -> Result<Option<PublicationKind>, CommonAmPreparedError<E>> {
        self.fach
            .validate(source, start)
            .map_err(|error| CommonAmPreparedError::Bearer(error.into()))?;
        let frame = PreparedFrame {
            source,
            fach: &mut self.fach,
            start,
            bytes: &mut self.prepared_tx,
        };
        let result = prepared_result(
            self.rlc
                .publish_next(now, |publication| frame.publish(publication, prepare)),
        )?;
        Ok(result.map(|(kind, submission)| {
            if submission.is_some() {
                self.status_submission = submission;
            }
            kind
        }))
    }

    pub fn publish_frame_mapped<E>(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        start: ChipInstant,
        map_count: impl FnMut(u64) -> Result<u32, E>,
        prepare: impl FnOnce(Publication<'_>, &mut [u8]) -> Result<(), E>,
    ) -> Result<Option<PublicationKind>, CommonAmPreparedError<E>> {
        self.fach
            .validate(source, start)
            .map_err(|error| CommonAmPreparedError::Bearer(error.into()))?;
        let fach = &mut self.fach;
        publish_payload_mapped(
            &mut self.rlc,
            &mut self.prepared_tx,
            &mut self.status_submission,
            now,
            map_count,
            prepare,
            |bytes| {
                fach.publish_tracked(source, start, bytes)
                    .map_err(CommonAmError::from)
            },
        )
        .map(|result| result.map(|result| result.kind))
        .map_err(|error| match error {
            AmPayloadPublishError::Bearer(error) | AmPayloadPublishError::Admission(error) => {
                CommonAmPreparedError::Bearer(error)
            }
            AmPayloadPublishError::Preparation(error) => CommonAmPreparedError::Preparation(error),
        })
    }

    /// Admission must return the actual scheduled transport receipt, never a fabricated success.
    pub fn publish_payload_mapped<P, A>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        map_count: impl FnMut(u64) -> Result<u32, P>,
        prepare: impl FnOnce(Publication<'_>, &mut [u8]) -> Result<(), P>,
        admit: impl FnOnce(&[u8]) -> Result<crate::tx_evidence::TransportSubmission, A>,
    ) -> Result<Option<AmPublication>, AmPayloadPublishError<P, A>> {
        self.origin(key).map_err(AmPayloadPublishError::Bearer)?;
        publish_payload_mapped(
            &mut self.rlc,
            &mut self.prepared_tx,
            &mut self.status_submission,
            now,
            map_count,
            prepare,
            admit,
        )
    }

    pub fn window_positions(&self) -> umts_rlc::acknowledged::WindowPositions {
        self.rlc.window_positions()
    }
    pub fn forecast_delivery(
        &self,
        key: UeKey,
        bytes: usize,
    ) -> Result<umts_rlc::acknowledged::DeliveryForecast, CommonAmError> {
        self.origin(key)?;
        self.rlc
            .forecast_delivery(bytes)
            .map_err(CommonAmError::Rlc)
    }
    pub fn pending_new_data(&self) -> bool {
        self.rlc.pending_new_data()
    }
    pub fn publication_ready(&mut self, now: ChipInstant) -> Result<bool, CommonAmError> {
        self.rlc.publication_ready(now).map_err(CommonAmError::Rlc)
    }
    pub fn pending_receive_data(&self) -> bool {
        self.rlc.pending_receive_data()
    }
    pub fn isolated_sdu_range(
        &self,
        key: UeKey,
        bytes: usize,
    ) -> Result<umts_rlc::acknowledged::NewSduRange, CommonAmError> {
        self.origin(key)?;
        self.rlc
            .isolated_sdu_range(bytes)
            .map_err(CommonAmError::Rlc)
    }
}

fn rrc_sdu<'a>(buffer: &'a mut [u8], encoded: Bits<'_>) -> Result<&'a [u8], CommonAmError> {
    if encoded.is_empty() {
        return Err(CommonAmError::Payload);
    }
    let bytes = encoded.len().div_ceil(BITS_PER_BYTE);
    let output = buffer.get_mut(..bytes).ok_or(CommonAmError::Payload)?;
    encoded
        .copy_to(output)
        .map_err(|_| CommonAmError::Payload)?;
    Ok(output)
}

#[cfg(test)]
struct PreparedFrame<'a> {
    source: &'a mut dyn CommonTxTarget,
    fach: &'a mut FachTransport,
    start: ChipInstant,
    bytes: &'a mut [u8],
}

#[cfg(test)]
impl PreparedFrame<'_> {
    #[cfg(test)]
    fn publish<E>(
        self,
        publication: Publication<'_>,
        prepare: impl FnOnce(Publication<'_>, &mut [u8]) -> Result<(), E>,
    ) -> Result<Option<StatusSubmission>, CommonAmPreparedError<E>> {
        prepare_payload(publication, self.bytes, prepare, |bytes| {
            self.fach.publish_tracked(self.source, self.start, bytes)
        })
        .map_err(|error| match error {
            AmPayloadPublishError::Bearer(error) => CommonAmPreparedError::Bearer(error),
            AmPayloadPublishError::Preparation(error) => CommonAmPreparedError::Preparation(error),
            AmPayloadPublishError::Admission(error) => CommonAmPreparedError::Bearer(error.into()),
        })
    }
}

fn prepare_payload<P, A>(
    publication: Publication<'_>,
    bytes: &mut [u8],
    prepare: impl FnOnce(Publication<'_>, &mut [u8]) -> Result<(), P>,
    admit: impl FnOnce(&[u8]) -> Result<crate::tx_evidence::TransportSubmission, A>,
) -> Result<Option<StatusSubmission>, AmPayloadPublishError<P, A>> {
    let original = publication.bytes;
    let status_coverage = publication.status_coverage;
    bytes.copy_from_slice(original);
    let clear = match publication.kind {
        PublicationKind::Data { .. } => AM_CLEAR_HEADER_BYTES,
        _ => original.len(),
    };
    prepare(publication, bytes).map_err(AmPayloadPublishError::Preparation)?;
    if bytes[..clear] != original[..clear] {
        return Err(AmPayloadPublishError::Bearer(CommonAmError::Payload));
    }
    let transport = admit(bytes).map_err(AmPayloadPublishError::Admission)?;
    Ok(status_coverage.map(|coverage| StatusSubmission {
        coverage,
        transport,
    }))
}

#[allow(clippy::too_many_arguments)]
fn publish_payload_mapped<P, A>(
    rlc: &mut AmEntity,
    bytes: &mut [u8],
    status: &mut Option<StatusSubmission>,
    now: ChipInstant,
    mut map_count: impl FnMut(u64) -> Result<u32, P>,
    prepare: impl FnOnce(Publication<'_>, &mut [u8]) -> Result<(), P>,
    admit: impl FnOnce(&[u8]) -> Result<crate::tx_evidence::TransportSubmission, A>,
) -> Result<Option<AmPublication>, AmPayloadPublishError<P, A>> {
    let result = rlc
        .publish_next_mapped(
            now,
            |position| map_count(position).map_err(AmPayloadPublishError::Preparation),
            |publication| prepare_payload(publication, bytes, prepare, admit),
        )
        .map_err(|error| match error {
            PublishError::Rlc(error) => AmPayloadPublishError::Bearer(CommonAmError::Rlc(error)),
            PublishError::Downstream(error) => error,
        })?;
    Ok(result.map(|(kind, submission)| {
        let status_coverage = submission.as_ref().map(|submission| submission.coverage);
        if submission.is_some() {
            *status = submission;
        }
        AmPublication {
            kind,
            status_coverage,
        }
    }))
}

#[cfg(test)]
type PublishedFrame = (PublicationKind, Option<StatusSubmission>);

#[cfg(test)]
fn prepared_result<E>(
    result: Result<Option<PublishedFrame>, PublishError<CommonAmPreparedError<E>>>,
) -> Result<Option<PublishedFrame>, CommonAmPreparedError<E>> {
    result.map_err(|error| match error {
        PublishError::Rlc(error) => CommonAmPreparedError::Bearer(CommonAmError::Rlc(error)),
        PublishError::Downstream(error) => error,
    })
}

fn log_downlink(key: UeKey, encoded: Bits<'_>) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let mut bytes = vec![0u8; encoded.len().div_ceil(8)];
    if encoded.copy_to(&mut bytes).is_err() {
        return;
    }
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in &bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    tracing::debug!(
        "RRC dedicated downlink: slot={} generation={} bits={} hex={hex}",
        key.slot,
        key.generation,
        encoded.len()
    );
}
