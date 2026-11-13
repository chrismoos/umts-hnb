// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{
    BearerId, DedicatedBearers, DownlinkFrame, DownlinkGrant, EmptyTransparentTraffic,
    FrameBuildError, TransparentTraffic,
};
use crate::rel99_admission::{AdditionPhase, FrameOpportunity, FramePolicy};
use umts_rrc_schema::class_definitions::ULDCCHMessageType;

mod circuits;
#[path = "../../../../release/packet_alias.rs"]
mod packet_alias;
mod requested;

const ACTIVATION_FRAME: u64 = 64;
const COMMAND_VISITS: usize = 128;
const INITIAL_FEEDBACK_LIMIT_SLOTS: u64 = 90;

fn retime_settings(
    mut settings: InitialOwnerSettings,
    activation: ScheduledActivation,
    publication_deadline: ChipInstant,
) -> InitialOwnerSettings {
    let frame = activation.time().index / CHIPS_PER_FRAME;
    settings.timing.uplink.time = activation.time();
    settings.timing.uplink.cfn = activation.cfn();
    settings.timing.uplink_frame = frame;
    settings.timing.downlink = settings.timing.uplink;
    settings.timing.downlink_frame = frame;
    settings.samples.first_receive = umts_dsp::fractional_sampling::SamplePosition::new(
        umts_types::time::SampleInstant {
            epoch: activation.time().epoch,
            index: activation.time().index * settings.samples.rate.get() as u64 + 32,
        },
        0.0,
    )
    .unwrap();
    settings.publication_deadline = publication_deadline;
    settings.response_timeout = NonZeroU64::new(
        activation.time().index - settings.now.index + settings.response_timeout.get(),
    )
    .unwrap();
    settings
}

struct AirPdu {
    submission: crate::tx_evidence::TransportSubmission,
    bytes: Vec<u8>,
}

struct AirPeer {
    previous: ReceiveIdentity,
    activation: ChipInstant,
    old: Arc<CompiledRel99>,
    successor: ReceiveIdentity,
    target: Arc<CompiledRel99>,
    pending: Vec<AirPdu>,
    delivered: usize,
    statuses: usize,
}

impl AirPeer {
    fn observe(&mut self, draft: &DownlinkFrame<'_>) {
        let input = draft.prepared_input();
        let plan = if input.identity == self.previous {
            &self.old
        } else {
            assert_eq!(input.identity, self.successor);
            &self.target
        };
        let rb = plan
            .configuration()
            .bearer(SignallingBearer::ControlAm.identity())
            .unwrap();
        let configured = plan.downlink_configured();
        let row = configured
            .combinations()
            .iter()
            .find(|row| row.tfci == input.tfci)
            .unwrap();
        for mapping in rb.mappings(umts_rrc::dedicated::DchDirection::Downlink) {
            let transport =
                umts_dsp::dedicated_transport::TransportId::new(mapping.transport).unwrap();
            let index = configured
                .transport_ids()
                .position(|id| id == transport)
                .unwrap();
            let block = configured.definition(transport).unwrap().formats[row.formats[index]];
            if block.block_bits == 0 || block.blocks == 0 {
                continue;
            }
            let Some(payload) = input
                .transports
                .iter()
                .find(|payload| payload.transport == transport)
            else {
                assert!(!(input.start.index / CHIPS_PER_FRAME).is_multiple_of(
                    configured.definition(transport).unwrap().interval.frames() as u64
                ));
                continue;
            };
            let payload = &payload.bits;
            assert_eq!(payload.len(), block.block_bits * block.blocks);
            for bits in payload.chunks_exact(block.block_bits) {
                let mut packed = vec![0; bits.len().div_ceil(BITS_PER_BYTE)];
                umts_dsp::bits::pack_msb(bits, &mut packed).unwrap();
                let decoded = umts_mac::decode_dedicated(
                    if mapping.logical.is_some() {
                        umts_mac::DedicatedMapping::Multiplexed
                    } else {
                        umts_mac::DedicatedMapping::Single
                    },
                    Bits::new(&packed, bits.len()).unwrap(),
                )
                .unwrap();
                if decoded.channel.map(|channel| channel.get()) != mapping.logical {
                    continue;
                }
                assert_eq!(decoded.payload.len() % BITS_PER_BYTE, 0);
                let mut bytes = vec![0; decoded.payload.len() / BITS_PER_BYTE];
                decoded.payload.copy_into(&mut bytes, 0).unwrap();
                assert!(self.pending.len() < COMMAND_VISITS);
                self.pending.push(AirPdu {
                    submission: draft.submission(),
                    bytes,
                });
            }
        }
    }

