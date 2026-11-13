// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::DedicatedBearers, packet_rabs::PacketRabs, rel99_plan::CompiledRel99,
    voice_bearers::VoiceTraffic,
};
use umts_user_plane::packet::BoundDatagrams;

#[derive(Debug)]
pub enum AdditionMediaError {
    Admission(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        AdmissionError,
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
    Authority(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_core::Error,
    ),
}

pub(super) enum CommitFailure<E> {
    Admission(AdmissionError),
    Media(E),
}

pub(super) trait MediaCommit {
    type Error;
    fn commit(
        self,
        accepted: &AcceptedAdditionalSetup,
        plan: Arc<CompiledRel99>,
        bank: &DedicatedBearers,
        publish: impl FnOnce() -> Result<(), AdmissionError>,
    ) -> Result<(), CommitFailure<Self::Error>>;
}

#[cfg(test)]
pub(super) struct NoMedia;
#[cfg(test)]
impl MediaCommit for NoMedia {
    type Error = std::convert::Infallible;
    fn commit(
        self,
        _: &AcceptedAdditionalSetup,
        _: Arc<CompiledRel99>,
        _: &DedicatedBearers,
        publish: impl FnOnce() -> Result<(), AdmissionError>,
    ) -> Result<(), CommitFailure<Self::Error>> {
        publish().map_err(CommitFailure::Admission)
    }
}

struct BoundMedia<'a, D: BoundDatagrams> {
    packets: &'a mut PacketRabs<D>,
    voices: &'a mut VoiceTraffic,
}
impl<D: BoundDatagrams> MediaCommit for BoundMedia<'_, D> {
    type Error = AdditionMediaError;
    fn commit(
        self,
        accepted: &AcceptedAdditionalSetup,
        plan: Arc<CompiledRel99>,
        bank: &DedicatedBearers,
        publish: impl FnOnce() -> Result<(), AdmissionError>,
    ) -> Result<(), CommitFailure<Self::Error>> {
        let packet = self
            .packets
            .prepare_additional_handoff(accepted, plan.clone(), bank)
            .map_err(|error| CommitFailure::Media(AdditionMediaError::Packet(error)))?;
        let voice = self
            .voices
            .prepare_additional_handoff(accepted, plan, bank)
            .map_err(|error| CommitFailure::Media(AdditionMediaError::Voice(error)))?;
        publish().map_err(CommitFailure::Admission)?;
        voice.commit();
        packet.commit();
        Ok(())
    }
}

impl Rel99Admission {
    #[cfg(test)]
    pub fn commit_addition_with_media<D: BoundDatagrams>(
        &mut self,
        previous: ReceiveIdentity,
        resources: &mut RadioResources,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        packets: &mut PacketRabs<D>,
        voices: &mut VoiceTraffic,
    ) -> Result<ReceiveIdentity, AdditionMediaError> {
        if self.additional_assignment(previous).is_some() {
            return Err(AdditionMediaError::Admission(AdmissionError::Phase));
        }
        self.commit_addition_owner(
            previous,
            resources,
            radio,
            receive,
            BoundMedia { packets, voices },
        )
        .map_err(|error| match error {
            CommitFailure::Admission(error) => AdditionMediaError::Admission(error),
            CommitFailure::Media(error) => error,
        })
    }
}

pub struct AdditionCommitServices<'a, C: umts_core::Transport, D: BoundDatagrams> {
    pub gateway: &'a umts_core::Gateway<C>,
    pub resources: &'a mut RadioResources,
    pub radio: &'a mut CommonSignalling,
    pub receive: &'a mut DedicatedController,
    pub packets: &'a mut PacketRabs<D>,
    pub voices: &'a mut VoiceTraffic,
}

impl Rel99Admission {
    pub fn commit_requested_addition<C: umts_core::Transport, D: BoundDatagrams>(
        &mut self,
        previous: ReceiveIdentity,
        services: AdditionCommitServices<'_, C, D>,
    ) -> Result<ReceiveIdentity, AdditionMediaError> {
        self.additional_assignment(previous)
            .ok_or(AdditionMediaError::Admission(AdmissionError::Phase))?
            .selection()
            .authorize_items(services.gateway)
            .map_err(AdditionMediaError::Authority)?;
        self.commit_addition_owner(
            previous,
            services.resources,
            services.radio,
            services.receive,
            BoundMedia {
                packets: services.packets,
                voices: services.voices,
            },
        )
        .map_err(|error| match error {
            CommitFailure::Admission(error) => AdditionMediaError::Admission(error),
            CommitFailure::Media(error) => error,
        })
    }
}
