// SPDX-License-Identifier: AGPL-3.0-or-later

use super::duplex::{Environment, environment};
use super::*;
use crate::{
    dedicated_rx::ReceiveIdentity,
    packet_bearers::PacketBearers,
    packet_rabs::PacketRabs,
    radio_resources::RadioResources,
    rel99_admission::{AdmissionPhase, InitialPacketError},
    rel99_planner::tests::admission::{
        rabs::{configured, offer},
        settings as owner_settings,
    },
    signalling::tests::{send, send_capability_report},
    voice_bearers::{self, VoiceTraffic},
};
use std::{io, net::SocketAddr, num::NonZeroU16};
use umts_iuh::session::ControlTime;
use umts_rrc::{dedicated::SetupPhase, establishment::TransactionId, nas::SignallingBearer};
use umts_user_plane::packet::{BoundDatagrams, Datagrams, Limits as PacketLimits, PacketPlane};

struct Memory;

impl Datagrams for Memory {
    fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        panic!("initial media preparation must not service network packets")
    }

    fn send(&mut self, _: &[u8], _: SocketAddr) -> io::Result<bool> {
        panic!("initial media preparation must not publish network packets")
    }
}

impl BoundDatagrams for Memory {
    fn local_address(&self) -> io::Result<SocketAddr> {
        Ok("127.0.0.1:2152".parse().unwrap())
    }
}

fn owners() -> (PacketRabs<Memory>, VoiceTraffic) {
    let plane = PacketPlane::new(
        Memory,
        PacketLimits {
            routes: count(1),
            packets_per_route: count(2),
            maximum_payload_bytes: count(2048),
            maximum_storage_bytes: 128 * 1024,
            receive_budget: count(1),
            route_visits: count(1),
        },
    )
    .unwrap();
    let packets = PacketRabs::new(PacketBearers::new(plane), None, NonZeroU16::MIN).unwrap();
    let voices = VoiceTraffic::new(voice_bearers::Limits {
        routes: count(2),
        frames_per_route: count(2),
        maximum_payload_bytes: count(64),
        maximum_storage_bytes: 64 * 1024,
    })
    .unwrap();
    (packets, voices)
}

fn begin_packet(
    env: &mut Environment,
    resources: &mut RadioResources,
    variant: u8,
    now: u64,
) -> ReceiveIdentity {
    let source = env._preparation.source();
    let controller = &mut env.controller;
    let key = controller
        .radio
        .request(&controller.port, at(0), &request(variant))
        .unwrap();
    let mut completion_peer = peer(source);
    send(
        &mut controller.radio,
        key,
        SignallingBearer::ControlAm,
        &mut completion_peer,
        &COMPLETE,
    );
    assert_eq!(controller.radio.process_one(), Some(Ok(key)));
    let mut nas_peer = peer(source);
    send(
        &mut controller.radio,
        key,
        SignallingBearer::NasAm,
        &mut nas_peer,
        &INITIAL,
    );
    assert_eq!(
        controller.radio.process_one(),
        Some(Err(SignallingError::RegistrationPending))
    );
    let registration = controller
        .core
        .drive(ControlTime(now), &mut controller.radio, |_, _| {
            Err::<(), _>("unexpected core event")
        })
        .unwrap();
    assert!(registration.uplink_published);
    let umts_iuh::hnbap::Message::UeRegister { identity, .. } = env._peer.last_hnbap() else {
        panic!("missing actual UE registration")
    };
    let context = ContextId::new(100 + key.slot).unwrap();
    env._peer.hnbap(umts_iuh::hnbap::Message::UeAccepted {
        identity,
        context,
        membership: None,
    });
    controller
        .core
        .drive(ControlTime(now + 1), &mut controller.radio, |_, _| {
            Err::<(), _>("unexpected core event")
        })
        .unwrap();
    assert_eq!(controller.radio.process_one(), Some(Ok(key)));
    controller
        .core
        .drive(ControlTime(now + 2), &mut controller.radio, |_, _| {
            Err::<(), _>("unexpected core event")
        })
        .unwrap();
    controller
        .radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    let mut ue = peer(source);
    ue.enqueue(&COMPLETE).unwrap();
    ue.publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    send_capability_report(&mut controller.radio, key, &mut ue);
    assert_eq!(controller.radio.process_one(), Some(Ok(key)));
    let pending = offer(
        &mut controller.core,
        &mut controller.radio,
        &env._peer,
        context,
        now + 3,
    );
    let request = pending.request();
    let policy = configured(&controller.radio, key);
    let identity = controller
        .begin_rab_dch(pending, &policy, resources, owner_settings(key.epoch))
        .unwrap_or_else(|rejected| panic!("{:?}", rejected.error));
    assert_eq!(
        controller
            .admission()
            .unwrap()
            .rab_assignment(identity)
            .unwrap()
            .request(),
        request
    );
    assert_prepublication(env, identity);
    identity
}

