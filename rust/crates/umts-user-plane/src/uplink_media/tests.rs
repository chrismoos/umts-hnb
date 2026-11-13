// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::iuup::{
    Acknowledgment,
    initialization::{Combination, Initialization, Rfci, Versions},
    negotiation::Receiver,
};

const INTERVAL: u64 = CHIPS_PER_SECOND / 50;
const PAYLOAD_TYPE: u8 = 96;
const SSRC: u32 = 0x1234_5678;
const WAIT: u64 = INTERVAL;
const CHECKED_IUUP_HEADER_BYTES: usize = 4;
const UNCHECKED_IUUP_HEADER_BYTES: usize = 3;

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn negotiated(rows: &[&[u16]], kind: DataPduType) -> Negotiated {
    let rows: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(index, lengths)| {
            Combination::new(Rfci::new(index as u8).unwrap(), lengths, None).unwrap()
        })
        .collect();
    let initialization =
        Initialization::new(&rows, Versions::new(3).unwrap(), kind, false).unwrap();
    let mut payload = [0; 128];
    let bytes = initialization.encode(&mut payload).unwrap();
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    receiver
        .request(Frame::Control {
            number: 0,
            acknowledgment: Acknowledgment::Procedure,
            mode: 0,
            procedure: 0,
            payload_crc: PayloadCheck::Valid,
            payload: &payload[..bytes],
        })
        .unwrap();
    assert!(receiver.negotiated().is_none());
    receiver
        .publish(&mut [0; 128], |_| Ok::<_, ()>(()))
        .unwrap();
    receiver.negotiated().unwrap().clone()
}

fn limits(frames: usize) -> Limits {
    Limits {
        frames: count(frames),
        wait_chips: NonZeroU64::new(WAIT).unwrap(),
        maximum_storage_bytes: 4096,
    }
}

fn source() -> RtpSource {
    RtpSource {
        sequence: u16::MAX,
        ssrc: SSRC,
        payload_type: PAYLOAD_TYPE,
    }
}

struct Harness {
    media: UplinkAssembler,
    rtp: RtpSource,
}

impl Harness {
    fn new(media: UplinkAssembler) -> Self {
        Self {
            media,
            rtp: source(),
        }
    }

    fn push(&mut self, piece: Piece<'_>) -> Result<(), Error> {
        self.media.push(piece)
    }

    fn counters(&self) -> Counters {
        self.media.counters()
    }

    fn publish<E>(
        &mut self,
        now: ChipInstant,
        clock: SourceClock,
        send: impl FnOnce(&[u8]) -> Result<bool, E>,
    ) -> Result<Progress, PublishError<E>> {
        self.media.publish(now, clock, &mut self.rtp, send)
    }
}

fn assembler(rows: &[&[u16]], delivery: &[Delivery], kind: DataPduType) -> Harness {
    Harness::new(
        UplinkAssembler::new(negotiated(rows, kind), at(0).epoch, delivery, limits(3)).unwrap(),
    )
}

fn clock() -> SourceClock {
    SourceClock::new(at(0), NonZeroU64::new(INTERVAL).unwrap(), 0, 0).unwrap()
}

fn piece<'a>(
    time: u64,
    subflow: usize,
    index: usize,
    blocks: usize,
    data: &'a [u8],
    width: usize,
    quality: RadioQuality,
) -> Piece<'a> {
    Piece {
        source: at(time),
        subflow,
        index,
        blocks: count(blocks),
        quality,
        data: Bits::new(data, width).unwrap(),
    }
}

fn publish(assembler: &mut Harness, now: u64, clock: SourceClock) -> (Progress, Vec<u8>) {
    let mut packet = Vec::new();
    let progress = assembler
        .publish(at(now), clock, |bytes| {
            packet.extend_from_slice(bytes);
            Ok::<_, ()>(true)
        })
        .unwrap();
    (progress, packet)
}

