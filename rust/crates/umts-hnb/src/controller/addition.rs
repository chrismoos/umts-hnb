// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CommonController, ControllerError, MediaServiceFailure, MediaServiceStep};
use crate::{
    packet_rabs::PacketRabs,
    radio_resources::RadioResources,
    rel99_admission::{
        AdditionCommitServices, AdditionMediaError, AdditionPhase, AdditionalMediaError,
        AdditionalMediaServices, AdmissionError, CircuitPlayoutTime, CircuitSettings,
        InitialCircuitError,
    },
    rel99_planner::RabPlan,
    voice_bearers::VoiceTraffic,
};
use std::num::NonZeroUsize;
use umts_core::Transport;
use umts_iuh::{ranap::rab::SetupOrModify, session::ControlTime};
use umts_user_plane::packet::BoundDatagrams;

pub struct AdditionalServiceSettings {
    pub control: ControlTime,
    pub playout: CircuitPlayoutTime,
    pub visits: NonZeroUsize,
}

#[derive(Debug)]
pub enum AdditionalServiceError {
    RecoveryRequired,
    Commit(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdditionMediaError,
    ),
    Media(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdditionalMediaError,
    ),
    Admission(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdmissionError,
    ),
}

impl<T: Transport> CommonController<T> {
    pub fn begin_initial_after_release_rab_with_settings(
        &mut self,
        released: crate::rel99_admission::SetupAfterRelease,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut RadioResources,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedInitial,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<
        crate::dedicated_rx::ReceiveIdentity,
        Box<crate::rel99_admission::RejectedRabAssignment>,
    > {
        let (Some(admission), Some(receive)) = (&mut self.admission, &self.dedicated) else {
            return Err(Box::new(crate::rel99_admission::RejectedRabAssignment {
                error: crate::rel99_admission::RabAdmissionError::Construction,
                pending: released.into_pending(),
                allocation: None,
                rollback_error: None,
            }));
        };
        crate::rel99_admission::InitialRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        }
        .begin_after_release_with_settings(released, settings)
    }

    pub fn additional_rab_ready(&self, identity: crate::dedicated_rx::ReceiveIdentity) -> bool {
        self.radio.additional_setup_ready(identity)
            && self
                .admission
                .as_ref()
                .is_some_and(|admission| admission.additional_request_ready(identity))
    }

    pub fn begin_additional_rab_with_delivery(
        &mut self,
        pending: crate::rab_handoff::PendingRabAssignment,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut RadioResources,
        launch: crate::rel99_admission::AdditionalLaunch,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedExisting,
            &crate::rel99_admission::AdditionalDelivery,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<
        crate::dedicated_rx::ReceiveIdentity,
        Box<crate::rel99_admission::RejectedAdditionalAssignment>,
    > {
        let reject = |pending| {
            Box::new(crate::rel99_admission::RejectedAdditionalAssignment {
                error: crate::rel99_admission::RabAdmissionError::Construction,
                pending,
                change: None,
                rollback_error: None,
            })
        };
        let Some(previous) = self.radio.dedicated_route(pending.request().key()) else {
            return Err(reject(pending));
        };
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Err(reject(pending));
        };
        crate::rel99_admission::AdditionalRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        }
        .begin_with_delivery(previous, pending, launch, settings)
    }

    pub fn begin_after_release_rab_with_delivery(
        &mut self,
        released: crate::rel99_admission::SetupAfterRelease,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut RadioResources,
        launch: crate::rel99_admission::AdditionalLaunch,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedExisting,
            &crate::rel99_admission::AdditionalDelivery,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<
        crate::dedicated_rx::ReceiveIdentity,
        Box<crate::rel99_admission::RejectedAdditionalAssignment>,
    > {
        let reject = |released: crate::rel99_admission::SetupAfterRelease| {
            Box::new(crate::rel99_admission::RejectedAdditionalAssignment {
                error: crate::rel99_admission::RabAdmissionError::Construction,
                pending: released.into_pending(),
                change: None,
                rollback_error: None,
            })
        };
        let Some(previous) = self.radio.dedicated_route(released.request().key()) else {
            return Err(reject(released));
        };
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Err(reject(released));
        };
        crate::rel99_admission::AdditionalRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        }
        .begin_after_release_with_delivery(previous, released, launch, settings)
    }

    pub fn service_additional_media<D: BoundDatagrams>(
        &mut self,
        resources: &mut RadioResources,
        packets: &mut PacketRabs<D>,
        voices: &mut VoiceTraffic,
        settings: AdditionalServiceSettings,
        mut circuit_settings: impl FnMut(
            &RabPlan,
            &SetupOrModify,
        ) -> Result<CircuitSettings, InitialCircuitError>,
    ) -> Result<MediaServiceStep, ControllerError> {
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Err(ControllerError::Configuration);
        };
        let core = &mut self.core;
        let radio = &mut self.radio;
        let mut report = MediaServiceStep::default();
        report.visited =
            admission.visit_additional_assignments(settings.visits, |admission, previous| {
                let mut identity = previous;
                let result = (|| {
                    match admission.addition_phase(identity) {
                        Some(AdditionPhase::ReadyToCommit) => {
                            identity = admission
                                .commit_requested_addition(
                                    previous,
                                    AdditionCommitServices {
                                        gateway: core.gateway(),
                                        resources,
                                        radio,
                                        receive,
                                        packets,
                                        voices,
                                    },
                                )
                                .map_err(AdditionalServiceError::Commit)?;
                        }
                        Some(AdditionPhase::Committed) => (),
                        Some(AdditionPhase::RecoveryRequired) => {
                            return Err(AdditionalServiceError::RecoveryRequired);
                        }
                        _ => return Ok(()),
                    }
                    let settled = admission
                        .service_additional_media(
                            identity,
                            AdditionalMediaServices {
                                core,
                                receive,
                                packets,
                                voices,
                                now: settings.control,
                                circuit_time: settings.playout,
                                circuit_budget: NonZeroUsize::MIN,
                            },
                            &mut circuit_settings,
                        )
                        .map_err(AdditionalServiceError::Media)?;
                    if settled {
                        admission
                            .finish_addition(identity, resources, radio, settings.playout.radio)
                            .map_err(AdditionalServiceError::Admission)?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    report.failures += 1;
                    if report.first_failure.is_none() {
                        report.first_failure = Some(MediaServiceFailure {
                            identity,
                            error: super::MediaServiceError::AdditionalService(error),
                        });
                    }
                }
            });
        Ok(report)
    }
}
