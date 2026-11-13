// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::{DedicatedBearers, DownlinkFrame, FrameBuildError, SduDelivery},
    dedicated_controller::DedicatedController,
    packet_bearers,
    packet_rabs::{self, PacketRabs},
    rel99_admission::{DuplexFramePolicy, FrameOpportunity, FramePolicy, TrafficFramePolicy},
    signalling::CommonSignalling,
    voice_bearers::{self, VoiceTraffic},
};
use std::num::NonZeroUsize;
use umts_dsp::tfci::Tfci;
use umts_security::ciphering::Scope;
use umts_types::{CnDomain, time::ChipInstant};
use umts_user_plane::packet::BoundDatagrams;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub enum Error {
    Scope,
    Voice(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        voice_bearers::Error,
    ),
    Packet(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        packet_rabs::Error,
    ),
}

pub struct Rel99Traffic<D: BoundDatagrams> {
    frames: TrafficFramePolicy<VoiceTraffic>,
    packets: PacketRabs<D>,
}

impl<D: BoundDatagrams> Rel99Traffic<D> {
    pub fn new(
        voices: VoiceTraffic,
        packets: PacketRabs<D>,
        maximum_combinations: NonZeroUsize,
    ) -> Self {
        Self {
            frames: TrafficFramePolicy::new(voices, maximum_combinations),
            packets,
        }
    }

    #[cfg(test)]
    pub fn voices_mut(&mut self) -> &mut VoiceTraffic {
        self.frames.source_mut()
    }
    #[cfg(test)]
    pub fn voices(&self) -> &VoiceTraffic {
        self.frames.source()
    }
    pub fn packets_mut(&mut self) -> &mut PacketRabs<D> {
        &mut self.packets
    }
    pub fn services_mut(&mut self) -> (&mut VoiceTraffic, &mut PacketRabs<D>) {
        (self.frames.source_mut(), &mut self.packets)
    }

    #[cfg(test)]
    pub fn into_parts(self) -> (VoiceTraffic, PacketRabs<D>) {
        (self.frames.into_source(), self.packets)
    }

    pub fn network_step(
        &mut self,
        receive: &mut DedicatedController,
    ) -> Result<packet_bearers::Step, packet_rabs::Error> {
        self.packets.step(receive)
    }
}

impl<D: BoundDatagrams> FramePolicy for Rel99Traffic<D> {
    fn select(
        &mut self,
        opportunity: FrameOpportunity,
        now: ChipInstant,
        bank: &mut DedicatedBearers,
        radio: &mut CommonSignalling,
    ) -> Result<Tfci, FrameBuildError> {
        self.frames.select(opportunity, now, bank, radio)
    }

    fn fill(
        &mut self,
        draft: &mut DownlinkFrame<'_>,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        self.frames.fill(draft, now, radio)
    }
}

impl<D: BoundDatagrams> DuplexFramePolicy for Rel99Traffic<D> {
    type UplinkError = Error;

    fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Self::UplinkError> {
        match delivery.metadata.scope {
            Scope::User(CnDomain::Cs) => self
                .frames
                .source_mut()
                .uplink(delivery)
                .map_err(Error::Voice),
            Scope::User(CnDomain::Ps) => self.packets.uplink(delivery).map_err(Error::Packet),
            _ => Err(Error::Scope),
        }
    }
}
