// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_user_plane::iuup::{Acknowledgment, negotiation::Cause};

const INITIALIZATION_PROCEDURE: u8 = 0;
const OTHER_PROCEDURE: u8 = 1;
const NACK_CAUSE_SHIFT: u8 = 2;

fn sender_ready(f: &Fixture, now: Instant) -> (VoiceTraffic, VoiceHandle, Vec<u8>) {
    let (mut source, handle) = prepare(f);
    source.activate(handle, &f.bank).unwrap();
    source
        .start_initialization(
            handle,
            &[initialization(0, &VOICE_BITS)],
            ModeVersion::new(1).unwrap(),
            settings(3),
        )
        .unwrap();
    let mut wire = Vec::new();
    source
        .publish_initialization_rtp_at(
            handle,
            now,
            TIMESTAMP,
            &mut [0; 256],
            &mut [0; 256],
            |bytes| {
                wire.extend_from_slice(bytes);
                Ok::<_, ()>(true)
            },
        )
        .unwrap();
    let mut peer = Receiver::new(Versions::new(3).unwrap()).unwrap();
    peer.request(iuup::decode(rtp::decode(&wire).unwrap().payload).unwrap())
        .unwrap();
    let mut ack = Vec::new();
    peer.publish(&mut [0; 64], |bytes| {
        ack.extend_from_slice(bytes);
        Ok::<_, ()>(())
    })
    .unwrap();
    assert_eq!(
        source
            .acknowledgment(handle, iuup::decode(&ack).unwrap(), now)
            .unwrap(),
        AckEffect::Complete
    );
    (source, handle, ack)
}

#[test]
fn ready_sender_commits_cn_reinitialization_only_after_accepted_final_ack() {
    let f = fixture(true);
    let now = Instant::now();
    let (mut source, handle, _) = sender_ready(&f, now);
    source.downlink(handle, &packet(0), anchor()).unwrap();
    let reserved = source.reserved_bytes();
    let mut cn = sender(1, &VOICE_BITS);
    request(&mut source, handle, &mut cn, now);
    assert_eq!(source.queued(handle), None);
    assert!(
        source
            .offer(f.bank.identity(), at(0), BearerId::new(10).unwrap())
            .is_none()
    );
    let mut blocked = Vec::new();
    assert!(
        source
            .publish_initialization_rtp_at(
                handle,
                now,
                TIMESTAMP,
                &mut [0; 256],
                &mut [0; 256],
                |bytes| {
                    blocked.extend_from_slice(bytes);
                    Ok::<_, ()>(false)
                }
            )
            .is_err()
    );
    assert_eq!(
        rtp::decode(&blocked).unwrap().sequence,
        INITIAL_SEQUENCE + 1
    );
    assert_eq!(source.reserved_bytes(), reserved);
    assert!(
        source
            .publish_initialization_rtp_at(
                handle,
                now,
                TIMESTAMP,
                &mut [0; 256],
                &mut [0; 256],
                |bytes| {
                    assert_eq!(bytes, blocked);
                    Err::<bool, _>("blocked")
                }
            )
            .is_err()
    );
    let accepted = receiver_response(&mut source, handle);
    assert_eq!(accepted, blocked);
    assert_eq!(
        cn.acknowledgment(
            iuup::decode(rtp::decode(&accepted).unwrap().payload).unwrap(),
            now
        )
        .unwrap(),
        AckEffect::Complete
    );
    assert_eq!(source.queued(handle), Some(0));
    assert!(source.downlink(handle, &packet(0), anchor()).is_err());
    source.downlink(handle, &packet(1), anchor()).unwrap();
    assert!(source.reserved_bytes() < reserved);
    source.release(handle).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
    assert!(
        source
            .request(
                handle,
                iuup::decode(rtp::decode(&accepted).unwrap().payload).unwrap()
            )
            .is_err()
    );
}

