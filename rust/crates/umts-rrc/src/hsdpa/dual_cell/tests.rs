// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa::{
        AckFeedback, ActivationParameters, FeedbackCycle, HarqConfiguration, MappedBearer,
        MeasurementFeedback, QueueConfiguration, QueueSet,
    },
    test_support::dedicated as fixture,
};
use std::num::{NonZeroU8, NonZeroU64, NonZeroUsize};
use umts_types::{RadioEpoch, channel_codes::OvsfCode, hsdpa::Category, time::ChipInstant};

fn secondary(qam64: bool) -> SecondaryCellConfiguration {
    SecondaryCellConfiguration::new(
        0x8421,
        10662,
        470,
        ControlChannels::new(&[OvsfCode::new(128, 2).unwrap()]).unwrap(),
        6,
        qam64,
    )
    .unwrap()
}

fn activation() -> ScheduledActivation {
    ScheduledActivation::new(
        ChipInstant {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            index: 0,
        },
        0,
        NonZeroU8::new(4).unwrap(),
    )
    .unwrap()
}

fn configuration(category: u8) -> InitialConfiguration {
    configuration_with_control(category, &[OvsfCode::new(128, 12).unwrap()])
}

fn configuration_with_control(category: u8, control: &[OvsfCode]) -> InitialConfiguration {
    let mut sizes = [None; 8];
    sizes[0] = NonZeroUsize::new(320);
    InitialConfiguration::new_mac_ehs(
        fixture::configuration(false, true, 10),
        ActivationParameters {
            h_rnti: 0x8420,
            harq: HarqConfiguration::implicit(
                Category::new(category).unwrap(),
                NonZeroU8::new(6).unwrap(),
            )
            .unwrap(),
            queues: QueueSet::new(vec![QueueConfiguration::new(1, 2, sizes, 100, 32).unwrap()])
                .unwrap(),
            control: ControlChannels::new(control).unwrap(),
            measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8)
                .unwrap(),
            acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
        },
        vec![MappedBearer {
            bearer: 20,
            flow: 2,
            logical: Some(3),
        }],
    )
    .unwrap()
}

#[test]
fn attached_secondary_enforces_combined_monitoring_limit() {
    let primary: Vec<_> = (12..16)
        .map(|index| OvsfCode::new(128, index).unwrap())
        .collect();
    for count in [1, 2, 3, 4] {
        let codes: Vec<_> = (2..2 + count)
            .map(|index| OvsfCode::new(128, index).unwrap())
            .collect();
        let secondary = SecondaryCellConfiguration::new(
            0x8420,
            10662,
            470,
            ControlChannels::new(&codes).unwrap(),
            6,
            false,
        )
        .unwrap();
        let configuration = configuration_with_control(10, &primary);
        let message = configuration.message_with_secondary_cell(
            TransactionId::new(1).unwrap(),
            activation(),
            SecondaryCellUpdate::Configure(&secondary),
        );
        assert_eq!(message.is_ok(), count <= 2);
        let result = configuration.with_secondary_cell(secondary);
        assert_eq!(result.is_ok(), count <= 2);
    }
}

fn round_trip(encoded: &EncodedRrc) -> class::DLDCCHMessage {
    let decoded =
        umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let repeated = encode_downlink_dedicated(&decoded.value).unwrap();
    assert_eq!(repeated.bytes, encoded.bytes);
    assert_eq!(repeated.bit_length, encoded.bit_length);
    *decoded.value
}

#[test]
fn secondary_configuration_fields_and_removal_are_explicit() {
    for qam64 in [false, true] {
        let secondary = secondary(qam64);
        let ie::DLSecondaryCellInfoFDD::newConfiguration(value) = secondary.wire() else {
            panic!("new configuration expected")
        };
        assert_eq!(value.uarfcn_dl.0, 10662);
        assert_eq!(value.primary_cpich_info.primary_scrambling_code.0, 470);
        assert_eq!(value.h_s_scchchannelisation_code_info, [ie::HSSCCHCodes(2)]);
        assert_eq!(value.measurement_power_offset.0, 6);
        assert!(value.hs_dsch_tbsize_table.is_none());
        assert_eq!(value.dl_64_qam_configured.is_some(), qam64);
        assert!(value.dl_scrambling_code.is_none());
        assert_eq!(rasn::uper::encode(&value.new_h_rnti).unwrap(), [0x84, 0x21]);
    }
    assert!(matches!(
        SecondaryCellUpdate::Continue.wire(),
        Some(ie::DLSecondaryCellInfoFDD::R_continue(()))
    ));
    assert!(SecondaryCellUpdate::Remove.wire().is_none());
}

