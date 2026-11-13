// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
pub(crate) mod admission;
use crate::test_support::{self, at, capabilities, count, rel99 as fixture};
use std::num::{NonZeroU8, NonZeroU16, NonZeroU64};
use umts_dsp::coding::DecodeBudget;
use umts_iuh::{
    Domain,
    ranap::rab::{Address, Association, First, Mode, Second, Transport, UserPlane},
};
use umts_rlc::{
    acknowledged::{AmLimits, Polling},
    unacknowledged::UmLimits,
};
use umts_rrc::{
    dedicated::DchDirection, establishment::TransactionId, transport::ConvolutionalRate,
};
use umts_rrc_schema::{class_definitions::DLDCCHMessageType, information_elements as ie};
use umts_security::counters::Start;

const CS_RAB: u8 = 7;
const PS_RAB: u8 = 9;
const VOICE_RATE: u32 = 12_200;
const PACKET_RATE: u32 = 12_800;
const USER_BYTES: usize = 2048;
const PACKET_CAPACITY: usize = 256;
const STORAGE_BYTES: usize = 4_000_000;

#[test]
fn packet_storage_covers_network_envelope_and_qos_without_changing_queue_or_wire_policy() {
    for um in [false, true] {
        for (bits, capacity) in [
            (1, PACKET_CAPACITY),
            (159, PACKET_CAPACITY),
            (12_000, PACKET_CAPACITY),
            (8000, 1500),
            (8000, 8192),
        ] {
            let mut request = ps_request(PACKET_RATE);
            request.first.parameters.as_mut().unwrap().maximum_sdu_bits =
                qos::MaxSduBits::new(bits).unwrap();
            let mut profile = packet(&request, um);
            profile.packet_capacity = count(capacity);
            let required = Requirements::new(CnDomain::Ps, &request).unwrap();
            let effective = service::packet_policy(&required, &profile).unwrap();
            let expected = usize::from(bits)
                .div_ceil(u8::BITS as usize)
                .max(capacity.min(USER_BYTES));
            match (profile.policy, effective) {
                (
                    UserRlcPolicy::Acknowledged {
                        network: before,
                        limits: original,
                    },
                    UserRlcPolicy::Acknowledged {
                        network: after,
                        limits,
                    },
                ) => {
                    assert_eq!(original.maximum_sdu_bytes.get(), USER_BYTES);
                    assert_eq!(limits.maximum_sdu_bytes.get(), expected);
                    assert_eq!(limits.queued_sdus, original.queued_sdus);
                    assert_eq!(limits.maximum_buffer_bytes, original.maximum_buffer_bytes);
                    assert_eq!(after.maximum_data_attempts, before.maximum_data_attempts);
                    assert_eq!(after.reset_timeout_chips, before.reset_timeout_chips);
                }
                (
                    UserRlcPolicy::Unacknowledged {
                        uplink: original_ul,
                        downlink: original_dl,
                    },
                    UserRlcPolicy::Unacknowledged { uplink, downlink },
                ) => {
                    for (original, effective) in [(original_ul, uplink), (original_dl, downlink)] {
                        assert_eq!(original.maximum_sdu_bytes.get(), USER_BYTES);
                        assert_eq!(effective.maximum_sdu_bytes.get(), expected);
                        assert_eq!(effective.maximum_pdu_bytes, original.maximum_pdu_bytes);
                        assert_eq!(effective.queued_sdus, original.queued_sdus);
                        assert_eq!(
                            effective.maximum_buffer_bytes,
                            original.maximum_buffer_bytes
                        );
                    }
                }
                _ => panic!("packet policy mode changed"),
            }
        }
        let mut request = ps_request(PACKET_RATE);
        let profile = packet(&request, um);
        request.first.parameters.as_mut().unwrap().maximum_sdu_bits =
            qos::MaxSduBits::new(32_768).unwrap();
        let required = Requirements::new(CnDomain::Ps, &request).unwrap();
        assert!(matches!(
            service::packet_policy(&required, &profile),
            Err(Error::Contract { .. })
        ));
    }
}

fn coding(crc: CrcSize) -> Coding {
    Coding {
        channel: ChannelCoding::Convolutional(ConvolutionalRate::Third),
        crc,
        rate_matching_attribute: 200,
    }
}

pub(super) fn policy(physical: &[NormalDpch]) -> Policy<'_> {
    Policy {
        hsdpa: false,
        hsupa: false,
        signalling: SignallingProfile {
            uplink: coding(CrcSize::Bits16),
            downlink: coding(CrcSize::Bits16),
            interval: TransportInterval::Ms40,
            maximum_blocks: 1,
            um_uplink_bits: 144,
            um_downlink_bits: 336,
        },
        physical,
        uplink_gains: GainFactors {
            control: 15,
            data: 15,
        },
        maximum_combinations: count(1024),
        maximum_total_coded_bits: count(500_000),
        maximum_user_storage_bytes: count(STORAGE_BYTES),
        transport: TransportPlanLimits {
            maximum_coded_bits: count(500_000),
            maximum_mapping_entries: count(1_000_000),
            decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
        },
    }
}

pub(super) fn limits() -> Rel99Limits {
    let mut information = capabilities::information();
    let radio = information.ue_radio_access_capability.as_mut().unwrap();
    let ul = &mut radio.transport_channel_capability.ul_trans_ch_capability;
    ul.max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsUL::e16;
    ul.max_transmitted_blocks = ie::MaxTransportBlocksUL::tb32;
    ul.max_number_of_tfc = ie::MaxNumberOfTFCUL::tfc1024;
    ul.max_number_of_tf = ie::MaxNumberOfTF::tf1024;
    let dl = &mut radio.transport_channel_capability.dl_trans_ch_capability;
    dl.max_simultaneous_trans_chs = ie::MaxSimultaneousTransChsDL::e16;
    dl.max_received_transport_blocks = ie::MaxTransportBlocksDL::tb32;
    dl.max_number_of_tfc = ie::MaxNumberOfTFCDL::tfc1024;
    dl.max_number_of_tf = ie::MaxNumberOfTF::tf1024;
    Rel99Limits::from_information(&information).unwrap()
}

fn request(domain: CnDomain, id: u8, parameters: qos::Parameters) -> SetupOrModify {
    let wire_domain = match domain {
        CnDomain::Cs => Domain::Cs,
        CnDomain::Ps => Domain::Ps,
    };
    let mut first = First::modification(id);
    first.parameters = Some(parameters);
    first.user_plane = Some(
        UserPlane::new(
            if domain == CnDomain::Cs {
                Mode::PredefinedSdu
            } else {
                Mode::Transparent
            },
            3,
        )
        .unwrap(),
    );
    first.transport = Some(Transport::new(
        Address::from_ip(wire_domain, "127.0.0.2".parse().unwrap()).unwrap(),
        if domain == CnDomain::Cs {
            Association::binding([0x0f, 0xa0, 0, 0])
        } else {
            Association::gtp(0)
        },
    ));
    SetupOrModify {
        first,
        second: Second::default(),
    }
}