    fn deliver(&mut self, env: &mut InitialEnvironment, now: ChipInstant) {
        let mut index = 0;
        while index < self.pending.len() {
            let receipt = &self.pending[index].submission;
            assert!(receipt.viable());
            assert_eq!(receipt.air_end().epoch, now.epoch);
            if !receipt.submitted() || receipt.air_end().index > now.index {
                index += 1;
                continue;
            }
            let pdu = self.pending.remove(index);
            env.sender.receive(now, &pdu.bytes).unwrap();
            self.delivered += 1;
        }
        for _ in 0..COMMAND_VISITS {
            let Some((kind, bytes)) = env
                .sender
                .publish_next(now, |publication| Ok::<_, ()>(publication.bytes.to_vec()))
                .unwrap()
            else {
                break;
            };
            assert!(matches!(
                kind,
                umts_rlc::acknowledged::PublicationKind::Status
                    | umts_rlc::acknowledged::PublicationKind::Data {
                        retransmission: true,
                        ..
                    }
            ));
            env.radio
                .receive_srb_payload(
                    env.key,
                    SignallingBearer::ControlAm,
                    ReceiveTime::simultaneous(now).with_provenance(ReceiveProvenance::Path(
                        if now.index < self.activation.index {
                            self.previous.configuration
                        } else {
                            self.successor.configuration
                        },
                    )),
                    Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap();
            self.statuses += usize::from(kind == umts_rlc::acknowledged::PublicationKind::Status);
        }
    }
}

struct ObservedPolicy<'a> {
    traffic: TrafficFramePolicy<EmptyTransparentTraffic>,
    peer: &'a mut AirPeer,
}

impl FramePolicy for ObservedPolicy<'_> {
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        now: ChipInstant,
        bank: &mut DedicatedBearers,
        radio: &mut CommonSignalling,
    ) -> Result<umts_dsp::tfci::Tfci, FrameBuildError> {
        self.traffic.select(opportunity, now, bank, radio)
    }
    fn fill(
        &mut self,
        draft: &mut DownlinkFrame<'_>,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        self.traffic.fill(draft, now, radio)?;
        self.peer.observe(draft);
        Ok(())
    }
}

fn connected() -> (InitialEnvironment, VoiceTraffic, RtpEndpoint, ChipInstant) {
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![request_for(FIRST_CIRCUIT, peer.local_address().unwrap())],
        2,
    );
    let mut traffic = voices(2);
    env.admission
        .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |_, _| {
            Ok(settings())
        })
        .unwrap();
    start(&mut env);
    let now = Instant::now();
    negotiate(&mut env, &mut traffic, &mut peer, now);
    let serviced = complete_radio(&mut env);
    let clock = time(&env, now);
    env.admission
        .service_circuits(
            env.identity,
            env.core.gateway(),
            &mut traffic,
            &mut env.receive,
            clock,
            count(1),
        )
        .unwrap();
    assert_eq!(
        env.admission
            .complete_circuits(
                env.identity,
                &mut env.core,
                &mut traffic,
                &mut env.receive,
                ControlTime(7)
            )
            .unwrap()
            .recorded,
        1
    );
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(env.identity).is_ok() {
            break;
        }
        step(&mut env, serviced);
    }
    assert!(env.admission.active_radio(env.identity).is_ok());
    (env, traffic, peer, serviced)
}

