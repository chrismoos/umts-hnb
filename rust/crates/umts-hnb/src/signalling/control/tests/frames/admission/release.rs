// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_admission::{ScheduledReleasePhase, ScheduledReleaseRequest};
use std::num::NonZeroU8;
use umts_rrc::dedicated::{
    Combination, ExistingDchRelease, RadioBearerReleaseProcedure, ScheduledActivation,
};

fn active() -> (Environment, ChipInstant) {
    let (env, now, _) = active_with_peer();
    (env, now)
}
struct ActivePeer {
    sender: umts_rlc::acknowledged::AmEntity,
    integrity: umts_security::contexts::IntegrityContext,
    fresh: u32,
}
fn active_with_peer() -> (Environment, ChipInstant, ActivePeer) {
    let mut env = started();
    let (integrity, fresh) = super::super::super::install_integrity(&mut env.radio, env.key);
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
        let window = env
            .transmit
            .generate_slot(&mut env.source, &mut samples)
            .unwrap();
        assert!(env.writer.accepted(window));
        let now = env.source.next_chip();
        step(&mut env, now);
    }
    let now = env.source.next_chip();
    until(&mut env, AdmissionPhase::AwaitingComplete, now);
    let setup = env.admission.procedure(env.identity).unwrap();
    let receipt = setup.receipt().unwrap();
    let transaction = setup.transaction();
    let mut sender = crate::signalling::tests::peer(&env.source);
    sender.enqueue(&super::super::super::COMPLETE).unwrap();
    sender
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    super::super::super::publish(&mut env.radio, env.key, receipt, &mut sender);
    let (kind, status) = sender
        .publish_next(now, |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    assert_eq!(kind, umts_rlc::acknowledged::PublicationKind::Status);
    let received = now.index + 100;
    let response = super::super::super::user_setup::response(transaction, 3);
    super::super::super::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut sender,
        &crate::test_support::integrity::protected(&integrity, fresh, 2, 2, response),
        received,
        ReceiveProvenance::Path(env.identity.configuration),
        Some(at(received)),
    );
    let serviced = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            serviced,
            super::super::super::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    step(&mut env, serviced);
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::AwaitingCommandAck)
    );
    env.radio
        .receive_srb_payload(
            env.key,
            SignallingBearer::ControlAm,
            ReceiveTime::simultaneous(serviced),
            Bits::new(&status, status.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
    step(&mut env, serviced);
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Active)
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    (
        env,
        serviced,
        ActivePeer {
            sender,
            integrity,
            fresh,
        },
    )
}