pub(crate) fn cs_request() -> SetupOrModify {
    let subflows = [vec![0, 39, 81], vec![0, 0, 103], vec![0, 0, 60]]
        .into_iter()
        .enumerate()
        .map(|(index, sizes)| {
            qos::Subflow::new(
                qos::ResidualError::new(1, 6).unwrap(),
                if index == 0 {
                    qos::Delivery::Yes
                } else {
                    qos::Delivery::NoDetection
                },
                (index == 0).then(|| qos::SduError::new(1, 3).unwrap()),
                Some(
                    qos::Formats::new(
                        sizes
                            .into_iter()
                            .map(|bits| qos::Format::new(Some(bits), None).unwrap())
                            .collect(),
                    )
                    .unwrap(),
                ),
            )
            .unwrap()
        })
        .collect();
    let mut parameters = qos::Parameters::new(
        qos::TrafficClass::Conversational,
        qos::Asymmetry::SymmetricBidirectional,
        qos::MaximumRates::new(&[VOICE_RATE]).unwrap(),
        qos::DeliveryOrder::Requested,
        qos::MaxSduBits::new(244).unwrap(),
        qos::Subflows::new(subflows).unwrap(),
    );
    parameters.guaranteed = Some(qos::GuaranteedRates::new(&[VOICE_RATE]).unwrap());
    parameters.transfer_delay = Some(80);
    parameters.source_statistics = Some(qos::SourceStatistics::Speech);
    request(CnDomain::Cs, CS_RAB, parameters)
}

pub(crate) fn ps_request(rate: u32) -> SetupOrModify {
    request(
        CnDomain::Ps,
        PS_RAB,
        qos::Parameters::new(
            qos::TrafficClass::Background,
            qos::Asymmetry::SymmetricBidirectional,
            qos::MaximumRates::new(&[rate]).unwrap(),
            qos::DeliveryOrder::Requested,
            qos::MaxSduBits::new(12_000).unwrap(),
            qos::Subflows::new(vec![
                qos::Subflow::new(
                    qos::ResidualError::new(1, 6).unwrap(),
                    qos::Delivery::No,
                    Some(qos::SduError::new(1, 3).unwrap()),
                    None,
                )
                .unwrap(),
            ])
            .unwrap(),
        ),
    )
}

#[test]
fn manually_supplied_ordered_packet_profile_cannot_signal_unordered_am() {
    let request = ps_request(PACKET_RATE);
    let mut profile = packet(&request, false);
    let Rel99Rlc::Acknowledged { downlink, .. } = &mut profile.rlc else {
        unreachable!()
    };
    downlink.in_sequence_delivery = false;
    let physical = [fixture::physical()];
    assert!(matches!(
        plan_initial(
            &fixture::retained(),
            &[rab(CnDomain::Ps, &request, Service::Packet(&profile))],
            &limits(),
            &policy(&physical)
        ),
        Err(Error::Contract { rab: PS_RAB })
    ));
}

fn contract(request: &SetupOrModify) -> Contract {
    let parameters = request.first.parameters.as_ref().unwrap();
    Contract {
        traffic_class: parameters.traffic_class,
        delivery_order: parameters.delivery_order,
        maximum_transfer_delay_ms: parameters.transfer_delay,
        subflows: parameters
            .subflows
            .as_slice()
            .iter()
            .map(|flow| Reliability {
                residual: flow.residual_error.clone(),
                sdu: flow.sdu_error.clone(),
                delivery: flow.delivery,
            })
            .collect(),
    }
}

pub(super) fn circuit(request: &SetupOrModify) -> CircuitProfile {
    CircuitProfile {
        contract: contract(request),
        interval: TransportInterval::Ms20,
        subflows: (0..3)
            .map(|index| CircuitSubflow {
                uplink: coding(if index == 0 {
                    CrcSize::Bits12
                } else {
                    CrcSize::None
                }),
                downlink: coding(if index == 0 {
                    CrcSize::Bits12
                } else {
                    CrcSize::None
                }),
                empty: if index == 0 {
                    EmptyTransport::ZeroSizeBlock
                } else {
                    EmptyTransport::NoBlock
                },
            })
            .collect(),
        supported_formats: vec![vec![0, 0, 0], vec![39, 0, 0], vec![81, 103, 60]],
        versions: 3,
    }
}

pub(super) fn packet(request: &SetupOrModify, um: bool) -> PacketProfile {
    let channel = PacketChannel {
        interval: TransportInterval::Ms20,
        rlc_bits: 320,
        payload_budget_bits: 256,
        maximum_blocks: 4,
        coding: Coding {
            channel: ChannelCoding::Turbo,
            crc: CrcSize::Bits16,
            rate_matching_attribute: 200,
        },
    };
    let policy = if um {
        let limits = UmLimits {
            maximum_pdu_bytes: count(40),
            maximum_sdu_bytes: count(USER_BYTES),
            queued_sdus: count(4),
            maximum_buffer_bytes: STORAGE_BYTES,
        };
        UserRlcPolicy::Unacknowledged {
            uplink: limits,
            downlink: limits,
        }
    } else {
        UserRlcPolicy::Acknowledged {
            network: crate::dedicated_bearers::NetworkAmPolicy {
                maximum_data_attempts: NonZeroU16::new(6).unwrap(),
                maximum_reset_attempts: NonZeroU16::new(4).unwrap(),
                reset_timeout_chips: NonZeroU64::new(1_000_000).unwrap(),
                polling: Polling {
                    every_pdus: NonZeroU16::new(1),
                    last_transmission: true,
                    last_retransmission: true,
                    timeout_chips: NonZeroU64::new(1_000_000),
                    prohibit_chips: None,
                },
                status_prohibit_chips: None,
                missing_pdu_indicator: false,
            },
            limits: AmLimits {
                maximum_sdu_bytes: count(USER_BYTES),
                queued_sdus: count(4),
                maximum_buffer_bytes: STORAGE_BYTES,
            },
        }
    };
    PacketProfile {
        contract: contract(request),
        rlc: if um {
            Rel99Rlc::Unacknowledged
        } else {
            fixture::am()
        },
        policy,
        packet_capacity: count(PACKET_CAPACITY),
        uplink: channel,
        downlink: channel,
        high_speed: None,
    }
}

fn rab<'a>(domain: CnDomain, request: &'a SetupOrModify, service: Service<'a>) -> Rab<'a> {
    Rab {
        domain,
        request,
        service,
    }
}