fn additional_request(env: &mut InitialEnvironment) -> crate::rab_handoff::PendingRabAssignment {
    let mut requested = item(INITIAL_RAB);
    requested.first.parameters.as_mut().unwrap().maximum =
        qos::MaximumRates::new(&[INITIAL_RATE]).unwrap();
    env.peer.rua(rua::Message::Connect {
        domain: Domain::Ps,
        context: env.context,
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: Assignment {
            setup: vec![requested],
            release: vec![],
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    let mut handoff = RabHandoff::new(WireLimits::default());
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    handoff.take().unwrap().unwrap()
}

fn prepare_addition(
    env: &mut InitialEnvironment,
) -> (crate::rab_handoff::PendingRabAssignment, PreparedExisting) {
    let pending = additional_request(env);
    let authority = pending.authorize_items(env.core.gateway()).unwrap();
    assert_eq!(authority.len(), 1);
    let retained = [env
        .admission
        .circuit_metadata(env.identity, FIRST_CIRCUIT)
        .unwrap()
        .clone()];
    let policy = policy(env);
    let selected = policy
        .services()
        .select(&[(CnDomain::Ps, &pending.assignment().setup[0])])
        .unwrap();
    let active = env.admission.active_radio(env.identity).unwrap();
    let planned = policy
        .plan_existing(&active, &env.radio, &retained, selected.rabs())
        .unwrap();
    let activation =
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(ACTIVATION_FRAME as u8).unwrap())
            .unwrap();
    let prepared = planned
        .allocate(active, &env.radio, &mut env.resources, activation)
        .unwrap();
    drop(authority);
    (pending, prepared)
}

fn advance(
    env: &mut InitialEnvironment,
    previous: ReceiveIdentity,
    now: ChipInstant,
    peer: &mut AirPeer,
) {
    env.rx.service(|_, result| {
        result.unwrap();
    });
    env.transmit.service().unwrap();
    let report = {
        let mut policy = ObservedPolicy {
            traffic: TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
            peer: &mut *peer,
        };
        env.admission
            .step_with_core(
                now,
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut policy,
                env.core.gateway(),
            )
            .unwrap()
    };
    assert!(report.failure.is_none(), "{report:?}");
    peer.deliver(env, now);
    assert_ne!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::RecoveryRequired),
        "additional setup failed: {:?}",
        env.admission.addition_failure(previous)
    );
    assert!(env.admission.take_unclaimed().is_none());
}

