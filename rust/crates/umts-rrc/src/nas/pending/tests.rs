// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    establishment::DomainStarts,
    nas::{CoreContextId, CoreNasKind, SignallingBearer, inspect_uplink_nas},
};
use std::num::NonZeroU64;
use umts_types::RadioEpoch;
fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 1,
        generation: NonZeroU64::MIN,
    }
}
fn pending(maximum: usize) -> PendingInitials {
    PendingInitials::new(key(), NonZeroUsize::new(maximum).unwrap()).unwrap()
}
fn message(domain: CnDomain, start: u32, nas: &[u8]) -> umts_rrc_codec::DecodedUplinkDedicated {
    let mut value = crate::nas::tests::initial(domain, Some(start));
    let umts_rrc_schema::class_definitions::ULDCCHMessageType::initialDirectTransfer(body) =
        &mut value.value.message
    else {
        panic!("initial direct transfer");
    };
    body.nas_message.0 = nas.to_vec().into();
    value
}
fn router(key: UeKey) -> NasRouter {
    NasRouter::new(
        key,
        CoreContextId::new(9).unwrap(),
        DomainStarts { cs: None, ps: None },
        false,
    )
}

#[test]
fn both_initials_survive_identification_and_preserve_arrival_order_until_publication() {
    let mut pending = pending(MAX_NAS_BYTES);
    let ps = message(CnDomain::Ps, 42, &[8, 1]);
    let cs = message(CnDomain::Cs, 17, &[5, 8]);
    pending
        .hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &ps, false).unwrap(),
        )
        .unwrap();
    pending
        .hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &cs, false).unwrap(),
        )
        .unwrap();
    drop(ps);
    drop(cs);
    assert_eq!(pending.len(), 2);
    assert_eq!(pending.retained_payload_bytes(), 2 * MAX_NAS_BYTES);
    let mut router = router(key());
    assert_eq!(
        pending.forward_one_with(&mut router, |_| Err::<(), _>(7u8)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(pending.len(), 2);
    assert!(router.connections().iter().all(Option::is_none));
    for (domain, bytes, start) in [
        (CnDomain::Ps, &[8, 1][..], 42),
        (CnDomain::Cs, &[5, 8][..], 17),
    ] {
        let result = pending
            .forward_one_with(&mut router, |event| {
                assert_eq!(event.connection.key.ue, key());
                assert_eq!(event.connection.key.domain, domain);
                assert_eq!(event.connection.context.get(), 9);
                assert_eq!(event.kind, CoreNasKind::Initial);
                assert_eq!(event.nas, bytes);
                assert_eq!(event.initial.unwrap().start, Some(start));
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert!(result.0.created);
    }
    assert!(pending.is_empty());
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(17),
            ps: Some(42)
        }
    );
    assert_eq!(
        pending
            .forward_one_with(&mut router, |_| Ok::<_, ()>(()))
            .unwrap(),
        None
    );
}

#[test]
fn conflicts_require_explicit_replacement_without_overwriting_the_peer_domain() {
    let mut pending = pending(32);
    let first = message(CnDomain::Cs, 1, &[5, 8]);
    let changed = message(CnDomain::Cs, 2, &[5, 9]);
    let peer = message(CnDomain::Ps, 3, &[8, 1]);
    assert_eq!(
        pending
            .hold(
                key(),
                inspect_uplink_nas(SignallingBearer::NasAm, &first, false).unwrap()
            )
            .unwrap(),
        Hold::Stored
    );
    assert_eq!(
        pending
            .hold(
                key(),
                inspect_uplink_nas(SignallingBearer::NasAm, &first, false).unwrap()
            )
            .unwrap(),
        Hold::Duplicate
    );
    pending
        .hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &peer, false).unwrap(),
        )
        .unwrap();
    assert_eq!(
        pending.hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &changed, false).unwrap()
        ),
        Err(Error::ConflictingInitial)
    );
    assert_eq!(
        pending
            .replace_initial(
                key(),
                inspect_uplink_nas(SignallingBearer::NasAm, &changed, false).unwrap()
            )
            .unwrap(),
        Hold::Stored
    );
    let mut router = router(key());
    pending
        .forward_one_with(&mut router, |event| {
            assert_eq!(event.connection.key.domain, CnDomain::Ps);
            assert_eq!(event.nas, &[8, 1]);
            Ok::<_, ()>(())
        })
        .unwrap();
    pending
        .forward_one_with(&mut router, |event| {
            assert_eq!(event.nas, &[5, 9]);
            assert_eq!(event.initial.unwrap().start, Some(2));
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn capacity_generation_and_sequence_failures_precede_mutation() {
    let mut pending = pending(2);
    let short = message(CnDomain::Cs, 1, &[5, 8]);
    let long = message(CnDomain::Ps, 2, &[8, 1, 0]);
    assert_eq!(
        pending.hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &long, false).unwrap()
        ),
        Err(Error::Capacity)
    );
    let stale = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(
        pending.hold(
            stale,
            inspect_uplink_nas(SignallingBearer::NasAm, &short, false).unwrap()
        ),
        Err(Error::Origin)
    );
    pending.next_sequence = u64::MAX;
    assert_eq!(
        pending.hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &short, false).unwrap()
        ),
        Err(Error::SequenceExhausted)
    );
    assert!(pending.is_empty());
    assert_eq!(pending.next_sequence, u64::MAX);
}

#[test]
fn wrong_router_security_gate_and_retirement_cannot_lose_pending_initials() {
    let mut pending = pending(16);
    let initial = message(CnDomain::Cs, 1, &[5, 8]);
    pending
        .hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &initial, false).unwrap(),
        )
        .unwrap();
    let mut wrong = router(UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    });
    assert_eq!(
        pending.forward_one_with(&mut wrong, |_| Ok::<_, ()>(())),
        Err(PublishError::Nas(NasError::Origin))
    );
    let mut secured = router(key());
    secured.require_security();
    assert_eq!(
        pending.forward_one_with(&mut secured, |_| Ok::<_, ()>(())),
        Err(PublishError::Nas(NasError::SecurityRequired))
    );
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.retire(wrong.key), Err(Error::Origin));
    assert_eq!(pending.len(), 1);
    pending.retire(key()).unwrap();
    assert!(pending.is_empty());
    assert_eq!(
        pending.hold(
            key(),
            inspect_uplink_nas(SignallingBearer::NasAm, &initial, false).unwrap()
        ),
        Err(Error::Retired)
    );
    assert_eq!(
        pending.forward_one_with(&mut router(key()), |_| Ok::<_, ()>(())),
        Err(PublishError::Owner(Error::Retired))
    );
    assert!(
        pending
            .slots
            .iter()
            .all(|slot| slot.bytes.iter().all(|byte| *byte == 0))
    );
}

#[test]
fn inspector_preserves_opaque_nas_without_allocating_a_core_identity() {
    let initial = message(CnDomain::Ps, 5, &[8, 1, 0xaa, 0xbb]);
    let inspected = inspect_uplink_nas(SignallingBearer::NasAm, &initial, false).unwrap();
    assert_eq!(inspected.domain(), CnDomain::Ps);
    assert_eq!(inspected.nas(), &[8, 1, 0xaa, 0xbb]);
    assert_eq!(inspected.initial().unwrap().start, Some(5));
    assert!(matches!(
        inspect_uplink_nas(SignallingBearer::ControlAm, &initial, false),
        Err(NasError::Bearer)
    ));
}