#[test]
fn actual_cs_ps_am_ps_um_and_combined_plans_encode_and_instantiate_real_owners() {
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let circuit = circuit(&cs);
    let am = packet(&ps, false);
    let um = packet(&ps, true);
    let physical = [fixture::physical()];
    let retained = fixture::retained();
    for rabs in [
        vec![rab(CnDomain::Cs, &cs, Service::Circuit(&circuit))],
        vec![rab(CnDomain::Ps, &ps, Service::Packet(&am))],
        vec![rab(CnDomain::Ps, &ps, Service::Packet(&um))],
    ] {
        let plan = plan_initial(&retained, &rabs, &limits(), &policy(&physical)).unwrap();
        assert!(Arc::ptr_eq(
            plan.setup().configuration(),
            plan.compiled().configuration()
        ));
        assert!(Arc::ptr_eq(plan.profiles().plan(), plan.compiled()));
        assert_eq!(plan.rabs().len(), rabs.len());
        let message = plan
            .setup()
            .message(TransactionId::new(0).unwrap())
            .unwrap();
        assert!(matches!(
            umts_rrc_codec::decode_downlink_dedicated(&message.bytes, message.bit_length)
                .unwrap()
                .value
                .message,
            DLDCCHMessageType::radioBearerSetup(_)
        ));
        let key = UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let endpoints = plan
            .profiles()
            .instantiate(key, [Some(Start::new(3).unwrap()); 2], at(0))
            .unwrap();
        assert_eq!(
            endpoints.len(),
            plan.setup()
                .rabs()
                .iter()
                .map(|rab| rab.bearers.len())
                .sum::<usize>()
        );
    }
    let (initial, addition) = admission::sequential_cs_then_ps_plans();
    assert_eq!(initial.rabs().len(), 1);
    assert_eq!(addition.retained_rabs().len(), 1);
    assert_eq!(addition.rabs().len(), 1);
}

#[test]
fn sequential_cs_ps_preserves_signalling_then_voice_then_packet_uplink_priority() {
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let circuit = circuit(&cs);
    let packet = packet(&ps, false);
    let physical = [fixture::physical()];
    let configured = policy(&physical);
    for cs_first in [false, true] {
        let requests = [
            rab(CnDomain::Cs, &cs, Service::Circuit(&circuit)),
            rab(CnDomain::Ps, &ps, Service::Packet(&packet)),
        ];
        let first = usize::from(!cs_first);
        let initial = plan_initial(
            &fixture::retained(),
            &requests[first..=first],
            &limits(),
            &configured,
        )
        .unwrap();
        let next = 1 - first;
        let addition = plan_existing(
            initial.compiled().clone(),
            initial.rabs(),
            &requests[next..=next],
            &limits(),
            &configured,
        )
        .unwrap();
        for plan in [initial.compiled(), addition.compiled()] {
            for bearer in plan.configuration().bearers() {
                let expected = match bearer.domain {
                    None => 1,
                    Some(CnDomain::Cs) => 2,
                    Some(CnDomain::Ps) => 3,
                };
                assert!(!bearer.mapping.uplink.is_empty());
                for mapping in &bearer.mapping.uplink {
                    assert_eq!(mapping.priority, expected, "bearer {}", bearer.id);
                }
            }
        }
    }
}

#[test]
fn mixed_new_rab_domains_are_rejected_before_rrc_construction() {
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let circuit = circuit(&cs);
    let packet = packet(&ps, false);
    let rabs = [
        rab(CnDomain::Cs, &cs, Service::Circuit(&circuit)),
        rab(CnDomain::Ps, &ps, Service::Packet(&packet)),
    ];
    assert!(matches!(
        plan_initial(
            &fixture::retained(),
            &rabs,
            &limits(),
            &policy(&[fixture::physical()])
        ),
        Err(Error::Unsupported)
    ));
}

#[test]
fn unequal_retained_srb_sizes_are_preserved_without_forcing_one_transport() {
    let mut retained = fixture::retained();
    retained[1].uplink_pdu_bits = 160;
    retained[1].downlink_pdu_bits = 352;
    retained[2].uplink_pdu_bits = 176;
    retained[2].downlink_pdu_bits = 368;
    let request = ps_request(PACKET_RATE);
    let service = packet(&request, false);
    let physical = [fixture::physical()];
    let plan = plan_initial(
        &retained,
        &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
        &limits(),
        &policy(&physical),
    )
    .unwrap();
    for old in retained {
        let bearer = plan.setup().configuration().bearer(old.id).unwrap();
        assert_eq!(bearer.rlc, old.rlc);
        for (direction, bits) in [
            (DchDirection::Uplink, old.uplink_pdu_bits),
            (DchDirection::Downlink, old.downlink_pdu_bits),
        ] {
            let configuration = plan.setup().configuration();
            let tfcs = if direction == DchDirection::Uplink {
                configuration.uplink()
            } else {
                configuration.downlink()
            };
            let mapping = bearer.mappings(direction).next().unwrap();
            assert!(
                tfcs.transport(mapping.transport)
                    .unwrap()
                    .formats()
                    .all(|format| format.rlc_bits == bits)
            );
        }
    }
}

#[test]
fn cs_subflows_keep_joint_rows_and_distinguish_empty_crc_block_from_no_block() {
    let request = cs_request();
    let service = circuit(&request);
    let physical = [fixture::physical()];
    let plan = plan_initial(
        &fixture::retained(),
        &[rab(CnDomain::Cs, &request, Service::Circuit(&service))],
        &limits(),
        &policy(&physical),
    )
    .unwrap();
    assert_eq!(
        plan.rabs()[0].circuit_interval,
        Some(TransportInterval::Ms20)
    );
    let configuration = plan.setup().configuration();
    let ids = &plan.setup().rabs()[0].bearers;
    for tfcs in [configuration.uplink(), configuration.downlink()] {
        for row in tfcs.combinations() {
            let mut sizes = Vec::new();
            for (index, id) in ids.iter().enumerate() {
                let mapping = configuration
                    .bearer(*id)
                    .unwrap()
                    .mappings(tfcs.direction())
                    .next()
                    .unwrap();
                let channel = tfcs
                    .transports()
                    .iter()
                    .position(|transport| transport.id == mapping.transport)
                    .unwrap();
                let format = tfcs.transports()[channel]
                    .formats()
                    .nth(row.formats[channel])
                    .unwrap();
                sizes.push(if format.blocks == 0 {
                    0
                } else {
                    format.rlc_bits
                });
                if sizes[index] == 0 {
                    assert_eq!(format.blocks, u16::from(index == 0));
                }
            }
            assert!(service.supported_formats.contains(&sizes));
        }
    }
}

