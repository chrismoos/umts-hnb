// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_user_plane::iuup::{
    Acknowledgment, Frame,
    initialization::{Combination, DataPduType, Initialization, ModeVersion, Rfci},
    negotiation::{AckEffect, Sender},
};

const PEER_RFCI: u8 = 7;

fn sender(lengths: &[u16]) -> Sender {
    let frame = Initialization::new(
        &[Combination::new(Rfci::new(PEER_RFCI).unwrap(), lengths, None).unwrap()],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap();
    Sender::new(
        &[frame],
        ModeVersion::new(1).unwrap(),
        Settings::new(Duration::from_secs(1), 3, Versions::new(3).unwrap()).unwrap(),
    )
    .unwrap()
}

fn send_initialization(
    peer: &mut RtpEndpoint,
    sender: &mut Sender,
    now: Instant,
    destination: SocketAddr,
    sequence: u16,
) {
    let mut payload = Vec::new();
    assert!(
        sender
            .publish(now, &mut [0; DATAGRAM_BYTES], |bytes| {
                payload.extend_from_slice(bytes);
                Ok::<_, ()>(())
            })
            .unwrap()
    );
    let mut bytes = vec![0; rtp::HEADER_BYTES + payload.len()];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: PAYLOAD_TYPE,
            sequence,
            timestamp: TIMESTAMP,
            ssrc: PEER_SOURCE,
            payload: &payload,
        },
        &mut bytes,
    )
    .unwrap();
    assert!(peer.send(&bytes, destination).unwrap());
}

fn service(env: &mut InitialEnvironment, traffic: &mut VoiceTraffic, now: Instant) {
    let clock = time(env, now);
    env.admission
        .service_circuits(
            env.identity,
            env.core.gateway(),
            traffic,
            &mut env.receive,
            clock,
            count(1),
        )
        .unwrap();
}

fn receive_ack(peer: &mut RtpEndpoint, expected: SocketAddr) -> Vec<u8> {
    let mut bytes = [0; DATAGRAM_BYTES];
    for _ in 0..POLL_BUDGET {
        if let Some((length, source)) = peer.receive(&mut bytes).unwrap() {
            assert_eq!(source, expected);
            return bytes[..length].to_vec();
        }
        std::thread::yield_now();
    }
    panic!("missing actual RTP initialization acknowledgment");
}

#[test]
fn cn_initiated_cs_rejects_bad_contract_then_requires_published_ack_and_actual_radio_completion() {
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![request_for(FIRST_CIRCUIT, peer.local_address().unwrap())],
        2,
    );
    let mut traffic = voices(1);
    env.admission
        .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |_, _| {
            let mut settings = settings();
            settings.initialization = CircuitInitialization::Receive(Versions::new(3).unwrap());
            Ok(settings)
        })
        .unwrap();
    let endpoint = env
        .admission
        .circuit_endpoint(env.identity, FIRST_CIRCUIT)
        .expect("actual bound CS receive endpoint");
    let now = Instant::now();
    let mut invalid = sender(&[0, 0, 0]);
    send_initialization(&mut peer, &mut invalid, now, endpoint, 0);
    for _ in 0..POLL_BUDGET {
        service(&mut env, &mut traffic, now);
        if env
            .admission
            .circuit_counters(env.identity)
            .unwrap()
            .received
            == 1
        {
            break;
        }
        std::thread::yield_now();
    }
    let counters = env.admission.circuit_counters(env.identity).unwrap();
    assert_eq!(
        (counters.received, counters.accepted, counters.rejected),
        (1, 0, 1)
    );
    assert_eq!(
        env.admission
            .circuit_progress(env.identity, &traffic)
            .unwrap()
            .ready,
        0
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    service(&mut env, &mut traffic, now);
    let nack = receive_ack(&mut peer, endpoint);
    let packet = rtp::decode(&nack).unwrap();
    assert_eq!(
        (packet.sequence, packet.payload_type, packet.ssrc),
        (SEQUENCE, PAYLOAD_TYPE, LOCAL_SOURCE)
    );
    assert!(matches!(
        iuup::decode(packet.payload).unwrap(),
        Frame::Control {
            acknowledgment: Acknowledgment::Negative,
            ..
        }
    ));
    assert_eq!(
        invalid
            .acknowledgment(iuup::decode(packet.payload).unwrap(), now)
            .unwrap(),
        AckEffect::Retry
    );

    let mut valid = sender(&[81, 103, 60]);
    send_initialization(&mut peer, &mut valid, now, endpoint, 1);
    for _ in 0..POLL_BUDGET {
        service(&mut env, &mut traffic, now);
        if env
            .admission
            .circuit_counters(env.identity)
            .unwrap()
            .accepted
            == 1
        {
            break;
        }
        std::thread::yield_now();
    }
    let counters = env.admission.circuit_counters(env.identity).unwrap();
    assert_eq!(
        (counters.received, counters.accepted, counters.rejected),
        (2, 1, 1)
    );
    assert_eq!(
        env.admission
            .circuit_progress(env.identity, &traffic)
            .unwrap()
            .ready,
        0
    );
    assert!(peer.receive(&mut [0; DATAGRAM_BYTES]).unwrap().is_none());
    service(&mut env, &mut traffic, now);
    let ack = receive_ack(&mut peer, endpoint);
    let packet = rtp::decode(&ack).unwrap();
    assert_eq!(packet.sequence, SEQUENCE + 1);
    assert_eq!(
        valid
            .acknowledgment(iuup::decode(packet.payload).unwrap(), now)
            .unwrap(),
        AckEffect::Complete
    );
    let ready = env
        .admission
        .circuit_progress(env.identity, &traffic)
        .unwrap();
    assert_eq!((ready.ready, ready.active, ready.recorded), (1, 0, 0));
    assert!(matches!(
        env.admission.complete_circuits(
            env.identity,
            &mut env.core,
            &mut traffic,
            &mut env.receive,
            ControlTime(7)
        ),
        Err(InitialCircuitError::Phase)
    ));
    assert_eq!(env.core.gateway().queued_frames(), 0);
    start(&mut env);
    complete_radio(&mut env);
    let completed = env
        .admission
        .complete_circuits(
            env.identity,
            &mut env.core,
            &mut traffic,
            &mut env.receive,
            ControlTime(7),
        )
        .unwrap();
    assert_eq!(
        (completed.ready, completed.active, completed.recorded),
        (1, 1, 1)
    );
    assert!(
        env.core
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected canonical CN-initiated CS outcome")
    };
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.setup.len(), 1);
    assert_eq!(response.setup[0].id.get(), FIRST_CIRCUIT);
    assert!(response.failed.is_empty());
    let transport = umts_iuh::ranap::rab::Transport::new(
        response.setup[0].address.clone().unwrap(),
        response.setup[0].association.clone().unwrap(),
    );
    assert_eq!(
        transport.ip_peer(Domain::Cs).unwrap(),
        umts_iuh::ranap::rab::Peer::Cs(endpoint)
    );
}
