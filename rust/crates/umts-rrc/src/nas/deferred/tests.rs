// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    establishment::DomainStarts,
    nas::{CoreContextId, CoreNasKind, SignallingBearer, inspect_uplink_nas},
};
use std::num::NonZeroU64;
use umts_rrc_codec::{DecodedUplinkDedicated, decode_uplink_dedicated};
use umts_rrc_schema::class_definitions::ULDCCHMessageType;
use umts_types::RadioEpoch;

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 2,
        generation: NonZeroU64::MIN,
    }
}
fn queue(capacity: usize) -> DeferredNas {
    DeferredNas::new(
        key(),
        NonZeroUsize::new(capacity).unwrap(),
        NonZeroUsize::new(32).unwrap(),
    )
    .unwrap()
}
fn router() -> NasRouter {
    NasRouter::new(
        key(),
        CoreContextId::new(7).unwrap(),
        DomainStarts { cs: None, ps: None },
        false,
    )
}
fn direct(domain: CnDomain, bytes: &[u8]) -> DecodedUplinkDedicated {
    let mut message = decode_uplink_dedicated(&[0x6c, 0x80, 0x08, 0x42, 0x08], 37).unwrap();
    let ULDCCHMessageType::uplinkDirectTransfer(body) = &mut message.value.message else {
        panic!("direct")
    };
    body.cn_domain_identity = super::super::wire_domain(domain);
    body.nas_message.0 = bytes.to_vec().into();
    message
}
fn hold(queue: &mut DeferredNas, message: &DecodedUplinkDedicated) -> Result<Hold, Error> {
    queue.hold(
        key(),
        inspect_uplink_nas(SignallingBearer::NasAm, message, false).unwrap(),
    )
}

#[test]
fn identification_gap_preserves_full_queue_order_and_publication_retry() {
    let mut queue = queue(2);
    let initial = crate::nas::tests::initial(CnDomain::Cs, Some(42));
    let before = direct(CnDomain::Cs, &[5, 0x31]);
    let after = direct(CnDomain::Cs, &[5, 0x94]);
    hold(&mut queue, &initial).unwrap();
    hold(&mut queue, &before).unwrap();
    let null = crate::nas::identification::interworking_null(1).unwrap();
    assert_eq!(null, [5, 0x70]);
    queue.hold_interworking_null(key(), &null).unwrap();
    assert_eq!(queue.len(), 3);
    assert_eq!(hold(&mut queue, &after), Err(Error::Capacity));
    let mut router = router();
    queue
        .forward_one_with(&mut router, |event| {
            assert_eq!(event.kind, CoreNasKind::Initial);
            Ok::<_, ()>(())
        })
        .unwrap();
    hold(&mut queue, &after).unwrap();
    queue
        .forward_one_with(&mut router, |event| {
            assert_eq!(event.nas, &[5, 0x31]);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        queue.forward_one_with(&mut router, |event| {
            assert_eq!(event.nas, &null);
            assert_eq!(event.kind, CoreNasKind::Direct);
            Err::<(), _>(7)
        }),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(queue.len(), 2);
    for expected in [&null[..], &[5, 0x94][..]] {
        queue
            .forward_one_with(&mut router, |event| {
                assert_eq!(event.connection.key.domain, CnDomain::Cs);
                assert_eq!(event.nas, expected);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    assert!(queue.is_empty());
    queue.hold_interworking_null(key(), &null).unwrap();
    queue.retire(key()).unwrap();
    assert!(queue.is_empty());
}

#[test]
fn mixed_initial_and_direct_fifo_survives_full_queue_retries_and_wrap() {
    let mut queue = queue(3);
    let cs = crate::nas::tests::initial(CnDomain::Cs, Some(42));
    let ps = crate::nas::tests::initial(CnDomain::Ps, Some(17));
    let direct = direct(CnDomain::Cs, &[5, 0x31]);
    assert_eq!(queue.retained_payload_bytes(), 96);
    assert_eq!(hold(&mut queue, &cs), Ok(Hold::Stored));
    assert_eq!(hold(&mut queue, &ps), Ok(Hold::Stored));
    assert_eq!(hold(&mut queue, &direct), Ok(Hold::Stored));
    assert_eq!(hold(&mut queue, &cs), Ok(Hold::Duplicate));
    assert_eq!(hold(&mut queue, &direct), Err(Error::Capacity));
    let mut router = router();
    assert_eq!(
        queue.forward_one_with(&mut router, |_| Err::<(), _>(7u8)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(queue.len(), 3);
    assert_eq!(router.connections(), [None; 2]);
    for (domain, kind) in [
        (CnDomain::Cs, CoreNasKind::Initial),
        (CnDomain::Ps, CoreNasKind::Initial),
        (CnDomain::Cs, CoreNasKind::Direct),
    ] {
        assert!(
            queue
                .forward_one_with(&mut router, |event| {
                    assert_eq!(event.connection.key.domain, domain);
                    assert_eq!(event.kind, kind);
                    Ok::<_, ()>(())
                })
                .unwrap()
                .is_some()
        );
    }
    for _ in 0..12 {
        assert_eq!(hold(&mut queue, &direct), Ok(Hold::Stored));
        assert!(
            queue
                .forward_one_with(&mut router, |event| {
                    assert_eq!(event.nas, &[5, 0x31]);
                    Ok::<_, ()>(())
                })
                .unwrap()
                .is_some()
        );
        assert!(queue.is_empty());
    }
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(42),
            ps: Some(17)
        }
    );
}

#[test]
fn repeated_direct_messages_are_not_deduplicated_and_retirement_erases_storage() {
    let mut queue = queue(3);
    let direct = direct(CnDomain::Cs, &[5, 0x31]);
    for _ in 0..3 {
        assert_eq!(hold(&mut queue, &direct), Ok(Hold::Stored))
    }
    assert_eq!(queue.len(), 3);
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(queue.retire(stale), Err(Error::Origin));
    assert_eq!(queue.len(), 3);
    queue.retire(key()).unwrap();
    assert!(
        queue
            .entries
            .iter()
            .all(|entry| entry.bytes.iter().all(|byte| *byte == 0) && entry.initial.is_none())
    );
    assert_eq!(hold(&mut queue, &direct), Err(Error::Retired));
    assert_eq!(
        queue.forward_one_with(&mut router(), |_| Ok::<_, ()>(())),
        Err(PublishError::Owner(Error::Retired))
    );
}

#[test]
fn invalid_origin_and_oversize_inputs_never_mutate_retained_messages() {
    let mut queue = queue(2);
    let cs = crate::nas::tests::initial(CnDomain::Cs, None);
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(
        queue.hold(
            stale,
            inspect_uplink_nas(SignallingBearer::NasAm, &cs, false).unwrap()
        ),
        Err(Error::Origin)
    );
    assert_eq!(
        hold(&mut queue, &direct(CnDomain::Cs, &[1; 33])),
        Err(Error::Capacity)
    );
    assert!(queue.is_empty());
    assert_eq!(hold(&mut queue, &cs), Ok(Hold::Stored));
    let mut router = router();
    router.require_security();
    assert_eq!(
        queue.forward_one_with(&mut router, |_| Ok::<_, ()>(())),
        Err(PublishError::Nas(super::super::NasError::SecurityRequired))
    );
    assert_eq!(queue.len(), 1);
}