#[test]
fn cs_subflow_that_is_empty_in_every_mode_keeps_its_wire_and_deferred_owner() {
    let mut request = cs_request();
    let parameters = request.first.parameters.as_mut().unwrap();
    parameters.maximum = qos::MaximumRates::new(&[7950]).unwrap();
    parameters.guaranteed = Some(qos::GuaranteedRates::new(&[7950]).unwrap());
    parameters.maximum_sdu_bits = qos::MaxSduBits::new(159).unwrap();
    let rows = [[0, 39, 75], [0, 0, 84], [0, 0, 0]];
    parameters.subflows = qos::Subflows::new(
        parameters
            .subflows
            .as_slice()
            .iter()
            .cloned()
            .zip(rows)
            .map(|(mut flow, sizes)| {
                flow.formats = Some(
                    qos::Formats::new(
                        sizes
                            .into_iter()
                            .map(|bits| qos::Format::new(Some(bits), None).unwrap())
                            .collect(),
                    )
                    .unwrap(),
                );
                flow
            })
            .collect(),
    )
    .unwrap();
    let mut service = circuit(&request);
    service.supported_formats = vec![vec![0, 0, 0], vec![39, 0, 0], vec![75, 84, 0]];
    let physical = [fixture::physical()];
    let plan = plan_initial(
        &fixture::retained(),
        &[rab(CnDomain::Cs, &request, Service::Circuit(&service))],
        &limits(),
        &policy(&physical),
    )
    .unwrap();
    let zero_bearer = plan.setup().rabs()[0].bearers[2];
    let configuration = plan.setup().configuration();
    for tfcs in [configuration.uplink(), configuration.downlink()] {
        let mapping = configuration
            .bearer(zero_bearer)
            .unwrap()
            .mappings(tfcs.direction())
            .next()
            .unwrap();
        assert!(
            tfcs.transport(mapping.transport)
                .unwrap()
                .formats()
                .all(|format| format.rlc_bits == 0 && format.blocks == 0)
        );
    }
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    assert_eq!(plan.profiles().placeholders(key).len(), 3);
    assert_eq!(
        plan.profiles()
            .instantiate(key, [Some(Start::new(3).unwrap()); 2], at(0))
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn packet_tfcs_is_dense_directional_and_keeps_a_single_block_at_low_rates() {
    for rate in [1, PACKET_RATE, 38_400] {
        let mut request = ps_request(rate);
        if rate == 38_400 {
            let parameters = request.first.parameters.as_mut().unwrap();
            parameters.asymmetry = qos::Asymmetry::AsymmetricBidirectional;
            parameters.maximum = qos::MaximumRates::new(&[38_400, PACKET_RATE]).unwrap();
        }
        let service = packet(&request, false);
        let physical = [fixture::physical()];
        let plan = plan_initial(
            &fixture::retained(),
            &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
            &limits(),
            &policy(&physical),
        )
        .unwrap();
        let configuration = plan.setup().configuration();
        let bearer = configuration
            .bearer(plan.setup().rabs()[0].bearers[0])
            .unwrap();
        for (tfcs, expected) in [
            (configuration.uplink(), 1),
            (configuration.downlink(), if rate == 38_400 { 3 } else { 1 }),
        ] {
            let id = bearer.mappings(tfcs.direction()).next().unwrap().transport;
            let counts: Vec<_> = tfcs
                .transport(id)
                .unwrap()
                .formats()
                .map(|format| format.blocks)
                .collect();
            assert_eq!(counts, (0..=expected).collect::<Vec<_>>());
        }
    }
}

#[test]
fn physical_candidates_fall_back_after_capability_and_real_phy_compilation_failure() {
    let request = ps_request(PACKET_RATE);
    let service = packet(&request, false);
    let mut codes = fixture::physical();
    codes.downlink_codes.push(4);
    let mut uplink = fixture::physical();
    uplink.uplink_minimum_sf = 256;
    let physical = [codes, uplink, fixture::physical()];
    let plan = plan_initial(
        &fixture::retained(),
        &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
        &limits(),
        &policy(&physical),
    )
    .unwrap();
    assert_eq!(
        plan.compiled()
            .configuration()
            .physical()
            .downlink_codes
            .len(),
        1
    );
    assert_eq!(
        plan.compiled().configuration().physical().uplink_minimum_sf,
        4
    );
}

#[test]
fn service_contract_mismatch_and_insufficient_sdu_memory_are_rejected() {
    let request = ps_request(PACKET_RATE);
    let physical = [fixture::physical()];
    for change in 0..4 {
        let mut service = packet(&request, false);
        match change {
            0 => service.contract.delivery_order = qos::DeliveryOrder::NotRequested,
            1 => service.contract.subflows[0].residual = qos::ResidualError::new(1, 5).unwrap(),
            2 => service.contract.subflows[0].delivery = qos::Delivery::Yes,
            _ => {
                let UserRlcPolicy::Acknowledged { limits, .. } = &mut service.policy else {
                    unreachable!()
                };
                limits.maximum_sdu_bytes = count(128);
            }
        }
        assert!(matches!(
            plan_initial(
                &fixture::retained(),
                &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
                &limits(),
                &policy(&physical)
            ),
            Err(Error::Contract { rab: PS_RAB })
        ));
    }
}

#[test]
fn codec_interval_is_explicit_and_cannot_silently_reduce_a_guaranteed_rate() {
    let request = cs_request();
    let mut service = circuit(&request);
    service.interval = TransportInterval::Ms40;
    let physical = [fixture::physical()];
    assert!(matches!(
        plan_initial(
            &fixture::retained(),
            &[rab(CnDomain::Cs, &request, Service::Circuit(&service))],
            &limits(),
            &policy(&physical)
        ),
        Err(Error::Rate { rab: CS_RAB })
    ));
}

#[test]
fn composition_capacity_and_duplicate_identities_fail_before_returning_a_plan() {
    let request = ps_request(PACKET_RATE);
    let service = packet(&request, false);
    let physical = [fixture::physical()];
    let rabs = [
        rab(CnDomain::Ps, &request, Service::Packet(&service)),
        rab(CnDomain::Ps, &request, Service::Packet(&service)),
    ];
    assert!(matches!(
        plan_initial(&fixture::retained(), &rabs, &limits(), &policy(&physical)),
        Err(Error::Identity)
    ));
    let mut bounded = policy(&physical);
    bounded.maximum_combinations = count(2);
    assert!(matches!(
        plan_initial(&fixture::retained(), &rabs[..1], &limits(), &bounded),
        Err(Error::CombinationBudget { maximum: 2, .. })
    ));
    let mut capability = limits();
    capability.uplink.formats = 3;
    assert!(
        plan_initial(
            &fixture::retained(),
            &rabs[..1],
            &capability,
            &policy(&physical)
        )
        .is_err()
    );
    let mut oversized = packet(&request, false);
    oversized.uplink.maximum_blocks = 32;
    assert!(matches!(
        plan_initial(
            &fixture::retained(),
            &[rab(CnDomain::Ps, &request, Service::Packet(&oversized))],
            &limits(),
            &policy(&physical)
        ),
        Err(Error::Unsupported)
    ));
}

fn signalling() -> (crate::common_tx::CommonTx, CommonSignalling, UeKey) {
    use crate::signalling::tests as fixture;
    let source = test_support::source(false);
    let mut radio = fixture::controller(&source);
    let key = fixture::establish(&mut radio, &source, 0);
    radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    let mut peer = fixture::peer(&source);
    peer.enqueue(&fixture::COMPLETE).unwrap();
    peer.publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    fixture::send_capability_report(&mut radio, key, &mut peer);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    (source, radio, key)
}

#[test]
fn actual_capability_owner_and_zero_width_srb1_bridge_reach_checked_allocation() {
    let (_source, radio, key) = signalling();
    let retained = radio.retained_signalling(key).unwrap();
    assert_eq!(
        retained
            .iter()
            .find(|rb| rb.id == 1)
            .unwrap()
            .uplink_pdu_bits,
        0
    );
    let request = ps_request(PACKET_RATE);
    let service = packet(&request, false);
    let physical = [fixture::physical()];
    let mut serving_policy = policy(&physical);
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    serving_policy.signalling.um_uplink_bits = control.uplink_pdu_bits;
    serving_policy.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let plan = plan_initial_for_ue(
        key,
        &radio,
        &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
        &serving_policy,
    )
    .unwrap();
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let prepared = plan.allocate(key, &radio, &mut resources).unwrap();
    assert_eq!(resources.outstanding(), 1);
    assert!(Arc::ptr_eq(
        prepared.setup.configuration(),
        prepared.allocation.plan().configuration()
    ));
    assert!(Arc::ptr_eq(
        prepared.profiles.plan(),
        prepared.allocation.plan()
    ));
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn real_ue_capability_revalidation_rolls_back_a_plan_built_for_a_larger_capability() {
    let (_source, radio, key) = signalling();
    let cs = cs_request();
    let cs_second = request(
        CnDomain::Cs,
        CS_RAB + 1,
        cs.first.parameters.clone().expect("circuit parameters"),
    );
    let circuit_second = circuit(&cs_second);
    let circuit = circuit(&cs);
    let rabs = [
        rab(CnDomain::Cs, &cs, Service::Circuit(&circuit)),
        rab(CnDomain::Cs, &cs_second, Service::Circuit(&circuit_second)),
    ];
    let physical = [fixture::physical()];
    let retained = radio.retained_signalling(key).unwrap();
    let mut serving_policy = policy(&physical);
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    serving_policy.signalling.um_uplink_bits = control.uplink_pdu_bits;
    serving_policy.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let plan = plan_initial(&retained, &rabs, &limits(), &serving_policy).unwrap();
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    assert!(matches!(
        plan.allocate(key, &radio, &mut resources),
        Err(Error::Resources(crate::radio_resources::Error::Capability(
            _
        )))
    ));
    assert_eq!(resources.outstanding(), 0);
    let smaller = plan_initial_for_ue(key, &radio, &rabs[..1], &serving_policy).unwrap();
    let prepared = smaller.allocate(key, &radio, &mut resources).unwrap();
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn yaml_policy_drives_canonical_cs_packet_and_combined_plans() {
    let config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let circuit = circuit(&cs);
    let am = packet(&ps, false);
    let um = packet(&ps, true);
    let configured_services = ConfiguredPolicy::new(&config).unwrap();
    let combined = configured_services
        .services()
        .select(&[(CnDomain::Cs, &cs), (CnDomain::Ps, &ps)])
        .unwrap();
    assert_eq!(combined.rabs().len(), 2);
    for rabs in [
        vec![rab(CnDomain::Cs, &cs, Service::Circuit(&circuit))],
        vec![rab(CnDomain::Ps, &ps, Service::Packet(&am))],
        vec![rab(CnDomain::Ps, &ps, Service::Packet(&um))],
    ] {
        let plan =
            plan_initial(&fixture::retained(), &rabs, &limits(), &configured.policy()).unwrap();
        let message = plan
            .setup()
            .message(TransactionId::new(0).unwrap())
            .unwrap();
        assert!(matches!(
            umts_rrc_codec::decode_downlink_dedicated(&message.bytes, message.bit_length)
                .unwrap()
                .value
                .message,
            DLDCCHMessageType::radioBearerSetup(_)
        ));
        assert!(Arc::ptr_eq(plan.profiles().plan(), plan.compiled()));
        assert_eq!(plan.rabs().len(), rabs.len());
    }
}

#[test]
fn shared_packet_defaults_select_real_dch_capacity_for_a_rel99_only_ue() {
    let (_source, radio, key) = signalling();
    let config = umts_config::Deployment::parse(
        include_str!("../../../../../config/defaults.yaml"),
        include_str!("../../../../../config/example.yaml"),
    )
    .unwrap()
    .config;
    let configured = ConfiguredPolicy::new(&config).unwrap();
    assert!(configured.policy().hsdpa);
    assert!(!simultaneous::hsdpa(
        radio.capabilities(key).unwrap(),
        &configured.policy()
    ));
    let request = ps_request(1_600_000);
    let selected = configured
        .services()
        .select(&[(CnDomain::Ps, &request)])
        .unwrap();
    let plan = configured
        .plan_initial(key, &radio, selected.rabs())
        .unwrap();
    let configuration = plan.setup().configuration();
    assert_eq!(configuration.physical().downlink_sf, 8);
    let bearer = configuration
        .bearer(plan.setup().rabs()[0].bearers[0])
        .unwrap();
    let mapping = bearer.mappings(DchDirection::Downlink).next().unwrap();
    let transport = configuration
        .downlink()
        .transports()
        .iter()
        .find(|transport| transport.id == mapping.transport)
        .unwrap();
    assert_eq!(transport.interval, TransportInterval::Ms10);
    assert_eq!(
        transport
            .formats()
            .map(|format| u32::from(format.rlc_bits) * u32::from(format.blocks))
            .max(),
        Some(6120)
    );
}

#[test]
fn configured_policy_reaches_actual_ue_capability_checked_allocation() {
    let (_source, radio, key) = signalling();
    let retained = radio.retained_signalling(key).unwrap();
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    config.rel99.physical.planner.signalling.um_uplink_bits = control.uplink_pdu_bits;
    config.rel99.physical.planner.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let request = ps_request(PACKET_RATE);
    let service = packet(&request, false);
    let plan = configured
        .plan_initial(
            key,
            &radio,
            &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
        )
        .unwrap();
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let prepared = plan.allocate(key, &radio, &mut resources).unwrap();
    assert_eq!(resources.outstanding(), 1);
    assert_eq!(
        prepared
            .allocation
            .plan()
            .configuration()
            .physical()
            .primary_scrambling,
        cell.information().primary_code().index()
    );
    assert_eq!(
        prepared
            .allocation
            .plan()
            .configuration()
            .physical()
            .uplink_scrambling,
        17
    );
    assert!(Arc::ptr_eq(
        prepared.profiles.plan(),
        prepared.allocation.plan()
    ));
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn configured_service_catalog_drives_cs_am_um_and_combined_canonical_owners() {
    let cs = cs_request();
    let ps = ps_request(PACKET_RATE);
    let reference: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    for um in [false, true] {
        let mut config = reference.clone();
        if um {
            let packet = &mut config.rel99.physical.planner.services.packet[0];
            let buffers = packet.am.take().unwrap().buffers;
            packet.um = Some(umts_config::R99UmPolicy {
                uplink: buffers.clone(),
                downlink: buffers,
            });
            packet.name = "background-um".into();
        }
        let yaml = serde_yaml::to_string(&config).unwrap();
        let reloaded = serde_yaml::from_str(&yaml).unwrap();
        let configured = ConfiguredPolicy::new(&reloaded).unwrap();
        for requests in [
            vec![(CnDomain::Cs, &cs)],
            vec![(CnDomain::Ps, &ps)],
            vec![(CnDomain::Cs, &cs), (CnDomain::Ps, &ps)],
        ] {
            let selected = configured.services().select(&requests).unwrap();
            assert_eq!(selected.names().len(), requests.len());
            if requests.len() > 1 {
                assert_eq!(selected.rabs().len(), 2);
                continue;
            }
            let plan = plan_initial(
                &fixture::retained(),
                selected.rabs(),
                &limits(),
                &configured.policy(),
            )
            .unwrap();
            let message = plan
                .setup()
                .message(TransactionId::new(0).unwrap())
                .unwrap();
            umts_rrc_codec::decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
            let key = UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            };
            let endpoints = plan
                .profiles()
                .instantiate(key, [Some(Start::new(7).unwrap()); 2], at(0))
                .unwrap();
            assert_eq!(
                endpoints.len(),
                plan.rabs()
                    .iter()
                    .map(|rab| rab.bearers.len())
                    .sum::<usize>()
            );
            assert!(Arc::ptr_eq(plan.profiles().plan(), plan.compiled()));
            assert_eq!(selected.names().len(), requests.len());
        }
    }
}

#[test]
fn selected_yaml_service_reaches_actual_ue_resource_allocation() {
    let (_source, radio, key) = signalling();
    let retained = radio.retained_signalling(key).unwrap();
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    config.rel99.physical.planner.signalling.um_uplink_bits = control.uplink_pdu_bits;
    config.rel99.physical.planner.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let request = ps_request(PACKET_RATE);
    let selected = configured
        .services()
        .select(&[(CnDomain::Ps, &request)])
        .unwrap();
    assert_eq!(selected.names(), ["background-am"]);
    let plan = configured
        .plan_initial(key, &radio, selected.rabs())
        .unwrap();
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let prepared = plan.allocate(key, &radio, &mut resources).unwrap();
    assert!(Arc::ptr_eq(
        prepared.profiles.plan(),
        prepared.allocation.plan()
    ));
    assert_eq!(prepared.rabs[0].rab, PS_RAB);
    resources.cancel(prepared.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

pub(super) fn reference_transport_limits() -> Rel99Limits {
    let mut result = limits();
    for direction in [&mut result.uplink, &mut result.downlink] {
        direction.transport_bits = 6400;
        direction.convolutional_bits = 6400;
        direction.turbo_bits = Some(6400);
        direction.transport_blocks = 32;
        direction.transport_channels = 8;
        direction.formats = 64;
    }
    result.uplink.combinations = 64;
    result.downlink.combinations = 128;
    result
}

#[test]
fn simultaneous_hsdpa_packet_plan_fits_associated_downlink() {
    let config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/hsdpa.yaml"
    ))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let capabilities =
        umts_rrc::capabilities::hsdpa::SimultaneousDch::Kbps64.apply(reference_transport_limits());
    let mut physical = configured.policy().physical.to_vec();
    for candidate in &mut physical {
        candidate.downlink_sf = 32;
        candidate.downlink_pilot_bits = 8;
    }
    let policy = Policy {
        physical: &physical,
        ..configured.policy()
    };
    let request = ps_request(1_600_000);
    let selected = configured
        .services()
        .select(&[(CnDomain::Ps, &request)])
        .unwrap();
    let mut retained = fixture::retained();
    for bearer in &mut retained {
        if bearer.id != 1 {
            bearer.uplink_pdu_bits = 232;
        }
    }
    let plan = plan_initial(&retained, selected.rabs(), &capabilities, &policy).unwrap();
    capabilities
        .validate_dch(plan.setup().configuration())
        .unwrap();
    assert_eq!(plan.setup().configuration().physical().downlink_sf, 32);
}

#[test]
fn fifteen_code_trial_packet_plan_fits_sf128_associated_downlink_with_368_bit_pdus() {
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/hsdpa-fifteen-code-outer-loop.yaml"
    ))
    .unwrap();
    let capabilities =
        umts_rrc::capabilities::hsdpa::SimultaneousDch::Kbps64.apply(reference_transport_limits());
    let plan_with = |config: &umts_config::Config| {
        let configured = ConfiguredPolicy::new(config).unwrap();
        let request = ps_request(1_600_000);
        let selected = configured
            .services()
            .select(&[(CnDomain::Ps, &request)])
            .unwrap();
        let mut retained = fixture::retained();
        for bearer in &mut retained {
            if bearer.id != 1 {
                bearer.uplink_pdu_bits = 232;
            }
        }
        plan_initial(
            &retained,
            selected.rabs(),
            &capabilities,
            &configured.policy(),
        )
        .map(|plan| {
            capabilities
                .validate_dch(plan.setup().configuration())
                .unwrap();
            plan.setup().configuration().physical().downlink_sf
        })
    };
    let packet = &config.rel99.physical.planner.services.packet[0];
    assert_eq!(packet.downlink.rlc_bits, 368);
    // 70 MAC-d PDUs of 368 bits (25,781 bits with the MAC-hs header) cover the 25,558-bit
    // Category 10 block, which 320-bit PDUs cannot (22,421 bits).
    assert_eq!(plan_with(&config).unwrap(), 128);
    config.rel99.physical.planner.services.packet[0]
        .downlink
        .rlc_bits = 656;
    config.rel99.physical.planner.services.packet[0]
        .downlink
        .maximum_blocks = 6;
    assert_eq!(plan_with(&config).unwrap(), 128);
    for (bits, blocks) in [(656u16, 1u16), (1312, 1), (2144, 1)] {
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .rlc_bits = bits;
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .maximum_blocks = blocks;
        assert_eq!(plan_with(&config).unwrap(), 128, "{bits}-bit HS-DSCH PDUs");
    }
}

#[test]
fn fixed9_voice_profile_rejects_sf256_before_radio_resource_preference() {
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/hsupa-four-code.yaml"
    ))
    .unwrap();
    let capabilities =
        umts_rrc::capabilities::hsdpa::SimultaneousDch::Kbps32.apply(reference_transport_limits());
    let request = cs_request();
    let mut retained = fixture::retained();
    for bearer in &mut retained {
        if bearer.id != 1 {
            bearer.uplink_pdu_bits = 232;
            bearer.downlink_pdu_bits = 240;
        }
    }
    let wider = config.rel99.physical.planner.physical_candidates[0].clone();
    let narrow = config.rel99.physical.planner.physical_candidates[1].clone();
    for (physical, accepted) in [(narrow, false), (wider, true)] {
        config.rel99.physical.planner.physical_candidates = vec![physical];
        let configured = ConfiguredPolicy::new(&config).unwrap();
        let selected = configured
            .services()
            .select(&[(CnDomain::Cs, &request)])
            .unwrap();
        let plan = plan_initial(
            &retained,
            selected.rabs(),
            &capabilities,
            &configured.policy(),
        );
        assert_eq!(
            plan.is_ok(),
            accepted,
            "{}",
            config.rel99.physical.planner.physical_candidates[0].downlink_sf
        );
    }
}

