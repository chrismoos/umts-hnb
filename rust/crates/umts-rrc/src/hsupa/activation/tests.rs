// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::{NonZeroU8, NonZeroU64};
use umts_rrc_codec::decode_downlink_dedicated;
use umts_types::{
    RadioEpoch,
    hsupa::{CodeLayout, PuncturingLimit, Rel6Category, TransportBlockTable},
    time::ChipInstant,
};

#[path = "tests_r7.rs"]
mod r7;

fn construction(
    voice: bool,
    shared: bool,
    maximum: &[u16],
) -> Result<DchCoexistenceActivation, DedicatedError> {
    let dedicated = crate::test_support::dedicated::configuration(voice, true, 10);
    let parameters = ActivationParameters {
        e_rnti: 0x8421,
        transport: TransportConfiguration::new(
            Tti::TwoMilliseconds,
            Rel6Category::new(6).unwrap(),
            TransportBlockTable::Zero,
            CodeLayout::new(maximum).unwrap(),
            PuncturingLimit::new(44).unwrap(),
        )
        .unwrap(),
        control: ControlConfiguration::new(8, 20).unwrap(),
        downlink: DownlinkControl::new(
            OvsfCode::new(256, 10).unwrap(),
            OvsfCode::new(128, 6).unwrap(),
            2,
            None,
            &[],
        )
        .unwrap(),
        references: crate::hsupa::ReferencePowers::single(Etfci::new(50).unwrap(), 20).unwrap(),
        minimum: Etfci::new(0).unwrap(),
        scheduling: SchedulingInformation {
            without_grant: Some(super::super::ReportingPeriod::EveryTti),
            with_grant: None,
            power_offset: 0,
        },
        rv_zero_only: false,
        flows: vec![ScheduledFlow::new(0, 0, 3).unwrap()],
    };
    let downlink = if shared {
        RetainedDownlink::Hsdpa {
            flow: 2,
            logical: Some(1),
        }
    } else {
        RetainedDownlink::Dedicated
    };
    let now = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index: 0,
    };
    DchCoexistenceActivation::new(
        dedicated,
        parameters,
        vec![MappedBearer {
            bearer: 20,
            uplink: UplinkMapping::new(1, 0, 1, 320, 1, true).unwrap(),
            downlink,
        }],
        ScheduledActivation::new(now, 0, NonZeroU8::new(4).unwrap()).unwrap(),
        shared,
    )
}

#[test]
fn coexistence_selection_removes_packet_dch_data_and_preserves_joint_voice_formats() {
    let construction = construction(true, true, &[2, 2]).unwrap();
    let original = construction.dedicated().uplink();
    let selected = construction.uplink();
    let packet = construction.dedicated().bearer(20).unwrap();
    let packet_columns: Vec<_> = original
        .transports()
        .iter()
        .enumerate()
        .filter_map(|(column, transport)| {
            packet
                .mapping
                .uplink
                .iter()
                .any(|mapping| mapping.transport == transport.id)
                .then_some(column)
        })
        .collect();
    assert!(!packet_columns.is_empty());
    assert!(selected.combinations().len() < original.combinations().len());
    assert_eq!(
        selected.combinations().len(),
        construction.source_combinations().len()
    );
    for (row, source) in selected
        .combinations()
        .iter()
        .zip(construction.source_combinations())
    {
        let source_row = &original.combinations()[*source];
        let retained_formats: Vec<_> = source_row
            .formats
            .iter()
            .enumerate()
            .filter_map(|(column, format)| (!packet_columns.contains(&column)).then_some(*format))
            .collect();
        assert_eq!(row.formats, retained_formats);
        for &column in &packet_columns {
            let format = original.transports()[column]
                .formats()
                .nth(source_row.formats[column])
                .unwrap();
            assert_eq!(format.blocks, 0);
        }
    }
    for row in original.combinations() {
        assert!(selected.combinations().iter().any(|retained| {
            row.formats
                .iter()
                .enumerate()
                .filter_map(|(column, format)| {
                    (!packet_columns.contains(&column)).then_some(format)
                })
                .eq(retained.formats.iter())
        }));
    }
}

