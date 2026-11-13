// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_control::tests::{enqueue_sdu, ingress as common_ingress},
    common_tx::tests::recover_current_fach,
    signalling::tests::{INITIAL, controller, establish, finish_registration, peer, send},
    test_support::{at, count, failed_cell_update, source},
};
use umts_mac::{FddCommonChannel, decode_common};
use umts_rlc::unacknowledged::UmReceiver;
use umts_rrc_codec::decode_downlink_common;

#[test]
fn common_ingress_retries_connection_admission_with_original_receive_time_and_no_partial_ue() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let mut input = common_ingress(2);
    for _ in 0..4 {
        controller.rb0.enqueue(Bits::new(&[0], 8).unwrap()).unwrap();
    }
    let request = crate::signalling::tests::request(0);
    let bytes = rasn::uper::encode(request.value.as_ref()).unwrap();
    enqueue_sdu(&mut input, &bytes, request.consumed_bits, at(123));
    let resources = controller.resources();
    for _ in 0..4 {
        let step = controller
            .service_common_one(&source, &mut input, TransactionId::new(0).unwrap())
            .unwrap();
        assert_eq!(step.transport.start, at(123));
        assert_eq!(step.work, CommonIngressWork::Deferred);
        assert_eq!(controller.counters().created, 0);
        assert_eq!(controller.resources(), resources);
        assert_eq!(input.pending(), 1);
        assert_eq!(input.counters().decoded, 1);
    }
    drain_common(&mut controller, &mut source);
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(0).unwrap())
        .unwrap();
    let CommonIngressWork::ConnectionRequested(key) = step.work else {
        panic!("connection request was not admitted: {:?}", step.work);
    };
    assert_eq!(step.transport.start, at(123));
    assert_eq!(controller.assigned(key).unwrap().srnti(), 1);
    assert_eq!(controller.counters().created, 1);
    assert_eq!(input.pending(), 0);
    assert!(
        controller
            .service_common_one(&source, &mut input, TransactionId::new(0).unwrap())
            .is_none()
    );
}

#[test]
fn common_ingress_routes_full_urnti_and_retries_release_without_touching_another_ue() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let other = establish(&mut controller, &source, 1);
    let assigned = controller.assigned(key).unwrap();
    assert_eq!(controller.key_for_urnti(assigned.urnti()), Some(key));
    let mut input = common_ingress(2);
    let bytes = encoded(&controller, key);
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(123));
    for _ in 0..4 {
        controller
            .core
            .push_radio_failure(
                UeKey {
                    slot: key.slot + 100,
                    ..key
                },
                Some(CoreContextId::new(999).unwrap()),
                false,
                [None; 2],
            )
            .unwrap();
    }
    for _ in 0..4 {
        let step = controller
            .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
            .unwrap();
        assert_eq!(step.work, CommonIngressWork::Deferred);
        assert_eq!(input.pending(), 1);
        assert_eq!(input.counters().decoded, 1);
        assert_eq!(controller.key_for_urnti(assigned.urnti()), Some(key));
        assert!(controller.assigned(other).is_some());
    }
    controller.core.clear();
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
        .unwrap();
    assert!(
        matches!(step.work, CommonIngressWork::ConnectionReleased { key: actual, .. } if actual == key)
    );
    assert_eq!(step.transport.start, at(123));
    assert_eq!(controller.key_for_urnti(assigned.urnti()), None);
    assert_eq!(input.pending(), 0);
    assert!(controller.assigned(other).is_some());
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(234));
    assert_eq!(
        controller
            .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
            .unwrap()
            .work,
        CommonIngressWork::UnknownContextReleaseQueued {
            urnti: assigned.urnti()
        }
    );
    assert_eq!(input.pending(), 0);
    assert!(controller.assigned(other).is_some());
}

