// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::dedicated as fixture;
use crate::{
    establishment::TransactionId,
    transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval},
};
use std::{
    num::{NonZeroU8, NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_rlc::acknowledged::{AmEntity, AmLimits, ReceiveTime};
use umts_rrc_codec::{DecodedUplinkDedicated, decode_downlink_dedicated, decode_uplink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{
    RadioEpoch, UeKey,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

mod release;
mod resume;

#[test]
fn singleton_transport_omits_mac_header_but_shared_transport_preserves_ids() {
    let (ul, dl, mut bearers, physical) = fixture::components(false, false, 10);
    bearers.retain(|bearer| bearer.id == 1);
    let singleton = Rel99Configuration::new(ul, dl, bearers, physical).unwrap();
    for (direction, transport) in [(DchDirection::Uplink, 5), (DchDirection::Downlink, 10)] {
        assert!(!singleton.multiplexed(direction, transport).unwrap());
        assert!(
            singleton
                .bearer(1)
                .unwrap()
                .mappings(direction)
                .all(|mapping| mapping.logical.is_none())
        );
    }
    let shared = fixture::configuration(false, false, 10);
    for (direction, transport) in [(DchDirection::Uplink, 5), (DchDirection::Downlink, 10)] {
        assert!(shared.multiplexed(direction, transport).unwrap());
        for id in 1..=3 {
            assert!(
                shared
                    .bearer(id)
                    .unwrap()
                    .mappings(direction)
                    .all(|mapping| mapping.logical == Some(id))
            );
        }
    }
    let (ul, dl, mut bearers, physical) = fixture::components(false, false, 10);
    bearers.retain(|bearer| bearer.id == 1);
    bearers[0].mapping.uplink[0].logical = Some(0);
    assert!(matches!(
        Rel99Configuration::new(ul, dl, bearers, physical),
        Err(DedicatedError::Mapping)
    ));
}

fn time(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn key() -> UeKey {
    UeKey {
        epoch: time(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    }
}
fn transaction() -> TransactionId {
    TransactionId::new(2).unwrap()
}
fn body(encoded: umts_rrc_codec::EncodedRrc) -> pdu::RadioBearerSetupR3IEs {
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let class::DLDCCHMessageType::radioBearerSetup(pdu::RadioBearerSetup::r3(value)) =
        decoded.value.message
    else {
        panic!("Rel99 RBS")
    };
    value.radio_bearer_setup_r3
}
fn initial(voice: bool, packet: bool) -> InitialDchSetup {
    InitialDchSetup::new(
        fixture::configuration(voice, packet, 10),
        fixture::retained(),
        fixture::rabs(voice, packet),
    )
    .unwrap()
}

#[test]
fn dedicated_secondary_scrambling_is_explicit_and_range_checked() {
    let source = fixture::configuration(false, true, 10);
    let mut physical = source.physical().clone();
    for invalid in [0, 16, u8::MAX] {
        physical.secondary_scrambling = Some(invalid);
        assert_eq!(physical.validate(), Err(DedicatedError::Physical));
    }
    for secondary in [1, 15] {
        physical.secondary_scrambling = Some(secondary);
        let (_, links) = physical.downlink_wire(true).unwrap();
        let Some(ie::DLDPCHInfoPerRL::fdd(link)) = &links.0[0].dl_dpch_info_per_rl else {
            panic!("FDD DPCH");
        };
        assert_eq!(
            link.p_cpich_usage_for_channel_est,
            ie::PCPICHUsageForChannelEst::mayBeUsed
        );
        assert!(link.dl_channelisation_code_list.0.iter().all(|code| {
            code.secondary_scrambling_code.as_ref().map(|code| code.0) == Some(secondary)
        }));
    }
}

fn complete(id: u8) -> DecodedUplinkDedicated {
    let value = class::ULDCCHMessage::new(
        None,
        class::ULDCCHMessageType::radioBearerSetupComplete(pdu::RadioBearerSetupComplete::new(
            ie::RRCTransactionIdentifier(id),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )),
    );
    let bytes = rasn::uper::encode(&value).unwrap();
    decode_uplink_dedicated(&bytes, bytes.len() * 8).unwrap()
}

fn assert_directional_tfs(wire: &ie::TransportFormatSet, expected: ie::LogicalChannelList) {
    let ie::TransportFormatSet::dedicatedTransChTFS(tfs) = wire else {
        panic!("DCH")
    };
    let groups = match &tfs.tti {
        ie::DedicatedTransChTFSTti::tti10(groups)
        | ie::DedicatedTransChTFSTti::tti20(groups)
        | ie::DedicatedTransChTFSTti::tti40(groups)
        | ie::DedicatedTransChTFSTti::tti80(groups) => groups,
        ie::DedicatedTransChTFSTti::dynamic(_) => panic!("unexpected dynamic TTI"),
    };
    assert!(
        groups
            .0
            .iter()
            .all(|group| group.logical_channel_list == expected)
    );
}

fn assert_bearer_mapping_rlc_sizes_are_all(mapping: &ie::RBMappingInfo) {
    // The dedicated option is first. A FACH/RACH option after it uses an explicit size list.
    for option in mapping.0.iter().take(1) {
        let Some(uplink) = option.ul_logical_channel_mappings.as_ref() else {
            panic!("missing UL mapping")
        };
        match uplink {
            ie::ULLogicalChannelMappings::oneLogicalChannel(mapping) => {
                assert_eq!(
                    mapping.rlc_size_list,
                    ie::ULLogicalChannelMappingRlcSizeList::allSizes(())
                );
            }
            ie::ULLogicalChannelMappings::twoLogicalChannels(mappings) => {
                for mapping in &mappings.ul_logical_channel_mapping {
                    assert_eq!(
                        mapping.rlc_size_list,
                        ie::ULLogicalChannelMappingRlcSizeList::allSizes(())
                    );
                }
            }
        }
    }
}

fn receipt() -> umts_rlc::acknowledged::SduReceipt {
    let config = crate::establishment::SignallingProfile.network_am_configuration(
        NonZeroUsize::new(42).unwrap(),
        NonZeroUsize::new(18).unwrap(),
        0,
        0,
    );
    let mut entity = AmEntity::new(
        config,
        AmLimits {
            maximum_sdu_bytes: NonZeroUsize::new(1024).unwrap(),
            queued_sdus: NonZeroUsize::new(4).unwrap(),
            maximum_buffer_bytes: 1_000_000,
        },
        time(0),
    )
    .unwrap();
    entity.enqueue_tracked(&[0x12]).unwrap()
}

#[test]
fn encoded_setup_uses_directional_tfs_and_pairs_bearer_rlc_sizes() {
    let combined = {
        let previous = fixture::configuration(true, false, 10);
        let target = fixture::configuration(true, true, 10);
        let activation =
            ScheduledActivation::new(time(0), 252, NonZeroU8::new(8).unwrap()).unwrap();
        body(
            ExistingDchSetup::new(previous, target, fixture::rabs(false, true), activation)
                .unwrap()
                .message(transaction())
                .unwrap(),
        )
    };
    for value in [
        body(initial(true, false).message(transaction()).unwrap()),
        body(initial(false, true).message(transaction()).unwrap()),
        combined,
    ] {
        for transport in value.ul_add_reconf_trans_ch_info_list.unwrap().0 {
            assert_directional_tfs(
                &transport.transport_format_set,
                ie::LogicalChannelList::configured(()),
            );
        }
        for transport in value.dl_add_reconf_trans_ch_info_list.unwrap().0 {
            let ie::DLAddReconfTransChInformationTfsSignallingMode::explicit_config(tfs) =
                transport.tfs_signalling_mode
            else {
                panic!("missing DL TFS")
            };
            assert_directional_tfs(&tfs, ie::LogicalChannelList::allSizes(()));
        }
        for rab in value.rab_information_setup_list.unwrap().0 {
            for bearer in rab.rb_information_setup_list.0 {
                assert_bearer_mapping_rlc_sizes_are_all(&bearer.rb_mapping_info);
            }
        }
    }
}

#[test]
fn initial_setup_has_now_complete_physical_information_and_mapping_only_existing_srbs() {
    for (voice, packet) in [(true, false), (false, true)] {
        let value = body(initial(voice, packet).message(transaction()).unwrap());
        assert!(value.activation_time.is_none());
        assert!(value.srb_information_setup_list.is_none());
        assert_eq!(
            value
                .rb_information_affected_list
                .unwrap()
                .0
                .iter()
                .map(|rb| rb.rb_identity.0)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(value.ul_channel_requirement.is_some());
        assert!(value.dl_common_information.is_some());
        assert!(value.dl_information_per_rl_list.is_some());
        let rabs = value.rab_information_setup_list.unwrap().0;
        assert_eq!(rabs.len(), usize::from(voice) + usize::from(packet));
        for rab in rabs {
            match rab.rab_info.cn_domain_identity {
                ie::CNDomainIdentity::cs_domain => {
                    assert_eq!(rab.rb_information_setup_list.0[0].rb_identity.0, 10);
                    assert!(rab.rb_information_setup_list.0[0].pdcp_info.is_none());
                    assert!(rab.rab_info.nas_synchronisation_indicator.is_some());
                }
                ie::CNDomainIdentity::ps_domain => {
                    assert_eq!(rab.rb_information_setup_list.0[0].rb_identity.0, 20);
                    assert!(rab.rb_information_setup_list.0[0].pdcp_info.is_some());
                }
            }
        }
    }
}
#[test]
fn existing_setup_retains_the_other_domain_and_omits_unchanged_physical_configuration() {
    let previous = fixture::configuration(true, false, 10);
    let initial = InitialDchSetup::new(
        previous.clone(),
        fixture::retained(),
        fixture::rabs(true, false),
    )
    .unwrap();
    let initial = body(initial.message(transaction()).unwrap());
    assert!(initial.activation_time.is_none());
    assert_eq!(initial.rab_information_setup_list.unwrap().0.len(), 1);
    let target = fixture::configuration(true, true, 10);
    let activation = ScheduledActivation::new(time(0), 252, NonZeroU8::new(8).unwrap()).unwrap();
    let setup = ExistingDchSetup::new(
        previous.clone(),
        target.clone(),
        fixture::rabs(false, true),
        activation,
    )
    .unwrap();
    assert!(Arc::ptr_eq(setup.previous(), &previous));
    assert!(Arc::ptr_eq(setup.configuration(), &target));
    let value = body(setup.message(transaction()).unwrap());
    assert_eq!(value.activation_time.unwrap().0, 4);
    assert!(value.ul_channel_requirement.is_none());
    assert!(value.dl_common_information.is_none());
    assert!(value.dl_information_per_rl_list.is_none());
    assert!(value.rb_information_affected_list.is_none());
    assert_eq!(
        value.rab_information_setup_list.unwrap().0[0]
            .rab_info
            .cn_domain_identity,
        ie::CNDomainIdentity::ps_domain
    );
}
#[test]
fn channel_identity_changes_delete_old_transport_without_recreating_retained_rlc() {
    let previous = fixture::configuration(true, false, 10);
    let target = fixture::configuration(true, true, 11);
    let activation = ScheduledActivation::new(time(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
    let setup =
        ExistingDchSetup::new(previous, target, fixture::rabs(false, true), activation).unwrap();
    let value = body(setup.message(transaction()).unwrap());
    assert_eq!(
        value.dl_deleted_trans_ch_info_list.unwrap().0[0]
            .dl_transport_channel_identity
            .0,
        10
    );
    assert!(value.srb_information_setup_list.is_none());
}
#[test]
fn fixed_am_size_changes_and_unaccounted_or_duplicated_bearers_are_rejected() {
    let configuration = fixture::configuration(true, true, 10);
    let mut retained = fixture::retained();
    retained[1].uplink_pdu_bits = 160;
    assert!(matches!(
        InitialDchSetup::new(configuration.clone(), retained, fixture::rabs(true, false)),
        Err(DedicatedError::Format)
    ));
    assert!(matches!(
        InitialDchSetup::new(
            configuration.clone(),
            fixture::retained(),
            fixture::rabs(true, false)
        ),
        Err(DedicatedError::Bearer)
    ));
    let mut rabs = fixture::rabs(true, true);
    rabs[1].bearers = vec![10];
    assert!(matches!(
        InitialDchSetup::new(configuration, fixture::retained(), rabs),
        Err(DedicatedError::Bearer)
    ));
}

#[test]
fn mixed_new_rab_domains_are_rejected_before_encoding() {
    assert!(matches!(
        InitialDchSetup::new(
            fixture::configuration(true, true, 10),
            fixture::retained(),
            fixture::rabs(true, true),
        ),
        Err(DedicatedError::Bearer)
    ));
}

#[test]
fn invalid_multiplexing_rlc_sizes_and_physical_units_are_rejected_before_encoding() {
    for mutation in 0..4 {
        let (ul, dl, mut bearers, mut physical) = fixture::components(true, true, 10);
        match mutation {
            0 => bearers[1].mapping.uplink[0].logical = None,
            1 => bearers[1].mapping.uplink[0].logical = Some(1),
            2 => physical.dpcch_offset_db = -61,
            _ => physical.frame_offset_256_chips = 1,
        }
        assert!(Rel99Configuration::new(ul, dl, bearers, physical).is_err());
    }
}
#[test]
fn doff_initial_cfn_uses_chips_and_wraps_without_using_receive_sample_time() {
    let mut physical = fixture::physical();
    physical.default_offset_512_chips = 75;
    assert_eq!(physical.cfn_for_sfn(0), Ok(255));
    assert_eq!(physical.cfn_for_sfn(4095), Ok(254));
    assert!(physical.cfn_for_sfn(4096).is_err());
    physical.frame_offset_256_chips = 1;
    assert!(physical.cfn_for_sfn(0).is_err());
}
#[test]
fn zero_rlc_size_and_ctfc_order_match_the_canonical_transport_plan() {
    assert_eq!(
        DchFormat {
            rlc_bits: 0,
            blocks: 1
        }
        .transport_bits(true),
        0
    );
    assert_eq!(
        DchFormat {
            rlc_bits: 144,
            blocks: 1
        }
        .transport_bits(true),
        148
    );
    let plan = fixture::configuration(true, true, 10);
    let expected = [
        0, 1, 14, 21, 28, 41, 60, 61, 74, 81, 88, 101, 120, 121, 134, 141, 148, 161, 180, 181, 194,
        201, 208, 221,
    ];
    assert_eq!(plan.uplink().ctfc_values(), expected);
    let wire = plan.uplink().wire().unwrap();
    let ie::TFCS::normalTFCI_Signalling(ie::ExplicitTFCSConfiguration::complete(value)) = wire
    else {
        panic!("complete TFCS")
    };
    let ie::TFCSReconfAddCtfcSize::ctfc8Bit(rows) = value.ctfc_size else {
        panic!("8-bit CTFC")
    };
    assert_eq!(
        rows.0
            .iter()
            .map(|row| u32::from(row.ctfc8))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        rows.0
            .iter()
            .all(|row| row.power_offset_information.is_some())
    );
}
#[test]
fn dedicated_size_encodings_cover_bit_ranges_and_actual_block_counts() {
    for size in [0, 81, 127, 128, 129, 255, 256, 257, 1023, 1024, 1025, 5000] {
        let transport = DchTransport {
            id: 1,
            interval: TransportInterval::Ms20,
            coding: ChannelCoding::Convolutional(ConvolutionalRate::Third),
            crc: CrcSize::Bits12,
            rate_matching_attribute: 256,
            groups: vec![DchFormatGroup {
                rlc_bits: size,
                blocks: vec![0, 1, 2, 17, 18, 512],
            }],
        };
        let wire = transport.wire_downlink().unwrap();
        let bytes = rasn::uper::encode(&wire).unwrap();
        assert_eq!(
            rasn::uper::decode::<ie::TransportFormatSet>(&bytes).unwrap(),
            wire
        );
        let ie::TransportFormatSet::dedicatedTransChTFS(tfs) = wire else {
            panic!("DCH")
        };
        let ie::DedicatedTransChTFSTti::tti20(groups) = tfs.tti else {
            panic!("20ms")
        };
        assert_eq!(
            groups.0[0].number_of_tb_size_list[2],
            ie::NumberOfTransportBlocks::small(2)
        );
        assert_eq!(
            groups.0[0].number_of_tb_size_list[5],
            ie::NumberOfTransportBlocks::large(512)
        );
    }
}

#[test]
fn dch_transport_serialization_matches_direction_for_cs_ps_and_combined_plans() {
    for (voice, packet) in [(true, false), (false, true), (true, true)] {
        let configuration = fixture::configuration(voice, packet, 10);
        for transport in configuration.uplink().transports() {
            assert_directional_tfs(
                &transport.wire_uplink().unwrap(),
                ie::LogicalChannelList::configured(()),
            );
        }
        for transport in configuration.downlink().transports() {
            assert_directional_tfs(
                &transport.wire_downlink().unwrap(),
                ie::LogicalChannelList::allSizes(()),
            );
        }
    }
}

#[test]
fn missing_uplink_empty_tfc_and_tti_projections_are_rejected() {
    let plan = fixture::configuration(true, true, 10);
    let transports = plan.uplink().transports().to_vec();
    let mut rows = plan.uplink().combinations().to_vec();
    rows.remove(0);
    assert!(matches!(
        DchTfcs::new(DchDirection::Uplink, transports.clone(), rows),
        Err(DedicatedError::Combination)
    ));
    let rows = vec![
        plan.uplink().combinations()[0].clone(),
        plan.uplink().combinations().last().unwrap().clone(),
    ];
    assert!(matches!(
        DchTfcs::new(DchDirection::Uplink, transports, rows),
        Err(DedicatedError::Combination)
    ));
}
#[test]
fn scheduled_activation_requires_common_old_and_new_tti_boundary() {
    let activation = ScheduledActivation::new(time(0), 0, NonZeroU8::new(3).unwrap()).unwrap();
    assert!(matches!(
        ExistingDchSetup::new(
            fixture::configuration(true, false, 10),
            fixture::configuration(true, true, 10),
            fixture::rabs(false, true),
            activation
        ),
        Err(DedicatedError::Activation)
    ));
}

#[test]
fn unrestricted_activation_preserves_the_full_cfn_cycle_and_absolute_time() {
    let start = time(17 * umts_types::time::CHIPS_PER_FRAME);
    for current in [0_u8, 127, 255] {
        for distance in 1..=u8::MAX {
            let activation =
                ScheduledActivation::new(start, current, NonZeroU8::new(distance).unwrap())
                    .unwrap();
            assert_eq!(activation.cfn(), current.wrapping_add(distance));
            assert_eq!(
                activation.time().index,
                start.index + u64::from(distance) * umts_types::time::CHIPS_PER_FRAME
            );
        }
    }
    assert!(ScheduledActivation::new(time(u64::MAX), 0, NonZeroU8::MIN).is_err());
}

#[test]
fn shorter_tti_empty_projection_does_not_require_preserving_unrelated_longer_formats() {
    let transports = [
        TransportInterval::Ms10,
        TransportInterval::Ms20,
        TransportInterval::Ms40,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, interval)| DchTransport {
        id: index as u8 + 1,
        interval,
        coding: ChannelCoding::Convolutional(ConvolutionalRate::Half),
        crc: CrcSize::Bits16,
        rate_matching_attribute: 200,
        groups: vec![DchFormatGroup {
            rlc_bits: 16,
            blocks: vec![0, 1],
        }],
    })
    .collect();
    let rows = [[0, 0, 0], [1, 0, 0], [0, 1, 0], [0, 0, 1], [1, 1, 1]]
        .into_iter()
        .map(|formats| Combination {
            formats: formats.to_vec(),
            gain: Some(GainFactors {
                control: 15,
                data: 15,
            }),
        })
        .collect();
    assert!(DchTfcs::new(DchDirection::Uplink, transports, rows).is_ok());
}

#[test]
fn parity_only_transport_still_requires_nonzero_data_channel_gain() {
    let transport = DchTransport {
        id: 1,
        interval: TransportInterval::Ms20,
        coding: ChannelCoding::Convolutional(ConvolutionalRate::Third),
        crc: CrcSize::Bits12,
        rate_matching_attribute: 200,
        groups: vec![DchFormatGroup {
            rlc_bits: 0,
            blocks: vec![1],
        }],
    };
    let rows = vec![Combination {
        formats: vec![0],
        gain: Some(GainFactors {
            control: 15,
            data: 0,
        }),
    }];
    assert!(matches!(
        DchTfcs::new(DchDirection::Uplink, vec![transport.clone()], rows.clone()),
        Err(DedicatedError::Gains)
    ));
    assert!(
        DchTfcs::new(
            DchDirection::Uplink,
            vec![DchTransport {
                crc: CrcSize::None,
                ..transport
            }],
            rows
        )
        .is_ok()
    );
}
#[test]
fn publication_does_not_complete_the_procedure_and_failure_preserves_retry() {
    let mut procedure = RadioBearerSetupProcedure::initial(
        key(),
        transaction(),
        initial(true, false),
        time(0),
        time(100),
        NonZeroU64::new(1000).unwrap(),
    )
    .unwrap();
    assert_eq!(
        procedure.publish_with(time(1), |_| Err::<umts_rlc::acknowledged::SduReceipt, _>(
            "full"
        )),
        Err(SetupPublicationError::Publication("full"))
    );
    assert_eq!(procedure.phase(), SetupPhase::Prepared);
    assert!(procedure.receipt().is_none());
    procedure
        .publish_with(time(2), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::new(time(10), time(2000)).unwrap(),
            &complete(2)
        ),
        Ok(SetupResponse::Complete)
    );
    assert_eq!(procedure.phase(), SetupPhase::Complete);
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(2001)), &complete(2)),
        Ok(SetupResponse::Duplicate)
    );
}

#[test]
fn invalid_response_counter_fields_cannot_complete_or_poison_a_retry() {
    let mut procedure = RadioBearerSetupProcedure::initial(
        key(),
        transaction(),
        initial(true, false),
        time(0),
        time(100),
        NonZeroU64::new(1000).unwrap(),
    )
    .unwrap();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let mut invalid = complete(transaction().get());
    let class::ULDCCHMessageType::radioBearerSetupComplete(body) = &mut invalid.value.message
    else {
        unreachable!()
    };
    body.count_c_activation_time = Some(ie::ActivationTime(7));
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(10)), &invalid),
        Err(DedicatedError::Activation)
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert!(procedure.completion_counters().is_none());
    assert!(procedure.completion().is_none());
    let valid = complete(transaction().get());
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(11)), &valid),
        Ok(SetupResponse::Complete)
    );
    assert!(procedure.completion_counters().is_some());
}
#[test]
fn user_admission_failure_does_not_commit_completion_or_advance_the_service_clock() {
    let mut procedure = RadioBearerSetupProcedure::initial(
        key(),
        transaction(),
        initial(true, false),
        time(0),
        time(100),
        NonZeroU64::new(1000).unwrap(),
    )
    .unwrap();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let response = complete(transaction().get());
    assert!(matches!(
        procedure.response_with(
            key(),
            ReceiveTime::simultaneous(time(20)),
            &response,
            |_| Err::<(), _>(7)
        ),
        Err(SetupResponseError::Admission(7))
    ));
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert!(procedure.completion().is_none());
    assert!(procedure.completion_counters().is_none());
    assert_eq!(
        procedure
            .response_with(
                key(),
                ReceiveTime::simultaneous(time(10)),
                &response,
                |_| Ok::<_, ()>(9)
            )
            .unwrap(),
        (SetupResponse::Complete, Some(9))
    );
    assert_eq!(
        procedure
            .response_with(
                key(),
                ReceiveTime::simultaneous(time(30)),
                &response,
                |_| -> Result<(), ()> {
                    panic!("duplicate response cannot install user state twice")
                }
            )
            .unwrap(),
        (SetupResponse::Duplicate, None)
    );
    assert!(matches!(
        procedure.response(key(), ReceiveTime::simultaneous(time(29)), &response),
        Err(DedicatedError::Time)
    ));
}
#[test]
fn expiration_before_publication_is_distinct_from_unknown_peer_state_after_admission() {
    for published in [false, true] {
        let mut procedure = RadioBearerSetupProcedure::initial(
            key(),
            transaction(),
            initial(false, true),
            time(0),
            time(100),
            NonZeroU64::new(1000).unwrap(),
        )
        .unwrap();
        if published {
            procedure
                .publish_with(time(2), |_| Ok::<_, ()>(receipt()))
                .unwrap();
        }
        assert_eq!(procedure.expire(time(2000)), Ok(true));
        assert_eq!(
            procedure.phase(),
            if published {
                SetupPhase::RecoveryRequired
            } else {
                SetupPhase::AbortedBeforePublication
            }
        );
    }
}

