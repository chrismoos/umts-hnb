// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    establishment::DomainStarts,
    integrity::UplinkIntegrity,
    nas::{CoreContextId, tests::initial},
};
use rasn::types::FixedBitString;
use std::num::NonZeroU64;
use umts_rrc_codec::{DecodedUplinkDedicated, decode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions::DLDCCHMessageType, information_elements as ie, pdu_definitions as pdu,
};
use umts_security::{
    Key,
    contexts::{CipherMaterial, IntegrityContext, KeyOrigin, KeySet},
    counters::Start,
};
use umts_types::{CnDomain, Direction, RadioEpoch, UeKey};

const FRESH: u32 = 0x12345678;

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 7,
        generation: NonZeroU64::MIN,
    }
}

fn router(fourth: bool) -> NasRouter {
    NasRouter::new(
        key(),
        CoreContextId::new(9).unwrap(),
        DomainStarts::default(),
        fourth,
    )
}

fn context(key: UeKey) -> IntegrityContext {
    let origin = KeyOrigin {
        ue: key,
        domain: CnDomain::Ps,
        generation: NonZeroU64::MIN,
    };
    KeySet::uia1(origin, CipherMaterial::Clear, Key::new([0x5a; 16]))
        .split()
        .1
}

fn fixed(value: u32) -> FixedBitString<32> {
    let mut bits = FixedBitString::default();
    for bit in 0..32 {
        bits.set(bit, value & (1 << (31 - bit)) != 0)
    }
    bits
}

fn protected(
    context: &IntegrityContext,
    bearer: u8,
    count: u32,
    original: &DecodedUplinkDedicated,
) -> Vec<u8> {
    let mut message = original.value.as_ref().clone();
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(u32::from(bearer))),
        ie::RRCMessageSequenceNumber(0),
    ));
    let preimage = rasn::uper::encode(&message).unwrap();
    let mac = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            count,
            FRESH,
            Direction::Uplink,
            &preimage,
            preimage.len() * 8,
        )
        .unwrap();
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(mac))),
        ie::RRCMessageSequenceNumber((count & 15) as u8),
    ));
    rasn::uper::encode(&message).unwrap()
}

#[test]
fn shared_signalling_key_domain_does_not_override_cs_ps_nas_routing_or_start_values() {
    let context = context(key());
    let mut uplink =
        UplinkIntegrity::new(context.clone(), 3, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    let mut router = router(true);
    router.require_security();
    for (domain, start, count) in [(CnDomain::Cs, 0x12345, 1), (CnDomain::Ps, 0x54321, 2)] {
        let bytes = protected(&context, 3, count, &initial(domain, Some(start)));
        let (forwarded, ()) = uplink
            .receive_with(
                context.origin(),
                &bytes,
                &mut vec![0; bytes.len()],
                |accepted| {
                    router.forward_authenticated_with(accepted, |event| {
                        assert_eq!(event.connection.key.ue, key());
                        assert_eq!(event.connection.key.domain, domain);
                        assert_eq!(event.connection.context.get(), 9);
                        assert_eq!(event.initial.as_ref().unwrap().start, Some(start));
                        assert_eq!(event.nas, [5, 0x24]);
                        Ok::<_, ()>(())
                    })
                },
            )
            .unwrap();
        assert_eq!(forwarded.connection.key.domain, domain);
        assert!(forwarded.created);
    }
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(0x12345),
            ps: Some(0x54321)
        }
    );
    assert!(router.connection(CnDomain::Cs).is_some() && router.connection(CnDomain::Ps).is_some());
    assert_eq!(uplink.last_sequence(), Some(2));
}

