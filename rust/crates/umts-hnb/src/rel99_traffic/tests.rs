// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{BearerId, UpperSdu},
    dedicated_rx::ReceiveIdentity,
    packet_bearers::PacketBearers,
    rel99_plan::CompiledRel99,
    test_support::{at, count, rel99},
    voice_bearers::{UplinkPolicy, VoiceHandle, VoiceKey},
};
use std::{
    io,
    net::SocketAddr,
    num::{NonZeroU8, NonZeroU16, NonZeroU64},
    sync::Arc,
};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_rlc::transparent::Quality;
use umts_types::{ConnectionKey, UeKey, bits::Bits, time::CHIPS_PER_FRAME};
use umts_user_plane::{
    iuup::initialization::Versions,
    packet::{Datagrams, Limits as PacketLimits, PacketPlane},
    uplink_media::{Delivery, RtpSource},
};

struct MemoryBoundDatagrams {
    marker: u64,
    receives: usize,
    sends: usize,
}
impl Datagrams for MemoryBoundDatagrams {
    fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        self.receives += 1;
        Ok(None)
    }
    fn send(&mut self, _: &[u8], _: SocketAddr) -> io::Result<bool> {
        self.sends += 1;
        Ok(true)
    }
}
impl BoundDatagrams for MemoryBoundDatagrams {
    fn local_address(&self) -> io::Result<SocketAddr> {
        Ok("127.0.0.1:2152".parse().unwrap())
    }
}

fn owners() -> (VoiceTraffic, PacketRabs<MemoryBoundDatagrams>) {
    let voices = VoiceTraffic::new(voice_bearers::Limits {
        routes: count(1),
        frames_per_route: count(2),
        maximum_payload_bytes: count(64),
        maximum_storage_bytes: 64 * 1024,
    })
    .unwrap();
    let plane = PacketPlane::new(
        MemoryBoundDatagrams {
            marker: 73,
            receives: 0,
            sends: 0,
        },
        PacketLimits {
            routes: count(1),
            packets_per_route: count(2),
            maximum_payload_bytes: count(128),
            maximum_storage_bytes: 128 * 1024,
            receive_budget: count(1),
            route_visits: count(1),
        },
    )
    .unwrap();
    let packets = PacketRabs::new(PacketBearers::new(plane), None, NonZeroU16::MIN).unwrap();
    (voices, packets)
}

fn identity() -> ReceiveIdentity {
    ReceiveIdentity {
        ue: UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::MIN,
    }
}

fn prepared_voice(voices: &mut VoiceTraffic) -> VoiceHandle {
    let plan = Arc::new(
        CompiledRel99::new(
            rel99::configuration(true, false, 10),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    );
    voices
        .prepare(
            VoiceKey {
                connection: ConnectionKey {
                    ue: identity().ue,
                    domain: CnDomain::Cs,
                },
                rab: 1,
            },
            identity(),
            plan,
            &[10, 11, 12].map(|id| BearerId::new(id).unwrap()),
            Versions::new(3).unwrap(),
            UplinkPolicy {
                delivery: vec![Delivery::No, Delivery::Yes, Delivery::NoDetect].into_boxed_slice(),
                interval_chips: NonZeroU64::new(2 * CHIPS_PER_FRAME).unwrap(),
                wait_chips: NonZeroU64::new(4 * CHIPS_PER_FRAME).unwrap(),
                timestamp: 9000,
                rtp: RtpSource {
                    sequence: 20,
                    ssrc: 77,
                    payload_type: 96,
                },
            },
        )
        .unwrap()
}

fn delivery(scope: Scope, metadata_bits: usize) -> SduDelivery<'static> {
    SduDelivery {
        mapping: crate::dedicated_bearers::DeliveryMapping::Committed,
        metadata: UpperSdu {
            identity: identity(),
            bearer: BearerId::new(5).unwrap(),
            scope,
            received: at(0),
            bits: metadata_bits,
            quality: Quality::Correct,
            coverage: None,
            transparent_origin: None,
        },
        data: Bits::new(&[0], 8).unwrap(),
    }
}

#[test]
fn construction_and_parts_preserve_the_original_prepared_voice_and_packet_owners() {
    let (mut voices, packets) = owners();
    let handle = prepared_voice(&mut voices);
    let reserved = voices.reserved_bytes();
    assert!(reserved > 0);
    let mut traffic = Rel99Traffic::new(voices, packets, count(4));
    assert_eq!(traffic.voices_mut().reserved_bytes(), reserved);
    assert_eq!(traffic.packets_mut().io_mut().marker, 73);
    traffic.packets_mut().io_mut().marker = 91;
    let (mut voices, mut packets) = traffic.into_parts();
    assert_eq!(voices.reserved_bytes(), reserved);
    voices.release(handle).unwrap();
    assert_eq!(voices.reserved_bytes(), 0);
    assert_eq!(packets.io_mut().marker, 91);
    assert_eq!(packets.io_mut().receives, 0);
    assert_eq!(packets.io_mut().sends, 0);
}

