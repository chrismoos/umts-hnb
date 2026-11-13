// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod downlink_power;
mod drain;
mod estimates;
mod grouped_tpc;
mod hsdpa_control;
mod transitions;

#[test]
fn control_publication_before_park_preserves_the_preparation_wake() {
    use std::{sync::mpsc, thread, time::Duration};

    let (mut producer, mut consumer) = endpoints(
        identity(0, 1).ue.epoch,
        Lane::Control,
        count(2),
        count(2),
        0,
        0,
    );
    let gate = Arc::new(AtomicBool::new(false));
    let worker_gate = gate.clone();
    let (ready, registered) = mpsc::channel();
    let (finished, completion) = mpsc::channel();
    let worker = thread::spawn(move || {
        consumer.link.wake.set(thread::current()).unwrap();
        ready.send(()).unwrap();
        while !worker_gate.load(Ordering::Acquire) {
            thread::yield_now();
        }
        thread::park();
        finished
            .send(consumer.commands.try_recv().map(|command| command.id))
            .unwrap();
    });
    let registered = registered.recv_timeout(Duration::from_secs(1));
    let published = producer.post(Action::Arm(identity(0, 1)), 0).ok();
    gate.store(true, Ordering::Release);
    let completion = completion.recv_timeout(Duration::from_secs(1));
    worker.thread().unpark();
    assert!(worker.join().is_ok());
    assert!(registered.is_ok());
    assert!(published.is_some());
    assert_eq!(completion.ok().flatten(), published);
}

#[test]
fn status_queries_do_not_interrupt_the_next_slot_wait() {
    assert!(!Action::Observe(identity(0, 1)).interrupts_wait());
    assert!(!Action::TakeHsdpaCompletion.interrupts_wait());
    assert!(!Action::Hsupa(crate::hsupa_tx::Command::TakeCompletion).interrupts_wait());
    assert!(Action::Arm(identity(0, 1)).interrupts_wait());
    assert!(Action::Remove(identity(0, 1)).interrupts_wait());
}

#[test]
fn frame_capacity_retry_retains_payload_without_publication_until_owner_progress() {
    let (_source, mut runtime, mut ports) = runtime(4, 4);
    let owner = owner(0, 1);
    let mut frame = input(&owner);
    let retry = at(CHIPS_PER_FRAME);
    frame.retry_after = Some(retry);
    let expected: Vec<_> = frame
        .transports
        .iter()
        .map(|payload| (payload.transport, payload.bits.clone()))
        .collect();
    let returned = ports.control.frame(frame).err().unwrap();
    assert_eq!(ports.control.outstanding(), 0);
    assert!(runtime.control.commands.try_recv().is_none());
    assert_eq!(returned.retry_after, Some(retry));
    assert!(
        returned
            .transports
            .iter()
            .zip(&expected)
            .all(|(actual, expected)| actual.transport == expected.0 && actual.bits == expected.1)
    );
    runtime
        .control
        .link
        .prepared_through
        .store(retry.index, Ordering::Release);
    assert!(ports.control.frame(returned).is_ok());
    assert_eq!(ports.control.outstanding(), 1);
}

fn single_decision(identity: ReceiveIdentity, slot: u64) -> TpcDecision {
    TpcDecision {
        identity,
        physical_slot_start: at(slot),
        command: Bit::One,
    }
}

fn initialization(first: bool, pairs: u8, slots: u32) -> TpcInitialization {
    TpcInitialization {
        first_radio_link_set: first,
        pattern_01_count: pairs,
        maximum_slots: std::num::NonZeroU32::new(slots).unwrap(),
    }
}

fn initialization_lane(cfn: u8, settings: TpcInitialization) -> TpcLane {
    TpcLane {
        identity: Some(identity(0, 1)),
        anchor: at(0),
        anchor_cfn: cfn,
        initialization: Some(settings),
        strict_from: None,
        decisions: [None; TPC_SLOTS],
        estimates: EstimateLane::default(),
        grouped: None,
    }
}

fn take_initial(lane: &mut TpcLane, slot: u64) -> Result<Bit, TransmitError> {
    lane.take(TpcRequest {
        identity: identity(0, 1),
        physical_slot_start: at(slot * CHIPS_PER_SLOT),
        synthesis_at: at(slot * CHIPS_PER_SLOT),
    })
}

