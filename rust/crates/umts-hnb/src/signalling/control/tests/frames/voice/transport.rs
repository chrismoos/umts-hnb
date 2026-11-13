// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::voice_bearers::{IngressSettings, InitializationStatus, TransportError};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    io,
    net::SocketAddr,
    rc::Rc,
};
use umts_user_plane::{
    iu_cs::RtpEndpoint,
    packet::{BoundDatagrams, Datagrams},
};

const SOURCE: u32 = 7;
const PAYLOAD_TYPE: u8 = 96;
const DATAGRAM_BYTES: usize = 512;
const ATTEMPTS: usize = 100;

#[test]
fn route_clock_corrects_tick_jitter_without_accumulating_rounding_across_wrap() {
    use crate::voice_bearers::{PlayoutSettings, PlayoutWindow};
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut init = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut init, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut init, now),
        AckEffect::Complete
    );
    let peer: SocketAddr = ([127, 0, 0, 1], 20002).into();
    let timestamp = u32::MAX - 100;
    let data = |rfci, timestamp| {
        let bytes = super::packet(rfci);
        let mut packet = rtp::decode(&bytes).unwrap();
        packet.timestamp = timestamp;
        let mut output = vec![0; bytes.len()];
        rtp::encode(packet, &mut output).unwrap();
        (output, peer)
    };
    let wrong_source = {
        let (bytes, peer) = data(0, 123);
        let mut packet = rtp::decode(&bytes).unwrap();
        packet.ssrc = SOURCE + 1;
        let mut output = vec![0; bytes.len()];
        rtp::encode(packet, &mut output).unwrap();
        (output, peer)
    };
    let memory = Memory {
        inbound: VecDeque::from([
            wrong_source,
            data(1, 0),
            data(0, timestamp),
            data(0, timestamp.wrapping_add(319)),
            data(0, timestamp.wrapping_add(640)),
            data(0, timestamp.wrapping_add(640)),
            data(0, timestamp.wrapping_add(1600)),
            data(0, timestamp.wrapping_add(640)),
        ]),
        sent: Sent::default(),
        blocked: Rc::new(Cell::new(false)),
    };
    let mut transport = source
        .attach_transport(handle, memory, peer, settings(Some(SOURCE)))
        .unwrap();
    let playout = PlayoutSettings {
        lead_chips: NonZeroU64::new(CHIPS_PER_FRAME).unwrap(),
        horizon_ticks: NonZeroU32::new(640).unwrap(),
    };
    let first = transport
        .receive_at(&mut source, now, at(0), playout, count(2))
        .unwrap();
    assert_eq!(
        (first.accepted, first.rejected, first.wrong_source),
        (0, 1, 1)
    );
    let valid = transport
        .receive_at(&mut source, now, at(0), playout, count(2))
        .unwrap();
    assert_eq!((valid.accepted, valid.rejected), (2, 0));
    assert!(
        source
            .offer(
                f.bank.identity(),
                at(2 * CHIPS_PER_FRAME),
                BearerId::new(10).unwrap()
            )
            .is_some()
    );
    assert!(
        source
            .offer(
                f.bank.identity(),
                at(4 * CHIPS_PER_FRAME),
                BearerId::new(10).unwrap()
            )
            .is_some()
    );
    let full = transport
        .receive_at(&mut source, now, at(0), playout, count(1))
        .unwrap();
    assert_eq!((full.accepted, full.rejected), (0, 1));
    let window = PlayoutWindow {
        observed: at(0),
        earliest: at(5 * CHIPS_PER_FRAME),
    };
    let retry = transport
        .receive_in(&mut source, now, window, playout, count(1))
        .unwrap();
    assert_eq!(retry.accepted, 1);
    assert!(
        source
            .offer(
                f.bank.identity(),
                at(6 * CHIPS_PER_FRAME),
                BearerId::new(10).unwrap()
            )
            .is_some()
    );
    let beyond = transport
        .receive_in(&mut source, now, window, playout, count(1))
        .unwrap();
    assert_eq!((beyond.accepted, beyond.rejected), (0, 1));
    let late = transport
        .receive_at(&mut source, now, at(7 * CHIPS_PER_FRAME), playout, count(1))
        .unwrap();
    assert_eq!((late.accepted, late.rejected), (0, 1));
    assert!(
        transport
            .receive_at(&mut source, now, at(0), playout, count(1))
            .is_err()
    );
}

