// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{
        DedicatedBearers, DownlinkFrame, EmptyTransparentTraffic, FrameBuildError,
        ReleaseOutputPolicy,
    },
    dedicated_rx::FrameAnchor,
    rel99_admission::{
        AdditionalLaunch, CoreReleaseError, FrameOpportunity, FramePolicy, ReleaseLaunchResult,
        ReleaseRabHost, ReleaseServices, ScheduledReleasePhase, ScheduledReleaseRequest,
    },
    voice_bearers::{Limits as VoiceLimits, VoiceTraffic},
};
use std::num::NonZeroU8;
use umts_rrc::dedicated::{DchDirection, ScheduledActivation};
use umts_rrc_schema::{
    class_definitions::ULDCCHMessageType, information_elements as ie, pdu_definitions as pdu,
};

const ACTIVATION_FRAME: u64 = 16;
const INITIAL_FEEDBACK_LIMIT_SLOTS: u64 = 90;
const COMMAND_VISITS: usize = 128;
const MATCHED_FILTER_HISTORY_SAMPLES: u64 = 32;

mod iu;
mod mixed;
mod recovery;

pub(crate) struct AirPeer {
    pub(crate) previous: ReceiveIdentity,
    pub(crate) successor: ReceiveIdentity,
    pub(crate) activation: ChipInstant,
    pub(crate) old: Arc<CompiledRel99>,
    pub(crate) target: Arc<CompiledRel99>,
    pub(crate) pending: Vec<(crate::tx_evidence::TransportSubmission, Vec<u8>)>,
    pub(crate) statuses: usize,
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
        for mapping in rb.mappings(DchDirection::Downlink) {
            let transport =
                umts_dsp::dedicated_transport::TransportId::new(mapping.transport).unwrap();
            let index = configured
                .transport_ids()
                .position(|id| id == transport)
                .unwrap();
            let definition = configured.definition(transport).unwrap();
            let block = definition.formats[row.formats[index]];
            if block.block_bits == 0 || block.blocks == 0 {
                continue;
            }
            let Some(payload) = input
                .transports
                .iter()
                .find(|payload| payload.transport == transport)
            else {
                assert!(
                    !(input.start.index / CHIPS_PER_FRAME)
                        .is_multiple_of(definition.interval.frames() as u64)
                );
                continue;
            };
            assert_eq!(payload.bits.len(), block.block_bits * block.blocks);
            for bits in payload.bits.chunks_exact(block.block_bits) {
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
                assert!(decoded.payload.len().is_multiple_of(BITS_PER_BYTE));
                let mut bytes = vec![0; decoded.payload.len() / BITS_PER_BYTE];
                decoded.payload.copy_into(&mut bytes, 0).unwrap();
                assert!(self.pending.len() < COMMAND_VISITS);
                self.pending.push((draft.submission(), bytes));
            }
        }
    }

    fn deliver(&mut self, env: &mut InitialEnvironment, now: ChipInstant) {
        let mut index = 0;
        while index < self.pending.len() {
            let receipt = &self.pending[index].0;
            assert!(receipt.viable());
            assert_eq!(receipt.air_end().epoch, now.epoch);
            if !receipt.submitted() || receipt.air_end().index > now.index {
                index += 1;
                continue;
            }
            let (_, bytes) = self.pending.remove(index);
            env.sender.receive(now, &bytes).unwrap();
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

fn scheduled(env: &mut InitialEnvironment, now: ChipInstant) -> Box<ScheduledReleaseRequest> {
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(env.identity).is_ok() {
            break;
        }
        step(env, now);
    }
    let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    let configured = ConfiguredPolicy::new(&config).unwrap();
    let activation =
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(ACTIVATION_FRAME as u8).unwrap())
            .unwrap();
    let active = env.admission.active_radio(env.identity).unwrap();
    let released: Vec<_> = active
        .plan()
        .configuration()
        .bearers()
        .iter()
        .filter(|rb| rb.domain.is_some())
        .map(|rb| BearerId::new(rb.id).unwrap())
        .collect();
    let planned = crate::rel99_planner::plan_release_for_ue(
        &active,
        &env.radio,
        &env.admission.active_rabs(env.identity).unwrap(),
        &released,
        &configured.policy(),
    )
    .unwrap();
    let prepared = planned
        .allocate(active, &env.radio, &mut env.resources, activation)
        .unwrap();
    let settings = release_settings(
        env.key.epoch,
        now,
        activation,
        at((ACTIVATION_FRAME - 1) * CHIPS_PER_FRAME),
    );
    env.admission
        .build_release_owners(env.identity, prepared, &mut env.radio, settings)
        .unwrap()
        .request
}

