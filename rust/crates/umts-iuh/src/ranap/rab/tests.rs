// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use qos::{
    Asymmetry, Delivery, DeliveryOrder, Format, Formats, GuaranteedRates, MaxSduBits, MaximumRates,
    Parameters, ResidualError, SduError, SourceStatistics, Subflow, Subflows, TrafficClass,
};

const AMR_BITS: u16 = 244;
const AMR_RATE: u32 = 12_200;
const PACKET_RATE: u32 = 384_000;
const NORMAL_RELEASE: u16 = 83;
const UNKNOWN_ITEM: u16 = 50_000;
const RESPONSE_SETUP_LIST: u16 = 52;
const RESPONSE_RELEASED_LIST: u16 = 43;
const RESPONSE_QUEUED_LIST: u16 = 38;
const RESPONSE_FAILED_LIST: u16 = 35;
const RESPONSE_RELEASE_FAILED_LIST: u16 = 39;

fn address() -> Address {
    Address::new(BitString::from_vec(vec![127, 0, 0, 1])).unwrap()
}

fn full_setup(id: u8, domain: Domain) -> SetupOrModify {
    let voice = domain == Domain::Cs;
    let rate = if voice { AMR_RATE } else { PACKET_RATE };
    let size = if voice { AMR_BITS } else { 12_000 };
    let formats = if voice {
        Some(Formats::new(vec![Format::new(Some(AMR_BITS), None).unwrap()]).unwrap())
    } else {
        None
    };
    let mut parameters = Parameters::new(
        if voice {
            TrafficClass::Conversational
        } else {
            TrafficClass::Background
        },
        Asymmetry::SymmetricBidirectional,
        MaximumRates::new(&[rate]).unwrap(),
        DeliveryOrder::Requested,
        MaxSduBits::new(size).unwrap(),
        Subflows::new(vec![
            Subflow::new(
                ResidualError::new(1, 6).unwrap(),
                if voice { Delivery::Yes } else { Delivery::No },
                Some(SduError::new(1, 3).unwrap()),
                formats,
            )
            .unwrap(),
        ])
        .unwrap(),
    );
    if voice {
        parameters.guaranteed = Some(GuaranteedRates::new(&[rate]).unwrap());
        parameters.transfer_delay = Some(80);
        parameters.source_statistics = Some(SourceStatistics::Speech);
    }
    let mut first = First::modification(id);
    first.parameters = Some(parameters);
    first.user_plane = Some(
        UserPlane::new(
            if voice {
                Mode::PredefinedSdu
            } else {
                Mode::Transparent
            },
            3,
        )
        .unwrap(),
    );
    first.transport = Some(Transport::new(
        address(),
        if voice {
            Association::binding([0x75, 0x30, 0, 0])
        } else {
            Association::gtp(0x1234_5678)
        },
    ));
    let mut second = Second::default();
    if !voice {
        second.pdp_types = Some(PdpTypes::new(vec![PdpType::Ipv4]).unwrap());
        second.reporting = Some(Reporting::DoNotReport);
        second.downlink_gtp_sequence = Some(u16::MAX);
        second.uplink_gtp_sequence = Some(0);
    }
    SetupOrModify { first, second }
}

fn release(id: u8) -> Release {
    Release::new(id, Cause::from_code(NORMAL_RELEASE).unwrap())
}

fn raw_pair(item: &SetupOrModify) -> Pair {
    Pair {
        id: SETUP_ITEM,
        first_criticality: Criticality::Reject,
        first: Any::new(encode_value(&item.first).unwrap()),
        second_criticality: Criticality::Ignore,
        second: Any::new(encode_value(&item.second).unwrap()),
    }
}

