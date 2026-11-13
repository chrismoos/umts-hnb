// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ConfiguredServices, SelectionError};
use crate::test_support::count;
use crate::{
    dedicated_bearers::UserRlcPolicy,
    rel99_planner::{
        EmptyTransport, HighSpeedDownlink, Service,
        tests::{cs_request, ps_request},
    },
};
use umts_config::{R99CircuitFormat, R99ServicesConfig, R99TrafficClass, R99UmPolicy};
use umts_rrc::{
    dedicated::Rel99Rlc,
    transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval},
};
use umts_rrc_schema::information_elements as ie;
use umts_types::CnDomain;

const PACKET_RATE: u32 = 12_800;
const PACKET_CAPACITY: usize = 1500;

#[test]
fn transparent_user_plane_normalizes_error_delivery_without_rewriting_wire_qos() {
    use umts_config::R99ErrorDelivery;
    use umts_iuh::ranap::rab::qos;
    let mut config = R99ServicesConfig::default();
    config.packet[0].contract.subflows[0].delivery = R99ErrorDelivery::NoDetection;
    config.packet[0].contract.subflows[0].sdu = None;
    let catalog = ConfiguredServices::new(&config, count(PACKET_CAPACITY)).unwrap();
    let mut request = ps_request(PACKET_RATE);
    let parameters = request.first.parameters.as_mut().unwrap();
    let mut flows = parameters.subflows.as_slice().to_vec();
    flows[0].delivery = qos::Delivery::Yes;
    parameters.subflows = qos::Subflows::new(flows).unwrap();
    let selected = catalog.select(&[(CnDomain::Ps, &request)]).unwrap();
    let Service::Packet(profile) = selected.rabs()[0].service else {
        panic!("expected packet")
    };
    assert_eq!(
        profile.contract.subflows[0].delivery,
        qos::Delivery::NoDetection
    );
    let wire = &selected.rabs()[0]
        .request
        .first
        .parameters
        .as_ref()
        .unwrap()
        .subflows
        .as_slice()[0];
    assert_eq!(wire.delivery, qos::Delivery::Yes);
    assert!(wire.sdu_error.is_some());
    let input: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    let policy = crate::rel99_planner::ConfiguredPolicy::new(&input).unwrap();
    let planned = crate::rel99_planner::plan_initial(
        &crate::test_support::rel99::retained(),
        selected.rabs(),
        &crate::rel99_planner::tests::limits(),
        &policy.policy(),
    )
    .unwrap();
    let message = planned
        .setup()
        .message(umts_rrc::establishment::TransactionId::new(0).unwrap())
        .unwrap();
    umts_rrc_codec::decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
    assert!(
        catalog
            .select(&[(CnDomain::Ps, &ps_request(PACKET_RATE))])
            .is_err()
    );
}

#[test]
fn transparent_packet_profile_cannot_claim_an_error_indication_channel() {
    let mut config = R99ServicesConfig::default();
    config.packet[0].contract.subflows[0].delivery = umts_config::R99ErrorDelivery::Indicate;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
}

#[test]
fn ordered_service_cannot_signal_out_of_order_am_delivery() {
    let mut config = R99ServicesConfig::default();
    config.packet[0]
        .am
        .as_mut()
        .unwrap()
        .ue
        .in_sequence_delivery = false;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
    config.packet[0].contract.delivery_order = false;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_ok());
}

