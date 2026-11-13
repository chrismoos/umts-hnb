// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn bits(input: &[u8], length: usize) -> Bits<'_> {
    Bits::new(input, length).unwrap()
}

#[test]
fn literal_tctfs_use_the_spec_width_and_preserve_non_octet_sdus() {
    let cases: &[(FddCommonChannel, &[u8], usize, CommonHeader)] = &[
        (
            FddCommonChannel::Rach,
            &[0x2c],
            8,
            CommonHeader::CommonControl,
        ),
        (FddCommonChannel::Fach, &[0x2c], 8, CommonHeader::Broadcast),
        (
            FddCommonChannel::Fach,
            &[0x40, 0xb0],
            14,
            CommonHeader::CommonControl,
        ),
        (
            FddCommonChannel::Fach,
            &[0x80, 0xb0],
            14,
            CommonHeader::CommonTraffic,
        ),
        (
            FddCommonChannel::Fach,
            &[0x50, 0xb0],
            14,
            CommonHeader::MulticastControl,
        ),
        (
            FddCommonChannel::Fach,
            &[0x5f, 0xb0],
            14,
            CommonHeader::MulticastSchedule,
        ),
        (
            FddCommonChannel::Fach,
            &[0x6e, 0xb0],
            14,
            CommonHeader::MulticastTraffic {
                channel: LogicalChannelId::new(15).unwrap(),
            },
        ),
    ];
    for &(transport, input, length, header) in cases {
        assert_eq!(common_header_bits(transport, header), Ok(length - 6));
        let pdu = decode_common(transport, bits(input, length)).unwrap();
        assert_eq!(pdu.header, header);
        assert_eq!(pdu.payload.len(), 6);
        assert_eq!(pdu.payload.read_u32(0, 6), Ok(0b101100));
        let mut encoded = [0xff; 2];
        assert_eq!(
            encode_common(transport, header, bits(&[0xb0], 6), &mut encoded),
            Ok(length)
        );
        assert_eq!(&encoded[..input.len()], input);
    }
}

#[test]
fn literal_common_dedicated_headers_put_type_identity_and_ct_in_spec_order() {
    for (transport, input) in [
        (FddCommonChannel::Rach, [0x5a, 0xbc, 0xd7, 0xa0]),
        (FddCommonChannel::Fach, [0xda, 0xbc, 0xd7, 0xa0]),
    ] {
        let pdu = decode_common(transport, bits(&input, 27)).unwrap();
        assert_eq!(common_header_bits(transport, pdu.header), Ok(24));
        assert_eq!(
            pdu.header,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(0xabcd),
                channel: LogicalChannelId::new(8).unwrap()
            }
        );
        assert_eq!(pdu.payload.read_u32(0, 3), Ok(5));
        let mut out = [0; 4];
        assert_eq!(
            encode_common(transport, pdu.header, pdu.payload, &mut out),
            Ok(27)
        );
        assert_eq!(out, input);
    }
    let input = [0xc1, 0x23, 0x45, 0x67, 0x80, 0xa0];
    let pdu = decode_common(FddCommonChannel::Fach, bits(&input, 43)).unwrap();
    assert_eq!(
        common_header_bits(FddCommonChannel::Fach, pdu.header),
        Ok(40)
    );
    assert_eq!(
        pdu.header,
        CommonHeader::Dedicated {
            identity: CommonIdentity::Utran(0x12345678),
            channel: LogicalChannelId::new(1).unwrap()
        }
    );
    assert_eq!(pdu.payload.read_u32(0, 3), Ok(5));
}

