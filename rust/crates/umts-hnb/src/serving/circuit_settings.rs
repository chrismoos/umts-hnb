// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    rel99_admission::{CircuitInitialization, CircuitSettings, InitialCircuitError},
    rel99_planner::RabPlan,
    voice_bearers::{self, IngressSettings, PlayoutSettings},
};
use anyhow::{Context, Result, ensure};
use std::{
    net::{IpAddr, SocketAddr},
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Duration,
};
use umts_config::{Config, RustCircuitInitialization};
use umts_iuh::{
    Domain,
    ranap::rab::{Peer, SetupOrModify},
};
use umts_types::time::CHIPS_PER_SECOND;
use umts_user_plane::{
    iuup::{initialization::Versions, negotiation::Settings},
    uplink_media::RtpSource,
};

const IMPLEMENTED_VERSIONS: u16 = 3;
const MILLISECONDS_PER_SECOND: u64 = 1000;
const RTP_TICKS_PER_MILLISECOND: u32 = 16;
const RTP_HALF_RANGE: u32 = 1 << 31;
const MAX_DATAGRAM_BYTES: u16 = 65_507;
const MAX_PAYLOAD_TYPE: u8 = 127;

pub(super) struct CircuitPlan {
    local: SocketAddr,
    advertised: IpAddr,
    ingress: IngressSettings,
    versions: Versions,
    negotiation: Settings,
    role: RustCircuitInitialization,
    wait_chips: NonZeroU64,
    playout: PlayoutSettings,
    next_source: Option<NonZeroU32>,
    sequence: u16,
    timestamp: u32,
}

#[cfg(test)]
mod tests;

fn unicast(address: IpAddr, unspecified: bool) -> bool {
    !address.is_multicast()
        && (unspecified || !address.is_unspecified())
        && !matches!(address, IpAddr::V4(ip) if ip.is_broadcast())
}

fn chips(milliseconds: u32) -> Result<NonZeroU64> {
    NonZeroU64::new(
        u64::from(milliseconds)
            .checked_mul(CHIPS_PER_SECOND)
            .context("circuit chip interval overflow")?
            / MILLISECONDS_PER_SECOND,
    )
    .context("circuit chip interval must be nonzero")
}

impl CircuitPlan {
    pub(super) fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        let circuit = &config.runtime.media.circuit;
        let bind: IpAddr = circuit
            .bind_ip
            .as_deref()
            .unwrap_or(&config.core.gtpu.bind_ip)
            .parse()
            .context("invalid circuit RTP bind address")?;
        let advertised: IpAddr = circuit
            .advertise_ip
            .as_deref()
            .unwrap_or(config.gtpu_advertise_ip())
            .parse()
            .context("invalid advertised circuit RTP address")?;
        ensure!(
            unicast(bind, true) && unicast(advertised, false),
            "circuit RTP addresses must be unicast and the advertised address specified"
        );
        ensure!(
            bind.is_ipv4() == advertised.is_ipv4(),
            "circuit RTP bind and advertised address families differ"
        );
        ensure!(
            circuit.payload_type <= MAX_PAYLOAD_TYPE
                && circuit.datagram_bytes > umts_user_plane::rtp::HEADER_BYTES as u16
                && circuit.datagram_bytes <= MAX_DATAGRAM_BYTES,
            "invalid circuit RTP payload type or datagram budget"
        );
        ensure!(
            circuit.versions != 0 && circuit.versions & !IMPLEMENTED_VERSIONS == 0,
            "unsupported Iu-UP mode-version mask"
        );
        let versions = Versions::new(circuit.versions)
            .map_err(|error| anyhow::anyhow!("Iu-UP versions: {error:?}"))?;
        let negotiation = Settings::new(
            Duration::from_millis(u64::from(circuit.timeout_ms)),
            circuit.repetitions,
            versions,
        )
        .map_err(|error| anyhow::anyhow!("Iu-UP initialization policy: {error:?}"))?;
        let horizon = circuit
            .horizon_ms
            .checked_mul(RTP_TICKS_PER_MILLISECOND)
            .and_then(NonZeroU32::new)
            .filter(|ticks| ticks.get() < RTP_HALF_RANGE)
            .context("circuit RTP horizon must fit the positive timestamp half-range")?;
        let seed = NonZeroU32::new(getrandom::u32().context("allocate circuit RTP source seed")?)
            .context("circuit RTP source seed was zero")?;
        let sequence_seed = getrandom::u32().context("allocate circuit RTP sequence seed")?;
        Ok(Self {
            local: SocketAddr::new(bind, 0),
            advertised,
            ingress: IngressSettings {
                datagram_bytes: NonZeroUsize::new(usize::from(circuit.datagram_bytes))
                    .context("empty circuit datagram budget")?,
                payload_type: circuit.payload_type,
                ssrc: None,
            },
            versions,
            negotiation,
            role: circuit.initialization,
            wait_chips: chips(circuit.wait_ms)?,
            playout: PlayoutSettings {
                lead_chips: chips(circuit.playout_lead_ms)?,
                horizon_ticks: horizon,
            },
            next_source: Some(seed),
            sequence: sequence_seed as u16,
            timestamp: getrandom::u32().context("allocate circuit RTP timestamp seed")?,
        })
    }

    pub(super) fn playout(&self) -> PlayoutSettings {
        self.playout
    }

    pub(super) fn settings(
        &mut self,
        rab: &RabPlan,
        request: &SetupOrModify,
    ) -> Result<CircuitSettings, InitialCircuitError> {
        voice_bearers::initialization::contract_from_rab(rab, request, self.versions).map_err(
            |error| InitialCircuitError::Voice {
                rab: rab.rab,
                error: voice_bearers::Error::Proposal(error),
            },
        )?;
        let transport = request
            .first
            .transport
            .as_ref()
            .ok_or(InitialCircuitError::Configuration)?;
        let Peer::Cs(peer) =
            transport
                .ip_peer(Domain::Cs)
                .map_err(|error| InitialCircuitError::Protocol {
                    rab: rab.rab,
                    error,
                })?
        else {
            return Err(InitialCircuitError::Configuration);
        };
        if peer.is_ipv4() != self.local.is_ipv4() {
            return Err(InitialCircuitError::Configuration);
        }
        let mask = rab
            .circuit_versions
            .ok_or(InitialCircuitError::Configuration)?;
        let versions = Versions::new(mask)
            .ok()
            .and_then(|offered| self.versions.intersection(offered))
            .ok_or(InitialCircuitError::Configuration)?;
        let initialization = match self.role {
            RustCircuitInitialization::Initiate => CircuitInitialization::Initiate(
                self.negotiation
                    .restrict_to(versions)
                    .map_err(|_| InitialCircuitError::Configuration)?,
            ),
            RustCircuitInitialization::Receive => CircuitInitialization::Receive(versions),
        };
        let ssrc = self.next_source.ok_or(InitialCircuitError::Capacity)?;
        self.next_source = ssrc.get().checked_add(1).and_then(NonZeroU32::new);
        let source = RtpSource {
            sequence: self.sequence,
            ssrc: ssrc.get(),
            payload_type: self.ingress.payload_type,
        };
        self.sequence = self.sequence.wrapping_add(1);
        Ok(CircuitSettings {
            local: self.local,
            advertised: Some(self.advertised),
            ingress: self.ingress,
            source,
            timestamp: self.timestamp,
            wait_chips: self.wait_chips,
            initialization,
        })
    }
}
