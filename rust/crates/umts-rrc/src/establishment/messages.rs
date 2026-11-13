// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AssignedRntis, EstablishmentError, TransactionId};
use umts_rrc_codec::{EncodedRrc, encode_downlink_common};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};

const FIRST_SRB: u8 = 1;
const CONTROL_SRB: u8 = 2;
const NAS_SRB: u8 = 3;
/// Transport channel of the CELL_FACH setup's placeholder DCH, deleted by the first CELL_DCH setup.
pub const UNUSED_DCH_ID: u8 = 31;
const FIRST_PRACH_RLC_SIZE_INDEX: u8 = 1;
// 128 + 8 * 2 = 144-bit RLC size, as in the TS 34.108 default.
const DUMMY_RLC_SIZE_TYPE2: u8 = 2;
const DUMMY_RATE_MATCHING_ATTRIBUTE: u16 = 160;
const MIN_DRX_COEFFICIENT: u8 = 3;
const MAX_DRX_COEFFICIENT: u8 = 9;
const MAX_WAIT_TIME_CODE: u8 = 15;
const PEER_STATUS_PROHIBIT: (ie::TimerStatusProhibit, u16) = (ie::TimerStatusProhibit::tsp200, 200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignallingProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionReason {
    Congestion,
    Unspecified,
}
impl SignallingProfile {
    // TS 25.331 UL/DL AM windows describe the UE transmitter/receiver. Network polling is local policy.
    pub fn network_am_configuration(
        self,
        transmit_pdu_bytes: std::num::NonZeroUsize,
        receive_pdu_bytes: std::num::NonZeroUsize,
        initial_tx_hfn: u32,
        initial_rx_hfn: u32,
    ) -> umts_rlc::acknowledged::AmConfig {
        use std::num::{NonZeroU16, NonZeroU64};
        use umts_rlc::acknowledged::{AmConfig, Polling};
        const CHIPS_PER_MILLISECOND: u64 = 3840;
        AmConfig {
            transmit_pdu_bytes,
            receive_pdu_bytes,
            transmit_window: NonZeroU16::new(self.receive_window())
                .expect("nonzero reference window"),
            receive_window: NonZeroU16::new(self.transmit_window())
                .expect("nonzero reference window"),
            maximum_data_attempts: NonZeroU16::new(u16::from(self.maximum_transmissions()))
                .expect("nonzero reference MaxDAT"),
            maximum_reset_attempts: NonZeroU16::new(u16::from(self.maximum_resets()))
                .expect("nonzero reference MaxRST"),
            reset_timeout_chips: NonZeroU64::new(
                u64::from(self.reset_timeout_ms()) * CHIPS_PER_MILLISECOND,
            )
            .expect("nonzero reset timer"),
            polling: Polling {
                every_pdus: Some(NonZeroU16::MIN),
                last_transmission: true,
                last_retransmission: true,
                timeout_chips: NonZeroU64::new(
                    u64::from(self.poll_timeout_ms()) * CHIPS_PER_MILLISECOND,
                ),
                prohibit_chips: None,
            },
            status_prohibit_chips: NonZeroU64::new(
                u64::from(self.status_prohibit_ms()) * CHIPS_PER_MILLISECOND,
            ),
            missing_pdu_indicator: true,
            accept_special_he: false,
            initial_tx_hfn,
            initial_rx_hfn,
        }
    }

    pub fn transmit_window(self) -> u16 {
        32
    }
    pub fn receive_window(self) -> u16 {
        32
    }
    pub fn reset_timeout_ms(self) -> u16 {
        400
    }
    pub fn maximum_resets(self) -> u8 {
        4
    }
    pub fn maximum_transmissions(self) -> u8 {
        10
    }
    pub fn poll_timeout_ms(self) -> u16 {
        200
    }
    pub fn status_prohibit_ms(self) -> u16 {
        100
    }

    pub fn peer_status_prohibit_ms(self) -> u16 {
        PEER_STATUS_PROHIBIT.1
    }

    // The setup's UL AM and DL STATUS fields configure the UE's RLC entity (TS 25.331 8.6.4.9) and
    // follow the TS 34.108 9.1.1 default. The network entity keeps its own local timers above.
    pub fn acknowledged_rlc(self) -> ie::RLCInfoChoice {
        ie::RLCInfoChoice::rlc_Info(ie::RLCInfo::new(
            Some(ie::ULRLCMode::ul_AM_RLC_Mode(ie::ULAMRLCMode::new(
                ie::TransmissionRLCDiscard::noDiscard(ie::MaxDAT::dat15),
                ie::TransmissionWindowSize::tw32,
                ie::TimerRST::tr500,
                ie::MaxRST::rst1,
                Some(ie::PollingInfo::new(
                    Some(ie::TimerPollProhibit::tpp200),
                    Some(ie::TimerPoll::tp200),
                    None,
                    Some(ie::PollSDU::sdu1),
                    true,
                    true,
                    Some(ie::PollWindow::pw99),
                    None,
                )),
            ))),
            Some(ie::DLRLCMode::dl_AM_RLC_Mode(ie::DLAMRLCMode::new(
                true,
                ie::ReceivingWindowSize::rw32,
                ie::DLRLCStatusInfo::new(Some(PEER_STATUS_PROHIBIT.0), None, true, None),
            ))),
        ))
    }
}

pub struct CellFachSetup {
    pub assigned: AssignedRntis,
    pub transaction: TransactionId,
    pub drx_coefficient: u8,
    pub signalling: SignallingProfile,
}

fn mapping(identity: u8) -> ie::RBMappingInfo {
    ie::RBMappingInfo(vec![ie::RBMappingOption::new(
        Some(ie::ULLogicalChannelMappings::oneLogicalChannel(
            ie::ULLogicalChannelMapping::new(
                ie::ULTransportChannelType::rach(()),
                Some(ie::LogicalChannelIdentity(identity)),
                // TS 25.331 8.6.4.8 forbids All on RACH and indexes the first SIB5 PRACH TFS.
                ie::ULLogicalChannelMappingRlcSizeList::explicitList(ie::RLCSizeExplicitList(
                    vec![ie::RLCSizeInfo::new(FIRST_PRACH_RLC_SIZE_INDEX)],
                )),
                ie::MACLogicalChannelPriority(identity),
            ),
        )),
        Some(ie::DLLogicalChannelMappingList(vec![
            ie::DLLogicalChannelMapping::new(
                ie::DLTransportChannelType::fach(()),
                Some(ie::LogicalChannelIdentity(identity)),
            ),
        ])),
    )])
}

fn srbs(profile: SignallingProfile) -> ie::SRBInformationSetupList2 {
    let um = ie::RLCInfoChoice::rlc_Info(ie::RLCInfo::new(
        Some(ie::ULRLCMode::ul_UM_RLC_Mode(ie::ULUMRLCMode::new(None))),
        Some(ie::DLRLCMode::dl_UM_RLC_Mode(())),
    ));
    ie::SRBInformationSetupList2(vec![
        ie::SRBInformationSetup::new(Some(ie::RBIdentity(FIRST_SRB)), um, mapping(FIRST_SRB)),
        ie::SRBInformationSetup::new(
            Some(ie::RBIdentity(CONTROL_SRB)),
            profile.acknowledged_rlc(),
            mapping(CONTROL_SRB),
        ),
        ie::SRBInformationSetup::new(
            Some(ie::RBIdentity(NAS_SRB)),
            ie::RLCInfoChoice::same_as_RB(ie::RBIdentity(CONTROL_SRB)),
            mapping(NAS_SRB),
        ),
    ])
}

// TS 25.331 8.1.3.4 Note 2: an R99 setup to CELL_FACH still includes a TrCH description.
// Some UEs reject the zero-size form.
fn unused_dch() -> ie::TransportFormatSet {
    ie::TransportFormatSet::dedicatedTransChTFS(ie::DedicatedTransChTFS::new(
        ie::DedicatedTransChTFSTti::tti40(ie::DedicatedDynamicTFInfoList(vec![
            ie::DedicatedDynamicTFInfo::new(
                ie::DedicatedDynamicTFInfoRlcSize::bitMode(ie::BitModeRLCSizeInfo::sizeType2(
                    ie::BitModeRLCSizeInfoSizeType2::new(DUMMY_RLC_SIZE_TYPE2, None),
                )),
                vec![ie::NumberOfTransportBlocks::one(())],
                ie::LogicalChannelList::allSizes(()),
            ),
        ])),
        ie::SemistaticTFInformation::new(
            ie::ChannelCodingType::convolutional(ie::CodingRate::third),
            ie::RateMatchingAttribute(DUMMY_RATE_MATCHING_ATTRIBUTE),
            ie::CRCSize::crc16,
        ),
    ))
}

pub fn build_cell_fach_setup(
    identity: &ie::InitialUEIdentity,
    parameters: &CellFachSetup,
) -> Result<EncodedRrc, EstablishmentError> {
    if !(MIN_DRX_COEFFICIENT..=MAX_DRX_COEFFICIENT).contains(&parameters.drx_coefficient) {
        return Err(EstablishmentError::Drx);
    }
    let body = pdu::RRCConnectionSetupR3IEs {
        initial_ue_identity: identity.clone(),
        rrc_transaction_identifier: ie::RRCTransactionIdentifier(parameters.transaction.get()),
        activation_time: None,
        new_u_rnti: parameters.assigned.wire(),
        new_c_rnti: Some(parameters.assigned.cell_wire()),
        rrc_state_indicator: ie::RRCStateIndicator::cell_FACH,
        utran_drx_cycle_length_coeff: ie::UTRANDRXCycleLengthCoefficient(
            parameters.drx_coefficient,
        ),
        // TS 34.108 9.1.1 requests the FDD and GSM capabilities in the setup itself.
        capability_update_requirement: Some(ie::CapabilityUpdateRequirement::new(
            true,
            false,
            Some(ie::SystemSpecificCapUpdateReqList(vec![
                ie::SystemSpecificCapUpdateReq::gsm,
            ])),
        )),
        srb_information_setup_list: srbs(parameters.signalling),
        ul_common_trans_ch_info: None,
        ul_add_reconf_trans_ch_info_list: ie::ULAddReconfTransChInfoList(vec![
            ie::ULAddReconfTransChInformation::new(
                ie::ULTrCHType::dch,
                ie::TransportChannelIdentity(UNUSED_DCH_ID),
                unused_dch(),
            ),
        ]),
        dl_common_trans_ch_info: None,
        dl_add_reconf_trans_ch_info_list: ie::DLAddReconfTransChInfoList(vec![
            ie::DLAddReconfTransChInformation::new(
                ie::DLTrCHType::dch,
                ie::TransportChannelIdentity(UNUSED_DCH_ID),
                ie::DLAddReconfTransChInformationTfsSignallingMode::sameAsULTrCH(
                    ie::ULTransportChannelIdentity::new(
                        ie::ULTrCHType::dch,
                        ie::TransportChannelIdentity(UNUSED_DCH_ID),
                    ),
                ),
                None,
                None,
            ),
        ]),
        frequency_info: None,
        max_allowed_ul_tx_power: None,
        ul_channel_requirement: None,
        dl_common_information: None,
        dl_information_per_rl_list: None,
    };
    let message = class::DLCCCHMessage::new(
        None,
        class::DLCCCHMessageType::rrcConnectionSetup(pdu::RRCConnectionSetup::r3(
            pdu::RRCConnectionSetupR3::new(body, None),
        )),
    );
    encode_downlink_common(&message).map_err(EstablishmentError::Codec)
}

pub fn build_rejection(
    identity: &ie::InitialUEIdentity,
    transaction: TransactionId,
    reason: RejectionReason,
    wait_time_code: u8,
) -> Result<EncodedRrc, EstablishmentError> {
    if wait_time_code > MAX_WAIT_TIME_CODE {
        return Err(EstablishmentError::WaitTime);
    }
    let cause = match reason {
        RejectionReason::Congestion => ie::RejectionCause::congestion,
        RejectionReason::Unspecified => ie::RejectionCause::unspecified,
    };
    let body = pdu::RRCConnectionRejectR3IEs::new(
        identity.clone(),
        ie::RRCTransactionIdentifier(transaction.get()),
        cause,
        ie::WaitTime(wait_time_code),
        None,
    );
    let message = class::DLCCCHMessage::new(
        None,
        class::DLCCCHMessageType::rrcConnectionReject(pdu::RRCConnectionReject::r3(
            pdu::RRCConnectionRejectR3::new(body, None),
        )),
    );
    encode_downlink_common(&message).map_err(EstablishmentError::Codec)
}
