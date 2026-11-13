// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    signalling::{
        SignallingBearer,
        tests::{
            controller, establish, limits, peer, request, send_capability_report_with_transaction,
        },
    },
    test_support::{at, source},
};
use std::num::{NonZeroU32, NonZeroU64};
use umts_types::{RadioEpoch, bits::Bits};

#[test]
fn actual_common_am_report_moves_owned_request_from_requested_to_pending_to_ready() {
    for compact in [false, true] {
        let source = source(compact);
        let mut radio = controller(&source);
        let key = establish(&mut radio, &source, 0);
        let transaction = match radio.ensure_capabilities(key, at(0)).unwrap() {
            CapabilityReadiness::Requested(transaction) => transaction,
            other => panic!("expected new enquiry, got {other:?}"),
        };
        assert_eq!(transaction.get(), 0);
        assert_eq!(
            radio.ensure_capabilities(key, at(0)),
            Ok(CapabilityReadiness::Pending(transaction))
        );
        assert!(radio.capabilities(key).is_none());
        let mut peer = peer(&source);
        peer.enqueue(&crate::signalling::tests::COMPLETE).unwrap();
        peer.publish_next(at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        send_capability_report_with_transaction(&mut radio, key, &mut peer, transaction);
        assert_eq!(radio.process_one(), Some(Ok(key)));
        assert_eq!(radio.capabilities(key).unwrap().key(), key);
        assert_eq!(
            radio.ensure_capabilities(key, at(0)),
            Ok(CapabilityReadiness::Ready)
        );
        assert_eq!(
            radio.ensure_capabilities(key, at(0)),
            Ok(CapabilityReadiness::Ready)
        );
    }
}

#[test]
fn backpressure_does_not_consume_a_transaction_and_retries_do_not_duplicate_publication() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    for _ in 0..limits().queued_sdus_per_bearer.get() {
        radio
            .registry
            .get_mut(key)
            .unwrap()
            .control
            .enqueue(key, Bits::new(&[0], 8).unwrap())
            .unwrap();
    }
    for _ in 0..3 {
        assert_eq!(
            radio.ensure_capabilities(key, at(0)),
            Err(SignallingError::Capacity)
        );
    }
    radio
        .publish_bearer(key, SignallingBearer::ControlAm, &mut source, at(0), at(0))
        .unwrap();
    radio
        .publish_bearer(
            key,
            SignallingBearer::ControlAm,
            &mut source,
            at(0),
            at(umts_types::time::CHIPS_PER_FRAME),
        )
        .unwrap();
    let first = TransactionId::new(0).unwrap();
    assert_eq!(
        radio.ensure_capabilities(key, at(0)),
        Ok(CapabilityReadiness::Requested(first))
    );
    for _ in 0..3 {
        assert_eq!(
            radio.ensure_capabilities(key, at(0)),
            Ok(CapabilityReadiness::Pending(first))
        );
    }
}

#[test]
fn expired_real_exchange_rotates_only_its_owned_transaction() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    assert_eq!(
        radio.ensure_capabilities(key, at(0)),
        Ok(CapabilityReadiness::Requested(
            TransactionId::new(0).unwrap()
        ))
    );
    radio
        .expire_some(
            at(limits().response_guard_chips.get()),
            NonZeroU32::new(8).unwrap(),
        )
        .unwrap();
    assert_eq!(radio.counters().capability_timeouts, 1);
    assert_eq!(
        radio.ensure_capabilities(key, at(limits().response_guard_chips.get())),
        Ok(CapabilityReadiness::Requested(
            TransactionId::new(1).unwrap()
        ))
    );
}

#[test]
fn existing_explicit_transactions_and_reports_are_not_confused_between_ues() {
    let source = source(false);
    let mut radio = controller(&source);
    let first = establish(&mut radio, &source, 0);
    let second = establish(&mut radio, &source, 1);
    let explicit = TransactionId::new(2).unwrap();
    radio.request_capabilities(second, at(0), explicit).unwrap();
    assert_eq!(
        radio.ensure_capabilities(second, at(0)),
        Ok(CapabilityReadiness::Pending(explicit))
    );
    let requested = TransactionId::new(0).unwrap();
    assert_eq!(
        radio.ensure_capabilities(first, at(0)),
        Ok(CapabilityReadiness::Requested(requested))
    );
    let mut peer = peer(&source);
    peer.enqueue(&crate::signalling::tests::COMPLETE).unwrap();
    peer.publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    send_capability_report_with_transaction(&mut radio, first, &mut peer, requested);
    assert_eq!(radio.process_one(), Some(Ok(first)));
    assert_eq!(
        radio.ensure_capabilities(first, at(0)),
        Ok(CapabilityReadiness::Ready)
    );
    assert_eq!(
        radio.ensure_capabilities(second, at(0)),
        Ok(CapabilityReadiness::Pending(explicit))
    );
    assert!(radio.capabilities(second).is_none());
}

#[test]
fn unestablished_or_foreign_epoch_calls_cannot_publish_an_enquiry() {
    let source = source(false);
    let mut radio = controller(&source);
    let unestablished = radio.request(&source, at(0), &request(0)).unwrap();
    assert_eq!(
        radio.ensure_capabilities(unestablished, at(0)),
        Err(SignallingError::Phase)
    );
    let key = establish(&mut radio, &source, 1);
    let mut foreign = at(0);
    foreign.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert_eq!(
        radio.ensure_capabilities(key, foreign),
        Err(SignallingError::Origin)
    );
    assert_eq!(
        radio.ensure_capabilities(key, at(0)),
        Ok(CapabilityReadiness::Requested(
            TransactionId::new(0).unwrap()
        ))
    );
}
