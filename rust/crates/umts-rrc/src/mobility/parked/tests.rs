// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::mobility::{RNC_BITS, SRNTI_BITS, word};
use umts_rrc_codec::{decode_downlink_dedicated, decode_uplink_common};
use umts_rrc_schema::class_definitions::ULCCCHMessage;

const URA: u16 = 0xa5c3;

fn transaction() -> TransactionId {
    TransactionId::new(3).unwrap()
}

fn decoded(encoded: &EncodedRrc) -> DLDCCHMessage {
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    assert_eq!(decoded.consumed_bits, encoded.bit_length);
    assert!(decoded.value.integrity_check_info.is_none());
    *decoded.value
}

fn states() -> [(ParkedState, ie::RRCStateIndicator, Option<u32>); 2] {
    [
        (ParkedState::CellPch, ie::RRCStateIndicator::cell_PCH, None),
        (
            ParkedState::UraPch { identity: URA },
            ie::RRCStateIndicator::ura_PCH,
            Some(u32::from(URA)),
        ),
    ]
}

#[test]
fn park_reconfiguration_has_only_the_state_the_drx_coefficient_and_the_ura_identity() {
    for (state, indicator, ura) in states() {
        for coefficient in [3, 6, 9] {
            let encoded = build_park_reconfiguration(transaction(), state, coefficient).unwrap();
            let DLDCCHMessageType::physicalChannelReconfiguration(
                pdu::PhysicalChannelReconfiguration::r3(body),
            ) = decoded(&encoded).message
            else {
                panic!("R3 PHYSICAL CHANNEL RECONFIGURATION required")
            };
            assert!(body.v3a0_non_critical_extensions.is_none());
            let ies = body.physical_channel_reconfiguration_r3;
            assert_eq!(ies.rrc_transaction_identifier.0, 3);
            assert_eq!(ies.rrc_state_indicator, indicator);
            assert_eq!(
                ies.utran_drx_cycle_length_coeff.map(|value| value.0),
                Some(coefficient)
            );
            assert_eq!(ies.ura_identity.map(|value| word(&value.0)), ura);
            assert!(ies.activation_time.is_none());
            assert!(ies.new_u_rnti.is_none() && ies.new_c_rnti.is_none());
            assert!(
                ies.integrity_protection_mode_info.is_none() && ies.ciphering_mode_info.is_none()
            );
            assert!(
                ies.cn_information_info.is_none() && ies.dl_counter_synchronisation_info.is_none()
            );
            assert!(ies.frequency_info.is_none() && ies.max_allowed_ul_tx_power.is_none());
            assert!(ies.ul_channel_requirement.is_none());
            assert!(
                ies.dl_common_information.is_none() && ies.dl_information_per_rl_list.is_none()
            );
            assert_eq!(
                ies.mode_specific_info,
                pdu::PhysicalChannelReconfigurationR3IEsModeSpecificInfo::fdd(
                    pdu::PhysicalChannelReconfigurationR3IEsModeSpecificInfoFdd::new(None)
                )
            );
        }
        for coefficient in [0, 2, 10, u8::MAX] {
            assert_eq!(
                build_park_reconfiguration(transaction(), state, coefficient),
                Err(MobilityError::DrxCoefficient)
            );
        }
    }
}

#[test]
fn literal_park_reconfiguration_matches_the_independent_bit_layout() {
    let encoded = build_park_reconfiguration(transaction(), ParkedState::CellPch, 6).unwrap();
    // No integrity, DL-DCCH choice 10 in 5 bits, r3, no extensions, fourteen option bits with
    // only the DRX coefficient present, transaction 3, cell_PCH, coefficient 6 as offset 3, fdd.
    let bits = "0 01010 0 0 00000100000000 11 10 011 0 0";
    let expected = bits
        .chars()
        .filter(|bit| *bit != ' ')
        .map(|bit| bit == '1')
        .collect::<Vec<_>>();
    assert_eq!(encoded.bit_length, expected.len());
    for (index, bit) in expected.iter().enumerate() {
        assert_eq!(
            encoded.bytes[index / 8] & (0x80 >> (index % 8)) != 0,
            *bit,
            "bit {index}"
        );
    }
}