#[test]
fn defaults_select_exact_joint_amr_rows_and_packet_contract() {
    let config = R99ServicesConfig::default();
    let catalog = ConfiguredServices::new(&config, count(PACKET_CAPACITY)).unwrap();
    let voice = cs_request();
    let packet = ps_request(PACKET_RATE);
    let selected = catalog
        .select(&[(CnDomain::Cs, &voice), (CnDomain::Ps, &packet)])
        .unwrap();
    assert_eq!(selected.names(), ["amr-narrowband", "background-am"]);
    let Service::Circuit(profile) = selected.rabs()[0].service else {
        panic!("expected circuit")
    };
    assert_eq!(profile.interval, TransportInterval::Ms20);
    assert_eq!(profile.versions, 3);
    assert_eq!(
        profile.supported_formats,
        [
            vec![0, 0, 0],
            vec![39, 0, 0],
            vec![42, 53, 0],
            vec![55, 63, 0],
            vec![75, 84, 0],
            vec![81, 103, 60]
        ]
    );
    assert!(!profile.supported_formats.contains(&vec![39, 103, 60]));
    assert!(matches!(
        profile.subflows[0].empty,
        EmptyTransport::ZeroSizeBlock
    ));
    assert!(matches!(profile.subflows[1].empty, EmptyTransport::NoBlock));
    assert!(matches!(profile.subflows[2].empty, EmptyTransport::NoBlock));
    assert!(matches!(
        profile.subflows[0].uplink.channel,
        ChannelCoding::Convolutional(ConvolutionalRate::Third)
    ));
    assert!(matches!(profile.subflows[0].uplink.crc, CrcSize::Bits12));
    assert!(matches!(profile.subflows[1].downlink.crc, CrcSize::None));
    assert_eq!(profile.subflows[1].downlink.rate_matching_attribute, 190);
    assert!(matches!(
        profile.subflows[2].downlink.channel,
        ChannelCoding::Convolutional(ConvolutionalRate::Half)
    ));
    assert_eq!(profile.subflows[2].downlink.rate_matching_attribute, 235);
    let Service::Packet(profile) = selected.rabs()[1].service else {
        panic!("expected packet")
    };
    assert_eq!(profile.uplink.rlc_bits, 320);
    assert_eq!(profile.uplink.payload_budget_bits, 256);
    let Rel99Rlc::Acknowledged { uplink, downlink } = &profile.rlc else {
        panic!("expected packet AM")
    };
    assert_eq!(
        uplink.transmission_window_size,
        ie::TransmissionWindowSize::tw512
    );
    assert_eq!(
        downlink.receiving_window_size,
        ie::ReceivingWindowSize::rw512
    );
    assert!(matches!(
        profile.downlink.coding.channel,
        ChannelCoding::Turbo
    ));
}

#[test]
fn peer_numeric_values_map_to_literal_asn_enumerators_and_network_chip_timers() {
    let mut config = R99ServicesConfig::default();
    let peer = &mut config.packet[0].am.as_mut().unwrap().ue;
    peer.uplink_window = 64;
    peer.downlink_window = 16;
    let catalog = ConfiguredServices::new(&config, count(PACKET_CAPACITY)).unwrap();
    let request = ps_request(PACKET_RATE);
    let selected = catalog.select(&[(CnDomain::Ps, &request)]).unwrap();
    let Service::Packet(profile) = selected.rabs()[0].service else {
        panic!("expected packet")
    };
    let Rel99Rlc::Acknowledged { uplink, downlink } = &profile.rlc else {
        panic!("expected AM")
    };
    // TS 25.331 11.2 binds dat10, tw64, tr400 and rst4 to values, not enum ordinals.
    assert_eq!(
        *uplink,
        ie::ULAMRLCMode::new(
            ie::TransmissionRLCDiscard::noDiscard(ie::MaxDAT::dat10),
            ie::TransmissionWindowSize::tw64,
            ie::TimerRST::tr400,
            ie::MaxRST::rst4,
            Some(ie::PollingInfo::new(
                None,
                Some(ie::TimerPoll::tp200),
                Some(ie::PollPDU::pdu1),
                None,
                true,
                true,
                None,
                None
            )),
        )
    );
    assert_eq!(
        *downlink,
        ie::DLAMRLCMode::new(
            true,
            ie::ReceivingWindowSize::rw16,
            ie::DLRLCStatusInfo::new(Some(ie::TimerStatusProhibit::tsp100), None, true, None)
        )
    );
    let UserRlcPolicy::Acknowledged { network, limits } = profile.policy else {
        panic!("expected AM policy")
    };
    assert_eq!(network.maximum_data_attempts.get(), 10);
    assert_eq!(network.maximum_reset_attempts.get(), 4);
    assert_eq!(network.reset_timeout_chips.get(), 1_536_000);
    assert_eq!(network.polling.timeout_chips.unwrap().get(), 768_000);
    assert_eq!(network.status_prohibit_chips.unwrap().get(), 384_000);
    assert_eq!(network.polling.every_pdus.unwrap().get(), 1);
    assert_eq!(limits.maximum_sdu_bytes.get(), 65_535);
    assert_eq!(limits.queued_sdus.get(), 16);
}

