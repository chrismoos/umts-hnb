// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    packet_bearers::PacketBearers,
    packet_rabs::PacketRabs,
    rel99_traffic::Rel99Traffic,
    voice_bearers::{Limits as VoiceLimits, VoiceTraffic},
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::{NonZeroU16, NonZeroUsize},
};
use umts_config::Config;
use umts_user_plane::packet::{BoundDatagrams, Limits as PacketLimits, PacketPlane, UdpEndpoint};

const GTP_USER_PORT: u16 = 2152;
const SUPPORTED_GTP_VERSION: NonZeroU16 = NonZeroU16::MIN;
const MAXIMUM_COMBINED_MEDIA_BYTES: usize = 512 * 1024 * 1024;

pub(super) struct MediaPlan {
    release_on_error_indication: bool,
    bind: SocketAddr,
    advertise: IpAddr,
    packet: PacketLimits,
    voice: VoiceLimits,
    maximum_combinations: NonZeroUsize,
    packet_storage_bytes: usize,
    voice_storage_bytes: usize,
}

fn count(value: u32) -> Result<NonZeroUsize> {
    usize::try_from(value)
        .ok()
        .and_then(NonZeroUsize::new)
        .context("media capacity must be nonzero and fit the host address space")
}

impl MediaPlan {
    pub(super) fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let gtpu = &config.core.gtpu;
        ensure!(
            gtpu.bind_port == GTP_USER_PORT,
            "Rust packet serving requires gtpu.bind_port=2152"
        );
        ensure!(
            !gtpu.allow_ephemeral_port,
            "Rust packet serving does not support gtpu.allow_ephemeral_port"
        );
        let bind: Ipv4Addr = gtpu
            .bind_ip
            .parse()
            .context("invalid GTP-U bind IPv4 address")?;
        ensure!(
            !bind.is_multicast() && !bind.is_broadcast(),
            "GTP-U bind address must be unicast or unspecified"
        );
        let advertise: Ipv4Addr = config
            .gtpu_advertise_ip()
            .parse()
            .context("invalid GTP-U advertised IPv4 address")?;
        ensure!(
            !advertise.is_unspecified() && !advertise.is_multicast() && !advertise.is_broadcast(),
            "GTP-U advertised address must be a specified unicast IPv4 address"
        );

