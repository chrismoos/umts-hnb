// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{establishment::TransactionId, integrity::StreamError, nas::build_downlink_nas};
use std::num::NonZeroU64;
use umts_rrc_codec::decode_downlink_dedicated;
use umts_security::{
    Key,
    contexts::{CipherMaterial, KeySet},
};
use umts_types::RadioEpoch;

fn ue() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 3,
        generation: NonZeroU64::MIN,
    }
}
fn context(domain: CnDomain, generation: u64) -> IntegrityContext {
    let origin = KeyOrigin {
        ue: ue(),
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
fn seeds() -> [IntegritySeed; SRB_COUNT] {
    [IntegritySeed {
        downlink_first_count: 0x12345000,
        uplink_start: Start::new(0x12345).unwrap(),
        uplink_previous_sequence: Some(15),
    }; SRB_COUNT]
}

#[test]
fn old_new_and_other_domain_have_independent_bearers_and_bounded_retention() {
    let old = context(CnDomain::Cs, 1);
    let old_origin = old.origin();
    let new = context(CnDomain::Cs, 2);
    let new_origin = new.origin();
    let ps = context(CnDomain::Ps, 1);
    let ps_origin = ps.origin();
    let mut epochs = IntegrityEpochs::new(ue(), 0xabcdef12);
    assert!(epochs.is_empty());
    epochs.insert(old, seeds()).unwrap();
    epochs.insert(new, seeds()).unwrap();
    epochs.insert(ps, seeds()).unwrap();
    assert_eq!(epochs.len(), 3);
    assert_eq!(
        epochs.insert(context(CnDomain::Ps, 2), seeds()),
        Err(EpochError::Capacity)
    );
    let encoded =
        build_downlink_nas(TransactionId::new(0).unwrap(), CnDomain::Cs, &[5, 0x24]).unwrap();
    let message = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
        .unwrap()
        .value;
    let stream = epochs.downlink(new_origin, 2).unwrap();
    assert_eq!(
        stream.publish_with(new_origin, &message, |_| Err::<(), _>(7)),
        Err(StreamError::Consumer(7))
    );
    assert_eq!(stream.next_count(), Ok(0x12345000));
    stream
        .publish_with(new_origin, &message, |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        epochs.downlink(new_origin, 2).unwrap().next_count(),
        Ok(0x12345001)
    );
    for origin in [old_origin, ps_origin] {
        for bearer in 0..=MAX_SRB {
            assert_eq!(
                epochs.downlink(origin, bearer).unwrap().next_count(),
                Ok(0x12345000)
            );
            assert_eq!(
                epochs.uplink(origin, bearer).unwrap().last_sequence(),
                Some(15)
            );
        }
    }
    assert_eq!(
        epochs.downlink(new_origin, 3).unwrap().next_count(),
        Ok(0x12345000)
    );
    epochs.retire(new_origin).unwrap();
    assert_eq!(epochs.len(), 2);
    assert_eq!(
        epochs.insert(context(CnDomain::Cs, 2), seeds()),
        Err(EpochError::ReusedGeneration)
    );
    epochs.insert(context(CnDomain::Ps, 2), seeds()).unwrap();
    assert_eq!(epochs.len(), 3);
    assert!(matches!(
        epochs.downlink(new_origin, 2),
        Err(EpochError::Unknown)
    ));
    assert_eq!(
        epochs.downlink(old_origin, 2).unwrap().next_count(),
        Ok(0x12345000)
    );
}

#[test]
fn malformed_seed_and_wrong_owner_leave_inventory_and_generation_unchanged() {
    let mut epochs = IntegrityEpochs::new(ue(), 0);
    let context = context(CnDomain::Cs, 1);
    let origin = context.origin();
    let mut malformed = seeds();
    malformed[4].uplink_previous_sequence = Some(16);
    assert_eq!(
        epochs.insert(context.clone(), malformed),
        Err(EpochError::Stream(Error::Algorithm(
            umts_security::Error::Count
        )))
    );
    assert!(epochs.is_empty());
    epochs.insert(context, seeds()).unwrap();
    let wrong = KeyOrigin {
        ue: UeKey {
            generation: NonZeroU64::new(2).unwrap(),
            ..ue()
        },
        ..origin
    };
    assert!(matches!(epochs.downlink(wrong, 2), Err(EpochError::Owner)));
    assert!(matches!(epochs.uplink(origin, 5), Err(EpochError::Bearer)));
    assert_eq!(epochs.retire(wrong), Err(EpochError::Owner));
    assert_eq!(epochs.len(), 1);
}