#[test]
fn asymmetric_packet_geometry_and_explicit_um_owners_are_preserved() {
    let mut config = R99ServicesConfig::default();
    let packet = &mut config.packet[0];
    let buffers = packet.am.take().unwrap().buffers;
    packet.um = Some(R99UmPolicy {
        uplink: buffers.clone(),
        downlink: buffers,
    });
    packet.uplink.rlc_bits = 512;
    packet.uplink.payload_budget_bits = 400;
    packet.uplink.interval_ms = 40;
    packet.downlink.rlc_bits = 320;
    packet.downlink.payload_budget_bits = 256;
    let catalog = ConfiguredServices::new(&config, count(PACKET_CAPACITY)).unwrap();
    let request = ps_request(PACKET_RATE);
    let selected = catalog.select(&[(CnDomain::Ps, &request)]).unwrap();
    let Service::Packet(profile) = selected.rabs()[0].service else {
        panic!("expected packet")
    };
    assert!(matches!(profile.rlc, Rel99Rlc::Unacknowledged));
    assert_eq!(profile.uplink.interval, TransportInterval::Ms40);
    assert_eq!(profile.downlink.interval, TransportInterval::Ms20);
    assert_eq!(profile.uplink.rlc_bits, 512);
    assert_eq!(profile.downlink.rlc_bits, 320);
    let UserRlcPolicy::Unacknowledged { uplink, downlink } = profile.policy else {
        panic!("expected UM policy")
    };
    assert_eq!(uplink.maximum_pdu_bytes.get(), 64);
    assert_eq!(downlink.maximum_pdu_bytes.get(), 40);
}

#[test]
fn exactly_one_packet_rlc_construction_is_required() {
    let mut config = R99ServicesConfig::default();
    let buffers = config.packet[0].am.as_ref().unwrap().buffers.clone();
    config.packet[0].um = Some(R99UmPolicy {
        uplink: buffers.clone(),
        downlink: buffers,
    });
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
    config.packet[0].um = None;
    config.packet[0].am = None;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
}

#[test]
fn unsupported_coding_geometry_storage_and_peer_values_are_rejected() {
    for case in 0..13 {
        let mut config = R99ServicesConfig::default();
        let packet = &mut config.packet[0];
        match case {
            0 => packet.uplink.coding.crc_bits = 10,
            1 => packet.uplink.coding.crc_bits = 0,
            2 => packet.uplink.payload_budget_bits = 305,
            3 => packet.uplink.rlc_bits = 319,
            4 => packet.am.as_mut().unwrap().buffers.queued_sdus = 0,
            5 => packet.am.as_mut().unwrap().buffers.maximum_buffer_bytes = 1,
            6 => packet.am.as_mut().unwrap().ue.reset_timeout_ms = 401,
            7 => packet.am.as_mut().unwrap().ue.maximum_data_attempts = 11,
            8 => packet.am.as_mut().unwrap().ue.uplink_window = 33,
            9 => packet.am.as_mut().unwrap().ue.downlink_window = 4095,
            10 => packet.am.as_mut().unwrap().network.reset_timeout_ms = 0,
            11 => packet.am.as_mut().unwrap().ue.poll_timeout_ms = Some(201),
            _ => packet.am.as_mut().unwrap().buffers.maximum_sdu_bytes = 65_537,
        }
        assert!(
            ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err(),
            "accepted case {case}"
        );
    }
    let mut config = R99ServicesConfig::default();
    config.circuit[0].subflows[0].downlink.crc_bits = 0;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
}

#[test]
fn duplicate_names_and_profile_format_and_request_counts_are_bounded() {
    let mut config = R99ServicesConfig::default();
    config.packet[0].name = config.circuit[0].name.clone();
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
    let mut config = R99ServicesConfig::default();
    let template = config.packet[0].clone();
    config.packet = (0..32)
        .map(|index| {
            let mut p = template.clone();
            p.name = format!("packet-{index}");
            p
        })
        .collect();
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
    let mut config = R99ServicesConfig::default();
    let duplicate = config.circuit[0].formats[0].clone();
    config.circuit[0].formats.push(duplicate);
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
    let mut config = R99ServicesConfig::default();
    config.circuit[0].formats = (0..65)
        .map(|bits| R99CircuitFormat {
            subflow_bits: vec![bits, 0, 0],
        })
        .collect();
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
    let catalog =
        ConfiguredServices::new(&R99ServicesConfig::default(), count(PACKET_CAPACITY)).unwrap();
    let request = ps_request(PACKET_RATE);
    assert!(matches!(catalog.select(&[]), Err(SelectionError::Capacity)));
    assert!(matches!(
        catalog.select(&[(CnDomain::Ps, &request); 17]),
        Err(SelectionError::Capacity)
    ));
}

#[test]
fn base_um_downlink_li_geometry_is_not_silently_extended() {
    let mut config = R99ServicesConfig::default();
    let packet = &mut config.packet[0];
    let buffers = packet.am.take().unwrap().buffers;
    packet.um = Some(R99UmPolicy {
        uplink: buffers.clone(),
        downlink: buffers,
    });
    packet.uplink.rlc_bits = 1024;
    packet.downlink.rlc_bits = 1000;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_ok());
    config.packet[0].downlink.rlc_bits = 1008;
    assert!(ConfiguredServices::new(&config, count(PACKET_CAPACITY)).is_err());
}

