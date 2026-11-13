// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::collections::VecDeque;
use umts_types::RadioEpoch;

mod release_batch;

#[derive(Default)]
struct Memory {
    incoming: VecDeque<(Vec<u8>, SocketAddr)>,
    outgoing: Vec<(Vec<u8>, SocketAddr)>,
    block: bool,
}
impl Datagrams for Memory {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let Some((bytes, peer)) = self.incoming.pop_front() else {
            return Ok(None);
        };
        let length = bytes.len().min(output.len());
        output[..length].copy_from_slice(&bytes[..length]);
        Ok(Some((length, peer)))
    }
    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool> {
        if self.block {
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
        routes: count(8),
        packets_per_route: count(2),
        maximum_payload_bytes: count(1500),
        maximum_storage_bytes: 1_000_000,
        receive_budget: count(4),
        route_visits: count(8),
    }
}
fn key(slot: u32) -> RouteKey {
    RouteKey {
        connection: ConnectionKey {
            ue: UeKey {
                epoch: RadioEpoch::new(NonZeroU64::MIN),
                slot,
                generation: NonZeroU64::MIN,
            },
            domain: CnDomain::Ps,
        },
        rab: 5,
    }
}
fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 2152))
}
fn data(teid: Teid, payload: &[u8]) -> Vec<u8> {
    let mut output = vec![0; payload.len() + gtpu::HEADER_BYTES];
    let size = gtpu::encode(
        gtpu::Packet {
            kind: MessageType::Data,
            teid: Some(teid.into()),
            sequence: None,
            n_pdu: None,
            payload,
        },
        &mut output,
    )
    .unwrap();
    output.truncate(size);
    output
}
#[test]
fn prepared_tunnels_drop_early_data_and_released_handles_cannot_alias_a_new_generation() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let first = plane
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    plane
        .io
        .incoming
        .push_back((data(first.local_teid(), b"early"), peer()));
    plane
        .step(|_, _| panic!("prepared tunnel must not deliver"))
        .unwrap();
    assert_eq!(plane.counters().before_activation, 1);
    plane.activate(first).unwrap();
    plane.enqueue_uplink(first, b"retired").unwrap();
    plane.release(first).unwrap();
    assert_eq!(plane.counters().retired_packets, 1);
    let mut next = key(0);
    next.connection.ue.generation = NonZeroU64::new(2).unwrap();
    let second = plane
        .prepare(next, peer(), Teid::new(100).unwrap())
        .unwrap();
    assert_ne!(first.local_teid(), second.local_teid());
    assert!(matches!(plane.activate(first), Err(Error::Identity)));
    plane
        .io
        .incoming
        .push_back((data(first.local_teid(), b"stale"), peer()));
    plane
        .step(|_, _| panic!("retired TEID must not deliver"))
        .unwrap();
    assert_eq!(plane.counters().unknown_tunnel, 1);
    // TS 29.060 7.3.7: the stale G-PDU is answered with an Error Indication naming its TEID.
    assert_eq!(plane.counters().error_indications_sent, 1);
    let (bytes, to) = plane.io.outgoing.last().unwrap();
    assert_eq!(*to, peer());
    let packet = gtpu::decode(bytes).unwrap();
    assert_eq!(packet.kind, MessageType::ErrorIndication);
    assert_eq!(
        gtpu::parse_error_indication(packet.payload).unwrap().teid,
        first.local_teid().get()
    );
}

#[test]
fn error_indication_from_the_peer_marks_only_the_named_tunnel_failed() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let first = plane
        .prepare(key(0), peer(), Teid::new(10).unwrap())
        .unwrap();
    let second = plane
        .prepare(key(1), peer(), Teid::new(11).unwrap())
        .unwrap();
    plane.activate(first).unwrap();
    plane.activate(second).unwrap();
    let mut indication = [0_u8; 64];
    let size = gtpu::error_indication(
        10,
        std::net::IpAddr::from([10, 0, 0, 1]),
        1,
        &mut indication,
    )
    .unwrap();
    plane
        .io
        .incoming
        .push_back((indication[..size].to_vec(), peer()));
    plane.step(|_, _| panic!("no data")).unwrap();
    assert_eq!(plane.counters().error_indications_received, 1);
    let mut failed = Vec::new();
    plane.peer_failed_routes(|handle| failed.push(handle));
    assert_eq!(failed, vec![first]);
    let mut deferred = Vec::new();
    plane.take_peer_failures(|handle| {
        deferred.push(handle);
        false
    });
    assert_eq!(deferred, vec![first]);
    let mut taken = Vec::new();
    plane.take_peer_failures(|handle| {
        taken.push(handle);
        true
    });
    plane.take_peer_failures(|handle| {
        taken.push(handle);
        true
    });
    assert_eq!(taken, vec![first]);
}

