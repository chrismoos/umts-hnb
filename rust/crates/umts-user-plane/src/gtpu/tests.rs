// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn minimal_data_and_echo_have_specified_octets_not_private_tunnel_metadata() {
    let mut output = [0xaa; 32];
    let packet = Packet {
        kind: MessageType::Data,
        teid: Some(PeerTeid::new(0x10203040)),
        sequence: None,
        n_pdu: None,
        payload: &[0x45, 0, 0, 20],
    };
    let size = encode(packet, &mut output).unwrap();
    assert_eq!(
        &output[..size],
        &[0x30, 255, 0, 4, 0x10, 0x20, 0x30, 0x40, 0x45, 0, 0, 20]
    );
    assert_eq!(decode(&output[..size]).unwrap(), packet);
    let size = echo_response(0x1234, &mut output).unwrap();
    assert_eq!(
        &output[..size],
        &[0x32, 2, 0, 6, 0, 0, 0, 0, 0x12, 0x34, 0, 0, 14, 0]
    );
}

#[test]
fn every_optional_flag_combination_ignores_only_fields_marked_unused() {
    for flags in 0..=OPTIONAL_FLAGS {
        let mut wire = vec![VERSION_PROTOCOL | flags | 8, G_PDU, 0, 1, 0, 0, 0, 1];
        if flags != 0 {
            wire.extend([0x12, 0x34, 0x56, 0]);
        }
        wire.push(0x45);
        let size = (wire.len() - HEADER_BYTES) as u16;
        wire[2..4].copy_from_slice(&size.to_be_bytes());
        let packet = decode(&wire).unwrap();
        assert_eq!(packet.payload, &[0x45]);
        assert_eq!(packet.sequence, (flags & SEQUENCE != 0).then_some(0x1234));
        assert_eq!(packet.n_pdu, (flags & N_PDU != 0).then_some(0x56));
        if flags & EXTENSION == 0 && flags != 0 {
            wire[11] = 0xff;
            assert_eq!(decode(&wire).unwrap().payload, &[0x45]);
        }
    }
}

#[test]
fn extension_walk_is_bounded_and_unknown_required_headers_are_not_ignored() {
    let wire = [
        0x34, 255, 0, 13, 0, 0, 0, 1, 0, 0, 0, 1, 1, 0xaa, 0xbb, 2, 1, 0xcc, 0xdd, 0, 0x45,
    ];
    assert_eq!(decode(&wire).unwrap().payload, &[0x45]);
    let mut required = wire;
    required[11] = 0xc0;
    assert_eq!(decode(&required), Err(WireError::RequiredExtension(0xc0)));
    let mut zero = wire;
    zero[12] = 0;
    assert_eq!(decode(&zero), Err(WireError::Length));
    for length in 0..wire.len() {
        assert!(decode(&wire[..length]).is_err());
    }
}

#[test]
fn zero_peer_teid_is_distinct_from_local_allocation_and_signalling_sequence() {
    assert!(Teid::new(0).is_err());
    assert_eq!(
        decode(&[0x30, 255, 0, 0, 0, 0, 0, 0]).unwrap().teid,
        Some(PeerTeid::new(0))
    );
    assert_eq!(decode(&[0x30, 1, 0, 0, 0, 0, 0, 0]), Err(WireError::Header));
    assert_eq!(
        decode(&[0x10, 255, 0, 0, 0, 0, 0, 1]),
        Err(WireError::Version(0))
    );
    assert_eq!(
        decode(&[0x20, 255, 0, 0, 0, 0, 0, 1]),
        Err(WireError::Header)
    );
}

#[test]
fn error_indication_carries_the_offending_teid_and_the_local_address() {
    let mut output = [0_u8; 64];
    let size = error_indication(
        0x0102_0304,
        std::net::IpAddr::from([10, 0, 0, 20]),
        7,
        &mut output,
    )
    .unwrap();
    // Header: flags 0x32 (S), type 26, length 4 + 5 + 7, TEID 0, sequence 7, N-PDU 0, next ext 0.
    assert_eq!(
        &output[..size],
        &[
            0x32, 26, 0, 16, 0, 0, 0, 0, 0, 7, 0, 0, 16, 1, 2, 3, 4, 133, 0, 4, 10, 0, 0, 20
        ]
    );
    let packet = decode(&output[..size]).unwrap();
    assert_eq!(packet.kind, MessageType::ErrorIndication);
    assert_eq!(packet.teid, None);
    assert_eq!(
        parse_error_indication(packet.payload).unwrap(),
        ErrorIndication {
            teid: 0x0102_0304,
            address: std::net::IpAddr::from([10, 0, 0, 20]),
        }
    );
    assert!(parse_error_indication(&[16, 1, 2, 3]).is_err());
    assert!(parse_error_indication(&[16, 1, 2, 3, 4]).is_err());
    assert!(parse_error_indication(&[16, 1, 2, 3, 4, 133, 0, 3, 1, 2, 3]).is_err());
}

#[test]
fn version_not_supported_and_extension_notification_are_header_only_signalling() {
    let mut output = [0_u8; 64];
    let size = version_not_supported(9, &mut output).unwrap();
    assert_eq!(&output[..size], &[0x32, 3, 0, 4, 0, 0, 0, 0, 0, 9, 0, 0]);
    let packet = decode(&output[..size]).unwrap();
    assert_eq!(packet.kind, MessageType::VersionNotSupported);
    let size = supported_extension_headers(&[], 10, &mut output).unwrap();
    assert_eq!(
        &output[..size],
        &[0x32, 31, 0, 6, 0, 0, 0, 0, 0, 10, 0, 0, 141, 0]
    );
    assert!(matches!(
        decode(&[0x52, 255, 0, 0, 0, 0, 0, 1]),
        Err(WireError::Version(2))
    ));
}
