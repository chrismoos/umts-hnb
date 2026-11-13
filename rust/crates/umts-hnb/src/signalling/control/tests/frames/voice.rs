// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::TransparentTraffic,
    voice_bearers::{Error, Limits, VoiceHandle, VoiceKey, VoiceTraffic},
};
use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};
use umts_types::{ConnectionKey, bits::Bits};
use umts_user_plane::{
    iuup::{
        self, Frame, FrameQuality, PayloadCheck,
        initialization::{Combination, DataPduType, Initialization, ModeVersion, Rfci, Versions},
        negotiation::{AckEffect, Sender, Settings},
    },
    media::RtpAnchor,
    rtp,
};

mod negotiation;
mod radio_handoff;
mod transport;

fn key(identity: ReceiveIdentity) -> VoiceKey {
    VoiceKey {
        connection: ConnectionKey {
            ue: identity.ue,
            domain: CnDomain::Cs,
        },
        rab: 1,
    }
}

fn prepare(f: &Fixture) -> (VoiceTraffic, VoiceHandle) {
    let mut source = VoiceTraffic::new(Limits {
        routes: count(2),
        frames_per_route: count(2),
        maximum_payload_bytes: count(64),
        maximum_storage_bytes: 32 * 1024,
    })
    .unwrap();
    let handle = source
        .prepare(
            key(f.bank.identity()),
            f.bank.identity(),
            f.plan.clone(),
            &[10, 11, 12].map(|id| BearerId::new(id).unwrap()),
            Versions::new(3).unwrap(),
            uplink_policy(),
        )
        .unwrap();
    (source, handle)
}

fn sender(rfci: u8, lengths: &[u16]) -> Sender {
    let frame = Initialization::new(
        &[Combination::new(Rfci::new(rfci).unwrap(), lengths, None).unwrap()],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap();
    Sender::new(
        &[frame],
        ModeVersion::new(1).unwrap(),
        Settings::new(Duration::from_millis(100), 3, Versions::new(3).unwrap()).unwrap(),
    )
    .unwrap()
}

fn uplink_policy() -> crate::voice_bearers::UplinkPolicy {
    use umts_user_plane::uplink_media::{Delivery, RtpSource};
    crate::voice_bearers::UplinkPolicy {
        delivery: vec![Delivery::No, Delivery::Yes, Delivery::NoDetect].into_boxed_slice(),
        interval_chips: NonZeroU64::new(2 * CHIPS_PER_FRAME).unwrap(),
        wait_chips: NonZeroU64::new(4 * CHIPS_PER_FRAME).unwrap(),
        timestamp: 9000,
        rtp: RtpSource {
            sequence: 20,
            ssrc: 77,
            payload_type: 96,
        },
    }
}

fn receive_voice_part(
    f: &mut Fixture,
    source: &mut VoiceTraffic,
    bearer_id: u8,
    width: usize,
    frame: u64,
) {
    use crate::{
        dedicated_bearers::ResetPermission,
        dedicated_rx::{DecodedTransport, TransportDelivery},
    };
    let transport = TransportId::new(
        f.plan
            .configuration()
            .bearer(bearer_id)
            .unwrap()
            .mapping
            .uplink[0]
            .transport,
    )
    .unwrap();
    let definition = f.plan.uplink_configured().definition(transport).unwrap();
    let format_index = definition
        .formats
        .iter()
        .position(|format| format.blocks == 1 && format.block_bits == width)
        .unwrap();
    let format = definition.formats[format_index];
    let payload: Vec<_> = (0..width)
        .map(|index| Bit::from(index.is_multiple_of(3)))
        .collect();
    f.bank
        .enqueue_transport(TransportDelivery {
            decoded: DecodedTransport {
                identity: f.bank.identity(),
                transport,
                format: format_index,
                start: at(frame * CHIPS_PER_FRAME),
                block_bits: width,
                blocks: 1,
                crc_checked: format.crc.width() != 0,
            },
            payload: &payload,
        })
        .unwrap();
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            for _ in 0..8 {
                f.bank
                    .service(at(10 * CHIPS_PER_FRAME), ciphers, ResetPermission::Defer)
                    .unwrap();
            }
        })
        .unwrap();
    assert!(f.bank.process_one(|delivery| {
        let origin = delivery.metadata.transparent_origin.unwrap();
        assert_eq!(origin.start, at(frame * CHIPS_PER_FRAME));
        assert!(delivery.metadata.received.index > origin.start.index);
        source.uplink(delivery).unwrap()
    }));
}

