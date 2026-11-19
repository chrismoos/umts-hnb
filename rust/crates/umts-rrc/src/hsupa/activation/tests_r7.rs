// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn evolved(voice: bool) -> DchCoexistenceActivation {
    let mut activation = construction(voice, true, &[2, 2]).unwrap();
    Arc::get_mut(&mut activation.definition).unwrap().mappings[0].downlink =
        RetainedDownlink::Ehs {
            queue: 5,
            logical: 3,
        };
    activation
}

fn body(message: class::DLDCCHMessage) -> pdu::RadioBearerReconfigurationR7IEs {
    let class::DLDCCHMessageType::radioBearerReconfiguration(
        pdu::RadioBearerReconfiguration::later_than_r3(outer),
    ) = message.message
    else {
        panic!("expected reconfiguration");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(extension) =
        outer.critical_extensions
    else {
        panic!("expected post R4");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(extension) = extension else {
        panic!("expected post R5");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(extension) = extension else {
        panic!("expected post R6");
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r7(extension) = extension else {
        panic!("expected R7");
    };
    extension.radio_bearer_reconfiguration_r7
}

fn check_mapping(body: &pdu::RadioBearerReconfigurationR7IEs, edch: bool) {
    assert!(body.dl_hspdsch_information.is_none());
    assert!(body.dl_common_information.is_none());
    assert!(body.ciphering_mode_info.is_none());
    assert!(body.integrity_protection_mode_info.is_none());
    let pdu::RadioBearerReconfigurationR7IEsSpecificationMode::complete(specification) =
        &body.specification_mode
    else {
        panic!("expected complete");
    };
    let rb = &specification
        .rb_information_affected_list
        .as_ref()
        .unwrap()
        .0[0];
    assert_eq!(rb.rb_identity.0, 20);
    let mapping = &rb.rb_mapping_info.0[0];
    let dl = &mapping.dl_logical_channel_mapping_list.as_ref().unwrap().0[0];
    assert!(matches!(
        dl.dl_transport_channel_type,
        ie::DLTransportChannelTypeR7::hsdsch(ie::DLTransportChannelTypeR7Hsdsch::mac_ehs(
            ie::MACEhsQueueId(5)
        ))
    ));
    assert_eq!(dl.logical_channel_identity.as_ref().unwrap().0, 3);
    let ie::ULLogicalChannelMappingsR6::oneLogicalChannel(ul) =
        mapping.ul_logical_channel_mappings.as_ref().unwrap()
    else {
        panic!("expected one UL logical channel");
    };
    assert_eq!(
        matches!(
            ul.ul_tr_ch_type,
            ie::ULLogicalChannelMappingR6UlTrCHType::e_dch(_)
        ),
        edch
    );
}

#[test]
fn activation_and_restoration_explicitly_retain_ehs_without_resetting_downlink() {
    use crate::{
        dedicated::ActivationMessage,
        hsupa::{DchRestoration, UplinkActivation},
    };
    for voice in [false, true] {
        let activation = evolved(voice);
        let encoded = activation.message(TransactionId::new(1).unwrap()).unwrap();
        let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        assert_eq!(
            *decoded.value,
            activation.value(TransactionId::new(1).unwrap()).unwrap()
        );
        let active = body(*decoded.value);
        check_mapping(&active, true);
        let ie::ULEDCHInformationR7ModeSpecificInfo::fdd(uplink) =
            active.ul_edch_information.unwrap().mode_specific_info
        else {
            panic!("expected FDD E-DCH");
        };
        let control = uplink.e_dpcch_info.unwrap();
        assert_eq!(control.e_dpcch_dpcch_power_offset.0, 8);
        let data = uplink.e_dpdch_info.unwrap();
        assert_eq!(data.reference_e_tfcis.0[0].reference_e_tfci, 50);
        assert_eq!(data.reference_e_tfcis.0[0].reference_e_tfci_po_r7, 20);
        assert_eq!(
            uplink
                .scheduling_transm_configuration
                .unwrap()
                .serving_grant
                .unwrap()
                .value,
            ZERO_GRANT
        );
        let link = &active.dl_information_per_rl_list.unwrap().0[0];
        let ie::DLInformationPerRLR7ModeSpecificInfo::fdd(fdd) = &link.mode_specific_info else {
            panic!("expected FDD radio link");
        };
        assert!(fdd.serving_hsdsch_rl_indicator && fdd.serving_edch_rl_indicator);
        assert!(link.e_agch_information.is_some());
        let ie::DLInformationPerRLR7ModeSpecificInfo2::fdd(feedback) = &link.mode_specific_info2
        else {
            panic!("expected FDD feedback");
        };
        assert!(feedback.e_hich_info.is_some());
        let previous = Arc::new(UplinkActivation::Coexistence(activation));
        let prior_time = previous.activation();
        let next = ScheduledActivation::new(
            prior_time.time(),
            prior_time.cfn(),
            NonZeroU8::new(4).unwrap(),
        )
        .unwrap();
        let restoration = DchRestoration::new(previous, next).unwrap();
        let encoded = restoration.message(TransactionId::new(2).unwrap()).unwrap();
        let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        let restored = body(*decoded.value);
        check_mapping(&restored, false);
        assert!(restored.ul_edch_information.is_none());
        let pdu::RadioBearerReconfigurationR7IEsSpecificationMode::complete(specification) =
            restored.specification_mode
        else {
            panic!("expected complete");
        };
        assert!(
            specification
                .ul_deleted_trans_ch_info_list
                .unwrap()
                .0
                .iter()
                .all(|item| matches!(item, ie::ULTransportChannelIdentityR6::e_dch(_)))
        );
    }
}

#[test]
fn evolved_mapping_ranges_and_mixed_header_types_are_rejected() {
    for (queue, logical) in [(8, 1), (0, 0), (0, 16)] {
        let mut activation = evolved(false);
        Arc::get_mut(&mut activation.definition).unwrap().mappings[0].downlink =
            RetainedDownlink::Ehs { queue, logical };
        assert!(matches!(
            activation.value(TransactionId::new(0).unwrap()),
            Err(DedicatedError::Mapping)
        ));
    }
    let activation = evolved(false);
    let first = activation.mappings()[0];
    let second = MappedBearer {
        bearer: 21,
        downlink: RetainedDownlink::Hsdpa {
            flow: 0,
            logical: Some(4),
        },
        ..first
    };
    assert!(super::super::r7::validate_mappings(&[first, second], true).is_err());
    assert!(super::super::r7::validate_mappings(&[first], false).is_err());
    assert!(
        super::super::r7::validate_mappings(
            &[
                first,
                MappedBearer {
                    bearer: 21,
                    ..first
                }
            ],
            true
        )
        .is_err()
    );
}

fn dual_body(message: class::DLDCCHMessage) -> pdu::RadioBearerReconfigurationR8IEs {
    let class::DLDCCHMessageType::radioBearerReconfiguration(
        pdu::RadioBearerReconfiguration::later_than_r3(value),
    ) = message.message
    else {
        panic!("reconfiguration")
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(value) =
        value.critical_extensions
    else {
        panic!("r4")
    };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r5") };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r6") };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r7") };
    let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(value) = value else { panic!("r8") };
    value.radio_bearer_reconfiguration_r8
}

#[test]
fn single_uplink_activation_and_restoration_retain_dual_downlink() {
    use crate::{
        dedicated::ActivationMessage,
        hsupa::{DchRestoration, UplinkActivation},
    };
    for voice in [false, true] {
        let active = UplinkActivation::Coexistence(evolved(voice))
            .with_secondary_cell(true)
            .unwrap();
        let encoded = active.message(TransactionId::new(1).unwrap()).unwrap();
        let active_body = dual_body(
            *decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                .unwrap()
                .value,
        );
        assert!(matches!(
            active_body.dl_secondary_cell_info_fdd,
            Some(ie::DLSecondaryCellInfoFDD::R_continue(()))
        ));
        assert!(active_body.dl_hspdsch_information.is_none());
        let ie::ULEDCHInformationR8ModeSpecificInfo::fdd(uplink) =
            active_body.ul_edch_information.unwrap().mode_specific_info
        else {
            panic!("FDD")
        };
        assert_eq!(uplink.e_dpcch_info.unwrap().e_dpcch_dpcch_power_offset.0, 8);
        let data = uplink.e_dpdch_info.unwrap();
        assert_eq!(data.reference_e_tfcis.0[0].reference_e_tfci, 50);
        assert_eq!(data.reference_e_tfcis.0[0].reference_e_tfci_po_r7, 20);
        assert!(data.min_reduced_e_dpdch_gain_factor.is_none());
        let link = &active_body.dl_information_per_rl_list.unwrap().0[0];
        let ie::DLInformationPerRLR8ModeSpecificInfo::fdd(fdd) = &link.mode_specific_info else {
            panic!("FDD")
        };
        assert!(fdd.serving_hsdsch_rl_indicator && fdd.serving_edch_rl_indicator);
        assert!(link.e_agch_information.is_some());
        let prior = active.activation();
        let next = ScheduledActivation::new(prior.time(), prior.cfn(), NonZeroU8::new(4).unwrap())
            .unwrap();
        assert!(active.rescheduled(next).unwrap().secondary_cell_active());
        let restoration = DchRestoration::new(Arc::new(active), next).unwrap();
        let encoded = restoration.message(TransactionId::new(2).unwrap()).unwrap();
        let restored = dual_body(
            *decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                .unwrap()
                .value,
        );
        assert!(matches!(
            restored.dl_secondary_cell_info_fdd,
            Some(ie::DLSecondaryCellInfoFDD::R_continue(()))
        ));
        assert!(restored.ul_edch_information.is_none());
        assert!(restored.dl_hspdsch_information.is_none());
    }
}

#[test]
fn process_allocation_keeps_the_secondary_cell() {
    use crate::hsupa::UplinkActivation;
    let transaction = TransactionId::new(1).unwrap();
    let single = UplinkActivation::Coexistence(evolved(false));
    let message = single.process_allocation(transaction, 0x55).unwrap();
    let class::DLDCCHMessageType::physicalChannelReconfiguration(
        pdu::PhysicalChannelReconfiguration::later_than_r3(value),
    ) = message.message
    else {
        panic!("reconfiguration")
    };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(value) =
        value.critical_extensions
    else {
        panic!("r4")
    };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r5") };
    assert!(matches!(value, pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::r6(_)));

    let dual = single.with_secondary_cell(true).unwrap();
    let message = dual.process_allocation(transaction, 0x55).unwrap();
    let class::DLDCCHMessageType::physicalChannelReconfiguration(
        pdu::PhysicalChannelReconfiguration::later_than_r3(value),
    ) = message.message
    else {
        panic!("reconfiguration")
    };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(value) =
        value.critical_extensions
    else {
        panic!("r4")
    };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r5") };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r6") };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r7") };
    let pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(value) = value else { panic!("r8") };
    let body = value.physical_channel_reconfiguration_r8;
    assert!(matches!(
        body.dl_secondary_cell_info_fdd,
        Some(ie::DLSecondaryCellInfoFDD::R_continue(()))
    ));
    assert!(body.activation_time.is_none());
    let ie::ULEDCHInformationR8ModeSpecificInfo::fdd(uplink) =
        body.ul_edch_information.unwrap().mode_specific_info
    else {
        panic!("FDD")
    };
    let allocation = uplink
        .scheduling_transm_configuration
        .unwrap()
        .ms2_sched_transm_grant_harqalloc
        .unwrap();
    assert_eq!(allocation.as_raw_slice(), [0x55u8.reverse_bits()]);
}