#[test]
fn rejected_cn_reinitialization_restores_exact_sender_media_and_shared_rtp_sequence() {
    let mut f = fixture(true);
    let now = Instant::now();
    let (mut source, handle, old_ack) = sender_ready(&f, now);
    source.downlink(handle, &packet(0), anchor()).unwrap();
    for (bearer, width) in [(10, 81), (11, 103), (12, 60)] {
        receive_voice_part(&mut f, &mut source, bearer, width, 0);
    }
    let mut original = Vec::new();
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                original.extend_from_slice(bytes);
                Ok::<_, ()>(false)
            })
            .unwrap(),
        Progress::Backpressure
    );
    let reserved = source.reserved_bytes();
    let mut cn = sender(1, &[82, 103, 60]);
    request(&mut source, handle, &mut cn, now);
    assert!(
        source
            .publish_initialization_rtp_at(
                handle,
                now,
                TIMESTAMP,
                &mut [0; 256],
                &mut [0; 256],
                |_| Ok::<_, ()>(false)
            )
            .is_err()
    );
    let nack = receiver_response(&mut source, handle);
    assert_eq!(rtp::decode(&nack).unwrap().sequence, INITIAL_SEQUENCE + 1);
    assert_eq!(
        cn.acknowledgment(
            iuup::decode(rtp::decode(&nack).unwrap().payload).unwrap(),
            now
        )
        .unwrap(),
        AckEffect::Retry
    );
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(source.reserved_bytes(), reserved);
    assert_eq!(
        source
            .acknowledgment(handle, iuup::decode(&old_ack).unwrap(), now)
            .unwrap(),
        AckEffect::Ignored
    );
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                let actual = rtp::decode(bytes).unwrap();
                let original = rtp::decode(&original).unwrap();
                assert_eq!(actual.sequence, INITIAL_SEQUENCE + 2);
                assert_eq!(
                    (actual.payload, actual.timestamp, actual.ssrc),
                    (original.payload, original.timestamp, original.ssrc)
                );
                Ok::<_, ()>(true)
            })
            .unwrap(),
        Progress::Sent
    );
    assert!(
        source
            .start_initialization(
                handle,
                &[initialization(2, &VOICE_BITS)],
                ModeVersion::new(1).unwrap(),
                settings(3)
            )
            .is_err()
    );
    source.release(handle).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
}

fn control<'a>(frame: &Initialization, number: u8, bytes: &'a mut [u8]) -> Frame<'a> {
    let length = frame.encode(bytes).unwrap();
    Frame::Control {
        number,
        acknowledgment: Acknowledgment::Procedure,
        mode: 0,
        procedure: INITIALIZATION_PROCEDURE,
        payload_crc: PayloadCheck::Valid,
        payload: &bytes[..length],
    }
}

#[test]
fn cn_chained_reinitialization_and_duplicate_replies_keep_one_original_route() {
    let f = fixture(true);
    let now = Instant::now();
    let (mut source, handle, _) = sender_ready(&f, now);
    let first = Initialization::new(
        &[Combination::new(Rfci::new(1).unwrap(), &VOICE_BITS, None).unwrap()],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        true,
    )
    .unwrap();
    let last = initialization(2, &[39, 0, 0]);
    let mut cn = Sender::new(
        &[first.clone(), last.clone()],
        ModeVersion::new(1).unwrap(),
        settings(3),
    )
    .unwrap();
    request(&mut source, handle, &mut cn, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut cn, now),
        AckEffect::Next
    );
    assert_eq!(source.queued(handle), None);
    let mut bytes = [0; 128];
    source
        .request(handle, control(&first, 0, &mut bytes))
        .unwrap();
    let duplicate = receiver_response(&mut source, handle);
    assert!(matches!(
        iuup::decode(rtp::decode(&duplicate).unwrap().payload).unwrap(),
        Frame::Control {
            number: 0,
            acknowledgment: Acknowledgment::Positive,
            ..
        }
    ));
    assert_eq!(source.queued(handle), None);
    request(&mut source, handle, &mut cn, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut cn, now),
        AckEffect::Complete
    );
    source.downlink(handle, &packet(1), anchor()).unwrap();
    assert_eq!(source.queued(handle), Some(1));
    source
        .request(handle, control(&last, 1, &mut bytes))
        .unwrap();
    receiver_response(&mut source, handle);
    assert_eq!(source.queued(handle), Some(1));
    source.release(handle).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
}

