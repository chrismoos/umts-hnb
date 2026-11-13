// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::gtpu::PeerTeid;
use std::collections::VecDeque;
use umts_types::RadioEpoch;

const GTP_PORT: u16 = 2152;
const PAYLOAD_BYTES: usize = 64;

#[derive(Default)]
struct Memory {
    incoming: VecDeque<(Vec<u8>, SocketAddr)>,
    outgoing: Vec<(Vec<u8>, SocketAddr)>,
    attempts: Vec<(Vec<u8>, SocketAddr)>,
    blocked: bool,
    fail_once: bool,
}

impl Datagrams for Memory {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let Some((bytes, peer)) = self.incoming.pop_front() else {
            return Ok(None);
        };
        assert!(bytes.len() <= output.len());
        output[..bytes.len()].copy_from_slice(&bytes);
        Ok(Some((bytes.len(), peer)))
    }

    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool> {
        self.attempts.push((bytes.to_vec(), peer));
        if std::mem::take(&mut self.fail_once) {
            return Err(io::Error::other("injected transport failure"));
        }
        if self.blocked {
            return Ok(false);
        }
        self.outgoing.push((bytes.to_vec(), peer));
        Ok(true)
    }
}

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn limits() -> Limits {
    Limits {
        routes: count(2),
        packets_per_route: count(3),
        maximum_payload_bytes: count(PAYLOAD_BYTES),
        maximum_storage_bytes: 100_000,
        receive_budget: count(8),
        route_visits: count(2),
    }
}

fn key(generation: u64) -> RouteKey {
    RouteKey {
        connection: ConnectionKey {
            ue: UeKey {
                epoch: RadioEpoch::new(NonZeroU64::MIN),
                slot: 0,
                generation: NonZeroU64::new(generation).unwrap(),
            },
            domain: CnDomain::Ps,
        },
        rab: 5,
    }
}

fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], GTP_PORT))
}
fn replacement_peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 2], GTP_PORT + 1))
}

