// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    signalling::tests::{COMPLETE, INITIAL},
    test_support::{at, count},
};
use std::num::NonZeroU64;

fn key(slot: u32) -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot,
        generation: NonZeroU64::MIN,
    }
}
fn ingress() -> DedicatedIngress {
    DedicatedIngress::new(count(2), count(4), count(32)).unwrap()
}
fn push(queue: &mut DedicatedIngress, key: UeKey, bearer: SignallingBearer, copies: usize) {
    let bytes = if bearer == SignallingBearer::ControlAm {
        &COMPLETE[..]
    } else {
        &INITIAL[..]
    };
    queue
        .admit(key, bearer, copies, copies * bytes.len())
        .unwrap();
    for _ in 0..copies {
        queue.push_admitted(key, bearer, at(0), bytes);
    }
}

#[test]
fn upper_service_time_stays_monotonic_when_original_radio_observations_are_reordered() {
    let mut queue = ingress();
    let bearer = SignallingBearer::ControlAm;
    queue.admit(key(1), bearer, 2, 2 * COMPLETE.len()).unwrap();
    for (received, serviced) in [(100, 200), (50, 300)] {
        queue.push_timed_admitted(
            key(1),
            bearer,
            ReceiveTime::new(at(received), at(serviced)).unwrap(),
            &COMPLETE,
            None,
        );
    }
    for _ in 0..3 {
        let view = queue.front_eligible(|_, _| true).unwrap();
        assert_eq!(view.time.received(), at(100));
        assert_eq!(view.time.serviced(), at(200));
    }
    queue.pop();
    let view = queue.front_eligible(|_, _| true).unwrap();
    assert_eq!(view.time.received(), at(50));
    assert_eq!(view.time.serviced(), at(300));
}

#[test]
fn suspended_nas_cannot_consume_control_capacity() {
    let mut queue = ingress();
    push(&mut queue, key(1), SignallingBearer::NasAm, 2);
    assert_eq!(
        queue.admit(key(2), SignallingBearer::NasAm, 1, INITIAL.len()),
        Err(SignallingError::Capacity)
    );
    push(&mut queue, key(2), SignallingBearer::ControlAm, 1);
    let view = queue
        .front_eligible(|_, bearer| bearer == SignallingBearer::ControlAm)
        .unwrap();
    assert_eq!(view.key, key(2));
    assert_eq!(view.bytes, COMPLETE);
    assert!(view.message.is_ok());
    queue.pop();
    assert!(
        queue
            .front_eligible(|_, bearer| bearer == SignallingBearer::ControlAm)
            .is_none()
    );
    assert_eq!(queue.data.ordered.len(), 2);
    assert_eq!(queue.front_eligible(|_, _| true).unwrap().key, key(1));
}

#[test]
fn pending_security_drains_old_nas_but_keeps_protected_nas_behind_completion() {
    use umts_rrc_schema::information_elements as ie;
    let mut queue = ingress();
    push(&mut queue, key(1), SignallingBearer::NasAm, 1);
    let mut protected = decode_uplink_dedicated(&INITIAL, INITIAL.len() * BITS_PER_BYTE)
        .unwrap()
        .value;
    protected.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(crate::test_support::integrity::fixed(0)),
        ie::RRCMessageSequenceNumber(0),
    ));
    let protected = rasn::uper::encode(&protected).unwrap();
    queue
        .admit(key(1), SignallingBearer::NasAm, 1, protected.len())
        .unwrap();
    queue.push_admitted(key(1), SignallingBearer::NasAm, at(1), &protected);
    push(&mut queue, key(1), SignallingBearer::ControlAm, 1);
    let eligible =
        |_, bearer, protected| bearer == SignallingBearer::ControlAm || protected == Some(false);
    assert_eq!(
        queue
            .front_eligible_with_protection(eligible)
            .unwrap()
            .bytes,
        INITIAL
    );
    queue.pop();
    assert_eq!(
        queue
            .front_eligible_with_protection(eligible)
            .unwrap()
            .bearer,
        SignallingBearer::ControlAm
    );
    queue.pop();
    assert!(queue.front_eligible_with_protection(eligible).is_none());
    assert!(queue.pending(key(1), SignallingBearer::NasAm));
    assert_eq!(queue.front_eligible(|_, _| true).unwrap().bytes, protected);
}

