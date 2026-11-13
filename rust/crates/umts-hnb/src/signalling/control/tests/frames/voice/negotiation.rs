// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::voice_bearers::InitializationStatus;
use umts_user_plane::{iuup::negotiation::Receiver, uplink_media::Progress};

mod reinitialization;

const TIMEOUT: Duration = Duration::from_millis(100);
const TIMESTAMP: u32 = 9000;
const INITIAL_SEQUENCE: u16 = 20;
const VOICE_BITS: [u16; 3] = [81, 103, 60];

fn initialization(rfci: u8, lengths: &[u16]) -> Initialization {
    Initialization::new(
        &[Combination::new(Rfci::new(rfci).unwrap(), lengths, None).unwrap()],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap()
}

fn settings(repetitions: u8) -> Settings {
    Settings::new(TIMEOUT, repetitions, Versions::new(3).unwrap()).unwrap()
}

fn receiver_response(source: &mut VoiceTraffic, handle: VoiceHandle) -> Vec<u8> {
    let mut wire = Vec::new();
    assert!(
        source
            .publish_initialization_rtp(handle, TIMESTAMP, &mut [0; 256], &mut [0; 256], |bytes| {
                wire.extend_from_slice(bytes);
                Ok::<_, ()>(true)
            },)
            .unwrap()
    );
    wire
}

fn committed(f: &Fixture, now: Instant) -> (VoiceTraffic, VoiceHandle) {
    let (mut source, handle) = prepare(f);
    source.activate(handle, &f.bank).unwrap();
    let mut peer = sender(0, &VOICE_BITS);
    request(&mut source, handle, &mut peer, now);
    let wire = receiver_response(&mut source, handle);
    let rtp = rtp::decode(&wire).unwrap();
    assert_eq!(rtp.sequence, INITIAL_SEQUENCE);
    assert_eq!(
        peer.acknowledgment(iuup::decode(rtp.payload).unwrap(), now),
        Ok(AckEffect::Complete)
    );
    (source, handle)
}

#[test]
fn rejected_reinitialization_restores_committed_downlink_and_uplink_with_shared_sequence() {
    let mut f = fixture(true);
    let now = Instant::now();
    let (mut source, handle) = committed(&f, now);
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
    assert_eq!(
        rtp::decode(&original).unwrap().sequence,
        INITIAL_SEQUENCE + 1
    );
    let mut rejected = sender(1, &[82, 103, 60]);
    request(&mut source, handle, &mut rejected, now);
    let mut blocked = Vec::new();
    assert!(
        source
            .publish_initialization_rtp(handle, TIMESTAMP, &mut [0; 256], &mut [0; 256], |bytes| {
                blocked.extend_from_slice(bytes);
                Ok::<_, ()>(false)
            },)
            .is_err()
    );
    assert_eq!(
        rtp::decode(&blocked).unwrap().sequence,
        INITIAL_SEQUENCE + 1
    );
    assert!(
        source
            .publish_initialization_rtp(handle, TIMESTAMP, &mut [0; 256], &mut [0; 256], |bytes| {
                assert_eq!(bytes, blocked);
                Err::<bool, _>("send failed")
            },)
            .is_err()
    );
    let nack = receiver_response(&mut source, handle);
    assert_eq!(nack, blocked);
    assert_eq!(
        rejected.acknowledgment(
            iuup::decode(rtp::decode(&nack).unwrap().payload).unwrap(),
            now
        ),
        Ok(AckEffect::Retry)
    );
    assert_eq!(source.queued(handle), Some(1));
    assert_eq!(
        source
            .offer(f.bank.identity(), at(0), BearerId::new(10).unwrap())
            .unwrap()
            .payload_bits_per_block,
        81
    );
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                let actual = rtp::decode(bytes).unwrap();
                let original = rtp::decode(&original).unwrap();
                assert_eq!(actual.sequence, INITIAL_SEQUENCE + 2);
                assert_eq!(actual.payload, original.payload);
                assert_eq!(actual.timestamp, original.timestamp);
                assert_eq!(actual.ssrc, original.ssrc);
                Ok::<_, ()>(true)
            })
            .unwrap(),
        Progress::Sent
    );
}

