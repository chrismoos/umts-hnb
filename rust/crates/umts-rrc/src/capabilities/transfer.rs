// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::{NonZeroU64, NonZeroUsize};

use rasn::types::FixedBitString;
use umts_rrc_codec::{
    CodecError, DecodedUplinkDedicated, EncodedRrc, MAX_DEDICATED_BITS, decode_uplink_dedicated,
    encode_downlink_dedicated,
};
use umts_rrc_schema::{
    class_definitions::{DLDCCHMessage, DLDCCHMessageType, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{UeKey, time::ChipInstant};

use crate::{
    establishment::TransactionId,
    integrity::{AcceptedUplink, DownlinkIntegrity, StreamError},
    security::Capabilities,
};

const CONTROL_SRB: u8 = 2;
const OCTET_BITS: usize = u8::BITS as usize;
const GSM_CLASSMARK2_IE: u8 = 0x33;
const GSM_CLASSMARK2_CONTENT_BYTES: u8 = 3;
const GSM_CLASSMARK2_BYTES: usize = 5;
const GSM_CLASSMARK3_MAX_BYTES: usize = 32;
const GSM_A51_UNSUPPORTED: u8 = 1 << 3;
const GSM_A52_SUPPORTED: u8 = 1;
const GSM_A53_SUPPORTED: u8 = 1 << 1;
const GSM_MULTIBAND_MASK: u8 = 0x70;
const GSM_MULTIBAND_SHIFT: u32 = 4;
const GSM_ALGORITHMS: usize = 7;
const SECURITY_ALGORITHMS: usize = 16;
const RETAINED_REPORTS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExchangePhase {
    Idle,
    AwaitingInformation,
    Confirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveResult {
    Confirmed,
    DuplicateConfirmed,
    ChangeRefused,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExchangeError<E> {
    Origin,
    Time,
    Phase,
    Expired,
    Message,
    Length,
    Transaction,
    MissingRadioCapability,
    InterRat,
    ProtectionRequired,
    Codec(CodecError),
    Security(crate::security::Error),
    Integrity(StreamError<E>),
    Downstream(E),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityReport {
    key: UeKey,
    information: Box<[u8]>,
    security: Capabilities,
    hsdpa: super::hsdpa::FddHsdpa,
    hsdpa_qam64: Option<umts_types::hsdpa::Category>,
    hsdpa_dual_cell: Option<super::hsdpa::DualCellHsdpa>,
    adjacent_measurements_without_gaps: bool,
    hsupa: super::hsupa::FddHsupa,
    simultaneous_dch: Option<super::hsdpa::SimultaneousDch>,
}

impl CapabilityReport {
    pub fn adjacent_measurements_without_gaps(&self) -> bool {
        self.adjacent_measurements_without_gaps
    }
    pub fn hsupa(&self) -> super::hsupa::FddHsupa {
        self.hsupa
    }
    pub fn simultaneous_dch(&self) -> Option<super::hsdpa::SimultaneousDch> {
        self.simultaneous_dch
    }
    pub fn hsdpa(&self) -> super::hsdpa::FddHsdpa {
        self.hsdpa
    }

    pub fn hsdpa_qam64(&self) -> Option<umts_types::hsdpa::Category> {
        self.hsdpa_qam64
    }

    pub fn hsdpa_dual_cell(&self) -> Option<super::hsdpa::DualCellHsdpa> {
        self.hsdpa_dual_cell
    }

    pub fn key(&self) -> UeKey {
        self.key
    }
    pub fn encoded_information(&self) -> &[u8] {
        &self.information
    }
    pub fn information(&self) -> Result<Box<pdu::UECapabilityInformation>, CodecError> {
        rasn::uper::decode(&self.information)
            .map(Box::new)
            .map_err(|_| CodecError::Decode)
    }
    pub fn security(&self) -> &Capabilities {
        &self.security
    }

    fn from_information<E>(
        key: UeKey,
        information: &pdu::UECapabilityInformation,
        maximum_bytes: usize,
    ) -> Result<Self, ExchangeError<E>> {
        let radio = information
            .ue_radio_access_capability
            .as_ref()
            .ok_or(ExchangeError::MissingRadioCapability)?;
        let security = security_from_radio(radio, information.ue_ratspecific_capability.as_ref())?;
        let hsdpa = super::hsdpa::from_information(information);
        let hsdpa_qam64 = super::hsdpa::qam64_from_information(information);
        let hsdpa_dual_cell = super::hsdpa::dual_cell_from_information(information);
        let adjacent_measurements_without_gaps =
            super::hsdpa::adjacent_measurements_without_gaps(information);
        let hsupa = super::hsupa::from_information(information);
        let simultaneous_dch = super::hsdpa::simultaneous_dch(information);
        let information = rasn::uper::encode(information)
            .map_err(|_| ExchangeError::Codec(CodecError::Encode))?;
        if information.len() > maximum_bytes {
            return Err(ExchangeError::Length);
        }
        Ok(Self {
            key,
            information: information.into_boxed_slice(),
            security,
            hsdpa,
            hsdpa_qam64,
            hsdpa_dual_cell,
            adjacent_measurements_without_gaps,
            hsupa,
            simultaneous_dch,
        })
    }
}

pub fn setup_security_capabilities(
    completion: &pdu::RRCConnectionSetupComplete,
) -> Result<Option<Capabilities>, ExchangeError<()>> {
    completion
        .ue_radio_access_capability
        .as_ref()
        .map(|radio| security_from_radio(radio, completion.ue_ratspecific_capability.as_ref()))
        .transpose()
}

fn security_from_radio<E>(
    radio: &ie::UERadioAccessCapability,
    inter_rat: Option<&ie::InterRATUERadioAccessCapabilityList>,
) -> Result<Capabilities, ExchangeError<E>> {
    let mut gsm = None;
    if let Some(inter_rat) = inter_rat {
        for capability in &inter_rat.0 {
            if let ie::InterRATUERadioAccessCapability::gsm(value) = capability {
                if gsm.is_some() {
                    return Err(ExchangeError::InterRat);
                }
                gsm = Some(gsm_security(value)?);
            }
        }
    }
    Capabilities::new(
        radio.security_capability.clone(),
        gsm.map(|value| ie::InterRATUESecurityCapList(vec![value])),
    )
    .map_err(ExchangeError::Security)
}

fn gsm_security<E>(
    value: &ie::InterRATUERadioAccessCapabilityGsm,
) -> Result<ie::InterRATUESecurityCapability, ExchangeError<E>> {
    let class2: &[u8] = value.gsm_classmark2.0.as_ref();
    let class3: &[u8] = value.gsm_classmark3.0.as_ref();
    if class2.len() != GSM_CLASSMARK2_BYTES
        || class2[0] != GSM_CLASSMARK2_IE
        || class2[1] != GSM_CLASSMARK2_CONTENT_BYTES
        || class3.is_empty()
        || class3.len() > GSM_CLASSMARK3_MAX_BYTES
    {
        return Err(ExchangeError::InterRat);
    }
    let multiband = (class3[0] & GSM_MULTIBAND_MASK) >> GSM_MULTIBAND_SHIFT;
    if !matches!(multiband, 0..=2 | 4..=6) {
        return Err(ExchangeError::InterRat);
    }
    // TS 24.008 10.5.1.6 inverts A5/1. TS 25.331's Classmark 2 includes the TLV header.
    let mut bits = FixedBitString::<GSM_ALGORITHMS>::default();
    bits.set(6, class2[2] & GSM_A51_UNSUPPORTED == 0);
    bits.set(5, class2[4] & GSM_A52_SUPPORTED != 0);
    bits.set(4, class2[4] & GSM_A53_SUPPORTED != 0);
    // TS 24.008 10.5.1.7 places A5/7 through A5/4 immediately after the multiband field.
    for index in 0..4 {
        bits.set(index, class3[0] & (1 << (3 - index)) != 0);
    }
    Ok(ie::InterRATUESecurityCapability::gsm(
        ie::InterRATUESecurityCapabilityGsm::new(ie::GsmSecurityCapability(bits)),
    ))
}

#[derive(Clone, Copy)]
struct Pending {
    transaction: TransactionId,
    deadline: ChipInstant,
}

pub struct CapabilityExchange {
    key: UeKey,
    changed_at: ChipInstant,
    pending: Option<Pending>,
    report: Option<CapabilityReport>,
    integrity_required: bool,
    maximum_information_bytes: NonZeroUsize,
}

impl CapabilityExchange {
    pub fn new(key: UeKey, now: ChipInstant) -> Result<Self, ExchangeError<()>> {
        Self::with_limit(
            key,
            now,
            NonZeroUsize::new(MAX_DEDICATED_BITS / OCTET_BITS).expect("nonzero codec limit"),
        )
    }
    pub fn with_limit(
        key: UeKey,
        now: ChipInstant,
        maximum_information_bytes: NonZeroUsize,
    ) -> Result<Self, ExchangeError<()>> {
        if key.epoch != now.epoch {
            return Err(ExchangeError::Origin);
        }
        if maximum_information_bytes.get() > MAX_DEDICATED_BITS / OCTET_BITS {
            return Err(ExchangeError::Length);
        }
        Ok(Self {
            key,
            changed_at: now,
            pending: None,
            report: None,
            integrity_required: false,
            maximum_information_bytes,
        })
    }
    pub fn buffer_bytes(maximum_information_bytes: NonZeroUsize) -> Option<usize> {
        if maximum_information_bytes.get() > MAX_DEDICATED_BITS / OCTET_BITS {
            return None;
        }
        let summary_bytes = std::mem::size_of::<Capabilities>()
            + std::mem::size_of::<ie::InterRATUESecurityCapability>()
            + 2 * SECURITY_ALGORITHMS.div_ceil(OCTET_BITS)
            + GSM_ALGORITHMS.div_ceil(OCTET_BITS);
        maximum_information_bytes
            .get()
            .checked_add(summary_bytes)?
            .checked_mul(RETAINED_REPORTS)
    }
    pub fn phase(&self) -> ExchangePhase {
        if self.pending.is_some() {
            ExchangePhase::AwaitingInformation
        } else if self.report.is_some() {
            ExchangePhase::Confirmed
        } else {
            ExchangePhase::Idle
        }
    }
    pub fn report(&self) -> Option<&CapabilityReport> {
        self.report.as_ref()
    }
    pub fn require_integrity(&mut self) {
        self.integrity_required = true;
    }
    pub fn pending_transaction(&self) -> Option<TransactionId> {
        self.pending.map(|pending| pending.transaction)
    }
    pub fn publish_enquiry_with<T, E>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        transaction: TransactionId,
        guard_chips: NonZeroU64,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ExchangeError<E>> {
        if self.integrity_required {
            return Err(ExchangeError::ProtectionRequired);
        }
        self.publish_enquiry(key, now, transaction, guard_chips, |message| {
            let encoded = encode_downlink_dedicated(message).map_err(ExchangeError::Codec)?;
            publish(&encoded).map_err(ExchangeError::Downstream)
        })
    }
    pub fn publish_authenticated_enquiry_with<T, E>(
        &mut self,
        now: ChipInstant,
        transaction: TransactionId,
        guard_chips: NonZeroU64,
        integrity: &mut DownlinkIntegrity,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ExchangeError<E>> {
        self.signer(integrity)?;
        let result = self.publish_enquiry(self.key, now, transaction, guard_chips, |message| {
            integrity
                .publish_with(integrity.origin(), message, publish)
                .map_err(ExchangeError::Integrity)
        })?;
        self.integrity_required = true;
        Ok(result)
    }
    fn publish_enquiry<T, E>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        transaction: TransactionId,
        guard_chips: NonZeroU64,
        publish: impl FnOnce(&DLDCCHMessage) -> Result<T, ExchangeError<E>>,
    ) -> Result<T, ExchangeError<E>> {
        self.time(key, now)?;
        if self.pending.is_some() {
            return Err(ExchangeError::Phase);
        }
        let deadline = ChipInstant {
            epoch: now.epoch,
            index: now
                .index
                .checked_add(guard_chips.get())
                .ok_or(ExchangeError::Time)?,
        };
        let message = DLDCCHMessage::new(
            None,
            DLDCCHMessageType::ueCapabilityEnquiry(pdu::UECapabilityEnquiry::r3(
                pdu::UECapabilityEnquiryR3::new(
                    pdu::UECapabilityEnquiryR3IEs::new(
                        ie::RRCTransactionIdentifier(transaction.get()),
                        ie::CapabilityUpdateRequirement::new(
                            true,
                            false,
                            Some(ie::SystemSpecificCapUpdateReqList(vec![
                                ie::SystemSpecificCapUpdateReq::gsm,
                            ])),
                        ),
                    ),
                    None,
                ),
            )),
        );
        let result = publish(&message)?;
        self.pending = Some(Pending {
            transaction,
            deadline,
        });
        self.changed_at = now;
        Ok(result)
    }

    pub fn receive_with<T, E>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        bearer: u8,
        message: &DecodedUplinkDedicated,
        publish_confirm: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<(ReceiveResult, T), ExchangeError<E>> {
        if self.integrity_required || message.value.integrity_check_info.is_some() {
            return Err(ExchangeError::ProtectionRequired);
        }
        if bearer != CONTROL_SRB {
            return Err(ExchangeError::Message);
        }
        self.receive(key, now, message, |message| {
            let encoded = encode_downlink_dedicated(message).map_err(ExchangeError::Codec)?;
            publish_confirm(&encoded).map_err(ExchangeError::Downstream)
        })
    }
    pub fn receive_authenticated_with<T, E>(
        &mut self,
        now: ChipInstant,
        accepted: AcceptedUplink<'_>,
        integrity: &mut DownlinkIntegrity,
        publish_confirm: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<(ReceiveResult, T), ExchangeError<E>> {
        if accepted.origin().ue != self.key || accepted.bearer() != CONTROL_SRB {
            return Err(ExchangeError::Origin);
        }
        self.signer(integrity)?;
        let decoded =
            decode_uplink_dedicated(accepted.bytes(), accepted.bytes().len() * OCTET_BITS)
                .map_err(ExchangeError::Codec)?;
        let result = self.receive(self.key, now, &decoded, |message| {
            integrity
                .publish_with(integrity.origin(), message, publish_confirm)
                .map_err(ExchangeError::Integrity)
        })?;
        self.integrity_required = true;
        Ok(result)
    }
    fn receive<T, E>(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        message: &DecodedUplinkDedicated,
        publish_confirm: impl FnOnce(&DLDCCHMessage) -> Result<T, ExchangeError<E>>,
    ) -> Result<(ReceiveResult, T), ExchangeError<E>> {
        self.time(key, now)?;
        let ULDCCHMessageType::ueCapabilityInformation(information) = &message.value.message else {
            return Err(ExchangeError::Message);
        };
        let transaction = information
            .rrc_transaction_identifier
            .as_ref()
            .map(|identifier| identifier.0)
            .ok_or(ExchangeError::Transaction)?;
        if message.consumed_bits > self.maximum_information_bytes.get() * OCTET_BITS {
            return Err(ExchangeError::Length);
        }
        if let Some(pending) = self.pending {
            if now.index >= pending.deadline.index {
                return Err(ExchangeError::Expired);
            }
            if transaction != pending.transaction.get() {
                return Err(ExchangeError::Transaction);
            }
        }
        let report = CapabilityReport::from_information(
            key,
            information,
            self.maximum_information_bytes.get(),
        )?;
        if self.pending.is_none() && self.report.is_none() {
            return Err(ExchangeError::Transaction);
        }
        let unchanged = if let Some(previous) = &self.report {
            let mut previous = previous.information().map_err(ExchangeError::Codec)?;
            previous.rrc_transaction_identifier = information.rrc_transaction_identifier.clone();
            previous.as_ref() == information
        } else {
            false
        };
        // TS 25.331 8.1.6.3: refuse unsupported changes without replacing active admission facts.
        let duplicate = self.pending.is_none() && unchanged;
        let refused = self.report.is_some() && !unchanged;
        let extension = refused.then(|| {
            pdu::UECapabilityInformationConfirmR3LaterNonCriticalExtensions::new(
                None,
                Some(pdu::UECapabilityInformationConfirmR3LaterNonCriticalExtensionsV770NonCriticalExtensions::new(
                    pdu::UECapabilityInformationConfirmV770extIEs::new(Some(
                        pdu::UECapabilityInformationConfirmV770extIEsAcceptanceOfChangeOfCapability::refused,
                    )),
                    None,
                )),
            )
        });
        let confirm = DLDCCHMessage::new(
            None,
            DLDCCHMessageType::ueCapabilityInformationConfirm(
                pdu::UECapabilityInformationConfirm::r3(
                    pdu::UECapabilityInformationConfirmR3::new(
                        pdu::UECapabilityInformationConfirmR3IEs::new(
                            ie::RRCTransactionIdentifier(transaction),
                        ),
                        extension,
                    ),
                ),
            ),
        );
        let result = publish_confirm(&confirm)?;
        if !duplicate && !refused {
            self.report = Some(report);
        }
        self.pending = None;
        self.changed_at = now;
        Ok((
            if refused {
                ReceiveResult::ChangeRefused
            } else if duplicate {
                ReceiveResult::DuplicateConfirmed
            } else {
                ReceiveResult::Confirmed
            },
            result,
        ))
    }
    pub fn expire(&mut self, key: UeKey, now: ChipInstant) -> Result<bool, ExchangeError<()>> {
        self.time(key, now)?;
        if self
            .pending
            .is_none_or(|pending| now.index < pending.deadline.index)
        {
            return Ok(false);
        }
        self.pending = None;
        self.changed_at = now;
        Ok(true)
    }
    fn signer<E>(&self, integrity: &DownlinkIntegrity) -> Result<(), ExchangeError<E>> {
        if integrity.origin().ue != self.key || integrity.bearer() != CONTROL_SRB {
            return Err(ExchangeError::Origin);
        }
        Ok(())
    }
    fn time<E>(&self, key: UeKey, now: ChipInstant) -> Result<(), ExchangeError<E>> {
        if key != self.key || now.epoch != self.key.epoch {
            return Err(ExchangeError::Origin);
        }
        if now.index < self.changed_at.index {
            return Err(ExchangeError::Time);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