#[test]
fn common_ingress_releases_foreign_identity_but_rejects_known_unsupported_update() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let assigned = controller.assigned(key).unwrap();
    let mut input = common_ingress(2);
    let foreign =
        AssignedRntis::new(assigned.rnc() + 1, assigned.srnti(), assigned.crnti()).unwrap();
    let mut update = failed_cell_update(foreign);
    let bytes = rasn::uper::encode(&update).unwrap();
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(123));
    assert_eq!(
        controller
            .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
            .unwrap()
            .work,
        CommonIngressWork::UnknownContextReleaseQueued {
            urnti: foreign.urnti()
        }
    );
    update = failed_cell_update(assigned);
    let ULCCCHMessageType::cellUpdate(body) = &mut update.message else {
        unreachable!()
    };
    body.am_rlc_error_indication_rb2_3or4 = false;
    let bytes = rasn::uper::encode(&update).unwrap();
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(234));
    // TS 25.331 8.3.1.5: a known UE still on common channels gets CELL UPDATE CONFIRM.
    let work = controller
        .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
        .unwrap()
        .work;
    assert!(
        matches!(work, CommonIngressWork::UpdateConfirmed { key: confirmed, .. } if confirmed == key),
        "{work:?}"
    );
    assert_eq!(input.pending(), 0);
    assert_eq!(controller.key_for_urnti(assigned.urnti()), Some(key));
    assert!(controller.assigned(key).is_some());
}

#[test]
fn unknown_context_release_retries_full_queue_and_preserves_live_ue_and_core() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let assigned = controller.assigned(key).unwrap();
    drain_common(&mut controller, &mut source);
    let foreign =
        AssignedRntis::new(assigned.rnc() + 1, assigned.srnti(), assigned.crnti()).unwrap();
    let bytes = rasn::uper::encode(&failed_cell_update(foreign)).unwrap();
    let mut input = common_ingress(2);
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(123));
    for _ in 0..controller.limits.queued_sdus_per_bearer.get() {
        controller.rb0.enqueue(Bits::new(&[0], 8).unwrap()).unwrap();
    }
    let resources = controller.resources();
    let created = controller.counters().created;
    for _ in 0..4 {
        let step = controller
            .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
            .unwrap();
        assert_eq!(step.work, CommonIngressWork::Deferred);
        assert_eq!(input.pending(), 1);
        assert_eq!(controller.resources(), resources);
        assert_eq!(controller.assigned(key), Some(assigned));
        assert!(controller.core.is_empty());
    }
    drain_common(&mut controller, &mut source);
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
        .unwrap();
    assert_eq!(
        step.work,
        CommonIngressWork::UnknownContextReleaseQueued {
            urnti: foreign.urnti()
        }
    );
    assert_eq!(input.pending(), 0);
    assert_eq!(controller.resources(), resources);
    assert_eq!(controller.counters().created, created);
    assert_eq!(controller.assigned(key), Some(assigned));
    assert!(controller.core.is_empty());
    let expected =
        build_unknown_context_release(foreign.urnti(), TransactionId::new(2).unwrap()).unwrap();
    let received = drain_common(&mut controller, &mut source);
    assert_eq!(received.len(), 1);
    let decoded = decode_downlink_common(&received[0], received[0].len() * BITS_PER_BYTE).unwrap();
    assert!(decoded.value.integrity_check_info.is_none());
    assert_eq!(
        decoded.value,
        decode_downlink_common(&expected.bytes, expected.bit_length)
            .unwrap()
            .value
    );
}

#[test]
fn protocol_error_request_replaces_existing_context_with_fresh_setup() {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let assigned = controller.assigned(key).unwrap();
    drain_common(&mut controller, &mut source);
    let mut request = crate::signalling::tests::request(0);
    let ULCCCHMessageType::rrcConnectionRequest(body) = &mut request.value.message else {
        unreachable!()
    };
    body.protocol_error_indicator =
        umts_rrc_schema::information_elements::ProtocolErrorIndicator::errorOccurred;
    let bytes = rasn::uper::encode(request.value.as_ref()).unwrap();
    let mut input = common_ingress(2);
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(123));
    let counters = controller.counters();
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
        .unwrap();
    let CommonIngressWork::ConnectionRequested(fresh) = step.work else {
        panic!(
            "protocol error request must be served with a setup, got {:?}",
            step.work
        )
    };
    assert_ne!(fresh, key);
    assert_eq!(controller.assigned(key), None);
    assert_ne!(controller.assigned(fresh), Some(assigned));
    assert_eq!(controller.counters().retired, counters.retired + 1);
    assert_eq!(controller.counters().created, counters.created + 1);
    assert_eq!(input.pending(), 0);
    let received = drain_common(&mut controller, &mut source);
    assert_eq!(received.len(), 1);
    let decoded = decode_downlink_common(&received[0], received[0].len() * BITS_PER_BYTE).unwrap();
    assert!(matches!(
        decoded.value.message,
        umts_rrc_schema::class_definitions::DLCCCHMessageType::rrcConnectionSetup(_)
    ));
}