#[test]
fn initialization_pattern_repeats_and_restarts_on_absolute_cfn_mod_four_including_wrap() {
    for cfn in [3, 255] {
        let mut lane = initialization_lane(cfn, initialization(true, 3, 100));
        for (slot, bit) in [
            (0, Bit::Zero),
            (1, Bit::One),
            (5, Bit::One),
            (6, Bit::One),
            (7, Bit::Zero),
            (14, Bit::Zero),
            (15, Bit::Zero),
            (16, Bit::One),
            (74, Bit::One),
            (75, Bit::Zero),
        ] {
            assert_eq!(
                take_initial(&mut lane, slot),
                Ok(bit),
                "CFN={cfn} slot={slot}"
            );
        }
    }
    let mut lane = initialization_lane(2, initialization(true, 3, 100));
    assert_eq!(take_initial(&mut lane, 29), Ok(Bit::One));
    assert_eq!(take_initial(&mut lane, 30), Ok(Bit::Zero));
    assert_eq!(take_initial(&mut lane, 90), Ok(Bit::Zero));
}

#[test]
fn initialization_all_up_cases_and_finite_expiry_never_silently_extend() {
    for settings in [initialization(false, 3, 3), initialization(true, 0, 3)] {
        let mut lane = initialization_lane(0, settings);
        for slot in 0..3 {
            assert_eq!(take_initial(&mut lane, slot), Ok(Bit::One));
        }
        assert_eq!(
            take_initial(&mut lane, 3),
            Err(TransmitError::TpcUnavailable)
        );
        assert_eq!(
            take_initial(&mut lane, 60),
            Err(TransmitError::TpcUnavailable)
        );
    }
}

#[test]
fn actual_feedback_permanently_cuts_over_initialization_at_earliest_admitted_slot() {
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    let mut session = owner(0, 1);
    session
        .configure_initial_tpc(initialization(true, 3, 100))
        .unwrap();
    install(&mut runtime, &mut ports, session);
    ports
        .feedback
        .publish_decision(single_decision(identity(0, 1), 6 * CHIPS_PER_SLOT))
        .unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Decision { result: Ok(()), .. }
    ));
    feedback(
        &mut runtime,
        &mut ports,
        identity(0, 1),
        4 * CHIPS_PER_SLOT,
        vec![Bit::Zero],
    );
    let lane = runtime.tpc_lane_mut(identity(0, 1)).unwrap();
    assert_eq!(take_initial(lane, 3), Ok(Bit::One));
    assert_eq!(take_initial(lane, 4), Ok(Bit::Zero));
    assert_eq!(take_initial(lane, 4), Err(TransmitError::TpcUnavailable));
    assert_eq!(take_initial(lane, 5), Err(TransmitError::TpcUnavailable));
    assert_eq!(take_initial(lane, 6), Ok(Bit::One));
    assert_eq!(take_initial(lane, 7), Err(TransmitError::TpcUnavailable));
    assert_eq!(take_initial(lane, 60), Err(TransmitError::TpcUnavailable));
    runtime.clear_tpc(identity(0, 1));
    assert!(
        runtime
            .tpc
            .iter()
            .all(|lane| lane.initialization.is_none() && lane.strict_from.is_none())
    );
}

#[test]
fn invalid_feedback_does_not_disable_initialization_and_default_sessions_stay_strict() {
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    install(&mut runtime, &mut ports, owner(0, 1));
    assert_eq!(
        take_initial(runtime.tpc_lane_mut(identity(0, 1)).unwrap(), 0),
        Err(TransmitError::TpcUnavailable)
    );
    let mut lane = initialization_lane(0, initialization(true, 3, 100));
    assert_eq!(
        lane.apply_commands(identity(0, 2), at(0), &[Bit::One], at(0)),
        Err(TransmitError::Identity)
    );
    assert_eq!(
        lane.apply_commands(identity(0, 1), at(1), &[Bit::One], at(0)),
        Err(TransmitError::Time)
    );
    assert_eq!(lane.strict_from, None);
    assert_eq!(take_initial(&mut lane, 0), Ok(Bit::Zero));
}

#[test]
fn initialization_configuration_requires_an_unused_prepared_session() {
    let settings = initialization(true, 3, 100);
    let mut session = owner(0, 1);
    session.configure_initial_tpc(settings).unwrap();
    assert_eq!(
        session.configure_initial_tpc(settings),
        Err(TransmitError::Phase)
    );
    let prepared = owner(0, 1);
    let draft = input(&prepared);
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    install(&mut runtime, &mut ports, prepared);
    frame(&mut runtime, &mut ports, draft);
    let actual = runtime.bank.sessions.iter_mut().flatten().next().unwrap();
    assert_eq!(
        actual.configure_initial_tpc(settings),
        Err(TransmitError::Phase)
    );
}