#[test]
fn a_waiting_control_batch_cannot_be_starved_by_new_smaller_batches() {
    let mut queue = ingress();
    push(&mut queue, key(1), SignallingBearer::ControlAm, 2);
    assert_eq!(
        queue.admit(key(2), SignallingBearer::ControlAm, 4, 4 * COMPLETE.len()),
        Err(SignallingError::Capacity)
    );
    assert_eq!(queue.control.waiting, Some(key(2)));
    queue.front_eligible(|_, _| true).unwrap();
    queue.pop();
    assert_eq!(
        queue.admit(key(1), SignallingBearer::ControlAm, 1, COMPLETE.len()),
        Err(SignallingError::Capacity)
    );
    assert_eq!(
        queue.admit(key(3), SignallingBearer::ControlAm, 0, 0),
        Ok(())
    );
    assert_eq!(queue.control.waiting, Some(key(2)));
    push(&mut queue, key(3), SignallingBearer::NasAm, 1);
    queue
        .front_eligible(|_, bearer| bearer == SignallingBearer::ControlAm)
        .unwrap();
    queue.pop();
    push(&mut queue, key(2), SignallingBearer::ControlAm, 4);
    assert_eq!(queue.control.waiting, None);
    assert_eq!(queue.control.ordered.capacity(), 4);
    assert_eq!(queue.control.free.capacity(), 4);
    let data = queue.front_eligible(|_, _| true).unwrap();
    assert_eq!(data.key, key(3));
    assert_eq!(data.bearer, SignallingBearer::NasAm);
    queue.pop();
    for _ in 0..4 {
        assert_eq!(queue.front_eligible(|_, _| true).unwrap().key, key(2));
        queue.pop();
    }
    assert_eq!(queue.control.free.len(), 4);
}

#[test]
fn retirement_clears_owned_bytes_and_waiters_without_affecting_a_reused_slot() {
    let mut queue = ingress();
    let old = key(1);
    let replacement = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..old
    };
    push(&mut queue, old, SignallingBearer::ControlAm, 1);
    push(&mut queue, replacement, SignallingBearer::ControlAm, 1);
    push(&mut queue, old, SignallingBearer::NasAm, 1);
    assert_eq!(
        queue.admit(old, SignallingBearer::ControlAm, 4, 4 * COMPLETE.len()),
        Err(SignallingError::Capacity)
    );
    queue.cancel_for(old);
    assert!(!queue.pending(old, SignallingBearer::ControlAm));
    assert!(!queue.pending(old, SignallingBearer::NasAm));
    assert_eq!(queue.control.waiting, None);
    assert_eq!(queue.front_eligible(|_, _| true).unwrap().key, replacement);
    for lane in [&queue.control, &queue.data] {
        for slot in &lane.slots {
            if slot.key.is_none() {
                assert!(slot.bytes.iter().all(|byte| *byte == 0));
            }
        }
    }
    queue.pop();
    push(&mut queue, key(2), SignallingBearer::ControlAm, 4);
    assert_eq!(queue.control.ordered.len(), 4);
}

#[test]
fn impossible_batches_and_sequence_exhaustion_do_not_capture_the_control_reservation() {
    let mut queue = ingress();
    for (copies, bytes) in [(5, COMPLETE.len()), (1, 33), (0, 1)] {
        assert_eq!(
            queue.admit(key(1), SignallingBearer::ControlAm, copies, bytes),
            Err(SignallingError::Capacity)
        );
        assert_eq!(queue.control.waiting, None);
    }
    queue.next_sequence = u64::MAX;
    assert_eq!(
        queue.admit(key(1), SignallingBearer::ControlAm, 1, COMPLETE.len()),
        Err(SignallingError::Capacity)
    );
    assert_eq!(queue.control.waiting, None);
    assert!(matches!(
        DedicatedIngress::new(NonZeroUsize::MAX, count(1), count(1)),
        Err(SignallingError::Capacity)
    ));
}

#[test]
fn eligibility_inspects_only_occupied_entries_and_preserves_original_bytes_on_retry() {
    let mut queue = DedicatedIngress::new(
        count(2),
        count(umts_rlc::acknowledged::MAX_DELIVERY_SDUS),
        count(32),
    )
    .unwrap();
    push(&mut queue, key(1), SignallingBearer::ControlAm, 1);
    push(&mut queue, key(2), SignallingBearer::NasAm, 1);
    let mut inspected = 0;
    assert!(
        queue
            .front_eligible(|_, _| {
                inspected += 1;
                false
            })
            .is_none()
    );
    assert_eq!(inspected, 2);
    for _ in 0..2 {
        let view = queue.front_eligible(|_, _| true).unwrap();
        assert_eq!(view.key, key(1));
        assert_eq!(view.bytes, COMPLETE);
        assert!(view.message.is_ok());
    }
    assert_eq!(queue.control.ordered.len(), 1);
    assert_eq!(queue.next_sequence, 2);
}

#[test]
fn the_complete_supported_am_delivery_batch_fits_the_reserved_control_lane() {
    let maximum = umts_rlc::acknowledged::MAX_DELIVERY_SDUS;
    let mut queue = DedicatedIngress::new(count(1), count(maximum), count(32)).unwrap();
    push(&mut queue, key(1), SignallingBearer::NasAm, 1);
    push(&mut queue, key(2), SignallingBearer::ControlAm, maximum);
    for _ in 0..maximum {
        assert_eq!(
            queue
                .front_eligible(|_, bearer| bearer == SignallingBearer::ControlAm)
                .unwrap()
                .key,
            key(2)
        );
        queue.pop();
    }
    assert!(queue.pending(key(1), SignallingBearer::NasAm));
    assert_eq!(queue.control.free.len(), maximum);
}
