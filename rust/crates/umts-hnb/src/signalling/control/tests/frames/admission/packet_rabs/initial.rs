// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    core_link::{
        CoreLink,
        tests::{Mock, Peer as CorePeer, ready, register},
    },
    rab_handoff::RabHandoff,
    radio_resources::RadioResources,
    rel99_admission::{InitialPacketError, InitialRabHost, TrafficFramePolicy},
    rel99_planner::{ConfiguredPolicy, tests::admission as owners},
};
use umts_rrc::establishment::TransactionId;

mod circuits;
mod release;

const INITIAL_RAB: u8 = 9;
const SECOND_RAB: u8 = INITIAL_RAB + 1;
const INITIAL_RATE: u32 = 12_800;

struct InitialEnvironment {
    source: CommonTx,
    writer: crate::tx_evidence::Writer,
    common: CommonTxPort,
    radio: CommonSignalling,
    core: CoreLink<Mock>,
    peer: CorePeer,
    context: ContextId,
    resources: RadioResources,
    receive: DedicatedController,
    rx: DedicatedSampleRuntime,
    transmit: DedicatedTransmitRuntime,
    feedback: DedicatedTpcPort,
    admission: Rel99Admission,
    packets: PacketRabs<Memory>,
    key: UeKey,
    identity: ReceiveIdentity,
    request: RabRequest,
    sender: umts_rlc::acknowledged::AmEntity,
}

fn environment() -> InitialEnvironment {
    environment_with(1, 2)
}

fn environment_with(items: usize, route_capacity: usize) -> InitialEnvironment {
    let requested = (0..items)
        .map(|index| {
            let mut requested = item(INITIAL_RAB + u8::try_from(index).unwrap());
            requested.first.parameters.as_mut().unwrap().maximum =
                qos::MaximumRates::new(&[INITIAL_RATE]).unwrap();
            requested
        })
        .collect();
    environment_for(Domain::Ps, requested, route_capacity)
}

fn environment_for(
    domain: Domain,
    requested: Vec<SetupOrModify>,
    route_capacity: usize,
) -> InitialEnvironment {
    environment_for_capabilities(domain, requested, route_capacity, domain == Domain::Cs)
}