#[test]
fn parked_cell_update_confirm_keeps_the_state_without_a_crnti_or_rlc_re_establishment() {
    for (state, indicator, ura) in states() {
        let target = ConfirmTarget::Parked {
            state,
            drx_coefficient: 7,
        };
        let encoded = build_cell_update_confirm(transaction(), target).unwrap();
        let DLDCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirm::r3(body)) =
            decoded(&encoded).message
        else {
            panic!("R3 CELL UPDATE CONFIRM required")
        };
        let ies = body.cell_update_confirm_r3;
        let mut expected = empty_confirm_ies(transaction(), indicator);
        expected.utran_drx_cycle_length_coeff = Some(ie::UTRANDRXCycleLengthCoefficient(7));
        expected.ura_identity = state.ura_identity();
        assert_eq!(ies, expected);
        assert_eq!(ies.rrc_transaction_identifier.0, 3);
        assert_eq!(ies.rrc_state_indicator, indicator);
        assert_eq!(
            ies.utran_drx_cycle_length_coeff.map(|value| value.0),
            Some(7)
        );
        assert_eq!(ies.ura_identity.map(|value| word(&value.0)), ura);
        assert!(ies.new_c_rnti.is_none() && ies.new_u_rnti.is_none());
        assert!(!ies.rlc_re_establish_indicator_rb2_3or4);
        assert!(!ies.rlc_re_establish_indicator_rb5or_above);
        assert!(ies.activation_time.is_none());
        assert!(
            ies.rb_information_release_list.is_none()
                && ies.rb_information_reconfig_list.is_none()
                && ies.rb_information_affected_list.is_none()
        );
        assert!(
            ies.ul_common_trans_ch_info.is_none()
                && ies.ul_add_reconf_trans_ch_info_list.is_none()
                && ies.dl_common_trans_ch_info.is_none()
                && ies.dl_add_reconf_trans_ch_info_list.is_none()
        );
        assert!(
            ies.frequency_info.is_none()
                && ies.ul_channel_requirement.is_none()
                && ies.dl_common_information.is_none()
                && ies.dl_information_per_rl_list.is_none()
        );
        for coefficient in [2, 10] {
            assert_eq!(
                build_cell_update_confirm(
                    transaction(),
                    ConfirmTarget::Parked {
                        state,
                        drx_coefficient: coefficient
                    }
                ),
                Err(MobilityError::DrxCoefficient)
            );
        }
    }
}

#[test]
fn fach_target_equals_the_existing_dcch_confirm() {
    for new_crnti in [None, Some(0xbeef)] {
        let general =
            cell_update_confirm_message(transaction(), ConfirmTarget::Fach { new_crnti }).unwrap();
        assert_eq!(
            general,
            crate::mobility::build_cell_update_confirm_dcch(transaction(), new_crnti)
        );
        let encoded =
            build_cell_update_confirm(transaction(), ConfirmTarget::Fach { new_crnti }).unwrap();
        let DLDCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirm::r3(body)) =
            decoded(&encoded).message
        else {
            panic!("R3 CELL UPDATE CONFIRM required")
        };
        let ies = body.cell_update_confirm_r3;
        assert_eq!(ies.rrc_state_indicator, ie::RRCStateIndicator::cell_FACH);
        assert_eq!(
            ies.new_c_rnti.map(|value| word(&value.0)),
            new_crnti.map(u32::from)
        );
        assert!(ies.utran_drx_cycle_length_coeff.is_none() && ies.ura_identity.is_none());
    }
}

fn assigned() -> AssignedRntis {
    AssignedRntis::new(0x123, 0x45678, 1).unwrap()
}