#[test]
fn single_decision_is_copy_and_uses_the_bounded_feedback_lane() {
    fn assert_copy<T: Copy>() {}
    assert_copy::<TpcDecision>();
    let (_source, mut runtime, mut ports) = runtime(1, 1);
    install(&mut runtime, &mut ports, owner(0, 1));
    let decision = single_decision(identity(0, 1), 0);
    let id = ports.feedback.publish_decision(decision).unwrap();
    assert_eq!(ports.feedback.publish_decision(decision), Err(decision));
    assert_eq!(runtime.service().unwrap(), 1);
    let event = ports.feedback.try_event().unwrap();
    assert_eq!(event.id, id);
    assert!(matches!(event.outcome,
        TransmitCommandOutcome::Decision { decision: actual, result: Ok(()) } if actual == decision));
    let request = TpcRequest {
        identity: decision.identity,
        physical_slot_start: decision.physical_slot_start,
        synthesis_at: at(0),
    };
    assert_eq!(
        runtime
            .tpc_lane_mut(decision.identity)
            .unwrap()
            .take(request),
        Ok(Bit::One)
    );
    assert_eq!(
        runtime
            .tpc_lane_mut(decision.identity)
            .unwrap()
            .take(request),
        Err(TransmitError::TpcUnavailable)
    );
}

#[test]
fn single_decision_rejects_duplicate_and_conflicts_with_batch_without_replacing_it() {
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    install(&mut runtime, &mut ports, owner(0, 1));
    feedback(&mut runtime, &mut ports, identity(0, 1), 0, vec![Bit::Zero]);
    let decision = single_decision(identity(0, 1), 0);
    ports.feedback.publish_decision(decision).unwrap();
    runtime.service().unwrap();
    assert!(matches!(ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Decision { decision: actual, result: Err(TransmitError::Conflict) } if actual == decision));
    assert_eq!(
        runtime
            .tpc_lane_mut(decision.identity)
            .unwrap()
            .take(TpcRequest {
                identity: decision.identity,
                physical_slot_start: at(0),
                synthesis_at: at(0),
            }),
        Ok(Bit::Zero)
    );
    ports.feedback.publish_decision(decision).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Decision { result: Ok(()), .. }
    ));
    ports.feedback.publish_decision(decision).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Decision {
            result: Err(TransmitError::Conflict),
            ..
        }
    ));
}

#[test]
fn single_decision_checks_generation_alignment_and_exact_horizon() {
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    install(&mut runtime, &mut ports, owner(0, 1));
    for (decision, expected) in [
        (
            single_decision(identity(0, 2), 0),
            Err(TransmitError::Identity),
        ),
        (single_decision(identity(0, 1), 1), Err(TransmitError::Time)),
        (
            single_decision(identity(0, 1), TPC_SLOTS as u64 * CHIPS_PER_SLOT),
            Err(TransmitError::Time),
        ),
        (
            single_decision(identity(0, 1), (TPC_SLOTS as u64 - 1) * CHIPS_PER_SLOT),
            Ok(()),
        ),
    ] {
        ports.feedback.publish_decision(decision).unwrap();
        runtime.service().unwrap();
        match ports.feedback.try_event().unwrap().outcome {
            TransmitCommandOutcome::Decision {
                decision: actual,
                result,
            } => {
                assert_eq!(actual, decision);
                assert_eq!(result, expected);
            }
            _ => panic!("expected decision result"),
        }
    }
    let mut chips = [Complex32::default(); CHIPS_PER_SLOT as usize];
    runtime
        .bank
        .mix_slot(at(0), &mut chips, |_| panic!("unarmed owner requested TPC"))
        .unwrap();
    assert_eq!(runtime.bank.next_chip(), at(CHIPS_PER_SLOT));
    ports
        .feedback
        .publish_decision(single_decision(identity(0, 1), 0))
        .unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Decision {
            result: Err(TransmitError::Time),
            ..
        }
    ));
}

