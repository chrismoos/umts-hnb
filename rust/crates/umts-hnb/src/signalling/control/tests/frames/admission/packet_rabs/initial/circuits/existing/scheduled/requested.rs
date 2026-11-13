// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_admission::{
    AdditionCommitServices, AdditionMediaError, AdditionalLaunch, AdditionalMediaServices,
    AdditionalRabHost, CircuitPlayoutTime,
};

#[test]
fn cs_connection_rejects_forbidden_packet_addition_before_staging_and_preserves_voice() {
    let (mut env, voices, _peer, now) = connected();
    let previous = env.identity;
    let endpoint = env
        .admission
        .circuit_endpoint(previous, FIRST_CIRCUIT)
        .unwrap();
    let reserved = voices.reserved_bytes();
    let resources = env.resources.outstanding();
    let charges = env.radio.resources();
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let pending = additional_request(&mut env);
    let request = pending.request();
    let policy = policy_for_packet_modes(&env, Some(vec![umts_config::PacketMode::HspaPlus]));
    let rejected = AdditionalRabHost {
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
        AdditionalLaunch {
            now,
            threshold: at(now.index + CHIPS_PER_FRAME),
            maximum_combinations: count(1024),
        },
        |_, _| panic!("forbidden packet mode must reject before owner construction"),
    )
    .unwrap_err();
    assert!(
        matches!(rejected.error, crate::rel99_admission::RabAdmissionError::Hsdpa(ref error) if error.contains("packet_modes"))
    );
    assert_eq!(rejected.pending.request(), request);
    assert!(rejected.change.is_none() && rejected.rollback_error.is_none());
    assert_eq!(env.resources.outstanding(), resources);
    assert_eq!(env.radio.resources(), charges);
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    assert!(Arc::ptr_eq(
        env.admission.active_radio(previous).unwrap().plan(),
        &old
    ));
    assert!(env.admission.additional_assignment(previous).is_none());
    assert_eq!(
        env.admission.circuit_endpoint(previous, FIRST_CIRCUIT),
        Some(endpoint)
    );
    assert_eq!(voices.reserved_bytes(), reserved);
    assert!(env.core.gateway().has_pending_rab_request(request));
}

#[test]
fn managed_packet_request_keeps_radio_media_and_canonical_outcome_ownership_together() {
    managed_packet_request(false);
}

#[test]
fn derived_delivery_reaches_real_peer_before_cutover_and_keeps_request_ownership() {
    managed_packet_request(true);
}