fn assert_data(bytes: &[u8], number: u8, quality: FrameQuality, rfci: u8, payload: &[u8]) {
    let packet = rtp::decode(bytes).unwrap();
    assert_eq!(packet.payload_type, PAYLOAD_TYPE);
    assert_eq!(packet.ssrc, SSRC);
    assert!(!packet.marker);
    let Frame::Data {
        number: actual_number,
        quality: actual_quality,
        rfci: actual_rfci,
        payload: actual_payload,
        ..
    } = iuup::decode(packet.payload).unwrap()
    else {
        panic!("expected Iu-UP data")
    };
    assert_eq!(
        (actual_number, actual_quality, actual_rfci),
        (number, quality, rfci)
    );
    assert_eq!(actual_payload, payload);
}

#[test]
fn reordered_block_fragments_pack_subflows_without_octet_gaps() {
    for kind in [DataPduType::Crc, DataPduType::NoCrc] {
        let mut assembler = assembler(&[&[6, 3]], &[Delivery::Yes, Delivery::Yes], kind);
        assembler
            .push(piece(0, 0, 1, 2, &[0x60], 3, RadioQuality::Good))
            .unwrap();
        assembler
            .push(piece(0, 1, 0, 1, &[0xc0], 3, RadioQuality::Good))
            .unwrap();
        assert_eq!(
            publish(&mut assembler, 0, clock()),
            (Progress::Waiting, vec![])
        );
        assembler
            .push(piece(0, 0, 0, 2, &[0xa0], 3, RadioQuality::Good))
            .unwrap();
        let (progress, bytes) = publish(&mut assembler, 0, clock());
        assert_eq!(progress, Progress::Sent);
        assert_data(&bytes, 0, FrameQuality::Good, 0, &[0xaf, 0]);
        let packet = rtp::decode(&bytes).unwrap();
        let header = if kind == DataPduType::Crc {
            CHECKED_IUUP_HEADER_BYTES
        } else {
            UNCHECKED_IUUP_HEADER_BYTES
        };
        assert_eq!(packet.payload.len(), header + 2);
        let Frame::Data { payload_crc, .. } = iuup::decode(packet.payload).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            payload_crc,
            if kind == DataPduType::Crc {
                PayloadCheck::Valid
            } else {
                PayloadCheck::Absent
            }
        );
    }
}

#[test]
fn source_gaps_and_wrap_drive_frame_and_timestamp_not_poll_time() {
    let mut assembler = assembler(&[&[8]], &[Delivery::Yes], DataPduType::Crc);
    let clock = SourceClock::new(
        at(0),
        NonZeroU64::new(INTERVAL).unwrap(),
        u32::MAX - 159,
        15,
    )
    .unwrap();
    for (ordinal, number, timestamp, sequence) in [
        (0, 15, u32::MAX - 159, u16::MAX),
        (3, 2, 800, 0),
        (20, 3, 6240, 1),
    ] {
        assembler
            .push(piece(
                ordinal * INTERVAL,
                0,
                0,
                1,
                &[0xa5],
                8,
                RadioQuality::Good,
            ))
            .unwrap();
        let (progress, bytes) = publish(&mut assembler, (ordinal + 5) * INTERVAL, clock);
        assert_eq!(progress, Progress::Sent);
        assert_data(&bytes, number, FrameQuality::Good, 0, &[0xa5]);
        let packet = rtp::decode(&bytes).unwrap();
        assert_eq!(packet.timestamp, timestamp);
        assert_eq!(packet.sequence, sequence);
    }
}

#[test]
fn transport_backpressure_and_error_retry_identical_datagrams() {
    let mut assembler = assembler(&[&[8]], &[Delivery::Yes], DataPduType::Crc);
    assembler
        .push(piece(0, 0, 0, 1, &[0x69], 8, RadioQuality::Good))
        .unwrap();
    let mut first = Vec::new();
    assert_eq!(
        assembler
            .publish(at(0), clock(), |bytes| {
                first.extend_from_slice(bytes);
                Ok::<_, &str>(false)
            })
            .unwrap(),
        Progress::Backpressure
    );
    assert!(matches!(
        assembler.publish(at(INTERVAL), clock(), |bytes| {
            assert_eq!(bytes, first);
            Err("transport unavailable")
        }),
        Err(PublishError::Transport("transport unavailable"))
    ));
    let (progress, accepted) = publish(&mut assembler, 2 * INTERVAL, clock());
    assert_eq!(progress, Progress::Sent);
    assert_eq!(accepted, first);
    assembler
        .push(piece(3 * INTERVAL, 0, 0, 1, &[0x96], 8, RadioQuality::Good))
        .unwrap();
    let (_, next) = publish(&mut assembler, 3 * INTERVAL, clock());
    assert_eq!(rtp::decode(&first).unwrap().sequence, u16::MAX);
    assert_eq!(rtp::decode(&next).unwrap().sequence, 0);
    assert_eq!(assembler.counters().backpressure, 1);
    assert_eq!(assembler.counters().sent, 2);
}