#[test]
fn invalid_control_cannot_replace_ready_sender_or_change_original_media() {
    let f = fixture(true);
    let now = Instant::now();
    let (mut source, handle, old_ack) = sender_ready(&f, now);
    source.downlink(handle, &packet(0), anchor()).unwrap();
    let reserved = source.reserved_bytes();
    for frame in [
        Frame::Data {
            number: 0,
            rfci: 0,
            quality: FrameQuality::Good,
            payload_crc: PayloadCheck::Valid,
            payload: &[],
        },
        Frame::Control {
            number: 0,
            acknowledgment: Acknowledgment::Positive,
            mode: 0,
            procedure: INITIALIZATION_PROCEDURE,
            payload_crc: PayloadCheck::Absent,
            payload: &[],
        },
    ] {
        assert!(source.request(handle, frame).is_err());
        assert_eq!(source.queued(handle), Some(1));
        assert_eq!(source.reserved_bytes(), reserved);
        assert_eq!(
            source
                .acknowledgment(handle, iuup::decode(&old_ack).unwrap(), now)
                .unwrap(),
            AckEffect::Ignored
        );
    }
    // TS 25.415 6.5.3.1: RATE CONTROL is acknowledged without disturbing the ready sender.
    source
        .request(
            handle,
            Frame::Control {
                number: 0,
                acknowledgment: Acknowledgment::Procedure,
                mode: 0,
                procedure: OTHER_PROCEDURE,
                payload_crc: PayloadCheck::Valid,
                payload: &[],
            },
        )
        .unwrap();
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(source.reserved_bytes(), reserved);
    let ack = receiver_response(&mut source, handle);
    assert!(matches!(
        iuup::decode(rtp::decode(&ack).unwrap().payload).unwrap(),
        Frame::Control {
            acknowledgment: Acknowledgment::Positive,
            procedure: OTHER_PROCEDURE,
            ..
        }
    ));
    source
        .request(
            handle,
            Frame::Control {
                number: 0,
                acknowledgment: Acknowledgment::Procedure,
                mode: 0,
                procedure: INITIALIZATION_PROCEDURE,
                payload_crc: PayloadCheck::Failed,
                payload: &[],
            },
        )
        .unwrap();
    let nack = receiver_response(&mut source, handle);
    assert!(matches!(
        iuup::decode(rtp::decode(&nack).unwrap().payload).unwrap(),
        Frame::Control {
            acknowledgment: Acknowledgment::Negative,
            ..
        }
    ));
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(source.reserved_bytes(), reserved);
    assert_eq!(
        source
            .acknowledgment(handle, iuup::decode(&old_ack).unwrap(), now)
            .unwrap(),
        AckEffect::Ignored
    );
}

#[test]
fn pending_or_terminal_sender_does_not_invent_a_simultaneous_initialization_winner() {
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    let now = Instant::now();
    source
        .start_initialization(
            handle,
            &[initialization(0, &VOICE_BITS)],
            ModeVersion::new(1).unwrap(),
            settings(0),
        )
        .unwrap();
    let mut bytes = [0; 128];
    assert!(matches!(
        source.request(
            handle,
            control(&initialization(1, &VOICE_BITS), 0, &mut bytes)
        ),
        Err(Error::Negotiation(
            umts_user_plane::iuup::negotiation::Error::Busy
        ))
    ));
    source
        .publish_initialization_at(handle, now, &mut [0; 256], |_| Ok::<_, ()>(()))
        .unwrap();
    source.tick_initialization(handle, now + TIMEOUT).unwrap();
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Failed(_)
    ));
    assert!(
        source
            .request(
                handle,
                control(&initialization(1, &VOICE_BITS), 0, &mut bytes)
            )
            .is_err()
    );
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Failed(_)
    ));
    source.release(handle).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
}

#[test]
fn receiver_protocol_reservation_is_enforced_without_destroying_an_existing_route() {
    let f = fixture(true);
    let (mut baseline, handle) = prepare(&f);
    let one_route = baseline.reserved_bytes();
    baseline.release(handle).unwrap();
    let mut source = VoiceTraffic::new(Limits {
        routes: count(2),
        frames_per_route: count(2),
        maximum_payload_bytes: count(64),
        maximum_storage_bytes: 2 * one_route - 1,
    })
    .unwrap();
    let first = source
        .prepare(
            key(f.bank.identity()),
            f.bank.identity(),
            f.plan.clone(),
            &[10, 11, 12].map(|id| BearerId::new(id).unwrap()),
            Versions::new(3).unwrap(),
            uplink_policy(),
        )
        .unwrap();
    source.activate(first, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &VOICE_BITS);
    request(&mut source, first, &mut peer, now);
    acknowledge(&mut source, first, &mut peer, now);
    source.downlink(first, &packet(0), anchor()).unwrap();
    let mut foreign = f.bank.identity();
    foreign.ue.slot += 1;
    assert!(matches!(
        source.prepare(
            VoiceKey {
                connection: ConnectionKey {
                    ue: foreign.ue,
                    domain: CnDomain::Cs
                },
                rab: 1
            },
            foreign,
            f.plan.clone(),
            &[10, 11, 12].map(|id| BearerId::new(id).unwrap()),
            Versions::new(3).unwrap(),
            uplink_policy()
        ),
        Err(Error::Capacity)
    ));
    assert_eq!(source.queued(first), Some(1));
    assert_eq!(source.reserved_bytes(), one_route);
    source.release(first).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
}