#[test]
fn route_clock_respects_prepared_cursor_without_advancing_observed_time() {
    use crate::voice_bearers::{PlayoutSettings, PlayoutWindow};
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut init = sender(0, &[81, 103, 60]);
    request(&mut source, handle, &mut init, now);
    assert_eq!(
        acknowledge(&mut source, handle, &mut init, now),
        AckEffect::Complete
    );
    let peer: SocketAddr = ([127, 0, 0, 1], 20002).into();
    let data = |timestamp| {
        let bytes = super::packet(0);
        let mut packet = rtp::decode(&bytes).unwrap();
        packet.timestamp = timestamp;
        let mut output = vec![0; bytes.len()];
        rtp::encode(packet, &mut output).unwrap();
        (output, peer)
    };
    let memory = Memory {
        inbound: VecDeque::from([data(9000), data(9320), data(9640)]),
        sent: Sent::default(),
        blocked: Rc::new(Cell::new(false)),
    };
    let mut transport = source
        .attach_transport(handle, memory, peer, settings(Some(SOURCE)))
        .unwrap();
    let playout = PlayoutSettings {
        lead_chips: NonZeroU64::new(CHIPS_PER_FRAME).unwrap(),
        horizon_ticks: NonZeroU32::new(640).unwrap(),
    };
    let first = transport
        .receive_in(
            &mut source,
            now,
            PlayoutWindow {
                observed: at(0),
                earliest: at(8 * CHIPS_PER_FRAME + 1),
            },
            playout,
            count(1),
        )
        .unwrap();
    assert_eq!(first.accepted, 1);
    assert_eq!(source.queued(handle), Some(1));
    assert!(
        source
            .offer(
                f.bank.identity(),
                at(2 * CHIPS_PER_FRAME),
                BearerId::new(10).unwrap()
            )
            .is_none()
    );
    assert!(
        source
            .offer(
                f.bank.identity(),
                at(10 * CHIPS_PER_FRAME),
                BearerId::new(10).unwrap()
            )
            .is_some()
    );
    let window = PlayoutWindow {
        observed: at(CHIPS_PER_FRAME),
        earliest: at(13 * CHIPS_PER_FRAME),
    };
    let stale = transport
        .receive_in(&mut source, now, window, playout, count(1))
        .unwrap();
    assert_eq!((stale.accepted, stale.rejected), (0, 1));
    assert_eq!(source.queued(handle), Some(0));
    let valid = transport
        .receive_in(&mut source, now, window, playout, count(1))
        .unwrap();
    assert_eq!(valid.accepted, 1);
    assert!(
        source
            .offer(
                f.bank.identity(),
                at(14 * CHIPS_PER_FRAME),
                BearerId::new(10).unwrap()
            )
            .is_some()
    );
    let foreign = PlayoutWindow {
        observed: at(CHIPS_PER_FRAME),
        earliest: ChipInstant {
            epoch: umts_types::RadioEpoch::new(NonZeroU64::new(at(0).epoch.get() + 1).unwrap()),
            index: 13 * CHIPS_PER_FRAME,
        },
    };
    assert!(matches!(
        transport.receive_in(&mut source, now, foreign, playout, count(1)),
        Err(TransportError::Route(Error::Radio))
    ));
    assert_eq!(source.queued(handle), Some(1));
}

fn settings(ssrc: Option<u32>) -> IngressSettings {
    IngressSettings {
        datagram_bytes: count(DATAGRAM_BYTES),
        payload_type: PAYLOAD_TYPE,
        ssrc,
    }
}

fn wrap(payload: &[u8], payload_type: u8, ssrc: u32) -> Vec<u8> {
    let mut bytes = vec![0; rtp::HEADER_BYTES + payload.len()];
    rtp::encode(
        rtp::Packet {
            marker: false,
            payload_type,
            sequence: 0,
            timestamp: 0,
            ssrc,
            payload,
        },
        &mut bytes,
    )
    .unwrap();
    bytes
}

fn request_bytes(peer: &mut Sender, now: Instant) -> Vec<u8> {
    let mut bytes = Vec::new();
    peer.publish(now, &mut [0; DATAGRAM_BYTES], |value| {
        bytes.extend_from_slice(value);
        Ok::<_, ()>(())
    })
    .unwrap();
    bytes
}

