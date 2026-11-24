// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    signalling::{
        protection::Protection,
        tests::{COMPLETE, controller, establish, peer},
    },
    test_support::{at, source},
};
use std::num::NonZeroU64;
use umts_security::{
    Key,
    ciphering::CipherUse,
    contexts::{CipherMaterial, KeyOrigin, KeySet},
    counters::{CounterInitialization, Start},
};
use umts_types::{
    CnDomain,
    bits::{BITS_PER_BYTE, Bits},
};

fn key() -> UeKey {
    UeKey {
        epoch: at(0).epoch,
        slot: 0,
        generation: NonZeroU64::MIN,
    }
}
fn plan(key: UeKey, sequences: &[(u8, u16)], counters: CounterInitialization) -> CipherPlan {
    let layout = BearerLayout::new(SignallingSet::Base, &[]).unwrap();
    let context = KeySet::uia1(
        KeyOrigin {
            ue: key,
            domain: CnDomain::Cs,
            generation: NonZeroU64::MIN,
        },
        CipherMaterial::Uea1(Key::new([0x22; 16])),
        Key::new([0x11; 16]),
    )
    .split()
    .0;
    CipherPlan {
        context,
        counters,
        downlink: ActivationSequences::new(layout, sequences).unwrap(),
        dpch_cfn: None,
    }
}

#[test]
fn command_publication_commits_all_downlink_maps_or_none_and_preserves_wrapped_count_anchors() {
    for counters in [
        CounterInitialization::NewKeys,
        CounterInitialization::ReusedKeys(Start::new(0xabcde).unwrap()),
    ] {
        let mut protection = Protection::new(key(), 0, 1024).unwrap();
        let pending = PendingCipher::prepare(
            key(),
            plan(key(), &[(1, 1), (2, 2), (3, 0)], counters),
            &protection.ciphers,
            [127, 4095, 4094],
            NewSduRange {
                first: 4095,
                next: 4098,
            },
        )
        .unwrap();
        assert_eq!(
            pending.publish_with(&mut protection.ciphers, || Err::<(), _>(
                SignallingError::Capacity
            )),
            Err(SignallingError::Capacity)
        );
        for (bearer, position) in [(1, 129), (2, 4098), (3, 4096)] {
            assert!(matches!(
                protection.ciphers.selected(
                    key(),
                    bearer,
                    Direction::Downlink,
                    Position::Sequence(position)
                ),
                Ok(CipherUse::Data { origin: None, .. })
            ));
        }
        pending
            .publish_with(&mut protection.ciphers, || Ok(()))
            .unwrap();
        for (bearer, position, sequence) in [(1, 129, 1), (2, 4098, 2), (3, 4096, 0)] {
            assert_eq!(
                protection.ciphers.selected(
                    key(),
                    bearer,
                    Direction::Downlink,
                    Position::Sequence(position)
                ),
                Ok(CipherUse::Data {
                    origin: Some(pending.plan.context.origin()),
                    count: counters.start().initial_count() | sequence
                })
            );
            assert!(matches!(
                protection.ciphers.selected(
                    key(),
                    bearer,
                    Direction::Downlink,
                    Position::Sequence(position - 1)
                ),
                Ok(CipherUse::Data { origin: None, .. })
            ));
            assert!(matches!(
                protection.ciphers.selected(
                    key(),
                    bearer,
                    Direction::Uplink,
                    Position::Sequence(position)
                ),
                Ok(CipherUse::Data { origin: None, .. })
            ));
        }
        assert!(pending.admits_downlink(SignallingBearer::Unacknowledged, 128));
        assert!(!pending.admits_downlink(SignallingBearer::Unacknowledged, 129));
        assert!(pending.admits_downlink(SignallingBearer::NasAm, 4095));
        assert!(!pending.admits_downlink(SignallingBearer::NasAm, 4096));
        assert!(pending.admits_downlink(SignallingBearer::ControlAm, 4098));
    }
}

