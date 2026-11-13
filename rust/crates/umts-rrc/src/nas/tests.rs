// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_rrc_codec::{decode_downlink_dedicated, decode_uplink_dedicated};
use umts_types::RadioEpoch;

// TS 25.331 UPER: no integrity, initial choice 5, GSM IDNNS IMSI routing, NAS length 2.
const INITIAL: [u8; 7] = [0x14, 0x10, 0x00, 0x00, 0x08, 0x29, 0x20];
// No integrity, UL choice 27, absent optionals, PS domain, NAS length 2.
const DIRECT: [u8; 5] = [0x6c, 0x80, 0x08, 0x42, 0x08];

#[test]
fn independent_v770_v860_bitmap_vector_retains_csg_and_original_nas() {
    // Extension presence and inner optional bits follow the basic 53-bit transfer.
    let mut bytes = [0x15, 0x10, 0, 0, 8, 0x29, 0x24, 0xd1, 0x20];
    for supported in [true, false] {
        bytes[8] = if supported { 0x20 } else { 0 };
        let message = decode_uplink_dedicated(&bytes, 67).unwrap();
        assert_eq!(message.consumed_bits, 67);
        let inspected = inspect_uplink_nas(SignallingBearer::NasAm, &message, false).unwrap();
        assert_eq!(inspected.initial().unwrap().supports_csg, supported);
        assert_eq!(inspected.nas(), &[5, 0x24]);
        assert_eq!(inspected.domain(), CnDomain::Cs);
    }
    assert!(decode_uplink_dedicated(&bytes, 66).is_err());
    assert!(
        !inspect_uplink_nas(SignallingBearer::NasAm, &initial(CnDomain::Ps, None), false)
            .unwrap()
            .initial()
            .unwrap()
            .supports_csg
    );
}

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 7,
        generation: NonZeroU64::MIN,
    }
}
fn context(n: u32) -> CoreContextId {
    CoreContextId::new(n).unwrap()
}
pub(super) fn initial(cn: CnDomain, start: Option<u32>) -> DecodedUplinkDedicated {
    let mut message = decode_uplink_dedicated(&INITIAL, 53).unwrap();
    let ULDCCHMessageType::initialDirectTransfer(body) = &mut message.value.message else {
        panic!("initial transfer");
    };
    assert_eq!(body.nas_message.0.as_ref(), &[0x05, 0x24]);
    body.cn_domain_identity = wire_domain(cn);
    if let Some(value) = start {
        let mut bits = rasn::types::FixedBitString::<START_BITS>::default();
        for bit in 0..START_BITS {
            bits.set(bit, value & (1 << (START_BITS - bit - 1)) != 0);
        }
        body.v3a0_non_critical_extensions =
            Some(pdu::InitialDirectTransferV3a0NonCriticalExtensions::new(
                pdu::InitialDirectTransferV3a0ext::new(Some(ie::STARTValue(bits))),
                None,
            ));
    }
    message
}
fn router(fourth: bool) -> NasRouter {
    NasRouter::new(
        key(),
        context(9),
        DomainStarts {
            cs: Some(1),
            ps: Some(2),
        },
        fourth,
    )
}
fn open(router: &mut NasRouter, domain: CnDomain) -> CoreConnection {
    router
        .forward_with(
            key(),
            SignallingBearer::NasAm,
            &initial(domain, None),
            |_| Ok::<_, ()>(()),
        )
        .unwrap()
        .0
        .connection
}