#[test]
fn real_rtp_pair_accepts_media_source_and_payload_type_distinct_from_initialization() {
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let mut peer = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let local = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut transport = source
        .attach_transport(handle, local, peer.local_address().unwrap(), settings(None))
        .unwrap();
    assert!(matches!(source.release(handle), Err(Error::Attached)));
    let now = Instant::now();
    let mut negotiation = sender(0, &[81, 103, 60]);
    let bytes = wrap(&request_bytes(&mut negotiation, now), PAYLOAD_TYPE, SOURCE);
    assert!(peer.send(&bytes, transport.local_address()).unwrap());
    let mut accepted = 0;
    for _ in 0..ATTEMPTS {
        let progress = transport
            .receive(&mut source, now, anchor(), count(2))
            .unwrap();
        accepted += progress.accepted;
        if accepted > 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(accepted, 1);
    assert_eq!(transport.source(), Some(SOURCE));
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    );
    assert!(
        transport
            .publish_initialization(&mut source, now, 9000)
            .unwrap()
    );
    let mut received = [0; DATAGRAM_BYTES];
    let mut ack = None;
    for _ in 0..ATTEMPTS {
        if let Some((length, from)) = peer.receive(&mut received).unwrap() {
            assert_eq!(from, transport.local_address());
            ack = Some(received[..length].to_vec());
            break;
        }
        std::thread::yield_now();
    }
    let ack = ack.expect("actual RTP acknowledgment");
    let packet = rtp::decode(&ack).unwrap();
    assert_eq!(packet.sequence, 20);
    assert_eq!(
        negotiation
            .acknowledgment(iuup::decode(packet.payload).unwrap(), now)
            .unwrap(),
        AckEffect::Complete
    );
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    );
    let original = super::packet(0);
    let mut media = rtp::decode(&original).unwrap();
    media.ssrc = SOURCE + 1;
    media.payload_type = PAYLOAD_TYPE + 1;
    let mut encoded = vec![0; original.len()];
    rtp::encode(media, &mut encoded).unwrap();
    assert!(peer.send(&encoded, transport.local_address()).unwrap());
    let mut accepted = 0;
    for _ in 0..ATTEMPTS {
        accepted += transport
            .receive(&mut source, now, anchor(), count(1))
            .unwrap()
            .accepted;
        if accepted > 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(accepted, 1);
    assert_eq!(transport.source(), Some(SOURCE + 1));
    assert_eq!(source.queued(handle), Some(1));
    assert!(transport.retire(&mut source).unwrap());
    assert!(!transport.retire(&mut source).unwrap());
    assert!(source.activate(handle, &f.bank).is_err());
}

type Sent = Rc<RefCell<Vec<Vec<u8>>>>;
struct Memory {
    inbound: VecDeque<(Vec<u8>, SocketAddr)>,
    sent: Sent,
    blocked: Rc<Cell<bool>>,
}
impl Datagrams for Memory {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        Ok(self.inbound.pop_front().map(|(bytes, from)| {
            let copied = bytes.len().min(output.len());
            output[..copied].copy_from_slice(&bytes[..copied]);
            (copied, from)
        }))
    }
    fn send(&mut self, bytes: &[u8], _: SocketAddr) -> io::Result<bool> {
        if self.blocked.get() {
            return Ok(false);
        }
        self.sent.borrow_mut().push(bytes.to_vec());
        Ok(true)
    }
}
impl BoundDatagrams for Memory {
    fn local_address(&self) -> io::Result<SocketAddr> {
        Ok(([127, 0, 0, 1], 20000).into())
    }
}

