// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

// TS 25.331 UL-CCCH choice 1, no optional fields, IMSI length 15, establishment cause registration, no protocol error.
const REQUEST: [u8; 10] = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];
const REQUEST_BITS: usize = 78;
const START_BITS: usize = 20;

#[test]
fn independent_request_vector_decodes_identity_cause_optional_fields_and_exact_basic_length() {
    let decoded = decode_uplink_common(&REQUEST, REQUEST_BITS).unwrap();
    assert_eq!(decoded.consumed_bits, REQUEST_BITS);
    assert!(decoded.value.integrity_check_info.is_none());
    let ULCCCHMessageType::rrcConnectionRequest(request) = decoded.value.message else {
        panic!("not a connection request")
    };
    let ie::InitialUEIdentity::imsi(identity) = request.initial_ue_identity else {
        panic!("not an IMSI")
    };
    assert_eq!(
        identity.0.iter().map(|digit| digit.0).collect::<Vec<_>>(),
        vec![0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
    assert_eq!(
        request.establishment_cause,
        ie::EstablishmentCause::registration
    );
    assert_eq!(
        request.protocol_error_indicator,
        ie::ProtocolErrorIndicator::noError
    );
    assert!(request.measured_results_on_rach.is_none());
    assert!(request.v3d0_non_critical_extensions.is_none());
}

#[test]
fn arbitrary_extension_padding_is_accepted_but_missing_basic_bits_are_not() {
    for suffix in [0, 0xa5, 0xff] {
        let mut bytes = REQUEST.to_vec();
        bytes[9] |= suffix & 3;
        bytes.extend([suffix; 22]);
        let decoded = decode_uplink_common(&bytes, 254).unwrap();
        assert_eq!(decoded.consumed_bits, REQUEST_BITS);
    }
    for length in 0..REQUEST_BITS {
        assert!(decode_uplink_common(&REQUEST[..length.div_ceil(8)], length).is_err());
    }
    assert_eq!(
        decode_uplink_common(&[], MAX_UPLINK_COMMON_BITS + 1).err(),
        Some(CodecError::InvalidBitLength)
    );
    assert_eq!(
        decode_uplink_common(&REQUEST[..9], REQUEST_BITS).err(),
        Some(CodecError::InvalidBitLength)
    );
}

#[test]
fn protected_connection_requests_and_spare_common_messages_are_rejected() {
    let mut decoded = decode_uplink_common(&REQUEST, REQUEST_BITS).unwrap();
    decoded.value.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(FixedBitString::default()),
        ie::RRCMessageSequenceNumber(1),
    ));
    let protected = rasn::uper::encode(decoded.value.as_ref()).unwrap();
    assert_eq!(
        decode_uplink_common(&protected, protected.len() * 8).err(),
        Some(CodecError::InvalidRrcValue)
    );
    assert_eq!(
        decode_uplink_common(&[0x78], 5).err(),
        Some(CodecError::UnsupportedPayload)
    );
}

#[test]
fn dedicated_setup_completion_matches_independent_choice_and_domain_start_bits() {
    // STARTList has a two-bit length determinant for its ASN.1 bound 1..4, even with only CS and PS values.
    let input = [0x48, 0x48, 0x48, 0xd1, 0x6a, 0x86, 0x42];
    let decoded = decode_uplink_dedicated(&input, 55).unwrap();
    assert_eq!(decoded.consumed_bits, 55);
    assert!(decoded.value.integrity_check_info.is_none());
    let ULDCCHMessageType::rrcConnectionSetupComplete(body) = decoded.value.message else {
        panic!("not Setup Complete")
    };
    assert_eq!(body.rrc_transaction_identifier.0, 2);
    assert_eq!(body.start_list.0.len(), 2);
    assert_eq!(
        body.start_list.0[0].cn_domain_identity,
        ie::CNDomainIdentity::cs_domain
    );
    assert_eq!(
        body.start_list.0[1].cn_domain_identity,
        ie::CNDomainIdentity::ps_domain
    );
    assert_eq!(
        body.start_list.0[0]
            .start_value
            .0
            .iter()
            .take(START_BITS)
            .fold(0u32, |value, bit| (value << 1) | u32::from(*bit)),
        0x12345
    );
    assert_eq!(
        body.start_list.0[1]
            .start_value
            .0
            .iter()
            .take(START_BITS)
            .fold(0u32, |value, bit| (value << 1) | u32::from(*bit)),
        0x54321
    );
    for bits in 0usize..55 {
        assert!(decode_uplink_dedicated(&input[..bits.div_ceil(8)], bits).is_err());
    }
    let mut padded = input.to_vec();
    padded[6] |= 1;
    padded.extend([0xa5; 8]);
    assert_eq!(
        decode_uplink_dedicated(&padded, padded.len() * 8)
            .unwrap()
            .consumed_bits,
        55
    );
}

#[test]
fn protected_setup_messages_and_dedicated_spares_do_not_cross_the_codec_boundary() {
    // The 55-bit completion has an added 32-bit MAC and 4-bit sequence number after its presence bit.
    let protected = [
        0x80, 0, 0, 0, 0x0c, 0x84, 0x84, 0x8d, 0x16, 0xa8, 0x64, 0x20,
    ];
    assert_eq!(
        decode_uplink_dedicated(&protected, 91).err(),
        Some(CodecError::InvalidRrcValue)
    );
    assert_eq!(
        decode_uplink_dedicated(&[0x7f, 0xc0], 10).err(),
        Some(CodecError::UnsupportedPayload)
    );
    assert_eq!(
        decode_downlink_common(&[0x70], 4).err(),
        Some(CodecError::UnsupportedPayload)
    );
    assert_eq!(
        decode_uplink_dedicated(&[], MAX_DEDICATED_BITS + 1).err(),
        Some(CodecError::InvalidBitLength)
    );
}
