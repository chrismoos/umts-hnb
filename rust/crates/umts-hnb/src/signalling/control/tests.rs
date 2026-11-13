// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    connection_security::SecurityTime,
    signalling::tests::{
        COMPLETE, INITIAL, controller, establish, finish_registration, peer, send,
    },
    test_support::{at, integrity, rel99 as fixture, source},
    tx_evidence,
};
use std::{sync::Arc, time::Instant};
use umts_iuh::session::ControlTime;
use umts_rlc::acknowledged::AmEntity;
use umts_rrc::dedicated::{DchTfcs, InitialDchSetup, Rel99Configuration, SetupPhase};
use umts_rrc_schema::{class_definitions::ULDCCHMessage, pdu_definitions as pdu};
use umts_security::{
    Key,
    contexts::{CipherMaterial, IntegrityContext, KeySet},
};
use umts_types::time::SampleInstant;

pub(super) mod additional;
mod frames;
mod recovery;
pub(super) mod release;
mod reports;
mod routes;
mod unprotected;
pub(super) mod user_setup;

pub(super) fn security_time() -> SecurityTime {
    SecurityTime {
        control: ControlTime(0),
        monotonic: Instant::now(),
    }
}
pub(super) fn ready(compact: bool) -> (CommonTx, CommonSignalling, UeKey, AmEntity) {
    let source = source(compact);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    send(
        &mut radio,
        key,
        SignallingBearer::NasAm,
        &mut peer(&source),
        &INITIAL,
    );
    finish_registration(&mut radio, key);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    radio
        .core()
        .publish_one(|_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let mut sender = peer(&source);
    sender.enqueue(&COMPLETE).unwrap();
    sender
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    (source, radio, key, sender)
}
pub(super) fn configuration(retained: &[RetainedBearer]) -> Arc<Rel99Configuration> {
    configuration_for(retained, false)
}
fn configuration_for(retained: &[RetainedBearer], packet: bool) -> Arc<Rel99Configuration> {
    let (ul, dl, mut bearers, physical) = fixture::components(true, packet, 10);
    for bearer in &mut bearers {
        if let Some(previous) = retained.iter().find(|previous| previous.id == bearer.id) {
            bearer.rlc = previous.rlc.clone();
        }
    }
    let rebuild = |tfcs: &DchTfcs, direction, size| {
        let mut transports = tfcs.transports().to_vec();
        for group in &mut transports.last_mut().unwrap().groups {
            group.rlc_bits = size;
        }
        DchTfcs::new(direction, transports, tfcs.combinations().to_vec()).unwrap()
    };
    Arc::new(
        Rel99Configuration::new(
            rebuild(&ul, DchDirection::Uplink, retained[1].uplink_pdu_bits),
            rebuild(&dl, DchDirection::Downlink, retained[1].downlink_pdu_bits),
            bearers,
            physical,
        )
        .unwrap(),
    )
}
fn procedure(radio: &CommonSignalling, key: UeKey) -> RadioBearerSetupProcedure {
    procedure_at(radio, key, at(0))
}
pub(in crate::signalling) fn procedure_at(
    radio: &CommonSignalling,
    key: UeKey,
    now: ChipInstant,
) -> RadioBearerSetupProcedure {
    let retained = radio.retained_signalling(key).unwrap();
    RadioBearerSetupProcedure::initial(
        key,
        radio.next_bearer_setup_transaction(key).unwrap(),
        InitialDchSetup::new(
            configuration(&retained),
            retained,
            fixture::rabs(true, false),
        )
        .unwrap(),
        now,
        ChipInstant {
            index: now.index + 1000,
            ..now
        },
        NonZeroU64::new(1_000_000).unwrap(),
    )
    .unwrap()
}
pub(super) fn complete(transaction: TransactionId) -> ULDCCHMessageType {
    ULDCCHMessageType::radioBearerSetupComplete(pdu::RadioBearerSetupComplete::new(
        ie::RRCTransactionIdentifier(transaction.get()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ))
}
fn clear(body: ULDCCHMessageType) -> Vec<u8> {
    rasn::uper::encode(&ULDCCHMessage::new(None, body)).unwrap()
}
fn uplink(
    radio: &mut CommonSignalling,
    key: UeKey,
    sender: &mut AmEntity,
    bytes: &[u8],
    received: u64,
) {
    uplink_on(
        radio,
        key,
        sender,
        bytes,
        received,
        umts_rlc::acknowledged::ReceiveProvenance::Unspecified,
    )
}
pub(super) fn uplink_on(
    radio: &mut CommonSignalling,
    key: UeKey,
    sender: &mut AmEntity,
    bytes: &[u8],
    received: u64,
    provenance: umts_rlc::acknowledged::ReceiveProvenance,
) {
    uplink_on_air(radio, key, sender, bytes, received, provenance, None)
}
#[allow(clippy::too_many_arguments)]
pub(super) fn uplink_on_air(
    radio: &mut CommonSignalling,
    key: UeKey,
    sender: &mut AmEntity,
    bytes: &[u8],
    received: u64,
    provenance: umts_rlc::acknowledged::ReceiveProvenance,
    air: Option<ChipInstant>,
) {
    sender.enqueue(bytes).unwrap();
    for _ in 0..64 {
        if !sender.pending_new_data() {
            break;
        }
        sender
            .publish_next(at(received), |publication| {
                let mut time = ReceiveTime::new(at(received), at(received + 10))
                    .unwrap()
                    .with_provenance(provenance);
                if let Some(air) = air {
                    time = time.with_air_start(air).unwrap();
                }
                radio.receive_srb_payload(
                    key,
                    SignallingBearer::ControlAm,
                    time,
                    Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE).unwrap(),
                )
            })
            .unwrap()
            .unwrap();
    }
    assert!(!sender.pending_new_data());
    assert_eq!(
        radio.collect_one(key, SignallingBearer::ControlAm, at(received + 10)),
        Ok(true)
    );
}

#[test]
fn bound_completion_requires_exact_new_configuration_even_after_dispatch_delay() {
    use crate::dedicated_rx::ReceiveIdentity;
    use umts_rlc::acknowledged::ReceiveProvenance;
    let new = NonZeroU64::new(2).unwrap();
    for provenance in [
        ReceiveProvenance::Unspecified,
        ReceiveProvenance::Mixed,
        ReceiveProvenance::Path(NonZeroU64::MIN),
        ReceiveProvenance::Path(new),
    ] {
        let (_source, mut radio, key, mut sender) = ready(false);
        let mut procedure = procedure(&radio, key);
        let receipt = radio
            .publish_bearer_setup_on(
                &mut procedure,
                ReceiveIdentity {
                    ue: key,
                    configuration: new,
                },
                at(0),
            )
            .unwrap();
        publish(&mut radio, key, receipt, &mut sender);
        uplink_on(
            &mut radio,
            key,
            &mut sender,
            &clear(complete(procedure.transaction())),
            100,
            provenance,
        );
        let expected = if provenance == ReceiveProvenance::Path(new) {
            Some(Ok(key))
        } else {
            Some(Err(SignallingError::Mapping))
        };
        assert_eq!(
            radio.process_one_at_with_control(at(150), security_time(), |message, mut security| {
                assert_eq!(message.time().provenance(), provenance);
                assert_eq!(message.time().received(), at(100));
                assert_eq!(message.time().serviced(), at(150));
                message
                    .bearer_setup_response(&mut procedure, &mut security)
                    .map(|_| ())
            }),
            expected
        );
        assert_eq!(
            procedure.phase(),
            if expected == Some(Ok(key)) {
                SetupPhase::Complete
            } else {
                SetupPhase::AwaitingComplete
            }
        );
    }
}

#[test]
fn bound_setup_rejects_foreign_ue_before_publication() {
    let (_source, mut radio, key, _) = ready(false);
    let mut procedure = procedure(&radio, key);
    let mut foreign = key;
    foreign.slot += 1;
    assert_eq!(
        radio.publish_bearer_setup_on(
            &mut procedure,
            crate::dedicated_rx::ReceiveIdentity {
                ue: foreign,
                configuration: NonZeroU64::MIN
            },
            at(0)
        ),
        Err(SignallingError::Origin)
    );
    assert_eq!(procedure.phase(), SetupPhase::Prepared);
    assert_eq!(procedure.receipt(), None);
}
pub(super) fn publish(
    radio: &mut CommonSignalling,
    key: UeKey,
    receipt: SduReceipt,
    sender: &mut AmEntity,
) {
    let first = SampleInstant {
        epoch: key.epoch,
        index: 0,
    };
    let (reader, _writer) = tx_evidence::channel(first).unwrap();
    let transport = reader.interval_receipt(at(0), at(38_400), 2, 0).unwrap();
    let bytes = radio
        .registry
        .get(key)
        .unwrap()
        .control
        .configuration()
        .transmit_pdu_bytes;
    for _ in 0..128 {
        if matches!(
            radio.bearer_setup_transmission(key, receipt).unwrap(),
            SduTransmission::InFlight {
                delimited: true,
                ..
            } | SduTransmission::Acknowledged
        ) {
            return;
        }
        assert!(
            radio
                .publish_srb_payload(key, SignallingBearer::ControlAm, at(0), bytes, |bytes| {
                    sender.receive(at(0), bytes).unwrap();
                    Ok(transport.clone())
                })
                .unwrap()
        );
    }
    panic!("setup SDU did not delimit within bounded publications");
}
pub(super) fn install_integrity(
    radio: &mut CommonSignalling,
    key: UeKey,
) -> (IntegrityContext, u32) {
    let context = KeySet::uia1(
        KeyOrigin {
            ue: key,
            domain: CnDomain::Cs,
            generation: NonZeroU64::MIN,
        },
        CipherMaterial::Clear,
        Key::new([0x53; 16]),
    )
    .split()
    .1;
    let fresh = radio.security(key).unwrap().fresh();
    radio.registry.get_mut(key).unwrap().protection.integrity =
        integrity::initial_schedule(&context, fresh);
    (context, fresh)
}

#[test]
fn retained_profiles_use_actual_common_channel_sizes() {
    for compact in [false, true] {
        let (_source, radio, key, _) = ready(compact);
        let retained = radio.retained_signalling(key).unwrap();
        for (index, entity) in [
            (1, &radio.registry.get(key).unwrap().control),
            (2, &radio.registry.get(key).unwrap().nas_bearer),
        ] {
            assert_eq!(
                usize::from(retained[index].uplink_pdu_bits),
                entity.configuration().receive_pdu_bytes.get() * BITS_PER_BYTE
            );
            assert_eq!(
                usize::from(retained[index].downlink_pdu_bits),
                entity.configuration().transmit_pdu_bytes.get() * BITS_PER_BYTE
            );
        }
        let procedure = procedure(&radio, key);
        assert_eq!(procedure.phase(), SetupPhase::Prepared);
    }
}

#[test]
fn raw_response_reaches_existing_procedure_without_replacing_rlc() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let positions = radio.registry.get(key).unwrap().control.window_positions();
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(complete(procedure.transaction())),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(130), security_time(), |message, mut security| {
            assert_eq!(message.key(), key);
            assert_eq!(message.bearer(), SignallingBearer::ControlAm);
            assert_eq!(message.protection(), ControlProtection::Unprotected);
            assert_eq!(message.time().received(), at(100));
            assert_eq!(message.time().serviced(), at(130));
            assert_eq!(security.active_origin(CnDomain::Cs), None);
            assert_eq!(
                message.bearer_setup_response(&mut procedure, &mut security)?,
                SetupResponse::Complete
            );
            Ok(())
        }),
        Some(Ok(key))
    );
    assert_eq!(procedure.phase(), SetupPhase::Complete);
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .control
            .window_positions()
            .next_transmit,
        positions.next_transmit
    );
}

