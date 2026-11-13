// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{SduDelivery, UpperSdu},
    dedicated_controller::DedicatedController,
    packet_bearers::PacketBearers,
};
use std::{collections::VecDeque, io, net::SocketAddr};
use umts_types::ConnectionKey;
use umts_user_plane::{
    gtpu::{self, MessageType, Teid},
    packet::{Datagrams, Limits, PacketPlane, RouteHandle, RouteKey},
};

#[derive(Default)]
pub(super) struct Memory {
    incoming: VecDeque<Vec<u8>>,
    outgoing: Vec<Vec<u8>>,
}
fn peer() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 2152))
}
impl Datagrams for Memory {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let Some(bytes) = self.incoming.pop_front() else {
            return Ok(None);
        };
        output[..bytes.len()].copy_from_slice(&bytes);
        Ok(Some((bytes.len(), peer())))
    }
    fn send(&mut self, bytes: &[u8], _: SocketAddr) -> io::Result<bool> {
        self.outgoing.push(bytes.to_vec());
        Ok(true)
    }
}
pub(super) fn prepare(
    identity: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
) -> (PacketBearers<Memory>, RouteHandle) {
    let plane = PacketPlane::new(
        Memory::default(),
        Limits {
            routes: count(2),
            packets_per_route: count(2),
            maximum_payload_bytes: count(1500),
            maximum_storage_bytes: 1_000_000,
            receive_budget: count(2),
            route_visits: count(2),
        },
    )
    .unwrap();
    let mut bridge = PacketBearers::new(plane);
    let key = RouteKey {
        connection: ConnectionKey {
            ue: identity.ue,
            domain: CnDomain::Ps,
        },
        rab: 5,
    };
    let route = bridge
        .prepare(
            key,
            identity,
            BearerId::new(20).unwrap(),
            plan,
            peer(),
            Teid::new(99).unwrap(),
        )
        .unwrap();
    (bridge, route)
}
pub(super) fn exercise(
    bridge: &mut PacketBearers<Memory>,
    route: RouteHandle,
    receive: &mut DedicatedController,
    identity: ReceiveIdentity,
) {
    let metadata = UpperSdu {
        identity,
        bearer: BearerId::new(20).unwrap(),
        scope: Scope::User(CnDomain::Ps),
        received: at(0),
        bits: 48,
        quality: umts_rlc::transparent::Quality::NotReported,
        coverage: None,
        transparent_origin: None,
    };
    for _ in 0..3 {
        assert!(
            !bridge
                .uplink(SduDelivery {
                    mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
                    metadata,
                    data: Bits::new(b"uplink", 48).unwrap(),
                })
                .unwrap()
        );
    }
    bridge.step(receive).unwrap();
    assert!(bridge.plane().io().outgoing.is_empty());
    bridge.activate(route, receive).unwrap();
    const REJECTED_SDU_BYTES: usize = 513;
    let mut rejected = vec![0; REJECTED_SDU_BYTES + gtpu::HEADER_BYTES];
    let length = gtpu::encode(
        gtpu::Packet {
            kind: MessageType::Data,
            teid: Some(route.local_teid().into()),
            sequence: None,
            n_pdu: None,
            payload: &[0xa5; REJECTED_SDU_BYTES],
        },
        &mut rejected,
    )
    .unwrap();
    rejected.truncate(length);
    bridge.io_mut().incoming.push_back(rejected);
    let mut bytes = vec![0; 128];
    let length = gtpu::encode(
        gtpu::Packet {
            kind: MessageType::Data,
            teid: Some(route.local_teid().into()),
            sequence: None,
            n_pdu: None,
            payload: b"downlink",
        },
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bridge.io_mut().incoming.push_back(bytes);
    let report = bridge.step(receive).unwrap();
    assert_eq!(report.transport.discarded, 1);
    assert_eq!(report.transport.delivered, 0);
    assert_eq!(report.radio_deferred, 0);
    assert_eq!(bridge.plane().counters().discarded_payload, 1);
    let report = bridge.step(receive).unwrap();
    assert_eq!(report.transport.delivered, 1);
    assert_eq!(report.radio_deferred, 0);
    assert!(
        bridge
            .uplink(SduDelivery {
                mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
                metadata,
                data: Bits::new(b"uplink", 48).unwrap()
            })
            .unwrap()
    );
    let mut erroneous = metadata;
    erroneous.quality = umts_rlc::transparent::Quality::Erroneous;
    assert!(
        bridge
            .uplink(SduDelivery {
                mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
                metadata: erroneous,
                data: Bits::new(b"uplink", 48).unwrap()
            })
            .is_err()
    );
    bridge.step(receive).unwrap();
    let outgoing = &bridge.plane().io().outgoing;
    assert_eq!(outgoing.len(), 1);
    let packet = gtpu::decode(&outgoing[0]).unwrap();
    assert_eq!(packet.teid, Some(gtpu::PeerTeid::new(99)));
    assert_eq!(packet.sequence, Some(0));
    assert_eq!(packet.payload, b"uplink");
    bridge.release(route).unwrap();
    assert!(
        bridge
            .uplink(SduDelivery {
                mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
                metadata,
                data: Bits::new(b"uplink", 48).unwrap(),
            })
            .is_err()
    );
    assert!(bridge.activate(route, receive).is_err());
}