#[test]
fn first_protocol_error_request_is_served_with_a_setup() {
    let source = source(false);
    let mut controller = controller(&source);
    let mut request = crate::signalling::tests::request(0);
    let ULCCCHMessageType::rrcConnectionRequest(body) = &mut request.value.message else {
        unreachable!()
    };
    body.protocol_error_indicator =
        umts_rrc_schema::information_elements::ProtocolErrorIndicator::errorOccurred;
    let bytes = rasn::uper::encode(request.value.as_ref()).unwrap();
    let mut input = common_ingress(2);
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(123));
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(2).unwrap())
        .unwrap();
    assert!(matches!(
        step.work,
        CommonIngressWork::ConnectionRequested(_)
    ));
    assert_eq!(controller.counters().created, 1);
    assert_eq!(input.pending(), 0);
}

fn registered(controller: &mut CommonSignalling, source: &CommonTx) -> UeKey {
    let key = establish(controller, source, 0);
    let mut peer = peer(source);
    send(
        controller,
        key,
        SignallingBearer::NasAm,
        &mut peer,
        &INITIAL,
    );
    finish_registration(controller, key);
    assert_eq!(controller.process_one(), Some(Ok(key)));
    controller.core.clear();
    let mut initial = INITIAL;
    initial[1] |= 0x80;
    send(
        controller,
        key,
        SignallingBearer::NasAm,
        &mut peer,
        &initial,
    );
    assert_eq!(controller.process_one(), Some(Ok(key)));
    controller.core.clear();
    key
}

fn encoded(controller: &CommonSignalling, key: UeKey) -> Vec<u8> {
    rasn::uper::encode(&failed_cell_update(controller.assigned(key).unwrap())).unwrap()
}