#[test]
fn queued_setup_cannot_accept_completion() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut procedure = procedure(&radio, key);
    radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(complete(procedure.transaction())),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(130), security_time(), |message, mut security| {
            message
                .bearer_setup_response(&mut procedure, &mut security)
                .map(|_| ())
        }),
        Some(Err(SignallingError::Phase))
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
}

#[test]
fn authenticated_response_and_backpressure_preserve_counter_and_observation_time() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (integrity, fresh) = install_integrity(&mut radio, key);
    let mut procedure = procedure(&radio, key);
    let expected = crate::dedicated_rx::ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::new(2).unwrap(),
    };
    let provenance = umts_rlc::acknowledged::ReceiveProvenance::Path(expected.configuration);
    let receipt = radio
        .publish_bearer_setup_on(&mut procedure, expected, at(0))
        .unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let bytes = integrity::protected(&integrity, fresh, 2, 2, complete(procedure.transaction()));
    uplink_on(&mut radio, key, &mut sender, &bytes, 100, provenance);
    assert_eq!(
        radio.process_one_at_with_control(at(120), security_time(), |message, _| {
            assert_eq!(
                message.protection(),
                ControlProtection::Integrity {
                    origin: integrity.origin(),
                    count: 2
                }
            );
            Err(SignallingError::Capacity)
        }),
        Some(Err(SignallingError::Capacity))
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(
        radio.process_one_at_with_control(at(150), security_time(), |message, mut security| {
            assert_eq!(message.time().received(), at(100));
            assert_eq!(message.time().serviced(), at(150));
            assert_eq!(message.time().provenance(), provenance);
            assert_eq!(
                message.protection(),
                ControlProtection::Integrity {
                    origin: integrity.origin(),
                    count: 2
                }
            );
            message
                .bearer_setup_response(&mut procedure, &mut security)
                .map(|_| ())
        }),
        Some(Ok(key))
    );
    uplink(&mut radio, key, &mut sender, &bytes, 200);
    assert_eq!(
        radio.process_one_at_with_control(at(220), security_time(), |_, _| panic!(
            "replayed integrity message reached owner"
        )),
        Some(Err(SignallingError::Duplicate))
    );
    assert_eq!(procedure.phase(), SetupPhase::Complete);
}