#[test]
fn peer_voice_capture_transport_plan_matches_initial_cs_composition() {
    let config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/hsupa-four-code.yaml"
    ))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let request = cs_request();
    let selected = configured
        .services()
        .select(&[(CnDomain::Cs, &request)])
        .unwrap();
    let mut retained = fixture::retained();
    for bearer in &mut retained {
        bearer.uplink_pdu_bits = 232;
        bearer.downlink_pdu_bits = 240;
    }
    let plan = plan_initial(
        &retained,
        selected.rabs(),
        &reference_transport_limits(),
        &configured.policy(),
    )
    .unwrap();
    let replay: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/voice-uplink.json"
    ))
    .unwrap();
    assert_eq!(
        serde_json::to_value(plan.setup().configuration().uplink().ctfc_values()).unwrap(),
        replay["ctfc"]
    );
    let configured = plan.compiled().uplink_configured();
    for expected in replay["transports"].as_array().unwrap() {
        let id =
            umts_dsp::dedicated_transport::TransportId::new(expected["id"].as_u64().unwrap() as u8)
                .unwrap();
        let actual = configured.definition(id).unwrap();
        assert_eq!(
            actual.interval.frames() as u64 * 10,
            expected["tti_ms"].as_u64().unwrap()
        );
        assert_eq!(
            actual.attribute.get() as u64,
            expected["attribute"].as_u64().unwrap()
        );
        for (index, format) in actual.formats.iter().enumerate() {
            let bits = expected["block_bits"]
                .as_u64()
                .unwrap_or_else(|| expected["block_bits"][index].as_u64().unwrap());
            assert_eq!(format.block_bits as u64, bits);
        }
        assert_eq!(
            serde_json::to_value(
                actual
                    .formats
                    .iter()
                    .map(|format| format.blocks)
                    .collect::<Vec<_>>()
            )
            .unwrap(),
            expected["block_counts"]
        );
    }
}