#[test]
fn original_uplink_tm_outputs_reassemble_into_wire_rtp_without_using_service_time() {
    use std::io;
    use umts_user_plane::{iu_cs::RtpEndpoint, packet::Datagrams, uplink_media::Progress};
    let mut f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    let mut control = Vec::new();
    source
        .publish_initialization_rtp(handle, 9000, &mut [0; 256], &mut [0; 256], |bytes| {
            control.extend_from_slice(bytes);
            Ok::<_, ()>(true)
        })
        .unwrap();
    let control = rtp::decode(&control).unwrap();
    assert_eq!(control.sequence, 20);
    assert_eq!(
        peer.acknowledgment(iuup::decode(control.payload).unwrap(), now)
            .unwrap(),
        AckEffect::Complete
    );
    for (bearer, width) in [(12, 60), (10, 81), (11, 103)] {
        receive_voice_part(&mut f, &mut source, bearer, width, 0);
    }
    let mut retained = Vec::new();
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                retained.extend_from_slice(bytes);
                Ok::<_, io::Error>(false)
            })
            .unwrap(),
        Progress::Backpressure
    );
    let mut endpoint = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut sink = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let destination = sink.local_address().unwrap();
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                assert_eq!(bytes, retained);
                endpoint.send(bytes, destination)
            })
            .unwrap(),
        Progress::Sent
    );
    let mut bytes = [0; 256];
    let mut received = None;
    for _ in 0..100 {
        if let Some(value) = sink.receive(&mut bytes).unwrap() {
            received = Some(value);
            break;
        }
        std::thread::yield_now();
    }
    let (length, from) = received.expect("local RTP datagram");
    assert_eq!(from, endpoint.local_address().unwrap());
    let packet = rtp::decode(&bytes[..length]).unwrap();
    assert_eq!(
        (packet.sequence, packet.timestamp, packet.ssrc),
        (21, 9000, 77)
    );
    let Frame::Data {
        number,
        rfci,
        quality,
        payload_crc,
        payload,
    } = iuup::decode(packet.payload).unwrap()
    else {
        panic!("voice data");
    };
    assert_eq!(
        (number, rfci, quality, payload_crc),
        (0, 0, FrameQuality::Good, PayloadCheck::Valid)
    );
    let bits = Bits::new(payload, 244).unwrap();
    let mut offset = 0;
    for width in [81, 103, 60] {
        for index in 0..width {
            assert_eq!(bits.bit(offset + index), Some(index.is_multiple_of(3)));
        }
        offset += width;
    }
}

#[test]
fn retired_voice_part_is_consumed_and_later_radio_frame_is_delivered() {
    use umts_user_plane::uplink_media::Progress;
    let mut f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut peer, now),
        AckEffect::Complete
    );
    receive_voice_part(&mut f, &mut source, 10, 81, 0);
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |_| Ok::<_, ()>(true))
            .unwrap(),
        Progress::Dropped
    );
    receive_voice_part(&mut f, &mut source, 11, 103, 0);
    receive_voice_part(&mut f, &mut source, 12, 60, 0);
    for (bearer, width) in [(10, 81), (11, 103), (12, 60)] {
        receive_voice_part(&mut f, &mut source, bearer, width, 2);
    }
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |_| Ok::<_, ()>(true))
            .unwrap(),
        Progress::Sent
    );
}