#[test]
fn replacement_commits_only_after_final_ack_is_accepted_by_transport() {
    let f = fixture(true);
    let now = Instant::now();
    let (mut source, handle) = committed(&f, now);
    source.downlink(handle, &packet(0), anchor()).unwrap();
    let mut next = sender(1, &VOICE_BITS);
    request(&mut source, handle, &mut next, now);
    assert_eq!(source.queued(handle), None);
    assert!(source.downlink(handle, &packet(1), anchor()).is_err());
    let mut blocked = Vec::new();
    assert!(
        source
            .publish_initialization_rtp(handle, TIMESTAMP, &mut [0; 256], &mut [0; 256], |bytes| {
                blocked.extend_from_slice(bytes);
                Ok::<_, ()>(false)
            },)
            .is_err()
    );
    assert_eq!(source.queued(handle), None);
    assert_eq!(
        rtp::decode(&blocked).unwrap().sequence,
        INITIAL_SEQUENCE + 1
    );
    let accepted = receiver_response(&mut source, handle);
    assert_eq!(accepted, blocked);
    assert_eq!(
        next.acknowledgment(
            iuup::decode(rtp::decode(&accepted).unwrap().payload).unwrap(),
            now
        ),
        Ok(AckEffect::Complete)
    );
    assert_eq!(source.queued(handle), Some(0));
    assert!(source.downlink(handle, &packet(0), anchor()).is_err());
    source.downlink(handle, &packet(1), anchor()).unwrap();
    assert_eq!(source.queued(handle), Some(1));
}

#[test]
fn original_sender_route_requires_received_final_ack_and_shares_rtp_sequence_with_media() {
    let mut f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    source
        .start_initialization(
            handle,
            &[initialization(0, &VOICE_BITS)],
            ModeVersion::new(1).unwrap(),
            settings(3),
        )
        .unwrap();
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    ));
    assert_eq!(source.queued(handle), None);
    let mut wire = Vec::new();
    assert!(
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
                }
            )
            .unwrap()
    );
    let sent = rtp::decode(&wire).unwrap();
    assert_eq!(sent.sequence, INITIAL_SEQUENCE);
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    ));
    assert!(source.downlink(handle, &packet(0), anchor()).is_err());
    let mut peer = Receiver::new(Versions::new(3).unwrap()).unwrap();
    peer.request(iuup::decode(sent.payload).unwrap()).unwrap();
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
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    ));
    assert_eq!(source.queued(handle), Some(0));
    source.downlink(handle, &packet(0), anchor()).unwrap();
    for (bearer, width) in [(10, 81), (11, 103), (12, 60)] {
        receive_voice_part(&mut f, &mut source, bearer, width, 0);
    }
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                assert_eq!(rtp::decode(bytes).unwrap().sequence, INITIAL_SEQUENCE + 1);
                Ok::<_, ()>(true)
            })
            .unwrap(),
        Progress::Sent
    );
    assert_eq!(
        source
            .acknowledgment(handle, iuup::decode(&ack).unwrap(), now)
            .unwrap(),
        AckEffect::Ignored
    );
}

#[test]
fn unaccepted_sender_publication_does_not_start_timeout_or_consume_rtp_sequence() {
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
    assert!(
        source
            .publish_initialization_at(handle, now, &mut [0; 256], |_| Err("send failed"))
            .is_err()
    );
    let later = now + Duration::from_secs(1);
    source.tick_initialization(handle, later).unwrap();
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    ));
    assert!(
        source
            .publish_initialization_rtp_at(
                handle,
                later,
                TIMESTAMP,
                &mut [0; 256],
                &mut [0; 256],
                |bytes| {
                    assert_eq!(bytes, blocked);
                    Ok::<_, ()>(true)
                }
            )
            .unwrap()
    );
    source.tick_initialization(handle, later + TIMEOUT).unwrap();
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Failed(_)
    ));
    assert!(
        !source
            .publish_initialization_at(
                handle,
                later + TIMEOUT,
                &mut [0; 256],
                |_| -> Result<(), ()> { panic!("terminal initialization must not publish") }
            )
            .unwrap()
    );
    source.release(handle).unwrap();
    assert!(source.initialization_status(handle).is_err());
    assert!(source.tick_initialization(handle, later + TIMEOUT).is_err());
    assert!(source.activate(handle, &f.bank).is_err());
    let replacement = source
        .prepare(
            key(f.bank.identity()),
            f.bank.identity(),
            f.plan.clone(),
            &[10, 11, 12].map(|id| BearerId::new(id).unwrap()),
            Versions::new(3).unwrap(),
            uplink_policy(),
        )
        .unwrap();
    assert_ne!(replacement, handle);
    assert!(
        source
            .start_initialization(
                handle,
                &[initialization(0, &VOICE_BITS)],
                ModeVersion::new(1).unwrap(),
                settings(0)
            )
            .is_err()
    );
    source
        .start_initialization(
            replacement,
            &[initialization(0, &VOICE_BITS)],
            ModeVersion::new(1).unwrap(),
            settings(0),
        )
        .unwrap();
}