#[test]
fn single_decision_foreign_epoch_and_closed_port_return_the_original_value() {
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    let mut decision = single_decision(identity(0, 1), 0);
    decision.physical_slot_start.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert_eq!(ports.feedback.publish_decision(decision), Err(decision));
    decision.physical_slot_start = at(0);
    decision.identity.ue.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert_eq!(ports.feedback.publish_decision(decision), Err(decision));
    decision.identity = identity(0, 1);
    runtime.close();
    assert_eq!(ports.feedback.publish_decision(decision), Err(decision));
}
use crate::{
    common_tx::CommonTx,
    dedicated_tx::tests::{compile, identity, payloads, session},
    test_support::{at, count, source},
};

fn limits(capacity: usize, budget: usize) -> TransmitRuntimeLimits {
    TransmitRuntimeLimits {
        command_capacity: count(capacity),
        commands_per_step: count(budget),
        feedback_capacity: count(capacity),
        feedback_per_step: count(budget),
        maximum_frame_payload_bits: count(10_000),
        maximum_inflight_retained_bits: count(2_000_000),
    }
}
fn runtime(
    capacity: usize,
    budget: usize,
) -> (CommonTx, DedicatedTransmitRuntime, DedicatedTransmitPorts) {
    let source = source(false);
    let (runtime, ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(4),
        count(2_000_000),
        limits(capacity, budget),
    )
    .unwrap();
    (source, runtime, ports)
}
fn owner(slot: u32, configuration: u64) -> Box<DedicatedTransmitSession> {
    Box::new(session(
        compile(true, false, true, 3 + slot as u16, 0),
        identity(slot, configuration),
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        4,
    ))
}
fn input(owner: &DedicatedTransmitSession) -> FrameInput {
    let tfci = Tfci::new(3).unwrap();
    FrameInput {
        emission: None,
        retry_after: None,
        storage: Box::default(),
        identity: owner.identity(),
        start: owner.next_prepare(),
        tfci,
        transports: payloads(owner, tfci)
            .into_iter()
            .map(|(transport, bits)| OwnedTransportPayload { transport, bits })
            .collect(),
    }
}
fn install(
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    owner: Box<DedicatedTransmitSession>,
) {
    let key = owner.identity();
    assert!(ports.control.prepare(owner).is_ok());
    runtime.service().unwrap();
    assert!(
        matches!(ports.control.try_event().unwrap().outcome, TransmitCommandOutcome::Prepared(identity) if identity == key)
    );
}
fn frame(
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    input: FrameInput,
) {
    assert!(ports.control.frame(input).is_ok());
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame { result: Ok(_), .. }
    ));
}
fn feedback(
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    key: ReceiveIdentity,
    first: u64,
    commands: Vec<Bit>,
) {
    assert!(
        ports
            .feedback
            .publish(TpcPlan {
                identity: key,
                first_slot: at(first),
                commands
            })
            .is_ok()
    );
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
}

#[test]
fn worker_admission_is_distinct_from_publication_and_returns_buffers_and_exact_removed_owner() {
    let (mut source, mut runtime, mut ports) = runtime(4, 4);
    let owner = owner(0, 1);
    let address = &*owner as *const DedicatedTransmitSession;
    let input = input(&owner);
    let payload_address = input.transports[0].bits.as_ptr();
    let key = owner.identity();
    let prepared = ports.control.prepare(owner).ok().unwrap();
    assert!(runtime.bank.session(key).is_none());
    let framed = ports.control.frame(input).ok().unwrap();
    let armed = ports.control.arm(key).unwrap();
    assert_eq!(runtime.service(), Ok(3));
    assert_eq!(ports.control.try_event().unwrap().id, prepared);
    let event = ports.control.try_event().unwrap();
    assert_eq!(event.id, framed);
    let TransmitCommandOutcome::Frame { input, result } = event.outcome else {
        panic!("frame outcome");
    };
    assert!(result.is_ok());
    assert_eq!(input.transports[0].bits.as_ptr(), payload_address);
    let event = ports.control.try_event().unwrap();
    assert_eq!(event.id, armed);
    assert!(matches!(
        event.outcome,
        TransmitCommandOutcome::Armed { result: Ok(()), .. }
    ));
    feedback(&mut runtime, &mut ports, key, 0, vec![Bit::Zero; 15]);
    let mut output = vec![Complex32::default(); source.samples_per_slot()];
    runtime.generate_slot(&mut source, &mut output).unwrap();
    assert_eq!(
        runtime.bank.session(key).unwrap().counters().physical_slots,
        1
    );
    ports.control.remove(key).unwrap();
    runtime.service().unwrap();
    let TransmitCommandOutcome::Removed {
        result: Ok(removed),
        ..
    } = ports.control.try_event().unwrap().outcome
    else {
        panic!("removed owner");
    };
    assert_eq!(&*removed as *const DedicatedTransmitSession, address);
    assert_eq!(removed.phase(), TransmitPhase::Retired);
    assert!(runtime.bank.session(key).is_none());
}