#[test]
fn command_validation_rejects_activation_inside_the_sdu_and_a_span_that_crosses_old_keys() {
    let mut protection = Protection::new(key(), 0, 1024).unwrap();
    assert!(matches!(
        PendingCipher::prepare(
            key(),
            plan(
                key(),
                &[(1, 0), (2, 1), (3, 0)],
                CounterInitialization::NewKeys
            ),
            &protection.ciphers,
            [0; 3],
            NewSduRange { first: 0, next: 2 }
        ),
        Err(SignallingError::ConfigurationFailure)
    ));
    let old = plan(
        key(),
        &[(1, 0), (2, 1), (3, 0)],
        CounterInitialization::NewKeys,
    );
    protection
        .ciphers
        .update_with(
            old.context,
            &[CipherActivation::Sequenced {
                bearer: 2,
                direction: Direction::Downlink,
                anchor: CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(1), 1)
                    .unwrap(),
                first_new: Position::Sequence(0),
            }],
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(matches!(
        PendingCipher::prepare(
            key(),
            plan(
                key(),
                &[(1, 0), (2, 8), (3, 0)],
                CounterInitialization::NewKeys
            ),
            &protection.ciphers,
            [0; 3],
            NewSduRange { first: 0, next: 2 }
        ),
        Err(SignallingError::ConfigurationFailure)
    ));
}

#[test]
fn pending_control_cipher_drops_out_of_order_pdus_and_never_consumes_unknown_cipher_pdus() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let context = controller.registry.get_mut(key).unwrap();
    let pending = PendingCipher::prepare(
        key,
        plan(
            key,
            &[(1, 0), (2, 32), (3, 0)],
            CounterInitialization::NewKeys,
        ),
        &context.protection.ciphers,
        [0; 3],
        NewSduRange { first: 0, next: 1 },
    )
    .unwrap();
    pending
        .publish_with(&mut context.protection.ciphers, || Ok(()))
        .unwrap();
    context.protection.pending_cipher = Some(pending);
    context.control.require_delivery_admission(key).unwrap();
    let mut peer = peer(&source);
    peer.enqueue(&COMPLETE).unwrap();
    peer.publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let mut pdus = Vec::new();
    for _ in 0..2 {
        peer.enqueue(&COMPLETE).unwrap();
        peer.publish_next(at(0), |publication| {
            pdus.push(publication.bytes.to_vec());
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    }
    let receive = |controller: &mut crate::signalling::CommonSignalling, bytes: &[u8]| {
        controller.receive(
            key,
            at(0),
            umts_mac::CommonPdu {
                header: umts_mac::CommonHeader::Dedicated {
                    identity: umts_mac::CommonIdentity::Cell(
                        controller.assigned(key).unwrap().crnti(),
                    ),
                    channel: crate::signalling::logical(SignallingBearer::ControlAm).unwrap(),
                },
                payload: Bits::new(bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
            },
        )
    };
    let before = controller
        .registry
        .get(key)
        .unwrap()
        .control
        .window_positions();
    assert_eq!(receive(&mut controller, &pdus[1]), Ok(()));
    assert_eq!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions(),
        before
    );
    receive(&mut controller, &pdus[0]).unwrap();
    assert_eq!(
        receive(&mut controller, &pdus[1]),
        Err(SignallingError::Capacity)
    );
    assert_eq!(
        controller.collect_one(key, SignallingBearer::ControlAm, at(0)),
        Ok(true)
    );
    assert_eq!(
        receive(&mut controller, &pdus[1]),
        Err(SignallingError::Capacity)
    );
    controller.ingress.cancel_for(key);
    controller
        .registry
        .get_mut(key)
        .unwrap()
        .protection
        .pending_cipher = None;
    receive(&mut controller, &pdus[1]).unwrap();
    assert_eq!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions()
            .next_receive,
        before.next_receive + 2
    );
}