#[test]
fn selection_fails_closed_on_contract_mismatch_or_malformed_request() {
    let mut config = R99ServicesConfig::default();
    config.packet[0].contract.traffic_class = R99TrafficClass::Interactive;
    let catalog = ConfiguredServices::new(&config, count(PACKET_CAPACITY)).unwrap();
    let request = ps_request(PACKET_RATE);
    assert!(matches!(
        catalog.select(&[(CnDomain::Ps, &request)]),
        Err(SelectionError::NoService {
            domain: CnDomain::Ps,
            ..
        })
    ));
    let catalog =
        ConfiguredServices::new(&R99ServicesConfig::default(), count(PACKET_CAPACITY)).unwrap();
    let mut request = ps_request(PACKET_RATE);
    request.first.parameters = None;
    assert!(matches!(
        catalog.select(&[(CnDomain::Ps, &request)]),
        Err(SelectionError::Request(_))
    ));
}

#[test]
fn declaration_order_and_compiled_snapshot_do_not_follow_later_config_mutation() {
    let mut config = R99ServicesConfig::default();
    let mut second = config.packet[0].clone();
    second.name = "second-background".to_owned();
    config.packet.push(second);
    let catalog = ConfiguredServices::new(&config, count(PACKET_CAPACITY)).unwrap();
    config.packet[0].name = "changed".to_owned();
    config.packet[0].uplink.rlc_bits = 80;
    config.packet.clear();
    let request = ps_request(PACKET_RATE);
    let selected = catalog.select(&[(CnDomain::Ps, &request)]).unwrap();
    assert_eq!(selected.names(), ["background-am"]);
    let Service::Packet(profile) = selected.rabs()[0].service else {
        panic!("expected packet")
    };
    assert_eq!(profile.uplink.rlc_bits, 320);
}

const HIGH_SPEED: HighSpeedDownlink = HighSpeedDownlink {
    rlc_bits: 4992,
    receive_window: 768,
};

fn high_speed_variant(
    config: &R99ServicesConfig,
    window: ie::ReceivingWindowSize,
) -> Option<(u16, u16)> {
    let catalog = ConfiguredServices::with_high_speed_downlink(
        config,
        count(PACKET_CAPACITY),
        Some(HIGH_SPEED),
    )
    .unwrap();
    let request = ps_request(PACKET_RATE);
    let selected = catalog.select(&[(CnDomain::Ps, &request)]).unwrap();
    let Service::Packet(profile) = selected.rabs()[0].service else {
        panic!("expected packet")
    };
    assert_eq!(
        profile.downlink.rlc_bits,
        config.packet[0].downlink.rlc_bits
    );
    profile.high_speed.as_deref().map(|resized| {
        assert!(resized.high_speed.is_none());
        assert_eq!(resized.uplink.rlc_bits, profile.uplink.rlc_bits);
        let Rel99Rlc::Acknowledged { downlink, .. } = &resized.rlc else {
            panic!("expected acknowledged mode")
        };
        assert_eq!(downlink.receiving_window_size, window);
        (resized.downlink.rlc_bits, profile.downlink.rlc_bits)
    })
}

#[test]
fn high_speed_downlink_resizes_am_services_and_keeps_the_base_profile() {
    use ie::ReceivingWindowSize::{rw512, rw768};
    let mut config = R99ServicesConfig::default();
    let base = config.packet[0].downlink.rlc_bits;
    assert_eq!(
        config.packet[0].am.as_ref().unwrap().ue.downlink_window,
        512
    );
    assert_eq!(
        high_speed_variant(&config, rw512),
        Some((HIGH_SPEED.rlc_bits, base))
    );

    config.packet[0].am.as_mut().unwrap().ue.downlink_window = 2047;
    assert_eq!(
        high_speed_variant(&config, rw768),
        Some((HIGH_SPEED.rlc_bits, base))
    );

    config.packet[0].downlink.rlc_bits = HIGH_SPEED.rlc_bits;
    config.packet[0].am.as_mut().unwrap().ue.downlink_window = HIGH_SPEED.receive_window;
    assert_eq!(high_speed_variant(&config, rw768), None);
}

#[test]
fn high_speed_downlink_leaves_um_services_unchanged() {
    let mut config = R99ServicesConfig::default();
    let packet = &mut config.packet[0];
    let buffers = packet.am.take().unwrap().buffers;
    packet.um = Some(R99UmPolicy {
        uplink: buffers.clone(),
        downlink: buffers,
    });
    assert_eq!(
        high_speed_variant(&config, ie::ReceivingWindowSize::rw768),
        None
    );
}
