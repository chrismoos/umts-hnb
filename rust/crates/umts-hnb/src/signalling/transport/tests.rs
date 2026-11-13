// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::common_tx::CommonTx;
use crate::{
    signalling::{
        CommonSignalling, IngressWork, logical,
        tests::{COMPLETE, controller, establish, peer, request},
    },
    test_support::{at, source},
};
use umts_mac::{CommonHeader, CommonIdentity, CommonPdu};
use umts_security::ciphering::CipherUse;
mod payload;
use umts_security::{
    Key,
    ciphering::CipherActivation,
    contexts::{CipherContext, CipherMaterial, KeyOrigin, KeySet},
};
use umts_types::bits::Bits;

fn cipher(key: UeKey) -> CipherContext {
    KeySet::uia1(
        KeyOrigin {
            ue: key,
            domain: umts_types::CnDomain::Cs,
            generation: std::num::NonZeroU64::MIN,
        },
        CipherMaterial::Uea1(Key::new([0x42; 16])),
        Key::new([0x35; 16]),
    )
    .split()
    .0
}

fn activate(
    controller: &mut CommonSignalling,
    key: UeKey,
    first: u64,
    directions: &[Direction],
) -> KeyOrigin {
    let cipher = cipher(key);
    let origin = cipher.origin();
    let activations = directions
        .iter()
        .map(|&direction| CipherActivation::Sequenced {
            bearer: SignallingBearer::ControlAm.identity(),
            direction,
            anchor: CountAnchor::new(
                CipherMode::Acknowledged,
                Position::Sequence(first),
                first as u32,
            )
            .unwrap(),
            first_new: Position::Sequence(first),
        })
        .collect::<Vec<_>>();
    controller
        .registry
        .get_mut(key)
        .unwrap()
        .protection
        .ciphers
        .update_with(cipher, &activations, || Ok::<_, ()>(()))
        .unwrap();
    origin
}

fn reset_pdu(source: &CommonTx) -> Vec<u8> {
    let mut peer = peer(source);
    peer.initiate_reset(at(0)).unwrap();
    peer.publish_next(at(0), |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap()
        .1
}

fn receive(
    controller: &mut CommonSignalling,
    key: UeKey,
    bytes: &[u8],
) -> Result<(), SignallingError> {
    controller.receive(
        key,
        at(0),
        CommonPdu {
            header: CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(controller.assigned(key).unwrap().crnti()),
                channel: logical(SignallingBearer::ControlAm).unwrap(),
            },
            payload: Bits::new(bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
        },
    )
}

#[test]
fn reset_rebases_only_its_ue_and_bearer_maps_with_the_actual_rlc_transition() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let first = establish(&mut controller, &source, 0);
    let second = establish(&mut controller, &source, 1);
    let mut peer = peer(&source);
    peer.initiate_reset(at(0)).unwrap();
    let mut reset = Vec::new();
    peer.publish_next(at(0), |publication| {
        reset.extend_from_slice(publication.bytes);
        Ok::<_, ()>(())
    })
    .unwrap()
    .unwrap();
    receive(&mut controller, first, &reset).unwrap();
    let positions = controller
        .registry
        .get(first)
        .unwrap()
        .control
        .window_positions();
    assert!(positions.next_transmit > 0);
    assert!(positions.next_receive > 0);
    for (key, bearer, direction, position, count) in [
        (
            first,
            SignallingBearer::ControlAm,
            Direction::Downlink,
            positions.next_transmit,
            1 << CipherMode::Acknowledged.sequence_bits(),
        ),
        (
            first,
            SignallingBearer::ControlAm,
            Direction::Uplink,
            positions.next_receive,
            1 << CipherMode::Acknowledged.sequence_bits(),
        ),
        (first, SignallingBearer::NasAm, Direction::Downlink, 0, 0),
        (
            second,
            SignallingBearer::ControlAm,
            Direction::Downlink,
            0,
            0,
        ),
    ] {
        assert_eq!(
            controller
                .registry
                .get(key)
                .unwrap()
                .protection
                .ciphers
                .selected(
                    key,
                    bearer.identity(),
                    direction,
                    Position::Sequence(position)
                ),
            Ok(CipherUse::Data {
                count,
                origin: None
            })
        );
    }
    receive(&mut controller, first, &reset).unwrap();
    assert_eq!(
        controller
            .registry
            .get(first)
            .unwrap()
            .control
            .window_positions(),
        positions
    );
    assert_eq!(
        controller.publish_bearer(
            first,
            SignallingBearer::ControlAm,
            &mut source,
            at(0),
            at(0)
        ),
        Ok(true)
    );
}

