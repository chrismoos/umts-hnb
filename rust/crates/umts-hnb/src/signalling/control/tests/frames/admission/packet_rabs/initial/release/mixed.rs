// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_admission::{
    AdditionCommitServices, AdditionPhase, AdditionalMediaServices, AdditionalRabHost,
    CircuitPlayoutTime, ReleaseFinish,
};
use std::{num::NonZeroU32, time::Instant};
use umts_security::contexts::IntegrityContext;

#[test]
fn mixed_release_then_setup_keeps_one_request_and_publishes_only_its_complete_response() {
    managed_release(true, true);
}

fn cleanup_slots(plan: &CompiledRel99) -> usize {
    let longest = plan
        .downlink_configured()
        .transport_ids()
        .map(|id| {
            plan.downlink_configured()
                .definition(id)
                .unwrap()
                .interval
                .frames()
        })
        .max()
        .unwrap();
    (owners::admission_limits().maximum_lead_frames.get() + longest)
        * usize::try_from(CHIPS_PER_FRAME / CHIPS_PER_SLOT).unwrap()
}

pub(super) struct MixedSetup<'a> {
    pub previous: ReceiveIdentity,
    pub now: ChipInstant,
    pub request: RabRequest,
    pub integrity: &'a IntegrityContext,
    pub fresh: u32,
}

