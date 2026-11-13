// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod hsdpa;
mod packet_rabs;
mod release;
mod resources;
use crate::{
    common_tx::CommonTxPort,
    dedicated_bearers::FramePoolLimits,
    dedicated_controller::{DedicatedController, DedicatedControllerLimits, PreparedDedicated},
    dedicated_rx::DedicatedSampleRuntime,
    dedicated_tx::DedicatedTpcPort,
    rel99_admission::{
        AdmissionError, AdmissionLimits, AdmissionPhase, InitialAdmission, Rel99Admission,
    },
};
use umts_rlc::acknowledged::{ReceiveProvenance, ReceiveTime, SduTransmission};
use umts_types::time::CONNECTION_FRAME_MODULUS;

struct Environment {
    plan: Arc<CompiledRel99>,
    source: CommonTx,
    writer: crate::tx_evidence::Writer,
    common: CommonTxPort,
    radio: CommonSignalling,
    receive: DedicatedController,
    rx: DedicatedSampleRuntime,
    transmit: DedicatedTransmitRuntime,
    feedback: DedicatedTpcPort,
    admission: Rel99Admission,
    key: UeKey,
    identity: ReceiveIdentity,
}
fn limits() -> AdmissionLimits {
    AdmissionLimits {
        visits: count(2),
        command_results: count(2),
        prefill_frames: count(4),
        maximum_lead_frames: count(8),
        health_interval_chips: NonZeroU64::new(1_000_000).unwrap(),
    }
}
fn environment() -> (Environment, Box<InitialAdmission>) {
    environment_at(0)
}
fn environment_at(frame: u64) -> (Environment, Box<InitialAdmission>) {
    environment_with_cfn(frame, frame as u8)
}
fn environment_with_cfn(frame: u64, cfn: u8) -> (Environment, Box<InitialAdmission>) {
    environment_from_fixture(
        frame,
        cfn,
        fixture_parts_with_plan(true, false, false, true, |radio, key| {
            Arc::new(
                CompiledRel99::new(
                    packet_only_configuration(&radio.retained_signalling(key).unwrap()),
                    count(200_000),
                    TransportPlanLimits {
                        maximum_coded_bits: count(200_000),
                        maximum_mapping_entries: count(300_000),
                        decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
                    },
                )
                .unwrap(),
            )
        }),
    )
}
fn environment_with_plan(
    build: impl FnOnce(&CommonSignalling, UeKey) -> Arc<CompiledRel99>,
) -> (Environment, Box<InitialAdmission>) {
    environment_from_fixture(
        0,
        0,
        fixture_parts_with_plan(true, false, false, true, build),
    )
}
fn environment_from_fixture(
    frame: u64,
    cfn: u8,
    fixture: Fixture,
) -> (Environment, Box<InitialAdmission>) {
    let Fixture {
        mut source,
        mut radio,
        key,
        plan,
        bank,
        runtime,
        ports,
    } = fixture;
    let identity = bank.identity();
    let timing = MappingActivation {
        uplink: FrameAnchor {
            time: at(frame * CHIPS_PER_FRAME),
            cfn,
        },
        uplink_frame: (frame / CONNECTION_FRAME_MODULUS) * CONNECTION_FRAME_MODULUS
            + u64::from(cfn),
        downlink: FrameAnchor {
            time: at(frame * CHIPS_PER_FRAME),
            cfn,
        },
        downlink_frame: (frame / CONNECTION_FRAME_MODULUS) * CONNECTION_FRAME_MODULUS
            + u64::from(cfn),
    };
    let bank = if frame == 0 && cfn == 0 {
        bank
    } else {
        let mut previous = bank;
        previous.retire();
        DedicatedBearers::from_compiled(
            identity,
            timing,
            plan.clone(),
            previous.into_endpoints().ok().unwrap(),
            BearerLimits {
                maximum_transport_bits: count(10_000),
                output_slots: count(8),
                maximum_sdu_bytes: radio.limits.maximum_sdu_bytes,
                maximum_buffer_bytes: 2_000_000,
            },
        )
        .unwrap()
    };
    let writer = source.submission_writer().unwrap();
    let common = source.control_port(count(8), count(8)).unwrap();
    let pool = radio
        .allocate_frame_pool(
            identity,
            plan.clone(),
            &ports.control,
            FramePoolLimits {
                payload_slots: count(1),
                receipt_slots: count(32),
                maximum_buffer_bytes: 2_000_000,
            },
        )
        .unwrap();
    let receiver = mapping::retirement::receive_owner(
        identity,
        MappingActivation {
            uplink: bank.frame_anchor(),
            uplink_frame: frame,
            downlink: bank.frame_anchor(),
            downlink_frame: frame,
        },
        &plan,
    );
    let (rx, port) = mapping::retirement::receive_runtime(key.epoch);
    let receive = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 4_000_000,
        },
    );
    let transmit = Box::new(
        DedicatedTransmitSession::new(
            identity,
            bank.frame_anchor(),
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
        RadioBearerSetupProcedure::initial(
            key,
            radio.next_bearer_setup_transaction(key).unwrap(),
            InitialDchSetup::new(
                plan.configuration().clone(),
                radio.retained_signalling(key).unwrap(),
                super::super::fixture::rabs(false, true),
            )
            .unwrap(),
            at(0),
            at(100 * CHIPS_PER_FRAME),
            NonZeroU64::new(100 * CHIPS_PER_FRAME).unwrap(),
        )
        .unwrap(),
    );
    let request = Box::new(InitialAdmission {
        receive: Box::new(PreparedDedicated {
            receiver,
            bearers: Box::new(bank),
        }),
        transmit,
        procedure,
        pool,
    });
    let admission = Rel99Admission::new(ports.control, count(receive.capacity()), limits())
        .ok()
        .unwrap();
    (
        Environment {
            plan,
            source,
            writer,
            common,
            radio,
            receive,
            rx,
            transmit: runtime,
            feedback: ports.feedback,
            admission,
            key,
            identity,
        },
        request,
    )
}
fn step(env: &mut Environment, now: ChipInstant) {
    env.rx.service(|_, result| {
        result.unwrap();
    });
    env.transmit.service().unwrap();
    let result = env
        .admission
        .step_with_policy(
            now,
            &mut env.radio,
            &mut env.receive,
            &mut env.common,
            &mut crate::rel99_admission::TrafficFramePolicy::new(
                crate::dedicated_bearers::EmptyTransparentTraffic,
                count(1024),
            ),
        )
        .unwrap();
    assert!(result.visits <= limits().visits.get());
    assert!(result.command_results <= limits().command_results.get());
    assert!(result.failure.is_none(), "{result:?}");
}
fn until(env: &mut Environment, phase: AdmissionPhase, now: ChipInstant) {
    for _ in 0..128 {
        step(env, now);
        if env.admission.phase(env.identity) == Some(phase) {
            return;
        }
    }
    panic!(
        "admission did not reach {phase:?}: {:?} {:?}",
        env.admission.phase(env.identity),
        env.admission.failure(env.identity)
    );
}
fn started() -> Environment {
    let (mut env, request) = environment();
    env.admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap();
    until(&mut env, AdmissionPhase::Prefilling, at(0));
    env.feedback
        .publish(TpcPlan {
            identity: env.identity,
            first_slot: at(0),
            commands: vec![Bit::Zero; 60],
        })
        .ok()
        .unwrap();
    env.transmit.service().unwrap();
    assert!(matches!(
        env.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
    until(&mut env, AdmissionPhase::Starting, at(0));
    env
}

#[test]
fn initial_admission_waits_for_real_rx_tx_and_complete_driver_evidence() {
    let mut env = started();
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().phase(),
        SetupPhase::Prepared
    );
    assert!(
        env.radio
            .registry
            .get(env.key)
            .unwrap()
            .bearer_change
            .is_none()
    );
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    let submission = env
        .admission
        .initial_frame_submission(env.identity)
        .unwrap();
    let maximum_slots =
        limits().maximum_lead_frames.get() * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) as usize;
    for _ in 0..maximum_slots {
        let window = env
            .transmit
            .generate_slot(&mut env.source, &mut samples)
            .unwrap();
        let now = env.source.next_chip();
        step(&mut env, now);
        assert_eq!(
            env.admission.procedure(env.identity).unwrap().phase(),
            SetupPhase::Prepared
        );
        assert!(env.writer.accepted(window));
        if submission.submitted() {
            break;
        }
    }
    assert!(submission.submitted());
    let now = env.source.next_chip();
    until(&mut env, AdmissionPhase::AwaitingComplete, now);
    let startup = env.admission.startup_progress(env.identity).unwrap();
    assert!(startup.bootstrap_submitted);
    assert_eq!(startup.publication_attempts, 1);
    assert_eq!(startup.last_backpressure, None);
    assert_eq!(
        startup.bootstrap.unwrap().emission,
        Some(crate::dedicated_tx::EmissionProgress::Generated)
    );
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().phase(),
        SetupPhase::AwaitingComplete
    );
    assert_eq!(env.radio.dedicated_route(env.key), None);
    drop(submission);
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, now);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    assert!(
        env.radio
            .registry
            .get(env.key)
            .unwrap()
            .protection
            .blocks_status()
    );
    assert!(
        env.admission
            .take_retired(env.identity)
            .unwrap()
            .transmit
            .is_some()
    );
    assert_eq!(env.radio.retire_failed_radio(env.identity), Ok(true));
    assert_eq!(env.radio.phase(env.key), None);
}

