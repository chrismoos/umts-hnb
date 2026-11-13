// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{establishment::TransactionId, nas::build_downlink_nas};
use umts_rrc_codec::decode_downlink_dedicated;
use umts_security::Key;
use umts_types::CnDomain;

fn parameters(key: &Uia1Key, direction: Direction) -> Parameters<'_> {
    Parameters {
        key,
        direction,
        count: 0x1234500f,
        fresh: 0x89abcdef,
        bearer: 3,
    }
}

#[test]
fn inserted_integrity_header_matches_generated_asn1_fields_and_preserves_payload() {
    let cipher = Uia1Key::new(&Key::new([0xa5; 16]));
    let original =
        build_downlink_nas(TransactionId::new(2).unwrap(), CnDomain::Ps, &[8, 0x42]).unwrap();
    let message = decode_downlink_dedicated(&original.bytes, original.bit_length).unwrap();
    let protected =
        protect_downlink(parameters(&cipher, Direction::Downlink), &message.value).unwrap();
    assert_eq!(protected.bit_length, original.bit_length + ADDED_BITS);
    let mut decoded = decode_downlink_dedicated(&protected.bytes, protected.bit_length).unwrap();
    let header = decoded.value.integrity_check_info.take().unwrap();
    assert_eq!(header.rrc_message_sequence_number.0, 15);
    assert_eq!(decoded.value.message, message.value.message);
    let mut preimage = protected.bytes.clone();
    put(&mut preimage, MAC_OFFSET, MAC_BITS, 3);
    put(&mut preimage, SEQUENCE_OFFSET, SEQUENCE_BITS, 0);
    assert_eq!(
        get(&protected.bytes, MAC_OFFSET, MAC_BITS).to_be_bytes(),
        cipher
            .authenticate(
                0x1234500f,
                0x89abcdef,
                Direction::Downlink,
                &preimage,
                preimage.len() * OCTET_BITS
            )
            .unwrap()
    );
}

#[test]
fn uplink_verifies_original_padding_and_extensions_without_decode_reencode() {
    let cipher = Uia1Key::new(&Key::new([0x5a; 16]));
    let mut bytes = vec![0x81, 0x23, 0x45, 0x67, 0x89, 0x14, 0xfa, 0xbc, 0xff];
    put(&mut bytes, MAC_OFFSET, MAC_BITS, 3);
    put(&mut bytes, SEQUENCE_OFFSET, SEQUENCE_BITS, 0);
    let mac = cipher
        .authenticate(
            0x1234500f,
            0x89abcdef,
            Direction::Uplink,
            &bytes,
            bytes.len() * 8,
        )
        .unwrap();
    put(&mut bytes, MAC_OFFSET, MAC_BITS, u32::from_be_bytes(mac));
    put(&mut bytes, SEQUENCE_OFFSET, SEQUENCE_BITS, 15);
    let mut scratch = vec![0; bytes.len()];
    let verified =
        verify_uplink(parameters(&cipher, Direction::Uplink), &bytes, &mut scratch).unwrap();
    assert_eq!(verified.bytes(), bytes);
    assert_eq!(verified.count(), 0x1234500f);
    for position in [MAC_OFFSET, PROTECTED_HEADER_BITS, bytes.len() * 8 - 1] {
        let mut corrupt = bytes.clone();
        corrupt[position / 8] ^= 1 << (7 - position % 8);
        assert!(matches!(
            verify_uplink(
                parameters(&cipher, Direction::Uplink),
                &corrupt,
                &mut scratch
            ),
            Err(Error::Authentication)
        ));
    }
    let mut wrong = parameters(&cipher, Direction::Uplink);
    wrong.count -= 1;
    assert!(matches!(
        verify_uplink(wrong, &bytes, &mut scratch),
        Err(Error::Sequence)
    ));
    assert!(matches!(
        verify_uplink(parameters(&cipher, Direction::Uplink), &bytes, &mut []),
        Err(Error::Length)
    ));
}

#[test]
fn direction_bearer_and_missing_header_fail_before_any_accepted_protection() {
    let key = Uia1Key::new(&Key::new([0; 16]));
    let mut invalid = parameters(&key, Direction::Uplink);
    invalid.bearer = 5;
    assert!(matches!(
        verify_uplink(invalid, &[0; 8], &mut [0; 8]),
        Err(Error::Bearer)
    ));
    assert!(matches!(
        verify_uplink(parameters(&key, Direction::Downlink), &[0; 8], &mut [0; 8]),
        Err(Error::Direction)
    ));
    assert!(matches!(
        verify_uplink(parameters(&key, Direction::Uplink), &[0; 8], &mut [0; 8]),
        Err(Error::Header)
    ));
}

#[test]
fn transparent_uplink_authenticates_exact_bits_at_every_octet_boundary() {
    let cipher = Uia1Key::new(&Key::new([0x5a; 16]));
    for unused in 0..OCTET_BITS {
        let mut bytes = [0x81, 0x23, 0x45, 0x67, 0x89, 0x14, 0xfa, 0xbc, 0xff];
        let bits = bytes.len() * OCTET_BITS - unused;
        put(&mut bytes, MAC_OFFSET, MAC_BITS, 0);
        put(&mut bytes, SEQUENCE_OFFSET, SEQUENCE_BITS, 0);
        let mut input = parameters(&cipher, Direction::Uplink);
        input.bearer = 0;
        let mac = cipher
            .authenticate(input.count, input.fresh, Direction::Uplink, &bytes, bits)
            .unwrap();
        put(&mut bytes, MAC_OFFSET, MAC_BITS, u32::from_be_bytes(mac));
        put(
            &mut bytes,
            SEQUENCE_OFFSET,
            SEQUENCE_BITS,
            input.count & SEQUENCE_MASK,
        );
        let mut scratch = [0; 9];
        let verified = verify_uplink_bits(input, &bytes, bits, &mut scratch).unwrap();
        assert_eq!(verified.bit_length(), bits);
        assert_eq!(verified.bytes(), bytes);
        let make_parameters = || {
            let mut value = parameters(&cipher, Direction::Uplink);
            value.bearer = 0;
            value
        };
        if unused != 0 {
            assert!(matches!(
                verify_uplink(make_parameters(), &bytes, &mut scratch),
                Err(Error::Authentication)
            ));
            bytes[8] ^= (1 << unused) - 1;
            verify_uplink_bits(make_parameters(), &bytes, bits, &mut scratch).unwrap();
        }
        bytes[(bits - 1) / OCTET_BITS] ^= 1 << (OCTET_BITS - 1 - (bits - 1) % OCTET_BITS);
        assert!(matches!(
            verify_uplink_bits(make_parameters(), &bytes, bits, &mut scratch),
            Err(Error::Authentication)
        ));
        for invalid in [0, PROTECTED_HEADER_BITS - 1, bytes.len() * OCTET_BITS + 1] {
            assert!(matches!(
                verify_uplink_bits(make_parameters(), &bytes, invalid, &mut scratch),
                Err(Error::Length)
            ));
        }
    }
}