pub(crate) fn release_settings(
    epoch: umts_types::RadioEpoch,
    now: ChipInstant,
    activation: ScheduledActivation,
    deadline: ChipInstant,
) -> crate::rel99_planner::InitialOwnerSettings {
    let mut settings = owners::settings(epoch);
    settings.timing.uplink = FrameAnchor {
        time: activation.time(),
        cfn: activation.cfn(),
    };
    settings.timing.downlink = settings.timing.uplink;
    settings.timing.uplink_frame = activation.time().index / CHIPS_PER_FRAME;
    settings.timing.downlink_frame = settings.timing.uplink_frame;
    settings.samples.first_receive = umts_dsp::fractional_sampling::SamplePosition::new(
        umts_types::time::SampleInstant {
            epoch,
            index: activation.time().index * settings.samples.rate.get() as u64
                + MATCHED_FILTER_HISTORY_SAMPLES,
        },
        0.0,
    )
    .unwrap();
    settings.now = now;
    settings.publication_deadline = deadline;
    settings.response_timeout =
        NonZeroU64::new(activation.time().index - now.index + settings.response_timeout.get())
            .unwrap();
    settings
}

pub(crate) fn release_step(
    env: &mut InitialEnvironment,
    previous: ReceiveIdentity,
    now: ChipInstant,
    peer: &mut AirPeer,
) {
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
            &mut ObservedPolicy {
                traffic: TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
                peer: &mut *peer,
            },
            env.core.gateway(),
        )
        .unwrap();
    assert!(report.failure.is_none(), "{report:?}");
    peer.deliver(env, now);
    assert_eq!(env.admission.release_failure(previous), None);
    assert!(env.admission.take_unclaimed().is_none());
}

pub(crate) fn generate(
    env: &mut InitialEnvironment,
    previous: ReceiveIdentity,
    successor: ReceiveIdentity,
    peer: &mut AirPeer,
) -> ChipInstant {
    let first = env.source.next_chip();
    let slot = first.index / CHIPS_PER_SLOT;
    if slot >= INITIAL_FEEDBACK_LIMIT_SLOTS {
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
        release_step(env, previous, now, peer);
    }
    now
}

#[test]
fn new_core_release_joins_real_radio_and_packet_retirement_then_records_only_new_request() {
    managed_release(false, false);
}

#[test]
fn derived_core_release_joins_real_radio_and_packet_retirement_then_records_only_new_request() {
    managed_release(true, false);
}

fn managed_release(derived: bool, mixed: bool) {
    managed_release_with(derived, mixed, |_, _, _, _| false);
}