#[test]
fn control_and_data_use_one_accepted_datagram_sequence() {
    let mut assembler = assembler(&[&[8]], &[Delivery::Yes], DataPduType::Crc);
    let mut control = [0; 32];
    let bytes = iuup::encode(
        Frame::Control {
            number: 0,
            acknowledgment: Acknowledgment::Positive,
            mode: 1,
            procedure: 0,
            payload_crc: PayloadCheck::Absent,
            payload: &[],
        },
        &mut control,
    )
    .unwrap();
    let mut output = [0; 64];
    assert!(
        assembler
            .rtp
            .publish(0, &control[..bytes], &mut output, |packet| {
                let packet = rtp::decode(packet).unwrap();
                assert_eq!(packet.sequence, u16::MAX);
                assert_eq!(packet.payload, &control[..bytes]);
                Ok::<_, ()>(true)
            })
            .unwrap()
    );
    assert!(
        !assembler
            .rtp
            .publish(0, &control[..bytes], &mut output, |_| Ok::<_, ()>(false))
            .unwrap()
    );
    assert!(matches!(
        assembler
            .rtp
            .publish(0, &control[..bytes], &mut output, |_| Err("unavailable")),
        Err(RtpPublishError::Transport("unavailable"))
    ));
    assembler
        .push(piece(0, 0, 0, 1, &[0xa5], 8, RadioQuality::Good))
        .unwrap();
    let (progress, bytes) = publish(&mut assembler, 0, clock());
    assert_eq!(progress, Progress::Sent);
    assert_eq!(rtp::decode(&bytes).unwrap().sequence, 0);
    assert_data(&bytes, 0, FrameQuality::Good, 0, &[0xa5]);
    assert_eq!(assembler.rtp.sequence, 1);
}

#[test]
fn invalid_rtp_configuration_and_capacity_do_not_advance_sequence() {
    let mut output = [0; 32];
    for payload_type in [PAYLOAD_TYPE - 1, DYNAMIC_PAYLOAD_LAST + 1] {
        let mut source = RtpSource {
            payload_type,
            ..source()
        };
        assert!(matches!(
            source.publish(0, &[0xa5], &mut output, |_| -> Result<bool, ()> {
                panic!("invalid RTP header reached transport")
            }),
            Err(RtpPublishError::Wire(WireError::Header))
        ));
        assert_eq!(source.sequence, u16::MAX);
    }
    let mut source = source();
    assert!(matches!(
        source.publish(0, &[0xa5], &mut [], |_| -> Result<bool, ()> {
            panic!("undersized RTP buffer reached transport")
        }),
        Err(RtpPublishError::Wire(WireError::Capacity))
    ));
    assert_eq!(source.sequence, u16::MAX);
}