#[test]
fn ingress_budget_peer_and_explicit_source_are_checked_before_negotiation() {
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let peer: SocketAddr = ([127, 0, 0, 1], 20002).into();
    let stranger: SocketAddr = ([127, 0, 0, 1], 20004).into();
    let now = Instant::now();
    let mut sender = sender(0, &[81, 103, 60]);
    let request = request_bytes(&mut sender, now);
    let sent = Sent::default();
    let blocked = Rc::new(Cell::new(true));
    let memory = Memory {
        inbound: VecDeque::from([
            (wrap(&request, PAYLOAD_TYPE, SOURCE), stranger),
            (wrap(&request, PAYLOAD_TYPE, SOURCE + 1), peer),
            (vec![0; DATAGRAM_BYTES], peer),
            (wrap(&request, PAYLOAD_TYPE + 1, SOURCE), peer),
            (super::packet(0), peer),
        ]),
        sent: sent.clone(),
        blocked: blocked.clone(),
    };
    let mut transport = source
        .attach_transport(handle, memory, peer, settings(Some(SOURCE)))
        .unwrap();
    let first = transport
        .receive(&mut source, now, anchor(), count(1))
        .unwrap();
    assert_eq!(
        (
            first.received,
            first.accepted,
            first.wrong_peer,
            first.wrong_payload_type
        ),
        (1, 0, 1, 0)
    );
    let second = transport
        .receive(&mut source, now, anchor(), count(3))
        .unwrap();
    assert_eq!(
        (
            second.received,
            second.accepted,
            second.wrong_source,
            second.malformed
        ),
        (3, 1, 1, 1)
    );
    let held = transport
        .receive(&mut source, now, anchor(), count(3))
        .unwrap();
    assert!(held.awaiting_publication);
    assert_eq!(held.received, 0);
    assert!(
        transport
            .publish_initialization(&mut source, now, 9000)
            .is_err()
    );
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    );
    assert!(sent.borrow().is_empty());
    blocked.set(false);
    assert!(
        transport
            .publish_initialization(&mut source, now, 9000)
            .unwrap()
    );
    assert_eq!(sent.borrow().len(), 1);
    assert_eq!(rtp::decode(&sent.borrow()[0]).unwrap().sequence, 20);
    let data = transport
        .receive(&mut source, now, anchor(), count(2))
        .unwrap();
    assert_eq!((data.received, data.accepted), (1, 1));
    assert_eq!(source.queued(handle), Some(1));
    assert!(matches!(
        transport.receive(&mut source, now, anchor(), count(65)),
        Err(TransportError::Configuration)
    ));
    assert!(transport.retire(&mut source).unwrap());
    let (_, foreign) = prepare(&f);
    assert!(source.activate(foreign, &f.bank).is_err());
}

#[test]
fn foreign_route_cannot_consume_owned_endpoint_or_retire_it() {
    let f = fixture(true);
    let (mut owner, handle) = prepare(&f);
    let (mut foreign, _) = prepare(&f);
    let peer = ([127, 0, 0, 1], 20002).into();
    let memory = Memory {
        inbound: VecDeque::new(),
        sent: Sent::default(),
        blocked: Rc::new(Cell::new(false)),
    };
    let mut transport = owner
        .attach_transport(handle, memory, peer, settings(None))
        .unwrap();
    assert!(
        transport
            .receive(&mut foreign, Instant::now(), anchor(), count(1))
            .is_err()
    );
    assert!(transport.retire(&mut foreign).is_err());
    assert!(matches!(owner.release(handle), Err(Error::Attached)));
    assert!(transport.retire(&mut owner).unwrap());
    let (mut owner, handle) = prepare(&f);
    let local = RtpEndpoint::bind(([127, 0, 0, 1], 0).into()).unwrap();
    let mut invalid = settings(None);
    invalid.payload_type = u8::MAX;
    assert!(matches!(
        owner.attach_transport(handle, local, peer, invalid),
        Err(TransportError::Configuration)
    ));
    owner.release(handle).unwrap();
}

fn voice_initialization() -> Initialization {
    Initialization::new(
        &[Combination::new(Rfci::new(0).unwrap(), &[81, 103, 60], None).unwrap()],
        Versions::new(3).unwrap(),
        DataPduType::Crc,
        false,
    )
    .unwrap()
}

fn sender_settings() -> Settings {
    Settings::new(Duration::from_millis(100), 3, Versions::new(3).unwrap()).unwrap()
}

fn empty() -> Memory {
    Memory {
        inbound: VecDeque::new(),
        sent: Sent::default(),
        blocked: Rc::new(Cell::new(false)),
    }
}

