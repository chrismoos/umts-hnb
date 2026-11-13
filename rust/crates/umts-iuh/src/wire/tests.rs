// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn independent_aligned_envelope_vector_preserves_outcome_and_open_ie_value() {
    // TS 25.469 HNB REGISTER ACCEPT: extensible CHOICE 1, procedure 1, reject, RNC-ID 0x0123.
    let literal = [
        0x20, 0x01, 0x00, 0x09, 0x00, 0x00, 0x01, 0x00, 0x0e, 0x00, 0x02, 0x01, 0x23,
    ];
    let packet = Packet::decode(Protocol::Hnbap, &literal, Limits::default()).unwrap();
    assert_eq!(packet.outcome, Outcome::Successful);
    assert_eq!(packet.procedure, 1);
    assert_eq!(packet.criticality, Criticality::Reject);
    assert_eq!(packet.required(14).unwrap().decode::<u16>(), Ok(0x0123));
    assert_eq!(packet.encode(Limits::default()).unwrap(), literal);
    for length in 0..literal.len() {
        assert!(Packet::decode(Protocol::Hnbap, &literal[..length], Limits::default()).is_err());
    }
    let mut extra = literal.to_vec();
    extra.push(0);
    assert_eq!(
        Packet::decode(Protocol::Hnbap, &extra, Limits::default()),
        Err(Error::Trailing)
    );
}

#[test]
fn ranap_intermediate_outcome_is_distinct_from_three_way_hnbap_rua_choice() {
    let mut packet = Packet::initiating(Protocol::Ranap, 0, Criticality::Reject, vec![]);
    packet.outcome = Outcome::Intermediate;
    let bytes = packet.encode(Limits::default()).unwrap();
    assert_eq!(bytes[0], 0x60);
    assert_eq!(
        Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap(),
        packet
    );
    assert!(Packet::decode(Protocol::Rua, &bytes, Limits::default()).is_err());
    packet.protocol = Protocol::Rua;
    assert_eq!(packet.encode(Limits::default()), Err(Error::Unsupported));
}

#[test]
fn unknown_criticality_duplicate_ids_and_missing_required_ies_remain_explicit() {
    for criticality in [
        Criticality::Reject,
        Criticality::Ignore,
        Criticality::Notify,
    ] {
        let packet = Packet::initiating(
            Protocol::Rua,
            4,
            Criticality::Ignore,
            vec![InformationElement::new(500, criticality, &false).unwrap()],
        );
        let decoded = Packet::decode(
            Protocol::Rua,
            &packet.encode(Limits::default()).unwrap(),
            Limits::default(),
        )
        .unwrap();
        let expected = match criticality {
            Criticality::Reject => Err(Error::UnknownRejectIe(500)),
            Criticality::Ignore => Ok(vec![]),
            Criticality::Notify => Ok(vec![500]),
        };
        assert_eq!(decoded.check(&[], &[]), expected);
        assert_eq!(decoded.required(4), Err(Error::MissingIe(4)));
    }
    let ie = InformationElement::new(4, Criticality::Reject, &false).unwrap();
    let packet = Packet::initiating(Protocol::Rua, 4, Criticality::Ignore, vec![ie.clone(), ie]);
    assert_eq!(packet.encode(Limits::default()), Err(Error::DuplicateIe(4)));
}

#[test]
fn count_preflight_bounds_allocation_before_ie_container_decoding() {
    let packet = Packet::initiating(Protocol::Rua, 4, Criticality::Ignore, vec![]);
    let mut bytes = packet.encode(Limits::default()).unwrap();
    assert_eq!(bytes, [0, 4, 0x40, 3, 0, 0, 0]);
    bytes[5..7].copy_from_slice(&u16::MAX.to_be_bytes());
    assert_eq!(
        Packet::decode(Protocol::Rua, &bytes, Limits::default()),
        Err(Error::Capacity)
    );
    assert_eq!(
        Packet::decode(
            Protocol::Rua,
            &bytes,
            Limits {
                maximum_packet_bytes: 1,
                ..Limits::default()
            }
        ),
        Err(Error::Capacity)
    );
}

#[test]
fn known_ies_follow_object_set_order_while_unknown_ignore_and_notify_remain_compatible() {
    let mut packet = Packet::initiating(
        Protocol::Rua,
        1,
        Criticality::Ignore,
        vec![
            InformationElement::new(7, Criticality::Reject, &false).unwrap(),
            InformationElement::new(3, Criticality::Reject, &false).unwrap(),
        ],
    );
    assert_eq!(packet.check(&[7, 3], &[]), Ok(vec![]));
    packet.ies.insert(
        1,
        InformationElement::new(999, Criticality::Notify, &false).unwrap(),
    );
    assert_eq!(packet.check(&[7, 3], &[]), Ok(vec![999]));
    packet.ies.swap(0, 2);
    assert_eq!(packet.check(&[7, 3], &[]), Err(Error::IeOrder(7)));
}

#[test]
fn fragmented_open_values_and_root_extensions_are_bounded_without_changing_wire_limits() {
    let data = vec![0xa5; FRAGMENT_UNIT + 617];
    let mut packet = Packet::initiating(
        Protocol::Rua,
        4,
        Criticality::Ignore,
        vec![InformationElement {
            id: 4,
            criticality: Criticality::Reject,
            value: Any::new(data),
        }],
    );
    packet
        .extensions
        .push(InformationElement::new(99, Criticality::Ignore, &true).unwrap());
    let encoded = packet.encode(Limits::default()).unwrap();
    assert_eq!(
        Packet::decode(Protocol::Rua, &encoded, Limits::default()).unwrap(),
        packet
    );
    assert_eq!(
        Packet::decode(
            Protocol::Rua,
            &encoded,
            Limits {
                maximum_value_bytes: FRAGMENT_UNIT,
                ..Limits::default()
            }
        ),
        Err(Error::Capacity)
    );
    assert_eq!(
        Packet::decode(
            Protocol::Rua,
            &encoded,
            Limits {
                maximum_ies: 1,
                ..Limits::default()
            }
        ),
        Err(Error::Capacity)
    );
    for length in [1, 3, 4, 9, FRAGMENT_UNIT, encoded.len() - 1] {
        assert!(Packet::decode(Protocol::Rua, &encoded[..length], Limits::default()).is_err());
    }
}
