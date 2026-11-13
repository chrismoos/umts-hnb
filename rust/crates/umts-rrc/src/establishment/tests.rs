// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_rrc_codec::{decode_downlink_common, decode_uplink_common, decode_uplink_dedicated};
use umts_rrc_schema::{
    class_definitions::{DLCCCHMessageType, ULDCCHMessageType},
    pdu_definitions as pdu,
};
use umts_types::{RadioEpoch, UeKey, time::ChipInstant};

const REQUEST: [u8; 10] = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];
// TS 25.331 UL-DCCH choice 18, no optional fields, transaction 2, CS/PS START values 0x12345/0x54321.
const COMPLETE: [u8; 7] = [0x48, 0x48, 0x48, 0xd1, 0x6a, 0x86, 0x42];
const COMPLETE_BITS: usize = 55;
// TS 25.331 R3 CELL_FACH setup in the TS 34.108 9.1.1 default form: capability update requirement,
// three SRBs and the 144-bit dummy DCH with the downlink same as uplink. Cross-checked with an
// independent ASN.1 decoder.
const FACH_SETUP: [u8; 53] = [
    0x31, 0x80, 0x12, 0x00, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00, 0x30, 0x00, 0x89, 0x1a, 0x29, 0x1a,
    0x2a, 0xb0, 0x20, 0x69, 0x1a, 0x84, 0x00, 0x05, 0x08, 0x59, 0xe8, 0xe4, 0x6c, 0x9a, 0x67, 0xc9,
    0xc4, 0xe3, 0x51, 0x80, 0x02, 0xa3, 0x14, 0x23, 0x52, 0x80, 0x04, 0xa4, 0x07, 0x88, 0x04, 0x40,
    0x47, 0x3e, 0xc0, 0x3d, 0x78,
];
const FACH_SETUP_BITS: usize = 422;

#[test]
fn reference_am_runtime_parameters_preserve_wire_units_windows_and_domain_counts() {
    let profile = SignallingProfile;
    let configuration = profile.network_am_configuration(
        std::num::NonZeroUsize::new(42).unwrap(),
        std::num::NonZeroUsize::new(29).unwrap(),
        0x12345,
        0x54321,
    );
    assert_eq!(
        configuration.transmit_window.get(),
        profile.receive_window()
    );
    assert_eq!(
        configuration.receive_window.get(),
        profile.transmit_window()
    );
    assert_eq!(
        configuration.maximum_data_attempts.get(),
        u16::from(profile.maximum_transmissions())
    );
    assert_eq!(
        configuration.maximum_reset_attempts.get(),
        u16::from(profile.maximum_resets())
    );
    assert_eq!(configuration.reset_timeout_chips.get(), 1_536_000);
    assert_eq!(configuration.polling.timeout_chips.unwrap().get(), 768_000);
    assert_eq!(configuration.status_prohibit_chips.unwrap().get(), 384_000);
    assert_eq!(configuration.initial_tx_hfn, 0x12345);
    assert_eq!(configuration.initial_rx_hfn, 0x54321);
    assert!(configuration.polling.prohibit_chips.is_none());
    assert!(!configuration.accept_special_he);
}

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn key(slot: u32) -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot,
        generation: NonZeroU64::MIN,
    }
}
fn parameters(slot: u16) -> CellFachSetup {
    CellFachSetup {
        assigned: AssignedRntis::new(1, 0x12345 + u32::from(slot), 0x2345 + slot).unwrap(),
        transaction: TransactionId::new(2).unwrap(),
        drx_coefficient: 5,
        signalling: SignallingProfile,
    }
}
fn prepare(slot: u16) -> Establishment {
    Establishment::prepare(
        key(u32::from(slot)),
        at(10),
        &decode_uplink_common(&REQUEST, 78).unwrap(),
        &parameters(slot),
    )
    .unwrap()
}