fn signalling_only(plan: &Arc<CompiledRel99>) -> Arc<CompiledRel99> {
    let old = plan.configuration();
    let bearers: Vec<_> = old
        .bearers()
        .iter()
        .filter(|rb| rb.domain.is_none())
        .cloned()
        .collect();
    let subset = |tfcs: &DchTfcs, direction| {
        let indices: Vec<_> = tfcs
            .transports()
            .iter()
            .enumerate()
            .filter(|(_, trch)| {
                bearers
                    .iter()
                    .any(|rb| rb.mappings(direction).any(|map| map.transport == trch.id))
            })
            .map(|(index, _)| index)
            .collect();
        let mut combinations: Vec<Combination> = Vec::new();
        for previous in tfcs.combinations() {
            let formats: Vec<_> = indices
                .iter()
                .map(|index| previous.formats[*index])
                .collect();
            if !combinations.iter().any(|row| row.formats == formats) {
                combinations.push(Combination {
                    formats,
                    gain: previous.gain,
                });
            }
        }
        DchTfcs::new(
            direction,
            indices
                .iter()
                .map(|index| tfcs.transports()[*index].clone())
                .collect(),
            combinations,
        )
        .unwrap()
    };
    let configuration = Arc::new(
        Rel99Configuration::new(
            subset(old.uplink(), DchDirection::Uplink),
            subset(old.downlink(), DchDirection::Downlink),
            bearers,
            old.physical().clone(),
        )
        .unwrap(),
    );
    Arc::new(
        CompiledRel99::new(
            configuration,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}

fn request(env: &mut Environment, now: ChipInstant) -> Box<ScheduledReleaseRequest> {
    let identity = ReceiveIdentity {
        ue: env.key,
        configuration: NonZeroU64::new(2).unwrap(),
    };
    let plan = signalling_only(&env.plan);
    let boundary = FrameAnchor {
        time: at(8 * CHIPS_PER_FRAME),
        cfn: 8,
    };
    let activation = MappingActivation {
        uplink: boundary,
        uplink_frame: 8,
        downlink: boundary,
        downlink_frame: 8,
    };
    let pool = env
        .admission
        .allocate_release_pool(
            env.identity,
            identity,
            plan.clone(),
            &mut env.radio,
            FramePoolLimits {
                payload_slots: count(1),
                receipt_slots: count(32),
                maximum_buffer_bytes: 2_000_000,
            },
        )
        .unwrap();
    let receiver = mapping::retirement::receive_owner(identity, activation, &plan);
    let transmit = Box::new(
        DedicatedTransmitSession::new(
            identity,
            boundary,
            plan.clone(),
            umts_dsp::dedicated::DpchAmplitudes {
                data: 0.2,
                tpc: 0.2,
                tfci: 0.2,
                pilot: 0.2,
            },
            TransmitLimits {
                frame_slots: count(8),
                maximum_frame_bits: count(100_000),
                maximum_retained_bits: count(1_000_000),
            },
        )
        .unwrap(),
    );
    let procedure = Box::new(
        RadioBearerReleaseProcedure::new(
            env.key,
            env.radio.next_bearer_release_transaction(env.key).unwrap(),
            ExistingDchRelease::new(
                env.plan.configuration().clone(),
                plan.configuration().clone(),
                ScheduledActivation::new(at(0), 0, NonZeroU8::new(8).unwrap()).unwrap(),
            )
            .unwrap(),
            now,
            at(7 * CHIPS_PER_FRAME),
            env.radio.limits.response_guard_chips,
        )
        .unwrap(),
    );
    Box::new(ScheduledReleaseRequest {
        receiver,
        transmit,
        procedure,
        pool,
        activation,
        change: None,
    })
}

#[test]
fn scheduled_release_rejects_mismatched_activation_without_losing_request_or_pool_owner() {
    let (mut env, now) = active();
    let mut request = request(&mut env, now);
    request.activation.downlink_frame += CONNECTION_FRAME_MODULUS;
    let pointer = std::ptr::from_ref(request.as_ref());
    let charged = env.radio.ledger.used();
    let rejected = env
        .admission
        .begin_release(env.identity, request, &env.radio, &mut env.receive)
        .unwrap_err();
    assert_eq!(std::ptr::from_ref(rejected.request.as_ref()), pointer);
    assert_eq!(
        rejected.error,
        AdmissionError::Receive(crate::dedicated_controller::DedicatedControlError::Bearer(
            BearerError::Time
        ))
    );
    assert_eq!(env.radio.ledger.used(), charged);
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Active)
    );
    assert_eq!(env.admission.release_phase(env.identity), None);
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    env.radio.retire_frame_pool(rejected.request.pool).unwrap();
}