#[test]
fn complete_cs_and_ps_assignments_preserve_the_actual_requested_fields() {
    for domain in [Domain::Cs, Domain::Ps] {
        let setup = full_setup(5, domain);
        setup.validate_new(domain).unwrap();
        let assignment = Assignment {
            setup: vec![setup.clone()],
            release: vec![],
        };
        let bytes = assignment.encode(Limits::default()).unwrap();
        let decoded = Assignment::decode(&bytes, Limits::default()).unwrap();
        assert_eq!(decoded.assignment, assignment);
        assert!(decoded.notify_ies.is_empty());
        let first = &decoded.assignment.setup[0].first;
        assert_eq!(first.id.get(), 5);
        assert_eq!(first.user_plane.as_ref().unwrap().version_mask(), 3);
        first
            .transport
            .as_ref()
            .unwrap()
            .association
            .validate_domain(domain)
            .unwrap();
        assert_eq!(
            first.parameters.as_ref().unwrap().maximum.values().next(),
            Some(if domain == Domain::Cs {
                AMR_RATE
            } else {
                PACKET_RATE
            })
        );
        assert!(
            setup
                .validate_new(if domain == Domain::Cs {
                    Domain::Ps
                } else {
                    Domain::Cs
                })
                .is_err()
        );
    }
    let mut invalid = full_setup(5, Domain::Ps);
    invalid.first.parameters.as_mut().unwrap().traffic_class = TrafficClass::Conversational;
    let mixed = Assignment {
        setup: vec![invalid, full_setup(6, Domain::Ps)],
        release: vec![],
    };
    let decoded = Assignment::decode(&mixed.encode(Limits::default()).unwrap(), Limits::default())
        .unwrap()
        .assignment;
    assert!(decoded.setup[0].validate_new(Domain::Ps).is_err());
    decoded.setup[1].validate_new(Domain::Ps).unwrap();
}

#[test]
fn setup_list_wire_contains_containers_and_two_independent_open_values() {
    let setup = full_setup(5, Domain::Cs);
    let first = encode_value(&setup.first).unwrap();
    let second = encode_value(&setup.second).unwrap();
    let bytes = Assignment {
        setup: vec![setup],
        release: vec![],
    }
    .encode(Limits::default())
    .unwrap();
    let packet = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
    let value = packet.required(SETUP_LIST).unwrap().value.as_bytes();
    let mut expected = vec![0, 0, 1, 0, SETUP_ITEM as u8, 0];
    assert!(first.len() < 128 && second.len() < 128);
    expected.push(first.len() as u8);
    expected.extend_from_slice(&first);
    expected.push(0x40);
    expected.push(second.len() as u8);
    expected.extend_from_slice(&second);
    assert_eq!(value, expected);
    preflight_rab_list(value, true, Limits::default()).unwrap();
    assert!(preflight_rab_list(value, false, Limits::default()).is_err());
}

#[test]
fn independent_release_vector_preserves_nested_ids_and_normal_nas_cause() {
    // TS 25.413 release item packs its two SEQUENCE bits, eight-bit RAB ID and NAS cause 83.
    let literal = [
        0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x01, 0x00, 0x29, 0x40, 0x0a, 0x00, 0x00, 0x01, 0x00,
        0x28, 0x40, 0x03, 0x01, 0x48, 0x80,
    ];
    let expected = Assignment {
        setup: vec![],
        release: vec![release(5)],
    };
    assert_eq!(
        Assignment::decode(&literal, Limits::default())
            .unwrap()
            .assignment,
        expected
    );
    assert_eq!(expected.encode(Limits::default()).unwrap(), literal);
    for length in 0..literal.len() {
        assert!(Assignment::decode(&literal[..length], Limits::default()).is_err());
    }
}