fn packet(teid: impl Into<PeerTeid>, sequence: Option<u16>, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; gtpu::HEADER_BYTES + gtpu::OPTIONAL_BYTES + payload.len()];
    let length = gtpu::encode(
        gtpu::Packet {
            kind: MessageType::Data,
            teid: Some(teid.into()),
            sequence,
            n_pdu: None,
            payload,
        },
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

fn plane(state: SequenceState) -> (PacketPlane<Memory>, RouteHandle) {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let handle = plane
        .prepare_with_state(key(1), peer(), PeerTeid::new(99), state)
        .unwrap();
    plane.activate(handle).unwrap();
    (plane, handle)
}

fn assert_state(
    plane: &mut PacketPlane<Memory>,
    handle: RouteHandle,
    uplink: u16,
    downlink: Option<u16>,
) {
    let state = plane.sequence_state(handle).unwrap();
    assert_eq!((state.next_uplink, state.next_downlink), (uplink, downlink));
}

#[test]
fn peer_zero_is_valid_data_while_local_zero_remains_unallocated_and_unknown() {
    assert_eq!(Teid::new(0), Err(WireError::Header));
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = plane.prepare(key(1), peer(), PeerTeid::new(0)).unwrap();
    assert_ne!(route.local_teid().get(), 0);
    plane.activate(route).unwrap();
    plane.enqueue_uplink(route, b"a").unwrap();
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    let (bytes, destination) = &plane.io().outgoing[0];
    assert_eq!(*destination, peer());
    assert_eq!(
        gtpu::decode(bytes).unwrap(),
        gtpu::Packet {
            kind: MessageType::Data,
            teid: Some(PeerTeid::new(0)),
            sequence: Some(0),
            n_pdu: None,
            payload: b"a",
        }
    );
    let literal = vec![0x30, 0xff, 0, 1, 0, 0, 0, 0, b'x'];
    assert_eq!(gtpu::decode(&literal).unwrap().teid, Some(PeerTeid::new(0)));
    plane.io_mut().incoming.push_back((literal, peer()));
    plane
        .step(|_, _| panic!("zero local TEID must not identify an allocated route"))
        .unwrap();
    assert_eq!(plane.counters().unknown_tunnel, 1);
    assert_eq!(plane.counters().malformed, 0);
}

#[test]
fn terminal_discard_advances_network_sequence_without_claiming_radio_delivery() {
    let (mut plane, route) = plane(SequenceState {
        next_uplink: 7,
        next_downlink: Some(u16::MAX),
    });
    plane.io_mut().incoming.push_back((
        packet(route.local_teid(), Some(u16::MAX), b"invalid"),
        peer(),
    ));
    plane
        .io_mut()
        .incoming
        .push_back((packet(route.local_teid(), Some(0), b"valid"), peer()));
    let report = plane
        .step(|_, payload| {
            assert_eq!(payload, b"invalid");
            DownlinkDisposition::Blocked
        })
        .unwrap();
    assert_eq!((report.delivered, report.discarded), (0, 0));
    assert_state(&mut plane, route, 7, Some(u16::MAX));
    let report = plane
        .step(|_, payload| {
            assert_eq!(payload, b"invalid");
            DownlinkDisposition::Discarded(DiscardReason::InvalidPayload)
        })
        .unwrap();
    assert_eq!((report.delivered, report.discarded), (0, 1));
    assert_state(&mut plane, route, 7, Some(0));
    let report = plane
        .step(|_, payload| {
            assert_eq!(payload, b"valid");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_eq!((report.delivered, report.discarded), (1, 0));
    assert_state(&mut plane, route, 7, Some(1));
    assert_eq!(plane.counters().blocked, 1);
    assert_eq!(plane.counters().discarded_payload, 1);
}

#[test]
fn seeded_sequences_wrap_only_on_accepted_send_and_downstream_delivery() {
    let (mut plane, route) = plane(SequenceState {
        next_uplink: u16::MAX,
        next_downlink: Some(100),
    });
    plane.enqueue_uplink(route, b"up").unwrap();
    plane
        .io_mut()
        .incoming
        .push_back((packet(route.local_teid(), Some(u16::MAX), b"down"), peer()));
    plane.io_mut().blocked = true;
    plane.step(|_, _| DownlinkDisposition::Blocked).unwrap();
    assert_state(&mut plane, route, u16::MAX, Some(100));
    plane.io_mut().blocked = false;
    plane.io_mut().fail_once = true;
    assert!(matches!(
        plane.step(|_, _| DownlinkDisposition::Blocked),
        Err(Error::Io(_))
    ));
    assert_state(&mut plane, route, u16::MAX, Some(100));
    plane
        .step(|handle, payload| {
            assert_eq!(handle, route);
            assert_eq!(payload, b"down");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_state(&mut plane, route, 0, Some(0));
    assert_eq!(plane.io().attempts.len(), 3);
    assert_eq!(plane.io().attempts[0], plane.io().attempts[1]);
    assert_eq!(plane.io().attempts[1], plane.io().attempts[2]);
    plane.enqueue_uplink(route, b"next").unwrap();
    plane
        .io_mut()
        .incoming
        .push_back((packet(route.local_teid(), Some(0), b"next down"), peer()));
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    assert_state(&mut plane, route, 1, Some(1));
    assert_eq!(
        gtpu::decode(&plane.io().outgoing[0].0).unwrap().sequence,
        Some(u16::MAX)
    );
    assert_eq!(
        gtpu::decode(&plane.io().outgoing[1].0).unwrap().sequence,
        Some(0)
    );
}

#[test]
fn unsequenced_downlink_retains_supplied_sequence_state_until_sequenced_delivery() {
    let (mut plane, route) = plane(SequenceState {
        next_uplink: 7,
        next_downlink: Some(1234),
    });
    plane
        .io_mut()
        .incoming
        .push_back((packet(route.local_teid(), None, b"unsequenced"), peer()));
    plane
        .step(|_, payload| {
            assert_eq!(payload, b"unsequenced");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_state(&mut plane, route, 7, Some(1234));
    plane
        .io_mut()
        .incoming
        .push_back((packet(route.local_teid(), Some(77), b"sequenced"), peer()));
    plane.step(|_, _| DownlinkDisposition::Blocked).unwrap();
    assert_state(&mut plane, route, 7, Some(1234));
    plane
        .step(|_, payload| {
            assert_eq!(payload, b"sequenced");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_state(&mut plane, route, 7, Some(78));
}

#[test]
fn active_update_preserves_queued_rx_tx_and_requires_exact_new_peer() {
    let (mut plane, route) = plane(SequenceState {
        next_uplink: 7,
        next_downlink: Some(100),
    });
    plane.enqueue_uplink(route, b"retained uplink").unwrap();
    plane.io_mut().incoming.push_back((
        packet(route.local_teid(), Some(u16::MAX), b"retained downlink"),
        peer(),
    ));
    plane.io_mut().blocked = true;
    plane.step(|_, _| DownlinkDisposition::Blocked).unwrap();
    plane
        .update(
            route,
            Update {
                peer: Some((replacement_peer(), PeerTeid::new(0))),
                next_uplink: Some(900),
                next_downlink: Some(77),
            },
        )
        .unwrap();
    assert_state(&mut plane, route, 900, Some(77));
    for wrong in [peer(), SocketAddr::from(([127, 0, 0, 2], GTP_PORT))] {
        plane
            .io_mut()
            .incoming
            .push_back((packet(route.local_teid(), Some(88), b"wrong peer"), wrong));
    }
    plane.io_mut().incoming.push_back((
        packet(route.local_teid(), Some(55), b"new downlink"),
        replacement_peer(),
    ));
    plane.io_mut().blocked = false;
    plane
        .step(|_, payload| {
            assert_eq!(payload, b"retained downlink");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_state(&mut plane, route, 901, Some(0));
    assert_eq!(plane.counters().wrong_peer, 2);
    let (bytes, destination) = &plane.io().outgoing[0];
    assert_eq!(*destination, replacement_peer());
    let sent = gtpu::decode(bytes).unwrap();
    assert_eq!(sent.teid, Some(PeerTeid::new(0)));
    assert_eq!(sent.sequence, Some(900));
    assert_eq!(sent.payload, b"retained uplink");
    plane
        .step(|_, payload| {
            assert_eq!(payload, b"new downlink");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_state(&mut plane, route, 901, Some(56));
}

#[test]
fn invalid_update_rolls_back_peer_and_both_sequences_without_losing_queues() {
    let (mut plane, route) = plane(SequenceState {
        next_uplink: 22,
        next_downlink: Some(33),
    });
    plane.enqueue_uplink(route, b"uplink").unwrap();
    plane
        .io_mut()
        .incoming
        .push_back((packet(route.local_teid(), Some(40), b"downlink"), peer()));
    plane.io_mut().blocked = true;
    plane.step(|_, _| DownlinkDisposition::Blocked).unwrap();
    for invalid in [
        SocketAddr::from(([127, 0, 0, 2], 0)),
        SocketAddr::from(([0, 0, 0, 0], GTP_PORT)),
    ] {
        assert!(matches!(
            plane.update(
                route,
                Update {
                    peer: Some((invalid, PeerTeid::new(123))),
                    next_uplink: Some(444),
                    next_downlink: Some(555),
                }
            ),
            Err(Error::Identity)
        ));
        assert_state(&mut plane, route, 22, Some(33));
    }
    plane.io_mut().blocked = false;
    plane
        .step(|_, payload| {
            assert_eq!(payload, b"downlink");
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_state(&mut plane, route, 23, Some(41));
    let (bytes, destination) = &plane.io().outgoing[0];
    assert_eq!(*destination, peer());
    let sent = gtpu::decode(bytes).unwrap();
    assert_eq!(sent.teid, Some(PeerTeid::new(99)));
    assert_eq!(sent.sequence, Some(22));
    assert_eq!(sent.payload, b"uplink");
}

#[test]
fn stale_and_foreign_updates_cannot_change_another_route() {
    let (mut plane, retired) = plane(SequenceState {
        next_uplink: 1,
        next_downlink: None,
    });
    plane.release(retired).unwrap();
    let active = plane
        .prepare_with_state(
            key(2),
            peer(),
            PeerTeid::new(88),
            SequenceState {
                next_uplink: 2,
                next_downlink: Some(3),
            },
        )
        .unwrap();
    plane.activate(active).unwrap();
    assert!(matches!(
        plane.update(
            retired,
            Update {
                next_uplink: Some(500),
                ..Update::default()
            }
        ),
        Err(Error::Identity)
    ));
    assert!(matches!(
        plane.sequence_state(retired),
        Err(Error::Identity)
    ));
    let mut foreign_plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let foreign = foreign_plane
        .prepare(key(2), peer(), PeerTeid::new(88))
        .unwrap();
    assert!(matches!(
        plane.update(
            foreign,
            Update {
                next_downlink: Some(500),
                ..Update::default()
            }
        ),
        Err(Error::Identity)
    ));
    assert_state(&mut plane, active, 2, Some(3));
}

#[test]
fn prepared_routes_reject_updates_without_changing_seeded_state() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = plane
        .prepare_with_state(
            key(1),
            peer(),
            PeerTeid::new(99),
            SequenceState {
                next_uplink: 12,
                next_downlink: Some(34),
            },
        )
        .unwrap();
    assert!(matches!(
        plane.update(
            route,
            Update {
                peer: Some((replacement_peer(), PeerTeid::new(0))),
                next_uplink: Some(99),
                next_downlink: Some(88),
            }
        ),
        Err(Error::Phase)
    ));
    assert_state(&mut plane, route, 12, Some(34));
    plane.activate(route).unwrap();
    plane.update(route, Update::default()).unwrap();
    assert_state(&mut plane, route, 12, Some(34));
    plane.enqueue_uplink(route, b"original peer").unwrap();
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    let (bytes, destination) = &plane.io().outgoing[0];
    assert_eq!(*destination, peer());
    let packet = gtpu::decode(bytes).unwrap();
    assert_eq!(packet.teid, Some(PeerTeid::new(99)));
    assert_eq!(packet.sequence, Some(12));
}