#[test]
fn bad_dispatch_time_does_not_consume_ingress() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(complete(procedure.transaction())),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(105), security_time(), |_, _| panic!(
            "invalid local time dispatched"
        )),
        Some(Err(SignallingError::Dedicated(DedicatedError::Time)))
    );
    assert_eq!(
        radio.process_one_at_with_control(at(120), security_time(), |message, mut security| {
            message
                .bearer_setup_response(&mut procedure, &mut security)
                .map(|_| ())
        }),
        Some(Ok(key))
    );
}

#[test]
fn admission_failure_does_not_advance_procedure_or_transaction() {
    let (_source, mut radio, key, _) = ready(false);
    let mut procedure = procedure(&radio, key);
    let transaction = procedure.transaction();
    let readiness = radio.bearer_setup_readiness(key).unwrap();
    for _ in 0..4 {
        radio
            .registry
            .get_mut(key)
            .unwrap()
            .control
            .enqueue(key, Bits::new(&[0; 8], 64).unwrap())
            .unwrap();
    }
    assert_eq!(
        radio.publish_bearer_setup(&mut procedure, at(0)),
        Err(SignallingError::Capacity)
    );
    assert_eq!(procedure.phase(), SetupPhase::Prepared);
    assert_eq!(procedure.receipt(), None);
    assert_eq!(radio.next_bearer_setup_transaction(key), Ok(transaction));
    assert!(radio.registry.get(key).unwrap().bearer_change.is_none());
    assert_eq!(radio.bearer_setup_readiness(key), Some(readiness));
}