#[test]
fn scheduled_release_rejects_distinct_transmitter_plan_before_staging_any_receiver() {
    let (mut env, now) = active();
    let mut request = request(&mut env, now);
    let successor = request.pool.identity();
    let other_plan = signalling_only(&env.plan);
    request.transmit = Box::new(
        DedicatedTransmitSession::new(
            successor,
            request.activation.downlink,
            other_plan,
            umts_dsp::dedicated::DpchAmplitudes {
                data: 0.2,
                tpc: 0.2,
                tfci: 0.2,
                pilot: 0.2,
            },
            TransmitLimits {
                frame_slots: count(8),
                maximum_frame_bits: count(100_000),
                maximum_retained_bits: count(1_000_000),
            },
        )
        .unwrap(),
    );
    let pointer = std::ptr::from_ref(request.as_ref());
    let charged = env.radio.ledger.used();
    let rejected = env
        .admission
        .begin_release(env.identity, request, &env.radio, &mut env.receive)
        .unwrap_err();
    assert_eq!(std::ptr::from_ref(rejected.request.as_ref()), pointer);
    assert_eq!(rejected.error, AdmissionError::Configuration);
    assert_eq!(env.radio.ledger.used(), charged);
    assert_eq!(env.admission.release_phase(env.identity), None);
    assert_eq!(
        env.receive.with_active_mapping(successor, |_| ()),
        Err(crate::dedicated_controller::DedicatedControlError::Identity)
    );
    env.radio.retire_frame_pool(rejected.request.pool).unwrap();
}

#[test]
fn scheduled_release_prepares_real_successor_without_replacing_original_active_identity() {
    let (mut env, now) = active();
    let request = request(&mut env, now);
    let successor = request.pool.identity();
    assert_eq!(
        env.admission
            .begin_release(env.identity, request, &env.radio, &mut env.receive)
            .unwrap(),
        successor
    );
    assert_eq!(
        env.admission.release_phase(env.identity),
        Some(ScheduledReleasePhase::Preparing)
    );
    for _ in 0..128 {
        step(&mut env, now);
        assert!(env.admission.take_unclaimed().is_none());
        if env.admission.release_phase(env.identity)
            == Some(ScheduledReleasePhase::AwaitingComplete)
        {
            break;
        }
    }
    assert_eq!(env.admission.release_failure(env.identity), None);
    assert_eq!(
        env.admission.release_phase(env.identity),
        Some(ScheduledReleasePhase::AwaitingComplete)
    );
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Active)
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    assert_eq!(
        env.receive
            .with_active_mapping(successor, |bank| bank.identity())
            .unwrap(),
        env.identity
    );
    let boundary = at(8 * CHIPS_PER_FRAME);
    let next = env.transmit.bank().session(successor).unwrap();
    assert!(next.queued_frames() >= limits().prefill_frames.get());
    assert!(
        next.next_prepare().index
            >= boundary.index + limits().prefill_frames.get() as u64 * CHIPS_PER_FRAME
    );
    assert!(
        env.transmit
            .bank()
            .session(env.identity)
            .unwrap()
            .next_prepare()
            .index
            <= boundary.index
    );
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    while env.source.next_chip().index <= boundary.index {
        let first_slot = env.source.next_chip();
        if first_slot.index >= 4 * CHIPS_PER_FRAME {
            let identity = if first_slot.index < boundary.index {
                env.identity
            } else {
                successor
            };
            env.feedback
                .publish(TpcPlan {
                    identity,
                    first_slot,
                    commands: vec![Bit::Zero],
                })
                .ok()
                .unwrap();
            env.transmit.service().unwrap();
            assert!(matches!(
                env.feedback.try_event().unwrap().outcome,
                TransmitCommandOutcome::Feedback { result: Ok(()), .. }
            ));
        }
        let window = env
            .transmit
            .generate_slot(&mut env.source, &mut samples)
            .unwrap();
        if first_slot.index == boundary.index {
            let current = env.source.next_chip();
            for _ in 0..8 {
                step(&mut env, current);
            }
            assert_eq!(env.admission.release_failure(env.identity), None);
            assert!(env.transmit.bank().session(env.identity).is_some());
        }
        assert!(env.writer.accepted(window));
        let current = env.source.next_chip();
        for _ in 0..4 {
            step(&mut env, current);
            assert!(env.admission.take_unclaimed().is_none());
            assert_eq!(env.admission.release_failure(env.identity), None);
            if let Some(previous) = env.transmit.bank().session(env.identity) {
                assert!(previous.next_prepare().index <= boundary.index);
            }
        }
    }
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    assert_eq!(
        env.transmit.bank().session(successor).unwrap().failure(),
        None
    );
}