#[test]
fn invalid_sender_geometry_cannot_replace_committed_receiver_media() {
    let f = fixture(true);
    let (mut source, handle) = committed(&f, Instant::now());
    source.downlink(handle, &packet(0), anchor()).unwrap();
    assert!(
        source
            .start_initialization(
                handle,
                &[initialization(1, &[82, 103, 60])],
                ModeVersion::new(1).unwrap(),
                settings(3)
            )
            .is_err()
    );
    assert_eq!(source.queued(handle), Some(1));
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    ));
}

#[test]
fn invalid_initial_sender_candidate_preserves_original_receiver() {
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    assert!(matches!(
        source.start_initialization(
            handle,
            &[initialization(0, &[82, 103, 60])],
            ModeVersion::new(1).unwrap(),
            settings(3)
        ),
        Err(Error::Mapping)
    ));
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    ));
    let now = Instant::now();
    let mut peer = sender(0, &VOICE_BITS);
    request(&mut source, handle, &mut peer, now);
    let wire = receiver_response(&mut source, handle);
    let packet = rtp::decode(&wire).unwrap();
    assert_eq!(packet.sequence, INITIAL_SEQUENCE);
    assert_eq!(
        peer.acknowledgment(iuup::decode(packet.payload).unwrap(), now),
        Ok(AckEffect::Complete)
    );
    assert!(matches!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    ));
}

#[test]
fn chained_sender_initialization_requires_every_ack_on_original_route() {
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let frames: Vec<_> = [true, false]
        .into_iter()
        .enumerate()
        .map(|(index, chained)| {
            Initialization::new(
                &[Combination::new(Rfci::new(index as u8).unwrap(), &VOICE_BITS, None).unwrap()],
                Versions::new(3).unwrap(),
                DataPduType::Crc,
                chained,
            )
            .unwrap()
        })
        .collect();
    source
        .start_initialization(handle, &frames, ModeVersion::new(1).unwrap(), settings(3))
        .unwrap();
    let now = Instant::now();
    let mut peer = Receiver::new(Versions::new(3).unwrap()).unwrap();
    for index in 0..frames.len() {
        let mut wire = Vec::new();
        assert!(
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
                    }
                )
                .unwrap()
        );
        let packet = rtp::decode(&wire).unwrap();
        assert_eq!(packet.sequence, INITIAL_SEQUENCE + index as u16);
        let frame = iuup::decode(packet.payload).unwrap();
        assert!(matches!(frame, Frame::Control { number, .. } if usize::from(number) == index));
        peer.request(frame).unwrap();
        assert!(matches!(
            source.initialization_status(handle).unwrap(),
            InitializationStatus::Pending
        ));
        assert_eq!(source.queued(handle), None);
        assert!(
            !source
                .publish_initialization_at(handle, now, &mut [0; 256], |_| -> Result<(), ()> {
                    panic!("next chained frame must wait for acknowledgment")
                })
                .unwrap()
        );
        let mut ack = Vec::new();
        peer.publish(&mut [0; 64], |bytes| {
            ack.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
        let effect = source
            .acknowledgment(handle, iuup::decode(&ack).unwrap(), now)
            .unwrap();
        if index + 1 == frames.len() {
            assert_eq!(effect, AckEffect::Complete);
            assert!(matches!(
                source.initialization_status(handle).unwrap(),
                InitializationStatus::Ready
            ));
        } else {
            assert_eq!(effect, AckEffect::Next);
            assert!(matches!(
                source.initialization_status(handle).unwrap(),
                InitializationStatus::Pending
            ));
        }
    }
    source.downlink(handle, &packet(0), anchor()).unwrap();
    assert_eq!(source.queued(handle), Some(1));
}