#[test]
fn startup_retains_publication_backpressure_after_bootstrap_delivery() {
    let mut env = started();
    for _ in 0..4 {
        env.radio
            .registry
            .get_mut(env.key)
            .unwrap()
            .control
            .enqueue(env.key, Bits::new(&[0; 8], 64).unwrap())
            .unwrap();
    }
    let submission = env
        .admission
        .initial_frame_submission(env.identity)
        .unwrap();
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    let maximum_slots =
        limits().maximum_lead_frames.get() * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) as usize;
    for _ in 0..maximum_slots {
        let window = env
            .transmit
            .generate_slot(&mut env.source, &mut samples)
            .unwrap();
        assert!(env.writer.accepted(window));
        let now = env.source.next_chip();
        step(&mut env, now);
        if submission.submitted() {
            break;
        }
    }
    assert!(submission.submitted());
    let now = env.source.next_chip();
    for _ in 0..16 {
        step(&mut env, now);
    }
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Starting)
    );
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().phase(),
        SetupPhase::Prepared
    );
    let startup = env
        .admission
        .startup_progress(env.identity)
        .unwrap()
        .clone();
    assert!(startup.bootstrap_submitted);
    assert!(startup.publication_attempts > 0);
    assert_eq!(
        startup.last_backpressure,
        Some((now, SignallingError::Capacity))
    );
    assert_eq!(
        startup.publication_readiness,
        env.radio.bearer_setup_readiness(env.key)
    );
    drop(submission);
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, now);
    let retired = env.admission.take_retired(env.identity).unwrap();
    assert_eq!(retired.failed_phase, Some(AdmissionPhase::Starting));
    assert_eq!(retired.startup, Some(startup));
}