#[test]
fn pending_setup_blocks_reset_and_second_setup_only_for_its_ue() {
    let (source, mut radio, key, _) = ready(false);
    let other = establish(&mut radio, &source, 1);
    send(
        &mut radio,
        other,
        SignallingBearer::NasAm,
        &mut peer(&source),
        &INITIAL,
    );
    finish_registration(&mut radio, other);
    assert_eq!(radio.process_one(), Some(Ok(other)));
    radio
        .core()
        .publish_one(|_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let mut first = procedure(&radio, key);
    let mut duplicate = procedure(&radio, key);
    radio.publish_bearer_setup(&mut first, at(0)).unwrap();
    assert!(
        radio
            .bearer_setup_readiness(key)
            .unwrap()
            .bearer_change_pending
    );
    assert!(
        !radio
            .bearer_setup_readiness(other)
            .unwrap()
            .bearer_change_pending
    );
    assert_eq!(
        radio.publish_bearer_setup(&mut duplicate, at(0)),
        Err(SignallingError::Capacity)
    );
    assert_eq!(
        radio.initiate_am_reset(key, SignallingBearer::NasAm, at(0)),
        Err(SignallingError::Phase)
    );
    let mut independent = procedure(&radio, other);
    assert!(radio.publish_bearer_setup(&mut independent, at(0)).is_ok());
    assert_eq!(independent.transaction(), first.transaction());
}

#[test]
fn published_timeout_requires_explicit_recovery_without_reseeding_rlc() {
    let (_source, mut radio, key, _) = ready(false);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    assert_eq!(
        radio.require_bearer_setup_recovery(&procedure),
        Err(SignallingError::Phase)
    );
    assert!(procedure.expire(at(1_000_000)).unwrap());
    radio.require_bearer_setup_recovery(&procedure).unwrap();
    assert_eq!(procedure.phase(), SetupPhase::RecoveryRequired);
    assert_eq!(
        radio.release_bearer_setup_receipt(key, receipt),
        Err(SignallingError::Phase)
    );
    assert_eq!(
        radio.initiate_am_reset(key, SignallingBearer::ControlAm, at(1_000_000)),
        Err(SignallingError::Phase)
    );
    assert_eq!(procedure.receipt(), Some(receipt));
}

#[test]
fn failure_preserves_failed_bearers_and_enters_explicit_recovery() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut procedure = procedure(&radio, key);
    let receipt = radio
        .publish_bearer_setup_on(
            &mut procedure,
            crate::dedicated_rx::ReceiveIdentity {
                ue: key,
                configuration: NonZeroU64::new(2).unwrap(),
            },
            at(0),
        )
        .unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let failure = pdu::RadioBearerSetupFailure::new(
        ie::RRCTransactionIdentifier(procedure.transaction().get()),
        ie::FailureCauseWithProtErr::configurationUnsupported(()),
        Some(ie::RBIdentityList(vec![ie::RBIdentity(10)])),
        None,
    );
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(ULDCCHMessageType::radioBearerSetupFailure(failure.clone())),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(120), security_time(), |message, mut security| {
            assert_eq!(
                message.bearer_setup_response(&mut procedure, &mut security)?,
                SetupResponse::Failure
            );
            Ok(())
        }),
        Some(Ok(key))
    );
    assert_eq!(procedure.failure(), Some(&failure));
    assert_eq!(
        radio.release_bearer_setup_receipt(key, receipt),
        Err(SignallingError::Phase)
    );
    radio.require_bearer_setup_recovery(&procedure).unwrap();
    assert_eq!(procedure.phase(), SetupPhase::FailureReported);
    assert!(
        radio
            .registry
            .get(key)
            .unwrap()
            .protection
            .bearer_reconfiguration_block()
            .is_some()
    );
}

