// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::integrity::{OCTET_BITS, SEQUENCE_BITS, SEQUENCE_OFFSET};
use crate::{establishment::TransactionId, nas::build_downlink_nas};
use std::num::NonZeroU64;
use umts_rrc_codec::{decode_downlink_dedicated, encode_downlink_dedicated};
use umts_rrc_schema::information_elements::{
    IntegrityCheckInfo, MessageAuthenticationCode, RRCMessageSequenceNumber,
};
use umts_security::{
    Key,
    contexts::{CipherMaterial, KeySet},
};
use umts_types::{CnDomain, RadioEpoch, UeKey};

fn context(domain: CnDomain, generation: u64) -> IntegrityContext {
    let origin = KeyOrigin {
        ue: UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 9,
            generation: NonZeroU64::MIN,
        },
        domain,
        generation: NonZeroU64::new(generation).unwrap(),
    };
    KeySet::uia1(
        origin,
        CipherMaterial::Clear,
        Key::new([generation as u8; 16]),
    )
    .split()
    .1
}

fn message() -> DLDCCHMessage {
    let encoded =
        build_downlink_nas(TransactionId::new(1).unwrap(), CnDomain::Cs, &[5, 0x24]).unwrap();
    *decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
        .unwrap()
        .value
}

fn uplink(context: &IntegrityContext, bearer: u8, count: u32, fresh: u32) -> Vec<u8> {
    let mut bytes = vec![0x81, 0x23, 0x45, 0x67, 0x89, 0x14, 0xfa, 0xbc, 0xff];
    super::super::put(
        &mut bytes,
        super::super::MAC_OFFSET,
        super::super::MAC_BITS,
        u32::from(bearer),
    );
    super::super::put(&mut bytes, SEQUENCE_OFFSET, SEQUENCE_BITS, 0);
    let mac = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            count,
            fresh,
            Direction::Uplink,
            &bytes,
            bytes.len() * OCTET_BITS,
        )
        .unwrap();
    super::super::put(
        &mut bytes,
        super::super::MAC_OFFSET,
        super::super::MAC_BITS,
        u32::from_be_bytes(mac),
    );
    super::super::put(&mut bytes, SEQUENCE_OFFSET, SEQUENCE_BITS, count & 15);
    bytes
}

#[test]
fn publication_failure_keeps_count_and_generated_encoder_defines_mac_preimage() {
    let context = context(CnDomain::Cs, 1);
    let origin = context.origin();
    let mut stream = DownlinkIntegrity::new(context.clone(), 3, 0x12345678, 15).unwrap();
    let message = message();
    let mut rejected = None;
    assert_eq!(
        stream.publish_with(origin, &message, |encoded| {
            rejected = Some(encoded.bytes.clone());
            Err::<(), _>("queue full")
        }),
        Err(StreamError::Consumer("queue full"))
    );
    assert_eq!(stream.next_count(), Ok(15));
    let encoded = stream
        .publish_with(origin, &message, |encoded| {
            Ok::<_, ()>(encoded.bytes.clone())
        })
        .unwrap();
    assert_eq!(Some(&encoded), rejected.as_ref());
    assert_eq!(stream.next_count(), Ok(16));
    let decoded = decode_downlink_dedicated(&encoded, encoded.len() * OCTET_BITS).unwrap();
    let received = decoded.value.integrity_check_info.as_ref().unwrap();
    assert_eq!(received.rrc_message_sequence_number.0, 15);
    let mut preimage_message = message;
    let mut rb = MessageAuthenticationCode(Default::default());
    rb.0.set(30, true);
    rb.0.set(31, true);
    preimage_message.integrity_check_info =
        Some(IntegrityCheckInfo::new(rb, RRCMessageSequenceNumber(0)));
    let preimage = encode_downlink_dedicated(&preimage_message).unwrap();
    let expected = context
        .algorithm(origin)
        .unwrap()
        .authenticate(
            15,
            0x12345678,
            Direction::Downlink,
            &preimage.bytes,
            preimage.bytes.len() * OCTET_BITS,
        )
        .unwrap();
    for bit in 0..32 {
        assert_eq!(
            received.message_authentication_code.0[bit],
            expected[bit / 8] & (1 << (7 - bit % 8)) != 0
        );
    }
}

#[test]
fn failed_authentication_and_delivery_do_not_advance_uplink_or_allow_replay() {
    let context = context(CnDomain::Ps, 2);
    let origin = context.origin();
    let fresh = 0x12345678;
    let mut stream =
        UplinkIntegrity::new(context.clone(), 2, fresh, Start::new(7).unwrap(), Some(15)).unwrap();
    let bytes = uplink(&context, 2, 0x7010, fresh);
    let mut scratch = vec![0; bytes.len()];
    let mut corrupt = bytes.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert!(matches!(
        stream.receive_with(origin, &corrupt, &mut scratch, |_| Ok::<_, ()>(())),
        Err(StreamError::Protection(Error::Authentication))
    ));
    assert_eq!(stream.last_sequence(), Some(15));
    assert_eq!(
        stream.receive_with(origin, &bytes, &mut scratch, |_| Err::<(), _>("queue full")),
        Err(StreamError::Consumer("queue full"))
    );
    assert_eq!(stream.last_sequence(), Some(15));
    stream
        .receive_with(origin, &bytes, &mut scratch, |accepted| {
            assert_eq!(accepted.origin(), origin);
            assert_eq!(accepted.bearer(), 2);
            assert_eq!(accepted.count(), 0x7010);
            assert_eq!(accepted.bytes(), bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(stream.last_sequence(), Some(0));
    assert_eq!(
        stream.receive_with(origin, &bytes, &mut scratch, |_| Ok::<_, ()>(())),
        Err(StreamError::Duplicate)
    );
    let later = uplink(&context, 2, 0x7013, fresh);
    stream
        .receive_with(origin, &later, &mut scratch, |_| Ok::<_, ()>(()))
        .unwrap();
    assert!(matches!(
        stream.receive_with(origin, &bytes, &mut scratch, |_| Ok::<_, ()>(())),
        Err(StreamError::Protection(Error::Authentication))
    ));
    assert_eq!(stream.last_sequence(), Some(3));
}

#[test]
fn unrelated_domain_and_key_generations_never_reach_consumers() {
    let context = context(CnDomain::Cs, 1);
    let origin = context.origin();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, 5, 0).unwrap();
    let mut uplink = UplinkIntegrity::new(context, 2, 5, Start::new(0).unwrap(), None).unwrap();
    for wrong in [
        KeyOrigin {
            domain: CnDomain::Ps,
            ..origin
        },
        KeyOrigin {
            generation: NonZeroU64::new(2).unwrap(),
            ..origin
        },
    ] {
        assert_eq!(
            downlink.publish_with(wrong, &message(), |_| -> Result<(), ()> {
                panic!("wrong owner was published")
            }),
            Err(StreamError::Security(umts_security::Error::Origin))
        );
        assert_eq!(
            uplink.receive_with(wrong, &[0; 8], &mut [0; 8], |_| -> Result<(), ()> {
                panic!("wrong owner was accepted")
            }),
            Err(StreamError::Security(umts_security::Error::Origin))
        );
    }
    assert_eq!(downlink.next_count(), Ok(0));
    assert_eq!(uplink.last_sequence(), None);
}