#[test]
fn failure_retains_the_cause_and_partial_success_list_without_claiming_rollback() {
    let mut procedure = RadioBearerSetupProcedure::initial(
        key(),
        transaction(),
        initial(true, false),
        time(0),
        time(100),
        NonZeroU64::new(1000).unwrap(),
    )
    .unwrap();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let value = class::ULDCCHMessage::new(
        None,
        class::ULDCCHMessageType::radioBearerSetupFailure(pdu::RadioBearerSetupFailure::new(
            ie::RRCTransactionIdentifier(2),
            ie::FailureCauseWithProtErr::configurationUnsupported(()),
            Some(ie::RBIdentityList(vec![ie::RBIdentity(10)])),
            None,
        )),
    );
    let bytes = rasn::uper::encode(&value).unwrap();
    let decoded = decode_uplink_dedicated(&bytes, bytes.len() * 8).unwrap();
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(2)), &decoded),
        Ok(SetupResponse::Failure)
    );
    assert_eq!(procedure.phase(), SetupPhase::FailureReported);
    assert_eq!(
        procedure
            .failure()
            .unwrap()
            .potentially_succesful_bearer_list
            .as_ref()
            .unwrap()
            .0[0]
            .0,
        10
    );
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(3)), &decoded),
        Ok(SetupResponse::Duplicate)
    );
}
#[test]
fn stale_generation_wrong_transaction_and_before_activation_cannot_confirm_setup() {
    let activation = ScheduledActivation::new(time(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
    let request = ExistingDchSetup::new(
        fixture::configuration(true, false, 10),
        fixture::configuration(true, true, 10),
        fixture::rabs(false, true),
        activation,
    )
    .unwrap();
    let mut procedure = RadioBearerSetupProcedure::existing(
        key(),
        transaction(),
        request,
        time(0),
        time(100),
        NonZeroU64::new(10 * CHIPS_PER_FRAME).unwrap(),
    )
    .unwrap();
    procedure
        .publish_with(time(1), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(
        procedure.response(stale, ReceiveTime::simultaneous(time(200)), &complete(2)),
        Err(DedicatedError::Identity)
    );
    assert_eq!(
        procedure.response(key(), ReceiveTime::simultaneous(time(200)), &complete(2)),
        Err(DedicatedError::Time)
    );
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::simultaneous(activation.time()),
            &complete(1)
        ),
        Err(DedicatedError::Transaction)
    );
    assert_eq!(
        procedure.response(
            key(),
            ReceiveTime::simultaneous(activation.time()),
            &complete(2)
        ),
        Ok(SetupResponse::Complete)
    );
}