fn generated_release_slot(
    env: &mut Environment,
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
) -> ChipInstant {
    let first_slot = env.source.next_chip();
    if first_slot.index >= 4 * CHIPS_PER_FRAME {
        env.feedback
            .publish(TpcPlan {
                identity: if first_slot.index < 8 * CHIPS_PER_FRAME {
                    previous
                } else {
                    successor
                },
                first_slot,
                commands: vec![Bit::Zero],
            })
            .ok()
            .unwrap();
        env.transmit.service().unwrap();
        assert!(matches!(
            env.feedback.try_event().unwrap().outcome,
            TransmitCommandOutcome::Feedback { result: Ok(()), .. }
        ));
    }
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    let window = env
        .transmit
        .generate_slot(&mut env.source, &mut samples)
        .unwrap();
    assert!(env.writer.accepted(window));
    let now = env.source.next_chip();
    for _ in 0..4 {
        step(env, now);
        assert!(env.admission.take_unclaimed().is_none());
    }
    now
}

struct ReadyRelease {
    env: Environment,
    now: ChipInstant,
    peer: ActivePeer,
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
    transaction: TransactionId,
}

fn ready_to_commit(mut env: Environment, now: ChipInstant, mut peer: ActivePeer) -> ReadyRelease {
    let previous = env.identity;
    let request = request(&mut env, now);
    let successor = request.pool.identity();
    let transaction = request.procedure.transaction();
    env.admission
        .begin_release(previous, request, &env.radio, &mut env.receive)
        .unwrap();
    for _ in 0..128 {
        step(&mut env, now);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::AwaitingComplete) {
            break;
        }
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::AwaitingComplete)
    );
    let mut now = now;
    let signalling = env
        .plan
        .configuration()
        .bearer(SignallingBearer::ControlAm.identity())
        .unwrap();
    for mapping in signalling.mappings(DchDirection::Downlink) {
        assert_eq!(
            env.plan
                .configuration()
                .downlink()
                .transport(mapping.transport)
                .unwrap()
                .interval,
            umts_rrc::transport::TransportInterval::Ms40
        );
    }
    let maximum_slots = env.radio.limits.response_guard_chips.get() / CHIPS_PER_SLOT;
    for _ in 0..maximum_slots {
        now = generated_release_slot(&mut env, previous, successor);
        let receipt = env
            .admission
            .release_procedure(previous)
            .unwrap()
            .receipt()
            .unwrap();
        if now.index > 8 * CHIPS_PER_FRAME
            && matches!(
                env.radio
                    .bearer_setup_transmission(env.key, receipt)
                    .unwrap(),
                SduTransmission::InFlight {
                    delimited: true,
                    ..
                } | SduTransmission::Acknowledged
            )
        {
            break;
        }
    }
    let receipt = env
        .admission
        .release_procedure(previous)
        .unwrap()
        .receipt()
        .unwrap();
    assert!(matches!(
        env.radio
            .bearer_setup_transmission(env.key, receipt)
            .unwrap(),
        SduTransmission::InFlight {
            delimited: true,
            ..
        } | SduTransmission::Acknowledged
    ));
    let first = umts_types::time::SampleInstant {
        epoch: env.key.epoch,
        index: 8 * CHIPS_PER_FRAME * 2 + 100,
    };
    env.rx
        .feed(crate::radio_io::MatchedSamples {
            window: umts_dsp::receive_filter::FilteredWindow {
                first_center: Some(first),
                sample_stride: 1,
                next_input: umts_types::time::SampleInstant {
                    index: first.index + 256,
                    ..first
                },
            },
            generation: 2,
            discontinuity: true,
            samples: &[num_complex::Complex32::default(); 256],
        })
        .unwrap();
    let body = || {
        ULDCCHMessageType::radioBearerReleaseComplete(
            umts_rrc_schema::pdu_definitions::RadioBearerReleaseComplete::new(
                ie::RRCTransactionIdentifier(transaction.get()),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
        )
    };
    let received = now.index + 100;
    super::super::super::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut peer.sender,
        &crate::test_support::integrity::protected(&peer.integrity, peer.fresh, 2, 3, body()),
        received,
        ReceiveProvenance::Path(successor.configuration),
        Some(at(received)),
    );
    now = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            now,
            super::super::super::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    for _ in 0..128 {
        step(&mut env, now);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::ReadyToCommit) {
            break;
        }
    }
    assert_eq!(env.admission.release_failure(previous), None);
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::ReadyToCommit)
    );
    ReadyRelease {
        env,
        now,
        peer,
        previous,
        successor,
        transaction,
    }
}

