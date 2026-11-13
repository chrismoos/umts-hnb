// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn common(context: &IntegrityContext, count: u32) -> Vec<u8> {
    let mut message = umts_rrc_codec::decode_uplink_common(
        &[
            0x00, 0x48, 0xd1, 0x59, 0xe1, 0x09, 0x1a, 0x2d, 0x50, 0xc8, 0x6c, 0x00,
        ],
        89,
    )
    .unwrap()
    .value;
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(0)),
        ie::RRCMessageSequenceNumber(0),
    ));
    let preimage = rasn::uper::encode(message.as_ref()).unwrap();
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
    rasn::uper::encode(message.as_ref()).unwrap()
}

#[test]
fn protected_initial_common_recovery_requires_complete_and_authentication() {
    let base = Instant::now();
    let first = context(1);
    let wrong = context(2);
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
    let bytes = common(&first, 1);
    assert_eq!(
        schedule.receive_common_with(
            key(),
            &bytes,
            &mut vec![0; bytes.len()],
            |_| Ok::<_, ()>(())
        ),
        Err(ScheduleError::Schedule(ScheduleFailure::MissingEpoch))
    );
    finish_unconfirmed(&mut schedule, &mut exchange, &first, base, 1, [0; 4]);
    let invalid = common(&wrong, 1);
    assert!(
        schedule
            .receive_common_with(
                key(),
                &invalid,
                &mut vec![0; invalid.len()],
                |_| Ok::<_, ()>(())
            )
            .is_err()
    );
    assert_eq!(
        schedule.receive_common_with(key(), &bytes, &mut vec![0; bytes.len()], |_| Err::<(), _>(
            "full"
        )),
        Err(ScheduleError::Downstream("full"))
    );
    schedule
        .receive_common_with(key(), &bytes, &mut vec![0; bytes.len()], |accepted| {
            assert_eq!(accepted.origin(), first.origin());
            assert_eq!(accepted.count(), 1);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(schedule.unconfirmed_origin(), Some(first.origin()));
}

#[test]
fn common_recovery_authenticates_one_prior_rb0_context_before_and_after_complete() {
    let base = Instant::now();
    let first = context(1);
    let second = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    assert_eq!(schedule.allows_unprotected_common(key()), Ok(true));
    initial(&mut schedule, &first, base);
    assert_eq!(schedule.allows_unprotected_common(key()), Ok(false));
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
    let wrong = common(&second, 17);
    assert!(
        schedule
            .receive_common_with(
                key(),
                &wrong,
                &mut vec![0; wrong.len()],
                |_| Ok::<_, ()>(())
            )
            .is_err()
    );
    let old = common(&first, 1);
    assert_eq!(
        schedule.receive_common_with(key(), &old, &mut vec![0; old.len()], |_| Err::<(), _>(
            "queue full"
        )),
        Err(ScheduleError::Downstream("queue full"))
    );
    schedule
        .receive_common_with(key(), &old, &mut vec![0; old.len()], |accepted| {
            assert_eq!(accepted.origin(), first.origin());
            assert_eq!(accepted.bearer(), 0);
            assert_eq!(accepted.count(), 1);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(matches!(
        schedule.receive_common_with(key(), &old, &mut vec![0; old.len()], |_| Ok::<_, ()>(())),
        Err(ScheduleError::Stream(StreamError::Duplicate))
    ));
    finish_unconfirmed(&mut schedule, &mut exchange, &second, base, 2, [0; 4]);
    let old = common(&first, 2);
    schedule
        .receive_common_with(key(), &old, &mut vec![0; old.len()], |accepted| {
            assert_eq!(accepted.origin(), first.origin());
            Ok::<_, ()>(())
        })
        .unwrap();
    schedule
        .confirm_delivery_with(key(), second.origin(), || Ok::<_, ()>(()))
        .unwrap();
    let current = common(&second, 17);
    schedule
        .receive_common_with(key(), &current, &mut vec![0; current.len()], |accepted| {
            assert_eq!(accepted.origin(), second.origin());
            assert_eq!(accepted.count(), 17);
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn initial_rb0_activation_zero_starts_without_an_invented_wrap() {
    for count in [1, 2, 15] {
        let base = Instant::now();
        let first = context(1);
        let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
        initial(&mut schedule, &first, base);
        assert_eq!(schedule.allows_unprotected_common(key()), Ok(false));
        let bytes = common(&first, count);
        schedule
            .receive_common_with(key(), &bytes, &mut vec![0; bytes.len()], |accepted| {
                assert_eq!(accepted.count(), count);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(matches!(
            schedule.receive_common_with(
                key(),
                &bytes,
                &mut vec![0; bytes.len()],
                |_| Ok::<_, ()>(())
            ),
            Err(ScheduleError::Stream(StreamError::Duplicate))
        ));
        let wrapped = common(&first, 16);
        schedule
            .receive_common_with(key(), &wrapped, &mut vec![0; wrapped.len()], |accepted| {
                assert_eq!(accepted.count(), 16);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
}

#[test]
fn rb0_nonzero_activation_initializes_from_its_announced_predecessor_not_the_old_sn() {
    let base = Instant::now();
    let first = context(1);
    let second = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    let old = common(&first, 15);
    schedule
        .receive_common_with(key(), &old, &mut vec![0; old.len()], |_| Ok::<_, ()>(()))
        .unwrap();
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
    finish_unconfirmed(&mut schedule, &mut exchange, &second, base, 2, [5, 0, 0, 0]);
    schedule
        .confirm_delivery_with(key(), second.origin(), || Ok::<_, ()>(()))
        .unwrap();
    let current = common(&second, 6);
    schedule
        .receive_common_with(key(), &current, &mut vec![0; current.len()], |accepted| {
            assert_eq!(accepted.origin(), second.origin());
            assert_eq!(accepted.count(), 6);
            Ok::<_, ()>(())
        })
        .unwrap();
}