#[test]
fn sender_to_receiver_transition_preserves_original_ranap_versions_and_full_compound_rows() {
    use crate::rel99_planner::RabPlan;
    use umts_iuh::ranap::rab::{Mode, RabId, UserPlane};
    use umts_rrc::transport::TransportInterval;
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let mut request_rab = crate::rel99_planner::tests::cs_request();
    request_rab.first.id = RabId::new(1);
    request_rab.first.user_plane = Some(UserPlane::new(Mode::PredefinedSdu, 2).unwrap());
    let parameters = request_rab.first.parameters.as_ref().unwrap();
    let metadata = RabPlan {
        domain: CnDomain::Cs,
        rab: 1,
        bearers: vec![10, 11, 12],
        maximum: parameters.effective_maximum().unwrap(),
        guaranteed: parameters.effective_guaranteed().unwrap(),
        packet_payload_budget_bits: None,
        packet_capacity: None,
        maximum_sdu_bits: parameters.maximum_sdu_bits.get(),
        circuit_interval: Some(TransportInterval::Ms20),
        circuit_versions: Some(2),
        combinations: (0..parameters.combination_count())
            .map(|index| parameters.combination(index).unwrap())
            .collect(),
    };
    source
        .start_rab_initialization(handle, &metadata, &request_rab, settings(3))
        .unwrap();
    let now = Instant::now();
    let mut initialization_wire = Vec::new();
    source
        .publish_initialization_at(handle, now, &mut [0; 256], |bytes| {
            initialization_wire.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    let mut original_peer = Receiver::new(Versions::new(3).unwrap()).unwrap();
    original_peer
        .request(iuup::decode(&initialization_wire).unwrap())
        .unwrap();
    let mut ack = Vec::new();
    original_peer
        .publish(&mut [0; 128], |bytes| {
            ack.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    source
        .acknowledgment(handle, iuup::decode(&ack).unwrap(), now)
        .unwrap();
    source.downlink(handle, &packet(2), anchor()).unwrap();
    let reserved = source.reserved_bytes();
    let mut unrequested_version = sender(7, &VOICE_BITS);
    request(&mut source, handle, &mut unrequested_version, now);
    let nack = receiver_response(&mut source, handle);
    assert!(
        matches!(iuup::decode(rtp::decode(&nack).unwrap().payload).unwrap(),Frame::Control{acknowledgment:Acknowledgment::Negative,payload,..} if payload[0]>>NACK_CAUSE_SHIFT==Cause::Version as u8)
    );
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(source.reserved_bytes(), reserved);
    let partial = Initialization::new(
        &[Combination::new(Rfci::new(7).unwrap(), &VOICE_BITS, None).unwrap()],
        Versions::new(2).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap();
    let mut partial = Sender::new(&[partial], ModeVersion::new(1).unwrap(), settings(3)).unwrap();
    request(&mut source, handle, &mut partial, now);
    let nack = receiver_response(&mut source, handle);
    assert!(
        matches!(iuup::decode(rtp::decode(&nack).unwrap().payload).unwrap(),Frame::Control{acknowledgment:Acknowledgment::Negative,payload,..} if payload[0]>>NACK_CAUSE_SHIFT==Cause::Rfci as u8)
    );
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(source.reserved_bytes(), reserved);
    let complete = Initialization::new(
        &[
            Combination::new(Rfci::new(20).unwrap(), &VOICE_BITS, None).unwrap(),
            Combination::new(Rfci::new(21).unwrap(), &[0, 0, 0], None).unwrap(),
            Combination::new(Rfci::new(22).unwrap(), &[39, 0, 0], None).unwrap(),
        ],
        Versions::new(2).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap();
    let mut complete = Sender::new(&[complete], ModeVersion::new(1).unwrap(), settings(3)).unwrap();
    request(&mut source, handle, &mut complete, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut complete, now),
        AckEffect::Complete
    );
    assert_eq!(source.queued(handle), Some(0));
    source.downlink(handle, &packet(20), anchor()).unwrap();
    assert_eq!(source.queued(handle), Some(1));
    source.release(handle).unwrap();
    assert_eq!(source.reserved_bytes(), 0);
}

#[test]
fn stale_or_foreign_handles_cannot_enter_receiver_transition_on_another_voice_route() {
    let f = fixture(true);
    let now = Instant::now();
    let (mut source, handle, _) = sender_ready(&f, now);
    let (mut foreign, foreign_handle) = prepare(&f);
    let mut bytes = [0; 128];
    let next = initialization(1, &VOICE_BITS);
    assert!(matches!(
        foreign.request(handle, control(&next, 0, &mut bytes)),
        Err(Error::Mapping)
    ));
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    ));
    source.release(handle).unwrap();
    assert!(matches!(
        source.request(handle, control(&next, 0, &mut bytes)),
        Err(Error::Mapping)
    ));
    foreign.release(foreign_handle).unwrap();
    assert_eq!((source.reserved_bytes(), foreign.reserved_bytes()), (0, 0));
}

#[test]
fn original_bound_rtp_transport_accepts_cn_reinitialization_after_sender_readiness() {
    use crate::voice_bearers::IngressSettings;
    use umts_user_plane::{iu_cs::RtpEndpoint, packet::Datagrams};
    const DATAGRAM_BYTES: usize = 512;
    const PEER_SOURCE: u32 = 7;
    const ATTEMPTS: usize = 100;
    let f = fixture(true);
    let now = Instant::now();
    let (mut source, handle, _) = sender_ready(&f, now);
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let endpoint = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut transport = source
        .attach_transport(
            handle,
            endpoint,
            peer.local_address().unwrap(),
            IngressSettings {
                datagram_bytes: count(DATAGRAM_BYTES),
                payload_type: 96,
                ssrc: Some(PEER_SOURCE),
            },
        )
        .unwrap();
    let mut cn = sender(1, &VOICE_BITS);
    let mut request_bytes = Vec::new();
    cn.publish(now, &mut [0; DATAGRAM_BYTES], |bytes| {
        request_bytes.extend_from_slice(bytes);
        Ok::<_, ()>(())
    })
    .unwrap();
    let mut bytes = vec![0; rtp::HEADER_BYTES + request_bytes.len()];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: 96,
            sequence: 0,
            timestamp: 0,
            ssrc: PEER_SOURCE,
            payload: &request_bytes,
        },
        &mut bytes,
    )
    .unwrap();
    assert!(peer.send(&bytes, transport.local_address()).unwrap());
    let mut accepted = 0;
    for _ in 0..ATTEMPTS {
        accepted += transport
            .receive(&mut source, now, anchor(), count(1))
            .unwrap()
            .accepted;
        if accepted == 1 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(accepted, 1);
    assert_eq!(source.queued(handle), None);
    assert!(
        transport
            .receive(&mut source, now, anchor(), count(1))
            .unwrap()
            .awaiting_publication
    );
    assert!(
        transport
            .publish_initialization(&mut source, now, TIMESTAMP)
            .unwrap()
    );
    let mut buffer = [0; DATAGRAM_BYTES];
    let mut ack = None;
    for _ in 0..ATTEMPTS {
        if let Some((length, from)) = peer.receive(&mut buffer).unwrap() {
            assert_eq!(from, transport.local_address());
            ack = Some(buffer[..length].to_vec());
            break;
        }
        std::thread::yield_now();
    }
    let ack = ack.expect("original route returns a real RTP initialization ACK");
    let packet = rtp::decode(&ack).unwrap();
    assert_eq!(packet.sequence, INITIAL_SEQUENCE + 1);
    assert_eq!(
        cn.acknowledgment(iuup::decode(packet.payload).unwrap(), now)
            .unwrap(),
        AckEffect::Complete
    );
    assert_eq!(transport.handle(), handle);
    assert_eq!(source.queued(handle), Some(0));
    assert!(transport.retire(&mut source).unwrap());
    assert_eq!(source.reserved_bytes(), 0);
}
