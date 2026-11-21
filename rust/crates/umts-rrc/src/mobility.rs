// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::establishment::{AssignedRntis, DomainStarts, TransactionId};
use rasn::types::FixedBitString;
use umts_rrc_codec::{CodecError, DecodedUplinkCommon, EncodedRrc, encode_downlink_common};
use umts_rrc_schema::{
    class_definitions::{
        DLCCCHMessage, DLCCCHMessageType, DLDCCHMessage, DLDCCHMessageType, ULCCCHMessageType,
    },
    information_elements as ie, pdu_definitions as pdu,
};

pub(crate) mod parked;
pub use parked::{
    ConfirmTarget, ParkedState, UraUpdate, UraUpdateCause, build_cell_update_confirm,
    build_park_reconfiguration, build_ura_update_confirm, cell_update_confirm_message,
    inspect_ura_update, park_reconfiguration_message, ura_update_confirm_message, ura_update_urnti,
};

const RNC_BITS: usize = 12;
const SRNTI_BITS: usize = 20;
const CRNTI_BITS: usize = 16;
const MINIMUM_RELEASE_RETRANSMISSIONS: u8 = 1;
const MAXIMUM_RELEASE_RETRANSMISSIONS: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReleaseCompleteRetries(u8);

impl ReleaseCompleteRetries {
    pub fn new(value: u8) -> Result<Self, MobilityError> {
        (MINIMUM_RELEASE_RETRANSMISSIONS..=MAXIMUM_RELEASE_RETRANSMISSIONS)
            .contains(&value)
            .then_some(Self(value))
            .ok_or(MobilityError::Message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseConnectionState {
    CellFach,
    CellDch(ReleaseCompleteRetries),
}

pub fn build_inactivity_release(
    transaction: TransactionId,
    state: ReleaseConnectionState,
) -> DLDCCHMessage {
    build_dcch_release(transaction, state, ie::ReleaseCause::userInactivity)
}

pub fn build_transition_failure_release(
    transaction: TransactionId,
    state: ReleaseConnectionState,
) -> DLDCCHMessage {
    build_dcch_release(transaction, state, ie::ReleaseCause::unspecified)
}

fn build_dcch_release(
    transaction: TransactionId,
    state: ReleaseConnectionState,
    cause: ie::ReleaseCause,
) -> DLDCCHMessage {
    // TS 25.331 10.2.37 makes N308 mandatory in CELL_DCH despite ASN.1 OPTIONAL.
    let n308 = match state {
        ReleaseConnectionState::CellFach => None,
        ReleaseConnectionState::CellDch(retries) => Some(ie::N308(retries.0)),
    };
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::rrcConnectionRelease(pdu::RRCConnectionRelease::r3(
            pdu::RRCConnectionReleaseR3::new(
                pdu::RRCConnectionReleaseR3IEs::new(
                    ie::RRCTransactionIdentifier(transaction.get()),
                    n308,
                    cause,
                    None,
                ),
                None,
            ),
        )),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateCause {
    CellReselection,
    Periodic,
    UplinkData,
    PagingResponse,
    ReenteredService,
    RadioLinkFailure,
    RlcUnrecoverable,
    Extended,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateFailureCause {
    ConfigurationUnsupported,
    PhysicalChannelFailure,
    IncompatibleSimultaneousReconfiguration,
    ProtocolError,
    CellUpdateOccurred,
    InvalidConfiguration,
    ConfigurationIncomplete,
    Other,
}
/// TS 25.331 8.3.1.3: the transaction is that of the CELL UPDATE CONFIRM the UE could not apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateFailure {
    pub transaction: u8,
    pub cause: UpdateFailureCause,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellUpdate {
    pub urnti: u32,
    pub starts: DomainStarts,
    pub cause: UpdateCause,
    pub signalling_rlc_error: bool,
    pub user_rlc_error: bool,
    pub failure: Option<UpdateFailure>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MobilityError {
    Message,
    Identity,
    Starts,
    DrxCoefficient,
    Codec(CodecError),
}

pub fn inspect_cell_update(
    assigned: AssignedRntis,
    decoded: &DecodedUplinkCommon,
) -> Result<CellUpdate, MobilityError> {
    let ULCCCHMessageType::cellUpdate(message) = &decoded.value.message else {
        return Err(MobilityError::Message);
    };
    let urnti = cell_update_urnti(decoded)?;
    if urnti != assigned.urnti() {
        return Err(MobilityError::Identity);
    }
    let starts = DomainStarts::from_wire(&message.start_list).map_err(|_| MobilityError::Starts)?;
    let cause = match message.cell_update_cause {
        ie::CellUpdateCause::cellReselection => UpdateCause::CellReselection,
        ie::CellUpdateCause::periodicalCellUpdate => UpdateCause::Periodic,
        ie::CellUpdateCause::uplinkDataTransmission => UpdateCause::UplinkData,
        ie::CellUpdateCause::utran_pagingResponse => UpdateCause::PagingResponse,
        ie::CellUpdateCause::re_enteredServiceArea => UpdateCause::ReenteredService,
        ie::CellUpdateCause::radiolinkFailure => UpdateCause::RadioLinkFailure,
        ie::CellUpdateCause::rlc_unrecoverableError => UpdateCause::RlcUnrecoverable,
        ie::CellUpdateCause::cellUpdateCause_ext => UpdateCause::Extended,
    };
    Ok(CellUpdate {
        urnti,
        starts,
        cause,
        signalling_rlc_error: message.am_rlc_error_indication_rb2_3or4,
        user_rlc_error: message.am_rlc_error_indication_rb5or_above,
        failure: message.failure_cause.as_ref().map(|failure| UpdateFailure {
            transaction: failure.rrc_transaction_identifier.0,
            cause: match failure.failure_cause {
                ie::FailureCauseWithProtErr::configurationUnsupported(()) => {
                    UpdateFailureCause::ConfigurationUnsupported
                }
                ie::FailureCauseWithProtErr::physicalChannelFailure(()) => {
                    UpdateFailureCause::PhysicalChannelFailure
                }
                ie::FailureCauseWithProtErr::incompatibleSimultaneousReconfiguration(()) => {
                    UpdateFailureCause::IncompatibleSimultaneousReconfiguration
                }
                ie::FailureCauseWithProtErr::protocolError(_) => UpdateFailureCause::ProtocolError,
                ie::FailureCauseWithProtErr::cellUpdateOccurred(()) => {
                    UpdateFailureCause::CellUpdateOccurred
                }
                ie::FailureCauseWithProtErr::invalidConfiguration(()) => {
                    UpdateFailureCause::InvalidConfiguration
                }
                ie::FailureCauseWithProtErr::configurationIncomplete(()) => {
                    UpdateFailureCause::ConfigurationIncomplete
                }
                _ => UpdateFailureCause::Other,
            },
        }),
    })
}

pub fn cell_update_urnti(decoded: &DecodedUplinkCommon) -> Result<u32, MobilityError> {
    let ULCCCHMessageType::cellUpdate(message) = &decoded.value.message else {
        return Err(MobilityError::Message);
    };
    Ok(urnti_word(&message.u_rnti))
}

pub(crate) fn urnti_wire(urnti: u32) -> ie::URNTI {
    ie::URNTI::new(
        ie::SRNCIdentity(fixed::<RNC_BITS>(urnti >> SRNTI_BITS)),
        ie::SRNTI(fixed::<SRNTI_BITS>(urnti)),
    )
}

pub(crate) fn urnti_word(identity: &ie::URNTI) -> u32 {
    (word(&identity.srnc_identity.0) << SRNTI_BITS) | word(&identity.s_rnti.0)
}

pub fn build_signalling_failure_release(
    assigned: AssignedRntis,
    transaction: TransactionId,
) -> Result<EncodedRrc, MobilityError> {
    build_release_for_urnti(assigned.urnti(), transaction)
}

pub fn build_unknown_context_release(
    urnti: u32,
    transaction: TransactionId,
) -> Result<EncodedRrc, MobilityError> {
    build_release_for_urnti(urnti, transaction)
}

/// TS 25.331 8.3.1.5: a CELL UPDATE from a UE staying on common channels is answered with
/// CELL UPDATE CONFIRM. The confirm keeps the UE in CELL_FACH and includes no reconfiguration,
/// so 8.3.1.7 requires no response unless a new C-RNTI is included.
pub fn build_cell_update_confirm_ccch(
    urnti: u32,
    transaction: TransactionId,
    new_crnti: Option<u16>,
) -> Result<EncodedRrc, MobilityError> {
    let identity = urnti_wire(urnti);
    let message = DLCCCHMessage::new(
        None,
        DLCCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirmCCCH::r3(
            pdu::CellUpdateConfirmCCCHR3::new(
                identity,
                parked::confirm_ies(transaction, ConfirmTarget::Fach { new_crnti })?,
                None,
            ),
        )),
    );
    encode_downlink_common(&message).map_err(MobilityError::Codec)
}

/// TS 25.331 8.3.1.5: the DCCH form is required once ciphering is configured, because the
/// CCCH form is allowed "only if ciphering is not required".
pub fn build_cell_update_confirm_dcch(
    transaction: TransactionId,
    new_crnti: Option<u16>,
) -> DLDCCHMessage {
    parked::confirm_message(parked::fach_confirm_ies(transaction, new_crnti))
}

fn build_release_for_urnti(
    urnti: u32,
    transaction: TransactionId,
) -> Result<EncodedRrc, MobilityError> {
    let identity = urnti_wire(urnti);
    let message = DLCCCHMessage::new(
        None,
        DLCCCHMessageType::rrcConnectionRelease(pdu::RRCConnectionReleaseCCCH::r3(
            pdu::RRCConnectionReleaseCCCHR3::new(
                pdu::RRCConnectionReleaseCCCHR3IEs::new(
                    identity,
                    pdu::RRCConnectionReleaseR3IEs::new(
                        ie::RRCTransactionIdentifier(transaction.get()),
                        None,
                        ie::ReleaseCause::re_establishmentReject,
                        None,
                    ),
                ),
                None,
            ),
        )),
    );
    encode_downlink_common(&message).map_err(MobilityError::Codec)
}

fn word<const N: usize>(bits: &FixedBitString<N>) -> u32 {
    bits.iter()
        .take(N)
        .fold(0, |value, bit| (value << 1) | u32::from(*bit))
}
fn fixed<const N: usize>(value: u32) -> FixedBitString<N> {
    let mut result = FixedBitString::default();
    for index in 0..N {
        result.set(index, value & (1 << (N - 1 - index)) != 0);
    }
    result
}

#[cfg(test)]
mod tests;