#[test]
fn secondary_configuration_rejects_invalid_channel_and_measurement_values() {
    for (uarfcn, scrambling, offset) in [
        (16384, 1, 0),
        (10662, 512, 0),
        (10662, 470, -13),
        (10662, 470, 27),
    ] {
        assert!(
            SecondaryCellConfiguration::new(
                1,
                uarfcn,
                scrambling,
                ControlChannels::new(&[OvsfCode::new(128, 2).unwrap()]).unwrap(),
                offset,
                true
            )
            .is_err()
        );
    }
}

#[test]
fn setup_and_reconfiguration_encode_r8_for_16qam_and_64qam() {
    for category in [10, 14] {
        let configuration = configuration(category);
        let secondary = secondary(category == 14);
        let transaction = TransactionId::new(2).unwrap();
        let setup = crate::dedicated::InitialDchSetup::new(
            configuration.dedicated().clone(),
            fixture::retained(),
            fixture::rabs(false, true),
        )
        .unwrap();
        let setup = setup
            .message_with_secondary_cell(
                transaction,
                &configuration,
                SecondaryCellUpdate::Configure(&secondary),
            )
            .unwrap();
        assert!(matches!(
            round_trip(&setup).message,
            class::DLDCCHMessageType::radioBearerSetup(_)
        ));
        let update = configuration
            .message_with_secondary_cell(
                transaction,
                activation(),
                SecondaryCellUpdate::Configure(&secondary),
            )
            .unwrap();
        assert!(matches!(
            round_trip(&update).message,
            class::DLDCCHMessageType::radioBearerReconfiguration(_)
        ));
    }
}

#[test]
fn physical_update_round_trips_new_continue_and_remove() {
    let primary = configuration(14)
        .with_secondary_cell(secondary(true))
        .unwrap();
    let secondary = secondary(true);
    let mut messages = Vec::new();
    for update in [
        SecondaryCellUpdate::Configure(&secondary),
        SecondaryCellUpdate::Continue,
        SecondaryCellUpdate::Remove,
    ] {
        let encoded = secondary_cell_physical_reconfiguration(
            &primary,
            TransactionId::new(1).unwrap(),
            activation(),
            update,
        )
        .unwrap();
        assert!(matches!(
            round_trip(&encoded).message,
            class::DLDCCHMessageType::physicalChannelReconfiguration(_)
        ));
        messages.push(encoded.bytes);
    }
    assert_ne!(messages[0], messages[1]);
    assert_ne!(messages[1], messages[2]);
}

#[test]
fn attached_secondary_configuration_selects_r8_in_procedure_messages() {
    let configuration = std::sync::Arc::new(
        configuration(14)
            .with_secondary_cell(secondary(true))
            .unwrap(),
    );
    let transaction = TransactionId::new(2).unwrap();
    let activation = InitialActivation::schedule(configuration.clone(), activation()).unwrap();
    let automatic = activation.message(transaction).unwrap();
    let explicit = activation
        .message_with_secondary_cell(
            transaction,
            SecondaryCellUpdate::Configure(configuration.secondary_cell().unwrap()),
        )
        .unwrap();
    assert_eq!(automatic.bytes, explicit.bytes);
    assert_eq!(automatic.bit_length, explicit.bit_length);
    let setup = crate::dedicated::InitialDchSetup::new(
        configuration.dedicated().clone(),
        fixture::retained(),
        fixture::rabs(false, true),
    )
    .unwrap();
    let automatic = setup
        .message_with_evolved_hsdpa(transaction, &configuration)
        .unwrap();
    let explicit = setup
        .message_with_secondary_cell(
            transaction,
            &configuration,
            SecondaryCellUpdate::Configure(configuration.secondary_cell().unwrap()),
        )
        .unwrap();
    assert_eq!(automatic.bytes, explicit.bytes);
    assert_eq!(automatic.bit_length, explicit.bit_length);
}