#[test]
fn unread_acknowledgments_keep_credits_even_when_the_command_ring_has_room() {
    let (_, mut runtime, mut ports) = runtime(2, 1);
    let key = identity(0, 1);
    let first = ports.control.observe(key).unwrap();
    let second = ports.control.observe(key).unwrap();
    assert_eq!(runtime.service(), Ok(1));
    assert!(ports.control.observe(key).is_err());
    assert_eq!(ports.control.outstanding(), 2);
    assert_eq!(ports.control.try_event().unwrap().id, first);
    let third = ports.control.observe(key).unwrap();
    assert_eq!(runtime.service(), Ok(1));
    assert_eq!(ports.control.try_event().unwrap().id, second);
    assert_eq!(runtime.service(), Ok(1));
    assert_eq!(ports.control.try_event().unwrap().id, third);
    assert_eq!(ports.control.outstanding(), 0);
}

#[test]
fn independent_feedback_lane_survives_full_control_credits_and_both_service_budgets_are_finite() {
    let (_, mut runtime, mut ports) = runtime(1, 1);
    install(&mut runtime, &mut ports, owner(0, 1));
    ports.control.observe(identity(0, 1)).unwrap();
    runtime.service().unwrap();
    assert!(ports.control.observe(identity(0, 1)).is_err());
    assert!(
        ports
            .feedback
            .publish(TpcPlan {
                identity: identity(0, 1),
                first_slot: at(0),
                commands: vec![Bit::One; 2]
            })
            .is_ok()
    );
    assert_eq!(runtime.service(), Ok(1));
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
    assert_eq!(ports.control.outstanding(), 1);
}

#[test]
fn frame_rejection_returns_the_same_payload_and_preserves_transport_history() {
    let (_, mut runtime, mut ports) = runtime(2, 2);
    let owner = owner(0, 1);
    let mut input = input(&owner);
    input.transports[0].bits.push(Bit::One);
    let address = input.transports[0].bits.as_ptr();
    install(&mut runtime, &mut ports, owner);
    assert!(ports.control.frame(input).is_ok());
    runtime.service().unwrap();
    let TransmitCommandOutcome::Frame { input, result } =
        ports.control.try_event().unwrap().outcome
    else {
        panic!("frame outcome");
    };
    assert_eq!(result, Err(TransmitError::Payload));
    assert_eq!(input.transports[0].bits.as_ptr(), address);
    assert_eq!(
        runtime.bank.session(identity(0, 1)).unwrap().next_prepare(),
        at(0)
    );
    assert_eq!(ports.control.retained_bits(), 0);
}

#[test]
fn rejected_install_returns_owner_and_does_not_replace_peer_tpc_or_code_reservations() {
    let (_, mut runtime, mut ports) = runtime(2, 2);
    install(&mut runtime, &mut ports, owner(0, 1));
    feedback(&mut runtime, &mut ports, identity(0, 1), 0, vec![Bit::One]);
    let rejected = owner(0, 2);
    let address = &*rejected as *const DedicatedTransmitSession;
    assert!(ports.control.prepare(rejected).is_ok());
    runtime.service().unwrap();
    let TransmitCommandOutcome::PrepareRejected { error, session } =
        ports.control.try_event().unwrap().outcome
    else {
        panic!("rejected owner");
    };
    assert_eq!(error, TransmitError::Conflict);
    assert_eq!(&*session as *const DedicatedTransmitSession, address);
    assert_eq!(runtime.tpc[0].identity, Some(identity(0, 1)));
    assert_eq!(runtime.tpc[0].decisions[0].unwrap().command, Bit::One);
}