#[test]
fn rescheduling_preserves_definition_and_rejects_foreign_epoch() {
    let original = construction(true, true, &[2, 2]).unwrap();
    let time = ChipInstant {
        index: 4 * umts_types::time::CHIPS_PER_FRAME,
        ..original.activation().time()
    };
    let activation = ScheduledActivation::new(time, 4, NonZeroU8::new(4).unwrap()).unwrap();
    let later = original.rescheduled(activation).unwrap();
    assert!(original.same_definition(&later));
    assert!(Arc::ptr_eq(original.dedicated(), later.dedicated()));
    assert_eq!(later.activation(), activation);
    assert!(!original.same_definition(&construction(true, true, &[2, 2]).unwrap()));
    let foreign = ScheduledActivation::new(
        ChipInstant {
            epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
            ..time
        },
        4,
        NonZeroU8::new(4).unwrap(),
    )
    .unwrap();
    assert!(original.rescheduled(foreign).is_err());
    let encoded = later.message(TransactionId::new(2).unwrap()).unwrap();
    assert!(decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).is_ok());
}

#[test]
fn edch_only_and_restoration_encode_complete_bearer_transitions() {
    use crate::hsupa::{EdchOnlyActivation, EdchOnlyBearerPlan};
    let original = construction(false, true, &[2, 2]).unwrap();
    let activation = original.activation;
    let definition = Arc::try_unwrap(original.definition).ok().unwrap();
    let mut parameters = definition.parameters;
    parameters.transport = TransportConfiguration::new(
        Tti::TwoMilliseconds,
        Rel6Category::new(6).unwrap(),
        TransportBlockTable::Zero,
        CodeLayout::new(&[2, 2, 4, 4]).unwrap(),
        PuncturingLimit::new(44).unwrap(),
    )
    .unwrap();
    let mappings = definition
        .dedicated
        .bearers()
        .iter()
        .enumerate()
        .map(|(index, bearer)| {
            let bits = definition
                .dedicated
                .uplink()
                .transport(bearer.mapping.uplink[0].transport)
                .unwrap()
                .formats()
                .find(|format| format.blocks != 0 && format.rlc_bits != 0)
                .unwrap()
                .rlc_bits;
            MappedBearer {
                bearer: bearer.id,
                uplink: UplinkMapping::new(index as u8 + 1, 0, index as u8, bits, 1, true).unwrap(),
                downlink: if bearer.domain == Some(CnDomain::Ps) {
                    RetainedDownlink::Hsdpa {
                        flow: 2,
                        logical: Some(1),
                    }
                } else {
                    RetainedDownlink::Dedicated
                },
            }
        })
        .collect::<Vec<_>>();
    let mut incomplete = mappings.clone();
    incomplete.pop();
    assert!(EdchOnlyBearerPlan::new(Arc::clone(&definition.dedicated), incomplete).is_err());
    let plan = EdchOnlyBearerPlan::new(Arc::clone(&definition.dedicated), mappings).unwrap();
    let construction = EdchOnlyActivation::new(plan, parameters, activation, true).unwrap();
    let message = construction
        .message(TransactionId::new(2).unwrap())
        .unwrap();
    let decoded = decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
    let class::DLDCCHMessageType::radioBearerReconfiguration(
        pdu::RadioBearerReconfiguration::later_than_r3(outer),
    ) = decoded.value.message
    else {
        panic!("reconfiguration");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(r5) =
        outer.critical_extensions
    else {
        panic!("r5");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(r6) = r5 else { panic!("r6"); };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::r6(r6) = r6 else { panic!("body"); };
    let body = r6.radio_bearer_reconfiguration_r6;
    assert!(body.ciphering_mode_info.is_none());
    assert!(body.integrity_protection_mode_info.is_none());
    let ie::ULDPCHInfoR6ModeSpecificInfo::fdd(physical) =
        body.ul_dpch_info.unwrap().mode_specific_info
    else {
        panic!("FDD");
    };
    let ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresence::notPresent(absent) =
        physical.dpdch_presence
    else {
        panic!("no DPDCH");
    };
    assert!(!absent.tfci_existence);
    assert!(absent.number_of_fbi_bits.is_none());
    let pdu::RadioBearerReconfigurationR6IEsSpecificationMode::complete(complete) =
        body.specification_mode
    else {
        panic!("complete");
    };
    assert_eq!(
        complete.ul_deleted_trans_ch_info_list.unwrap().0.len(),
        definition.dedicated.uplink().transports().len()
    );
    assert_eq!(
        complete.rb_information_affected_list.unwrap().0.len(),
        definition.dedicated.bearers().len()
    );
    assert!(complete.dl_deleted_trans_ch_info_list.is_none());
    assert!(complete.ul_common_trans_ch_info.is_none());
    assert_eq!(
        complete.ul_add_reconf_trans_ch_info_list.unwrap().0.len(),
        1
    );
    verify_dch_restoration(crate::hsupa::UplinkActivation::EdchOnly(construction));
}

#[test]
fn coexistence_restoration_releases_edch_and_preserves_voice_and_hsdpa() {
    for voice in [false, true] {
        verify_dch_restoration(crate::hsupa::UplinkActivation::Coexistence(
            construction(voice, true, &[2, 2]).unwrap(),
        ));
    }
}

fn verify_dch_restoration(previous: crate::hsupa::UplinkActivation) {
    use crate::{dedicated::ActivationMessage, hsupa::DchRestoration};
    let previous = Arc::new(previous);
    let activation = previous.activation();
    assert!(DchRestoration::new(previous.clone(), activation).is_err());
    let next = ScheduledActivation::new(
        activation.time(),
        activation.cfn(),
        NonZeroU8::new(4).unwrap(),
    )
    .unwrap();
    let restoration = DchRestoration::new(previous.clone(), next).unwrap();
    let encoded = restoration.message(TransactionId::new(1).unwrap()).unwrap();
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let class::DLDCCHMessageType::radioBearerReconfiguration(
        pdu::RadioBearerReconfiguration::later_than_r3(outer),
    ) = decoded.value.message
    else {
        panic!("restoration");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(r5) =
        outer.critical_extensions
    else {
        panic!("r5");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(r6) = r5 else { panic!("r6"); };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::r6(r6) = r6 else { panic!("body"); };
    let body = r6.radio_bearer_reconfiguration_r6;
    assert_eq!(body.activation_time.unwrap().0, next.cfn());
    assert!(body.ciphering_mode_info.is_none());
    assert!(body.integrity_protection_mode_info.is_none());
    assert!(body.new_h_rnti.is_none());
    assert!(body.ul_edch_information.is_none());
    assert!(body.dl_hspdsch_information.is_none());
    let ie::ULDPCHInfoR6ModeSpecificInfo::fdd(physical) =
        body.ul_dpch_info.unwrap().mode_specific_info
    else {
        panic!("FDD");
    };
    let ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresence::present(present) =
        physical.dpdch_presence
    else {
        panic!("restored DPDCH");
    };
    assert_eq!(
        present.number_of_dpdch.0,
        previous.dedicated().physical().uplink_dpdch_count
    );
    assert_eq!(
        present.tfci_existence,
        previous.dedicated().physical().uplink_tfci
    );
    let pdu::RadioBearerReconfigurationR6IEsSpecificationMode::complete(complete) =
        body.specification_mode
    else {
        panic!("complete");
    };
    assert!(complete.ul_common_trans_ch_info.is_some());
    assert_eq!(
        complete.ul_add_reconf_trans_ch_info_list.unwrap().0.len(),
        previous.dedicated().uplink().transports().len()
    );
    let deleted = complete.ul_deleted_trans_ch_info_list.unwrap().0;
    assert_eq!(deleted.len(), previous.parameters().flows.len());
    for (deleted, flow) in deleted.iter().zip(&previous.parameters().flows) {
        assert_eq!(
            *deleted,
            ie::ULTransportChannelIdentityR6::e_dch(ie::EDCHMACDFlowIdentity(flow.identity()))
        );
    }
    let affected = complete.rb_information_affected_list.unwrap().0;
    assert_eq!(affected.len(), previous.mappings().len());
    for affected in affected {
        let bearer = previous.dedicated().bearer(affected.rb_identity.0).unwrap();
        let mapping = &affected.rb_mapping_info.0[0];
        let ie::ULLogicalChannelMappingsR6::oneLogicalChannel(ul) =
            mapping.ul_logical_channel_mappings.as_ref().unwrap()
        else {
            panic!("one UL channel");
        };
        let ie::ULLogicalChannelMappingR6UlTrCHType::dch_rach_usch(ul) = &ul.ul_tr_ch_type else {
            panic!("restored DCH mapping");
        };
        assert_eq!(
            ul.ul_transport_channel_type,
            ie::ULTransportChannelType::dch(ie::TransportChannelIdentity(
                bearer.mapping.uplink[0].transport
            ))
        );
        if bearer.domain == Some(CnDomain::Ps) {
            assert_eq!(
                mapping.dl_logical_channel_mapping_list.as_ref().unwrap().0[0]
                    .dl_transport_channel_type,
                ie::DLTransportChannelTypeR5::hsdsch(ie::MACDFlowIdentity(2))
            );
        }
    }
    assert!(complete.dl_deleted_trans_ch_info_list.is_none());
}

#[test]
fn encoded_coexistence_preserves_downlink_voice_and_security() {
    for voice in [false, true] {
        for shared in [false, true] {
            let construction = construction(voice, shared, &[2, 2]).unwrap();
            let message = construction
                .message(TransactionId::new(2).unwrap())
                .unwrap();
            let decoded = decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
            let class::DLDCCHMessageType::radioBearerReconfiguration(
                pdu::RadioBearerReconfiguration::later_than_r3(outer),
            ) = decoded.value.message
            else {
                panic!("reconfiguration");
            };
            let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(
                r5,
            ) = outer.critical_extensions
            else {
                panic!("r5");
            };
            let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(r6) = r5 else { panic!("r6"); };
            let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::r6(r6) = r6 else { panic!("r6 body"); };
            let body = r6.radio_bearer_reconfiguration_r6;
            assert_eq!(body.activation_time.unwrap().0, 4);
            assert!(body.ciphering_mode_info.is_none());
            assert!(body.integrity_protection_mode_info.is_none());
            assert!(body.ul_dpch_info.is_none());
            assert!(body.dl_hspdsch_information.is_none());
            assert!(body.new_h_rnti.is_none());
            let encoded_rnti = body
                .new_primary_e_rnti
                .unwrap()
                .0
                .iter()
                .take(u16::BITS as usize)
                .fold(0u16, |value, bit| (value << 1) | u16::from(*bit));
            assert_eq!(encoded_rnti, 0x8421);
            let uplink = body.ul_edch_information.unwrap();
            assert!(uplink.mac_es_e_reset_indicator.is_some());
            assert_eq!(
                uplink
                    .scheduling_transm_configuration
                    .unwrap()
                    .serving_grant
                    .unwrap()
                    .value,
                ZERO_GRANT
            );
            let pdu::RadioBearerReconfigurationR6IEsSpecificationMode::complete(complete) =
                body.specification_mode
            else {
                panic!("complete");
            };
            assert!(complete.rb_information_reconfig_list.is_none());
            assert!(complete.dl_deleted_trans_ch_info_list.is_none());
            let deleted = complete.ul_deleted_trans_ch_info_list.as_ref().unwrap();
            let packet = construction.dedicated().bearer(20).unwrap();
            assert_eq!(deleted.0.len(), packet.mapping.uplink.len());
            for (deleted, mapping) in deleted.0.iter().zip(&packet.mapping.uplink) {
                assert_eq!(
                    *deleted,
                    ie::ULTransportChannelIdentityR6::dch_usch(
                        ie::ULTransportChannelIdentityR6DchUsch::new(
                            ie::ULTrCHType::dch,
                            ie::TransportChannelIdentity(mapping.transport),
                        ),
                    )
                );
                assert!(construction.uplink().transport(mapping.transport).is_none());
            }
            let common = complete.ul_common_trans_ch_info.as_ref().unwrap();
            let ie::ULCommonTransChInfoR4ModeSpecificInfo::fdd(common) =
                common.mode_specific_info.as_ref().unwrap()
            else {
                panic!("FDD common transport information");
            };
            assert_eq!(common.ul_tfcs, construction.uplink().wire().unwrap());
            let transports = &complete
                .ul_add_reconf_trans_ch_info_list
                .as_ref()
                .unwrap()
                .0;
            let retained = construction.uplink().transports();
            assert_eq!(transports.len(), retained.len() + 1);
            for (encoded, retained) in transports.iter().zip(retained) {
                let ie::ULAddReconfTransChInformationR6::dch_usch(encoded) = encoded else {
                    panic!("retained DCH");
                };
                assert_eq!(encoded.transport_channel_identity.0, retained.id);
                assert_eq!(
                    encoded.transport_format_set,
                    retained.wire_uplink().unwrap()
                );
            }
            assert!(matches!(
                transports.last(),
                Some(ie::ULAddReconfTransChInformationR6::e_dch(_))
            ));
            let affected = complete.rb_information_affected_list.unwrap().0;
            assert_eq!(affected.len(), 1);
            assert_eq!(affected[0].rb_identity.0, 20);
            let option = &affected[0].rb_mapping_info.0[0];
            let dl = &option.dl_logical_channel_mapping_list.as_ref().unwrap().0[0];
            assert_eq!(
                matches!(
                    dl.dl_transport_channel_type,
                    ie::DLTransportChannelTypeR5::hsdsch(_)
                ),
                shared
            );
            let ie::ULLogicalChannelMappingsR6::oneLogicalChannel(ul) =
                option.ul_logical_channel_mappings.as_ref().unwrap()
            else {
                panic!("mapping");
            };
            let ie::ULLogicalChannelMappingR6UlTrCHType::e_dch(edch) = &ul.ul_tr_ch_type else {
                panic!("E-DCH");
            };
            assert_eq!(edch.ddi.0, 1);
            assert_eq!(edch.logical_channel_identity.0, 1);
            assert_eq!(edch.e_dch_mac_d_flow_identity.0, 0);
            assert_eq!(edch.rlc_pdu_size_list.0.len(), 1);
        }
    }
}

#[test]
fn four_code_layout_cannot_reuse_the_legacy_dpdch_configuration() {
    assert!(matches!(
        construction(false, true, &[2, 2, 4, 4]),
        Err(DedicatedError::Physical)
    ));
}

#[test]
fn coexistence_activation_retains_transaction_ownership_on_publication_failure() {
    use crate::dedicated::{PublicationError, ReconfigurationPhase};
    use umts_rlc::acknowledged::SduReceipt;
    use umts_types::UeKey;
    let at = |index| ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    };
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 2,
        generation: NonZeroU64::MIN,
    };
    let construction = Arc::new(crate::hsupa::UplinkActivation::Coexistence(
        construction(true, true, &[2, 2]).unwrap(),
    ));
    let mut procedure = crate::hsupa::ActivationProcedure::new(
        key,
        TransactionId::new(2).unwrap(),
        Arc::clone(&construction),
        at(0),
        at(100),
        NonZeroU64::new(300_000).unwrap(),
    )
    .unwrap();
    assert!(Arc::ptr_eq(procedure.construction(), &construction));
    assert_eq!(procedure.key(), key);
    assert_eq!(
        procedure.publish_with(at(1), |_| Err::<SduReceipt, _>("full")),
        Err(PublicationError::Publication("full"))
    );
    assert_eq!(procedure.phase(), ReconfigurationPhase::Prepared);
    assert!(procedure.receipt().is_none());
    assert!(procedure.expire(at(100)).unwrap());
    assert_eq!(
        procedure.phase(),
        ReconfigurationPhase::AbortedBeforePublication
    );
    assert!(
        procedure
            .publish_with(at(101), |_| -> Result<SduReceipt, ()> {
                panic!("expired transaction cannot publish")
            })
            .is_err()
    );
}
