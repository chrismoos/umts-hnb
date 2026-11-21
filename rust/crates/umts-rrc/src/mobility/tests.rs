// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc_codec::{decode_downlink_common, decode_uplink_common};
use umts_rrc_schema::class_definitions::ULCCCHMessage;

#[test]
fn inactivity_release_uses_dcch_and_a_normal_transaction_with_user_inactivity_cause() {
    for state in [
        ReleaseConnectionState::CellFach,
        ReleaseConnectionState::CellDch(ReleaseCompleteRetries::new(2).unwrap()),
    ] {
        let message = build_inactivity_release(TransactionId::new(3).unwrap(), state);
        let encoded = umts_rrc_codec::encode_downlink_dedicated(&message).unwrap();
        let decoded =
            umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        let DLDCCHMessageType::rrcConnectionRelease(pdu::RRCConnectionRelease::r3(release)) =
            decoded.value.message
        else {
            panic!("DCCH release required")
        };
        assert_eq!(
            release
                .rrc_connection_release_r3
                .rrc_transaction_identifier
                .0,
            3
        );
        assert_eq!(
            release.rrc_connection_release_r3.release_cause,
            ie::ReleaseCause::userInactivity
        );
        assert!(decoded.value.integrity_check_info.is_none());
        assert_eq!(
            release.rrc_connection_release_r3.n_308.map(|value| value.0),
            match state {
                ReleaseConnectionState::CellFach => None,
                ReleaseConnectionState::CellDch(_) => Some(2),
            }
        );
    }
    assert!(ReleaseCompleteRetries::new(0).is_err());
    assert!(ReleaseCompleteRetries::new(9).is_err());
}

fn assigned() -> AssignedRntis {
    AssignedRntis::new(0x123, 0x45678, 1).unwrap()
}
fn request() -> ULCCCHMessage {
    ULCCCHMessage::new(
        None,
        ULCCCHMessageType::cellUpdate(pdu::CellUpdate::new(
            ie::URNTI::new(
                ie::SRNCIdentity(fixed::<RNC_BITS>(0x123)),
                ie::SRNTI(fixed::<SRNTI_BITS>(0x45678)),
            ),
            ie::STARTList(vec![
                ie::STARTSingle::new(
                    ie::CNDomainIdentity::cs_domain,
                    ie::STARTValue(fixed::<20>(0x12345)),
                ),
                ie::STARTSingle::new(
                    ie::CNDomainIdentity::ps_domain,
                    ie::STARTValue(fixed::<20>(0x54321)),
                ),
            ]),
            true,
            false,
            ie::CellUpdateCause::rlc_unrecoverableError,
            None,
            ie::RbTimerIndicator::new(false, false),
            None,
            None,
        )),
    )
}

#[test]
fn cell_update_matches_spec_bit_fields_and_validates_full_urnti_and_domain_starts() {
    // UL-CCCH presence/choice, CellUpdate optional bitmap, U-RNTI, START list, error flags, cause and timer flags.
    let wire = [
        0x00, 0x48, 0xd1, 0x59, 0xe1, 0x09, 0x1a, 0x2d, 0x50, 0xc8, 0x6c, 0x00,
    ];
    let decoded = decode_uplink_common(&wire, 89).unwrap();
    assert_eq!(decoded.consumed_bits, 89);
    assert_eq!(*decoded.value, request());
    let update = inspect_cell_update(assigned(), &decoded).unwrap();
    assert_eq!(update.urnti, 0x12345678);
    assert_eq!(
        update.starts,
        DomainStarts {
            cs: Some(0x12345),
            ps: Some(0x54321)
        }
    );
    assert_eq!(update.cause, UpdateCause::RlcUnrecoverable);
    assert!(update.signalling_rlc_error);
    assert!(!update.user_rlc_error);
    assert!(update.failure.is_none());
    assert_eq!(rasn::uper::encode(&request()).unwrap(), wire);
    assert_eq!(
        inspect_cell_update(AssignedRntis::new(0x124, 0x45678, 1).unwrap(), &decoded),
        Err(MobilityError::Identity)
    );
    let mut invalid = request();
    let ULCCCHMessageType::cellUpdate(update) = &mut invalid.message else {
        unreachable!()
    };
    update.start_list.0[1].cn_domain_identity = ie::CNDomainIdentity::cs_domain;
    let wire = rasn::uper::encode(&invalid).unwrap();
    let decoded = decode_uplink_common(&wire, wire.len() * 8).unwrap();
    assert_eq!(
        inspect_cell_update(assigned(), &decoded),
        Err(MobilityError::Starts)
    );
}

#[test]
fn signalling_failure_release_matches_spec_bits_and_excludes_common_integrity() {
    let release =
        build_signalling_failure_release(assigned(), TransactionId::new(2).unwrap()).unwrap();
    // DL-CCCH choice 2, r3, U-RNTI, no N308/RPLMN, transaction 2 and re-establishment reject.
    assert_eq!(release.bytes, [0x20, 0x48, 0xd1, 0x59, 0xe0, 0xa0]);
    assert_eq!(release.bit_length, 45);
    let mut decoded = decode_downlink_common(&release.bytes, release.bit_length)
        .unwrap()
        .value;
    assert!(decoded.integrity_check_info.is_none());
    decoded.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed::<32>(0)),
        ie::RRCMessageSequenceNumber(0),
    ));
    let invalid = rasn::uper::encode(&decoded).unwrap();
    assert_eq!(
        decode_downlink_common(&invalid, invalid.len() * 8).unwrap_err(),
        CodecError::InvalidRrcValue
    );
}

