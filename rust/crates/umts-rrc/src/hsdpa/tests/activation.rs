// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{dedicated::ScheduledActivation, establishment::TransactionId};
use std::num::NonZeroU64;
use umts_rlc::acknowledged::{AmEntity, AmLimits, ReceiveTime, SduReceipt};
use umts_rrc_codec::{DecodedUplinkDedicated, decode_downlink_dedicated, decode_uplink_dedicated};
use umts_rrc_schema::{class_definitions as class, pdu_definitions as pdu};
use umts_types::{RadioEpoch, UeKey, time::ChipInstant};

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 2,
        generation: NonZeroU64::MIN,
    }
}

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: key().epoch,
        index,
    }
}

fn construction(voice: bool) -> InitialActivation {
    let dedicated = crate::test_support::dedicated::configuration(voice, true, 10);
    let mut sizes = [None; 8];
    sizes[0] = NonZeroUsize::new(324);
    let parameters = ActivationParameters {
        h_rnti: 0x8421,
        harq: HarqConfiguration::implicit(Category::new(10).unwrap(), NonZeroU8::new(6).unwrap())
            .unwrap(),
        queues: QueueSet::new(vec![QueueConfiguration::new(1, 2, sizes, 100, 32).unwrap()])
            .unwrap(),
        control: ControlChannels::new(&[OvsfCode::new(128, 12).unwrap()]).unwrap(),
        measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8).unwrap(),
        acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
    };
    InitialActivation::new(
        dedicated,
        parameters,
        vec![MappedBearer {
            bearer: 20,
            flow: 2,
            logical: Some(1),
        }],
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap(),
    )
    .unwrap()
}

#[test]
fn encoded_activation_preserves_security_and_only_remaps_packet_downlink() {
    for voice in [false, true] {
        let activation = construction(voice);
        let encoded = activation.message(TransactionId::new(2).unwrap()).unwrap();
        let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
        let class::DLDCCHMessageType::radioBearerReconfiguration(
            pdu::RadioBearerReconfiguration::later_than_r3(outer),
        ) = decoded.value.message
        else {
            panic!("expected reconfiguration");
        };
        assert_eq!(outer.rrc_transaction_identifier.0, 2);
        let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(
            extension,
        ) = outer.critical_extensions
        else {
            panic!("expected Release 5 extension");
        };
        let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::r5(
            extension,
        ) = extension
        else {
            panic!("expected Release 5");
        };
        let body = extension.radio_bearer_reconfiguration_r5;
        assert!(body.ciphering_mode_info.is_none());
        assert!(body.integrity_protection_mode_info.is_none());
        assert_eq!(body.activation_time.unwrap().0, 4);
        let h_rnti = body
            .new_h_rnti
            .unwrap()
            .0
            .iter()
            .take(u16::BITS as usize)
            .fold(0_u16, |word, bit| (word << 1) | u16::from(*bit));
        assert_eq!(h_rnti, 0x8421);
        let pdu::RadioBearerReconfigurationR5IEsSpecificationMode::complete(specification) =
            body.specification_mode
        else {
            panic!("expected complete specification");
        };
        assert!(specification.rb_information_reconfig_list.is_none());
        assert!(specification.dl_deleted_trans_ch_info_list.is_none());
        let affected = specification.rb_information_affected_list.unwrap().0;
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0].rb_identity.0, 20);
        let mapping = &affected[0].rb_mapping_info.0[0];
        let old = activation.dedicated().bearer(20).unwrap().mapping.wire();
        assert_eq!(
            mapping.ul_logical_channel_mappings,
            old.0[0].ul_logical_channel_mappings
        );
        let downlink = &mapping.dl_logical_channel_mapping_list.as_ref().unwrap().0[0];
        assert!(matches!(
            downlink.dl_transport_channel_type,
            ie::DLTransportChannelTypeR5::hsdsch(ie::MACDFlowIdentity(2))
        ));
        assert_eq!(downlink.logical_channel_identity.as_ref().unwrap().0, 1);
        let links = body.dl_information_per_rl_list.unwrap().0;
        let ie::DLInformationPerRLR5ModeSpecificInfo::fdd(link) = &links[0].mode_specific_info
        else {
            panic!("expected FDD link");
        };
        assert!(link.serving_hsdsch_rl_indicator);
        assert!(links[0].dl_dpch_info_per_rl.is_none());
        assert!(
            body.dl_hspdsch_information
                .unwrap()
                .measurement_feedback_info
                .is_some()
        );
    }
}

fn receipt() -> SduReceipt {
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
        at(0),
    )
    .unwrap();
    entity.enqueue_tracked(&[0x12]).unwrap()
}

fn complete() -> DecodedUplinkDedicated {
    let value = class::ULDCCHMessage::new(
        None,
        class::ULDCCHMessageType::radioBearerReconfigurationComplete(
            pdu::RadioBearerReconfigurationComplete::new(
                ie::RRCTransactionIdentifier(2),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
        ),
    );
    let encoded = rasn::uper::encode(&value).unwrap();
    decode_uplink_dedicated(&encoded, encoded.len() * 8).unwrap()
}

#[test]
fn activation_waits_for_publication_time_and_successful_response_admission() {
    let mut procedure = ActivationProcedure::new(
        key(),
        TransactionId::new(2).unwrap(),
        std::sync::Arc::new(construction(true)),
        at(0),
        at(100),
        NonZeroU64::new(300000).unwrap(),
    )
    .unwrap();
    assert!(
        procedure
            .publish_with(at(1), |_| Err::<SduReceipt, _>("full"))
            .is_err()
    );
    assert_eq!(procedure.phase(), ActivationPhase::Prepared);
    procedure
        .publish_with(at(2), |_| Ok::<_, ()>(receipt()))
        .unwrap();
    let response = complete();
    assert!(
        procedure
            .response_with(
                key(),
                ReceiveTime::simultaneous(at(100)),
                &response,
                |_| Ok::<_, ()>(())
            )
            .is_err()
    );
    assert!(
        procedure
            .response_with(
                key(),
                ReceiveTime::simultaneous(at(200000)),
                &response,
                |_| Err::<(), _>("worker full")
            )
            .is_err()
    );
    assert_eq!(procedure.phase(), ActivationPhase::AwaitingComplete);
    assert_eq!(
        procedure
            .response_with(
                key(),
                ReceiveTime::simultaneous(at(200001)),
                &response,
                |_| Ok::<_, ()>(7)
            )
            .unwrap(),
        (ActivationResponse::Complete, Some(7))
    );
    assert_eq!(
        procedure
            .response_with(
                key(),
                ReceiveTime::simultaneous(at(200002)),
                &response,
                |_| -> Result<(), ()> { panic!("duplicate must not commit twice") }
            )
            .unwrap(),
        (ActivationResponse::Duplicate, None)
    );
}