fn managed_packet_request(derived: bool) {
    let (mut env, mut voices, mut peer, serviced) = connected();
    let previous = env.identity;
    let original_request = env.request;
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let endpoint = env
        .admission
        .circuit_endpoint(previous, FIRST_CIRCUIT)
        .unwrap();
    let metadata = env
        .admission
        .circuit_metadata(previous, FIRST_CIRCUIT)
        .unwrap();
    let bearer = BearerId::new(metadata.bearers[0]).unwrap();
    let rfci = metadata
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
    const MAX_ACTIVATION_AHEAD: u64 = u8::MAX as u64;
    const RETAINED_MEDIA_LEAD_FRAMES: u64 = 128;
    let retained_media_frame = ACTIVATION_FRAME
        + if derived {
            RETAINED_MEDIA_LEAD_FRAMES
        } else {
            32
        };
    let due = at(retained_media_frame * CHIPS_PER_FRAME);
    let horizon_seconds =
        (retained_media_frame * CHIPS_PER_FRAME).div_ceil(umts_types::time::CHIPS_PER_SECOND);
    let mapping_horizon =
        NonZeroU32::new(rtp::IU_CLOCK_HZ * u32::try_from(horizon_seconds).unwrap()).unwrap();
    let mut data = [0; DATAGRAM_BYTES];
    let length = iuup::encode(
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
    let mut wire = vec![0; rtp::HEADER_BYTES + length];
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
            payload: &data[..length],
        },
        &mut wire,
    )
    .unwrap();
    assert!(peer.send(&wire, endpoint).unwrap());
    let instant = Instant::now();
    for _ in 0..POLL_BUDGET {
        env.admission
            .service_circuits(
                previous,
                env.core.gateway(),
                &mut voices,
                &mut env.receive,
                CircuitTime {
                    control: instant,
                    radio: serviced,
                    anchor: RtpAnchor::new(TIMESTAMP, at(0), mapping_horizon).unwrap(),
                },
                count(1),
            )
            .unwrap();
        if voices.offer(previous, due, bearer).is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(
        voices
            .offer(previous, due, bearer)
            .unwrap()
            .payload_bits_per_block,
        81
    );
    let reserved = voices.reserved_bytes();
    let mut pending = additional_request(&mut env);
    let request = pending.request();
    assert_ne!(request, original_request);
    let mut activation =
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(ACTIVATION_FRAME as u8).unwrap())
            .unwrap();
    let settings = owner_settings(&env, activation, serviced);
    let policy = policy(&env);
    let (integrity, fresh) = control::install_integrity(&mut env.radio, env.key);
    let mut target = None;
    let charges = env.radio.resources();
    let reservations = env.resources.outstanding();
    if derived {
        let rejected = AdditionalRabHost {
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
            AdditionalLaunch {
                now: serviced,
                threshold: at(serviced.index + (MAX_ACTIVATION_AHEAD + 1) * CHIPS_PER_FRAME),
                maximum_combinations: count(1024),
            },
            |_, _| panic!("invalid activation horizon must reject before owner construction"),
        )
        .unwrap_err();
        assert_eq!(rejected.pending.request(), request);
        assert!(rejected.change.is_none() && rejected.rollback_error.is_none());
        pending = rejected.pending;
        assert_eq!(env.radio.resources(), charges);
        assert_eq!(env.resources.outstanding(), reservations);
        assert!(
            env.core
                .gateway()
                .authorize_rab(request, INITIAL_RAB, RabOperation::SetupOrModify)
                .is_ok()
        );
    }
    let mut host = AdditionalRabHost {
        gateway: env.core.gateway(),
        policy: &policy,
        radio: &mut env.radio,
        resources: &mut env.resources,
        receive: &mut env.receive,
        admission: &mut env.admission,
    };
    let successor = if derived {
        host.begin_with_delivery(
            previous,
            pending,
            AdditionalLaunch {
                now: serviced,
                threshold: at(serviced.index + CHIPS_PER_FRAME),
                maximum_combinations: count(1024),
            },
            |prepared, delivery| {
                activation = delivery.activation;
                assert_eq!(prepared.setup.activation(), activation);
                assert!(delivery.wire_bits > 0);
                assert!(delivery.publication_deadline.index > serviced.index);
                assert!(delivery.publication_deadline.index < activation.time().index);
                target = Some(prepared.change.plan().clone());
                Ok(retime_settings(
                    settings,
                    activation,
                    delivery.publication_deadline,
                ))
            },
        )
        .unwrap()
    } else {
        host.begin_with_settings(previous, pending, activation, |prepared| {
            target = Some(prepared.change.plan().clone());
            assert_eq!(prepared.rabs.len(), 1);
            assert_eq!(prepared.rabs[0].rab, INITIAL_RAB);
            Ok(settings)
        })
        .unwrap()
    };
    assert_eq!(env.resources.outstanding(), reservations);
    assert!(env.radio.resources().queue_bytes > charges.queue_bytes);
    let activation_frame = activation.time().index / CHIPS_PER_FRAME;
    assert!(activation.time().index < due.index);
    let target = target.unwrap();
    let mut air = AirPeer {
        previous,
        activation: activation.time(),
        old: old.clone(),
        successor,
        target: target.clone(),
        pending: Vec::new(),
        delivered: 0,
        statuses: 0,
    };
    assert_eq!(
        env.admission
            .additional_assignment(previous)
            .unwrap()
            .request(),
        request
    );
    assert!(
        !env.admission
            .additional_assignment(previous)
            .unwrap()
            .outcomes_recorded()
    );
    let mut now = serviced;
    for _ in 0..COMMAND_VISITS {
        advance(&mut env, previous, now, &mut air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
    }
    for _ in 0..activation_frame * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::AwaitingComplete)
    );
    assert!(env.admission.uplink_handoff_pending(successor));
    assert!(!env.admission.uplink_handoff_pending(previous));
    let procedure = env.admission.addition_procedure(previous).unwrap();
    let transaction = procedure.transaction();
    let receipt = procedure.receipt().unwrap();
    for _ in 0..(activation_frame + 12) * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if env
            .radio
            .bearer_setup_transmission(env.key, receipt)
            .unwrap()
            == SduTransmission::Acknowledged
        {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
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
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert!(now.index > activation.time().index);
    assert!(air.delivered > 0 && air.statuses > 0);
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
        advance(&mut env, previous, now, &mut air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::ReadyToCommit) {
            break;
        }
    }
    assert_eq!(
        env.admission.addition_phase(previous),
        Some(AdditionPhase::ReadyToCommit)
    );
    assert!(matches!(
        env.admission.commit_addition_with_media(
            previous,
            &mut env.resources,
            &mut env.radio,
            &mut env.receive,
            &mut env.packets,
            &mut voices
        ),
        Err(AdditionMediaError::Admission(AdmissionError::Phase))
    ));
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
                    voices: &mut voices
                }
            )
            .unwrap(),
        successor
    );
    let active = env.admission.active_rabs(successor).unwrap();
    assert!(!env.admission.uplink_handoff_pending(successor));
    assert_eq!(active.len(), 1);
    assert_eq!(
        (active[0].domain, active[0].rab),
        (CnDomain::Cs, FIRST_CIRCUIT)
    );
    let route = RouteKey {
        connection: umts_types::ConnectionKey {
            ue: env.key,
            domain: CnDomain::Ps,
        },
        rab: INITIAL_RAB,
    };
    assert!(env.packets.requested(route).is_none());
    assert!(
        env.core
            .gateway()
            .authorize_rab(request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_ok()
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    let media_time = CircuitPlayoutTime {
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
                    voices: &mut voices,
                    now: ControlTime(10),
                    circuit_time: media_time,
                    circuit_budget: count(1)
                },
                |_, _| panic!("packet addition must not construct another circuit endpoint")
            )
            .unwrap()
    );
    assert!(
        env.admission
            .additional_assignment(successor)
            .unwrap()
            .outcomes_recorded()
    );
    assert!(
        env.admission
            .service_additional_media(
                successor,
                AdditionalMediaServices {
                    core: &mut env.core,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut voices,
                    now: ControlTime(10),
                    circuit_time: media_time,
                    circuit_budget: count(1)
                },
                |_, _| panic!("recorded packet assignment must not reconstruct media")
            )
            .unwrap()
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    let active = env.admission.active_rabs(successor).unwrap();
    assert_eq!(active.len(), 2);
    assert!(
        active
            .iter()
            .any(|rab| (rab.domain, rab.rab) == (CnDomain::Ps, INITIAL_RAB))
    );
    assert!(env.packets.requested(route).is_some());
    assert!(env.packets.sequence_state(route).is_ok());
    assert_eq!(
        env.admission.circuit_endpoint(successor, FIRST_CIRCUIT),
        Some(endpoint)
    );
    assert_eq!(voices.reserved_bytes(), reserved);
    assert!(voices.offer(previous, due, bearer).is_none());
    assert_eq!(
        voices
            .offer(successor, due, bearer)
            .unwrap()
            .payload_bits_per_block,
        81
    );
    env.core
        .drive(ControlTime(11), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected canonical additional RAB outcome")
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.setup.len(), 1);
    assert_eq!(response.setup[0].id.get(), INITIAL_RAB);
    assert!(
        response.failed.is_empty()
            && response.released.is_empty()
            && response.release_failed.is_empty()
            && response.queued.is_empty()
    );
    assert!(
        env.core
            .gateway()
            .authorize_rab(request, INITIAL_RAB, RabOperation::SetupOrModify)
            .is_err()
    );
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
    let cleanup_slots = (owners::admission_limits().maximum_lead_frames.get() + longest_tti_frames)
        * usize::try_from(CHIPS_PER_FRAME / CHIPS_PER_SLOT).unwrap();
    for _ in 0..cleanup_slots {
        if env
            .admission
            .finish_addition(
                successor,
                &mut env.resources,
                &mut env.radio,
                env.source.next_chip(),
            )
            .unwrap()
        {
            finished = true;
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
    }
    assert!(
        finished,
        "managed assignment cleanup did not finish at {now:?}"
    );
    assert_eq!(env.admission.addition_phase(successor), None);
    let retained = env
        .admission
        .retained_additional_assignments(successor)
        .unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].request(), request);
    assert!(retained[0].outcomes_recorded());
    assert_eq!(env.resources.outstanding(), 1);
    assert!(env.packets.requested(route).is_some());
    assert_eq!(
        env.admission.circuit_endpoint(successor, FIRST_CIRCUIT),
        Some(endpoint)
    );
}