fn environment_for_capabilities(
    domain: Domain,
    requested: Vec<SetupOrModify>,
    route_capacity: usize,
    expanded_transport: bool,
) -> InitialEnvironment {
    let mut source = crate::test_support::source(false);
    let mut radio = crate::signalling::tests::controller(&source);
    let (mut core, peer) = ready(&mut radio);
    let (key, context, _) = register(&mut core, &peer, &mut radio, &source, 0, 3);
    radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    let mut sender = crate::signalling::tests::peer(&source);
    sender.enqueue(&crate::signalling::tests::COMPLETE).unwrap();
    sender
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    if expanded_transport {
        use umts_rrc_schema::information_elements as ie;
        let mut information = crate::test_support::capabilities::information();
        information.rrc_transaction_identifier = Some(ie::RRCTransactionIdentifier(2));
        let transport = &mut information
            .ue_radio_access_capability
            .as_mut()
            .unwrap()
            .transport_channel_capability;
        transport.ul_trans_ch_capability.max_simultaneous_trans_chs =
            ie::MaxSimultaneousTransChsUL::e8;
        transport.ul_trans_ch_capability.max_transmitted_blocks = ie::MaxTransportBlocksUL::tb8;
        transport.ul_trans_ch_capability.max_number_of_tfc = ie::MaxNumberOfTFCUL::tfc128;
        transport.dl_trans_ch_capability.max_simultaneous_trans_chs =
            ie::MaxSimultaneousTransChsDL::e8;
        transport
            .dl_trans_ch_capability
            .max_received_transport_blocks = ie::MaxTransportBlocksDL::tb8;
        transport.dl_trans_ch_capability.max_number_of_tfc = ie::MaxNumberOfTFCDL::tfc128;
        crate::signalling::tests::send_capability_report_information(
            &mut radio,
            key,
            &mut sender,
            information,
        );
    } else {
        crate::signalling::tests::send_capability_report(&mut radio, key, &mut sender);
    }
    assert_eq!(radio.process_one(), Some(Ok(key)));
    let ranap = Assignment {
        setup: requested,
        release: vec![],
    }
    .encode(WireLimits::default())
    .unwrap();
    peer.rua(match domain {
        Domain::Cs => rua::Message::Direct {
            domain,
            context,
            ranap,
        },
        Domain::Ps => rua::Message::Connect {
            domain,
            context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap,
        },
    });
    let mut handoff = RabHandoff::new(WireLimits::default());
    core.drive(ControlTime(6), &mut radio, |_, event| {
        handoff.offer(event).map(|_| ())
    })
    .unwrap();
    let pending = handoff.take().unwrap().unwrap();
    let request = pending.request();
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    let retained = radio.retained_signalling(key).unwrap();
    let control = retained.iter().find(|bearer| bearer.id == 2).unwrap();
    config.rel99.physical.planner.signalling.um_uplink_bits = control.uplink_pdu_bits;
    config.rel99.physical.planner.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let am = config.rel99.physical.planner.services.packet[0]
        .am
        .as_mut()
        .unwrap();
    am.ue.uplink_window = 32;
    am.ue.downlink_window = 32;
    let buffers = &mut am.buffers;
    buffers.maximum_sdu_bytes = 2048;
    buffers.maximum_buffer_bytes = 100_000;
    let policy = ConfiguredPolicy::new(&config).unwrap();
    let cell = crate::test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let (transmit, ports) = owners::transmit(&source);
    let (rx, receive) = owners::receive(key.epoch, 8);
    let mut admission = Rel99Admission::new(ports.control, count(8), owners::admission_limits())
        .ok()
        .unwrap();
    let identity = InitialRabHost {
        gateway: core.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    }
    .begin(pending, owners::settings(key.epoch))
    .unwrap_or_else(|error| panic!("{:?}", error.error));
    let writer = source.submission_writer().unwrap();
    let common = source.control_port(count(8), count(8)).unwrap();
    InitialEnvironment {
        source,
        writer,
        common,
        radio,
        core,
        peer,
        context,
        resources,
        receive,
        rx,
        transmit,
        feedback: ports.feedback,
        admission,
        packets: PacketRabs::new(
            PacketBearers::new(
                PacketPlane::new(
                    Memory::default(),
                    PacketLimits {
                        routes: count(route_capacity),
                        packets_per_route: count(2),
                        maximum_payload_bytes: count(1500),
                        maximum_storage_bytes: 1_000_000,
                        receive_budget: count(2),
                        route_visits: count(2),
                    },
                )
                .unwrap(),
            ),
            None,
            NonZeroU16::new(3).unwrap(),
        )
        .unwrap(),
        key,
        identity,
        request,
        sender,
    }
}

fn step(env: &mut InitialEnvironment, now: ChipInstant) {
    env.rx.service(|_, result| {
        result.unwrap();
    });
    env.transmit.service().unwrap();
    let report = env
        .admission
        .step_with_core(
            now,
            &mut env.radio,
            &mut env.receive,
            &mut env.common,
            &mut TrafficFramePolicy::new(
                crate::dedicated_bearers::EmptyTransparentTraffic,
                count(1024),
            ),
            env.core.gateway(),
        )
        .unwrap();
    assert!(report.failure.is_none(), "{report:?}");
}

fn until(env: &mut InitialEnvironment, phase: AdmissionPhase, now: ChipInstant) {
    for _ in 0..128 {
        step(env, now);
        if env.admission.phase(env.identity) == Some(phase) {
            return;
        }
        if phase != AdmissionPhase::Retired
            && matches!(
                env.admission.phase(env.identity),
                Some(AdmissionPhase::Stopping | AdmissionPhase::Retired)
            )
        {
            phase_failure(env, phase, now);
        }
    }
    phase_failure(env, phase, now);
}