#[test]
fn missing_subflows_are_associated_by_air_tti_not_late_delivery_order() {
    use umts_user_plane::uplink_media::Progress;
    let mut f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    acknowledge(&mut source, handle, &mut peer, now);
    receive_voice_part(&mut f, &mut source, 10, 81, 0);
    receive_voice_part(&mut f, &mut source, 11, 103, 2);
    receive_voice_part(&mut f, &mut source, 12, 60, 0);
    let mut packet = Vec::new();
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |bytes| {
                packet.extend_from_slice(bytes);
                Ok::<_, ()>(true)
            })
            .unwrap(),
        Progress::Sent
    );
    let packet = rtp::decode(&packet).unwrap();
    let Frame::Data {
        quality, payload, ..
    } = iuup::decode(packet.payload).unwrap()
    else {
        panic!("voice data")
    };
    assert_eq!(quality, FrameQuality::BadRadio);
    let missing = Bits::new(payload, 244).unwrap().subspan(81..184).unwrap();
    assert!((0..103).all(|index| missing.bit(index) == Some(false)));
    assert_eq!(
        source
            .publish_uplink(handle, at(10 * CHIPS_PER_FRAME), |_| -> Result<bool, ()> {
                panic!("missing critical class must not transmit")
            })
            .unwrap(),
        Progress::Dropped
    );
}

fn request(source: &mut VoiceTraffic, handle: VoiceHandle, sender: &mut Sender, now: Instant) {
    let mut wire = Vec::new();
    sender
        .publish(now, &mut [0; 256], |bytes| {
            wire.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    source
        .request(handle, iuup::decode(&wire).unwrap())
        .unwrap();
}

fn acknowledge(
    source: &mut VoiceTraffic,
    handle: VoiceHandle,
    sender: &mut Sender,
    now: Instant,
) -> AckEffect {
    let mut wire = Vec::new();
    source
        .publish_initialization(handle, &mut [0; 64], |bytes| {
            wire.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    sender
        .acknowledgment(iuup::decode(&wire).unwrap(), now)
        .unwrap()
}

fn packet(rfci: u8) -> Vec<u8> {
    let mut payload = [0; 64];
    let bytes = iuup::encode(
        Frame::Data {
            number: 0,
            quality: FrameQuality::Good,
            rfci,
            payload_crc: PayloadCheck::Valid,
            payload: &[0x5a; 31],
        },
        &mut payload,
    )
    .unwrap();
    let mut packet = vec![0; rtp::HEADER_BYTES + bytes];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type: 96,
            sequence: 0,
            timestamp: 0,
            ssrc: 7,
            payload: &payload[..bytes],
        },
        &mut packet,
    )
    .unwrap();
    packet
}

fn anchor() -> RtpAnchor {
    RtpAnchor::new(0, at(0), NonZeroU32::new(3200).unwrap()).unwrap()
}

#[test]
fn negotiated_voice_rtp_fills_original_cs_and_packet_radio_frame_together() {
    let mut f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    assert!(source.downlink(handle, &packet(0), anchor()).is_err());
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    assert_eq!(source.queued(handle), None);
    assert!(
        source
            .publish_initialization(handle, &mut [0; 64], |_| Err("socket blocked"))
            .is_err()
    );
    assert_eq!(source.queued(handle), None);
    assert_eq!(
        acknowledge(&mut source, handle, &mut peer, now),
        AckEffect::Complete
    );
    source.downlink(handle, &packet(0), anchor()).unwrap();
    assert_eq!(source.queued(handle), Some(1));
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), &[0x42; 200])
        .unwrap();
    let selected = f
        .bank
        .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024))
        .unwrap();
    assert_eq!(selected, Tfci::new(23).unwrap());
    let mut draft = f
        .bank
        .begin_frame(f.plan.clone(), &f.ports.control, at(0), selected)
        .unwrap();
    draft
        .fill_traffic(at(0), &mut f.radio, &mut source)
        .unwrap();
    assert!(draft.complete());
    let input = draft.finish().ok().unwrap();
    assert_eq!(source.queued(handle), Some(0));
    let payload = [0x5a; 31];
    let expected = Bits::new(&payload, 244).unwrap();
    let mut offset = 0;
    for (transport, width) in [(6, 81), (7, 103), (8, 60)] {
        let actual = &input
            .transports
            .iter()
            .find(|entry| entry.transport.get() == transport)
            .unwrap()
            .bits;
        assert_eq!(actual.len(), width);
        for (index, actual) in actual.iter().enumerate() {
            assert_eq!(*actual == Bit::One, expected.bit(offset + index).unwrap());
        }
        offset += width;
    }
    assert_eq!(
        input
            .transports
            .iter()
            .find(|entry| entry.transport.get() == 9)
            .unwrap()
            .bits
            .len(),
        4 * 320
    );
}