fn assert_prepublication(env: &Environment, identity: ReceiveIdentity) {
    let admission = env.controller.admission().unwrap();
    assert_eq!(admission.phase(identity), Some(AdmissionPhase::Preparing));
    assert_eq!(
        admission.procedure(identity).unwrap().phase(),
        SetupPhase::Prepared
    );
    assert!(env.controller.radio.dedicated_route(identity.ue).is_none());
}

fn service(
    env: &mut Environment,
    packets: &mut PacketRabs<Memory>,
    voices: &mut VoiceTraffic,
    visits: usize,
) -> MediaServiceStep {
    env.controller
        .service_initial_media(
            packets,
            voices,
            ControlTime(20),
            count(visits),
            |_| panic!("packet assignments must not request circuit playout time"),
            |_, _| panic!("packet assignments must not request circuit settings"),
        )
        .unwrap()
}

#[test]
fn empty_media_service_never_calls_circuit_or_packet_transport() {
    let mut env = environment();
    let (mut packets, mut voices) = owners();
    for visits in [1, 2, 16] {
        let report = service(&mut env, &mut packets, &mut voices, visits);
        assert_eq!(report.visited, 0);
        assert_eq!(report.failures, 0);
        assert!(report.first_failure.is_none());
    }
}

#[test]
fn packet_preparation_is_bounded_and_peer_capacity_failure_does_not_publish_radio() {
    let mut env = environment();
    let source = env._preparation.source();
    let mut resources = RadioResources::new(
        env.controller.epoch(),
        source.information(),
        count(2),
        17..=24,
    )
    .unwrap();
    let first = begin_packet(&mut env, &mut resources, 0, 3);
    let second = begin_packet(&mut env, &mut resources, 1, 7);
    let (mut packets, mut voices) = owners();

    let report = service(&mut env, &mut packets, &mut voices, 1);
    assert_eq!(report.visited, 1);
    assert_eq!(report.failures, 0);
    assert!(report.first_failure.is_none());
    let admission = env.controller.admission().unwrap();
    let first_progress = admission.packet_progress(first).unwrap();
    let second_progress = admission.packet_progress(second).unwrap();
    assert_eq!((first_progress.items, first_progress.prepared), (1, 1));
    assert_eq!((second_progress.items, second_progress.prepared), (1, 0));
    assert_eq!((first_progress.bound, first_progress.recorded), (0, 0));

    let report = service(&mut env, &mut packets, &mut voices, 2);
    assert_eq!(report.visited, 2);
    assert_eq!(report.failures, 1);
    let failure = report.first_failure.unwrap();
    assert_eq!(failure.identity, second);
    assert!(matches!(
        failure.error,
        MediaServiceError::Packet(InitialPacketError::Packet {
            error: crate::packet_rabs::Error::Capacity,
            ..
        })
    ));

    let report = service(&mut env, &mut packets, &mut voices, 16);
    assert_eq!(report.visited, 2);
    assert_eq!(report.failures, 1);
    assert_eq!(report.first_failure.unwrap().identity, second);
    let admission = env.controller.admission().unwrap();
    assert_eq!(admission.packet_progress(first), Some(first_progress));
    assert_eq!(admission.packet_progress(second), Some(second_progress));
    assert_prepublication(&env, first);
    assert_prepublication(&env, second);
    assert_eq!(resources.outstanding(), 2);
}