#[test]
fn joint_service_borrows_mutate_both_original_owners_and_preserve_voice_handle_authority() {
    let (voices, packets) = owners();
    let mut traffic = Rel99Traffic::new(voices, packets, count(4));
    let handle = {
        let (voices, packets) = traffic.services_mut();
        assert_eq!(packets.io_mut().marker, 73);
        packets.io_mut().marker = 91;
        prepared_voice(voices)
    };
    let reserved = traffic.voices_mut().reserved_bytes();
    assert!(reserved > 0);
    {
        let (voices, packets) = traffic.services_mut();
        assert_eq!(voices.reserved_bytes(), reserved);
        assert!(matches!(
            voices.initialization_status(handle),
            Ok(voice_bearers::InitializationStatus::Pending)
        ));
        voices
            .tick_initialization(handle, std::time::Instant::now())
            .unwrap();
        packets.io_mut().marker += 1;
        assert_eq!(packets.io_mut().receives, 0);
        assert_eq!(packets.io_mut().sends, 0);
    }
    let (mut voices, mut packets) = traffic.into_parts();
    assert_eq!(voices.reserved_bytes(), reserved);
    assert!(matches!(
        voices.initialization_status(handle),
        Ok(voice_bearers::InitializationStatus::Pending)
    ));
    voices.release(handle).unwrap();
    assert_eq!(voices.reserved_bytes(), 0);
    assert_eq!(packets.io_mut().marker, 92);
    assert_eq!(packets.io_mut().receives, 0);
    assert_eq!(packets.io_mut().sends, 0);
}

#[test]
fn ended_media_drains_only_its_exact_radio_route_until_release() {
    let (mut voices, packets) = owners();
    let handle = prepared_voice(&mut voices);
    let reserved = voices.reserved_bytes();
    voices.begin_media_drain(handle).unwrap();
    voices.begin_media_drain(handle).unwrap();
    let mut traffic = Rel99Traffic::new(voices, packets, count(4));
    let mut unit = delivery(Scope::User(CnDomain::Cs), 8);
    unit.metadata.bearer = BearerId::new(10).unwrap();
    assert!(traffic.uplink(unit).unwrap());
    let mut foreign = delivery(Scope::User(CnDomain::Cs), 8);
    foreign.metadata.bearer = BearerId::new(10).unwrap();
    foreign.metadata.identity.configuration = NonZeroU64::new(2).unwrap();
    assert!(matches!(traffic.uplink(foreign), Err(Error::Voice(_))));
    assert_eq!(traffic.voices().drained_uplink(handle), Some(1));
    assert_eq!(traffic.voices().reserved_bytes(), reserved);
    traffic.voices_mut().release(handle).unwrap();
    assert_eq!(traffic.voices().reserved_bytes(), 0);
    let mut unit = delivery(Scope::User(CnDomain::Cs), 8);
    unit.metadata.bearer = BearerId::new(10).unwrap();
    assert!(matches!(traffic.uplink(unit), Err(Error::Voice(_))));
}

#[test]
fn signalling_scope_is_rejected_without_entering_either_user_plane_owner() {
    let (voices, packets) = owners();
    let mut traffic = Rel99Traffic::new(voices, packets, count(4));
    assert!(matches!(
        traffic.uplink(delivery(Scope::Signalling, 8)),
        Err(Error::Scope)
    ));
    assert_eq!(traffic.voices_mut().reserved_bytes(), 0);
    assert_eq!(traffic.packets_mut().io_mut().receives, 0);
    assert_eq!(traffic.packets_mut().io_mut().sends, 0);
}

#[test]
fn cs_and_ps_unknown_routes_preserve_errors_from_the_correct_owner() {
    let (voices, packets) = owners();
    let mut traffic = Rel99Traffic::new(voices, packets, count(4));
    assert!(matches!(
        traffic.uplink(delivery(Scope::User(CnDomain::Cs), 8)),
        Err(Error::Voice(_))
    ));
    assert!(matches!(
        traffic.uplink(delivery(Scope::User(CnDomain::Ps), 8)),
        Err(Error::Packet(_))
    ));
}

#[test]
fn inconsistent_payload_lengths_are_never_accepted_or_routed_across_domains() {
    let (voices, packets) = owners();
    let mut traffic = Rel99Traffic::new(voices, packets, count(4));
    for bits in [0, 7, 9, usize::MAX] {
        assert!(matches!(
            traffic.uplink(delivery(Scope::User(CnDomain::Cs), bits)),
            Err(Error::Voice(_))
        ));
        assert!(matches!(
            traffic.uplink(delivery(Scope::User(CnDomain::Ps), bits)),
            Err(Error::Packet(_))
        ));
    }
    assert_eq!(traffic.packets_mut().io_mut().sends, 0);
}