#[test]
fn r99_fach_setup_follows_the_ts34108_default_with_common_bearers_and_a_dummy_dch() {
    let procedure = prepare(0);
    let response = procedure.response();
    assert_eq!(response.bytes, FACH_SETUP);
    assert_eq!(response.bit_length, FACH_SETUP_BITS);
    // Independent UPER prefix: no integrity, DL choice 3, r3, no later extension, new-C-RNTI and
    // capability update requirement present.
    assert_eq!(&response.bytes[..2], &[0x31, 0x80]);
    let decoded = decode_downlink_common(&response.bytes, response.bit_length).unwrap();
    assert_eq!(decoded.consumed_bits, response.bit_length);
    assert!(decoded.value.integrity_check_info.is_none());
    let DLCCCHMessageType::rrcConnectionSetup(pdu::RRCConnectionSetup::r3(setup)) =
        decoded.value.message
    else {
        panic!("unexpected setup construction")
    };
    let setup = setup.rrc_connection_setup_r3;
    assert_eq!(setup.rrc_transaction_identifier.0, 2);
    assert_eq!(setup.new_u_rnti, parameters(0).assigned.wire());
    assert_eq!(setup.new_c_rnti, Some(parameters(0).assigned.cell_wire()));
    assert_eq!(setup.rrc_state_indicator, ie::RRCStateIndicator::cell_FACH);
    assert!(setup.activation_time.is_none());
    assert!(setup.ul_channel_requirement.is_none());
    assert!(setup.dl_common_information.is_none());
    assert!(setup.dl_information_per_rl_list.is_none());
    assert_eq!(setup.srb_information_setup_list.0.len(), 3);
    for (position, srb) in setup.srb_information_setup_list.0.iter().enumerate() {
        let identity = (position + 1) as u8;
        assert_eq!(srb.rb_identity.as_ref().unwrap().0, identity);
        let mapping = &srb.rb_mapping_info.0[0];
        let ie::ULLogicalChannelMappings::oneLogicalChannel(uplink) =
            mapping.ul_logical_channel_mappings.as_ref().unwrap()
        else {
            panic!("unexpected UL mapping")
        };
        assert_eq!(
            uplink.ul_transport_channel_type,
            ie::ULTransportChannelType::rach(())
        );
        assert_eq!(
            uplink.logical_channel_identity.as_ref().unwrap().0,
            identity
        );
        assert_eq!(
            uplink.rlc_size_list,
            ie::ULLogicalChannelMappingRlcSizeList::explicitList(ie::RLCSizeExplicitList(vec![
                ie::RLCSizeInfo::new(1)
            ]))
        );
        let downlink = &mapping.dl_logical_channel_mapping_list.as_ref().unwrap().0[0];
        assert_eq!(
            downlink.dl_transport_channel_type,
            ie::DLTransportChannelType::fach(())
        );
        assert_eq!(
            downlink.logical_channel_identity.as_ref().unwrap().0,
            identity
        );
    }
    let ie::RLCInfoChoice::rlc_Info(srb1) = &setup.srb_information_setup_list.0[0].rlc_info_choice
    else {
        panic!("missing SRB1 RLC configuration")
    };
    assert!(matches!(
        srb1.ul_rlc_mode,
        Some(ie::ULRLCMode::ul_UM_RLC_Mode(_))
    ));
    assert_eq!(srb1.dl_rlc_mode, Some(ie::DLRLCMode::dl_UM_RLC_Mode(())));
    let ie::RLCInfoChoice::rlc_Info(srb2) = &setup.srb_information_setup_list.0[1].rlc_info_choice
    else {
        panic!("missing SRB2 RLC configuration")
    };
    let Some(ie::ULRLCMode::ul_AM_RLC_Mode(ul_am)) = &srb2.ul_rlc_mode else {
        panic!("SRB2 must use AM")
    };
    assert_eq!(
        ul_am.transmission_window_size,
        ie::TransmissionWindowSize::tw32
    );
    assert_eq!(ul_am.timer_rst, ie::TimerRST::tr500);
    assert_eq!(ul_am.max_rst, ie::MaxRST::rst1);
    assert_eq!(
        ul_am.transmission_rlc_discard,
        ie::TransmissionRLCDiscard::noDiscard(ie::MaxDAT::dat15)
    );
    let polling = ul_am.polling_info.as_ref().unwrap();
    assert_eq!(polling.timer_poll, Some(ie::TimerPoll::tp200));
    assert_eq!(
        polling.timer_poll_prohibit,
        Some(ie::TimerPollProhibit::tpp200)
    );
    assert_eq!(polling.poll_sdu, Some(ie::PollSDU::sdu1));
    assert_eq!(polling.poll_pdu, None);
    assert_eq!(polling.poll_window, Some(ie::PollWindow::pw99));
    assert_eq!(
        setup.capability_update_requirement,
        Some(ie::CapabilityUpdateRequirement::new(
            true,
            false,
            Some(ie::SystemSpecificCapUpdateReqList(vec![
                ie::SystemSpecificCapUpdateReq::gsm
            ])),
        ))
    );
    let Some(ie::DLRLCMode::dl_AM_RLC_Mode(dl_am)) = &srb2.dl_rlc_mode else {
        panic!("SRB2 must use AM")
    };
    assert!(dl_am.in_sequence_delivery);
    assert_eq!(dl_am.receiving_window_size, ie::ReceivingWindowSize::rw32);
    assert_eq!(
        dl_am.dl_rlc_status_info.timer_status_prohibit,
        Some(ie::TimerStatusProhibit::tsp200)
    );
    assert_eq!(SignallingProfile.peer_status_prohibit_ms(), 200);
    assert_eq!(
        setup.srb_information_setup_list.0[2].rlc_info_choice,
        ie::RLCInfoChoice::same_as_RB(ie::RBIdentity(2))
    );
    assert_eq!(setup.ul_add_reconf_trans_ch_info_list.0.len(), 1);
    assert_eq!(setup.dl_add_reconf_trans_ch_info_list.0.len(), 1);
    let zero = &setup.ul_add_reconf_trans_ch_info_list.0[0];
    assert_eq!(zero.ul_transport_channel_type, ie::ULTrCHType::dch);
    let ie::TransportFormatSet::dedicatedTransChTFS(tfs) = &zero.transport_format_set else {
        panic!("not a dedicated transport description")
    };
    let ie::DedicatedTransChTFSTti::tti40(formats) = &tfs.tti else {
        panic!("wrong TTI")
    };
    assert_eq!(formats.0.len(), 1);
    assert_eq!(
        formats.0[0].rlc_size,
        ie::DedicatedDynamicTFInfoRlcSize::bitMode(ie::BitModeRLCSizeInfo::sizeType2(
            ie::BitModeRLCSizeInfoSizeType2::new(2, None)
        ))
    );
    assert_eq!(
        formats.0[0].number_of_tb_size_list,
        vec![ie::NumberOfTransportBlocks::one(())]
    );
    assert_eq!(
        formats.0[0].logical_channel_list,
        ie::LogicalChannelList::allSizes(())
    );
    assert_eq!(
        zero.transport_channel_identity,
        ie::TransportChannelIdentity(31)
    );
    assert_eq!(
        setup.dl_add_reconf_trans_ch_info_list.0[0].tfs_signalling_mode,
        ie::DLAddReconfTransChInformationTfsSignallingMode::sameAsULTrCH(
            ie::ULTransportChannelIdentity::new(
                ie::ULTrCHType::dch,
                ie::TransportChannelIdentity(31)
            )
        )
    );
}