#[test]
fn feedback_generation_overlap_and_lateness_checks_are_atomic() {
    let (mut source, mut runtime, mut ports) = runtime(4, 4);
    let owner = owner(0, 1);
    let input = input(&owner);
    install(&mut runtime, &mut ports, owner);
    frame(&mut runtime, &mut ports, input);
    feedback(
        &mut runtime,
        &mut ports,
        identity(0, 1),
        0,
        vec![Bit::One, Bit::Zero],
    );
    assert!(
        ports
            .feedback
            .publish(TpcPlan {
                identity: identity(0, 1),
                first_slot: at(CHIPS_PER_SLOT),
                commands: vec![Bit::One, Bit::One]
            })
            .is_ok()
    );
    assert!(
        ports
            .feedback
            .publish(TpcPlan {
                identity: identity(0, 2),
                first_slot: at(2 * CHIPS_PER_SLOT),
                commands: vec![Bit::One]
            })
            .is_ok()
    );
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback {
            result: Err(TransmitError::Conflict),
            ..
        }
    ));
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback {
            result: Err(TransmitError::Identity),
            ..
        }
    ));
    assert!(runtime.tpc[0].decisions[2].is_none());
    ports.control.arm(identity(0, 1)).unwrap();
    runtime.service().unwrap();
    ports.control.try_event().unwrap();
    let mut output = vec![Complex32::default(); source.samples_per_slot()];
    runtime.generate_slot(&mut source, &mut output).unwrap();
    runtime.generate_slot(&mut source, &mut output).unwrap();
    assert!(
        ports
            .feedback
            .publish(TpcPlan {
                identity: identity(0, 1),
                first_slot: at(0),
                commands: vec![Bit::One]
            })
            .is_ok()
    );
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback {
            result: Err(TransmitError::Time),
            ..
        }
    ));
    assert_eq!(
        runtime.bank.session(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Armed
    );
}

#[test]
fn missing_tpc_fails_its_ue_without_reusing_a_command_or_stopping_peer_waveform() {
    let (mut source, mut runtime, mut ports) = runtime(4, 4);
    for slot in 0..2 {
        let owner = owner(slot, 1);
        let input = input(&owner);
        install(&mut runtime, &mut ports, owner);
        frame(&mut runtime, &mut ports, input);
        feedback(
            &mut runtime,
            &mut ports,
            identity(slot, 1),
            0,
            vec![Bit::Zero; if slot == 0 { 1 } else { 15 }],
        );
        ports.control.arm(identity(slot, 1)).unwrap();
        runtime.service().unwrap();
        ports.control.try_event().unwrap();
    }
    let mut output = vec![Complex32::default(); source.samples_per_slot()];
    for _ in 0..3 {
        runtime.generate_slot(&mut source, &mut output).unwrap();
    }
    assert_eq!(
        runtime.bank.session(identity(0, 1)).unwrap().failure(),
        Some(&TransmitError::TpcUnavailable)
    );
    assert_eq!(
        runtime.bank.session(identity(1, 1)).unwrap().phase(),
        TransmitPhase::Armed
    );
    let evidence = runtime
        .bank
        .session(identity(0, 1))
        .unwrap()
        .failure_evidence()
        .unwrap();
    assert_eq!(evidence.identity, identity(0, 1));
    assert_eq!(evidence.operation, TransmitOperation::Synthesis);
    assert_eq!(evidence.error, TransmitError::TpcUnavailable);
    assert_eq!(evidence.render_cursor, at(CHIPS_PER_SLOT));
    assert_eq!(evidence.phase, TransmitPhase::Armed);
    assert_eq!(evidence.counters.physical_slots, 1);
    assert!(
        runtime
            .bank
            .session(identity(1, 1))
            .unwrap()
            .failure_evidence()
            .is_none()
    );
    assert_eq!(
        runtime
            .bank
            .session(identity(1, 1))
            .unwrap()
            .counters()
            .physical_slots,
        3
    );
}

#[test]
fn expired_initial_tpc_retains_the_first_failure_window_through_removal() {
    let (mut source, mut runtime, mut ports) = runtime(4, 4);
    let mut session = owner(0, 1);
    session
        .configure_initial_tpc(initialization(true, 1, 2))
        .unwrap();
    let input = input(&session);
    let key = session.identity();
    install(&mut runtime, &mut ports, session);
    frame(&mut runtime, &mut ports, input);
    ports.control.arm(key).unwrap();
    runtime.service().unwrap();
    ports.control.try_event().unwrap();
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    for _ in 0..4 {
        runtime.generate_slot(&mut source, &mut samples).unwrap();
    }
    ports.control.remove(key).unwrap();
    runtime.service().unwrap();
    let TransmitCommandOutcome::Removed {
        result: Ok(session),
        ..
    } = ports.control.try_event().unwrap().outcome
    else {
        panic!("failed owner must remain recoverable")
    };
    let evidence = session.failure_evidence().unwrap();
    assert_eq!(evidence.identity, key);
    assert_eq!(evidence.error, TransmitError::TpcUnavailable);
    assert_eq!(evidence.operation, TransmitOperation::Synthesis);
    assert_eq!(evidence.render_cursor, at(2 * CHIPS_PER_SLOT));
    assert_eq!(evidence.anchor.time, at(0));
    assert_eq!(evidence.counters.physical_slots, 2);
    assert_eq!(
        evidence.initial_tpc_maximum_slots,
        std::num::NonZeroU32::new(2)
    );
    assert_eq!(session.phase(), TransmitPhase::Retired);
    assert!(runtime.available());
}