#[test]
fn one_backpressured_ue_preserves_its_order_without_blocking_another_ue() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let first = plane
        .prepare(key(0), peer(), Teid::new(10).unwrap())
        .unwrap();
    let second = plane
        .prepare(key(1), peer(), Teid::new(11).unwrap())
        .unwrap();
    plane.activate(first).unwrap();
    plane.activate(second).unwrap();
    for (handle, bytes) in [(first, b"a"), (first, b"b"), (second, b"x")] {
        plane
            .io
            .incoming
            .push_back((data(handle.local_teid(), bytes), peer()));
    }
    let mut received = Vec::new();
    let report = plane
        .step(|handle, payload| {
            if handle == first {
                return DownlinkDisposition::Blocked;
            }
            received.push(payload.to_vec());
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_eq!(report.received, 3);
    assert_eq!(report.delivered, 1);
    assert_eq!(received, [b"x".to_vec()]);
    for expected in [b"a", b"b"] {
        plane
            .step(|handle, payload| {
                assert_eq!(handle, first);
                assert_eq!(payload, expected);
                DownlinkDisposition::Accepted
            })
            .unwrap();
    }
    assert_eq!(plane.counters().blocked, 1);
}

#[test]
fn permanent_rejection_retires_only_that_packet_and_does_not_block_another_ue() {
    for reason in [
        DiscardReason::InvalidPayload,
        DiscardReason::InvalidRadioBinding,
    ] {
        let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
        let first = plane.prepare(key(0), peer(), PeerTeid::new(10)).unwrap();
        let second = plane.prepare(key(1), peer(), PeerTeid::new(11)).unwrap();
        plane.activate(first).unwrap();
        plane.activate(second).unwrap();
        for (handle, bytes) in [
            (first, b"bad".as_slice()),
            (first, b"next"),
            (second, b"peer"),
        ] {
            plane
                .io
                .incoming
                .push_back((data(handle.local_teid(), bytes), peer()));
        }
        let report = plane
            .step(|handle, payload| {
                if handle == first {
                    assert_eq!(payload, b"bad");
                    DownlinkDisposition::Discarded(reason)
                } else {
                    assert_eq!(handle, second);
                    assert_eq!(payload, b"peer");
                    DownlinkDisposition::Accepted
                }
            })
            .unwrap();
        assert_eq!(
            (report.received, report.delivered, report.discarded),
            (3, 1, 1)
        );
        let report = plane
            .step(|handle, payload| {
                assert_eq!(handle, first);
                assert_eq!(payload, b"next");
                DownlinkDisposition::Accepted
            })
            .unwrap();
        assert_eq!((report.delivered, report.discarded), (1, 0));
        let counters = plane.counters();
        assert_eq!((counters.delivered, counters.blocked), (2, 0));
        assert_eq!(
            counters.discarded_payload,
            u64::from(reason == DiscardReason::InvalidPayload)
        );
        assert_eq!(
            counters.discarded_binding,
            u64::from(reason == DiscardReason::InvalidRadioBinding)
        );
        plane
            .step(|_, _| panic!("all terminal packets were consumed"))
            .unwrap();
    }
}

#[test]
fn send_backpressure_retains_packet_and_sequence_while_wrong_peer_is_rejected() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = plane
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    plane.activate(route).unwrap();
    plane.enqueue_uplink(route, b"first").unwrap();
    plane.io.block = true;
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    assert!(plane.io.outgoing.is_empty());
    plane.io.block = false;
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    plane.enqueue_uplink(route, b"second").unwrap();
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    for (index, (bytes, destination)) in plane.io.outgoing.iter().enumerate() {
        let packet = gtpu::decode(bytes).unwrap();
        assert_eq!(packet.teid, Some(PeerTeid::new(99)));
        assert_eq!(packet.sequence, Some(index as u16));
        assert_eq!(*destination, peer());
    }
    plane.io.incoming.push_back((
        data(route.local_teid(), b"forged"),
        SocketAddr::from(([127, 0, 0, 2], 2152)),
    ));
    plane.step(|_, _| panic!("wrong peer")).unwrap();
    assert_eq!(plane.counters().wrong_peer, 1);
}