#[test]
fn rejected_core_publication_keeps_both_nas_and_integrity_state_retryable() {
    let context = context(key());
    let mut uplink =
        UplinkIntegrity::new(context.clone(), 3, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    let mut router = router(false);
    router.require_security();
    let original = initial(CnDomain::Cs, Some(7));
    assert_eq!(
        router.forward_with(key(), SignallingBearer::NasAm, &original, |_| Ok::<_, ()>(
            ()
        )),
        Err(NasPublishError::Procedure(NasError::SecurityRequired))
    );
    let bytes = protected(&context, 3, 1, &original);
    let mut scratch = vec![0; bytes.len()];
    assert!(
        uplink
            .receive_with(context.origin(), &bytes, &mut scratch, |accepted| router
                .forward_authenticated_with(accepted, |_| Err::<(), _>(7)))
            .is_err()
    );
    assert_eq!(uplink.last_sequence(), Some(0));
    assert_eq!(router.connections(), [None, None]);
    assert_eq!(router.starts(), DomainStarts::default());
    uplink
        .receive_with(context.origin(), &bytes, &mut scratch, |accepted| {
            router.forward_authenticated_with(accepted, |_| Ok::<_, ()>(()))
        })
        .unwrap();
    assert_eq!(uplink.last_sequence(), Some(1));
    assert_eq!(router.starts().cs, Some(7));
}

#[test]
fn protected_downlink_commits_transaction_and_count_together_without_requiring_the_nas_domain_key()
{
    let mut router = router(true);
    let connection = router
        .forward_with(
            key(),
            SignallingBearer::NasAm,
            &initial(CnDomain::Cs, None),
            |_| Ok::<_, ()>(()),
        )
        .unwrap()
        .0
        .connection;
    router.require_security();
    let context = context(key());
    let mut downlink = DownlinkIntegrity::new(context.clone(), 3, FRESH, 0).unwrap();
    let mut rejected = None;
    assert!(
        router
            .downlink_authenticated_with(
                connection,
                DownlinkPriority::Signalling,
                &[5, 0x21],
                &mut downlink,
                |bearer, encoded| {
                    assert_eq!(bearer, SignallingBearer::NasAm);
                    rejected = Some(encoded.clone());
                    Err::<(), _>(7)
                }
            )
            .is_err()
    );
    assert_eq!(router.transaction, 0);
    assert_eq!(downlink.next_count(), Ok(0));
    let encoded = router
        .downlink_authenticated_with(
            connection,
            DownlinkPriority::Signalling,
            &[5, 0x21],
            &mut downlink,
            |_, encoded| Ok::<_, ()>(encoded.clone()),
        )
        .unwrap();
    assert_eq!(Some(&encoded), rejected.as_ref());
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    assert!(decoded.value.integrity_check_info.is_some());
    let DLDCCHMessageType::downlinkDirectTransfer(pdu::DownlinkDirectTransfer::r3(body)) =
        decoded.value.message
    else {
        panic!("not Direct Transfer")
    };
    assert_eq!(
        body.downlink_direct_transfer_r3.cn_domain_identity,
        ie::CNDomainIdentity::cs_domain
    );
    assert_eq!(
        body.downlink_direct_transfer_r3.nas_message.0.as_ref(),
        [5, 0x21]
    );
    assert_eq!(
        body.downlink_direct_transfer_r3
            .rrc_transaction_identifier
            .0,
        0
    );
    assert_eq!(router.transaction, 1);
    assert_eq!(downlink.next_count(), Ok(1));
    let mut fourth = DownlinkIntegrity::new(context, 4, FRESH, 0).unwrap();
    assert_eq!(
        router.downlink_authenticated_with(
            connection,
            DownlinkPriority::Signalling,
            &[5, 0x21],
            &mut fourth,
            |_, _| Ok::<_, ()>(())
        ),
        Err(NasProtectedError::Nas(NasError::Bearer))
    );
    router
        .downlink_authenticated_with(
            connection,
            DownlinkPriority::Sms,
            &[5, 0x21],
            &mut fourth,
            |bearer, _| {
                assert_eq!(bearer, SignallingBearer::LowPriorityNasAm);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert_eq!(fourth.next_count(), Ok(1));
    assert_eq!(downlink.next_count(), Ok(1));
}

#[test]
fn authentication_for_another_ue_or_non_nas_srb_cannot_open_a_core_link() {
    let mut router = router(false);
    router.require_security();
    for (ue, bearer) in [
        (
            UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..key()
            },
            3,
        ),
        (key(), 2),
        (key(), 4),
    ] {
        let context = context(ue);
        let mut uplink = UplinkIntegrity::new(
            context.clone(),
            bearer,
            FRESH,
            Start::new(0).unwrap(),
            Some(0),
        )
        .unwrap();
        let bytes = protected(&context, bearer, 1, &initial(CnDomain::Cs, None));
        assert!(
            uplink
                .receive_with(
                    context.origin(),
                    &bytes,
                    &mut vec![0; bytes.len()],
                    |accepted| router.forward_authenticated_with(accepted, |_| -> Result<(), ()> {
                        panic!("invalid route reached core")
                    })
                )
                .is_err()
        );
        assert_eq!(uplink.last_sequence(), Some(0));
    }
    assert_eq!(router.connections(), [None, None]);
}