fn drain_common(controller: &mut CommonSignalling, source: &mut CommonTx) -> Vec<Vec<u8>> {
    let mut peer = UmReceiver::new(
        UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        },
        UmLimits {
            maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(source, 1).unwrap(),
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let mut samples = vec![num_complex::Complex32::default(); source.samples_per_slot()];
    let mut received = Vec::new();
    for _ in 0..64 {
        if !controller.rb0.has_pending() {
            return received;
        }
        let start = source.next_chip();
        assert!(controller.publish_rb0(source, start).unwrap());
        for slot in 0..15 {
            source.generate_slot(&mut samples).unwrap();
            if slot == 0 {
                let bytes = recover_current_fach(source);
                let mac = decode_common(
                    FddCommonChannel::Fach,
                    Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap();
                assert_eq!(mac.header, CommonHeader::CommonControl);
                let mut bytes = vec![0; mac.payload.len() / BITS_PER_BYTE];
                mac.payload.copy_to(&mut bytes).unwrap();
                received.extend(peer.receive(&bytes).unwrap().sdus().map(<[u8]>::to_vec));
            }
        }
    }
    panic!("bounded common release publication");
}

#[test]
fn rlc_error_update_admits_release_core_teardown_and_only_its_ue_retirement() {
    for flags in [(true, false), (false, true), (true, true)] {
        release_error_update(flags);
    }
}

fn release_error_update(flags: (bool, bool)) {
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let other = establish(&mut controller, &source, 1);
    let assigned = controller.assigned(key).unwrap();
    let connections = [
        controller.connection(key, umts_types::CnDomain::Cs),
        controller.connection(key, umts_types::CnDomain::Ps),
    ];
    assert!(connections.iter().all(Option::is_some));
    let mut message = failed_cell_update(assigned);
    let ULCCCHMessageType::cellUpdate(body) = &mut message.message else {
        unreachable!()
    };
    body.am_rlc_error_indication_rb2_3or4 = flags.0;
    body.am_rlc_error_indication_rb5or_above = flags.1;
    let bytes = rasn::uper::encode(&message).unwrap();
    let update = controller
        .release_failed_rlc_update(
            key,
            at(0),
            &bytes,
            bytes.len() * BITS_PER_BYTE,
            TransactionId::new(2).unwrap(),
        )
        .unwrap();
    assert_eq!(update.signalling_rlc_error, flags.0);
    assert_eq!(update.user_rlc_error, flags.1);
    assert_eq!(controller.assigned(key), None);
    assert!(controller.assigned(other).is_some());
    controller
        .core
        .publish_one(|event| {
            assert_eq!(event.key, key);
            let CoreEventKind::RadioFailure {
                connections: actual,
                ..
            } = event.kind
            else {
                panic!("radio failure")
            };
            assert_eq!(actual, connections);
            Ok::<_, ()>(())
        })
        .unwrap()
        .unwrap();
    let expected =
        build_signalling_failure_release(assigned, TransactionId::new(2).unwrap()).unwrap();
    let mut releases = 0;
    for bytes in drain_common(&mut controller, &mut source) {
        let decoded = decode_downlink_common(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
        if matches!(
            decoded.value.message,
            umts_rrc_schema::class_definitions::DLCCCHMessageType::rrcConnectionRelease(_)
        ) {
            assert_eq!(
                decoded.value,
                decode_downlink_common(&expected.bytes, expected.bit_length)
                    .unwrap()
                    .value
            );
            releases += 1;
        }
    }
    assert_eq!(releases, 1);
    assert_eq!(
        controller.release_failed_rlc_update(
            key,
            at(0),
            &bytes,
            bytes.len() * BITS_PER_BYTE,
            TransactionId::new(2).unwrap()
        ),
        Err(SignallingError::Origin)
    );
}

#[test]
fn release_backpressure_identity_and_non_failure_updates_cannot_partially_retire_a_ue() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let bytes = encoded(&controller, key);
    let before = controller.rb0.pending_sdus();
    for _ in 0..controller.limits.core_entries.get() {
        controller
            .core
            .push_radio_failure(
                UeKey {
                    slot: key.slot + 1,
                    ..key
                },
                Some(CoreContextId::new(999).unwrap()),
                false,
                [None; 2],
            )
            .unwrap();
    }
    assert_eq!(
        controller.release_failed_rlc_update(
            key,
            at(0),
            &bytes,
            bytes.len() * BITS_PER_BYTE,
            TransactionId::new(0).unwrap()
        ),
        Err(SignallingError::Capacity)
    );
    assert!(controller.assigned(key).is_some());
    assert_eq!(controller.rb0.pending_sdus(), before);
    controller.core.clear();
    let mut message = failed_cell_update(controller.assigned(key).unwrap());
    let ULCCCHMessageType::cellUpdate(update) = &mut message.message else {
        unreachable!()
    };
    update.am_rlc_error_indication_rb2_3or4 = false;
    update.am_rlc_error_indication_rb5or_above = false;
    let normal = rasn::uper::encode(&message).unwrap();
    assert_eq!(
        controller.release_failed_rlc_update(
            key,
            at(0),
            &normal,
            normal.len() * BITS_PER_BYTE,
            TransactionId::new(0).unwrap()
        ),
        Err(SignallingError::Message)
    );
    assert!(controller.core.is_empty());
    assert!(controller.assigned(key).is_some());
    let mut wrong = key;
    wrong.generation = NonZeroU64::new(key.generation.get() + 1).unwrap();
    assert_eq!(
        controller.release_failed_rlc_update(
            wrong,
            at(0),
            &bytes,
            bytes.len() * BITS_PER_BYTE,
            TransactionId::new(0).unwrap()
        ),
        Err(SignallingError::Origin)
    );
    while controller.rb0.pending_sdus() < controller.limits.queued_sdus_per_bearer.get() {
        controller
            .rb0
            .enqueue(Bits::new(&[0x55], 8).unwrap())
            .unwrap();
    }
    assert!(
        controller
            .release_failed_rlc_update(
                key,
                at(0),
                &bytes,
                bytes.len() * BITS_PER_BYTE,
                TransactionId::new(0).unwrap()
            )
            .is_err()
    );
    assert!(controller.core.is_empty());
    assert!(controller.assigned(key).is_some());
}

fn plain_cell_update(
    controller: &CommonSignalling,
    key: UeKey,
    cause: umts_rrc_schema::information_elements::CellUpdateCause,
) -> Vec<u8> {
    let mut message = failed_cell_update(controller.assigned(key).unwrap());
    let ULCCCHMessageType::cellUpdate(update) = &mut message.message else {
        unreachable!()
    };
    update.am_rlc_error_indication_rb2_3or4 = false;
    update.am_rlc_error_indication_rb5or_above = false;
    update.cell_update_cause = cause;
    rasn::uper::encode(&message).unwrap()
}

fn drain_dcch_um(
    controller: &mut CommonSignalling,
    source: &mut CommonTx,
    key: UeKey,
) -> Vec<Vec<u8>> {
    let (pdu_bytes, _) = CommonAmBearer::sizes(source, 1).unwrap();
    let mut peer = UmReceiver::new(
        UmConfig {
            li_width: LiWidth::Seven,
            interpretation: EInterpretation::Normal,
        },
        UmLimits {
            maximum_pdu_bytes: pdu_bytes,
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(4),
            maximum_buffer_bytes: 1024 * 1024,
        },
    )
    .unwrap();
    let mut samples = vec![num_complex::Complex32::default(); source.samples_per_slot()];
    let mut received = Vec::new();
    for _ in 0..64 {
        let start = source.next_chip();
        let published = controller
            .publish_bearer(key, SignallingBearer::Unacknowledged, source, start, start)
            .unwrap();
        if !published {
            return received;
        }
        for slot in 0..15 {
            source.generate_slot(&mut samples).unwrap();
            if slot == 0 {
                let bytes = recover_current_fach(source);
                let mac = decode_common(
                    FddCommonChannel::Fach,
                    Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap();
                assert!(
                    matches!(mac.header, umts_mac::CommonHeader::Dedicated { .. }),
                    "{:?}",
                    mac.header
                );
                let mut bytes = vec![0; mac.payload.len() / BITS_PER_BYTE];
                mac.payload.copy_to(&mut bytes).unwrap();
                received.extend(peer.receive(&bytes).unwrap().sdus().map(<[u8]>::to_vec));
            }
        }
    }
    panic!("bounded DCCH publication");
}

fn confirms(
    controller: &mut CommonSignalling,
    source: &mut CommonTx,
    key: UeKey,
) -> Vec<umts_rrc_schema::pdu_definitions::CellUpdateConfirmR3IEs> {
    drain_dcch_um(controller, source, key)
        .into_iter()
        .filter_map(|bytes| {
            let decoded =
                umts_rrc_codec::decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE)
                    .unwrap();
            match decoded.value.message {
                umts_rrc_schema::class_definitions::DLDCCHMessageType::cellUpdateConfirm(
                    umts_rrc_schema::pdu_definitions::CellUpdateConfirm::r3(body),
                ) => Some(body.cell_update_confirm_r3),
                _ => None,
            }
        })
        .collect()
}

fn cell_update(controller: &mut CommonSignalling, key: UeKey, bytes: &[u8]) -> CommonIngressWork {
    controller
        .handle_cell_update(
            key,
            at(0),
            bytes,
            bytes.len() * BITS_PER_BYTE,
            TransactionId::new(0).unwrap(),
        )
        .unwrap()
}

#[test]
fn full_um_queue_retains_cell_update_and_confirms_after_capacity_returns() {
    use umts_rrc_schema::information_elements::CellUpdateCause;
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let bytes = plain_cell_update(&controller, key, CellUpdateCause::periodicalCellUpdate);
    for _ in 0..4 {
        controller
            .registry
            .get_mut(key)
            .unwrap()
            .um
            .enqueue(key, Bits::new(&[0], 8).unwrap())
            .unwrap();
    }
    let mut input = common_ingress(2);
    enqueue_sdu(&mut input, &bytes, bytes.len() * BITS_PER_BYTE, at(123));
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(0).unwrap())
        .unwrap();
    assert_eq!(step.work, CommonIngressWork::Deferred);
    assert_eq!(input.pending(), 1);
    drain_dcch_um(&mut controller, &mut source, key);
    let step = controller
        .service_common_one(&source, &mut input, TransactionId::new(0).unwrap())
        .unwrap();
    assert!(
        matches!(step.work, CommonIngressWork::UpdateConfirmed { key: actual, .. } if actual == key)
    );
    assert_eq!(input.pending(), 0);
    assert_eq!(confirms(&mut controller, &mut source, key).len(), 1);
}

#[test]
fn periodic_cell_update_on_common_channels_is_confirmed_and_keeps_the_ue() {
    use umts_rrc_schema::information_elements::{CellUpdateCause, RRCStateIndicator};
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let assigned = controller.assigned(key).unwrap();
    let bytes = plain_cell_update(&controller, key, CellUpdateCause::periodicalCellUpdate);
    let work = cell_update(&mut controller, key, &bytes);
    let CommonIngressWork::UpdateConfirmed {
        key: confirmed,
        update,
    } = work
    else {
        panic!("confirm expected, got {work:?}")
    };
    assert_eq!(confirmed, key);
    assert_eq!(update.cause, umts_rrc::mobility::UpdateCause::Periodic);
    assert!(controller.core.is_empty());
    assert_eq!(controller.assigned(key), Some(assigned));
    let first = confirms(&mut controller, &mut source, key);
    assert_eq!(first.len(), 1);
    let ies = &first[0];
    assert_eq!(ies.rrc_state_indicator, RRCStateIndicator::cell_FACH);
    assert!(ies.new_c_rnti.is_none() && ies.new_u_rnti.is_none());
    assert!(
        !ies.rlc_re_establish_indicator_rb2_3or4 && !ies.rlc_re_establish_indicator_rb5or_above
    );
    // A later update gets the next transaction identifier of its own message type.
    let bytes = plain_cell_update(&controller, key, CellUpdateCause::uplinkDataTransmission);
    cell_update(&mut controller, key, &bytes);
    let second = confirms(&mut controller, &mut source, key);
    assert_eq!(second.len(), 1);
    assert_eq!(
        second[0].rrc_transaction_identifier.0,
        (ies.rrc_transaction_identifier.0 + 1) % 4
    );
    assert!(controller.assigned(key).is_some());
}

#[test]
fn re_entered_service_update_keeps_the_crnti_and_needs_no_response() {
    use umts_rrc_schema::information_elements::CellUpdateCause;
    let mut source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let bytes = plain_cell_update(&controller, key, CellUpdateCause::re_enteredServiceArea);
    cell_update(&mut controller, key, &bytes);
    let first = confirms(&mut controller, &mut source, key);
    assert_eq!(first.len(), 1);
    assert!(first[0].new_c_rnti.is_none() && first[0].new_u_rnti.is_none());
    assert!(controller.assigned(key).is_some());
}

#[test]
fn rlc_error_update_still_releases_through_the_cell_update_entry() {
    let source = source(false);
    let mut controller = controller(&source);
    let key = registered(&mut controller, &source);
    let bytes = encoded(&controller, key);
    let work = controller
        .handle_cell_update(
            key,
            at(0),
            &bytes,
            bytes.len() * BITS_PER_BYTE,
            TransactionId::new(2).unwrap(),
        )
        .unwrap();
    assert!(
        matches!(work, CommonIngressWork::ConnectionReleased { key: released, .. } if released == key)
    );
    assert_eq!(controller.assigned(key), None);
}

#[test]
fn request_beyond_capacity_is_rejected_with_congestion_and_a_wait_time() {
    use umts_rrc_schema::class_definitions::DLCCCHMessageType;
    let mut source = source(false);
    let mut controller = controller(&source);
    let capacity = controller.limits.maximum_ues.get() as u8;
    for variant in 0..capacity {
        establish(&mut controller, &source, variant);
        // Flush each setup so the CCCH queue, not the UE registry, never limits admission.
        drain_common(&mut controller, &mut source);
    }
    let mut input = common_ingress(2);
    let mut bytes = super::super::tests::REQUEST;
    bytes[7] = capacity;
    enqueue_sdu(&mut input, &bytes, 78, at(500));
    let work = controller
        .service_common_one(&source, &mut input, TransactionId::new(1).unwrap())
        .unwrap()
        .work;
    assert_eq!(
        work,
        CommonIngressWork::ConnectionRejected {
            wait_time_seconds: CONGESTION_WAIT_TIME_SECONDS
        }
    );
    let mut rejects = 0;
    for bytes in drain_common(&mut controller, &mut source) {
        let decoded = decode_downlink_common(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
        let DLCCCHMessageType::rrcConnectionReject(
            umts_rrc_schema::pdu_definitions::RRCConnectionReject::r3(reject),
        ) = decoded.value.message
        else {
            continue;
        };
        let ies = reject.rrc_connection_reject_r3;
        assert_eq!(
            ies.rejection_cause,
            umts_rrc_schema::information_elements::RejectionCause::congestion
        );
        assert_eq!(ies.wait_time.0, CONGESTION_WAIT_TIME_SECONDS);
        assert_eq!(ies.rrc_transaction_identifier.0, 1);
        rejects += 1;
    }
    assert_eq!(rejects, 1);
}
