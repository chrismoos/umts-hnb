// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_types::RadioEpoch;

fn nonzero(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}
fn ue() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(nonzero(1)),
        slot: 0,
        generation: nonzero(1),
    }
}
fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: ue().epoch,
        index,
    }
}
fn process(incarnation: u64) -> HarqProcess<Vec<u8>> {
    HarqProcess::new(ue(), nonzero(incarnation), ProcessId::new(0).unwrap())
}

#[test]
fn transmission_count_tracks_submitted_attempts_not_reservations() {
    let mut process = process(1);
    process.replace(vec![1]);
    let first = process.schedule(at(0), at(30000)).unwrap();
    process.lost(first.key).unwrap();
    assert_eq!(process.transmitted_count(), 0);
    let retry = process.schedule(at(40000), at(70000)).unwrap();
    assert_eq!(process.transmitted(first.key), Err(HarqError::Stale));
    assert_eq!(process.transmitted_count(), 0);
    process.transmitted(retry.key).unwrap();
    assert_eq!(process.transmitted_count(), 1);
    assert_eq!(process.transmitted(retry.key), Err(HarqError::State));
    assert_eq!(process.transmitted_count(), 1);
    process
        .feedback(retry.key, retry.expected_ack, Feedback::Nack)
        .unwrap();
    let unsubmitted = process.schedule(at(80000), at(110000)).unwrap();
    process.lost(unsubmitted.key).unwrap();
    assert_eq!(process.transmitted_count(), 1);
    let submitted = process.schedule(at(120000), at(150000)).unwrap();
    process.transmitted(submitted.key).unwrap();
    process.lost(submitted.key).unwrap();
    assert_eq!(process.transmitted_count(), 2);
    let final_attempt = process.schedule(at(160000), at(190000)).unwrap();
    process.transmitted(final_attempt.key).unwrap();
    assert_eq!(process.transmitted_count(), 3);
    process
        .feedback(final_attempt.key, final_attempt.expected_ack, Feedback::Ack)
        .unwrap();
    assert_eq!(process.transmitted_count(), 0);
    process.replace(vec![2]);
    let replacement = process.schedule(at(200000), at(230000)).unwrap();
    process.transmitted(replacement.key).unwrap();
    assert_eq!(process.transmitted_count(), 1);
    assert_eq!(process.replace(vec![3]), Some(vec![2]));
    assert_eq!(process.transmitted_count(), 0);
    let cancelled = process.schedule(at(240000), at(270000)).unwrap();
    process.transmitted(cancelled.key).unwrap();
    process.cancel();
    assert_eq!(process.transmitted_count(), 0);
}

#[test]
fn ndi_changes_only_for_new_transmitted_data_and_payload_survives_retry() {
    let mut process = process(1);
    assert!(process.replace(vec![1, 2, 3]).is_none());
    let first = process.schedule(at(0), at(30000)).unwrap();
    assert!(!first.new_data);
    assert_eq!(
        process.feedback(first.key, first.expected_ack, Feedback::Ack),
        Err(HarqError::State)
    );
    process.transmitted(first.key).unwrap();
    assert!(
        process
            .feedback(first.key, first.expected_ack, Feedback::Nack)
            .unwrap()
            .is_none()
    );
    assert_eq!(process.payload().unwrap(), &[1, 2, 3]);
    let retry = process.schedule(at(40000), at(70000)).unwrap();
    assert!(!retry.new_data);
    process.transmitted(retry.key).unwrap();
    assert_eq!(
        process.feedback(first.key, first.expected_ack, Feedback::Ack),
        Err(HarqError::Stale)
    );
    assert_eq!(
        process
            .feedback(retry.key, retry.expected_ack, Feedback::Ack)
            .unwrap(),
        Some(vec![1, 2, 3])
    );
    process.replace(vec![4]);
    assert!(process.schedule(at(80000), at(110000)).unwrap().new_data);
}

#[test]
fn fresh_incarnation_rejects_feedback_from_replaced_process() {
    let mut old = process(1);
    let mut fresh = process(2);
    old.replace(vec![1]);
    fresh.replace(vec![2]);
    let first = old.schedule(at(0), at(30000)).unwrap();
    let second = fresh.schedule(at(0), at(30000)).unwrap();
    fresh.transmitted(second.key).unwrap();
    assert_eq!(
        fresh.feedback(first.key, first.expected_ack, Feedback::Ack),
        Err(HarqError::Stale)
    );
    assert_eq!(fresh.payload().unwrap(), &[2]);
}

#[test]
fn closed_feedback_window_retains_payload_and_rejects_late_result() {
    let mut process = process(1);
    process.replace(vec![1]);
    let first = process.schedule(at(0), at(30000)).unwrap();
    process.transmitted(first.key).unwrap();
    assert_eq!(
        process.feedback_missing(first.key, at(32559)),
        Err(HarqError::Time)
    );
    process.feedback_missing(first.key, at(32560)).unwrap();
    let retry = process.schedule(at(40000), at(70000)).unwrap();
    process.transmitted(retry.key).unwrap();
    assert_eq!(
        process.feedback(first.key, at(30000), Feedback::Ack),
        Err(HarqError::Stale)
    );
    assert_eq!(process.cancel(), Some(vec![1]));
}

