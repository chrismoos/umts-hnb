// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{SduDelivery, UpperSdu},
    dedicated_controller::{DedicatedController, DedicatedControllerLimits},
    packet_bearers::{NetworkBinding, PacketBearers},
};
use std::{io, net::SocketAddr};
use umts_user_plane::{
    gtpu::{self, PeerTeid},
    packet::{Datagrams, Limits, PacketPlane, RouteHandle, RouteKey, SequenceState},
};

#[derive(Default)]
struct Memory {
    outgoing: Vec<Vec<u8>>,
}
impl Datagrams for Memory {
    fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        Ok(None)
    }
    fn send(&mut self, bytes: &[u8], _: SocketAddr) -> io::Result<bool> {
        self.outgoing.push(bytes.to_vec());
        Ok(true)
    }
}
fn packets(f: &Fixture) -> (PacketBearers<Memory>, RouteHandle) {
    let plane = PacketPlane::new(
        Memory::default(),
        Limits {
            routes: count(4),
            packets_per_route: count(4),
            maximum_payload_bytes: count(1500),
            maximum_storage_bytes: 1_000_000,
            receive_budget: count(4),
            route_visits: count(4),
        },
    )
    .unwrap();
    let mut packets = PacketBearers::new(plane);
    let route = packets
        .prepare_bound(
            RouteKey {
                connection: umts_types::ConnectionKey {
                    ue: f.key,
                    domain: CnDomain::Ps,
                },
                rab: 5,
            },
            f.bank.identity(),
            BearerId::new(20).unwrap(),
            f.plan.clone(),
            NetworkBinding {
                peer: SocketAddr::from(([127, 0, 0, 1], 2152)),
                peer_teid: PeerTeid::new(99),
                sequence: SequenceState {
                    next_uplink: 17,
                    next_downlink: Some(23),
                },
            },
        )
        .unwrap();
    packets.activate_on_bank(route, &f.bank).unwrap();
    (packets, route)
}
fn uplink(packets: &mut PacketBearers<Memory>, identity: ReceiveIdentity, bytes: &[u8]) -> bool {
    packets
        .uplink(SduDelivery {
            mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
            metadata: UpperSdu {
                identity,
                bearer: BearerId::new(20).unwrap(),
                scope: Scope::User(CnDomain::Ps),
                received: at(0),
                bits: bytes.len() * BITS_PER_BYTE,
                quality: umts_rlc::transparent::Quality::NotReported,
                coverage: None,
                transparent_origin: None,
            },
            data: Bits::new(bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
        })
        .unwrap()
}

#[test]
fn packet_handoff_drop_and_commit_preserve_exact_teid_queue_and_sequence_then_reclaim_alias() {
    let mut f = fixture(true);
    let previous = f.bank.identity();
    let next = next_identity(&f);
    let plan = subset(&f, true, false);
    let (mut packets, route) = packets(&f);
    let other_ue = umts_types::UeKey {
        slot: f.key.slot + 1,
        ..f.key
    };
    let other = packets
        .prepare_bound(
            RouteKey {
                connection: umts_types::ConnectionKey {
                    ue: other_ue,
                    domain: CnDomain::Ps,
                },
                rab: 5,
            },
            ReceiveIdentity {
                ue: other_ue,
                configuration: NonZeroU64::MIN,
            },
            BearerId::new(20).unwrap(),
            f.plan.clone(),
            NetworkBinding {
                peer: SocketAddr::from(([127, 0, 0, 1], 2152)),
                peer_teid: PeerTeid::new(100),
                sequence: SequenceState {
                    next_uplink: 31,
                    next_downlink: Some(41),
                },
            },
        )
        .unwrap();
    let other_sequence = packets.sequence_state(other).unwrap();
    assert!(uplink(&mut packets, previous, b"before"));
    let before = packets.sequence_state(route).unwrap();
    let token = accepted(&mut f, next, &plan);
    let (receive, transmit, _writer) = retire_workers(&mut f, next, &plan);
    let guard = packets
        .prepare_release_handoff(&token, plan.clone(), &f.bank)
        .unwrap();
    assert_eq!(guard.retained(), [route]);
    assert!(guard.removed().is_empty());
    drop(guard);
    assert_eq!(packets.sequence_state(route).unwrap(), before);
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &receive,
                &transmit,
                ReleaseOutputPolicy::DiscardReleased,
                ciphers,
                |_, bank| {
                    let prepared = packets.prepare_release_handoff(&token, plan.clone(), bank)?;
                    Ok::<_, crate::packet_bearers::Error>(prepared.commit())
                },
            )
        })
        .unwrap();
    assert_eq!(packets.sequence_state(route).unwrap(), before);
    assert!(uplink(&mut packets, previous, b"old path"));
    assert!(uplink(&mut packets, next, b"new path"));
    let (_runtime, port) = receive_runtime(f.key.epoch);
    let mut controller = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 4_000_000,
        },
    );
    let mut sent_packets = 0;
    for _ in 0..3 {
        let report = packets.step(&mut controller).unwrap();
        assert_eq!(report.aliases_retired, 0);
        assert!(report.transport.visited <= 4);
        sent_packets += report.transport.sent;
    }
    assert_eq!(sent_packets, 3);
    assert_eq!(packets.step(&mut controller).unwrap().transport.sent, 0);
    let sent: Vec<_> = packets
        .plane()
        .io()
        .outgoing
        .iter()
        .map(|bytes| gtpu::decode(bytes).unwrap())
        .collect();
    assert_eq!(
        sent.iter()
            .map(|packet| packet.sequence)
            .collect::<Vec<_>>(),
        [Some(17), Some(18), Some(19)]
    );
    assert!(
        sent.iter()
            .all(|packet| packet.teid == Some(PeerTeid::new(99)))
    );
    assert_eq!(
        sent.iter().map(|packet| packet.payload).collect::<Vec<_>>(),
        [
            b"before".as_slice(),
            b"old path".as_slice(),
            b"new path".as_slice()
        ]
    );
    assert!(packets.retire_handoff_alias(route, &f.bank).unwrap());
    assert!(!packets.retire_handoff_alias(route, &f.bank).unwrap());
    let metadata = UpperSdu {
        identity: previous,
        bearer: BearerId::new(20).unwrap(),
        scope: Scope::User(CnDomain::Ps),
        received: at(0),
        bits: BITS_PER_BYTE,
        quality: umts_rlc::transparent::Quality::NotReported,
        coverage: None,
        transparent_origin: None,
    };
    assert!(
        packets
            .uplink(SduDelivery {
                mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
                metadata,
                data: Bits::new(&[0x42], BITS_PER_BYTE).unwrap()
            })
            .is_err()
    );
    assert!(uplink(&mut packets, next, b"still live"));
    assert_eq!(packets.sequence_state(other).unwrap(), other_sequence);
}