#[test]
fn signalling_release_matches_independent_wire_and_closes_only_after_publication() {
    for (domain, bytes) in [(CnDomain::Cs, [0x44, 0x80]), (CnDomain::Ps, [0x44, 0xa0])] {
        let message = signalling_connection_release_message(TransactionId::new(2).unwrap(), domain);
        let encoded = encode_downlink_dedicated(&message).unwrap();
        assert_eq!(encoded.bytes, bytes);
        assert_eq!(encoded.bit_length, 11);
    }
    let mut router = NasRouter::new(
        key(),
        context(9),
        DomainStarts {
            cs: Some(7),
            ps: Some(11),
        },
        false,
    );
    let cs = open(&mut router, CnDomain::Cs);
    let ps = open(&mut router, CnDomain::Ps);
    let mut schedule =
        crate::integrity::IntegritySchedule::new(key(), 0, crate::security::SignallingSet::Base);
    let before = router.transaction;
    assert!(
        router
            .release_scheduled_with(cs, &mut schedule, |_, _| Err::<(), _>(7))
            .is_err()
    );
    assert_eq!(router.connection(CnDomain::Cs), Some(cs));
    assert_eq!(router.transaction, before);
    router
        .release_scheduled_with(cs, &mut schedule, |bearer, encoded| {
            assert_eq!(bearer, SignallingBearer::ControlAm);
            let decoded =
                umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                    .unwrap();
            assert!(matches!(
                decoded.value.message,
                DLDCCHMessageType::signallingConnectionRelease(_)
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(router.connection(CnDomain::Cs), None);
    assert_eq!(router.connection(CnDomain::Ps), Some(ps));
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(7),
            ps: Some(11)
        }
    );
    assert!(
        router
            .release_scheduled_with(cs, &mut schedule, |_, _| -> Result<(), ()> {
                panic!("already closed domain re-notified")
            })
            .is_err()
    );
    open(&mut router, CnDomain::Cs);
    assert_eq!(router.connection(CnDomain::Ps), Some(ps));
}

#[test]
fn independent_direct_transfer_vectors_preserve_payload_domain_and_transaction() {
    let direct = decode_uplink_dedicated(&DIRECT, 37).unwrap();
    let ULDCCHMessageType::uplinkDirectTransfer(body) = direct.value.message else {
        panic!("direct transfer");
    };
    assert_eq!(body.cn_domain_identity, ie::CNDomainIdentity::ps_domain);
    assert_eq!(body.nas_message.0.as_ref(), &[0x08, 0x41]);
    let encoded =
        build_downlink_nas(TransactionId::new(2).unwrap(), CnDomain::Ps, &[0x08, 0x42]).unwrap();
    // No integrity, DL choice 5, r3, absent extension, transaction 2, PS, 12-bit constrained NAS length.
    assert_eq!(encoded.bit_length, 39);
    assert_eq!(encoded.bytes, [0x14, 0xa0, 0x02, 0x10, 0x84]);
}

#[test]
fn initial_publication_is_atomic_and_domains_keep_independent_starts() {
    let mut router = router(false);
    let ps = initial(CnDomain::Ps, Some(0x54321));
    assert_eq!(
        router.forward_with(key(), SignallingBearer::NasAm, &ps, |_| Err::<(), _>(
            "queue"
        )),
        Err(NasPublishError::Downstream("queue"))
    );
    assert_eq!(router.connections(), [None, None]);
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(1),
            ps: Some(2)
        }
    );
    let (forwarded, ()) = router
        .forward_with(key(), SignallingBearer::NasAm, &ps, |event| {
            assert_eq!(event.kind, CoreNasKind::Initial);
            assert_eq!(event.nas, &[0x05, 0x24]);
            assert_eq!(event.initial.as_ref().unwrap().start, Some(0x54321));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(forwarded.created);
    assert_eq!(forwarded.connection.key.domain, CnDomain::Ps);
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(1),
            ps: Some(0x54321)
        }
    );
    let cs = open(&mut router, CnDomain::Cs);
    assert_ne!(Some(cs), router.connection(CnDomain::Ps));
    let (repeated, ()) = router
        .forward_with(key(), SignallingBearer::NasAm, &ps, |event| {
            assert_eq!(event.kind, CoreNasKind::Direct);
            assert!(event.initial.is_some());
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(!repeated.created);
    assert_eq!(repeated.connection, forwarded.connection);
}

#[test]
fn no_nas_deduplication_and_no_direct_transfer_before_domain_exists() {
    let mut router = router(false);
    let message = decode_uplink_dedicated(&DIRECT, 37).unwrap();
    assert_eq!(
        router.forward_with(
            key(),
            SignallingBearer::NasAm,
            &message,
            |_| Ok::<_, ()>(())
        ),
        Err(NasPublishError::Procedure(NasError::Domain))
    );
    let ps = open(&mut router, CnDomain::Ps);
    let mut count = 0;
    for _ in 0..2 {
        router
            .forward_with(key(), SignallingBearer::NasAm, &message, |event| {
                assert_eq!(event.connection, ps);
                assert_eq!(event.kind, CoreNasKind::Direct);
                assert!(event.initial.is_none());
                count += 1;
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    assert_eq!(count, 2);
}

#[test]
fn stale_generation_bearer_and_security_are_rejected_while_domains_share_context() {
    let mut router = router(false);
    let cs = initial(CnDomain::Cs, None);
    for bearer in [
        SignallingBearer::Unacknowledged,
        SignallingBearer::ControlAm,
        SignallingBearer::LowPriorityNasAm,
    ] {
        assert_eq!(
            router.forward_with(key(), bearer, &cs, |_| Ok::<_, ()>(())),
            Err(NasPublishError::Procedure(NasError::Bearer))
        );
    }
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(
        router.forward_with(stale, SignallingBearer::NasAm, &cs, |_| Ok::<_, ()>(())),
        Err(NasPublishError::Procedure(NasError::Origin))
    );
    let link = open(&mut router, CnDomain::Cs);
    let ps = open(&mut router, CnDomain::Ps);
    assert_eq!(ps.context, link.context);
    assert_ne!(ps.key.domain, link.key.domain);
    let wrong = CoreConnection {
        context: context(2),
        ..link
    };
    assert_eq!(
        router.downlink_with(
            wrong,
            DownlinkPriority::Signalling,
            &[1],
            |_, _| Ok::<_, ()>(())
        ),
        Err(NasPublishError::Procedure(NasError::Context))
    );
    router.require_security();
    assert_eq!(
        router.forward_with(key(), SignallingBearer::NasAm, &cs, |_| Ok::<_, ()>(())),
        Err(NasPublishError::Procedure(NasError::SecurityRequired))
    );
    assert_eq!(
        router.downlink_with(
            link,
            DownlinkPriority::Signalling,
            &[1],
            |_, _| Ok::<_, ()>(())
        ),
        Err(NasPublishError::Procedure(NasError::SecurityRequired))
    );
}

#[test]
fn downlink_priority_fallback_and_transaction_rollback_follow_queue_admission() {
    for fourth in [false, true] {
        let mut router = router(fourth);
        let link = open(&mut router, CnDomain::Ps);
        let mut attempted = None;
        assert_eq!(
            router.downlink_with(link, DownlinkPriority::Sms, &[0x09, 0x01], |_, message| {
                attempted = Some(message.clone());
                Err::<(), _>("queue")
            }),
            Err(NasPublishError::Downstream("queue"))
        );
        for expected in [0, 1, 2, 3, 0] {
            router
                .downlink_with(
                    link,
                    DownlinkPriority::Sms,
                    &[0x09, 0x01],
                    |bearer, message| {
                        assert_eq!(
                            bearer,
                            if fourth {
                                SignallingBearer::LowPriorityNasAm
                            } else {
                                SignallingBearer::NasAm
                            }
                        );
                        if expected == 0 {
                            assert_eq!(message, attempted.as_ref().unwrap());
                        }
                        let decoded =
                            decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
                        let DLDCCHMessageType::downlinkDirectTransfer(
                            pdu::DownlinkDirectTransfer::r3(body),
                        ) = decoded.value.message
                        else {
                            panic!("downlink direct");
                        };
                        assert_eq!(
                            body.downlink_direct_transfer_r3
                                .rrc_transaction_identifier
                                .0,
                            expected
                        );
                        assert_eq!(
                            body.downlink_direct_transfer_r3.nas_message.0.as_ref(),
                            &[0x09, 0x01]
                        );
                        Ok::<_, ()>(())
                    },
                )
                .unwrap();
        }
    }
    assert_eq!(
        DownlinkPriority::from_sapi(0),
        Ok(DownlinkPriority::Signalling)
    );
    assert_eq!(DownlinkPriority::from_sapi(3), Ok(DownlinkPriority::Sms));
    assert_eq!(DownlinkPriority::from_sapi(1), Err(NasError::Priority));
}

#[test]
fn domain_retirement_is_atomic_and_nas_lengths_and_context_width_are_checked() {
    let mut router = router(false);
    open(&mut router, CnDomain::Cs);
    open(&mut router, CnDomain::Ps);
    let before = router.connections();
    assert_eq!(router.retire_with(|_| Err::<(), _>("queue")), Err("queue"));
    assert_eq!(router.connections(), before);
    router
        .retire_with(|connections| {
            assert_eq!(connections, before);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(router.connections(), [None, None]);
    assert_eq!(
        CoreContextId::new(1 << CONTEXT_BITS),
        Err(NasError::Context)
    );
    assert_eq!(context(0).get(), 0);
    assert!(matches!(
        build_downlink_nas(TransactionId::new(0).unwrap(), CnDomain::Cs, &[]),
        Err(NasError::Length)
    ));
    assert!(matches!(
        build_downlink_nas(
            TransactionId::new(0).unwrap(),
            CnDomain::Cs,
            &vec![0; MAX_NAS_BYTES + 1]
        ),
        Err(NasError::Length)
    ));
}