        let routes = count(config.runtime.maximum_ues)?;
        let queued = count(config.runtime.queued_sdus_per_bearer)?;
        let payload = count(config.runtime.maximum_sdu_bytes)?;
        let packet_budget = usize::try_from(config.runtime.media.maximum_packet_bytes)?;
        let voice_budget = usize::try_from(config.runtime.media.maximum_voice_bytes)?;
        let combined = packet_budget
            .checked_add(voice_budget)
            .context("combined media budget overflow")?;
        ensure!(
            combined <= MAXIMUM_COMBINED_MEDIA_BYTES,
            "combined media budget exceeds 512 MiB"
        );
        let plane_budget = packet_budget
            .checked_sub(payload.get())
            .context("packet budget cannot reserve UDP payload scratch")?;
        let packet = PacketLimits {
            routes,
            packets_per_route: queued,
            maximum_payload_bytes: payload,
            maximum_storage_bytes: plane_budget,
            receive_budget: count(u32::from(config.receiver.dedicated.work_per_cycle))?,
            route_visits: count(u32::from(config.receiver.dedicated.work_per_cycle))?,
        };
        let packet_storage_bytes = PacketPlane::<UdpEndpoint>::required_storage_bytes(packet)
            .map_err(|error| anyhow!("packet media budget: {error:?}"))?
            .checked_add(payload.get())
            .context("packet media scratch reservation overflow")?;
        ensure!(
            packet_storage_bytes <= packet_budget,
            "packet media reservation exceeds its budget"
        );
        let voice = VoiceLimits {
            routes,
            frames_per_route: queued,
            maximum_payload_bytes: payload,
            maximum_storage_bytes: voice_budget,
        };
        let voice_storage_bytes = voice
            .required_route_storage_bytes()
            .map_err(|error| anyhow!("voice media sizing: {error:?}"))?
            .checked_mul(routes.get())
            .context("voice media reservation overflow")?;
        ensure!(
            voice_storage_bytes <= voice_budget,
            "voice media reservation exceeds its budget ({voice_storage_bytes} bytes)"
        );
        Ok(Self {
            release_on_error_indication: gtpu.release_rab_on_error_indication,
            bind: SocketAddr::new(IpAddr::V4(bind), gtpu.bind_port),
            advertise: IpAddr::V4(advertise),
            packet,
            voice,
            maximum_combinations: count(u32::from(
                config.rel99.physical.planner.budgets.maximum_combinations,
            ))?,
            packet_storage_bytes,
            voice_storage_bytes,
        })
    }

    pub(super) fn bind_address(&self) -> SocketAddr {
        self.bind
    }

    #[cfg(test)]
    fn storage_bytes(&self) -> (usize, usize) {
        (self.packet_storage_bytes, self.voice_storage_bytes)
    }

    pub(super) fn construct<D: BoundDatagrams>(self, datagrams: D) -> Result<Rel99Traffic<D>> {
        ensure!(
            datagrams
                .local_address()
                .context("read injected GTP-U bound address")?
                == self.bind,
            "GTP-U transport is not bound to the planned canonical address"
        );
        ensure!(
            self.voice_storage_bytes <= self.voice.maximum_storage_bytes,
            "voice media storage does not fit its validated reservation"
        );
        let mut plane = PacketPlane::new(datagrams, self.packet)
            .map_err(|error| anyhow!("construct packet plane: {error:?}"))?;
        plane.set_local_address(self.bind.ip());
        ensure!(
            plane
                .storage_bytes()
                .checked_add(self.packet.maximum_payload_bytes.get())
                == Some(self.packet_storage_bytes),
            "packet media storage does not match its validated reservation"
        );
        let packets = PacketRabs::new(
            PacketBearers::new(plane),
            Some(self.advertise),
            SUPPORTED_GTP_VERSION,
        )
        .map_err(|error| anyhow!("construct packet RAB owner: {error:?}"))?
        .with_error_indication_release(self.release_on_error_indication);
        let voices = VoiceTraffic::new(self.voice)
            .map_err(|error| anyhow!("construct voice media owner: {error:?}"))?;
        Ok(Rel99Traffic::new(
            voices,
            packets,
            self.maximum_combinations,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use umts_user_plane::packet::Datagrams;

    struct Memory {
        local: SocketAddr,
        marker: u64,
    }
    impl Datagrams for Memory {
        fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
            panic!("planning cannot receive packets")
        }
        fn send(&mut self, _: &[u8], _: SocketAddr) -> io::Result<bool> {
            panic!("planning cannot send packets")
        }
    }
    impl BoundDatagrams for Memory {
        fn local_address(&self) -> io::Result<SocketAddr> {
            Ok(self.local)
        }
    }
    fn config() -> Config {
        serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99.yaml"
        )))
        .unwrap()
    }

    #[test]
    fn pure_plan_derives_routes_queues_and_separate_cell_budgets() {
        let config = config();
        let plan = MediaPlan::new(&config).unwrap();
        assert_eq!(
            plan.packet.routes.get(),
            config.runtime.maximum_ues as usize
        );
        assert_eq!(plan.voice.routes, plan.packet.routes);
        assert_eq!(
            plan.packet.packets_per_route.get(),
            config.runtime.queued_sdus_per_bearer as usize
        );
        assert_eq!(plan.voice.frames_per_route, plan.packet.packets_per_route);
        assert_eq!(
            plan.voice.maximum_payload_bytes.get(),
            config.runtime.maximum_sdu_bytes as usize
        );
        let (packet, voice) = plan.storage_bytes();
        assert!(packet <= config.runtime.media.maximum_packet_bytes as usize);
        assert!(voice <= config.runtime.media.maximum_voice_bytes as usize);
        assert!(packet > plan.packet.maximum_payload_bytes.get());
    }

    #[test]
    fn injected_actual_bound_owner_is_preserved_without_network_io() {
        let plan = MediaPlan::new(&config()).unwrap();
        let local = plan.bind_address();
        let mut traffic = plan.construct(Memory { local, marker: 71 }).unwrap();
        assert_eq!(traffic.packets_mut().io_mut().marker, 71);
        assert_eq!(traffic.packets_mut().local_address(), local);
    }

    #[test]
    fn packet_and_voice_reservations_have_exact_budget_boundaries() {
        let mut config = config();
        let plan = MediaPlan::new(&config).unwrap();
        let (packet, voice) = plan.storage_bytes();
        config.runtime.media.maximum_packet_bytes = u32::try_from(packet).unwrap();
        config.runtime.media.maximum_voice_bytes = u32::try_from(voice).unwrap();
        assert!(MediaPlan::new(&config).is_ok());
        config.runtime.media.maximum_packet_bytes -= 1;
        assert!(MediaPlan::new(&config).is_err());
        config.runtime.media.maximum_packet_bytes += 1;
        config.runtime.media.maximum_voice_bytes -= 1;
        assert!(MediaPlan::new(&config).is_err());
    }

    #[test]
    fn unsupported_gtpu_flags_ports_addresses_and_injected_bindings_fail_closed() {
        let mutations: &[fn(&mut Config)] = &[
            |config| config.core.gtpu.allow_ephemeral_port = true,
            |config| config.core.gtpu.bind_port = 2153,
            |config| config.core.gtpu.advertise_ip = Some("0.0.0.0".into()),
            |config| config.core.gtpu.bind_ip = "224.0.0.1".into(),
            |config| config.runtime.maximum_sdu_bytes = 65_536,
            |config| config.runtime.media.maximum_packet_bytes = 0,
        ];
        for mutation in mutations {
            let mut config = config();
            mutation(&mut config);
            assert!(MediaPlan::new(&config).is_err());
        }
        let plan = MediaPlan::new(&config()).unwrap();
        assert!(
            plan.construct(Memory {
                local: "127.0.0.1:2153".parse().unwrap(),
                marker: 0
            })
            .is_err()
        );
    }

    #[test]
    fn wildcard_bind_requires_a_real_explicit_advertised_address() {
        let mut config = config();
        config.core.gtpu.bind_ip = "0.0.0.0".into();
        config.core.gtpu.advertise_ip = Some("127.0.0.20".into());
        let plan = MediaPlan::new(&config).unwrap();
        assert_eq!(plan.bind_address(), "0.0.0.0:2152".parse().unwrap());
        assert!(
            plan.construct(Memory {
                local: "0.0.0.0:2152".parse().unwrap(),
                marker: 0
            })
            .is_ok()
        );
    }
}