#[test]
fn mismatched_retained_pdu_size_fails_before_admission() {
    let (_source, mut radio, key, _) = ready(false);
    let mut retained = radio.retained_signalling(key).unwrap();
    for bearer in retained.iter_mut().skip(1) {
        bearer.uplink_pdu_bits += 8;
    }
    let mut procedure = RadioBearerSetupProcedure::initial(
        key,
        radio.next_bearer_setup_transaction(key).unwrap(),
        InitialDchSetup::new(
            configuration(&retained),
            retained,
            fixture::rabs(true, false),
        )
        .unwrap(),
        at(0),
        at(1000),
        NonZeroU64::new(1_000_000).unwrap(),
    )
    .unwrap();
    assert_eq!(
        radio.publish_bearer_setup(&mut procedure, at(0)),
        Err(SignallingError::ConfigurationFailure)
    );
    assert_eq!(procedure.phase(), SetupPhase::Prepared);
    assert!(radio.registry.get(key).unwrap().bearer_change.is_none());
}

#[test]
fn protected_queue_failure_does_not_consume_downlink_integrity_sequence() {
    let (_source, mut radio, key, _) = ready(false);
    install_integrity(&mut radio, key);
    let mut procedure = procedure(&radio, key);
    let before = radio
        .registry
        .get(key)
        .unwrap()
        .protection
        .integrity
        .next_downlink_sequence(key, 2)
        .unwrap();
    for _ in 0..4 {
        radio
            .registry
            .get_mut(key)
            .unwrap()
            .control
            .enqueue(key, Bits::new(&[0; 8], 64).unwrap())
            .unwrap();
    }
    assert_eq!(
        radio.publish_bearer_setup(&mut procedure, at(0)),
        Err(SignallingError::Capacity)
    );
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .protection
            .integrity
            .next_downlink_sequence(key, 2)
            .unwrap(),
        before
    );
    assert_eq!(
        radio.next_bearer_setup_transaction(key).unwrap(),
        procedure.transaction()
    );
}