#[test]
fn common_only_driver_progress_cannot_publish_initial_bearer_setup() {
    let mut env = started();
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
        assert!(
            env.writer
                .accepted(env.source.generate_slot(&mut samples).unwrap())
        );
    }
    let now = env.source.next_chip();
    for _ in 0..16 {
        step(&mut env, now);
    }
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Starting)
    );
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().receipt(),
        None
    );
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, now);
    let retired = env.admission.take_retired(env.identity).unwrap();
    assert_eq!(retired.failed_phase, Some(AdmissionPhase::Starting));
    let startup = retired.startup.unwrap();
    assert!(startup.packets_prepared && startup.circuits_prepared);
    assert!(!startup.bootstrap_submitted);
    assert_eq!(startup.publication_attempts, 0);
    let proof = startup.bootstrap.unwrap();
    assert_ne!(
        proof.emission,
        Some(crate::dedicated_tx::EmissionProgress::Generated)
    );
    assert!(proof.resolved_until.unwrap().index >= proof.next.index);
}

#[test]
fn cancellation_at_every_unpublished_stage_reclaims_actual_owners_and_cell_pool() {
    for phase in [
        AdmissionPhase::Preparing,
        AdmissionPhase::ArmingReceive,
        AdmissionPhase::Prefilling,
        AdmissionPhase::ArmingTransmit,
        AdmissionPhase::Starting,
    ] {
        let (mut env, request) = environment();
        let charged = env.radio.ledger.used().queue_bytes;
        let pool_bytes = request.pool.storage_bytes() as u64;
        env.admission
            .begin_initial(request, &env.radio, &env.receive)
            .unwrap();
        if phase != AdmissionPhase::Preparing {
            until(&mut env, phase, at(0));
        }
        env.admission.cancel(env.identity).unwrap();
        until(&mut env, AdmissionPhase::Retired, at(0));
        assert_eq!(env.radio.ledger.used().queue_bytes, charged - pool_bytes);
        assert_eq!(env.receive.used_bearer_bytes(), 0);
        assert_eq!(env.radio.dedicated_route(env.key), None);
        let retired = env.admission.take_retired(env.identity).unwrap();
        assert_eq!(retired.error, AdmissionError::Cancelled);
        assert_eq!(retired.failed_phase, Some(phase));
        assert_eq!(
            retired.procedure.phase(),
            SetupPhase::AbortedBeforePublication
        );
        assert!(retired.receive.is_some() || retired.unadmitted_receive.is_some());
        assert!(retired.transmit.is_some());
    }
}