#[test]
fn rejection_matches_independent_choice_identity_transaction_cause_and_wait_time_bits() {
    let request = decode_uplink_common(&REQUEST, 78).unwrap();
    let umts_rrc_schema::class_definitions::ULCCCHMessageType::rrcConnectionRequest(request) =
        request.value.message
    else {
        panic!("request")
    };
    let encoded = build_rejection(
        &request.initial_ue_identity,
        TransactionId::new(2).unwrap(),
        RejectionReason::Congestion,
        3,
    )
    .unwrap();
    assert_eq!(encoded.bit_length, 81);
    assert_eq!(
        encoded.bytes,
        [0x10, 0x24, 0, 0x40, 0x40, 0, 0, 0, 0, 0x61, 0x80]
    );
    assert!(
        build_rejection(
            &request.initial_ue_identity,
            TransactionId::new(2).unwrap(),
            RejectionReason::Congestion,
            16
        )
        .is_err()
    );
}

#[test]
fn publication_is_transactional_and_cannot_establish_without_matching_dedicated_completion() {
    let mut procedure = prepare(0);
    let complete = decode_uplink_dedicated(&COMPLETE, COMPLETE_BITS).unwrap();
    assert_eq!(
        procedure.complete(key(0), at(20), &complete),
        Err(ProcedureError::Phase)
    );
    let original = procedure.response().clone();
    assert_eq!(
        procedure.publish_with(at(20), NonZeroU64::new(100).unwrap(), |_| Err::<(), _>(
            "full"
        )),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(procedure.phase(), EstablishmentPhase::Prepared);
    procedure
        .publish_with(at(20), NonZeroU64::new(100).unwrap(), |message| {
            assert_eq!(message, &original);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(procedure.phase(), EstablishmentPhase::AwaitingComplete);
    assert!(procedure.starts().is_none());
    assert_eq!(
        procedure.complete(key(0), at(19), &complete),
        Err(ProcedureError::Time)
    );
    assert_eq!(
        procedure.retransmit_with(at(19), |_| Ok::<_, ()>(())),
        Err(PublishError::Procedure(ProcedureError::Time))
    );
    procedure
        .retransmit_with(at(30), |message| {
            assert_eq!(message, &original);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        procedure.complete(key(0), at(40), &complete),
        Ok(Completion::Established)
    );
    assert_eq!(procedure.phase(), EstablishmentPhase::Established);
    let ULDCCHMessageType::rrcConnectionSetupComplete(expected) = &complete.value.message else {
        panic!("completion")
    };
    assert_eq!(procedure.setup_complete(), Some(expected));
    assert_eq!(
        procedure.complete(key(0), at(39), &complete),
        Err(ProcedureError::Time)
    );
    assert_eq!(
        procedure.starts(),
        Some(DomainStarts {
            cs: Some(0x12345),
            ps: Some(0x54321)
        })
    );
    assert_eq!(
        procedure.complete(key(0), at(50), &complete),
        Ok(Completion::Duplicate)
    );
}

#[test]
fn wrong_origin_transaction_and_duplicate_domains_never_mutate_the_waiting_connection() {
    let mut procedure = prepare(0);
    procedure
        .publish_with(at(20), NonZeroU64::new(100).unwrap(), |_| Ok::<_, ()>(()))
        .unwrap();
    let mut complete = decode_uplink_dedicated(&COMPLETE, COMPLETE_BITS).unwrap();
    assert_eq!(
        procedure.complete(key(1), at(30), &complete),
        Err(ProcedureError::Origin)
    );
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key(0)
    };
    assert_eq!(
        procedure.complete(stale, at(30), &complete),
        Err(ProcedureError::Origin)
    );
    let ULDCCHMessageType::rrcConnectionSetupComplete(body) = &mut complete.value.message else {
        panic!("completion")
    };
    body.rrc_transaction_identifier.0 = 1;
    assert_eq!(
        procedure.complete(key(0), at(30), &complete),
        Err(ProcedureError::Transaction)
    );
    let ULDCCHMessageType::rrcConnectionSetupComplete(body) = &mut complete.value.message else {
        panic!("completion")
    };
    body.rrc_transaction_identifier.0 = 2;
    body.start_list.0[1].cn_domain_identity = ie::CNDomainIdentity::cs_domain;
    assert_eq!(
        procedure.complete(key(0), at(30), &complete),
        Err(ProcedureError::MessageConstruction(
            EstablishmentError::Starts
        ))
    );
    assert_eq!(procedure.phase(), EstablishmentPhase::AwaitingComplete);
    assert!(procedure.starts().is_none());
}

#[test]
fn timeout_is_absolute_retransmissions_do_not_extend_it_and_release_is_emitted_once() {
    let mut procedure = prepare(0);
    procedure
        .publish_with(at(20), NonZeroU64::new(100).unwrap(), |_| Ok::<_, ()>(()))
        .unwrap();
    procedure
        .retransmit_with(at(110), |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(procedure.expire(at(119)), Ok(None));
    assert_eq!(procedure.expire(at(120)), Ok(Some(key(0))));
    assert_eq!(procedure.expire(at(121)), Ok(None));
    assert_eq!(procedure.close_reason(), Some(CloseReason::Timeout));
    let complete = decode_uplink_dedicated(&COMPLETE, COMPLETE_BITS).unwrap();
    assert_eq!(
        procedure.complete(key(0), at(122), &complete),
        Err(ProcedureError::Phase)
    );
    assert_eq!(
        procedure.abort(CloseReason::LowerLayerFailure),
        Err(ProcedureError::Phase)
    );
}

#[test]
fn independent_connections_keep_starts_and_failures_private_and_conflicting_replays_cannot_reset_starts()
 {
    let mut one = prepare(0);
    let mut two = prepare(1);
    for procedure in [&mut one, &mut two] {
        procedure
            .publish_with(at(20), NonZeroU64::new(100).unwrap(), |_| Ok::<_, ()>(()))
            .unwrap();
    }
    let mut complete = decode_uplink_dedicated(&COMPLETE, COMPLETE_BITS).unwrap();
    one.complete(key(0), at(30), &complete).unwrap();
    let saved = one.starts();
    let ULDCCHMessageType::rrcConnectionSetupComplete(body) = &mut complete.value.message else {
        panic!("completion")
    };
    body.start_list.0[0].start_value.0.set(0, true);
    assert_eq!(
        one.complete(key(0), at(31), &complete),
        Err(ProcedureError::ConflictingCompletion)
    );
    assert_eq!(one.starts(), saved);
    assert_eq!(two.starts(), None);
    assert_eq!(two.abort(CloseReason::LowerLayerFailure), Ok(key(1)));
    assert_eq!(one.phase(), EstablishmentPhase::Established);
    assert_eq!(
        one.abort(CloseReason::PublicationCancelled),
        Err(ProcedureError::Phase)
    );
}

#[test]
fn invalid_timing_identity_and_transaction_inputs_fail_without_publication() {
    assert!(AssignedRntis::new(4096, 1, 1).is_err());
    assert!(AssignedRntis::new(1, 1 << 20, 1).is_err());
    assert!(TransactionId::new(4).is_err());
    let mut procedure = prepare(0);
    assert_eq!(
        procedure.publish_with(at(9), NonZeroU64::MIN, |_| Ok::<_, ()>(())),
        Err(PublishError::Procedure(ProcedureError::Time))
    );
    assert_eq!(
        procedure.publish_with(at(u64::MAX), NonZeroU64::MIN, |_| Ok::<_, ()>(())),
        Err(PublishError::Procedure(ProcedureError::Time))
    );
    assert_eq!(procedure.phase(), EstablishmentPhase::Prepared);
    assert_eq!(
        procedure.abort(CloseReason::PublicationCancelled),
        Ok(key(0))
    );
}

#[test]
fn request_classification_and_single_domain_starts_preserve_explicit_context() {
    let mut procedure = prepare(0);
    let mut request = decode_uplink_common(&REQUEST, 78).unwrap();
    assert_eq!(
        procedure.classify_request(&request),
        RequestMatch::Retransmission
    );
    let umts_rrc_schema::class_definitions::ULCCCHMessageType::rrcConnectionRequest(body) =
        &mut request.value.message
    else {
        panic!("request")
    };
    body.protocol_error_indicator = ie::ProtocolErrorIndicator::errorOccurred;
    assert_eq!(
        procedure.classify_request(&request),
        RequestMatch::ConfigurationFailure
    );
    let umts_rrc_schema::class_definitions::ULCCCHMessageType::rrcConnectionRequest(body) =
        &mut request.value.message
    else {
        panic!("request")
    };
    let ie::InitialUEIdentity::imsi(identity) = &mut body.initial_ue_identity else {
        panic!("identity")
    };
    identity.0[0].0 = 9;
    assert_eq!(
        procedure.classify_request(&request),
        RequestMatch::DifferentIdentity
    );
    procedure
        .publish_with(at(20), NonZeroU64::new(100).unwrap(), |_| Ok::<_, ()>(()))
        .unwrap();
    let mut completion = decode_uplink_dedicated(&COMPLETE, COMPLETE_BITS).unwrap();
    let ULDCCHMessageType::rrcConnectionSetupComplete(body) = &mut completion.value.message else {
        panic!("completion")
    };
    body.start_list.0.pop();
    procedure.complete(key(0), at(30), &completion).unwrap();
    assert_eq!(procedure.starts().unwrap().get(CnDomain::Cs), Some(0x12345));
    assert_eq!(procedure.starts().unwrap().get(CnDomain::Ps), None);
}