#[test]
fn configured_reset_retains_the_selected_origin_in_both_directions_and_is_idempotent() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let origin = activate(
        &mut controller,
        key,
        0,
        &[Direction::Downlink, Direction::Uplink],
    );
    let reset = reset_pdu(&source);
    receive(&mut controller, key, &reset).unwrap();
    let positions = controller
        .registry
        .get(key)
        .unwrap()
        .control
        .window_positions();
    for (direction, position) in [
        (Direction::Downlink, positions.next_transmit),
        (Direction::Uplink, positions.next_receive),
    ] {
        assert_eq!(
            controller
                .registry
                .get(key)
                .unwrap()
                .protection
                .ciphers
                .selected(key, 2, direction, Position::Sequence(position)),
            Ok(CipherUse::Data {
                count: 1 << CipherMode::Acknowledged.sequence_bits(),
                origin: Some(origin)
            })
        );
    }
    receive(&mut controller, key, &reset).unwrap();
    assert_eq!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions(),
        positions
    );
}

#[test]
fn reset_rejects_future_or_directionally_inconsistent_keys_before_rlc_commit() {
    for (first, directions) in [
        (5, &[Direction::Downlink, Direction::Uplink][..]),
        (0, &[Direction::Downlink][..]),
    ] {
        let source = source(false);
        let mut controller = controller(&source);
        let key = establish(&mut controller, &source, 0);
        let origin = activate(&mut controller, key, first, directions);
        let before = controller
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions();
        assert_eq!(
            controller.initiate_am_reset(key, SignallingBearer::ControlAm, at(0)),
            Err(SignallingError::CipherResetRequired)
        );
        assert_eq!(
            controller.registry.get(key).unwrap().control.state(),
            umts_rlc::acknowledged::AmState::Ready
        );
        assert_eq!(
            receive(&mut controller, key, &reset_pdu(&source)),
            Err(SignallingError::CipherResetRequired)
        );
        let context = controller.registry.get(key).unwrap();
        assert_eq!(context.control.window_positions(), before);
        assert!(context.protection.blocks_status());
        assert_eq!(
            context.protection.ciphers.selected(
                key,
                2,
                Direction::Downlink,
                Position::Sequence(first)
            ),
            Ok(CipherUse::Data {
                count: first as u32,
                origin: Some(origin)
            })
        );
        assert_eq!(
            controller.initiate_am_reset(key, SignallingBearer::ControlAm, at(0)),
            Err(SignallingError::Phase)
        );
    }
}

#[test]
fn reset_reports_pending_recovery_without_mutating_rlc_positions() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let context = controller.registry.get_mut(key).unwrap();
    let before = context.control.window_positions();
    context.protection.admission =
        super::super::protection::ControlAdmission::RecoveryRequired { rejected: None };
    let error = receive(&mut controller, key, &reset_pdu(&source)).unwrap_err();
    let SignallingError::CipherResetBlocked { bearer, blockers } = error else {
        panic!("expected reset admission evidence, got {error:?}");
    };
    assert_eq!(bearer, SignallingBearer::ControlAm);
    assert!(blockers.admission);
    assert!(!blockers.bearer_change);
    assert!(!blockers.cs_security);
    assert!(!blockers.ps_security);
    assert_eq!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions(),
        before
    );
}