#[test]
fn old_am_output_and_partial_reassembly_keep_alias_without_relabeling_the_sdu() {
    let mut f = fixture(true);
    let previous = f.bank.identity();
    let old_plan = f.plan.clone();
    let next = next_identity(&f);
    let plan = subset(&f, true, false);
    let (mut packets, route) = packets(&f);
    let mut sender = receiver();
    sender.enqueue(b"old output").unwrap();
    sender.enqueue(&[0x63; 400]).unwrap();
    enqueue_packet(&mut f, &old_plan, previous, 4, 2, &mut sender).unwrap();
    service(&mut f, 4);
    let token = accepted(&mut f, next, &plan);
    let (receive, transmit, _writer) = retire_workers(&mut f, next, &plan);
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &receive,
                &transmit,
                ReleaseOutputPolicy::DiscardReleased,
                ciphers,
                |_, bank| {
                    Ok::<_, crate::packet_bearers::Error>(
                        packets
                            .prepare_release_handoff(&token, plan.clone(), bank)?
                            .commit(),
                    )
                },
            )
        })
        .unwrap();
    assert!(!packets.retire_handoff_alias(route, &f.bank).unwrap());
    let mut delivered = false;
    assert!(f.bank.process_one(|delivery| {
        assert_eq!(delivery.metadata.identity, previous);
        assert_eq!(
            delivery.metadata.coverage.unwrap().provenance(),
            ReceiveProvenance::Path(previous.configuration)
        );
        assert!(packets.uplink(delivery).unwrap());
        delivered = true;
        true
    }));
    assert!(delivered);
    assert!(!packets.retire_handoff_alias(route, &f.bank).unwrap());
}