#[test]
fn closure_preserves_published_results_and_never_acknowledges_unprocessed_commands() {
    let (_, mut runtime, mut ports) = runtime(3, 1);
    let a = ports.control.observe(identity(0, 1)).unwrap();
    let b = ports.control.observe(identity(1, 1)).unwrap();
    runtime.service().unwrap();
    runtime.close();
    let event = ports.control.try_event().unwrap();
    assert_eq!(event.id, a);
    assert!(matches!(
        event.outcome,
        TransmitCommandOutcome::Observed { .. }
    ));
    let event = ports.control.try_event().unwrap();
    assert_eq!(event.id, b);
    assert!(matches!(event.outcome, TransmitCommandOutcome::Closed));
    assert!(ports.control.try_event().is_none());
    assert!(!ports.control.available());
    assert!(ports.control.observe(identity(0, 1)).is_err());
}

#[test]
fn producer_loss_closes_the_runtime_instead_of_running_with_stale_control() {
    let (_, mut runtime, ports) = runtime(2, 1);
    let DedicatedTransmitPorts {
        mut control,
        feedback,
    } = ports;
    control.observe(identity(0, 1)).unwrap();
    drop(feedback);
    assert_eq!(runtime.service(), Err(TransmitError::Closed));
    assert!(matches!(
        control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Closed
    ));
}

#[test]
fn pending_memory_budget_rejects_publication_before_ownership_transfer() {
    let source = source(false);
    let (_runtime, mut ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(4),
        count(2_000_000),
        TransmitRuntimeLimits {
            maximum_inflight_retained_bits: count(1),
            ..limits(2, 1)
        },
    )
    .unwrap();
    let session = owner(0, 1);
    let address = &*session as *const DedicatedTransmitSession;
    let returned = ports.control.prepare(session).err().unwrap();
    assert_eq!(&*returned as *const DedicatedTransmitSession, address);
    assert_eq!(ports.control.outstanding(), 0);
    assert_eq!(ports.control.retained_bits(), 0);
}

#[test]
fn exact_tpc_coordinates_survive_ring_wrap_without_replaying_old_decisions() {
    let mut lane = TpcLane {
        identity: Some(identity(0, 1)),
        anchor: at(512),
        anchor_cfn: 0,
        initialization: None,
        strict_from: None,
        decisions: [None; TPC_SLOTS],
        estimates: EstimateLane::default(),
        grouped: None,
    };
    for batch in 0..4 {
        let first = 512 + batch * 32 * CHIPS_PER_SLOT;
        let plan = TpcPlan {
            identity: identity(0, 1),
            first_slot: at(first),
            commands: vec![Bit::from(batch % 2 == 1); 32],
        };
        lane.apply(&plan, at(first)).unwrap();
        for slot in 0..32 {
            let request = TpcRequest {
                identity: identity(0, 1),
                physical_slot_start: at(first + slot * CHIPS_PER_SLOT),
                synthesis_at: at(first),
            };
            assert_eq!(lane.take(request), Ok(Bit::from(batch % 2 == 1)));
            assert_eq!(lane.take(request), Err(TransmitError::TpcUnavailable));
        }
    }
}