pub(super) fn finish_and_setup(
    env: &mut InitialEnvironment,
    air: &mut AirPeer,
    voices: &mut VoiceTraffic,
    setup: MixedSetup<'_>,
) {
    let MixedSetup {
        previous,
        mut now,
        request,
        integrity,
        fresh,
    } = setup;
    assert!(
        !env.core
            .drive(ControlTime(11), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert!(
        env.core
            .gateway()
            .authorize_rab(request, SECOND_RAB, RabOperation::SetupOrModify)
            .is_ok()
    );
    let mut completed = None;
    for _ in 0..=cleanup_slots(&air.target) {
        release_step(env, previous, now, air);
        match env
            .admission
            .finish_release_request(previous, &mut env.resources, now)
            .unwrap()
        {
            ReleaseFinish::Pending => now = generate(env, air.previous, previous, air),
            ReleaseFinish::Complete(owner) => {
                completed = owner;
                break;
            }
        }
    }
    if completed.is_none() {
        match env
            .admission
            .finish_release_request(previous, &mut env.resources, now)
            .unwrap()
        {
            ReleaseFinish::Complete(owner) => completed = owner,
            ReleaseFinish::Pending => (),
        }
    }
    let owner = completed.expect("actual release drain must return original mixed request");
    assert_eq!(owner.request(), request);
    assert!(owner.has_setup() && owner.outcomes_recorded());
    let released = owner
        .into_setup_after_release(env.core.gateway())
        .unwrap_or_else(|_| panic!("resolved release must preserve setup authority"));
    assert_eq!(released.pending().request(), request);
    assert_eq!(released.pending().assignment().release.len(), 1);
    assert_eq!(released.pending().assignment().setup.len(), 1);
    assert_eq!(env.admission.release_phase(previous), None);
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(previous).is_ok() {
            break;
        }
        release_step(env, previous, now, air);
    }
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../rust/test-support/fixtures/config/rel99.yaml"
    )))
    .unwrap();
    let retained = env.radio.retained_signalling(env.key).unwrap();
    let control = retained
        .iter()
        .find(|rb| rb.id == SignallingBearer::ControlAm.identity())
        .unwrap();
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
    let mut activation = None;
    let mut target = None;
    let successor = AdditionalRabHost {
        gateway: env.core.gateway(),
        policy: &policy,
        radio: &mut env.radio,
        resources: &mut env.resources,
        receive: &mut env.receive,
        admission: &mut env.admission,
    }
    .begin_after_release_with_delivery(
        previous,
        released,
        AdditionalLaunch {
            now,
            threshold: at(now.index + CHIPS_PER_FRAME),
            maximum_combinations: count(1024),
        },
        |prepared, delivery| {
            activation = Some(delivery.activation);
            target = Some(prepared.change.plan().clone());
            assert!(delivery.wire_bits > 0);
            Ok(release_settings(
                env.key.epoch,
                now,
                delivery.activation,
                delivery.publication_deadline,
            ))
        },
    )
    .unwrap();
    let activation = activation.unwrap();
    let target = target.unwrap();
    assert_eq!(
        env.admission
            .additional_assignment(previous)
            .unwrap()
            .request(),
        request
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    *air = AirPeer {
        previous,
        successor,
        activation: activation.time(),
        old,
        target: target.clone(),
        pending: Vec::new(),
        statuses: 0,
    };
    for _ in 0..COMMAND_VISITS {
        release_step(env, previous, now, air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
    }
    let activation_frame = activation.time().index / CHIPS_PER_FRAME;
    for _ in 0..activation_frame * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
        now = generate(env, previous, successor, air);
        assert_eq!(env.admission.addition_failure(previous), None);
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::AwaitingComplete)
    );
    let procedure = env.admission.addition_procedure(previous).unwrap();
    let receipt = procedure.receipt().unwrap();
    let transaction = procedure.transaction();
    for _ in 0..(activation_frame + 12) * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if env
            .radio
            .bearer_setup_transmission(env.key, receipt)
            .unwrap()
            == SduTransmission::Acknowledged
        {
            break;
        }
        now = generate(env, previous, successor, air);
    }
    assert_eq!(
        env.radio
            .bearer_setup_transmission(env.key, receipt)
            .unwrap(),
        SduTransmission::Acknowledged
    );
    assert!(now.index < activation.time().index);
    for _ in 0..activation_frame * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if now.index > activation.time().index {
            break;
        }
        now = generate(env, previous, successor, air);
    }
    assert!(now.index > activation.time().index);
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
            generation: 3,
            discontinuity: true,
            samples: &[num_complex::Complex32::default(); 256],
        })
        .unwrap();
    let received = now.index + 100;
    control::uplink_on_air(
        &mut env.radio,
        env.key,
        &mut env.sender,
        &crate::test_support::integrity::protected(
            integrity,
            fresh,
            SignallingBearer::ControlAm.identity(),
            3,
            control::user_setup::response(transaction, 3),
        ),
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
        release_step(env, previous, now, air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::ReadyToCommit) {
            break;
        }
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::ReadyToCommit)
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert_eq!(
        env.admission
            .commit_requested_addition(
                previous,
                AdditionCommitServices {
                    gateway: env.core.gateway(),
                    resources: &mut env.resources,
                    radio: &mut env.radio,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut *voices,
                }
            )
            .unwrap(),
        successor
    );
    assert_eq!(env.admission.active_rabs(successor).unwrap().len(), 0);
    let circuit_time = CircuitPlayoutTime {
        control: Instant::now(),
        radio: now,
        settings: crate::voice_bearers::PlayoutSettings {
            lead_chips: NonZeroU64::new(2 * CHIPS_PER_FRAME).unwrap(),
            horizon_ticks: NonZeroU32::new(16_000).unwrap(),
        },
    };
    assert!(
        env.admission
            .service_additional_media(
                successor,
                AdditionalMediaServices {
                    core: &mut env.core,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut *voices,
                    now: ControlTime(12),
                    circuit_time,
                    circuit_budget: count(1),
                },
                |_, _| panic!("packet-only mixed setup must not allocate a circuit endpoint")
            )
            .unwrap()
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert_eq!(env.admission.active_rabs(successor).unwrap().len(), 1);
    assert!(
        env.packets
            .requested(RouteKey {
                connection: umts_types::ConnectionKey {
                    ue: env.key,
                    domain: umts_types::CnDomain::Ps
                },
                rab: SECOND_RAB
            })
            .is_some()
    );
    assert!(
        env.core
            .drive(ControlTime(13), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected mixed canonical response");
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.released.len(), 1);
    assert_eq!(response.released[0].id.get(), INITIAL_RAB);
    assert_eq!(response.setup.len(), 1);
    assert_eq!(response.setup[0].id.get(), SECOND_RAB);
    assert!(
        response.failed.is_empty()
            && response.release_failed.is_empty()
            && response.queued.is_empty()
    );
    assert!(!env.core.gateway().has_pending_rab_request(request));
    let mut finished = false;
    for _ in 0..=cleanup_slots(&target) {
        release_step(env, successor, now, air);
        if env
            .admission
            .finish_addition(successor, &mut env.resources, &mut env.radio, now)
            .unwrap()
        {
            finished = true;
            break;
        }
        now = generate(env, previous, successor, air);
    }
    if !finished {
        finished = env
            .admission
            .finish_addition(successor, &mut env.resources, &mut env.radio, now)
            .unwrap();
    }
    assert!(finished);
    assert_eq!(
        env.admission
            .retained_additional_assignments(successor)
            .unwrap()[0]
            .request(),
        request
    );
    assert_eq!(env.resources.outstanding(), 1);
}