#[test]
fn successor_partial_packet_does_not_block_retirement_of_the_previous_alias() {
    let mut f = fixture(true);
    let next = next_identity(&f);
    let plan = subset(&f, true, false);
    let (mut packets, route) = packets(&f);
    let token = accepted(&mut f, next, &plan);
    let (receive, transmit, _writer) = retire_workers(&mut f, next, &plan);
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &receive,
                &transmit,
                ReleaseOutputPolicy::DiscardReleased,
                ciphers,
                |_, bank| {
                    Ok::<_, crate::packet_bearers::Error>(
                        packets
                            .prepare_release_handoff(&token, plan.clone(), bank)?
                            .commit(),
                    )
                },
            )
        })
        .unwrap();
    let mut sender = receiver();
    sender.enqueue(&[0x63; 400]).unwrap();
    enqueue_packet(&mut f, &plan, next, 4, 4, &mut sender).unwrap();
    service(&mut f, 6);
    assert!(
        !f.bank
            .process_one(|_| panic!("partial packet must not be delivered"))
    );
    assert!(packets.retire_handoff_alias(route, &f.bank).unwrap());
    assert!(!packets.retire_handoff_alias(route, &f.bank).unwrap());
    sender.enqueue(&[0x74; 400]).unwrap();
    let mut delivered = false;
    for frame in (6..38).step_by(2) {
        enqueue_packet(&mut f, &plan, next, 4, frame, &mut sender).unwrap();
        service(&mut f, frame + 2);
        f.bank.process_one(|delivery| {
            assert_eq!(delivery.metadata.identity, next);
            assert_eq!(delivery.metadata.bits, 400 * BITS_PER_BYTE);
            assert_eq!(
                delivery.metadata.coverage.unwrap().provenance(),
                ReceiveProvenance::Path(next.configuration)
            );
            for bit in 0..400 * BITS_PER_BYTE {
                assert_eq!(
                    delivery.data.bit(bit).unwrap(),
                    0x63 & (1 << (7 - bit % 8)) != 0
                );
            }
            assert!(packets.uplink(delivery).unwrap());
            delivered = true;
            true
        });
        if delivered {
            break;
        }
    }
    assert!(
        delivered,
        "successor packet survives predecessor retirement"
    );
}

#[test]
fn removed_packet_route_drops_only_its_original_queue_at_atomic_commit() {
    let mut f = fixture(true);
    let previous = f.bank.identity();
    let next = next_identity(&f);
    let plan = subset(&f, false, true);
    let (mut packets, route) = packets(&f);
    assert!(uplink(&mut packets, previous, b"retire me"));
    let token = accepted(&mut f, next, &plan);
    let (receive, transmit, _writer) = retire_workers(&mut f, next, &plan);
    let guard = packets
        .prepare_release_handoff(&token, plan.clone(), &f.bank)
        .unwrap();
    assert!(guard.retained().is_empty());
    assert_eq!(guard.removed(), [route]);
    drop(guard);
    assert!(packets.sequence_state(route).is_ok());
    f.radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with_view(
                &token,
                &receive,
                &transmit,
                ReleaseOutputPolicy::DiscardReleased,
                ciphers,
                |_, bank| {
                    Ok::<_, crate::packet_bearers::Error>(
                        packets
                            .prepare_release_handoff(&token, plan.clone(), bank)?
                            .commit(),
                    )
                },
            )
        })
        .unwrap();
    assert!(packets.sequence_state(route).is_err());
    assert_eq!(packets.plane().counters().retired_packets, 1);
}