fn phase_failure(env: &InitialEnvironment, phase: AdmissionPhase, now: ChipInstant) -> ! {
    panic!(
        "expected {phase:?}, got {:?}, failure {:?}, procedure {:?}, receipt {:?}, now {now:?}, source {:?}, receive {:?}, tx {:?}, packets {:?}, physical {:?}",
        env.admission.phase(env.identity),
        env.admission.failure(env.identity),
        env.admission
            .procedure(env.identity)
            .map(|procedure| procedure.phase()),
        env.admission
            .procedure(env.identity)
            .and_then(|procedure| procedure.receipt()),
        env.source.next_chip(),
        env.receive.phase(env.identity),
        env.transmit
            .bank()
            .session(env.identity)
            .map(|session| (session.phase(), session.counters())),
        env.admission.packet_progress(env.identity),
        env.admission
            .procedure(env.identity)
            .map(|procedure| procedure.configuration().physical()),
    );
}

fn start(env: &mut InitialEnvironment) {
    until(env, AdmissionPhase::Prefilling, at(0));
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
    until(env, AdmissionPhase::Starting, at(0));
}

fn complete_radio(env: &mut InitialEnvironment) -> ChipInstant {
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
        replenish_feedback(env);
        assert!(
            env.writer.accepted(
                env.transmit
                    .generate_slot(&mut env.source, &mut samples)
                    .unwrap()
            )
        );
        let now = env.source.next_chip();
        step(env, now);
    }
    let now = env.source.next_chip();
    until(env, AdmissionPhase::AwaitingComplete, now);
    let procedure = env.admission.procedure(env.identity).unwrap();
    let receipt = procedure.receipt().unwrap();
    let transaction = procedure.transaction();
    control::publish(&mut env.radio, env.key, receipt, &mut env.sender);
    let (kind, status) = env
        .sender
        .publish_next(now, |publication| Ok::<_, ()>(publication.bytes.to_vec()))
        .unwrap()
        .unwrap();
    assert_eq!(kind, umts_rlc::acknowledged::PublicationKind::Status);
    let received = now.index + 100;
    let mut complete = control::user_setup::response(transaction, 3);
    let umts_rrc_schema::class_definitions::ULDCCHMessageType::radioBearerSetupComplete(body) =
        &mut complete
    else {
        unreachable!()
    };
    body.count_c_activation_time = Some(umts_rrc_schema::information_elements::ActivationTime(208));
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut env.sender,
        &control::clear(complete),
        received,
        ReceiveProvenance::Path(env.identity.configuration),
        Some(at(received)),
    );
    let serviced = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            serviced,
            control::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    step(env, serviced);
    env.radio
        .receive_srb_payload(
            env.key,
            SignallingBearer::ControlAm,
            ReceiveTime::simultaneous(serviced),
            Bits::new(&status, status.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
    until(env, AdmissionPhase::Active, serviced);
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Active)
    );
    assert!(
        env.receive
            .with_active_bank(env.identity, |bank| bank.user_bearers_ready())
            .unwrap()
    );
    serviced
}