#[test]
fn reset_cannot_bypass_queued_control_admission_or_reuse_status_submission() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    assert_eq!(
        controller.publish_bearer(key, SignallingBearer::ControlAm, &mut source, at(0), at(0)),
        Ok(true)
    );
    assert!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .status_submission(key)
            .unwrap()
            .is_some()
    );
    controller
        .ingress
        .admit(key, SignallingBearer::ControlAm, 1, COMPLETE.len())
        .unwrap();
    controller
        .ingress
        .push_admitted(key, SignallingBearer::ControlAm, at(0), &COMPLETE);
    assert_eq!(
        controller.initiate_am_reset(key, SignallingBearer::ControlAm, at(0)),
        Err(SignallingError::Capacity)
    );
    controller.ingress.cancel_for(key);
    receive(&mut controller, key, &reset_pdu(&source)).unwrap();
    assert!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .status_submission(key)
            .unwrap()
            .is_none()
    );
    assert!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .published_status(key)
            .unwrap()
            .is_none()
    );
}

#[test]
fn local_reset_validates_bearer_clock_and_ue_generation_before_mutating_rlc() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = establish(&mut controller, &source, 0);
    let before = controller
        .registry
        .get(key)
        .unwrap()
        .control
        .window_positions();
    assert_eq!(
        controller.initiate_am_reset(key, SignallingBearer::Unacknowledged, at(0)),
        Err(SignallingError::Mapping)
    );
    let mut stale = key;
    stale.generation = std::num::NonZeroU64::new(key.generation.get() + 1).unwrap();
    assert_eq!(
        controller.initiate_am_reset(stale, SignallingBearer::ControlAm, at(0)),
        Err(SignallingError::Origin)
    );
    let mut wrong_clock = at(0);
    wrong_clock.epoch = umts_types::RadioEpoch::new(
        std::num::NonZeroU64::new(wrong_clock.epoch.get() + 1).unwrap(),
    );
    assert!(
        controller
            .initiate_am_reset(key, SignallingBearer::ControlAm, wrong_clock)
            .is_err()
    );
    assert_eq!(
        controller
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions(),
        before
    );
    assert_eq!(
        controller.registry.get(key).unwrap().control.state(),
        umts_rlc::acknowledged::AmState::Ready
    );
    controller
        .initiate_am_reset(key, SignallingBearer::ControlAm, at(0))
        .unwrap();
    assert_eq!(
        controller.registry.get(key).unwrap().control.state(),
        umts_rlc::acknowledged::AmState::ResetPending
    );
}

#[test]
fn collection_services_one_candidate_per_call_and_advances_past_blocked_ues() {
    let source = source(false);
    let mut controller = controller(&source);
    let first = controller.request(&source, at(0), &request(0)).unwrap();
    let second = controller.request(&source, at(0), &request(1)).unwrap();
    for key in [first, second] {
        let mut peer = peer(&source);
        peer.enqueue(&COMPLETE).unwrap();
        peer.publish_next(at(0), |publication| {
            receive(&mut controller, key, publication.bytes)
        })
        .unwrap()
        .unwrap();
    }
    let capacity = umts_rlc::acknowledged::MAX_DELIVERY_SDUS;
    for _ in 0..capacity {
        controller
            .ingress
            .admit(first, SignallingBearer::ControlAm, 1, COMPLETE.len())
            .unwrap();
        controller
            .ingress
            .push_admitted(first, SignallingBearer::ControlAm, at(0), &COMPLETE);
    }
    assert_eq!(
        controller.collect_ingress_one(at(0)),
        Ok(IngressWork::Blocked {
            key: first,
            bearer: SignallingBearer::ControlAm
        })
    );
    assert_eq!(controller.collect_ingress_one(at(0)), Ok(IngressWork::Idle));
    assert_eq!(
        controller.collect_ingress_one(at(0)),
        Ok(IngressWork::Blocked {
            key: second,
            bearer: SignallingBearer::ControlAm
        })
    );
    controller.ingress.cancel_for(first);
    for _ in 0..(2 * controller.registry.capacity() - 3) {
        assert_eq!(controller.collect_ingress_one(at(0)), Ok(IngressWork::Idle));
    }
    assert_eq!(
        controller.collect_ingress_one(at(0)),
        Ok(IngressWork::Collected {
            key: first,
            bearer: SignallingBearer::ControlAm
        })
    );
    assert_eq!(controller.collect_ingress_one(at(0)), Ok(IngressWork::Idle));
    assert_eq!(
        controller.collect_ingress_one(at(0)),
        Ok(IngressWork::Collected {
            key: second,
            bearer: SignallingBearer::ControlAm
        })
    );
}