#[test]
fn delivery_policies_preserve_radio_quality_semantics() {
    for policy in [Delivery::No, Delivery::Yes, Delivery::NoDetect] {
        for quality in [
            RadioQuality::Good,
            RadioQuality::Bad,
            RadioQuality::NotReported,
        ] {
            let mut assembler = assembler(&[&[8]], &[policy], DataPduType::Crc);
            assembler
                .push(piece(0, 0, 0, 1, &[0x5a], 8, quality))
                .unwrap();
            let (progress, bytes) = publish(&mut assembler, 0, clock());
            if policy == Delivery::No && quality == RadioQuality::Bad {
                assert_eq!(progress, Progress::Dropped);
                assert!(bytes.is_empty());
                assert_eq!(assembler.counters().suppressed, 1);
            } else {
                let expected = if policy == Delivery::Yes && quality == RadioQuality::Bad {
                    FrameQuality::BadRadio
                } else {
                    FrameQuality::Good
                };
                assert_eq!(progress, Progress::Sent);
                assert_data(&bytes, 0, expected, 0, &[0x5a]);
            }
        }
    }
    let mut assembler = assembler(&[&[8, 8]], &[Delivery::No, Delivery::Yes], DataPduType::Crc);
    for subflow in 0..2 {
        assembler
            .push(piece(0, subflow, 0, 1, &[0x5a], 8, RadioQuality::Bad))
            .unwrap();
    }
    assert_eq!(
        publish(&mut assembler, 0, clock()),
        (Progress::Dropped, vec![])
    );
}

