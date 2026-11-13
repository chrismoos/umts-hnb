// SPDX-License-Identifier: AGPL-3.0-or-later

use super::CommonController;
use crate::{
    dedicated_rx::ReceiveIdentity,
    packet_rabs::PacketRabs,
    rab_handoff::{FailedRabAssignment, RejectionStep},
    rel99_admission::{AdditionalMediaError, RetiredAdmission},
    voice_bearers::VoiceTraffic,
};
use std::num::NonZeroUsize;
use umts_core::Transport;
use umts_iuh::ranap::rab::Cause;
use umts_iuh::session::ControlTime;
use umts_user_plane::packet::BoundDatagrams;

impl<T: Transport> CommonController<T> {
    pub fn visit_failed_changes(
        &mut self,
        maximum: NonZeroUsize,
        mut visit: impl FnMut(ReceiveIdentity),
    ) -> usize {
        self.admission.as_mut().map_or(0, |admission| {
            admission.visit_failed_changes(maximum, |_, identity| visit(identity))
        })
    }
    pub fn take_failed_change(
        &mut self,
        identity: ReceiveIdentity,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<
        Option<crate::rel99_admission::FailedChangeRequest>,
        crate::rel99_admission::AdmissionError,
    > {
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Err(crate::rel99_admission::AdmissionError::Phase);
        };
        admission.take_failed_change(identity, &mut self.radio, receive, resources)
    }
    pub fn finish_failed_change(
        &mut self,
        retired: &mut RetiredAdmission,
        resources: &mut crate::radio_resources::RadioResources,
        now: umts_types::time::ChipInstant,
        control: ControlTime,
        cause: Cause,
    ) -> Result<bool, crate::rel99_admission::FailedChangeError> {
        retired.failed_change.as_mut().map_or(Ok(true), |failed| {
            failed.finish(&mut self.core, resources, now, control, cause)
        })
    }
    pub fn finish_additional_media_failure_dch<D: BoundDatagrams>(
        &mut self,
        retired: &mut RetiredAdmission,
        packets: &mut PacketRabs<D>,
        voices: &mut VoiceTraffic,
        now: ControlTime,
        cause: Cause,
    ) -> Result<(), AdditionalMediaError> {
        retired.finish_additional_media_failure(&mut self.core, packets, voices, now, cause)
    }

    pub fn visit_retired_initial_rabs(
        &mut self,
        maximum: NonZeroUsize,
        mut visit: impl FnMut(ReceiveIdentity),
    ) -> usize {
        self.admission.as_mut().map_or(0, |admission| {
            admission.visit_retired_initial_rabs(maximum, |_, identity| visit(identity))
        })
    }

    pub fn record_failed_assignment(
        &mut self,
        failure: &mut FailedRabAssignment,
        now: ControlTime,
        budget: NonZeroUsize,
    ) -> Result<RejectionStep, umts_core::Error> {
        failure.record(&mut self.core, now, budget)
    }
}
