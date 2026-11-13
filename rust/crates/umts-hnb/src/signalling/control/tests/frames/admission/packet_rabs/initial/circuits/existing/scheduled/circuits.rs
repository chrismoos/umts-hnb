// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_admission::{
    AdditionCommitServices, AdditionalMediaServices, AdditionalRabHost, CircuitPlayoutTime,
    RetainedCircuitServices,
};
use umts_rrc_schema::information_elements as ie;

const COUNT_C_MINIMUM_AHEAD_FRAMES: u64 = 200;
const COUNT_C_ALIGNMENT_FRAMES: u64 = 8;

fn expanded_packet_environment() -> InitialEnvironment {
    let mut requested = item(INITIAL_RAB);
    requested.first.parameters.as_mut().unwrap().maximum =
        qos::MaximumRates::new(&[INITIAL_RATE]).unwrap();
    environment_for_capabilities(Domain::Ps, vec![requested], 2, true)
}

#[test]
fn managed_circuit_after_packet_data_initializes_once_and_retains_its_real_rtp_endpoint() {
    let mut env = expanded_packet_environment();
    prepare(&mut env);
    start(&mut env);
    let serviced = complete_radio(&mut env);
    assert_eq!(
        env.admission
            .complete_packets(
                env.identity,
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
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(env.identity).is_ok() {
            break;
        }
        step(&mut env, serviced);
    }
    let previous = env.identity;
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    let packet_route = RouteKey {
        connection: umts_types::ConnectionKey {
            ue: env.key,
            domain: CnDomain::Ps,
        },
        rab: INITIAL_RAB,
    };
    let sequences = env.packets.sequence_state(packet_route).unwrap();
    let mut voices = voices(1);
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let requested = request_for(FIRST_CIRCUIT, peer.local_address().unwrap());
    env.peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context: env.context,
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
    let pending = handoff.take().unwrap().unwrap();
    let request = pending.request();
    let activation =
        ScheduledActivation::new(at(0), 0, NonZeroU8::new(ACTIVATION_FRAME as u8).unwrap())
            .unwrap();
    let radio_settings = owner_settings(&env, activation, serviced);
    let configured = policy(&env);
    let (integrity, fresh) = control::install_integrity(&mut env.radio, env.key);
    let mut target = None;
    let successor = AdditionalRabHost {
        gateway: env.core.gateway(),
        policy: &configured,
        radio: &mut env.radio,
        resources: &mut env.resources,
        receive: &mut env.receive,
        admission: &mut env.admission,
    }
    .begin_with_settings(previous, pending, activation, |prepared| {
        target = Some(prepared.change.plan().clone());
        Ok(radio_settings)
    })
    .unwrap();
    let target = target.unwrap();
    let mut air = AirPeer {
        previous,
        activation: activation.time(),
        old,
        successor,
        target: target.clone(),
        pending: Vec::new(),
        delivered: 0,
        statuses: 0,
    };
    let mut now = serviced;
    for _ in 0..COMMAND_VISITS {
        advance(&mut env, previous, now, &mut air);
        if env.admission.addition_phase(previous) == Some(AdditionPhase::AwaitingComplete) {
            break;
        }
    }
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
    for _ in 0..(ACTIVATION_FRAME + 12) * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
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
    for _ in 0..ACTIVATION_FRAME * (CHIPS_PER_FRAME / CHIPS_PER_SLOT) {
        if now.index > activation.time().index {
            break;
        }
        now = generate(&mut env, previous, successor, &mut air);
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
            generation: 2,
            discontinuity: true,
            samples: &[num_complex::Complex32::default(); 256],
        })
        .unwrap();
    let mut complete = control::user_setup::response(transaction, 3);
    let ULDCCHMessageType::radioBearerSetupComplete(body) = &mut complete else {
        unreachable!()
    };
    let received = now.index + 100;
    // TS 25.331 8.2.2.3 requires a multiple of eight at least 200 frames after first transmission.
    let count_c_frame = (received / CHIPS_PER_FRAME + COUNT_C_MINIMUM_AHEAD_FRAMES)
        .div_ceil(COUNT_C_ALIGNMENT_FRAMES)
        * COUNT_C_ALIGNMENT_FRAMES;
    let count_c_cfn = u8::try_from(count_c_frame % (u64::from(u8::MAX) + 1)).unwrap();
    assert_eq!(u64::from(count_c_cfn) % COUNT_C_ALIGNMENT_FRAMES, 0);
    assert!(count_c_frame - received / CHIPS_PER_FRAME >= COUNT_C_MINIMUM_AHEAD_FRAMES);
    body.count_c_activation_time = Some(ie::ActivationTime(count_c_cfn));
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
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].domain, CnDomain::Ps);
    let clock = CircuitPlayoutTime {
        control: Instant::now(),
        radio: now,
        settings: crate::voice_bearers::PlayoutSettings {
            lead_chips: NonZeroU64::new(2 * CHIPS_PER_FRAME).unwrap(),
            horizon_ticks: NonZeroU32::new(16_000).unwrap(),
        },
    };
    let attempts = Cell::new(0);
    assert!(
        !env.admission
            .service_additional_media(
                successor,
                AdditionalMediaServices {
                    core: &mut env.core,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut voices,
                    now: ControlTime(10),
                    circuit_time: clock,
                    circuit_budget: count(1)
                },
                |rab, _| {
                    assert_eq!(rab.rab, FIRST_CIRCUIT);
                    attempts.set(attempts.get() + 1);
                    Ok(settings())
                }
            )
            .unwrap()
    );
    assert_eq!(attempts.get(), 1);
    let reserved = voices.reserved_bytes();
    let mut input = [0; DATAGRAM_BYTES];
    let mut initialization = None;
    for _ in 0..POLL_BUDGET {
        if let Some((length, endpoint)) = peer.receive(&mut input).unwrap() {
            initialization = Some((input[..length].to_vec(), endpoint));
            break;
        }
        std::thread::yield_now();
    }
    let (wire, endpoint) = initialization.expect("actual added-circuit INIT RTP datagram");
    let packet = rtp::decode(&wire).unwrap();
    assert_eq!(
        (packet.payload_type, packet.sequence, packet.ssrc),
        (PAYLOAD_TYPE, SEQUENCE, LOCAL_SOURCE)
    );
    assert!(
        !env.admission
            .service_additional_media(
                successor,
                AdditionalMediaServices {
                    core: &mut env.core,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut voices,
                    now: ControlTime(10),
                    circuit_time: clock,
                    circuit_budget: count(1)
                },
                |_, _| panic!("pending added circuit must retain original endpoint")
            )
            .unwrap()
    );
    assert_eq!(voices.reserved_bytes(), reserved);
    assert!(
        env.core
            .gateway()
            .authorize_rab(request, FIRST_CIRCUIT, RabOperation::SetupOrModify)
            .is_ok()
    );
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    receiver
        .request(iuup::decode(packet.payload).unwrap())
        .unwrap();
    let mut ack = Vec::new();
    receiver
        .publish(&mut [0; DATAGRAM_BYTES], |bytes| {
            ack.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    let formats = receiver.negotiated().unwrap().formats();
    let format = *formats.rows().max_by_key(|row| row.payload_bits()).unwrap();
    assert!(format.payload_bits() > 0);
    let rfci = format.rfci().get();
    let expected_bits = usize::from(format.lengths()[0]);
    let subflow_storage: Vec<_> = format
        .lengths()
        .iter()
        .map(|bits| vec![0x5a; usize::from(*bits).div_ceil(BITS_PER_BYTE)])
        .collect();
    let subflows: Vec<_> = subflow_storage
        .iter()
        .zip(format.lengths())
        .map(|(bytes, bits)| Bits::new(bytes, usize::from(*bits)).unwrap())
        .collect();
    let mut payload = vec![0; format.payload_bits().div_ceil(BITS_PER_BYTE)];
    let payload_bytes = formats
        .pack(format.rfci(), &subflows, &mut payload)
        .unwrap();
    assert_eq!(payload_bytes, payload.len());
    let mut response = vec![0; rtp::HEADER_BYTES + ack.len()];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: PAYLOAD_TYPE,
            sequence: 0,
            timestamp: TIMESTAMP,
            ssrc: PEER_SOURCE,
            payload: &ack,
        },
        &mut response,
    )
    .unwrap();
    assert!(peer.send(&response, endpoint).unwrap());
    let mut complete = false;
    for _ in 0..POLL_BUDGET {
        complete = env
            .admission
            .service_additional_media(
                successor,
                AdditionalMediaServices {
                    core: &mut env.core,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut voices,
                    now: ControlTime(10),
                    circuit_time: clock,
                    circuit_budget: count(1),
                },
                |_, _| panic!("ACK processing must not reconstruct circuit media"),
            )
            .unwrap();
        if complete {
            break;
        }
        std::thread::yield_now();
    }
    assert!(complete);
    assert_eq!(env.admission.active_rabs(successor).unwrap().len(), 2);
    assert_eq!(env.packets.sequence_state(packet_route).unwrap(), sequences);
    env.core
        .drive(ControlTime(11), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected canonical added CS outcome")
    };
    assert_eq!((domain, context), (Domain::Cs, env.context));
    let outcome = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(outcome.setup.len(), 1);
    assert_eq!(outcome.setup[0].id.get(), FIRST_CIRCUIT);
    assert!(outcome.failed.is_empty());
    assert!(
        env.admission
            .service_additional_media(
                successor,
                AdditionalMediaServices {
                    core: &mut env.core,
                    receive: &mut env.receive,
                    packets: &mut env.packets,
                    voices: &mut voices,
                    now: ControlTime(11),
                    circuit_time: clock,
                    circuit_budget: count(1)
                },
                |_, _| panic!("recorded added circuit must retain original endpoint")
            )
            .unwrap()
    );
    if env.source.next_chip().index < now.index {
        now = generate(&mut env, previous, successor, &mut air);
    }
    let longest = target
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
    let mut finished = false;
    for _ in 0..(owners::admission_limits().maximum_lead_frames.get() + longest)
        * usize::try_from(CHIPS_PER_FRAME / CHIPS_PER_SLOT).unwrap()
    {
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
        "added circuit did not reach retained ownership at {now:?}"
    );
    assert_eq!(
        env.admission
            .retained_additional_assignments(successor)
            .unwrap()
            .len(),
        1
    );
    let metadata = env
        .admission
        .active_rabs(successor)
        .unwrap()
        .into_iter()
        .find(|rab| rab.domain == CnDomain::Cs)
        .unwrap();
    let bearer = BearerId::new(metadata.bearers[0]).unwrap();
    let mut data = [0; DATAGRAM_BYTES];
    let length = iuup::encode(
        iuup::Frame::Data {
            number: 0,
            quality: iuup::FrameQuality::Good,
            rfci,
            payload_crc: iuup::PayloadCheck::Valid,
            payload: &payload,
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
            timestamp: TIMESTAMP,
            ssrc: PEER_SOURCE,
            payload: &data[..length],
        },
        &mut wire,
    )
    .unwrap();
    assert!(peer.send(&wire, endpoint).unwrap());
    let lower = env.source.next_chip();
    let clock = CircuitPlayoutTime {
        control: Instant::now(),
        radio: lower,
        ..clock
    };
    let mut offered = None;
    for _ in 0..POLL_BUDGET {
        assert_eq!(
            env.admission
                .service_retained_additional_circuits(
                    successor,
                    RetainedCircuitServices {
                        gateway: env.core.gateway(),
                        voices: &mut voices,
                        receive: &mut env.receive,
                        time: clock,
                        budget: count(1)
                    }
                )
                .unwrap(),
            1
        );
        offered = (0..=owners::admission_limits().maximum_lead_frames.get() + longest + 4)
            .map(|offset| at((lower.index / CHIPS_PER_FRAME + offset as u64) * CHIPS_PER_FRAME))
            .find(|due| voices.offer(successor, *due, bearer).is_some());
        if offered.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    let due = offered.expect("retained added circuit must still accept actual peer RTP data");
    assert_eq!(
        voices
            .offer(successor, due, bearer)
            .unwrap()
            .payload_bits_per_block,
        expected_bits
    );
    assert_eq!(voices.reserved_bytes(), reserved);
    assert!(env.packets.requested(packet_route).is_some());
}
