// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::iuup::{
    Acknowledgment,
    initialization::{Combination, Initialization, Versions},
    negotiation::Receiver,
};
use std::num::NonZeroU64;

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn negotiated() -> Negotiated {
    let formats = Initialization::new(
        &[
            Combination::new(Rfci::new(0).unwrap(), &[3, 0, 6], None).unwrap(),
            Combination::new(Rfci::new(1).unwrap(), &[0, 0, 0], None).unwrap(),
        ],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap();
    let mut payload = [0; 64];
    let bytes = formats.encode(&mut payload).unwrap();
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
    receiver.publish(&mut [0; 64], |_| Ok::<_, ()>(())).unwrap();
    receiver.negotiated().unwrap().clone()
}
fn data(rfci: u8) -> Frame<'static> {
    Frame::Data {
        number: 3,
        quality: FrameQuality::Good,
        rfci,
        payload_crc: PayloadCheck::Valid,
        payload: &[0xb9, 0xff],
    }
}

#[test]
fn subflow_admission_is_transactional_and_completed_frames_release_capacity() {
    let mut queue = MediaQueue::new(negotiated(), at(0).epoch, count(1), 2).unwrap();
    queue.enqueue(at(0), data(0)).unwrap();
    assert_eq!(queue.offer(at(0), 0).unwrap().bits, 3);
    assert_eq!(queue.offer(at(0), 1), None);
    assert_eq!(queue.offer(at(0), 7), None);
    assert!(matches!(
        queue.publish(at(0), 0, |_| Err("busy")),
        Err(PublishError::Admission("busy"))
    ));
    assert!(queue.offer(at(0), 0).is_some());
    assert_eq!(queue.queued(), 1);
    queue
        .publish(at(0), 0, |bits| {
            assert_eq!(bits.read_u32(0, 3), Ok(5));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(queue.offer(at(0), 0).is_none());
    assert_eq!(queue.queued(), 1);
    queue
        .publish(at(0), 2, |bits| {
            assert_eq!(bits.read_u32(0, 6), Ok(51));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(queue.queued(), 0);
    assert_eq!(queue.enqueue(at(0), data(0)), Err(Error::Time));
    queue.enqueue(at(100), data(0)).unwrap();
    assert_eq!(queue.counters().published_subflows, 2);
}

#[test]
fn duplicates_overflow_expiry_and_epoch_mismatch_are_isolated() {
    let mut queue = MediaQueue::new(negotiated(), at(0).epoch, count(2), 4).unwrap();
    queue.enqueue(at(200), data(0)).unwrap();
    queue.enqueue(at(100), data(0)).unwrap();
    assert_eq!(queue.enqueue(at(200), data(0)), Err(Error::Duplicate));
    assert_eq!(queue.enqueue(at(300), data(0)), Err(Error::Capacity));
    queue.retire_before(at(200)).unwrap();
    assert_eq!(queue.queued(), 1);
    assert!(queue.offer(at(200), 2).is_some());
    assert_eq!(queue.enqueue(at(100), data(0)), Err(Error::Time));
    assert_eq!(queue.retire_before(at(199)), Err(Error::Time));
    let foreign = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 300,
    };
    assert_eq!(queue.enqueue(foreign, data(0)), Err(Error::Time));
    assert_eq!(queue.retire_before(foreign), Err(Error::Time));
    queue.enqueue(at(300), data(0)).unwrap();
    assert_eq!(queue.counters().expired, 1);
    assert_eq!(queue.counters().overflow, 1);
    assert_eq!(queue.counters().duplicate, 1);
}

#[test]
fn no_data_and_invalid_quality_do_not_consume_queue_slots() {
    let mut queue = MediaQueue::new(negotiated(), at(0).epoch, count(1), 2).unwrap();
    queue.enqueue(at(0), data(1)).unwrap();
    assert_eq!(queue.queued(), 0);
    assert_eq!(queue.counters().no_data, 1);
    let Frame::Data {
        number,
        rfci,
        payload_crc,
        payload,
        ..
    } = data(0)
    else {
        unreachable!()
    };
    assert_eq!(
        queue.enqueue(
            at(100),
            Frame::Data {
                number,
                rfci,
                payload_crc,
                payload,
                quality: FrameQuality::BadRadio
            }
        ),
        Err(Error::Media(MediaError::Quality))
    );
    assert_eq!(queue.queued(), 0);
    assert!(MediaQueue::new(negotiated(), at(0).epoch, count(2), 3).is_err());
    assert!(
        MediaQueue::new(
            negotiated(),
            at(0).epoch,
            count(MAX_MEDIA_SLOTS + 1),
            usize::MAX
        )
        .is_err()
    );
}

#[test]
fn rtp_mapping_handles_timestamp_wrap_without_arrival_clock_subtraction() {
    let anchor = RtpAnchor::new(u32::MAX - 159, at(1000), NonZeroU32::new(640).unwrap()).unwrap();
    assert_eq!(anchor.map(160).unwrap(), at(1000 + 320 * 240));
    assert_eq!(anchor.map(u32::MAX - 160), Err(Error::Time));
    assert_eq!(anchor.map(481), Err(Error::Time));
    let overflow = RtpAnchor::new(0, at(u64::MAX), NonZeroU32::MIN).unwrap();
    assert_eq!(overflow.map(1), Err(Error::Time));
    assert!(RtpAnchor::new(0, at(0), NonZeroU32::new(RTP_HALF_RANGE).unwrap()).is_err());
    let mut pdu = [0; 64];
    let bytes = crate::iuup::encode(data(0), &mut pdu).unwrap();
    let mut packet = [0; 128];
    let bytes = crate::rtp::encode(
        crate::rtp::Packet {
            marker: false,
            payload_type: 96,
            sequence: 10,
            timestamp: 160,
            ssrc: 7,
            payload: &pdu[..bytes],
        },
        &mut packet,
    )
    .unwrap();
    let mut queue = MediaQueue::new(negotiated(), at(0).epoch, count(1), 2).unwrap();
    queue.enqueue_rtp(&packet[..bytes], anchor).unwrap();
    assert!(queue.offer(at(1000 + 320 * 240), 0).is_some());
    packet[bytes - 1] ^= 1;
    assert_eq!(
        queue.enqueue_rtp(&packet[..bytes], anchor),
        Err(IngressError::Media(Error::Media(MediaError::PayloadCrc)))
    );
}
#[test]
fn bounded_route_projection_advances_without_retiming_or_limiting_session_duration() {
    let horizon = NonZeroU32::new(640).unwrap();
    let first_timestamp = u32::MAX - 159;
    let first = RtpAnchor::new(first_timestamp, at(100_000), horizon).unwrap();
    let mut reference = first;
    for step in 1..100u32 {
        let timestamp = first_timestamp.wrapping_add(step * 320);
        let expected = at(100_000 + u64::from(step) * 320 * 240);
        assert_eq!(reference.project(timestamp).unwrap(), expected);
        let advanced = reference.advance(timestamp).unwrap();
        assert_eq!(advanced.project(timestamp).unwrap(), expected);
        let older = timestamp.wrapping_sub(320);
        assert_eq!(
            advanced.project(older).unwrap(),
            reference.project(older).unwrap()
        );
        assert!(advanced.advance(older).is_err());
        assert!(advanced.project(timestamp.wrapping_add(641)).is_err());
        assert!(advanced.project(timestamp.wrapping_sub(641)).is_err());
        reference = advanced;
    }
    let underflow = RtpAnchor::new(1, at(0), horizon).unwrap();
    assert_eq!(underflow.project(0), Err(Error::Time));
    let overflow = RtpAnchor::new(0, at(u64::MAX), horizon).unwrap();
    assert_eq!(overflow.project(1), Err(Error::Time));
}
