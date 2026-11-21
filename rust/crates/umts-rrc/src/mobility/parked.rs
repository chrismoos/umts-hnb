// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CRNTI_BITS, MobilityError, fixed, urnti_word};
use crate::establishment::{AssignedRntis, TransactionId};
use umts_rrc_codec::{DecodedUplinkCommon, EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions::{DLDCCHMessage, DLDCCHMessageType, ULCCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};

// TS 25.331 10.3.3.49: UTRAN DRX cycle length coefficient, cycle of 2^k frames.
const MINIMUM_DRX_COEFFICIENT: u8 = 3;
const MAXIMUM_DRX_COEFFICIENT: u8 = 9;
const URA_IDENTITY_BITS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkedState {
    CellPch,
    /// TS 25.331 8.6.2.1: the identity must be one of those broadcast in SIB2.
    UraPch {
        identity: u16,
    },
}

impl ParkedState {
    fn indicator(self) -> ie::RRCStateIndicator {
        match self {
            Self::CellPch => ie::RRCStateIndicator::cell_PCH,
            Self::UraPch { .. } => ie::RRCStateIndicator::ura_PCH,
        }
    }

    fn ura_identity(self) -> Option<ie::URAIdentity> {
        match self {
            Self::CellPch => None,
            Self::UraPch { identity } => Some(ie::URAIdentity(fixed::<URA_IDENTITY_BITS>(
                u32::from(identity),
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfirmTarget {
    /// TS 25.331 8.3.1.6: a UE with no C-RNTI that stays in CELL_FACH repeats the cell update.
    Fach { new_crnti: Option<u16> },
    /// CELL_FACH for a UE whose packet bearers are about to return to CELL_DCH. TS 25.331
    /// 8.3.1.6: both ends restart the RLC entities of bearers 5 and above with the HFN set
    /// to the START value of the CELL UPDATE.
    Wake { crnti: u16 },
    /// TS 25.331 8.3.1.7: this confirm requires no response from the UE.
    Parked {
        state: ParkedState,
        drx_coefficient: u8,
    },
}

fn drx(coefficient: u8) -> Result<ie::UTRANDRXCycleLengthCoefficient, MobilityError> {
    (MINIMUM_DRX_COEFFICIENT..=MAXIMUM_DRX_COEFFICIENT)
        .contains(&coefficient)
        .then_some(ie::UTRANDRXCycleLengthCoefficient(coefficient))
        .ok_or(MobilityError::DrxCoefficient)
}

/// TS 25.331 8.2.2.3: a target of CELL_PCH or URA_PCH requires the DRX coefficient. The UE
/// answers PHYSICAL CHANNEL RECONFIGURATION COMPLETE in the old configuration and changes
/// state once RLC has acknowledged it.
pub fn park_reconfiguration_message(
    transaction: TransactionId,
    state: ParkedState,
    drx_coefficient: u8,
) -> Result<DLDCCHMessage, MobilityError> {
    let ies = pdu::PhysicalChannelReconfigurationR3IEs {
        rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        activation_time: None,
        new_u_rnti: None,
        new_c_rnti: None,
        rrc_state_indicator: state.indicator(),
        utran_drx_cycle_length_coeff: Some(drx(drx_coefficient)?),
        cn_information_info: None,
        ura_identity: state.ura_identity(),
        dl_counter_synchronisation_info: None,
        frequency_info: None,
        max_allowed_ul_tx_power: None,
        ul_channel_requirement: None,
        mode_specific_info: pdu::PhysicalChannelReconfigurationR3IEsModeSpecificInfo::fdd(
            pdu::PhysicalChannelReconfigurationR3IEsModeSpecificInfoFdd::new(None),
        ),
        dl_common_information: None,
        dl_information_per_rl_list: None,
    };
    Ok(DLDCCHMessage::new(
        None,
        DLDCCHMessageType::physicalChannelReconfiguration(pdu::PhysicalChannelReconfiguration::r3(
            pdu::PhysicalChannelReconfigurationR3::new(ies, None),
        )),
    ))
}

pub fn build_park_reconfiguration(
    transaction: TransactionId,
    state: ParkedState,
    drx_coefficient: u8,
) -> Result<EncodedRrc, MobilityError> {
    encode(&park_reconfiguration_message(
        transaction,
        state,
        drx_coefficient,
    )?)
}

pub fn cell_update_confirm_message(
    transaction: TransactionId,
    target: ConfirmTarget,
) -> Result<DLDCCHMessage, MobilityError> {
    Ok(confirm_message(confirm_ies(transaction, target)?))
}

pub fn build_cell_update_confirm(
    transaction: TransactionId,
    target: ConfirmTarget,
) -> Result<EncodedRrc, MobilityError> {
    encode(&cell_update_confirm_message(transaction, target)?)
}

pub(crate) fn confirm_message(ies: pdu::CellUpdateConfirmR3IEs) -> DLDCCHMessage {
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirm::r3(
            pdu::CellUpdateConfirmR3::new(ies, None),
        )),
    )
}

pub(super) fn confirm_ies(
    transaction: TransactionId,
    target: ConfirmTarget,
) -> Result<pdu::CellUpdateConfirmR3IEs, MobilityError> {
    match target {
        ConfirmTarget::Fach { new_crnti } => Ok(fach_confirm_ies(transaction, new_crnti)),
        ConfirmTarget::Wake { crnti } => {
            let mut ies = fach_confirm_ies(transaction, Some(crnti));
            ies.rlc_re_establish_indicator_rb5or_above = true;
            Ok(ies)
        }
        ConfirmTarget::Parked {
            state,
            drx_coefficient,
        } => {
            let mut ies = empty_confirm_ies(transaction, state.indicator());
            ies.utran_drx_cycle_length_coeff = Some(drx(drx_coefficient)?);
            ies.ura_identity = state.ura_identity();
            Ok(ies)
        }
    }
}

pub(super) fn fach_confirm_ies(
    transaction: TransactionId,
    new_crnti: Option<u16>,
) -> pdu::CellUpdateConfirmR3IEs {
    let mut ies = empty_confirm_ies(transaction, ie::RRCStateIndicator::cell_FACH);
    ies.new_c_rnti = new_crnti.map(|value| ie::CRNTI(fixed::<CRNTI_BITS>(u32::from(value))));
    ies
}

pub(crate) fn empty_confirm_ies(
    transaction: TransactionId,
    state: ie::RRCStateIndicator,
) -> pdu::CellUpdateConfirmR3IEs {
    pdu::CellUpdateConfirmR3IEs {
        rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        activation_time: None,
        new_u_rnti: None,
        new_c_rnti: None,
        rrc_state_indicator: state,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UraUpdateCause {
    ChangeOfUra,
    Periodic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UraUpdate {
    pub urnti: u32,
    pub cause: UraUpdateCause,
    pub protocol_error: bool,
}

pub fn ura_update_urnti(decoded: &DecodedUplinkCommon) -> Result<u32, MobilityError> {
    let ULCCCHMessageType::uraUpdate(message) = &decoded.value.message else {
        return Err(MobilityError::Message);
    };
    Ok(urnti_word(&message.u_rnti))
}

pub fn inspect_ura_update(
    assigned: AssignedRntis,
    decoded: &DecodedUplinkCommon,
) -> Result<UraUpdate, MobilityError> {
    let ULCCCHMessageType::uraUpdate(message) = &decoded.value.message else {
        return Err(MobilityError::Message);
    };
    let urnti = urnti_word(&message.u_rnti);
    if urnti != assigned.urnti() {
        return Err(MobilityError::Identity);
    }
    let cause = match message.ura_update_cause {
        ie::URAUpdateCause::changeOfURA => UraUpdateCause::ChangeOfUra,
        ie::URAUpdateCause::periodicURAUpdate => UraUpdateCause::Periodic,
        ie::URAUpdateCause::dummy | ie::URAUpdateCause::spare1 => {
            return Err(MobilityError::Message);
        }
    };
    Ok(UraUpdate {
        urnti,
        cause,
        protocol_error: matches!(
            message.protocol_error_indicator,
            ie::ProtocolErrorIndicatorWithMoreInfo::errorOccurred(_)
        ),
    })
}

/// TS 25.331 8.3.1.7: this confirm requires no response from the UE.
pub fn ura_update_confirm_message(
    transaction: TransactionId,
    state: ParkedState,
    drx_coefficient: u8,
) -> Result<DLDCCHMessage, MobilityError> {
    let ies = pdu::URAUpdateConfirmR3IEs {
        rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        new_u_rnti: None,
        new_c_rnti: None,
        rrc_state_indicator: state.indicator(),
        utran_drx_cycle_length_coeff: Some(drx(drx_coefficient)?),
        cn_information_info: None,
        ura_identity: state.ura_identity(),
        dl_counter_synchronisation_info: None,
    };
    Ok(DLDCCHMessage::new(
        None,
        DLDCCHMessageType::uraUpdateConfirm(pdu::URAUpdateConfirm::r3(
            pdu::URAUpdateConfirmR3::new(ies, None),
        )),
    ))
}

pub fn build_ura_update_confirm(
    transaction: TransactionId,
    state: ParkedState,
    drx_coefficient: u8,
) -> Result<EncodedRrc, MobilityError> {
    encode(&ura_update_confirm_message(
        transaction,
        state,
        drx_coefficient,
    )?)
}

fn encode(message: &DLDCCHMessage) -> Result<EncodedRrc, MobilityError> {
    encode_downlink_dedicated(message).map_err(MobilityError::Codec)
}

#[cfg(test)]
mod tests;