#[test]
fn hsupa_uplink_plan_retains_source_derived_transport_configuration() {
    let config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/hsupa-coexistence.yaml"
    ))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let limits =
        umts_rrc::capabilities::hsdpa::SimultaneousDch::Kbps64.apply(reference_transport_limits());
    let mut physical = configured.policy().physical.to_vec();
    for candidate in &mut physical {
        candidate.downlink_sf = 32;
        candidate.downlink_pilot_bits = 8;
    }
    let policy = Policy {
        physical: &physical,
        ..configured.policy()
    };
    let request = ps_request(1_600_000);
    let selected = configured
        .services()
        .select(&[(CnDomain::Ps, &request)])
        .unwrap();
    let mut retained = fixture::retained();
    for bearer in &mut retained {
        if bearer.id != 1 {
            bearer.uplink_pdu_bits = 232;
        }
    }
    let plan = plan_initial(&retained, selected.rabs(), &limits, &policy).unwrap();
    let replay: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/hsupa-uplink.json"
    ))
    .unwrap();
    assert_eq!(
        serde_json::to_value(plan.setup().configuration().uplink().ctfc_values()).unwrap(),
        replay["ctfc"]
    );
    let physical = plan.setup().configuration().physical();
    assert_eq!(
        u64::from(physical.uplink_scrambling),
        replay["scrambling_code"].as_u64().unwrap()
    );
    assert_eq!(
        u64::from(physical.puncturing_percent),
        replay["puncturing_percent"].as_u64().unwrap()
    );
    let configured = plan.compiled().uplink_configured();
    for expected in replay["transports"].as_array().unwrap() {
        let id =
            umts_dsp::dedicated_transport::TransportId::new(expected["id"].as_u64().unwrap() as u8)
                .unwrap();
        let actual = configured.definition(id).unwrap();
        assert_eq!(
            actual.interval.frames() as u64 * 10,
            expected["tti_ms"].as_u64().unwrap()
        );
        assert_eq!(
            actual.attribute.get() as u64,
            expected["attribute"].as_u64().unwrap()
        );
        assert!(
            actual
                .formats
                .iter()
                .all(|format| format.block_bits as u64 == expected["block_bits"].as_u64().unwrap()),
            "transport {}: {:?}",
            id.get(),
            actual.formats
        );
        let counts: Vec<_> = actual.formats.iter().map(|format| format.blocks).collect();
        assert_eq!(
            serde_json::to_value(counts).unwrap(),
            expected["block_counts"]
        );
    }
}