#[test]
fn canonical_plan_rejection_returns_the_original_request_without_staging() {
    let (mut env, mut request) = environment();
    let other_plan = Arc::new(
        CompiledRel99::new(
            request.transmit.plan().configuration().clone(),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    request.transmit = Box::new(
        DedicatedTransmitSession::new(
            env.identity,
            request.transmit.frame_anchor(),
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
    let used = env.radio.ledger.used();
    let rejected = env
        .admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap_err();
    assert_eq!(rejected.error, AdmissionError::Configuration);
    assert_eq!(pointer, std::ptr::from_ref(rejected.request.as_ref()));
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    assert_eq!(env.radio.ledger.used(), used);
    env.radio.retire_frame_pool(rejected.request.pool).unwrap();
}

#[test]
fn expiry_before_publication_does_not_leave_registered_radio_owners() {
    let (mut env, request) = environment();
    env.admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap();
    until(&mut env, AdmissionPhase::Retired, at(100 * CHIPS_PER_FRAME));
    assert_eq!(
        env.admission.failure(env.identity),
        Some(&AdmissionError::Time)
    );
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    assert!(
        env.radio
            .registry
            .get(env.key)
            .unwrap()
            .bearer_change
            .is_none()
    );
}

#[test]
fn successful_bound_completion_switches_only_its_signalling_route_and_waits_for_rlc_ack() {
    let mut env = started();
    let (mut packet_plane, packet_route) = super::packet::prepare(env.identity, env.plan.clone());
    assert!(
        packet_plane
            .activate(packet_route, &mut env.receive)
            .is_err()
    );
    assert!(
        !env.receive
            .with_active_bank(env.identity, |bank| bank.user_bearers_ready())
            .unwrap()
    );
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
        assert!(
            env.writer.accepted(
                env.transmit
                    .generate_slot(&mut env.source, &mut samples)
                    .unwrap()
            )
        );
        let now = env.source.next_chip();
        step(&mut env, now);
    }
    let now = env.source.next_chip();
    until(&mut env, AdmissionPhase::AwaitingComplete, now);
    let receipt = env
        .admission
        .procedure(env.identity)
        .unwrap()
        .receipt()
        .unwrap();
    let transaction = env.admission.procedure(env.identity).unwrap().transaction();
    let mut sender = crate::signalling::tests::peer(&env.source);
    sender.enqueue(&super::super::COMPLETE).unwrap();
    sender
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    super::super::publish(&mut env.radio, env.key, receipt, &mut sender);
    let (kind, held_status) = sender
        .publish_next(now, |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    assert_eq!(kind, umts_rlc::acknowledged::PublicationKind::Status);
    let received = now.index + 100;
    let complete = super::super::user_setup::response(transaction, 3);
    super::super::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut sender,
        &super::super::clear(complete),
        received,
        ReceiveProvenance::Path(env.identity.configuration),
        Some(at(received)),
    );
    let serviced = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            serviced,
            super::super::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    step(&mut env, serviced);
    assert!(
        env.receive
            .with_active_bank(env.identity, |bank| bank.user_bearers_ready())
            .unwrap()
    );
    env.radio
        .with_bearer_access(env.key, |ciphers, _, _| {
            assert!(ciphers.has_user_bearers(env.key).unwrap());
            assert_eq!(
                ciphers
                    .selected(
                        env.key,
                        20,
                        umts_types::Direction::Downlink,
                        Position::Sequence(3 << 12)
                    )
                    .unwrap(),
                umts_security::ciphering::CipherUse::Data {
                    origin: None,
                    count: 3 << 12
                }
            );
        })
        .unwrap();
    assert_eq!(env.radio.dedicated_route(env.key), Some(env.identity));
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::AwaitingCommandAck)
    );
    assert!(matches!(
        env.radio.bearer_setup_transmission(env.key, receipt),
        Ok(SduTransmission::InFlight {
            delimited: true,
            ..
        })
    ));
    env.radio
        .receive_srb_payload(
            env.key,
            SignallingBearer::ControlAm,
            ReceiveTime::simultaneous(serviced),
            Bits::new(&held_status, held_status.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
    step(&mut env, serviced);
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Active)
    );
    super::packet::exercise(
        &mut packet_plane,
        packet_route,
        &mut env.receive,
        env.identity,
    );
    assert!(
        env.radio
            .registry
            .get(env.key)
            .unwrap()
            .bearer_change
            .is_none()
    );
    assert_eq!(
        env.radio.publish_bearer(
            env.key,
            SignallingBearer::ControlAm,
            &mut env.source,
            serviced,
            at(8 * CHIPS_PER_FRAME)
        ),
        Err(SignallingError::Phase)
    );
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, serviced);
}