fn setup_r8(value: class::DLDCCHMessage) -> pdu::RadioBearerSetupR8IEs {
    let class::DLDCCHMessageType::radioBearerSetup(pdu::RadioBearerSetup::later_than_r3(value)) =
        value.message
    else {
        panic!("r8 setup expected")
    };
    let pdu::RadioBearerSetupLaterThanR3CriticalExtensions::criticalExtensions(value) =
        value.critical_extensions
    else {
        panic!("r4")
    };
    let pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
        value,
    ) = value
    else {
        panic!("r5")
    };
    let pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r6") };
    let pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r7") };
    let pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(value) = value else { panic!("r8") };
    value.radio_bearer_setup_r8
}

fn release_r8(value: class::DLDCCHMessage) -> pdu::RadioBearerReleaseR8IEs {
    let class::DLDCCHMessageType::radioBearerRelease(pdu::RadioBearerRelease::later_than_r3(value)) =
        value.message
    else {
        panic!("r8 release expected")
    };
    let pdu::RadioBearerReleaseLaterThanR3CriticalExtensions::criticalExtensions(value) =
        value.critical_extensions
    else {
        panic!("r4")
    };
    let pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
        value,
    ) = value
    else {
        panic!("r5")
    };
    let pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r6") };
    let pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(value) = value else { panic!("r7") };
    let pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(value) = value else { panic!("r8") };
    value.radio_bearer_release_r8
}

#[test]
fn cs_addition_and_release_continue_dual_cell_without_reset() {
    let hs = std::sync::Arc::new(
        configuration(10)
            .with_secondary_cell(secondary(false))
            .unwrap(),
    );
    let combined = fixture::configuration(true, true, 10);
    let transaction = TransactionId::new(1).unwrap();
    let setup = crate::dedicated::ExistingDchSetup::new(
        hs.dedicated().clone(),
        combined.clone(),
        fixture::rabs(true, false),
        activation(),
    )
    .unwrap()
    .with_retained_hsdpa(Some(hs.clone()));
    let setup = setup_r8(round_trip(&setup.message(transaction).unwrap()));
    assert!(matches!(
        setup.dl_secondary_cell_info_fdd,
        Some(ie::DLSecondaryCellInfoFDD::R_continue(()))
    ));
    assert!(setup.dl_hspdsch_information.is_none());
    assert!(setup.dl_common_information.is_none());
    assert!(setup.new_h_rnti.is_none());
    let release =
        crate::dedicated::ExistingDchRelease::new(combined, hs.dedicated().clone(), activation())
            .unwrap()
            .with_retained_hsdpa(Some(hs));
    let release = release_r8(round_trip(&release.message(transaction).unwrap()));
    assert!(matches!(
        release.dl_secondary_cell_info_fdd,
        Some(ie::DLSecondaryCellInfoFDD::R_continue(()))
    ));
    assert!(release.dl_hspdsch_information.is_none());
    assert!(release.dl_common_information.is_none());
}

#[test]
fn releasing_last_packet_bearer_removes_secondary_cell() {
    let hs = std::sync::Arc::new(
        configuration(10)
            .with_secondary_cell(secondary(false))
            .unwrap(),
    );
    let release = crate::dedicated::ExistingDchRelease::new(
        fixture::configuration(true, true, 10),
        fixture::configuration(true, false, 10),
        activation(),
    )
    .unwrap()
    .with_retained_hsdpa(Some(hs));
    let release = release_r8(round_trip(
        &release.message(TransactionId::new(1).unwrap()).unwrap(),
    ));
    assert!(release.dl_secondary_cell_info_fdd.is_none());
    assert!(
        release
            .rb_information_release_list
            .0
            .iter()
            .any(|id| id.0 == 20)
    );
    assert!(
        release
            .dl_deleted_trans_ch_info_list
            .unwrap()
            .0
            .iter()
            .any(|value| matches!(
                value.dl_transport_channel_type,
                ie::DLTrCHTypeId2R7::hsdsch(ie::DLTrCHTypeId2R7Hsdsch::mac_ehs(ie::MACEhsQueueId(
                    1
                )))
            ))
    );
}