#[test]
fn high_requested_rate_plans_encode_and_instantiate_with_complete_reference_transport_limits() {
    let request = ps_request(1_600_000);
    let mut service = packet(&request, false);
    service.uplink.maximum_blocks = 31;
    service.downlink.maximum_blocks = 31;
    let mut retained = fixture::retained();
    for bearer in &mut retained {
        if bearer.id != 1 {
            bearer.uplink_pdu_bits = 232;
        }
    }
    let config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let capabilities = reference_transport_limits();
    let plan = plan_initial(
        &retained,
        &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
        &capabilities,
        &configured.policy(),
    )
    .unwrap();
    capabilities
        .validate_dch(plan.setup().configuration())
        .unwrap();
    assert_eq!(
        plan.rabs()[0].maximum,
        qos::Rates::Symmetric { both: 1_600_000 }
    );
    let encoded = plan
        .setup()
        .message(TransactionId::new(0).unwrap())
        .unwrap();
    assert!(matches!(
        umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
            .unwrap()
            .value
            .message,
        DLDCCHMessageType::radioBearerSetup(_)
    ));
    let key = UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    };
    assert_eq!(
        plan.profiles()
            .instantiate(key, [Some(Start::new(3).unwrap()); 2], at(0))
            .unwrap()
            .len(),
        1
    );
    let cs = cs_request();
    let voice = circuit(&cs);
    let initial = plan_initial(
        &retained,
        &[rab(CnDomain::Cs, &cs, Service::Circuit(&voice))],
        &capabilities,
        &configured.policy(),
    )
    .unwrap();
    let combined = plan_existing(
        initial.compiled().clone(),
        initial.rabs(),
        &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
        &capabilities,
        &configured.policy(),
    )
    .unwrap();
    capabilities
        .validate_dch(combined.compiled().configuration())
        .unwrap();
    assert_eq!(combined.retained_rabs().len(), 1);
    assert_eq!(combined.rabs().len(), 1);
}