#[test]
fn deadline_missing_yes_subflow_zero_fills_missing_bits_and_marks_bad_radio() {
    for partially_present in [false, true] {
        let mut assembler = assembler(
            &[&[3, 6]],
            &[Delivery::Yes, Delivery::Yes],
            DataPduType::Crc,
        );
        assembler
            .push(piece(0, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
            .unwrap();
        if partially_present {
            assembler
                .push(piece(0, 1, 0, 2, &[0xe0], 3, RadioQuality::Good))
                .unwrap();
        }
        assert_eq!(
            publish(&mut assembler, WAIT - 1, clock()),
            (Progress::Waiting, vec![])
        );
        let (progress, bytes) = publish(&mut assembler, WAIT, clock());
        assert_eq!(progress, Progress::Sent);
        let payload = if partially_present {
            [0xbc, 0]
        } else {
            [0xa0, 0]
        };
        assert_data(&bytes, 0, FrameQuality::BadRadio, 0, &payload);
        assert_eq!(assembler.counters().missing, 1);
        assert_eq!(assembler.counters().bad_radio, 1);
        assert_eq!(
            assembler.push(piece(0, 1, 1, 2, &[0xe0], 3, RadioQuality::Good)),
            Err(Error::Retired)
        );
    }
}

#[test]
fn missing_no_or_no_detect_subflow_suppresses_without_consuming_sequence() {
    for policy in [Delivery::No, Delivery::NoDetect] {
        let mut assembler = assembler(&[&[3, 6]], &[Delivery::Yes, policy], DataPduType::Crc);
        assembler
            .push(piece(0, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
            .unwrap();
        assert_eq!(
            publish(&mut assembler, WAIT, clock()),
            (Progress::Dropped, vec![])
        );
        assembler
            .push(piece(INTERVAL, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
            .unwrap();
        assembler
            .push(piece(INTERVAL, 1, 0, 1, &[0xc0], 6, RadioQuality::Good))
            .unwrap();
        let (progress, bytes) = publish(&mut assembler, INTERVAL, clock());
        assert_eq!(progress, Progress::Sent);
        assert_eq!(rtp::decode(&bytes).unwrap().sequence, u16::MAX);
        assert_eq!(assembler.counters().missing, 1);
        assert_eq!(assembler.counters().suppressed, 1);
    }
}

#[test]
fn deadline_ambiguity_is_dropped_instead_of_selecting_a_format() {
    let mut assembler = assembler(
        &[&[3, 6], &[3, 9]],
        &[Delivery::Yes, Delivery::Yes],
        DataPduType::Crc,
    );
    assembler
        .push(piece(0, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
        .unwrap();
    assert_eq!(
        publish(&mut assembler, WAIT - 1, clock()),
        (Progress::Waiting, vec![])
    );
    assert_eq!(
        publish(&mut assembler, WAIT, clock()),
        (Progress::Dropped, vec![])
    );
    assert_eq!(assembler.counters().ambiguous, 1);
    assert_eq!(assembler.counters().sent, 0);
    assembler
        .push(piece(INTERVAL, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
        .unwrap();
    assembler
        .push(piece(
            INTERVAL,
            1,
            0,
            1,
            &[0xff, 0x80],
            9,
            RadioQuality::Good,
        ))
        .unwrap();
    let (progress, bytes) = publish(&mut assembler, INTERVAL, clock());
    assert_eq!(progress, Progress::Sent);
    assert_data(&bytes, 1, FrameQuality::Good, 1, &[0xbf, 0xf0]);
}

#[test]
fn invalid_and_duplicate_fragments_do_not_replace_accepted_payload() {
    let mut assembler = assembler(&[&[6]], &[Delivery::Yes], DataPduType::Crc);
    assert_eq!(
        assembler.push(piece(0, 1, 0, 1, &[0], 6, RadioQuality::Good)),
        Err(Error::Geometry)
    );
    assert_eq!(
        assembler.push(piece(0, 0, 2, 2, &[0], 3, RadioQuality::Good)),
        Err(Error::Geometry)
    );
    assert_eq!(
        assembler.push(piece(
            0,
            0,
            0,
            MAX_FRAGMENTS + 1,
            &[0],
            1,
            RadioQuality::Good
        )),
        Err(Error::Geometry)
    );
    assert_eq!(
        assembler.push(piece(0, 0, 0, 1, &[], 0, RadioQuality::Good)),
        Err(Error::Geometry)
    );
    assert_eq!(
        assembler.push(piece(0, 0, 0, 1, &[0], 5, RadioQuality::Good)),
        Err(Error::Geometry)
    );
    assembler
        .push(piece(0, 0, 0, 2, &[0xa0], 3, RadioQuality::Good))
        .unwrap();
    assert_eq!(
        assembler.push(piece(0, 0, 0, 2, &[0xe0], 3, RadioQuality::Bad)),
        Err(Error::Duplicate)
    );
    assert_eq!(
        assembler.push(piece(0, 0, 1, 3, &[0xc0], 2, RadioQuality::Good)),
        Err(Error::Geometry)
    );
    assembler
        .push(piece(0, 0, 1, 2, &[0x60], 3, RadioQuality::Good))
        .unwrap();
    let (progress, bytes) = publish(&mut assembler, 0, clock());
    assert_eq!(progress, Progress::Sent);
    assert_data(&bytes, 0, FrameQuality::Good, 0, &[0xac]);
    assert_eq!(
        assembler.push(piece(0, 0, 0, 1, &[0], 6, RadioQuality::Good)),
        Err(Error::Retired)
    );
}

#[test]
fn bounded_capacity_preserves_existing_frames_and_oldest_source_order() {
    let negotiated = negotiated(&[&[8]], DataPduType::Crc);
    let mut assembler = Harness::new(
        UplinkAssembler::new(negotiated, at(0).epoch, &[Delivery::Yes], limits(2)).unwrap(),
    );
    assembler
        .push(piece(INTERVAL, 0, 0, 1, &[0x22], 8, RadioQuality::Good))
        .unwrap();
    assembler
        .push(piece(0, 0, 0, 1, &[0x11], 8, RadioQuality::Good))
        .unwrap();
    assert_eq!(
        assembler.push(piece(2 * INTERVAL, 0, 0, 1, &[0x33], 8, RadioQuality::Good)),
        Err(Error::Capacity)
    );
    let (_, bytes) = publish(&mut assembler, INTERVAL, clock());
    assert_data(&bytes, 0, FrameQuality::Good, 0, &[0x11]);
    let (_, bytes) = publish(&mut assembler, INTERVAL, clock());
    assert_data(&bytes, 1, FrameQuality::Good, 0, &[0x22]);
    assert_eq!(
        publish(&mut assembler, INTERVAL, clock()),
        (Progress::Idle, vec![])
    );
}

#[test]
fn retired_piece_does_not_reopen_frame_or_prevent_next_frame() {
    let mut assembler = assembler(&[&[3, 6]], &[Delivery::Yes, Delivery::No], DataPduType::Crc);
    assembler
        .push(piece(0, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
        .unwrap();
    assert_eq!(publish(&mut assembler, WAIT, clock()).0, Progress::Dropped);
    assert_eq!(
        assembler.push(piece(0, 1, 0, 1, &[0xfc], 6, RadioQuality::Good)),
        Err(Error::Retired)
    );
    assert_eq!(assembler.counters().retired_pieces, 1);
    let mut foreign = piece(0, 1, 0, 1, &[0xfc], 6, RadioQuality::Good);
    foreign.source.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
    assert_eq!(assembler.push(foreign), Err(Error::Time));
    assert_eq!(
        assembler.push(piece(0, 1, 0, 1, &[0], 5, RadioQuality::Good)),
        Err(Error::Geometry)
    );
    assembler
        .push(piece(INTERVAL, 0, 0, 1, &[0xa0], 3, RadioQuality::Good))
        .unwrap();
    assembler
        .push(piece(INTERVAL, 1, 0, 1, &[0xfc], 6, RadioQuality::Good))
        .unwrap();
    assert_eq!(publish(&mut assembler, INTERVAL, clock()).0, Progress::Sent);
    assert_eq!(assembler.counters().sent, 1);
    assert_eq!(assembler.counters().retired_pieces, 1);
}

#[test]
fn configuration_and_storage_budgets_are_checked_before_use() {
    let formats = negotiated(&[&[8]], DataPduType::Crc);
    let make = |delivery: &[Delivery], limits| {
        UplinkAssembler::new(formats.clone(), at(0).epoch, delivery, limits)
    };
    assert!(matches!(make(&[], limits(1)), Err(Error::Configuration)));
    assert!(matches!(
        make(&[Delivery::Yes], limits(MAX_MEDIA_SLOTS + 1)),
        Err(Error::Configuration)
    ));
    assert!(matches!(
        make(
            &[Delivery::Yes],
            Limits {
                maximum_storage_bytes: 0,
                ..limits(1)
            }
        ),
        Err(Error::Capacity)
    ));
    assert!(SourceClock::new(at(0), NonZeroU64::MIN, 0, 0).is_err());
    assert!(
        SourceClock::new(
            at(0),
            NonZeroU64::new(INTERVAL).unwrap(),
            0,
            FRAME_MODULUS as u8
        )
        .is_err()
    );
}

#[test]
fn epoch_and_time_regressions_do_not_publish_or_retire_payload() {
    let mut assembler = assembler(&[&[8]], &[Delivery::Yes], DataPduType::Crc);
    let foreign = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 0,
    };
    let mut bad = piece(0, 0, 0, 1, &[0xa5], 8, RadioQuality::Good);
    bad.source = foreign;
    assert_eq!(assembler.push(bad), Err(Error::Time));
    assert_eq!(
        assembler.push(piece(u64::MAX, 0, 0, 1, &[0xa5], 8, RadioQuality::Good)),
        Err(Error::Time)
    );
    assembler
        .push(piece(INTERVAL, 0, 0, 1, &[0xa5], 8, RadioQuality::Good))
        .unwrap();
    assert_eq!(
        publish(&mut assembler, 0, clock()),
        (Progress::Waiting, vec![])
    );
    assert!(matches!(
        assembler.publish(foreign, clock(), |_| Ok::<_, ()>(true)),
        Err(PublishError::Media(Error::Time))
    ));
    assert_eq!(
        publish(&mut assembler, INTERVAL - 1, clock()),
        (Progress::Waiting, vec![])
    );
    assert!(matches!(
        assembler.publish(at(INTERVAL - 2), clock(), |_| Ok::<_, ()>(true)),
        Err(PublishError::Media(Error::Time))
    ));
    let wrong_clock = SourceClock::new(foreign, NonZeroU64::new(INTERVAL).unwrap(), 0, 0).unwrap();
    assert!(matches!(
        assembler.publish(at(INTERVAL), wrong_clock, |_| Ok::<_, ()>(true)),
        Err(PublishError::Media(Error::Time))
    ));
    let (progress, bytes) = publish(&mut assembler, INTERVAL, clock());
    assert_eq!(progress, Progress::Sent);
    assert_data(&bytes, 1, FrameQuality::Good, 0, &[0xa5]);
    assert_eq!(clock().validate(at(1)), Err(Error::Time));
    let later = SourceClock::new(at(INTERVAL), NonZeroU64::new(INTERVAL).unwrap(), 0, 0).unwrap();
    assert_eq!(later.validate(at(0)), Err(Error::Time));
}