#[test]
fn wrong_ue_and_non_serving_layouts_cannot_stage_partial_activation() {
    let protection = Protection::new(key(), 0, 1024).unwrap();
    let wrong = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    let mut invalid = plan(
        wrong,
        &[(1, 0), (2, 8), (3, 0)],
        CounterInitialization::NewKeys,
    );
    assert!(matches!(
        PendingCipher::prepare(
            key(),
            invalid,
            &protection.ciphers,
            [0; 3],
            NewSduRange { first: 0, next: 1 }
        ),
        Err(SignallingError::ConfigurationFailure)
    ));
    invalid = plan(
        key(),
        &[(1, 0), (2, 8), (3, 0)],
        CounterInitialization::NewKeys,
    );
    let layout = BearerLayout::new(SignallingSet::Base, &[(5, CipherMode::Transparent)]).unwrap();
    invalid.downlink = ActivationSequences::new(layout, &[(1, 0), (2, 8), (3, 0)]).unwrap();
    invalid.dpch_cfn = Some(8);
    assert!(matches!(
        PendingCipher::prepare(
            key(),
            invalid,
            &protection.ciphers,
            [0; 3],
            NewSduRange { first: 0, next: 1 }
        ),
        Err(SignallingError::ConfigurationFailure)
    ));
}

#[test]
fn uncollected_plaintext_control_cannot_be_reinterpreted_by_starting_security() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = controller
        .request(&source, at(0), &crate::signalling::tests::request(0))
        .unwrap();
    let mut peer = peer(&source);
    peer.enqueue(&COMPLETE).unwrap();
    peer.publish_next(at(0), |publication| {
        controller.receive(
            key,
            at(0),
            umts_mac::CommonPdu {
                header: umts_mac::CommonHeader::Dedicated {
                    identity: umts_mac::CommonIdentity::Cell(
                        controller.assigned(key).unwrap().crnti(),
                    ),
                    channel: crate::signalling::logical(SignallingBearer::ControlAm).unwrap(),
                },
                payload: Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE)
                    .unwrap(),
            },
        )
    })
    .unwrap()
    .unwrap();
    let now = crate::connection_security::SecurityTime {
        control: umts_iuh::session::ControlTime(0),
        monotonic: std::time::Instant::now(),
    };
    assert_eq!(
        controller.publish_security_command(key, umts_iuh::Domain::Cs, now),
        Err(SignallingError::Capacity)
    );
    assert_eq!(
        controller.collect_one(key, SignallingBearer::ControlAm, at(0)),
        Ok(true)
    );
    assert_eq!(
        controller.publish_security_command(key, umts_iuh::Domain::Cs, now),
        Err(SignallingError::Capacity)
    );
    assert_eq!(controller.process_one(), Some(Ok(key)));
    assert_eq!(
        controller.publish_security_command(key, umts_iuh::Domain::Cs, now),
        Err(SignallingError::Phase)
    );
}

#[test]
fn transport_holds_new_cipher_downlink_without_consuming_rlc_or_its_prepared_sdu() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let context = controller.registry.get_mut(key).unwrap();
    context
        .nas_bearer
        .enqueue(
            key,
            Bits::new(&COMPLETE, COMPLETE.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
    let pending = PendingCipher::prepare(
        key,
        plan(
            key,
            &[(1, 0), (2, 32), (3, 0)],
            CounterInitialization::NewKeys,
        ),
        &context.protection.ciphers,
        [0; 3],
        NewSduRange { first: 0, next: 1 },
    )
    .unwrap();
    pending
        .publish_with(&mut context.protection.ciphers, || Ok(()))
        .unwrap();
    context.protection.pending_cipher = Some(pending);
    let positions = context.nas_bearer.window_positions();
    assert_eq!(
        controller.publish_bearer(key, SignallingBearer::NasAm, &mut source, at(0), at(0)),
        Err(SignallingError::Capacity)
    );
    let context = controller.registry.get_mut(key).unwrap();
    assert_eq!(context.nas_bearer.window_positions(), positions);
    assert!(context.nas_bearer.pending_new_data());
    context.protection.pending_cipher = None;
    assert_eq!(
        controller.publish_bearer(key, SignallingBearer::NasAm, &mut source, at(0), at(0)),
        Ok(true)
    );
    assert_eq!(
        controller
            .registry
            .get(key)
            .unwrap()
            .nas_bearer
            .window_positions()
            .next_transmit,
        positions.next_transmit + 1
    );
}