#[test]
fn lost_untransmitted_data_does_not_advance_ndi_and_replacement_returns_custody() {
    let mut process = process(1);
    process.replace(vec![1]);
    let first = process.schedule(at(0), at(30000)).unwrap();
    process.lost(first.key).unwrap();
    assert_eq!(process.replace(vec![2]), Some(vec![1]));
    assert!(!process.schedule(at(40000), at(70000)).unwrap().new_data);
}

fn entity(key: UeKey, category: u8) -> HarqEntity<Vec<u8>> {
    HarqEntity::new(
        key,
        nonzero(1),
        umts_types::hsdpa::Category::new(category).unwrap(),
        at(512),
        &[std::num::NonZeroUsize::new(3200).unwrap(); 6],
        std::num::NonZeroU8::MIN,
    )
    .unwrap()
}

#[test]
fn entity_applies_spacing_across_processes_without_throttling_peers() {
    let mut first = entity(ue(), 1);
    let mut peer = entity(UeKey { slot: 1, ..ue() }, 10);
    let zero = ProcessId::new(0).unwrap();
    let one = ProcessId::new(1).unwrap();
    assert_eq!(first.process_state(zero), Ok(ProcessState::Empty));
    assert_eq!(first.next_downlink(at(0)), Ok(at(512)));
    assert_eq!(first.next_downlink(at(513)), Ok(at(512 + CHIPS_PER_HS_TTI)));
    first.replace(zero, vec![1]).unwrap();
    assert_eq!(first.process_state(zero), Ok(ProcessState::Ready));
    first.replace(one, vec![2]).unwrap();
    peer.replace(zero, vec![3]).unwrap();
    let start = at(512);
    let reserved = first.schedule(zero, start, at(30000)).unwrap();
    assert_eq!(first.process_state(zero), Ok(ProcessState::Busy));
    assert_eq!(
        first.next_downlink(start),
        Ok(at(512 + 3 * CHIPS_PER_HS_TTI))
    );
    assert_eq!(peer.next_downlink(start), Ok(start));
    assert!(first.schedule(one, at(512 + 7680), at(40000)).is_err());
    assert!(first.schedule(one, at(513 + 3 * 7680), at(60000)).is_err());
    assert!(peer.schedule(zero, start, at(30000)).is_ok());
    first.lost(reserved.key).unwrap();
    assert_eq!(first.process_state(zero), Ok(ProcessState::Ready));
    assert_eq!(
        first.next_downlink(start),
        Ok(at(512 + 3 * CHIPS_PER_HS_TTI))
    );
    assert!(first.next_downlink(at(u64::MAX)).is_err());
    assert!(first.schedule(one, start, at(30000)).is_err());
    assert!(first.schedule(one, at(512 + 3 * 7680), at(60000)).is_ok());
    assert_eq!(first.payload(zero).unwrap().unwrap(), &[1]);
}

#[test]
fn acknowledgement_repetition_blocks_the_affected_ues_intervening_ttis() {
    let mut owner = HarqEntity::new(
        ue(),
        nonzero(1),
        umts_types::hsdpa::Category::new(10).unwrap(),
        at(512),
        &[std::num::NonZeroUsize::new(3200).unwrap(); 6],
        std::num::NonZeroU8::new(4).unwrap(),
    )
    .unwrap();
    for index in 0..2 {
        owner
            .replace(ProcessId::new(index).unwrap(), index)
            .unwrap();
    }
    let transmission = owner
        .schedule(ProcessId::new(0).unwrap(), at(512), at(30000))
        .unwrap();
    owner.transmitted(transmission.key).unwrap();
    assert_eq!(
        owner.next_downlink(at(512)),
        Ok(at(512 + 4 * CHIPS_PER_HS_TTI))
    );
    let end = 30000 + 3 * CHIPS_PER_HS_TTI + CHIPS_PER_SLOT;
    assert!(
        owner
            .feedback_missing(transmission.key, at(end - 1))
            .is_err()
    );
    owner.feedback_missing(transmission.key, at(end)).unwrap();
    for tti in 1..4 {
        assert!(
            owner
                .schedule(ProcessId::new(1).unwrap(), at(512 + tti * 7680), at(90000))
                .is_err()
        );
    }
    assert!(
        owner
            .schedule(ProcessId::new(1).unwrap(), at(512 + 4 * 7680), at(90000))
            .is_ok()
    );
}

#[test]
fn entity_preserves_custody_on_rejection_and_failed_schedule_is_transactional() {
    let mut owner = entity(ue(), 10);
    let zero = ProcessId::new(0).unwrap();
    let absent = ProcessId::new(7).unwrap();
    let rejected = owner.replace(absent, vec![9]).unwrap_err();
    assert_eq!(rejected.payload, vec![9]);
    owner.replace(zero, vec![1]).unwrap();
    assert!(owner.schedule(zero, at(512), at(511)).is_err());
    let reserved = owner.schedule(zero, at(512), at(30000)).unwrap();
    assert_eq!(owner.replace(zero, vec![2]).unwrap_err().payload, vec![2]);
    assert!(owner.cancel(zero).is_err());
    owner.transmitted(reserved.key).unwrap();
    assert!(owner.replace(zero, vec![3]).is_err());
    assert_eq!(
        owner
            .feedback(reserved.key, at(30000), Feedback::Ack)
            .unwrap(),
        Some(vec![1])
    );
    owner.replace(zero, vec![4]).unwrap();
    assert_eq!(owner.into_payloads().collect::<Vec<_>>(), vec![vec![4]]);
}
