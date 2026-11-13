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
pub struct CellUpdate {
    pub urnti: u32,
    pub starts: DomainStarts,
    pub cause: UpdateCause,
    pub signalling_rlc_error: bool,
    pub user_rlc_error: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MobilityError {
    Message,
    Identity,
    Starts,
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
    })
}

pub fn cell_update_urnti(decoded: &DecodedUplinkCommon) -> Result<u32, MobilityError> {
    let ULCCCHMessageType::cellUpdate(message) = &decoded.value.message else {
        return Err(MobilityError::Message);
    };
    Ok((word(&message.u_rnti.srnc_identity.0) << SRNTI_BITS) | word(&message.u_rnti.s_rnti.0))
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
    let identity = ie::URNTI::new(
        ie::SRNCIdentity(fixed::<RNC_BITS>(urnti >> SRNTI_BITS)),
        ie::SRNTI(fixed::<SRNTI_BITS>(urnti)),
    );
    let message = DLCCCHMessage::new(
        None,
        DLCCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirmCCCH::r3(
            pdu::CellUpdateConfirmCCCHR3::new(
                identity,
                cell_update_confirm_ies(transaction, new_crnti),
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
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirm::r3(
            pdu::CellUpdateConfirmR3::new(cell_update_confirm_ies(transaction, new_crnti), None),
        )),
    )
}

fn cell_update_confirm_ies(
    transaction: TransactionId,
    new_crnti: Option<u16>,
) -> pdu::CellUpdateConfirmR3IEs {
    pdu::CellUpdateConfirmR3IEs {
        rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        activation_time: None,
        new_u_rnti: None,
        new_c_rnti: new_crnti.map(|value| ie::CRNTI(fixed::<CRNTI_BITS>(u32::from(value)))),
        rrc_state_indicator: ie::RRCStateIndicator::cell_FACH,
        utran_drx_cycle_length_coeff: None,
        rlc_re_establish_indicator_rb2_3or4: false,
        rlc_re_establish_indicator_rb5or_above: false,
        cn_information_info: None,
        ura_identity: None,
        rb_information_release_list: None,
        rb_information_reconfig_list: None,
        rb_information_affected_list: None,
        dl_counter_synchronisation_info: None,
        ul_common_trans_ch_info: None,
        ul_deleted_trans_ch_info_list: None,
        ul_add_reconf_trans_ch_info_list: None,
        mode_specific_trans_ch_info: pdu::CellUpdateConfirmR3IEsModeSpecificTransChInfo::fdd(
            pdu::CellUpdateConfirmR3IEsModeSpecificTransChInfoFdd::new(None, None),
        ),
        dl_common_trans_ch_info: None,
        dl_deleted_trans_ch_info_list: None,
        dl_add_reconf_trans_ch_info_list: None,
        frequency_info: None,
        max_allowed_ul_tx_power: None,
        ul_channel_requirement: None,
        mode_specific_phys_ch_info: pdu::CellUpdateConfirmR3IEsModeSpecificPhysChInfo::fdd(
            pdu::CellUpdateConfirmR3IEsModeSpecificPhysChInfoFdd::new(None),
        ),
        dl_common_information: None,
        dl_information_per_rl_list: None,
    }
}

fn build_release_for_urnti(
    urnti: u32,
    transaction: TransactionId,
) -> Result<EncodedRrc, MobilityError> {
    let identity = ie::URNTI::new(
        ie::SRNCIdentity(fixed::<RNC_BITS>(urnti >> SRNTI_BITS)),
        ie::SRNTI(fixed::<SRNTI_BITS>(urnti)),
    );
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