#[test]
fn queues_receive_work_and_registration_are_bounded_before_allocation() {
    let mut invalid = limits();
    invalid.maximum_storage_bytes = 1;
    assert!(matches!(
        PacketPlane::new(Memory::default(), invalid),
        Err(Error::Capacity)
    ));
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = plane
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    plane.activate(route).unwrap();
    for _ in 0..10 {
        plane
            .io
            .incoming
            .push_back((data(route.local_teid(), b"full"), peer()));
    }
    let report = plane.step(|_, _| DownlinkDisposition::Blocked).unwrap();
    assert_eq!(report.received, limits().receive_budget.get());
    assert_eq!(plane.counters().receive_overflow, 2);
    assert_eq!(plane.io.incoming.len(), 6);
    for _ in 0..2 {
        plane.enqueue_uplink(route, b"full").unwrap();
    }
    assert!(matches!(
        plane.enqueue_uplink(route, b"overflow"),
        Err(Error::Capacity)
    ));
    assert_eq!(plane.release_ue(key(0).connection.ue), 1);
}

#[test]
fn receive_burst_reclaims_full_queue_in_fifo_order_without_exceeding_work_budget() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let route = plane
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    plane.activate(route).unwrap();
    for payload in [b"a", b"b", b"c", b"d", b"e"] {
        plane
            .io
            .incoming
            .push_back((data(route.local_teid(), payload), peer()));
    }
    let mut received = Vec::new();
    let first = plane
        .step(|handle, payload| {
            assert_eq!(handle, route);
            received.push(payload.to_vec());
            DownlinkDisposition::Accepted
        })
        .unwrap();
    assert_eq!(first.received, limits().receive_budget.get());
    assert_eq!(first.delivered, 3);
    assert_eq!(plane.io.incoming.len(), 1);
    assert_eq!(plane.counters().receive_overflow, 0);
    for _ in 0..2 {
        plane
            .step(|_, payload| {
                received.push(payload.to_vec());
                DownlinkDisposition::Accepted
            })
            .unwrap();
    }
    assert_eq!(
        received,
        [b"a", b"b", b"c", b"d", b"e"].map(|bytes| bytes.to_vec())
    );
    assert_eq!(plane.counters().delivered, 5);
}

#[test]
fn full_blocked_route_cannot_bypass_its_head_or_starve_a_peer() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let blocked = plane
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    let ready = plane
        .prepare(key(1), peer(), Teid::new(100).unwrap())
        .unwrap();
    plane.activate(blocked).unwrap();
    plane.activate(ready).unwrap();
    for (route, payload) in [
        (blocked, b"a"),
        (blocked, b"b"),
        (blocked, b"c"),
        (ready, b"x"),
    ] {
        plane
            .io
            .incoming
            .push_back((data(route.local_teid(), payload), peer()));
    }
    let mut blocked_calls = 0;
    let report = plane
        .step(|handle, payload| {
            if handle == blocked {
                blocked_calls += 1;
                assert_eq!(payload, b"a");
                DownlinkDisposition::Blocked
            } else {
                assert_eq!(handle, ready);
                assert_eq!(payload, b"x");
                DownlinkDisposition::Accepted
            }
        })
        .unwrap();
    assert_eq!(blocked_calls, 2);
    assert_eq!(report.delivered, 1);
    assert_eq!(plane.counters().receive_overflow, 1);
    for expected in [b"a", b"b"] {
        plane
            .step(|handle, payload| {
                assert_eq!(handle, blocked);
                assert_eq!(payload, expected);
                DownlinkDisposition::Accepted
            })
            .unwrap();
    }
}

#[test]
fn a_handle_from_another_plane_cannot_authorize_an_identical_tunnel() {
    let mut first = PacketPlane::new(Memory::default(), limits()).unwrap();
    let mut second = PacketPlane::new(Memory::default(), limits()).unwrap();
    second.next_teid = first.next_teid;
    let foreign = first
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    let local = second
        .prepare(key(0), peer(), Teid::new(99).unwrap())
        .unwrap();
    assert_eq!(foreign.local_teid(), local.local_teid());
    assert!(first.owns_handle(foreign));
    assert!(!second.owns_handle(foreign));
    assert!(second.owns_handle(local));
    assert!(matches!(second.activate(foreign), Err(Error::Identity)));
    assert_eq!(second.phase(local).unwrap(), Phase::Prepared);
}

#[test]
fn echo_is_answered_with_the_request_sequence_and_zero_recovery_without_a_route() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    plane
        .io
        .incoming
        .push_back((vec![0x32, 1, 0, 4, 0, 0, 0, 0, 0x12, 0x34, 0, 0], peer()));
    plane.step(|_, _| panic!("not user data")).unwrap();
    let (bytes, destination) = &plane.io.outgoing[0];
    assert_eq!(*destination, peer());
    let reply = gtpu::decode(bytes).unwrap();
    assert_eq!(reply.kind, MessageType::EchoResponse);
    assert_eq!(reply.sequence, Some(0x1234));
    assert_eq!(reply.payload, &[14, 0]);
}

