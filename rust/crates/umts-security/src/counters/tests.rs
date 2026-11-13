// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

#[test]
fn counter_initialization_preserves_new_old_key_policy_and_reinitialized_wrap() {
    let start = Start::new(0x12345).unwrap();
    for (initialization, base) in [
        (CounterInitialization::NewKeys, 0),
        (CounterInitialization::ReusedKeys(start), 0x12345000),
    ] {
        for next in 0..16 {
            for previous in 0..16 {
                assert_eq!(
                    initialization.integrity_first_count(next, Some(previous)),
                    Ok(base + u32::from(next) + if next < previous { 16 } else { 0 })
                );
            }
            assert_eq!(
                initialization.integrity_first_count(next, None),
                Ok(base | u32::from(next))
            );
        }
        assert_eq!(
            initialization.integrity_first_count(16, None),
            Err(Error::Count)
        );
        assert_eq!(
            initialization.integrity_first_count(0, Some(16)),
            Err(Error::Count)
        );
    }
}
fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 0,
        generation: NonZeroU64::MIN,
    }
}

#[test]
fn recovered_receive_floor_preserves_exact_count_wrap_and_rejected_admission() {
    for count in [0, 1, 15, 16, 31, 32, u32::MAX] {
        let mut receiver = IntegrityReceiver::from_next_count(key(), Direction::Uplink, count);
        assert_eq!(receiver.last_count(), count.checked_sub(1));
        let sequence = (count & 15) as u8;
        assert_eq!(
            receiver.receive_with(key(), Direction::Uplink, sequence, |_| Err::<(), _>(7)),
            Err(ReceiveError::Rejected(7))
        );
        assert_eq!(receiver.last_count(), count.checked_sub(1));
        receiver
            .receive_with(key(), Direction::Uplink, sequence, |actual| {
                assert_eq!(actual, count);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(receiver.last_count(), Some(count));
        assert_eq!(
            receiver.receive_with(key(), Direction::Uplink, sequence, |_| Ok::<_, ()>(())),
            Err(ReceiveError::Duplicate)
        );
    }
}

#[test]
fn first_receive_floor_accepts_any_first_sn_at_or_after_the_floor_without_speculative_counts() {
    for floor in [0, 1, 16, 31, 32, u32::MAX - 15, u32::MAX] {
        for sequence in 0_u8..16 {
            let mut receiver = IntegrityReceiver::from_next_count(key(), Direction::Uplink, floor);
            let expected = floor.checked_add(u32::from(sequence).wrapping_sub(floor) & 15);
            let actual = receiver.receive_with(key(), Direction::Uplink, sequence, |count| {
                Ok::<_, ()>(count)
            });
            match expected {
                Some(count) => assert_eq!(actual, Ok(count)),
                None => assert_eq!(actual, Err(ReceiveError::Counter(Error::Exhausted))),
            }
        }
    }
}

#[test]
fn all_count_layouts_put_start_in_the_twenty_most_significant_bits() {
    let start = Start::new(0x54321).unwrap();
    assert_eq!(start.initial_count(), 0x54321000);
    for (mode, hfn) in [
        (CipherMode::Acknowledged, 0x54321),
        (CipherMode::Unacknowledged, 0xa86420),
        (CipherMode::Transparent, 0x543210),
    ] {
        assert_eq!(mode.compose(hfn, 0).unwrap(), start.initial_count());
        assert_eq!(mode.compose(hfn, 17).unwrap(), start.initial_count() | 17);
        assert_eq!(
            mode.compose(1 << (32 - mode.sequence_bits()), 0),
            Err(Error::Count)
        );
        assert_eq!(
            mode.compose(hfn, 1 << mode.sequence_bits()),
            Err(Error::Count)
        );
    }
    assert_eq!(Start::new(1 << 20), Err(Error::Count));
}

#[test]
fn integrity_counter_advances_only_after_publication_and_preserves_generation() {
    let mut counter =
        IntegrityCounter::new(key(), Direction::Downlink, Start::new(0x54321).unwrap());
    assert_eq!(
        counter.publish_with(key(), Direction::Downlink, |_| Err::<(), _>(7)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(counter.next(), Ok(0x54321000));
    for sequence in 0..33 {
        assert_eq!(counter.sequence(), Ok((sequence % 16) as u8));
        counter
            .publish_with(key(), Direction::Downlink, |count| {
                assert_eq!(count, 0x54321000 + sequence);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    assert_eq!(
        counter.publish_with(
            UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..key()
            },
            Direction::Downlink,
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Counter(Error::Origin))
    );
    assert_eq!(
        counter.publish_with(key(), Direction::Uplink, |_| Ok::<_, ()>(())),
        Err(PublishError::Counter(Error::Origin))
    );
}

#[test]
fn exhausted_integrity_count_never_wraps_to_a_reused_nonce() {
    let mut counter = IntegrityCounter {
        key: key(),
        direction: Direction::Downlink,
        next: Some(u32::MAX),
    };
    counter
        .publish_with(key(), Direction::Downlink, |count| {
            assert_eq!(count, u32::MAX);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(counter.next(), Err(Error::Exhausted));
    assert_eq!(
        counter.publish_with(key(), Direction::Downlink, |_| Ok::<_, ()>(())),
        Err(PublishError::Counter(Error::Exhausted))
    );
}

#[test]
fn receive_count_commits_only_authenticated_and_accepted_messages() {
    let mut receiver = IntegrityReceiver::new(
        key(),
        Direction::Uplink,
        Start::new(0x54321).unwrap(),
        Some(14),
    )
    .unwrap();
    for (sequence, expected) in [(15, 0x5432100f), (0, 0x54321010), (3, 0x54321013)] {
        let previous = receiver.last_sequence();
        assert_eq!(
            receiver.receive_with(key(), Direction::Uplink, sequence, |count| {
                assert_eq!(count, expected);
                Err::<(), _>("authentication or queue failure")
            }),
            Err(ReceiveError::Rejected("authentication or queue failure"))
        );
        assert_eq!(receiver.last_sequence(), previous);
        assert_eq!(
            receiver.receive_with(key(), Direction::Uplink, sequence, Ok::<_, ()>),
            Ok(expected)
        );
        assert_eq!(receiver.last_sequence(), Some(sequence));
        assert_eq!(
            receiver.receive_with(key(), Direction::Uplink, sequence, |_| -> Result<(), ()> {
                panic!("duplicate reached authentication")
            }),
            Err(ReceiveError::Duplicate)
        );
    }
}

#[test]
fn receive_generation_direction_range_and_exhaustion_fail_without_callback() {
    let mut receiver =
        IntegrityReceiver::new(key(), Direction::Uplink, Start::new(0).unwrap(), None).unwrap();
    let reject = |_| -> Result<(), ()> { panic!("invalid admission reached authentication") };
    assert_eq!(
        receiver.receive_with(key(), Direction::Downlink, 0, reject),
        Err(ReceiveError::Counter(Error::Origin))
    );
    assert_eq!(
        receiver.receive_with(
            UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..key()
            },
            Direction::Uplink,
            0,
            reject
        ),
        Err(ReceiveError::Counter(Error::Origin))
    );
    assert_eq!(
        receiver.receive_with(key(), Direction::Uplink, 16, reject),
        Err(ReceiveError::Counter(Error::Count))
    );
    receiver.hfn = (1 << (u32::BITS - INTEGRITY_SN_BITS)) - 1;
    receiver.last_sequence = Some(15);
    assert_eq!(
        receiver.receive_with(key(), Direction::Uplink, 0, reject),
        Err(ReceiveError::Counter(Error::Exhausted))
    );
    assert_eq!(receiver.last_sequence(), Some(15));
    assert!(matches!(
        IntegrityReceiver::new(key(), Direction::Uplink, Start::new(0).unwrap(), Some(16)),
        Err(Error::Count)
    ));
}