fn replenish_feedback(env: &mut InitialEnvironment) {
    const FEEDBACK_WINDOW_SLOTS: u64 = 30;
    let slot = env.source.next_chip().index / CHIPS_PER_SLOT;
    if slot == 0 || !slot.is_multiple_of(FEEDBACK_WINDOW_SLOTS) {
        return;
    }
    env.feedback
        .publish(TpcPlan {
            identity: env.identity,
            first_slot: at((slot + FEEDBACK_WINDOW_SLOTS) * CHIPS_PER_SLOT),
            commands: vec![Bit::Zero; FEEDBACK_WINDOW_SLOTS as usize],
        })
        .ok()
        .unwrap();
    env.transmit.service().unwrap();
    assert!(matches!(
        env.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
}

fn prepare(env: &mut InitialEnvironment) {
    let progress = env
        .admission
        .prepare_packets(env.identity, env.core.gateway(), &mut env.packets)
        .unwrap();
    assert_eq!(
        (progress.items, progress.prepared, progress.recorded),
        (1, 1, 0)
    );
}

#[test]
fn frame_queries_preserve_committed_configuration_for_idle_inspection() {
    let mut env = environment();
    prepare(&mut env);
    start(&mut env);
    complete_radio(&mut env);
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    let mut observed_query = false;
    for _ in 0..umts_types::time::SLOTS_PER_FRAME {
        assert!(env.admission.committed_radio(env.identity).is_ok());
        if env.admission.active_radio(env.identity).is_err() {
            observed_query = true;
            break;
        }
        replenish_feedback(&mut env);
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
    assert!(observed_query);
    env.admission.cancel(env.identity).unwrap();
    assert!(env.admission.committed_radio(env.identity).is_err());
}

#[test]
fn prepared_transport_retries_without_duplication_and_cannot_complete_before_actual_radio() {
    let mut env = environment();
    prepare(&mut env);
    prepare(&mut env);
    assert!(matches!(
        env.admission.complete_packets(
            env.identity,
            &mut env.core,
            &mut env.packets,
            &mut env.receive,
            ControlTime(7)
        ),
        Err(InitialPacketError::Phase)
    ));
    assert!(
        env.core
            .gateway()
            .authorize_rab(env.request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_ok()
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    let key = RouteKey {
        connection: umts_types::ConnectionKey {
            ue: env.key,
            domain: CnDomain::Ps,
        },
        rab: INITIAL_RAB,
    };
    assert!(env.packets.requested(key).is_none());
    assert!(env.packets.sequence_state(key).is_err());
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, at(0));
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    assert_eq!(env.resources.outstanding(), 1);
    let mut foreign_packets = rabs();
    assert!(matches!(
        retired.finish_packet_failure(
            &mut env.core,
            &mut foreign_packets,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap()
        ),
        Err(InitialPacketError::Packet {
            error: packet_rabs::Error::Identity,
            ..
        })
    ));
    let retained = retired.packet_assignment.as_ref().unwrap().progress();
    assert_eq!(
        (retained.prepared, retained.retired, retained.recorded),
        (1, 0, 0)
    );
    let progress = retired
        .finish_packet_failure(
            &mut env.core,
            &mut env.packets,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!((progress.retired, progress.recorded), (1, 1));
    let again = retired
        .finish_packet_failure(
            &mut env.core,
            &mut env.packets,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!(again, progress);
    assert!(
        env.core
            .gateway()
            .authorize_rab(env.request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_err()
    );
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected RAB failure")
    };
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert!(response.setup.is_empty());
    assert_eq!(response.failed.len(), 1);
    assert_eq!(response.failed[0].id.get(), INITIAL_RAB);
    env.resources
        .release(retired.resources.take().unwrap())
        .unwrap();
}

#[test]
fn real_radio_completion_records_once_and_uses_canonical_outbox_retry() {
    let mut env = environment();
    prepare(&mut env);
    start(&mut env);
    assert!(matches!(
        env.admission.complete_packets(
            env.identity,
            &mut env.core,
            &mut env.packets,
            &mut env.receive,
            ControlTime(7)
        ),
        Err(InitialPacketError::Phase)
    ));
    complete_radio(&mut env);
    assert!(env.admission.frame_backpressure() > 0);
    assert!(matches!(
        env.admission.complete_packets(
            env.identity,
            &mut env.core,
            &mut env.packets,
            &mut env.receive,
            ControlTime(5)
        ),
        Err(InitialPacketError::Core {
            error: umts_core::Error::Time,
            ..
        })
    ));
    let bound = env.admission.packet_progress(env.identity).unwrap();
    assert_eq!((bound.items, bound.bound, bound.recorded), (1, 1, 0));
    assert_eq!(env.core.gateway().queued_frames(), 0);
    let progress = env
        .admission
        .complete_packets(
            env.identity,
            &mut env.core,
            &mut env.packets,
            &mut env.receive,
            ControlTime(7),
        )
        .unwrap();
    assert_eq!((progress.items, progress.recorded), (1, 1));
    assert_eq!(env.core.gateway().queued_frames(), 0);
    env.peer.set_blocked(true);
    let first = env
        .core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    assert!(first.rab_published);
    assert_eq!(env.core.gateway().queued_frames(), 1);
    assert!(
        env.core
            .gateway()
            .authorize_rab(env.request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_err()
    );
    assert_eq!(
        env.admission
            .complete_packets(
                env.identity,
                &mut env.core,
                &mut env.packets,
                &mut env.receive,
                ControlTime(8)
            )
            .unwrap(),
        progress
    );
    assert!(
        !env.core
            .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert_eq!(env.core.gateway().queued_frames(), 1);
    env.peer.set_blocked(false);
    env.core
        .drive(ControlTime(10), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(env.core.gateway().queued_frames(), 0);
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected RAB success")
    };
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.setup.len(), 1);
    assert!(response.failed.is_empty());
    let teid = local_teid(&response.setup[0]);
    let key = RouteKey {
        connection: umts_types::ConnectionKey {
            ue: env.key,
            domain: CnDomain::Ps,
        },
        rab: INITIAL_RAB,
    };
    let state = env.packets.sequence_state(key).unwrap();
    assert_eq!(
        (state.next_uplink, state.next_downlink),
        (u16::MAX, Some(42))
    );
    env.packets
        .io_mut()
        .incoming
        .push_back((downlink(teid, 42, b"actual packet"), peer()));
    assert_eq!(
        env.packets
            .step(&mut env.receive)
            .unwrap()
            .transport
            .delivered,
        1
    );
}

#[test]
fn cancellation_under_frame_backpressure_returns_original_owners_and_lease() {
    let mut env = environment();
    prepare(&mut env);
    start(&mut env);
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    assert!(
        env.writer.accepted(
            env.transmit
                .generate_slot(&mut env.source, &mut samples)
                .unwrap()
        )
    );
    let now = env.source.next_chip();
    for _ in 0..128 {
        step(&mut env, now);
        if env.admission.frame_backpressure() > 0 {
            break;
        }
    }
    assert!(env.admission.frame_backpressure() > 0);
    assert!(env.admission.failure(env.identity).is_none());
    assert!(
        env.admission
            .procedure(env.identity)
            .unwrap()
            .receipt()
            .is_none()
    );
    assert_eq!(env.resources.outstanding(), 1);
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, now);
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    assert!(retired.rab_assignment.is_some());
    assert_eq!(env.resources.outstanding(), 1);
    let progress = retired
        .finish_packet_failure(
            &mut env.core,
            &mut env.packets,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!((progress.retired, progress.recorded), (1, 1));
    env.resources
        .release(retired.resources.take().unwrap())
        .unwrap();
    assert_eq!(env.resources.outstanding(), 0);
}

#[test]
fn revoked_authority_prevents_preparation_and_stale_failure_does_not_replace_outcome() {
    let mut env = environment();
    env.peer.rua(rua::Message::Disconnect {
        domain: Domain::Ps,
        context: env.context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    env.core
        .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    assert!(matches!(
        env.admission
            .prepare_packets(env.identity, env.core.gateway(), &mut env.packets),
        Err(InitialPacketError::Core { .. })
    ));
    until(&mut env, AdmissionPhase::Retired, at(0));
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    let progress = retired
        .finish_packet_failure(
            &mut env.core,
            &mut env.packets,
            ControlTime(8),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!(
        (progress.retired, progress.recorded, progress.superseded),
        (1, 0, 1)
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    env.resources
        .release(retired.resources.take().unwrap())
        .unwrap();
}

#[test]
fn partial_two_rab_preparation_retries_then_retires_into_one_complete_failure_response() {
    let mut env = environment_with(2, 1);
    for _ in 0..3 {
        assert!(matches!(
            env.admission
                .prepare_packets(env.identity, env.core.gateway(), &mut env.packets),
            Err(InitialPacketError::Packet {
                rab: SECOND_RAB,
                error: packet_rabs::Error::Capacity
            })
        ));
        let partial = env.admission.packet_progress(env.identity).unwrap();
        assert_eq!(
            (
                partial.items,
                partial.prepared,
                partial.bound,
                partial.recorded
            ),
            (2, 1, 0, 0)
        );
        assert_eq!(
            env.admission.phase(env.identity),
            Some(AdmissionPhase::Preparing)
        );
        assert!(
            env.admission
                .procedure(env.identity)
                .unwrap()
                .receipt()
                .is_none()
        );
        assert_eq!(env.core.gateway().queued_frames(), 0);
    }
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, at(0));
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    let progress = retired
        .finish_packet_failure(
            &mut env.core,
            &mut env.packets,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!(
        (progress.items, progress.retired, progress.recorded),
        (2, 2, 2)
    );
    assert_eq!(env.resources.outstanding(), 1);
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected complete RAB response")
    };
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert!(response.setup.is_empty());
    assert_eq!(
        response
            .failed
            .iter()
            .map(|item| item.id.get())
            .collect::<Vec<_>>(),
        [INITIAL_RAB, SECOND_RAB]
    );
    assert_eq!(
        retired
            .finish_packet_failure(
                &mut env.core,
                &mut env.packets,
                ControlTime(9),
                umts_iuh::ranap::rab::Cause::from_code(32).unwrap()
            )
            .unwrap(),
        progress
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    env.resources
        .release(retired.resources.take().unwrap())
        .unwrap();
    assert_eq!(env.resources.outstanding(), 0);
}

#[test]
fn released_first_rab_does_not_block_failure_for_the_still_current_second_rab() {
    let mut env = environment_with(2, 2);
    let prepared = env
        .admission
        .prepare_packets(env.identity, env.core.gateway(), &mut env.packets)
        .unwrap();
    assert_eq!((prepared.items, prepared.prepared), (2, 2));
    env.peer.rua(rua::Message::Direct {
        domain: Domain::Ps,
        context: env.context,
        ranap: Assignment {
            setup: vec![],
            release: vec![umts_iuh::ranap::rab::Release::new(
                INITIAL_RAB,
                umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
            )],
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    env.core
        .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    assert!(
        env.core
            .gateway()
            .authorize_rab(env.request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_err()
    );
    env.core
        .gateway()
        .authorize_rab(env.request, SECOND_RAB, RabOperation::SetupOrModify)
        .unwrap();
    env.admission.cancel(env.identity).unwrap();
    assert_eq!(env.resources.outstanding(), 1);
    until(&mut env, AdmissionPhase::Retired, at(0));
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    let progress = retired
        .finish_packet_failure(
            &mut env.core,
            &mut env.packets,
            ControlTime(8),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!(
        (
            progress.items,
            progress.retired,
            progress.recorded,
            progress.superseded
        ),
        (2, 2, 1, 1)
    );
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    env.core
        .drive(ControlTime(10), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected remaining RAB failure")
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert!(response.setup.is_empty());
    assert!(response.released.is_empty());
    assert!(response.release_failed.is_empty());
    assert_eq!(
        response
            .failed
            .iter()
            .map(|item| item.id.get())
            .collect::<Vec<_>>(),
        [SECOND_RAB]
    );
    assert_eq!(
        retired
            .finish_packet_failure(
                &mut env.core,
                &mut env.packets,
                ControlTime(10),
                umts_iuh::ranap::rab::Cause::from_code(32).unwrap()
            )
            .unwrap(),
        progress
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    env.resources
        .release(retired.resources.take().unwrap())
        .unwrap();
    assert_eq!(env.resources.outstanding(), 0);
}
