// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

mod existing;
mod mixed;
mod receive;
use crate::{
    rel99_admission::{CircuitInitialization, CircuitSettings, CircuitTime, InitialCircuitError},
    voice_bearers::{IngressSettings, VoiceTraffic},
};
use std::{
    cell::Cell,
    net::SocketAddr,
    num::NonZeroU32,
    time::{Duration, Instant},
};
use umts_user_plane::{
    iu_cs::RtpEndpoint,
    iuup::{
        self,
        initialization::Versions,
        negotiation::{Receiver, Settings},
    },
    media::RtpAnchor,
    packet::Datagrams,
    rtp,
    uplink_media::RtpSource,
};

const FIRST_CIRCUIT: u8 = 7;
const SECOND_CIRCUIT: u8 = FIRST_CIRCUIT + 1;
const PAYLOAD_TYPE: u8 = 96;
const LOCAL_SOURCE: u32 = 99;
const PEER_SOURCE: u32 = 77;
const TIMESTAMP: u32 = 9000;
const SEQUENCE: u16 = 20;
const DATAGRAM_BYTES: usize = 512;
const POLL_BUDGET: usize = 100;

fn voices(capacity: usize) -> VoiceTraffic {
    VoiceTraffic::new(crate::voice_bearers::Limits {
        routes: count(capacity),
        frames_per_route: count(2),
        maximum_payload_bytes: count(64),
        maximum_storage_bytes: 128_000,
    })
    .unwrap()
}

fn request_for(id: u8, peer: SocketAddr) -> SetupOrModify {
    let mut request = crate::rel99_planner::tests::cs_request();
    request.first.id = umts_iuh::ranap::rab::RabId::new(id);
    request.first.transport = Some(umts_iuh::ranap::rab::Transport::new(
        umts_iuh::ranap::rab::Address::from_ip(Domain::Cs, peer.ip()).unwrap(),
        umts_iuh::ranap::rab::Association::binding([
            peer.port().to_be_bytes()[0],
            peer.port().to_be_bytes()[1],
            0,
            0,
        ]),
    ));
    request
}

fn settings() -> CircuitSettings {
    CircuitSettings {
        local: ([127, 0, 0, 1], 0).into(),
        advertised: None,
        ingress: IngressSettings {
            datagram_bytes: count(DATAGRAM_BYTES),
            payload_type: PAYLOAD_TYPE,
            ssrc: Some(PEER_SOURCE),
        },
        source: RtpSource {
            sequence: SEQUENCE,
            ssrc: LOCAL_SOURCE,
            payload_type: PAYLOAD_TYPE,
        },
        timestamp: TIMESTAMP,
        wait_chips: NonZeroU64::new(4 * CHIPS_PER_FRAME).unwrap(),
        initialization: CircuitInitialization::Initiate(
            Settings::new(Duration::from_secs(1), 3, Versions::new(3).unwrap()).unwrap(),
        ),
    }
}

fn time(env: &InitialEnvironment, now: Instant) -> CircuitTime {
    CircuitTime {
        control: now,
        radio: env.source.next_chip(),
        anchor: RtpAnchor::new(TIMESTAMP, at(0), NonZeroU32::new(3200).unwrap()).unwrap(),
    }
}

fn negotiate(
    env: &mut InitialEnvironment,
    traffic: &mut VoiceTraffic,
    peer: &mut RtpEndpoint,
    now: Instant,
) {
    let mut received = [0; DATAGRAM_BYTES];
    let mut initialization = None;
    for _ in 0..POLL_BUDGET {
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
        if let Some((length, from)) = peer.receive(&mut received).unwrap() {
            initialization = Some((received[..length].to_vec(), from));
            break;
        }
        std::thread::yield_now();
    }
    let (wire, from) = initialization.expect("actual RNC initialization datagram");
    let packet = rtp::decode(&wire).unwrap();
    assert_eq!(
        (packet.payload_type, packet.ssrc, packet.sequence),
        (PAYLOAD_TYPE, LOCAL_SOURCE, SEQUENCE)
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
    let mut rtp_ack = vec![0; rtp::HEADER_BYTES + ack.len()];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: PAYLOAD_TYPE,
            sequence: 0,
            timestamp: TIMESTAMP,
            ssrc: PEER_SOURCE,
            payload: &ack,
        },
        &mut rtp_ack,
    )
    .unwrap();
    assert!(peer.send(&rtp_ack, from).unwrap());
    for _ in 0..POLL_BUDGET {
        let clock = time(env, now);
        let progress = env
            .admission
            .service_circuits(
                env.identity,
                env.core.gateway(),
                traffic,
                &mut env.receive,
                clock,
                count(1),
            )
            .unwrap();
        if progress.ready == 1 {
            return;
        }
        std::thread::yield_now();
    }
    panic!("actual peer ACK did not initialize the admitted circuit");
}