#[test]
fn connected_scheduled_release_commits_both_actual_workers_and_keeps_successor_running() {
    use crate::{
        dedicated_bearers::{MappingReleaseError, ReleaseOutputPolicy},
        dedicated_controller::DedicatedReleaseError,
        signalling::BearerReleaseCommitError,
    };
    let (env, now, peer) = active_with_peer();
    let ReadyRelease {
        mut env,
        now,
        mut peer,
        previous,
        successor,
        transaction,
    } = ready_to_commit(env, now, peer);
    let body = || {
        ULDCCHMessageType::radioBearerReleaseComplete(
            umts_rrc_schema::pdu_definitions::RadioBearerReleaseComplete::new(
                ie::RRCTransactionIdentifier(transaction.get()),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
        )
    };
    let mut calls = 0;
    let rejected = env.admission.commit_release_with(
        previous,
        &mut env.radio,
        &mut env.receive,
        ReleaseOutputPolicy::DiscardReleased,
        |_| {
            calls += 1;
            Err::<(), _>(9)
        },
    );
    assert!(
        matches!(
            rejected,
            Err(BearerReleaseCommitError::Commit(
                DedicatedReleaseError::Release(MappingReleaseError::Cleanup(9))
            ))
        ),
        "{rejected:?}"
    );
    assert_eq!(calls, 1);
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    assert_eq!(env.admission.phase(previous), Some(AdmissionPhase::Active));
    let result = env
        .admission
        .commit_release_with(
            previous,
            &mut env.radio,
            &mut env.receive,
            ReleaseOutputPolicy::DiscardReleased,
            |released| {
                assert_eq!(released.iter().map(|id| id.get()).collect::<Vec<_>>(), [20]);
                Ok::<_, ()>(7)
            },
        )
        .unwrap();
    assert_eq!(result.previous, previous);
    assert_eq!(result.next, successor);
    assert_eq!(result.cleanup, 7);
    assert_eq!(env.admission.phase(previous), None);
    assert_eq!(env.admission.phase(successor), Some(AdmissionPhase::Active));
    assert_eq!(env.radio.dedicated_route(env.key), Some(successor));
    let duplicate_time = now.index + 100;
    super::super::super::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut peer.sender,
        &crate::test_support::integrity::protected(&peer.integrity, peer.fresh, 2, 4, body()),
        duplicate_time,
        ReceiveProvenance::Path(successor.configuration),
        Some(at(duplicate_time)),
    );
    assert_eq!(
        env.radio.process_one_at_with_control(
            at(duplicate_time + 20),
            super::super::super::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    assert_eq!(
        env.admission.release_phase(successor),
        Some(ScheduledReleasePhase::Committed)
    );
    let before = env
        .transmit
        .bank()
        .session(successor)
        .unwrap()
        .next_prepare();
    env.identity = successor;
    for _ in 0..120 {
        generated_release_slot(&mut env, previous, successor);
        if env
            .transmit
            .bank()
            .session(successor)
            .unwrap()
            .next_prepare()
            .index
            > before.index
        {
            break;
        }
    }
    assert!(
        env.transmit
            .bank()
            .session(successor)
            .unwrap()
            .next_prepare()
            .index
            > before.index
    );
    assert_eq!(
        env.transmit.bank().session(successor).unwrap().failure(),
        None
    );
}