fn generate(
    env: &mut InitialEnvironment,
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
    peer: &mut AirPeer,
) -> ChipInstant {
    let first = env.source.next_chip();
    if first.index / CHIPS_PER_SLOT >= INITIAL_FEEDBACK_LIMIT_SLOTS {
        env.feedback
            .publish(TpcPlan {
                identity: if first.index < peer.activation.index {
                    previous
                } else {
                    successor
                },
                first_slot: first,
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
    assert!(
        env.writer.accepted(
            env.transmit
                .generate_slot(&mut env.source, &mut samples)
                .unwrap()
        )
    );
    let now = env.source.next_chip();
    for _ in 0..4 {
        advance(env, previous, now, peer);
    }
    now
}

#[test]
fn foreign_receive_owner_rejection_returns_original_change_and_refunds_candidate_pool() {
    let (mut env, traffic, _peer, serviced) = connected();
    let previous = env.identity;
    let (_pending, prepared) = prepare_addition(&mut env);
    let successor = prepared.change.receive_identity();
    let settings = owner_settings(&env, prepared.setup.activation(), serviced);
    let before = env.radio.resources();
    let (_foreign_runtime, mut foreign_receive) = owners::receive(env.key.epoch, 2);
    let built = env
        .admission
        .build_existing_owners(previous, &prepared, &mut env.radio, settings)
        .unwrap();
    let rejected = built
        .begin(prepared.change, &mut env.admission, &mut foreign_receive)
        .unwrap_err();
    assert_eq!(rejected.change.receive_identity(), successor);
    assert_eq!(env.radio.resources(), before);
    assert_eq!(env.admission.addition_phase(previous), None);
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    assert!(env.admission.active_radio(previous).is_ok());
    assert_eq!(
        env.admission
            .circuit_progress(previous, &traffic)
            .unwrap()
            .ready,
        1
    );
    env.resources
        .cancel_reconfiguration(rejected.change)
        .unwrap();
    assert_eq!(env.resources.outstanding(), 1);
}

#[test]
fn actual_scheduled_addition_preserves_original_voice_and_authenticates_new_packet_setup() {
    with_combined(|_| {});
}

#[test]
fn setup_rejection_before_worker_switch_is_admitted_on_the_previous_mapping() {
    let (mut env, _traffic, _peer, serviced) = connected();
    let previous = env.identity;
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let pending = additional_request(&mut env);
    let activation =
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(ACTIVATION_FRAME as u8).unwrap())
            .unwrap();
    let settings = owner_settings(&env, activation, serviced);
    let policy = policy(&env);
    let (integrity, fresh) = control::install_integrity(&mut env.radio, env.key);
    let mut target = None;
    let mut activation = activation;
    let successor = crate::rel99_admission::AdditionalRabHost {
        gateway: env.core.gateway(),
        policy: &policy,
        radio: &mut env.radio,
        resources: &mut env.resources,
        receive: &mut env.receive,
        admission: &mut env.admission,
    }
    .begin_with_delivery(
        previous,
        pending,
        crate::rel99_admission::AdditionalLaunch {
            now: serviced,
            threshold: at(serviced.index + CHIPS_PER_FRAME),
            maximum_combinations: count(1024),
        },
        |prepared, delivery| {
            activation = delivery.activation;
            target = Some(prepared.change.plan().clone());
            Ok(retime_settings(
                settings,
                activation,
                delivery.publication_deadline,
            ))
        },
    )
    .unwrap();
    let mut air = AirPeer {
        previous,
        activation: activation.time(),
        old,
        successor,
        target: target.unwrap(),
        pending: Vec::new(),
        delivered: 0,
        statuses: 0,
    };
    let mut now = serviced;
    for _ in 0..ACTIVATION_FRAME * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        advance(&mut env, previous, now, &mut air);
        let procedure = env.admission.addition_procedure(previous).unwrap();
        if env.admission.addition_phase(previous) == Some(AdditionPhase::Switching)
            && procedure.receipt().is_some_and(|receipt| {
                matches!(
                    env.radio.bearer_setup_transmission(env.key, receipt),
                    Ok(SduTransmission::InFlight {
                        delimited: true,
                        ..
                    })
                )
            })
        {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::Switching)
    );
    assert!(now.index < air.activation.index);
    let transaction = env
        .admission
        .addition_procedure(previous)
        .unwrap()
        .transaction();
    let failure = pdu::RadioBearerSetupFailure::new(
        ie::RRCTransactionIdentifier(transaction.get()),
        ie::FailureCauseWithProtErr::invalidConfiguration(()),
        None,
        None,
    );
    let received = now.index + 100;
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut env.sender,
        &crate::test_support::integrity::protected(
            &integrity,
            fresh,
            2,
            2,
            ULDCCHMessageType::radioBearerSetupFailure(failure.clone()),
        ),
        received,
        ReceiveProvenance::Path(previous.configuration),
        Some(at(received)),
    );
    assert_eq!(
        env.radio.process_one_at_with_control(
            at(received + 20),
            control::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap(),
        ),
        Some(Ok(env.key)),
    );
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::RecoveryRequired)
    );
    assert_eq!(
        env.admission
            .addition_procedure(previous)
            .unwrap()
            .failure(),
        Some(&failure)
    );
    assert!(
        !env.radio
            .registry
            .get(env.key)
            .unwrap()
            .protection
            .blocks_status()
    );
}