fn managed_release_with(
    derived: bool,
    mixed: bool,
    after_commit: impl FnOnce(
        &mut InitialEnvironment,
        ReceiveIdentity,
        ReceiveIdentity,
        ChipInstant,
    ) -> bool,
) {
    let mut env = environment();
    prepare(&mut env);
    start(&mut env);
    complete_radio(&mut env);
    let previous = env.identity;
    assert_eq!(
        env.admission
            .complete_packets(
                previous,
                &mut env.core,
                &mut env.packets,
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
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert!(
        env.core
            .gateway()
            .authorize_rab(env.request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_err()
    );
    let original_request = env.request;
    let route = RouteKey {
        connection: umts_types::ConnectionKey {
            ue: env.key,
            domain: CnDomain::Ps,
        },
        rab: INITIAL_RAB,
    };
    assert!(env.packets.requested(route).is_some());
    assert!(env.packets.sequence_state(route).is_ok());
    let setup = if mixed {
        let mut requested = crate::rel99_planner::tests::ps_request(INITIAL_RATE);
        requested.first.id = umts_iuh::ranap::rab::RabId::new(SECOND_RAB);
        vec![requested]
    } else {
        Vec::new()
    };
    env.peer.rua(rua::Message::Direct {
        domain: Domain::Ps,
        context: env.context,
        ranap: Assignment {
            setup,
            release: vec![umts_iuh::ranap::rab::Release::new(
                INITIAL_RAB,
                umts_iuh::ranap::rab::Cause::from_code(NORMAL_RELEASE).unwrap(),
            )],
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
    let pending = handoff.take().unwrap().unwrap();
    let release_request = pending.request();
    assert_ne!(release_request, original_request);
    assert!(release_request.same_connection(original_request));
    let mut now = at(env.source.next_chip().index + 120);
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(previous).is_ok() {
            break;
        }
        step(&mut env, now);
    }
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let (integrity, fresh) = control::install_integrity(&mut env.radio, env.key);
    let mut activation =
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(ACTIVATION_FRAME as u8).unwrap())
            .unwrap();
    let mut target = None;
    let successor = if derived {
        let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99.yaml"
        )))
        .unwrap();
        let configured = ConfiguredPolicy::new(&config).unwrap();
        let launched = ReleaseRabHost {
            gateway: env.core.gateway(),
            policy: &configured,
            radio: &mut env.radio,
            resources: &mut env.resources,
            receive: &mut env.receive,
            admission: &mut env.admission,
        }
        .begin_with_delivery(
            previous,
            pending,
            AdditionalLaunch {
                now,
                threshold: at(now.index + CHIPS_PER_FRAME),
                maximum_combinations: count(1024),
            },
            |prepared, delivery| {
                activation = delivery.activation;
                target = Some(prepared.plan().clone());
                assert!(delivery.wire_bits > 0);
                assert!(delivery.publication_deadline.index > now.index);
                assert!(delivery.publication_deadline.index < activation.time().index);
                Ok(release_settings(
                    env.key.epoch,
                    now,
                    activation,
                    delivery.publication_deadline,
                ))
            },
        )
        .unwrap();
        match launched {
            ReleaseLaunchResult::Scheduled(identity) => identity,
            ReleaseLaunchResult::OutcomeOnly(_) => {
                panic!("known active PS RAB requires actual radio release")
            }
        }
    } else {
        let authority = env
            .admission
            .prepare_core_release(previous, pending, env.core.gateway())
            .unwrap();
        assert!(authority.needs_radio());
        assert_eq!(authority.radio_bearers().len(), 1);
        let request = scheduled(&mut env, now);
        let successor = request.pool.identity();
        target = Some(request.pool.plan().clone());
        assert_eq!(
            env.admission
                .begin_core_release(
                    request,
                    authority,
                    env.core.gateway(),
                    &env.radio,
                    &mut env.receive
                )
                .unwrap(),
            successor
        );
        successor
    };
    let target = target.unwrap();
    let activation_frame = activation.time().index / CHIPS_PER_FRAME;
    let mut air = AirPeer {
        previous,
        successor,
        activation: activation.time(),
        old,
        target: target.clone(),
        pending: Vec::new(),
        statuses: 0,
    };
    let transaction = env
        .admission
        .release_procedure(previous)
        .unwrap()
        .transaction();
    assert_eq!(
        env.admission
            .record_release_outcomes(previous, &mut env.core, ControlTime(9), count(1))
            .unwrap(),
        0
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    for _ in 0..COMMAND_VISITS {
        release_step(&mut env, previous, now, &mut air);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::AwaitingComplete) {
            break;
        }
    }
    for _ in 0..(activation.time().index - now.index) / CHIPS_PER_SLOT {
        if env.admission.release_phase(previous) != Some(ScheduledReleasePhase::Switching) {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::AwaitingComplete)
    );
    assert!(env.admission.uplink_handoff_pending(successor));
    assert!(!env.admission.uplink_handoff_pending(previous));
    assert!(!env.admission.uplink_handoff_pending(ReceiveIdentity {
        configuration: NonZeroU64::new(successor.configuration.get() + 1).unwrap(),
        ..successor
    }));
    assert!(!env.admission.uplink_handoff_pending(ReceiveIdentity {
        ue: UeKey {
            generation: NonZeroU64::new(successor.ue.generation.get() + 1).unwrap(),
            ..successor.ue
        },
        ..successor
    }));
    let receipt = env
        .admission
        .release_procedure(previous)
        .unwrap()
        .receipt()
        .unwrap();
    for _ in 0..(activation_frame + 12) * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        now = generate(&mut env, previous, successor, &mut air);
        if now.index > activation.time().index
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
    assert!(now.index > activation.time().index);
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
    let response =
        ULDCCHMessageType::radioBearerReleaseComplete(pdu::RadioBearerReleaseComplete::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            None,
            None,
            None,
            None,
            None,
            None,
        ));
    let received = now.index + 100;
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut env.sender,
        &crate::test_support::integrity::protected(&integrity, fresh, 2, 2, response),
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
        release_step(&mut env, previous, now, &mut air);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::ReadyToCommit) {
            break;
        }
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::ReadyToCommit)
    );
    assert!(env.packets.requested(route).is_some());
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert_eq!(
        env.admission
            .record_release_outcomes(previous, &mut env.core, ControlTime(9), count(1))
            .unwrap(),
        0
    );
    let mut voices = VoiceTraffic::new(VoiceLimits {
        routes: count(1),
        frames_per_route: count(2),
        maximum_payload_bytes: count(64),
        maximum_storage_bytes: 128_000,
    })
    .unwrap();
    let cell = crate::test_support::plan(false);
    let mut foreign_resources =
        RadioResources::new(env.key.epoch, cell.information(), count(8), 17..=24).unwrap();
    assert!(matches!(
        env.admission.commit_core_release(
            previous,
            ReleaseOutputPolicy::DiscardReleased,
            ReleaseServices {
                gateway: env.core.gateway(),
                resources: &mut foreign_resources,
                radio: &mut env.radio,
                receive: &mut env.receive,
                packets: &mut env.packets,
                voices: &mut voices,
            },
        ),
        Err(crate::signalling::BearerReleaseCommitError::Commit(
            crate::dedicated_controller::DedicatedReleaseError::Release(
                crate::dedicated_bearers::MappingReleaseError::Cleanup(
                    crate::rel99_admission::MediaReleaseError::Resources(
                        crate::radio_resources::Error::Identity
                    )
                )
            )
        ))
    ));
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::ReadyToCommit)
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    assert!(env.packets.requested(route).is_some());
    assert_eq!(env.resources.outstanding(), 1);
    let committed = env
        .admission
        .commit_core_release(
            previous,
            ReleaseOutputPolicy::DiscardReleased,
            ReleaseServices {
                gateway: env.core.gateway(),
                resources: &mut env.resources,
                radio: &mut env.radio,
                receive: &mut env.receive,
                packets: &mut env.packets,
                voices: &mut voices,
            },
        )
        .unwrap();
    assert_eq!((committed.previous, committed.next), (previous, successor));
    assert!(!env.admission.uplink_handoff_pending(successor));
    assert_eq!(
        (
            committed.cleanup.packets.retained,
            committed.cleanup.packets.removed,
            committed.cleanup.retained_voices
        ),
        (0, 1, 0)
    );
    assert_eq!(env.admission.phase(previous), None);
    assert_eq!(env.admission.phase(successor), Some(AdmissionPhase::Active));
    assert_eq!(env.radio.dedicated_route(env.key), Some(successor));
    assert!(env.packets.requested(route).is_none());
    assert!(env.packets.sequence_state(route).is_err());
    let packets = env.admission.packet_progress(successor).unwrap();
    assert_eq!(
        (packets.items, packets.retired, packets.recorded),
        (1, 1, 1)
    );
    assert_eq!(env.resources.outstanding(), 1);
    if after_commit(&mut env, previous, successor, now) {
        return;
    }
    assert!(matches!(
        env.admission
            .record_release_outcomes(successor, &mut env.core, ControlTime(8), count(1)),
        Err(CoreReleaseError::Authority(umts_core::Error::Time))
    ));
    assert_eq!(
        env.admission
            .record_release_outcomes(successor, &mut env.core, ControlTime(10), count(1))
            .unwrap(),
        1
    );
    assert_eq!(
        env.admission
            .record_release_outcomes(successor, &mut env.core, ControlTime(10), count(1))
            .unwrap(),
        0
    );
    if mixed {
        mixed::finish_and_setup(
            &mut env,
            &mut air,
            &mut voices,
            mixed::MixedSetup {
                previous: successor,
                now,
                request: release_request,
                integrity: &integrity,
                fresh,
            },
        );
        return;
    }
    env.peer.set_blocked(true);
    assert!(
        env.core
            .drive(ControlTime(11), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert_eq!(env.core.gateway().queued_frames(), 1);
    assert!(
        !env.core
            .drive(ControlTime(12), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    env.peer.set_blocked(false);
    env.core
        .drive(ControlTime(13), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected canonical release response")
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.released.len(), 1);
    assert_eq!(response.released[0].id.get(), INITIAL_RAB);
    assert!(
        response.setup.is_empty()
            && response.failed.is_empty()
            && response.release_failed.is_empty()
            && response.queued.is_empty()
    );
    assert!(
        env.core
            .gateway()
            .authorize_rab(release_request, INITIAL_RAB, RabOperation::Release)
            .is_err()
    );
    assert!(env.core.gateway().rab_connection_current(original_request));
    assert!(
        !env.admission
            .finish_release(successor, &mut env.resources, at(0))
            .unwrap()
    );
    let longest_tti = target
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
    let cleanup_slots = (owners::admission_limits().maximum_lead_frames.get() + longest_tti)
        * usize::try_from(CHIPS_PER_FRAME / CHIPS_PER_SLOT).unwrap();
    for _ in 0..cleanup_slots {
        release_step(&mut env, successor, now, &mut air);
        if env
            .admission
            .finish_release(successor, &mut env.resources, now)
            .unwrap()
        {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    if env.admission.release_phase(successor).is_some() {
        assert!(matches!(
            env.admission
                .finish_release(successor, &mut foreign_resources, now),
            Err(crate::rel99_admission::AdmissionError::Resources(
                crate::radio_resources::Error::Identity
            ))
        ));
        assert!(
            !env.admission
                .release_progress(successor)
                .unwrap()
                .resources_retired
        );
        assert!(
            env.admission
                .finish_release(successor, &mut env.resources, now)
                .unwrap()
        );
    }
    assert!(air.statuses > 0);
    assert_eq!(
        env.admission.release_phase(successor),
        None,
        "drain={:?} now={now:?} statuses={} pending={} rlc={:?}",
        env.admission.release_progress(successor),
        air.statuses,
        air.pending.len(),
        env.radio.bearer_setup_transmission(env.key, receipt)
    );
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(successor).is_ok() {
            break;
        }
        release_step(&mut env, successor, now, &mut air);
    }
    let active = env.admission.active_radio(successor).unwrap();
    assert_eq!(active.lease().receive_identity(), successor);
    assert!(
        active
            .plan()
            .configuration()
            .bearers()
            .iter()
            .all(|rb| rb.domain.is_none())
    );
    assert!(env.admission.additional_request_ready(successor));
    let next = env
        .resources
        .prepare_reconfiguration(
            active.lease(),
            active.plan(),
            active.plan().configuration(),
            count(500_000),
            TransportPlanLimits {
                maximum_coded_bits: count(500_000),
                maximum_mapping_entries: count(1_000_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap();
    assert!(next.receive_identity().configuration > successor.configuration);
    env.resources.cancel_reconfiguration(next).unwrap();
    env.peer.rua(rua::Message::Direct {
        domain: Domain::Ps,
        context: env.context,
        ranap: Assignment {
            setup: Vec::new(),
            release: vec![umts_iuh::ranap::rab::Release::new(
                INITIAL_RAB,
                umts_iuh::ranap::rab::Cause::from_code(NORMAL_RELEASE).unwrap(),
            )],
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    let mut handoff = RabHandoff::new(WireLimits::default());
    env.core
        .drive(ControlTime(14), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    let pending = handoff.take().unwrap().unwrap();
    let mut unknown = if derived {
        let config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99.yaml"
        )))
        .unwrap();
        let configured = ConfiguredPolicy::new(&config).unwrap();
        match (ReleaseRabHost {
            gateway: env.core.gateway(),
            policy: &configured,
            radio: &mut env.radio,
            resources: &mut env.resources,
            receive: &mut env.receive,
            admission: &mut env.admission,
        })
        .begin_with_delivery(
            successor,
            pending,
            AdditionalLaunch {
                now,
                threshold: at(now.index + CHIPS_PER_FRAME),
                maximum_combinations: count(1024),
            },
            |_, _| panic!("unknown RAB release must not construct radio owners"),
        )
        .unwrap()
        {
            ReleaseLaunchResult::OutcomeOnly(outcome) => outcome,
            ReleaseLaunchResult::Scheduled(_) => {
                panic!("unknown RAB must not schedule a physical replacement")
            }
        }
    } else {
        env.admission
            .prepare_core_release(successor, pending, env.core.gateway())
            .unwrap()
    };
    assert!(!unknown.needs_radio());
    assert_eq!(
        unknown
            .record_ready(&mut env.core, ControlTime(15), count(1))
            .unwrap(),
        1
    );
    assert!(unknown.outcomes_recorded());
    assert_eq!(env.resources.outstanding(), 1);
}
