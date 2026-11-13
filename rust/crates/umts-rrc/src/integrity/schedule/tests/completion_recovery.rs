// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn unconfirmed(last_old_count: u32) -> (IntegritySchedule, IntegrityContext, IntegrityContext) {
    let base = Instant::now();
    let first = context(1);
    let second = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    for count in 1..last_old_count {
        schedule
            .publish_with(key(), 2, &message(), |encoded| {
                assert_downlink(&first, 2, count, encoded);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    for count in 2..=last_old_count {
        let bytes = complete_bytes(&first, count, [0; 4]);
        schedule
            .receive_with(
                key(),
                2,
                &bytes,
                &mut vec![0; bytes.len()],
                |_, accepted| {
                    assert_eq!(accepted.count(), count);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
    }
    let mut exchange = exchange(
        &second,
        base,
        IntegrityChange::Modify(IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap()),
    );
    exchange
        .publish_scheduled_with(
            base,
            second.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let sequence = (last_old_count + 1) & 15;
    let count = sequence
        + if sequence < (last_old_count & 15) {
            16
        } else {
            0
        };
    finish_unconfirmed(&mut schedule, &mut exchange, &second, base, count, [0; 4]);
    (schedule, first, second)
}

#[test]
fn unconfirmed_completion_keeps_recovery_and_blocks_collection_retirement_and_next_command() {
    let (mut schedule, first, second) = unconfirmed(1);
    assert_eq!(schedule.pending_origin(), None);
    assert_eq!(schedule.unconfirmed_origin(), Some(second.origin()));
    assert_eq!(schedule.collect_unused(key()), Err(ScheduleFailure::Phase));
    assert_eq!(
        schedule.retire(key(), first.origin()),
        Err(ScheduleFailure::Phase)
    );
    let third = context(3);
    let mut exchange = exchange(
        &third,
        Instant::now(),
        IntegrityChange::Modify(IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap()),
    );
    assert!(matches!(
        exchange.publish_scheduled_with(
            Instant::now(),
            third,
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(())
        ),
        Err(ScheduleError::Schedule(ScheduleFailure::Phase))
    ));
    assert_eq!(
        schedule.confirm_delivery_with(key(), second.origin(), || Err::<(), _>("not proven")),
        Err(ScheduleError::Downstream("not proven"))
    );
    assert_eq!(schedule.unconfirmed_origin(), Some(second.origin()));
    schedule
        .confirm_delivery_with(key(), second.origin(), || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(schedule.unconfirmed_origin(), None);
    assert!(schedule.collect_unused(key()).is_ok());
    assert_eq!(
        schedule.recover_completed_with(key(), second.origin(), |_| Ok::<_, ()>(())),
        Err(ScheduleError::Schedule(ScheduleFailure::Phase))
    );
}

#[test]
fn completed_recovery_uses_old_counts_plus_skips_and_is_transactional_through_wrap() {
    for old_count in [1, 31] {
        let (mut schedule, first, second) = unconfirmed(old_count);
        let before = schedule.next_downlink_sequence(key(), 2).unwrap();
        assert_eq!(
            schedule.recover_completed_with(key(), second.origin(), |_| Err::<(), _>(
                "recovery queue full"
            )),
            Err(ScheduleError::Downstream("recovery queue full"))
        );
        assert_eq!(schedule.unconfirmed_origin(), Some(second.origin()));
        assert_eq!(schedule.next_downlink_sequence(key(), 2), Ok(before));
        schedule
            .recover_completed_with(key(), second.origin(), |plan| {
                assert_eq!(plan.previous_control_origin, Some(first.origin()));
                assert_eq!(plan.next_control_count, Some(old_count + 1));
                assert_eq!(plan.next_uplink_control_count, Some(old_count + 2));
                assert!(!plan.release_connection);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(schedule.unconfirmed_origin(), None);
        schedule
            .publish_with(key(), 2, &message(), |encoded| {
                assert_downlink(&first, 2, old_count + 1, encoded);
                Ok::<_, ()>(())
            })
            .unwrap();
        let new_key = complete_bytes(&second, old_count + 2, [0; 4]);
        assert!(
            schedule
                .receive_with(
                    key(),
                    2,
                    &new_key,
                    &mut vec![0; new_key.len()],
                    |_, _| Ok::<_, ()>(())
                )
                .is_err()
        );
        let old_key = complete_bytes(&first, old_count + 2, [0; 4]);
        schedule
            .receive_with(
                key(),
                2,
                &old_key,
                &mut vec![0; old_key.len()],
                |_, accepted| {
                    assert_eq!(accepted.origin(), first.origin());
                    assert_eq!(accepted.count(), old_count + 2);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        assert!(matches!(
            schedule.receive_with(
                key(),
                2,
                &old_key,
                &mut vec![0; old_key.len()],
                |_, _| Ok::<_, ()>(())
            ),
            Err(ScheduleError::Stream(StreamError::Duplicate))
        ));
    }
}

#[test]
fn first_security_completion_cannot_recover_to_unprotected_signalling() {
    let base = Instant::now();
    let first = context(1);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    let mut exchange = exchange(&first, base, IntegrityChange::Start { fresh: FRESH });
    exchange
        .publish_scheduled_with(
            base,
            first.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    finish_unconfirmed(&mut schedule, &mut exchange, &first, base, 1, [0; 4]);
    let mut wrong = key();
    wrong.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        schedule.recover_completed_with(wrong, first.origin(), |_| Ok::<_, ()>(())),
        Err(ScheduleError::Schedule(ScheduleFailure::Owner))
    );
    schedule
        .recover_completed_with(key(), first.origin(), |plan| {
            assert!(plan.release_connection);
            assert_eq!(plan.previous_control_origin, None);
            assert_eq!(plan.next_control_count, None);
            assert_eq!(plan.next_uplink_control_count, None);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(schedule.release_required());
    assert!(
        schedule
            .publish_with(key(), 3, &message(), |_| Ok::<_, ()>(()))
            .is_err()
    );
}