#[test]
fn radio_publication_rejection_keeps_voice_and_wrong_epoch_or_generation_cannot_offer() {
    let mut f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    acknowledge(&mut source, handle, &mut peer, now);
    source.downlink(handle, &packet(0), anchor()).unwrap();
    let selected = f
        .bank
        .select_traffic(at(0), at(0), &mut f.radio, &source, count(1024))
        .unwrap();
    let formats = f.plan.downlink_configured();
    let row = formats
        .combinations()
        .iter()
        .find(|row| row.tfci == selected)
        .unwrap();
    let index = formats
        .transport_ids()
        .position(|id| id.get() == 6)
        .unwrap();
    let grant = DownlinkGrant {
        identity: f.bank.identity(),
        bearer: BearerId::new(10).unwrap(),
        transport: TransportId::new(6).unwrap(),
        format: row.formats[index],
        start: at(0),
    };
    assert!(
        source
            .publish(grant, |_| Err(BearerError::Capacity.into()))
            .is_err()
    );
    assert_eq!(
        source
            .offer(grant.identity, grant.start, grant.bearer)
            .unwrap()
            .payload_bits_per_block,
        81
    );
    let mut foreign = f.bank.identity();
    foreign.configuration = NonZeroU64::new(foreign.configuration.get() + 1).unwrap();
    assert!(source.offer(foreign, at(0), grant.bearer).is_none());
    let late = RtpAnchor::new(0, at(CHIPS_PER_FRAME), NonZeroU32::new(3200).unwrap()).unwrap();
    assert!(matches!(
        source.downlink(handle, &packet(0), late),
        Err(Error::Radio)
    ));
    source
        .publish(grant, |sdus| {
            assert_eq!(sdus.len(), 1);
            assert_eq!(sdus[0].len(), 81);
            Ok(())
        })
        .unwrap();
    assert!(
        source
            .offer(grant.identity, grant.start, grant.bearer)
            .is_none()
    );
    assert_eq!(source.queued(handle), Some(1));
    source
        .retire_before(handle, at(2 * CHIPS_PER_FRAME))
        .unwrap();
    assert_eq!(source.queued(handle), Some(0));
}

#[test]
fn reinitialization_cannot_publish_old_media_and_stale_handles_cannot_reactivate() {
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut peer = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    acknowledge(&mut source, handle, &mut peer, now);
    source.downlink(handle, &packet(0), anchor()).unwrap();
    let mut next = sender(1, &[81, 103, 60]);
    request(&mut source, handle, &mut next, now);
    assert!(
        source
            .offer(f.bank.identity(), at(0), BearerId::new(10).unwrap())
            .is_none()
    );
    assert!(source.downlink(handle, &packet(0), anchor()).is_err());
    assert_eq!(
        acknowledge(&mut source, handle, &mut next, now),
        AckEffect::Complete
    );
    assert_eq!(source.queued(handle), Some(0));
    source.downlink(handle, &packet(1), anchor()).unwrap();
    assert_eq!(source.queued(handle), Some(1));
    source.release(handle).unwrap();
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
            .request(
                handle,
                Frame::Data {
                    number: 0,
                    quality: FrameQuality::Good,
                    rfci: 0,
                    payload_crc: PayloadCheck::Valid,
                    payload: &[]
                }
            )
            .is_err()
    );
    source.activate(replacement, &f.bank).unwrap();
}

#[test]
fn pending_radio_or_impossible_joint_voice_geometry_cannot_be_accepted() {
    let f = fixture_parts(true, true, true, true);
    let (mut source, handle) = prepare(&f);
    assert!(matches!(
        source.activate(handle, &f.bank),
        Err(Error::Radio)
    ));
    let now = Instant::now();
    let mut peer = sender(0, &[82, 103, 60]);
    request(&mut source, handle, &mut peer, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut peer, now),
        AckEffect::Retry
    );
    assert_eq!(source.queued(handle), None);
    let wrong = VoiceKey {
        connection: ConnectionKey {
            ue: f.bank.identity().ue,
            domain: CnDomain::Ps,
        },
        rab: 2,
    };
    assert!(
        source
            .prepare(
                wrong,
                f.bank.identity(),
                f.plan.clone(),
                &[BearerId::new(10).unwrap()],
                Versions::new(3).unwrap(),
                uplink_policy(),
            )
            .is_err()
    );
}