struct Combined<'a> {
    env: &'a mut InitialEnvironment,
    voices: &'a mut VoiceTraffic,
    identity: ReceiveIdentity,
    now: ChipInstant,
    integrity: &'a umts_security::contexts::IntegrityContext,
    fresh: u32,
}

fn with_combined(test: impl FnOnce(Combined<'_>)) {
    let (mut env, mut traffic, mut peer, serviced) = connected();
    let previous = env.identity;
    let endpoint = env
        .admission
        .circuit_endpoint(previous, FIRST_CIRCUIT)
        .unwrap();
    let bearer = BearerId::new(
        env.admission
            .circuit_metadata(previous, FIRST_CIRCUIT)
            .unwrap()
            .bearers[0],
    )
    .unwrap();
    let due = at((ACTIVATION_FRAME + 24) * CHIPS_PER_FRAME);
    let rfci = env
        .admission
        .circuit_metadata(previous, FIRST_CIRCUIT)
        .unwrap()
        .combinations
        .iter()
        .enumerate()
        .max_by_key(|(_, row)| {
            row.subflow_bits
                .iter()
                .map(|bits| usize::from(*bits))
                .sum::<usize>()
        })
        .map(|(index, _)| u8::try_from(index).unwrap())
        .unwrap();
    let mut data = [0; DATAGRAM_BYTES];
    let bytes = iuup::encode(
        iuup::Frame::Data {
            number: 0,
            quality: iuup::FrameQuality::Good,
            rfci,
            payload_crc: iuup::PayloadCheck::Valid,
            payload: &[0x5a; 31],
        },
        &mut data,
    )
    .unwrap();
    let mut wire = vec![0; rtp::HEADER_BYTES + bytes];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: PAYLOAD_TYPE,
            sequence: 1,
            timestamp: TIMESTAMP
                + u32::try_from(
                    due.index * u64::from(rtp::IU_CLOCK_HZ) / umts_types::time::CHIPS_PER_SECOND,
                )
                .unwrap(),
            ssrc: PEER_SOURCE,
            payload: &data[..bytes],
        },
        &mut wire,
    )
    .unwrap();
    assert!(peer.send(&wire, endpoint).unwrap());
    let control_now = Instant::now();
    for _ in 0..POLL_BUDGET {
        env.admission
            .service_circuits(
                previous,
                env.core.gateway(),
                &mut traffic,
                &mut env.receive,
                CircuitTime {
                    control: control_now,
                    radio: serviced,
                    anchor: RtpAnchor::new(TIMESTAMP, at(0), NonZeroU32::new(16_000).unwrap())
                        .unwrap(),
                },
                count(1),
            )
            .unwrap();
        if traffic.offer(previous, due, bearer).is_some() {
            break;
        }
        std::thread::yield_now();
    }
    let offered = traffic.offer(previous, due, bearer).unwrap_or_else(|| {
        panic!(
            "original RTP queue: {:?}",
            env.admission.circuit_counters(previous)
        )
    });
    assert_eq!(offered.payload_bits_per_block, 81);
    let reserved = traffic.reserved_bytes();
    let (pending, prepared) = prepare_addition(&mut env);
    let added_rabs = prepared.rabs.clone();
    let successor = prepared.change.receive_identity();
    let target = prepared.change.plan().clone();
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let original_pool_bytes =
        crate::dedicated_bearers::storage_requirements(&old, owners::settings(env.key.epoch).pool)
            .unwrap()
            .1;
    let mut air = AirPeer {
        previous,
        activation: prepared.setup.activation().time(),
        old,
        successor,
        target: target.clone(),
        pending: Vec::new(),
        delivered: 0,
        statuses: 0,
    };
    let activation = prepared.setup.activation();
    let settings = owner_settings(&env, activation, serviced);
    let (integrity, fresh) = control::install_integrity(&mut env.radio, env.key);
    let built = env
        .admission
        .build_existing_owners(previous, &prepared, &mut env.radio, settings)
        .unwrap();
    assert_eq!(built.identity(), successor);
    let PreparedExisting { change, .. } = prepared;
    assert_eq!(
        built
            .begin(change, &mut env.admission, &mut env.receive)
            .unwrap(),
        successor
    );
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::Preparing)
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    assert_eq!(env.resources.outstanding(), 1);
    for _ in 0..COMMAND_VISITS {
        advance(&mut env, previous, serviced, &mut air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
    }
    let mut now = serviced;
    for _ in 0..ACTIVATION_FRAME * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::AwaitingComplete)
    );
    let procedure = env.admission.addition_procedure(previous).unwrap();
    let transaction = procedure.transaction();
    let receipt = procedure.receipt().unwrap();
    assert!(Arc::ptr_eq(
        procedure.configuration(),
        target.configuration()
    ));
    for _ in 0..(ACTIVATION_FRAME + 12) * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        now = generate(&mut env, previous, successor, &mut air);
        if env
            .radio
            .bearer_setup_transmission(env.key, receipt)
            .unwrap()
            == SduTransmission::Acknowledged
        {
            break;
        }
    }
    assert_eq!(
        env.radio
            .bearer_setup_transmission(env.key, receipt)
            .unwrap(),
        SduTransmission::Acknowledged,
        "scheduled command not received before cutover: {now:?}, delivered={}, statuses={}",
        air.delivered,
        air.statuses
    );
    assert!(now.index < activation.time().index);
    while now.index <= activation.time().index {
        now = generate(&mut env, previous, successor, &mut air);
    }
    let first = umts_types::time::SampleInstant {
        epoch: env.key.epoch,
        index: activation.time().index * 2 + 100,
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
    let complete = control::user_setup::response(transaction, 3);
    assert!(matches!(
        complete,
        ULDCCHMessageType::radioBearerSetupComplete(_)
    ));
    let received = now.index + 100;
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut env.sender,
        &crate::test_support::integrity::protected(&integrity, fresh, 2, 2, complete),
        received,
        ReceiveProvenance::Path(successor.configuration),
        Some(at(received)),
    );
    now = at(received + 20);
    assert_eq!(
        env.radio.process_one_at_with_control(
            now,
            control::security_time(),
            |received, mut security| env
                .admission
                .response(&received, &mut security, &mut env.receive)
                .unwrap()
        ),
        Some(Ok(env.key))
    );
    for _ in 0..COMMAND_VISITS {
        advance(&mut env, previous, now, &mut air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::ReadyToCommit) {
            break;
        }
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::ReadyToCommit)
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    assert!(matches!(
        env.admission.commit_addition(
            previous,
            &mut env.resources,
            &mut env.radio,
            &mut env.receive
        ),
        Err(AdmissionError::Phase)
    ));
    assert!(traffic.offer(previous, due, bearer).is_some());
    assert_eq!(
        env.admission
            .commit_addition_with_media(
                previous,
                &mut env.resources,
                &mut env.radio,
                &mut env.receive,
                &mut env.packets,
                &mut traffic
            )
            .unwrap(),
        successor
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(successor));
    assert_eq!(env.admission.phase(previous), None);
    assert_eq!(env.admission.phase(successor), Some(AdmissionPhase::Active));
    env.admission
        .record_radio_fixture_addition(successor, pending.request(), &added_rabs);
    assert_eq!(
        env.admission.circuit_endpoint(successor, FIRST_CIRCUIT),
        Some(endpoint)
    );
    let circuit = env.admission.circuit_progress(successor, &traffic).unwrap();
    assert_eq!((circuit.active, circuit.ready, circuit.recorded), (1, 1, 1));
    assert_eq!(traffic.reserved_bytes(), reserved);
    assert!(traffic.offer(previous, due, bearer).is_none());
    assert_eq!(
        traffic
            .offer(successor, due, bearer)
            .unwrap()
            .payload_bits_per_block,
        81
    );
    let mapping = target
        .configuration()
        .bearer(bearer.get())
        .unwrap()
        .mappings(umts_rrc::dedicated::DchDirection::Downlink)
        .next()
        .unwrap();
    let transport = umts_dsp::dedicated_transport::TransportId::new(mapping.transport).unwrap();
    let format = target
        .downlink_configured()
        .definition(transport)
        .unwrap()
        .formats
        .iter()
        .position(|format| format.block_bits == 81 && format.blocks == 1)
        .unwrap();
    let mut delivered = Vec::new();
    traffic
        .publish(
            DownlinkGrant {
                identity: successor,
                bearer,
                transport,
                format,
                start: due,
            },
            |blocks| {
                assert_eq!(blocks.len(), 1);
                delivered.extend((0..blocks[0].len()).map(|index| blocks[0].bit(index).unwrap()));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(delivered.len(), 81);
    assert!(delivered.iter().any(|bit| *bit));
    assert!(traffic.offer(successor, due, bearer).is_none());
    assert!(
        env.receive
            .with_active_bank(successor, |bank| bank.user_bearers_ready())
            .unwrap()
    );
    assert!(pending.authorize_items(env.core.gateway()).is_ok());
    assert_eq!(env.resources.outstanding(), 1);
    assert_eq!(
        env.admission
            .committed_identity_at(successor.ue.slot as usize),
        Some(successor)
    );
    assert_eq!(
        env.admission.active_identity_at(successor.ue.slot as usize),
        None
    );
    let before_cleanup = env.radio.resources();
    let mut finished = false;
    if env.source.next_chip().index < now.index {
        now = generate(&mut env, previous, successor, &mut air);
    }
    let longest_tti_frames = target
        .downlink_configured()
        .transport_ids()
        .map(|id| {
            target
                .downlink_configured()
                .definition(id)
                .unwrap()
                .interval
                .frames()
        })
        .max()
        .unwrap();
    let cleanup_frames = owners::admission_limits().maximum_lead_frames.get() + longest_tti_frames;
    for _ in 0..u64::try_from(cleanup_frames).unwrap() * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        let lower = env.source.next_chip();
        air.deliver(&mut env, lower);
        if env
            .admission
            .finish_addition(successor, &mut env.resources, &mut env.radio, lower)
            .unwrap()
        {
            finished = true;
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert!(
        finished,
        "real peer ACK did not retire scheduled setup: {:?}, {now:?}",
        env.radio.bearer_setup_transmission(env.key, receipt)
    );
    assert!(air.delivered > 0 && air.statuses > 0);
    assert_eq!(env.admission.addition_phase(successor), None);
    assert_eq!(
        env.radio.resources().queue_bytes,
        before_cleanup.queue_bytes - u64::try_from(original_pool_bytes).unwrap()
    );
    let credits = env.radio.resources();
    assert!(matches!(
        env.admission.finish_addition(
            successor,
            &mut env.resources,
            &mut env.radio,
            env.source.next_chip()
        ),
        Err(AdmissionError::Phase)
    ));
    assert_eq!(env.radio.resources(), credits);
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(successor).is_ok() {
            break;
        }
        let lower = env.source.next_chip();
        advance(&mut env, successor, lower, &mut air);
    }
    let active = env.admission.active_radio(successor).unwrap();
    let retry = env
        .resources
        .prepare_reconfiguration(
            active.lease(),
            active.plan(),
            target.configuration(),
            count(500_000),
            TransportPlanLimits {
                maximum_coded_bits: count(500_000),
                maximum_mapping_entries: count(1_000_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap();
    env.resources.cancel_reconfiguration(retry).unwrap();
    assert_eq!(env.resources.outstanding(), 1);
    test(Combined {
        env: &mut env,
        voices: &mut traffic,
        identity: successor,
        now,
        integrity: &integrity,
        fresh,
    });
}