#[test]
fn transport_must_fit_complete_media_before_either_role_can_become_ready() {
    const MEDIA_BYTES: usize = 31;
    const IUUP_BYTES: usize = 4;
    let complete = rtp::HEADER_BYTES + IUUP_BYTES + MEDIA_BYTES;
    let f = fixture(true);
    let peer = ([127, 0, 0, 1], 20002).into();
    let (mut source, handle) = prepare(&f);
    source
        .start_initialization(
            handle,
            &[voice_initialization()],
            ModeVersion::new(1).unwrap(),
            sender_settings(),
        )
        .unwrap();
    for bytes in [32, complete] {
        let mut small = settings(None);
        small.datagram_bytes = count(bytes);
        assert!(matches!(
            source.attach_transport(handle, empty(), peer, small),
            Err(TransportError::Route(Error::Capacity))
        ));
    }
    let mut fits = settings(None);
    fits.datagram_bytes = count(complete + 1);
    let mut transport = source
        .attach_transport(handle, empty(), peer, fits)
        .unwrap();
    assert!(transport.retire(&mut source).unwrap());

    let (mut source, handle) = prepare(&f);
    let mut small = settings(None);
    small.datagram_bytes = count(32);
    let now = Instant::now();
    let mut sender = sender(0, &[81, 103, 60]);
    let bytes = wrap(&request_bytes(&mut sender, now), PAYLOAD_TYPE, SOURCE);
    assert!(bytes.len() < small.datagram_bytes.get());
    let sent = Sent::default();
    let memory = Memory {
        inbound: VecDeque::from([(bytes, peer)]),
        sent: sent.clone(),
        blocked: Rc::new(Cell::new(false)),
    };
    let mut transport = source
        .attach_transport(handle, memory, peer, small)
        .unwrap();
    assert!(matches!(
        source.start_initialization(
            handle,
            &[voice_initialization()],
            ModeVersion::new(1).unwrap(),
            sender_settings()
        ),
        Err(Error::Capacity)
    ));
    let rejected = transport
        .receive(&mut source, now, anchor(), count(1))
        .unwrap();
    assert_eq!(
        (rejected.received, rejected.accepted, rejected.rejected),
        (1, 0, 1)
    );
    assert_eq!(transport.source(), None);
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    );
    assert!(
        transport
            .publish_initialization(&mut source, now, 9000)
            .unwrap()
    );
    let sent = sent.borrow();
    let nack = rtp::decode(&sent[0]).unwrap();
    assert_eq!(
        sender
            .acknowledgment(iuup::decode(nack.payload).unwrap(), now)
            .unwrap(),
        AckEffect::Retry
    );
    assert!(transport.retire(&mut source).unwrap());
}

#[test]
fn received_ack_initializes_original_sender_through_owned_transport() {
    use umts_user_plane::iuup::negotiation::Receiver;
    let f = fixture(true);
    let (mut source, handle) = prepare(&f);
    source.activate(handle, &f.bank).unwrap();
    let now = Instant::now();
    let mut reference = sender(0, &[81, 103, 60]);
    let request = request_bytes(&mut reference, now);
    let mut receiver = Receiver::new(Versions::new(3).unwrap()).unwrap();
    receiver.request(iuup::decode(&request).unwrap()).unwrap();
    let mut ack = Vec::new();
    receiver
        .publish(&mut [0; 64], |bytes| {
            ack.extend_from_slice(bytes);
            Ok::<_, ()>(())
        })
        .unwrap();
    source
        .start_initialization(
            handle,
            &[voice_initialization()],
            ModeVersion::new(1).unwrap(),
            sender_settings(),
        )
        .unwrap();
    let peer = ([127, 0, 0, 1], 20002).into();
    let sent = Sent::default();
    let memory = Memory {
        inbound: VecDeque::from([
            (wrap(&ack, PAYLOAD_TYPE, SOURCE), peer),
            (super::packet(0), peer),
        ]),
        sent: sent.clone(),
        blocked: Rc::new(Cell::new(false)),
    };
    let mut transport = source
        .attach_transport(handle, memory, peer, settings(None))
        .unwrap();
    assert!(
        transport
            .publish_initialization(&mut source, now, 9000)
            .unwrap()
    );
    assert_eq!(rtp::decode(&sent.borrow()[0]).unwrap().payload, request);
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Pending
    );
    let progress = transport
        .receive(&mut source, now, anchor(), count(2))
        .unwrap();
    assert_eq!((progress.received, progress.accepted), (2, 2));
    assert_eq!(transport.source(), Some(SOURCE));
    assert_eq!(
        source.initialization_status(handle).unwrap(),
        InitializationStatus::Ready
    );
    assert_eq!(source.queued(handle), Some(1));
    assert!(transport.retire(&mut source).unwrap());
}