#[test]
fn admission_route_cannot_be_selected_from_an_unproven_completion() {
    let (mut env, request) = environment();
    assert_eq!(
        env.radio.select_dedicated_route(env.identity),
        Err(SignallingError::Phase)
    );
    env.radio.retire_frame_pool(request.pool).unwrap();
    assert_eq!(env.radio.dedicated_route(env.key), None);
}

#[test]
fn initial_admission_waits_for_the_bounded_physical_prefill_window_before_staging() {
    let (mut env, request) = environment_at(64);
    env.admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap();
    for _ in 0..16 {
        step(&mut env, at(0));
    }
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Preparing)
    );
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    step(&mut env, at(60 * CHIPS_PER_FRAME));
    assert!(env.receive.used_bearer_bytes() > 0);
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, at(60 * CHIPS_PER_FRAME));
}

#[test]
fn cancelling_a_retired_admission_cannot_resurrect_released_pool_state() {
    let (mut env, request) = environment();
    env.admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap();
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, at(0));
    assert_eq!(
        env.admission.cancel(env.identity),
        Err(AdmissionError::Phase)
    );
    step(&mut env, at(0));
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Retired)
    );
}

#[test]
fn a_same_identity_pool_from_another_ledger_cannot_substitute_the_cell_lease() {
    use crate::signalling::{ResourceLedger, Resources};
    let (mut env, mut request) = environment();
    let mut ledger = ResourceLedger::new(
        env.key.epoch,
        Resources {
            queue_bytes: 4_000_000,
            ..Resources::default()
        },
        8,
    );
    let Fixture {
        ports,
        runtime: _runtime,
        ..
    } = fixture_parts(true, false, false, true);
    let foreign = crate::dedicated_bearers::FramePool::new(
        env.identity,
        request.pool.plan().clone(),
        &ports.control,
        FramePoolLimits {
            payload_slots: count(1),
            receipt_slots: count(32),
            maximum_buffer_bytes: 2_000_000,
        },
        &mut ledger,
    )
    .unwrap();
    let original = std::mem::replace(&mut request.pool, foreign);
    assert!(env.radio.owns_frame_pool(&original));
    assert!(!env.radio.owns_frame_pool(&request.pool));
    let rejected = env
        .admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap_err();
    assert_eq!(rejected.error, AdmissionError::Identity);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    rejected.request.pool.retire(&mut ledger).unwrap();
    env.radio.retire_frame_pool(original).unwrap();
}

#[test]
fn the_same_driver_cannot_substitute_another_tx_command_runtime() {
    let (mut env, request) = environment();
    let (_runtime, ports) = DedicatedTransmitRuntime::new(
        &env.source,
        count(2),
        count(4),
        count(2_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(8),
            commands_per_step: count(8),
            feedback_capacity: count(2),
            feedback_per_step: count(2),
            maximum_frame_payload_bits: count(10_000),
            maximum_inflight_retained_bits: count(2_000_000),
        },
    )
    .unwrap();
    let mut foreign = Rel99Admission::new(ports.control, count(env.receive.capacity()), limits())
        .ok()
        .unwrap();
    let rejected = foreign
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap_err();
    assert_eq!(rejected.error, AdmissionError::Identity);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    env.radio.retire_frame_pool(rejected.request.pool).unwrap();
}

#[test]
fn initial_cfn_must_match_the_advertised_doff_and_actual_cell_sfn() {
    let (mut env, request) = environment_with_cfn(0, 4);
    let rejected = env
        .admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap_err();
    assert_eq!(rejected.error, AdmissionError::Time);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    env.radio.retire_frame_pool(rejected.request.pool).unwrap();
}

#[test]
fn worker_closure_retains_unreturned_transmitter_ownership_and_pool_charge() {
    let mut env = started();
    for _ in 0..4 {
        step(&mut env, at(0));
    }
    let charged = env.radio.ledger.used().queue_bytes;
    env.transmit.close();
    for _ in 0..32 {
        env.rx.service(|_, result| {
            result.unwrap();
        });
        let report = env
            .admission
            .step(
                at(0),
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                |opportunity| opportunity.idle,
                |_, _| Ok(()),
            )
            .unwrap();
        assert!(report.failure.is_none());
    }
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Stopping)
    );
    assert_eq!(
        env.admission.failure(env.identity),
        Some(&AdmissionError::Closed)
    );
    assert_eq!(env.radio.ledger.used().queue_bytes, charged);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    assert!(matches!(
        env.admission.take_retired(env.identity),
        Err(AdmissionError::Phase)
    ));
}