#[test]
fn actual_nonblocking_udp_carries_both_tunnel_directions() {
    let endpoint = UdpEndpoint::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let local = endpoint.local_address().unwrap();
    let remote = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    remote
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let mut plane = PacketPlane::new(endpoint, limits()).unwrap();
    let route = plane
        .prepare(key(0), remote.local_addr().unwrap(), Teid::new(99).unwrap())
        .unwrap();
    plane.activate(route).unwrap();
    remote
        .send_to(&data(route.local_teid(), b"downlink"), local)
        .unwrap();
    let mut delivered = false;
    for _ in 0..100 {
        plane
            .step(|handle, payload| {
                assert_eq!(handle, route);
                assert_eq!(payload, b"downlink");
                delivered = true;
                DownlinkDisposition::Accepted
            })
            .unwrap();
        if delivered {
            break;
        }
        std::thread::yield_now();
    }
    assert!(delivered);
    plane.enqueue_uplink(route, b"uplink").unwrap();
    plane.step(|_, _| DownlinkDisposition::Accepted).unwrap();
    let mut output = [0; 128];
    let (length, from) = remote.recv_from(&mut output).unwrap();
    assert_eq!(from, local);
    let packet = gtpu::decode(&output[..length]).unwrap();
    assert_eq!(packet.teid, Some(PeerTeid::new(99)));
    assert_eq!(packet.payload, b"uplink");
}

#[test]
fn path_supervision_sends_echo_requests_and_fails_the_path_after_unanswered_attempts() {
    use std::time::Duration;
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let first = plane
        .prepare(key(0), peer(), Teid::new(10).unwrap())
        .unwrap();
    plane.activate(first).unwrap();
    plane.set_test_time(Duration::from_secs(0));
    plane.step(|_, _| panic!("no data")).unwrap();
    assert_eq!(plane.counters().echo_requests_sent, 0);
    plane.set_test_time(Duration::from_secs(61));
    plane.step(|_, _| panic!("no data")).unwrap();
    assert_eq!(plane.counters().echo_requests_sent, 1);
    let (bytes, to) = plane.io.outgoing.last().unwrap().clone();
    assert_eq!(to, peer());
    let request = gtpu::decode(&bytes).unwrap();
    assert_eq!(request.kind, MessageType::EchoRequest);
    // An Echo Response with the same sequence clears the outstanding request.
    let mut response = vec![0; 32];
    let size = gtpu::echo_response(request.sequence.unwrap(), &mut response).unwrap();
    plane
        .io
        .incoming
        .push_back((response[..size].to_vec(), peer()));
    plane.set_test_time(Duration::from_secs(62));
    plane.step(|_, _| panic!("no data")).unwrap();
    plane.set_test_time(Duration::from_secs(70));
    plane.step(|_, _| panic!("no data")).unwrap();
    assert_eq!(plane.counters().echo_requests_sent, 1);
    let mut failed = Vec::new();
    plane.peer_failed_routes(|handle| failed.push(handle));
    assert!(failed.is_empty());
    // Five unanswered attempts fail the path and the routes on it.
    let mut now = 122;
    plane.set_test_time(Duration::from_secs(now));
    plane.step(|_, _| panic!("no data")).unwrap();
    for _ in 0..5 {
        now += 4;
        plane.set_test_time(Duration::from_secs(now));
        plane.step(|_, _| panic!("no data")).unwrap();
    }
    assert_eq!(plane.counters().echo_requests_sent, 6);
    assert_eq!(plane.counters().path_failures, 1);
    plane.peer_failed_routes(|handle| failed.push(handle));
    assert_eq!(failed, vec![first]);
}

#[test]
fn released_route_reports_queued_and_discarded_downlink_octets() {
    let mut plane = PacketPlane::new(Memory::default(), limits()).unwrap();
    let first = plane
        .prepare(key(0), peer(), Teid::new(10).unwrap())
        .unwrap();
    plane.activate(first).unwrap();
    plane
        .io
        .incoming
        .push_back((data(first.local_teid(), b"dropped"), peer()));
    plane
        .io
        .incoming
        .push_back((data(first.local_teid(), b"queued!!"), peer()));
    let mut calls = 0;
    plane
        .step(|_, _| {
            calls += 1;
            if calls == 1 {
                DownlinkDisposition::Discarded(DiscardReason::InvalidPayload)
            } else {
                DownlinkDisposition::Blocked
            }
        })
        .unwrap();
    plane.release(first).unwrap();
    let mut volumes = Vec::new();
    plane.drain_released_volumes(|route, volume| volumes.push((route, volume)));
    assert_eq!(volumes, vec![(first.key(), 15)]);
    plane.drain_released_volumes(|_, _| panic!("drained once"));
}
