// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{packet_bearers::PacketHandoff, packet_rabs::PacketRabs, voice_bearers::VoiceTraffic};
use umts_core::{Gateway, Transport};
use umts_user_plane::packet::BoundDatagrams;

pub struct ReleaseServices<'a, C: Transport, D: BoundDatagrams> {
    pub gateway: &'a Gateway<C>,
    pub resources: &'a mut RadioResources,
    pub radio: &'a mut CommonSignalling,
    pub receive: &'a mut DedicatedController,
    pub packets: &'a mut PacketRabs<D>,
    pub voices: &'a mut VoiceTraffic,
}
#[derive(Debug)]
pub enum MediaReleaseError {
    Authority,
    Resources(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        crate::radio_resources::Error,
    ),
    Packet(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::packet_rabs::Error,
    ),
    Voice(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::voice_bearers::Error,
    ),
}
#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct MediaHandoff {
    pub packets: PacketHandoff,
    pub retained_voices: usize,
}
impl Rel99Admission {
    pub fn commit_core_release<C: Transport, D: BoundDatagrams>(
        &mut self,
        previous: ReceiveIdentity,
        output: ReleaseOutputPolicy,
        services: ReleaseServices<'_, C, D>,
    ) -> Result<
        MappingRelease<MediaHandoff>,
        BearerReleaseCommitError<DedicatedReleaseError<MediaReleaseError>>,
    > {
        let ReleaseServices {
            gateway,
            resources,
            radio,
            receive,
            packets,
            voices,
        } = services;
        self.commit_release_media(
            previous,
            output,
            MediaServices {
                gateway,
                resources,
                radio,
                receive,
                packets,
                voices,
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn commit_core_release_packet_plane<
        C: Transport,
        D: umts_user_plane::packet::Datagrams,
    >(
        &mut self,
        previous: ReceiveIdentity,
        output: ReleaseOutputPolicy,
        services: TestReleaseServices<'_, C, D>,
    ) -> Result<
        MappingRelease<MediaHandoff>,
        BearerReleaseCommitError<DedicatedReleaseError<MediaReleaseError>>,
    > {
        let TestReleaseServices {
            gateway,
            resources,
            radio,
            receive,
            packets,
            voices,
        } = services;
        self.commit_release_media(
            previous,
            output,
            MediaServices {
                gateway,
                resources,
                radio,
                receive,
                packets,
                voices,
            },
        )
    }

    fn commit_release_media<C: Transport, P: ReleasePacketService>(
        &mut self,
        previous: ReceiveIdentity,
        output: ReleaseOutputPolicy,
        services: MediaServices<'_, C, P>,
    ) -> Result<
        MappingRelease<MediaHandoff>,
        BearerReleaseCommitError<DedicatedReleaseError<MediaReleaseError>>,
    > {
        let MediaServices {
            gateway,
            resources,
            radio,
            receive,
            packets,
            voices,
        } = services;
        let entry = self.entry_mut(previous).ok_or_else(phase_error)?;
        let release = entry.lifecycle.release().ok_or_else(phase_error)?;
        let authority = release.core.as_ref().ok_or_else(phase_error)?;
        if !authority.authorized(gateway) {
            return Err(BearerReleaseCommitError::Commit(
                DedicatedReleaseError::Release(
                    crate::dedicated_bearers::MappingReleaseError::Cleanup(
                        MediaReleaseError::Authority,
                    ),
                ),
            ));
        }
        let plan = release
            .pool
            .as_ref()
            .ok_or_else(phase_error)?
            .plan()
            .clone();
        commit_entry_with_view(
            entry,
            Some(resources),
            radio,
            receive,
            output,
            |released, accepted, bank, circuits, initial_packets, additional, publish| {
                let packet = packets.prepare(accepted, plan.clone(), bank)?;
                let transports = circuits
                    .iter_mut()
                    .chain(
                        additional
                            .iter_mut()
                            .filter_map(AdditionalAssignment::circuits_mut),
                    )
                    .flat_map(|state| state.released_transports(released));
                let voice = voices
                    .prepare_release_handoff(accepted, plan, bank, transports)
                    .map_err(MediaReleaseError::Voice)?;
                publish().map_err(MediaReleaseError::Resources)?;
                let retained_voices = voice.commit();
                if let Some(initial) = initial_packets.as_mut() {
                    initial.finish_radio_release(packet.removed());
                }
                for assignment in additional.iter_mut() {
                    assignment.finish_radio_release(released, packet.removed());
                }
                let packets = packet.commit();
                if let Some(circuits) = circuits.as_mut() {
                    circuits.finish_radio_release(released);
                }
                additional.retain(|assignment| !assignment.retired(voices));
                Ok(MediaHandoff {
                    packets,
                    retained_voices,
                })
            },
        )
    }
}

struct MediaServices<'a, C: Transport, P> {
    gateway: &'a Gateway<C>,
    resources: &'a mut RadioResources,
    radio: &'a mut CommonSignalling,
    receive: &'a mut DedicatedController,
    packets: &'a mut P,
    voices: &'a mut VoiceTraffic,
}

#[cfg(test)]
pub(crate) struct TestReleaseServices<'a, C: Transport, D: umts_user_plane::packet::Datagrams> {
    pub gateway: &'a Gateway<C>,
    pub resources: &'a mut RadioResources,
    pub radio: &'a mut CommonSignalling,
    pub receive: &'a mut DedicatedController,
    pub packets: &'a mut crate::packet_bearers::PacketBearers<D>,
    pub voices: &'a mut VoiceTraffic,
}

trait ReleasePacketGuard {
    fn removed(&self) -> &[umts_user_plane::packet::RouteHandle];
    fn commit(self) -> PacketHandoff;
}
trait ReleasePacketService {
    type Prepared<'a>: ReleasePacketGuard
    where
        Self: 'a;
    fn prepare<'a>(
        &'a mut self,
        accepted: &AcceptedBearerRelease,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
        bank: &'a crate::dedicated_bearers::DedicatedBearers,
    ) -> Result<Self::Prepared<'a>, MediaReleaseError>;
}
impl<D: BoundDatagrams> ReleasePacketGuard for crate::packet_rabs::PreparedPacketRabHandoff<'_, D> {
    fn removed(&self) -> &[umts_user_plane::packet::RouteHandle] {
        self.removed()
    }
    fn commit(self) -> PacketHandoff {
        self.commit()
    }
}
impl<D: BoundDatagrams> ReleasePacketService for PacketRabs<D> {
    type Prepared<'a>
        = crate::packet_rabs::PreparedPacketRabHandoff<'a, D>
    where
        Self: 'a;
    fn prepare<'a>(
        &'a mut self,
        accepted: &AcceptedBearerRelease,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
        bank: &'a crate::dedicated_bearers::DedicatedBearers,
    ) -> Result<Self::Prepared<'a>, MediaReleaseError> {
        self.prepare_release_handoff(accepted, plan, bank)
            .map_err(MediaReleaseError::Packet)
    }
}
#[cfg(test)]
impl<D: umts_user_plane::packet::Datagrams> ReleasePacketGuard
    for crate::packet_bearers::PreparedPacketHandoff<'_, D>
{
    fn removed(&self) -> &[umts_user_plane::packet::RouteHandle] {
        self.removed()
    }
    fn commit(self) -> PacketHandoff {
        self.commit()
    }
}
#[cfg(test)]
impl<D: umts_user_plane::packet::Datagrams> ReleasePacketService
    for crate::packet_bearers::PacketBearers<D>
{
    type Prepared<'a>
        = crate::packet_bearers::PreparedPacketHandoff<'a, D>
    where
        Self: 'a;
    fn prepare<'a>(
        &'a mut self,
        accepted: &AcceptedBearerRelease,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
        bank: &'a crate::dedicated_bearers::DedicatedBearers,
    ) -> Result<Self::Prepared<'a>, MediaReleaseError> {
        self.prepare_release_handoff(accepted, plan, bank)
            .map_err(|error| MediaReleaseError::Packet(crate::packet_rabs::Error::Binding(error)))
    }
}