#[test]
fn simultaneous_edch_voice_adapts_both_bearer_addition_orders() {
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    config.rel99.physical.planner.services.packet[0]
        .uplink
        .rlc_bits = 952;
    config.rel99.physical.planner.services.packet[0]
        .uplink
        .maximum_blocks = 4;
    config.rel99.physical.planner.services.packet[0]
        .uplink
        .interval_ms = 10;
    config.rel99.physical.initial_ul_puncturing_limit_percent = 76;
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let ordinary = reference_transport_limits();
    let limits = umts_rrc::capabilities::hsupa::simultaneous_dch_limits(ordinary);
    assert_eq!(limits.downlink, ordinary.downlink);
    assert_eq!(limits.uplink_physical_bits, 2400);
    let mut physical = configured.policy().physical.to_vec();
    for candidate in &mut physical {
        simultaneous::adapt_uplink(candidate).unwrap();
    }
    let policy = Policy {
        physical: &physical,
        ..configured.policy()
    };
    let cs = cs_request();
    let voice = circuit(&cs);
    let ps = ps_request(1_600_000);
    for packet_first in [false, true] {
        let voice_rab = rab(CnDomain::Cs, &cs, Service::Circuit(&voice));
        let selected = configured
            .services()
            .select(&[(CnDomain::Ps, &ps)])
            .unwrap();
        let data_rab = &selected.rabs()[0];
        let (first, second) = if packet_first {
            (data_rab, &voice_rab)
        } else {
            (&voice_rab, data_rab)
        };
        let initial = plan_initial(
            &fixture::retained(),
            std::slice::from_ref(first),
            &ordinary,
            &configured.policy(),
        )
        .unwrap();
        let combined = plan_existing(
            initial.compiled().clone(),
            initial.rabs(),
            std::slice::from_ref(second),
            &limits,
            &policy,
        )
        .unwrap_or_else(|error| panic!("packet_first={packet_first}: {error:?}"));
        limits
            .validate_dch(combined.compiled().configuration())
            .unwrap();
        assert_eq!(
            combined
                .compiled()
                .configuration()
                .physical()
                .uplink_minimum_sf,
            16
        );
        assert_eq!(combined.retained_rabs().len(), 1);
        assert_eq!(combined.rabs().len(), 1);
    }
}

#[test]
fn guaranteed_packet_rate_is_rechecked_after_ue_budget_selection() {
    for (rate, accepted) in [(128_000, true), (300_000, false)] {
        let mut request = ps_request(1_600_000);
        let qos = request.first.parameters.as_mut().unwrap();
        qos.traffic_class = qos::TrafficClass::Streaming;
        qos.guaranteed = Some(qos::GuaranteedRates::new(&[rate]).unwrap());
        qos.transfer_delay = Some(80);
        qos.source_statistics = Some(qos::SourceStatistics::Unknown);
        let mut service = packet(&request, false);
        service.contract.maximum_transfer_delay_ms = Some(80);
        service.uplink.maximum_blocks = 31;
        service.downlink.maximum_blocks = 31;
        let physical = [fixture::physical()];
        let result = plan_initial(
            &fixture::retained(),
            &[rab(CnDomain::Ps, &request, Service::Packet(&service))],
            &reference_transport_limits(),
            &policy(&physical),
        );
        if accepted {
            result.unwrap();
        } else {
            assert!(matches!(result, Err(Error::Rate { rab: PS_RAB })));
        }
    }
}

#[test]
fn hsdpa_selects_zero_dch_blocks_without_a_zero_block_service_profile() {
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../rust/test-support/fixtures/config/hsdpa-fifteen-code-outer-loop.yaml"
    ))
    .unwrap();
    let capabilities =
        umts_rrc::capabilities::hsdpa::SimultaneousDch::Kbps64.apply(reference_transport_limits());
    let plan_with = |config: &umts_config::Config| {
        let configured = ConfiguredPolicy::new(config).unwrap();
        let request = ps_request(1_600_000);
        let selected = configured
            .services()
            .select(&[(CnDomain::Ps, &request)])
            .unwrap();
        let mut retained = fixture::retained();
        for bearer in &mut retained {
            if bearer.id != 1 {
                bearer.uplink_pdu_bits = 232;
            }
        }
        plan_initial(
            &retained,
            selected.rabs(),
            &capabilities,
            &configured.policy(),
        )
        .map(|plan| {
            capabilities
                .validate_dch(plan.setup().configuration())
                .unwrap();
            let configuration = plan.setup().configuration();
            let expected_bits = config.hspa.downlink.rlc_pdu_bits;
            for bearer in configuration
                .bearers()
                .iter()
                .filter(|bearer| bearer.domain == Some(CnDomain::Ps))
            {
                for mapping in &bearer.mapping.downlink {
                    for format in configuration
                        .downlink()
                        .transport(mapping.transport)
                        .unwrap()
                        .formats()
                    {
                        assert_eq!(format.rlc_bits, expected_bits);
                        if expected_bits >= 4096 {
                            // Larger than the kbps64 associated-DCH
                            // transport budget: data must use HS-DSCH.
                            assert_eq!(format.blocks, 0);
                        }
                    }
                }
            }
            configuration.physical().downlink_sf
        })
    };
    assert_eq!(config.hspa.downlink.rlc_pdu_bits, 4640);
    assert_eq!(plan_with(&config).unwrap(), 128);
    for (bits, blocks) in [(656u16, 1u16), (1312, 1), (4096, 3)] {
        config.hspa.downlink.rlc_pdu_bits = bits;
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .rlc_bits = bits;
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .maximum_blocks = blocks;
        assert_eq!(plan_with(&config).unwrap(), 128);
    }
    for bits in [656u16, 1312, 2144, 4096, 4640, 4992] {
        config.hspa.downlink.rlc_pdu_bits = bits;
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .rlc_bits = bits;
        config.rel99.physical.planner.services.packet[0]
            .downlink
            .maximum_blocks = 0;
        assert_eq!(
            plan_with(&config).unwrap(),
            128,
            "{bits}-bit zero-block PDUs"
        );
    }
}