#[test]
fn original_cs_owners_require_prepared_transport_radio_complete_and_real_peer_ack() {
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![request_for(FIRST_CIRCUIT, peer.local_address().unwrap())],
        2,
    );
    let mut traffic = voices(2);
    start(&mut env);
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
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
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Starting)
    );
    assert!(
        env.admission
            .procedure(env.identity)
            .unwrap()
            .receipt()
            .is_none()
    );
    let calls = Cell::new(0);
    for _ in 0..2 {
        let progress = env
            .admission
            .prepare_circuits(
                env.identity,
                env.core.gateway(),
                &mut traffic,
                |rab, request| {
                    assert_eq!(rab.rab, FIRST_CIRCUIT);
                    assert_eq!(request.first.id.get(), FIRST_CIRCUIT);
                    calls.set(calls.get() + 1);
                    Ok(settings())
                },
            )
            .unwrap();
        assert_eq!(
            (progress.items, progress.prepared, progress.recorded),
            (1, 1, 0)
        );
    }
    assert_eq!(calls.get(), 1);
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
    let now = Instant::now();
    negotiate(&mut env, &mut traffic, &mut peer, now);
    assert_eq!(env.core.gateway().queued_frames(), 0);
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
    complete_radio(&mut env);
    let clock = time(&env, now);
    let progress = env
        .admission
        .service_circuits(
            env.identity,
            env.core.gateway(),
            &mut traffic,
            &mut env.receive,
            clock,
            count(1),
        )
        .unwrap();
    assert_eq!((progress.active, progress.ready), (1, 1));
    assert!(matches!(
        env.admission.complete_circuits(
            env.identity,
            &mut env.core,
            &mut traffic,
            &mut env.receive,
            ControlTime(5)
        ),
        Err(InitialCircuitError::Core {
            error: umts_core::Error::Time,
            ..
        })
    ));
    let pending = env
        .admission
        .circuit_progress(env.identity, &traffic)
        .unwrap();
    assert_eq!((pending.bound, pending.recorded), (1, 0));
    let done = env
        .admission
        .complete_circuits(
            env.identity,
            &mut env.core,
            &mut traffic,
            &mut env.receive,
            ControlTime(7),
        )
        .unwrap();
    assert_eq!((done.items, done.recorded), (1, 1));
    env.peer.set_blocked(true);
    assert!(
        env.core
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert_eq!(env.core.gateway().queued_frames(), 1);
    let again = env
        .admission
        .complete_circuits(
            env.identity,
            &mut env.core,
            &mut traffic,
            &mut env.receive,
            ControlTime(8),
        )
        .unwrap();
    assert_eq!(again.recorded, 1);
    env.peer.set_blocked(false);
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected canonical CS response")
    };
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.setup.len(), 1);
    assert!(response.failed.is_empty());
    assert_eq!(response.setup[0].id.get(), FIRST_CIRCUIT);
    let transport = umts_iuh::ranap::rab::Transport::new(
        response.setup[0]
            .address
            .clone()
            .expect("actual bound RTP address"),
        response.setup[0]
            .association
            .clone()
            .expect("actual bound RTP association"),
    );
    assert!(
        matches!(transport.ip_peer(Domain::Cs).unwrap(),umts_iuh::ranap::rab::Peer::Cs(address) if address.port()!=0 && address.port().is_multiple_of(2) && address.ip().is_loopback())
    );
}

#[test]
fn partial_cs_capacity_retry_and_cancel_retire_exact_owners_before_reporting_failure() {
    let peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let address = peer.local_address().unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![
            request_for(FIRST_CIRCUIT, address),
            request_for(SECOND_CIRCUIT, address),
        ],
        2,
    );
    let mut traffic = voices(1);
    for _ in 0..2 {
        assert!(
            env.admission
                .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |_, _| Ok(
                    settings()
                ))
                .is_err()
        );
        let progress = env
            .admission
            .circuit_progress(env.identity, &traffic)
            .unwrap();
        assert_eq!(
            (progress.items, progress.prepared, progress.recorded),
            (2, 1, 0)
        );
    }
    env.admission.cancel(env.identity).unwrap();
    until(&mut env, AdmissionPhase::Retired, at(0));
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    assert_eq!(env.resources.outstanding(), 1);
    let mut foreign = voices(1);
    assert!(
        retired
            .finish_circuit_failure(
                &mut env.core,
                &mut foreign,
                ControlTime(7),
                umts_iuh::ranap::rab::Cause::from_code(32).unwrap()
            )
            .is_err()
    );
    let progress = retired
        .finish_circuit_failure(
            &mut env.core,
            &mut traffic,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!((progress.retired, progress.recorded), (2, 2));
    let again = retired
        .finish_circuit_failure(
            &mut env.core,
            &mut traffic,
            ControlTime(7),
            umts_iuh::ranap::rab::Cause::from_code(32).unwrap(),
        )
        .unwrap();
    assert_eq!((again.retired, again.recorded), (2, 2));
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected complete CS failure response")
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
        vec![FIRST_CIRCUIT, SECOND_CIRCUIT]
    );
    env.resources
        .release(retired.resources.take().unwrap())
        .unwrap();
    assert_eq!(env.resources.outstanding(), 0);
}

#[test]
fn committed_core_disconnect_revokes_cs_preparation_without_fabricated_failure() {
    let peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![request_for(FIRST_CIRCUIT, peer.local_address().unwrap())],
        2,
    );
    let mut traffic = voices(1);
    env.admission
        .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |_, _| {
            Ok(settings())
        })
        .unwrap();
    env.peer.rua(rua::Message::Disconnect {
        domain: Domain::Cs,
        context: env.context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    env.core
        .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    assert!(
        env.admission
            .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |_, _| Ok(
                settings()
            ))
            .is_err()
    );
    until(&mut env, AdmissionPhase::Retired, at(0));
    let mut retired = env.admission.take_retired(env.identity).unwrap();
    let progress = retired
        .finish_circuit_failure(
            &mut env.core,
            &mut traffic,
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
    assert_eq!(env.resources.outstanding(), 0);
}