fn ura_update(cause: ie::URAUpdateCause) -> umts_rrc_codec::DecodedUplinkCommon {
    let message = ULCCCHMessage::new(
        None,
        ULCCCHMessageType::uraUpdate(pdu::URAUpdate::new(
            ie::URNTI::new(
                ie::SRNCIdentity(fixed::<RNC_BITS>(0x123)),
                ie::SRNTI(fixed::<SRNTI_BITS>(0x45678)),
            ),
            cause,
            ie::ProtocolErrorIndicatorWithMoreInfo::noError(()),
            None,
        )),
    );
    let wire = rasn::uper::encode(&message).unwrap();
    decode_uplink_common(&wire, wire.len() * 8).unwrap()
}

#[test]
fn ura_update_reports_the_cause_and_checks_the_full_urnti() {
    for (wire, cause) in [
        (ie::URAUpdateCause::changeOfURA, UraUpdateCause::ChangeOfUra),
        (
            ie::URAUpdateCause::periodicURAUpdate,
            UraUpdateCause::Periodic,
        ),
    ] {
        let decoded = ura_update(wire);
        assert_eq!(ura_update_urnti(&decoded), Ok(0x12345678));
        assert_eq!(
            inspect_ura_update(assigned(), &decoded),
            Ok(UraUpdate {
                urnti: 0x12345678,
                cause,
                protocol_error: false,
            })
        );
        assert_eq!(
            inspect_ura_update(AssignedRntis::new(0x123, 0x45679, 1).unwrap(), &decoded),
            Err(MobilityError::Identity)
        );
    }
    for spare in [ie::URAUpdateCause::dummy, ie::URAUpdateCause::spare1] {
        assert_eq!(
            inspect_ura_update(assigned(), &ura_update(spare)),
            Err(MobilityError::Message)
        );
    }
    let wire = [
        0x00, 0x48, 0xd1, 0x59, 0xe1, 0x09, 0x1a, 0x2d, 0x50, 0xc8, 0x6c, 0x00,
    ];
    let cell_update = decode_uplink_common(&wire, 89).unwrap();
    assert_eq!(
        inspect_ura_update(assigned(), &cell_update),
        Err(MobilityError::Message)
    );
    assert_eq!(ura_update_urnti(&cell_update), Err(MobilityError::Message));
}

#[test]
fn ura_update_confirm_keeps_ura_pch_with_the_drx_coefficient_and_identity() {
    let state = ParkedState::UraPch { identity: URA };
    let encoded = build_ura_update_confirm(transaction(), state, 5).unwrap();
    let DLDCCHMessageType::uraUpdateConfirm(pdu::URAUpdateConfirm::r3(body)) =
        decoded(&encoded).message
    else {
        panic!("R3 URA UPDATE CONFIRM required")
    };
    assert!(body.later_non_critical_extensions.is_none());
    let ies = body.ura_update_confirm_r3;
    assert_eq!(ies.rrc_transaction_identifier.0, 3);
    assert_eq!(ies.rrc_state_indicator, ie::RRCStateIndicator::ura_PCH);
    assert_eq!(
        ies.utran_drx_cycle_length_coeff.map(|value| value.0),
        Some(5)
    );
    assert_eq!(
        ies.ura_identity.map(|value| word(&value.0)),
        Some(u32::from(URA))
    );
    assert!(ies.new_u_rnti.is_none() && ies.new_c_rnti.is_none());
    assert!(ies.integrity_protection_mode_info.is_none() && ies.ciphering_mode_info.is_none());
    assert!(ies.cn_information_info.is_none() && ies.dl_counter_synchronisation_info.is_none());
    for coefficient in [2, 10] {
        assert_eq!(
            build_ura_update_confirm(transaction(), state, coefficient),
            Err(MobilityError::DrxCoefficient)
        );
    }
}