#[test]
fn unprotected_completion_cannot_bypass_active_integrity() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let rejected_before = radio.counters().rejected;
    install_integrity(&mut radio, key);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(complete(procedure.transaction())),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(120), security_time(), |_, _| panic!(
            "unauthenticated control dispatched"
        )),
        Some(Err(SignallingError::Nas(NasError::SecurityRequired)))
    );
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
    assert_eq!(radio.counters().rejected, rejected_before + 1);
}

#[test]
fn invalid_mac_does_not_reach_bearer_procedure() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let (integrity, fresh) = install_integrity(&mut radio, key);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    let bytes = integrity::protected(
        &integrity,
        fresh ^ 1,
        2,
        2,
        complete(procedure.transaction()),
    );
    uplink(&mut radio, key, &mut sender, &bytes, 100);
    let result = radio.process_one_at_with_control(at(120), security_time(), |_, _| {
        panic!("invalid MAC dispatched")
    });
    assert!(matches!(result, Some(Err(SignallingError::Integrity(_)))));
    assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
}

#[test]
fn receipt_release_requires_response_and_actual_peer_status() {
    let (_source, mut radio, key, mut sender) = ready(false);
    let mut procedure = procedure(&radio, key);
    let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
    publish(&mut radio, key, receipt, &mut sender);
    assert_eq!(
        radio.release_bearer_setup_receipt(key, receipt),
        Err(SignallingError::Phase)
    );
    uplink(
        &mut radio,
        key,
        &mut sender,
        &clear(complete(procedure.transaction())),
        100,
    );
    assert_eq!(
        radio.process_one_at_with_control(at(120), security_time(), |message, mut security| {
            message
                .bearer_setup_response(&mut procedure, &mut security)
                .map(|_| ())
        }),
        Some(Ok(key))
    );
    for _ in 0..64 {
        if radio.bearer_setup_transmission(key, receipt).unwrap() == SduTransmission::Acknowledged {
            break;
        }
        let publication = sender
            .publish_next(at(200), |publication| {
                radio.receive_srb_payload(
                    key,
                    SignallingBearer::ControlAm,
                    ReceiveTime::new(at(200), at(210)).unwrap(),
                    Bits::new(publication.bytes, publication.bytes.len() * BITS_PER_BYTE).unwrap(),
                )
            })
            .unwrap();
        if publication.is_none() {
            break;
        }
    }
    assert_eq!(
        radio.bearer_setup_transmission(key, receipt),
        Ok(SduTransmission::Acknowledged)
    );
    radio.release_bearer_setup_receipt(key, receipt).unwrap();
    assert!(radio.registry.get(key).unwrap().bearer_change.is_none());
    assert_eq!(
        radio.next_bearer_setup_transaction(key).unwrap().get(),
        (procedure.transaction().get() + 1) % 4
    );
}

#[test]
fn scheduled_combined_setup_checks_original_receive_time_against_activation() {
    use std::num::NonZeroU8;
    use umts_rrc::dedicated::{ExistingDchSetup, ScheduledActivation};
    for received in [100, 153_600] {
        let (_source, mut radio, key, mut sender) = ready(false);
        let retained = radio.retained_signalling(key).unwrap();
        let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
        let setup = ExistingDchSetup::new(
            configuration(&retained),
            configuration_for(&retained, true),
            fixture::rabs(false, true),
            activation,
        )
        .unwrap();
        let mut procedure = RadioBearerSetupProcedure::existing(
            key,
            radio.next_bearer_setup_transaction(key).unwrap(),
            setup,
            at(0),
            at(1000),
            NonZeroU64::new(1_000_000).unwrap(),
        )
        .unwrap();
        let receipt = radio.publish_bearer_setup(&mut procedure, at(0)).unwrap();
        publish(&mut radio, key, receipt, &mut sender);
        uplink(
            &mut radio,
            key,
            &mut sender,
            &clear(complete(procedure.transaction())),
            received,
        );
        let result = radio.process_one_at_with_control(
            at(200_000),
            security_time(),
            |message, mut security| {
                message
                    .bearer_setup_response(&mut procedure, &mut security)
                    .map(|_| ())
            },
        );
        if received < activation.time().index {
            assert_eq!(
                result,
                Some(Err(SignallingError::Dedicated(DedicatedError::Time)))
            );
            assert_eq!(procedure.phase(), SetupPhase::AwaitingComplete);
        } else {
            assert_eq!(result, Some(Ok(key)));
            assert_eq!(procedure.phase(), SetupPhase::Complete);
        }
    }
}
