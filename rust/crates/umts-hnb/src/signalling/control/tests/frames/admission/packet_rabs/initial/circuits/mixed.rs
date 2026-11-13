// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_user_plane::iuup::{Frame, FrameQuality, PayloadCheck};

const INITIALIZATION_TIMEOUT: Duration = Duration::from_millis(1);
const RFCI_HIGHEST: u8 = 2;
const UNABLE_TO_ESTABLISH_ALL_RFCS: u16 = 47;

#[test]
fn terminal_first_initialization_does_not_block_healthy_sibling_or_canonical_outcome_retry() {
    let failed_peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut healthy_peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut env = environment_for(
        Domain::Cs,
        vec![
            request_for(FIRST_CIRCUIT, failed_peer.local_address().unwrap()),
            request_for(SECOND_CIRCUIT, healthy_peer.local_address().unwrap()),
        ],
        2,
    );
    let mut traffic = voices(2);
    env.admission
        .prepare_circuits(env.identity, env.core.gateway(), &mut traffic, |rab, _| {
            let mut settings = settings();
            if rab.rab == FIRST_CIRCUIT {
                settings.initialization = CircuitInitialization::Initiate(
                    Settings::new(INITIALIZATION_TIMEOUT, 0, Versions::new(3).unwrap()).unwrap(),
                );
            }
            Ok(settings)
        })
        .unwrap();
    let failed_endpoint = env
        .admission
        .circuit_endpoint(env.identity, FIRST_CIRCUIT)
        .unwrap();
    start(&mut env);
    complete_radio(&mut env);
    let now = Instant::now();
    negotiate(&mut env, &mut traffic, &mut healthy_peer, now);
    let expired = now + INITIALIZATION_TIMEOUT;
    let clock = time(&env, expired);
    let progress = env
        .admission
        .service_circuits(
            env.identity,
            env.core.gateway(),
            &mut traffic,
            &mut env.receive,
            clock,
            count(2),
        )
        .unwrap();
    assert_eq!((progress.items, progress.failed, progress.ready), (2, 1, 1));
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
    let retry = env
        .admission
        .circuit_progress(env.identity, &traffic)
        .unwrap();
    assert_eq!(
        (retry.failed, retry.retired, retry.recorded, retry.ready),
        (1, 1, 0, 1)
    );
    assert!(
        env.admission
            .circuit_endpoint(env.identity, FIRST_CIRCUIT)
            .is_none()
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    let released_port = RtpEndpoint::bind(failed_endpoint).unwrap();
    assert_eq!(released_port.local_address().unwrap(), failed_endpoint);
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
        (
            completed.items,
            completed.recorded,
            completed.failed,
            completed.retired,
            completed.ready,
            completed.active
        ),
        (2, 2, 1, 1, 1, 1)
    );
    assert!(
        env.core
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("expected one mixed canonical CS outcome")
    };
    let response = AssignmentResponse::decode(&ranap, WireLimits::default())
        .unwrap()
        .0;
    assert_eq!(response.failed.len(), 1);
    assert_eq!(
        (response.failed[0].id.get(), response.failed[0].cause.code()),
        (FIRST_CIRCUIT, UNABLE_TO_ESTABLISH_ALL_RFCS)
    );
    assert_eq!(response.setup.len(), 1);
    assert_eq!(response.setup[0].id.get(), SECOND_CIRCUIT);
    let procedure = env.admission.procedure(env.identity).unwrap();
    let bearers: Vec<_> = procedure
        .rabs()
        .iter()
        .find(|rab| rab.identity == FIRST_CIRCUIT)
        .unwrap()
        .bearers
        .iter()
        .map(|id| BearerId::new(*id).unwrap())
        .collect();
    let plan = env
        .transmit
        .bank()
        .session(env.identity)
        .unwrap()
        .plan()
        .clone();
    let probe = traffic
        .prepare(
            crate::voice_bearers::VoiceKey {
                connection: umts_types::ConnectionKey {
                    ue: env.key,
                    domain: CnDomain::Cs,
                },
                rab: FIRST_CIRCUIT,
            },
            env.identity,
            plan,
            &bearers,
            Versions::new(3).unwrap(),
            crate::voice_bearers::UplinkPolicy {
                delivery: vec![
                    umts_user_plane::uplink_media::Delivery::Yes,
                    umts_user_plane::uplink_media::Delivery::NoDetect,
                    umts_user_plane::uplink_media::Delivery::NoDetect,
                ]
                .into_boxed_slice(),
                interval_chips: NonZeroU64::new(2 * CHIPS_PER_FRAME).unwrap(),
                wait_chips: NonZeroU64::new(4 * CHIPS_PER_FRAME).unwrap(),
                timestamp: TIMESTAMP,
                rtp: settings().source,
            },
        )
        .expect("failed route returns its bounded route and bearer reservations");
    traffic.release(probe).unwrap();
    assert!(
        env.core
            .gateway()
            .authorize_rab(env.request, SECOND_CIRCUIT, RabOperation::SetupOrModify)
            .is_err()
    );

    let healthy_endpoint = env
        .admission
        .circuit_endpoint(env.identity, SECOND_CIRCUIT)
        .unwrap();
    let accepted = env
        .admission
        .circuit_counters(env.identity)
        .unwrap()
        .accepted;
    let mut iuup = [0; DATAGRAM_BYTES];
    let length = iuup::encode(
        Frame::Data {
            number: 0,
            rfci: RFCI_HIGHEST,
            quality: FrameQuality::Good,
            payload_crc: PayloadCheck::Valid,
            payload: &[0x5a; 31],
        },
        &mut iuup,
    )
    .unwrap();
    let mut bytes = vec![0; rtp::HEADER_BYTES + length];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: PAYLOAD_TYPE,
            sequence: 1,
            timestamp: TIMESTAMP,
            ssrc: PEER_SOURCE,
            payload: &iuup[..length],
        },
        &mut bytes,
    )
    .unwrap();
    assert!(healthy_peer.send(&bytes, healthy_endpoint).unwrap());
    for _ in 0..POLL_BUDGET {
        let mut clock = time(&env, expired);
        clock.anchor = RtpAnchor::new(
            TIMESTAMP,
            at(4 * CHIPS_PER_FRAME),
            NonZeroU32::new(3200).unwrap(),
        )
        .unwrap();
        let progress = env
            .admission
            .service_circuits(
                env.identity,
                env.core.gateway(),
                &mut traffic,
                &mut env.receive,
                clock,
                count(2),
            )
            .unwrap();
        assert_eq!(
            (
                progress.recorded,
                progress.failed,
                progress.retired,
                progress.ready,
                progress.active
            ),
            (2, 1, 1, 1, 1)
        );
        if env
            .admission
            .circuit_counters(env.identity)
            .unwrap()
            .accepted
            == accepted + 1
        {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(
        env.admission
            .circuit_counters(env.identity)
            .unwrap()
            .accepted,
        accepted + 1
    );
    assert!(
        !env.core
            .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
}
