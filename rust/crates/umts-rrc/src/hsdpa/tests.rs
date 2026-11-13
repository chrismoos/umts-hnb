// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::{NonZeroU8, NonZeroUsize};
use umts_rrc_schema::information_elements as ie;
use umts_types::{channel_codes::OvsfCode, hsdpa::Category};
mod activation;

#[test]
fn implicit_partition_rounds_down_and_explicit_partition_retains_process_order() {
    let implicit =
        HarqConfiguration::implicit(Category::new(1).unwrap(), NonZeroU8::new(7).unwrap()).unwrap();
    assert_eq!(
        implicit.process_memory(),
        &[NonZeroUsize::new(2742).unwrap(); 7]
    );
    assert!(matches!(
        implicit.wire().memory_partitioning,
        ie::HARQInfoMemoryPartitioning::implicit(())
    ));
    let memory = [800, 1600, 17600, 40000].map(|bits| NonZeroUsize::new(bits).unwrap());
    let explicit = HarqConfiguration::explicit(Category::new(10).unwrap(), &memory).unwrap();
    assert_eq!(explicit.process_memory(), memory);
    assert_eq!(explicit.wire().number_of_processes, 4);
    let ie::HARQInfoMemoryPartitioning::explicit(sizes) = explicit.wire().memory_partitioning
    else {
        panic!("expected explicit partition");
    };
    assert_eq!(
        sizes,
        [
            ie::HARQMemorySize::hms800,
            ie::HARQMemorySize::hms1600,
            ie::HARQMemorySize::hms17600,
            ie::HARQMemorySize::hms40000
        ]
    );
    assert!(HarqConfiguration::explicit(Category::new(1).unwrap(), &memory).is_err());
    assert!(
        HarqConfiguration::explicit(
            Category::new(10).unwrap(),
            &[NonZeroUsize::new(3201).unwrap()]
        )
        .is_err()
    );
}

#[test]
fn feedback_preserves_half_db_units_and_two_ms_tti_conversion() {
    assert!(
        MeasurementFeedback::new(0, FeedbackCycle::Ms2, NonZeroU8::new(2).unwrap(), 0).is_err()
    );
    let feedback =
        MeasurementFeedback::new(-3, FeedbackCycle::Ms10, NonZeroU8::new(2).unwrap(), 7).unwrap();
    assert_eq!(feedback.cycle().period_ttis().unwrap().get(), 5);
    assert!(FeedbackCycle::Disabled.period_ttis().is_none());
    let ie::MeasurementFeedbackInfoModeSpecificInfo::fdd(value) =
        feedback.wire().mode_specific_info
    else {
        panic!("expected FDD feedback");
    };
    assert_eq!(value.measurement_power_offset.0, -3);
    assert_eq!(value.feedback_cycle, ie::FeedbackCycle::fc10);
    assert_eq!(value.cqi_repetition_factor.0, 2);
    assert_eq!(value.delta_cqi.0, 7);
    assert!(MeasurementFeedback::new(27, FeedbackCycle::Ms2, NonZeroU8::MIN, 0).is_err());
    assert!(
        MeasurementFeedback::new(0, FeedbackCycle::Ms2, NonZeroU8::new(5).unwrap(), 0).is_err()
    );
}

#[test]
fn control_channel_set_is_per_ue_bounded_and_release_specific() {
    let codes = [12, 13, 14, 15].map(|index| OvsfCode::new(128, index).unwrap());
    let channels = ControlChannels::new(&codes).unwrap();
    let feedback = MeasurementFeedback::new(0, FeedbackCycle::Ms2, NonZeroU8::MIN, 0).unwrap();
    let r5 = channels.downlink_r5(feedback);
    let r6 = channels.downlink_r6(feedback);
    assert!(matches!(
        r5.mode_specific_info,
        ie::DLHSPDSCHInformationModeSpecificInfo::fdd(())
    ));
    assert!(matches!(
        r6.mode_specific_info,
        ie::DLHSPDSCHInformationR6ModeSpecificInfo::fdd(())
    ));
    let ie::HSSCCHInfoModeSpecificInfo::fdd(control) = r5.hs_scch_info.unwrap().mode_specific_info
    else {
        panic!("expected FDD control");
    };
    assert_eq!(
        control
            .h_s_scchchannelisation_code_info
            .iter()
            .map(|code| code.0)
            .collect::<Vec<_>>(),
        [12, 13, 14, 15]
    );
    assert!(control.dl_scrambling_code.is_none());
    assert!(ControlChannels::new(&[codes[0], codes[0]]).is_err());
    assert!(ControlChannels::new(&[OvsfCode::new(64, 1).unwrap()]).is_err());
}

#[test]
fn initial_queue_configuration_contains_sizes_and_no_deletion() {
    let mut sizes = [None; 8];
    sizes[0] = NonZeroUsize::new(148);
    sizes[3] = NonZeroUsize::new(320);
    let queue = || QueueConfiguration::new(2, 1, sizes, 100, 32).unwrap();
    assert!(QueueSet::new(vec![queue(), queue()]).is_err());
    let queues = QueueSet::new(vec![queue()]).unwrap();
    let harq = HarqConfiguration::implicit(Category::new(10).unwrap(), NonZeroU8::new(6).unwrap())
        .unwrap();
    let wire = queues.initial_transport_info(&harq);
    let flow = wire.add_or_reconf_mac_d_flow.unwrap();
    assert!(flow.mac_hs_del_queue_list.is_none());
    let entries = flow.mac_hs_add_reconf_queue_list.unwrap().0;
    assert_eq!(entries[0].mac_hs_queue_id, 2);
    assert_eq!(entries[0].mac_d_flow_id.0, 1);
    let sizes = &entries[0].mac_d_pdu_size_info_list.as_ref().unwrap().0;
    assert_eq!(
        sizes
            .iter()
            .map(|size| (size.mac_d_pdu_index, size.mac_d_pdu_size))
            .collect::<Vec<_>>(),
        [(0, 148), (3, 320)]
    );
}

#[test]
fn acknowledgement_configuration_preserves_dedicated_power_fields() {
    let dedicated = crate::test_support::dedicated::physical();
    let feedback = AckFeedback::new(6, 5, NonZeroU8::new(2).unwrap()).unwrap();
    let ie::ULDPCHPowerControlInfoR5::fdd(r5) = feedback.power_r5(&dedicated).unwrap() else {
        panic!("expected FDD power");
    };
    let ie::ULDPCHPowerControlInfoR6::fdd(r6) =
        feedback.power_r6(&dedicated, HarqPreamble::Mode1).unwrap()
    else {
        panic!("expected FDD power");
    };
    let original = dedicated.power_wire().unwrap();
    assert_eq!(r5.dpcch_power_offset, original.dpcch_power_offset);
    assert_eq!(r5.pc_preamble, original.pc_preamble);
    assert_eq!(r5.s_rb_delay, original.s_rb_delay);
    assert_eq!(r5.power_control_algorithm, original.power_control_algorithm);
    assert_eq!(r6.dpcch_power_offset, original.dpcch_power_offset);
    assert_eq!(r6.power_control_algorithm, original.power_control_algorithm);
    assert_eq!(r6.delta_ack.unwrap().0, 6);
    assert_eq!(r6.delta_nack.unwrap().0, 5);
    assert_eq!(r6.ack_nack_repetition_factor.unwrap().0, 2);
    assert_eq!(r6.harq_preamble_mode.0, 1);
}