#[test]
fn unknown_context_release_preserves_the_full_urnti() {
    for urnti in [0, 0x000f_ffff, 0xfff0_0000, u32::MAX] {
        let release = build_unknown_context_release(urnti, TransactionId::new(2).unwrap()).unwrap();
        assert_eq!(release.bit_length, 45);
        let bits = release
            .bytes
            .iter()
            .flat_map(|byte| (0..8).rev().map(move |shift| (byte >> shift) & 1));
        let encoded = bits
            .skip(6)
            .take(32)
            .fold(0_u32, |value, bit| (value << 1) | u32::from(bit));
        assert_eq!(encoded, urnti);
    }
}

#[test]
fn unknown_context_release_keeps_the_existing_failure_release_vector() {
    let assigned = assigned();
    assert_eq!(
        build_unknown_context_release(assigned.urnti(), TransactionId::new(2).unwrap()).unwrap(),
        build_signalling_failure_release(assigned, TransactionId::new(2).unwrap()).unwrap()
    );
}

#[test]
fn cell_update_confirm_keeps_cell_fach_and_needs_no_response_without_a_new_crnti() {
    let assigned = assigned();
    let confirm =
        build_cell_update_confirm_ccch(assigned.urnti(), TransactionId::new(1).unwrap(), None)
            .unwrap();
    let decoded = decode_downlink_common(&confirm.bytes, confirm.bit_length).unwrap();
    assert!(decoded.value.integrity_check_info.is_none());
    let DLCCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirmCCCH::r3(body)) =
        decoded.value.message
    else {
        panic!("CCCH confirm required")
    };
    assert_eq!(
        word(&body.u_rnti.srnc_identity.0),
        assigned.urnti() >> SRNTI_BITS
    );
    assert_eq!(
        word(&body.u_rnti.s_rnti.0),
        assigned.urnti() & ((1 << SRNTI_BITS) - 1)
    );
    let ies = body.cell_update_confirm_r3;
    assert_eq!(ies.rrc_transaction_identifier.0, 1);
    assert_eq!(ies.rrc_state_indicator, ie::RRCStateIndicator::cell_FACH);
    assert!(ies.new_c_rnti.is_none());
    assert!(ies.new_u_rnti.is_none());
    assert!(!ies.rlc_re_establish_indicator_rb2_3or4);
    assert!(!ies.rlc_re_establish_indicator_rb5or_above);
    assert!(ies.ciphering_mode_info.is_none() && ies.integrity_protection_mode_info.is_none());
    assert!(
        ies.rb_information_release_list.is_none() && ies.rb_information_affected_list.is_none()
    );
    assert!(ies.ul_add_reconf_trans_ch_info_list.is_none() && ies.frequency_info.is_none());
}

#[test]
fn cell_update_confirm_dcch_carries_the_new_crnti_that_requires_a_mobility_confirm() {
    let message = build_cell_update_confirm_dcch(TransactionId::new(2).unwrap(), Some(0xbeef));
    let encoded = umts_rrc_codec::encode_downlink_dedicated(&message).unwrap();
    let decoded =
        umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let DLDCCHMessageType::cellUpdateConfirm(pdu::CellUpdateConfirm::r3(body)) =
        decoded.value.message
    else {
        panic!("DCCH confirm required")
    };
    let ies = body.cell_update_confirm_r3;
    assert_eq!(ies.rrc_transaction_identifier.0, 2);
    assert_eq!(ies.rrc_state_indicator, ie::RRCStateIndicator::cell_FACH);
    assert_eq!(ies.new_c_rnti.map(|value| word(&value.0)), Some(0xbeef));
}

#[test]
fn cell_update_reports_the_failed_confirm_transaction_and_cause() {
    for (wire, cause) in [
        (
            ie::FailureCauseWithProtErr::physicalChannelFailure(()),
            UpdateFailureCause::PhysicalChannelFailure,
        ),
        (
            ie::FailureCauseWithProtErr::invalidConfiguration(()),
            UpdateFailureCause::InvalidConfiguration,
        ),
        (
            ie::FailureCauseWithProtErr::configurationUnsupported(()),
            UpdateFailureCause::ConfigurationUnsupported,
        ),
        (
            ie::FailureCauseWithProtErr::unsupportedMeasurement(()),
            UpdateFailureCause::Other,
        ),
    ] {
        let mut message = request();
        let ULCCCHMessageType::cellUpdate(update) = &mut message.message else {
            unreachable!()
        };
        update.cell_update_cause = ie::CellUpdateCause::radiolinkFailure;
        update.failure_cause = Some(ie::FailureCauseWithProtErrTrId::new(
            ie::RRCTransactionIdentifier(2),
            wire,
        ));
        let wire = rasn::uper::encode(&message).unwrap();
        let decoded = decode_uplink_common(&wire, wire.len() * 8).unwrap();
        let update = inspect_cell_update(assigned(), &decoded).unwrap();
        assert_eq!(update.cause, UpdateCause::RadioLinkFailure);
        assert_eq!(
            update.failure,
            Some(UpdateFailure {
                transaction: 2,
                cause
            })
        );
    }
}