#[test]
fn rach_never_accepts_utran_identity_or_reserved_channel_codes() {
    assert_eq!(
        decode_common(FddCommonChannel::Rach, bits(&[0x40; 8], 64)).err(),
        Some(MacError::Identity)
    );
    for value in [0x80, 0xc0] {
        assert_eq!(
            decode_common(FddCommonChannel::Rach, bits(&[value], 8)).err(),
            Some(MacError::Reserved)
        );
    }
    for value in [0xe0, 0xf0] {
        assert_eq!(
            decode_common(FddCommonChannel::Fach, bits(&[value; 8], 64)).err(),
            Some(MacError::Reserved)
        );
    }
    for value in [0x41, 0x4f, 0x51, 0x5e, 0x70, 0x81, 0xbf] {
        assert_eq!(
            decode_common(FddCommonChannel::Fach, bits(&[value, 0], 16)).err(),
            Some(MacError::Reserved)
        );
    }
    assert_eq!(
        decode_common(FddCommonChannel::Fach, bits(&[0x6f, 0xa0], 12)).err(),
        Some(MacError::Reserved)
    );
    assert_eq!(
        decode_common(FddCommonChannel::Rach, bits(&[0x50, 0, 0x0f, 0xa0], 28)).err(),
        Some(MacError::Reserved)
    );
}

#[test]
fn truncated_headers_and_header_only_pdus_do_not_return_payload() {
    let input = [0xda, 0xbc, 0xd7, 0xa0];
    for length in 0..=24 {
        assert!(decode_common(FddCommonChannel::Fach, bits(&input, length)).is_err());
    }
    assert_eq!(
        decode_common(FddCommonChannel::Fach, bits(&[0x40], 8)).err(),
        Some(MacError::EmptyPayload)
    );
    assert_eq!(
        decode_common(FddCommonChannel::Rach, bits(&[0], 2)).err(),
        Some(MacError::EmptyPayload)
    );
}

#[test]
fn dedicated_multiplexing_is_explicit_and_headerless_mapping_does_not_steal_payload_bits() {
    let pdu = decode_dedicated(DedicatedMapping::Single, bits(&[0xa8], 5)).unwrap();
    assert_eq!(pdu.channel, None);
    assert_eq!(pdu.payload.read_u32(0, 5), Ok(0b10101));
    let pdu = decode_dedicated(DedicatedMapping::Multiplexed, bits(&[0xea, 0x80], 9)).unwrap();
    assert_eq!(pdu.channel.unwrap().get(), 15);
    assert_eq!(pdu.payload.read_u32(0, 5), Ok(0b10101));
    let mut output = [0xff; 2];
    assert_eq!(
        encode_dedicated(
            DedicatedMapping::Multiplexed,
            pdu.channel,
            pdu.payload,
            &mut output
        ),
        Ok(9)
    );
    assert_eq!(output, [0xea, 0x80]);
    assert_eq!(
        decode_dedicated(DedicatedMapping::Multiplexed, bits(&[0xf0], 8)).err(),
        Some(MacError::Reserved)
    );
}

#[test]
fn invalid_mapping_and_capacity_fail_before_modifying_output() {
    let mut output = [0xa5; 4];
    for header in [
        CommonHeader::Broadcast,
        CommonHeader::Dedicated {
            identity: CommonIdentity::Utran(7),
            channel: LogicalChannelId::new(1).unwrap(),
        },
    ] {
        assert_eq!(
            encode_common(
                FddCommonChannel::Rach,
                header,
                bits(&[0xff], 8),
                &mut output
            ),
            Err(MacError::Mapping)
        );
        assert_eq!(output, [0xa5; 4]);
    }
    assert_eq!(
        encode_common(
            FddCommonChannel::Fach,
            CommonHeader::CommonControl,
            bits(&[0xff; 4], 32),
            &mut output
        ),
        Err(MacError::Capacity)
    );
    assert_eq!(output, [0xa5; 4]);
    assert_eq!(
        encode_dedicated(
            DedicatedMapping::Multiplexed,
            None,
            bits(&[0xff], 8),
            &mut output
        ),
        Err(MacError::Mapping)
    );
    assert_eq!(output, [0xa5; 4]);
    assert!(LogicalChannelId::new(0).is_err());
    assert!(LogicalChannelId::new(16).is_err());
}