#[test]
fn omitted_modification_fields_and_nas_only_changes_are_preserved() {
    let mut first = First::modification(7);
    let empty = SetupOrModify {
        first: first.clone(),
        second: Second::default(),
    };
    empty.validate(Domain::Cs).unwrap();
    assert!(empty.validate_new(Domain::Cs).is_err());
    first.nas_synchronization = Some(fixed(&[0xa0]).unwrap());
    let assignment = Assignment {
        setup: vec![SetupOrModify {
            first,
            second: Second::default(),
        }],
        release: vec![],
    };
    let decoded = Assignment::decode(
        &assignment.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap()
    .assignment;
    assert_eq!(decoded, assignment);
    let first = &decoded.setup[0].first;
    assert!(first.parameters.is_none() && first.user_plane.is_none() && first.transport.is_none());
    assert_eq!(octets(first.nas_synchronization.as_ref().unwrap()), [0xa0]);
    assert_eq!(first.nas_synchronization_value(), Some(10));
}

#[test]
fn mixed_setup_release_retains_both_operations_and_sequence_state() {
    let mut setup = full_setup(5, Domain::Ps);
    setup.second.downlink_npdu_sequence = Some(1234);
    setup.second.uplink_npdu_sequence = Some(5678);
    let assignment = Assignment {
        setup: vec![setup],
        release: vec![release(7), release(8)],
    };
    let bytes = assignment.encode(Limits::default()).unwrap();
    let packet = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
    assert_eq!(
        packet.ies.iter().map(|ie| ie.id).collect::<Vec<_>>(),
        [SETUP_LIST, RELEASE_LIST]
    );
    assert_eq!(
        Assignment::decode(&bytes, Limits::default())
            .unwrap()
            .assignment,
        assignment
    );
}

#[test]
fn all_five_response_lists_use_the_intermediate_outcome_and_distinct_item_ids() {
    let mut setup = Setup::new(1);
    setup.address = Some(address());
    setup.association = Some(Association::gtp(0x9876_5432));
    let mut released = Released::new(2);
    released.volumes = Some(Volumes::new(vec![Volume::new(u32::MAX, Some(u8::MAX))]).unwrap());
    released.downlink_gtp_sequence = Some(u16::MAX);
    released.uplink_gtp_sequence = Some(0);
    let response = AssignmentResponse {
        setup: vec![setup],
        released: vec![released],
        queued: vec![Queued::new(3)],
        failed: vec![Failed::new(4, Cause::from_code(66).unwrap())],
        release_failed: vec![Failed::new(5, Cause::from_code(30).unwrap())],
    };
    let bytes = response.encode(Limits::default()).unwrap();
    assert_eq!(bytes[0], 0x60);
    let packet = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
    assert_eq!(packet.outcome, Outcome::Intermediate);
    assert_eq!(
        packet.ies.iter().map(|ie| ie.id).collect::<Vec<_>>(),
        [
            RESPONSE_SETUP_LIST,
            RESPONSE_RELEASED_LIST,
            RESPONSE_QUEUED_LIST,
            RESPONSE_FAILED_LIST,
            RESPONSE_RELEASE_FAILED_LIST
        ]
    );
    assert_eq!(
        AssignmentResponse::decode(&bytes, Limits::default()).unwrap(),
        (response, vec![])
    );
    let mut wrong = packet;
    wrong.outcome = Outcome::Successful;
    assert_eq!(
        AssignmentResponse::decode(&wrong.encode(Limits::default()).unwrap(), Limits::default()),
        Err(Error::Unsupported)
    );
}

#[test]
fn duplicate_rab_ids_are_rejected_across_request_and_response_lists() {
    let assignment = Assignment {
        setup: vec![full_setup(5, Domain::Ps)],
        release: vec![release(5)],
    };
    assert_eq!(assignment.encode(Limits::default()), Err(Error::Identity));
    for response in [
        AssignmentResponse {
            setup: vec![Setup::new(5)],
            released: vec![Released::new(5)],
            ..AssignmentResponse::default()
        },
        AssignmentResponse {
            queued: vec![Queued::new(5)],
            failed: vec![Failed::new(5, Cause::from_code(66).unwrap())],
            ..AssignmentResponse::default()
        },
        AssignmentResponse {
            failed: vec![Failed::new(5, Cause::from_code(66).unwrap())],
            release_failed: vec![Failed::new(5, Cause::from_code(30).unwrap())],
            ..AssignmentResponse::default()
        },
    ] {
        assert_eq!(response.encode(Limits::default()), Err(Error::Identity));
    }
    let packet = Packet::initiating(
        Protocol::Ranap,
        RAB_ASSIGNMENT,
        Criticality::Reject,
        vec![
            encode_list(
                RELEASE_LIST,
                RELEASE_ITEM,
                &[release(5), release(5)],
                Limits::default(),
            )
            .unwrap(),
        ],
    );
    assert_eq!(
        Assignment::decode(
            &packet.encode(Limits::default()).unwrap(),
            Limits::default()
        ),
        Err(Error::Identity)
    );
}

#[test]
fn requests_and_responses_require_at_least_one_reported_rab() {
    assert!(Assignment::default().encode(Limits::default()).is_err());
    assert!(
        AssignmentResponse::default()
            .encode(Limits::default())
            .is_err()
    );
    let empty_request =
        Packet::initiating(Protocol::Ranap, RAB_ASSIGNMENT, Criticality::Reject, vec![]);
    assert!(
        Assignment::decode(
            &empty_request.encode(Limits::default()).unwrap(),
            Limits::default()
        )
        .is_err()
    );
    let mut empty_response = empty_request;
    empty_response.outcome = Outcome::Intermediate;
    assert!(
        AssignmentResponse::decode(
            &empty_response.encode(Limits::default()).unwrap(),
            Limits::default()
        )
        .is_err()
    );
}

#[test]
fn nested_list_preflight_bounds_item_counts_and_rejects_truncation_and_trailing_data() {
    let limits = Limits {
        maximum_ies: 2,
        ..Limits::default()
    };
    for pairs in [false, true] {
        assert_eq!(
            preflight_rab_list(&[0, 0xff, 0xff], pairs, limits),
            Err(Error::Capacity)
        );
        assert_eq!(
            preflight_rab_list(&[1, 0, 0, 0xff, 0xff], pairs, limits),
            Err(Error::Capacity)
        );
    }
    let setup = full_setup(5, Domain::Cs);
    let encoded = encode_value(&PairList(vec![PairContainer(vec![raw_pair(&setup)])])).unwrap();
    preflight_rab_list(&encoded, true, limits).unwrap();
    for length in 0..encoded.len() {
        assert!(preflight_rab_list(&encoded[..length], true, limits).is_err());
    }
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert_eq!(
        preflight_rab_list(&trailing, true, limits),
        Err(Error::Trailing)
    );
    let mut criticality = encoded.clone();
    criticality[5] = 0xc0;
    assert_eq!(
        preflight_rab_list(&criticality, true, limits),
        Err(Error::Decode)
    );
    assert_eq!(
        preflight_rab_list(
            &encoded,
            true,
            Limits {
                maximum_value_bytes: encoded.len() - 1,
                ..limits
            }
        ),
        Err(Error::Capacity)
    );
}

#[test]
fn cause_boundaries_preserve_choice_and_reject_reserved_value_256() {
    for code in [1, 64, 65, 80, 81, 96, 97, 112, 113, 128, 129, 255, 257, 512] {
        let cause = Cause::from_code(code).unwrap();
        assert_eq!(cause.code(), code);
        assert_eq!(
            decode_value::<Cause>(&encode_value(&cause).unwrap()).unwrap(),
            cause
        );
    }
    for code in [0, 256, 513] {
        assert!(Cause::from_code(code).is_err());
    }
    let reserved = Release::new(5, Cause::NonStandard(NonStandard(256)));
    let packet = Packet::initiating(
        Protocol::Ranap,
        RAB_ASSIGNMENT,
        Criticality::Reject,
        vec![encode_list(RELEASE_LIST, RELEASE_ITEM, &[reserved], Limits::default()).unwrap()],
    );
    assert!(
        Assignment::decode(
            &packet.encode(Limits::default()).unwrap(),
            Limits::default()
        )
        .is_err()
    );
}

#[test]
fn binding_port_uses_first_two_network_order_octets_without_reinterpreting_teid() {
    let binding = Association::binding([0x75, 0x30, 0xaa, 0xbb]);
    assert_eq!(binding.binding_port(), Some(30_000));
    assert_eq!(
        encode_value(&binding).unwrap(),
        [0x40, 0x75, 0x30, 0xaa, 0xbb]
    );
    let decoded = decode_value::<Association>(&[0x40, 0x75, 0x30, 0xaa, 0xbb]).unwrap();
    assert_eq!(decoded, binding);
    assert_eq!(Association::gtp(0x7530_aabb).binding_port(), None);
    assert_eq!(binding.validate_domain(Domain::Ps), Err(Error::Identity));
    assert_eq!(
        Association::gtp(1).validate_domain(Domain::Cs),
        Err(Error::Identity)
    );
    assert!(UserPlane::new(Mode::PredefinedSdu, 0).is_err());
}

#[test]
fn unknown_pair_criticality_is_applied_to_both_values_without_decoding_them() {
    for first_criticality in [
        Criticality::Ignore,
        Criticality::Notify,
        Criticality::Reject,
    ] {
        let setup = SetupOrModify {
            first: First::modification(5),
            second: Second::default(),
        };
        let list = PairList(vec![PairContainer(vec![
            raw_pair(&setup),
            Pair {
                id: UNKNOWN_ITEM,
                first_criticality,
                first: Any::new(vec![0xff]),
                second_criticality: Criticality::Notify,
                second: Any::new(vec![0xff]),
            },
        ])]);
        let packet = Packet::initiating(
            Protocol::Ranap,
            RAB_ASSIGNMENT,
            Criticality::Reject,
            vec![Ie::new(SETUP_LIST, Criticality::Ignore, &list).unwrap()],
        );
        let decoded = Assignment::decode(
            &packet.encode(Limits::default()).unwrap(),
            Limits::default(),
        );
        if first_criticality == Criticality::Reject {
            assert_eq!(decoded, Err(Error::UnknownRejectIe(UNKNOWN_ITEM)));
        } else {
            let decoded = decoded.unwrap();
            assert_eq!(decoded.assignment.setup, [setup]);
            assert_eq!(decoded.notify_ies, [UNKNOWN_ITEM]);
        }
    }
}