#[test]
fn actual_preparation_and_driver_submission_use_the_same_owned_dedicated_runtime() {
    use crate::radio_io::{PumpProgress, TxSettings, prepare_tx};
    use sdr_hal::{Radio, memory::MemoryRadio};
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(1000, count(1), count(1), count(2)),
    )
    .unwrap();
    let mut ports = preparation
        .dedicated_ports(count(2), count(4), count(2_000_000), limits(4, 4))
        .unwrap();
    assert!(preparation.dedicated_paired_with(&ports.control));
    let owner = owner(0, 1);
    let frame = input(&owner);
    assert!(ports.control.prepare(owner).is_ok());
    assert!(ports.control.frame(frame).is_ok());
    ports.control.arm(identity(0, 1)).unwrap();
    assert!(
        ports
            .feedback
            .publish(TpcPlan {
                identity: identity(0, 1),
                first_slot: at(0),
                commands: vec![Bit::Zero; 15]
            })
            .is_ok()
    );
    pump.start().unwrap();
    for _ in 0..15 {
        assert!(preparation.prepare_one().unwrap().is_some());
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        let block = peer.output.recv().unwrap();
        assert!(!block.samples.is_empty());
        assert!(
            block
                .samples
                .iter()
                .all(|sample| sample.re.is_finite() && sample.im.is_finite())
        );
    }
    assert_eq!(ports.control.outstanding(), 3);
    for _ in 0..3 {
        ports.control.try_event().unwrap();
    }
    ports.feedback.try_event().unwrap();
    ports.control.observe(identity(0, 1)).unwrap();
    ports.control.remove(identity(0, 1)).unwrap();
    preparation.prepare_one().unwrap().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Observed {
            state: Some((
                TransmitPhase::Armed,
                TransmitCounters {
                    consumed_frames: 1,
                    ..
                },
                None
            )),
            ..
        }
    ));
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Removed { result: Ok(_), .. }
    ));
    drop(preparation);
    assert!(!ports.control.available());
    assert!(!ports.feedback.available());
}

#[test]
fn delayed_initial_admission_returns_its_owner_with_an_exact_late_stage() {
    let (mut source, mut runtime, mut ports) = runtime(4, 4);
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    runtime.generate_slot(&mut source, &mut samples).unwrap();
    let owner = owner(0, 1);
    let identity = owner.identity();
    assert!(ports.control.prepare(owner).is_ok());
    runtime.service().unwrap();
    let TransmitCommandOutcome::PrepareRejected { error, session } =
        ports.control.try_event().unwrap().outcome
    else {
        panic!("expired initial owner must be returned")
    };
    assert_eq!(error, TransmitError::Late(TransmitTimingStage::Admission));
    assert_eq!(session.identity(), identity);
    assert_eq!(session.phase(), TransmitPhase::Prepared);
    let evidence = session.failure_evidence().unwrap();
    assert_eq!(evidence.identity, identity);
    assert_eq!(evidence.operation, TransmitOperation::Admission);
    assert_eq!(evidence.render_cursor, at(CHIPS_PER_SLOT));
    assert_eq!(evidence.requested, at(0));
    assert_eq!(evidence.anchor.time, at(0));
    assert_eq!(evidence.counters.physical_slots, 0);
    assert!(runtime.bank().session(identity).is_none());
    assert!(runtime.available());
}

#[test]
fn command_budget_delay_rejects_a_late_frame_and_arm_without_claiming_activation() {
    let (mut source, mut runtime, mut ports) = runtime(4, 1);
    let owner = owner(0, 1);
    let input = input(&owner);
    assert!(ports.control.prepare(owner).is_ok());
    assert!(ports.control.frame(input).is_ok());
    ports.control.arm(identity(0, 1)).unwrap();
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    assert_eq!(runtime.service(), Ok(1));
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Prepared(_)
    ));
    runtime.generate_slot(&mut source, &mut samples).unwrap();
    assert_eq!(runtime.service(), Ok(1));
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame {
            result: Err(TransmitError::Late(TransmitTimingStage::Frame)),
            ..
        }
    ));
    runtime.generate_slot(&mut source, &mut samples).unwrap();
    assert_eq!(runtime.service(), Ok(1));
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Armed {
            result: Err(TransmitError::Late(TransmitTimingStage::Arm)),
            ..
        }
    ));
    assert_eq!(
        runtime.bank.session(identity(0, 1)).unwrap().phase(),
        TransmitPhase::Prepared
    );
    let evidence = runtime
        .bank
        .session(identity(0, 1))
        .unwrap()
        .failure_evidence()
        .unwrap();
    assert_eq!(evidence.operation, TransmitOperation::Frame);
    assert_eq!(
        evidence.error,
        TransmitError::Late(TransmitTimingStage::Frame)
    );
    assert_eq!(evidence.render_cursor, at(CHIPS_PER_SLOT));
    assert_eq!(evidence.requested, at(0));
    assert_eq!(evidence.phase, TransmitPhase::Prepared);
    assert_eq!(
        runtime
            .bank
            .session(identity(0, 1))
            .unwrap()
            .counters()
            .physical_slots,
        0
    );
}
