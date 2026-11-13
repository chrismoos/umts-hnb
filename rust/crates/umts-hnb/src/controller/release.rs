// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::ReleaseOutputPolicy, dedicated_controller::DedicatedReleaseError,
    signalling::BearerReleaseCommitError,
};

#[derive(Debug)]
pub enum ReleaseServiceError {
    Configuration,
    RecoveryRequired {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: crate::dedicated_rx::ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: Option<crate::rel99_admission::AdmissionError>,
    },
    Admission {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: crate::dedicated_rx::ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::rel99_admission::AdmissionError,
    },
    Core {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: crate::dedicated_rx::ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::rel99_admission::CoreReleaseError,
    },
    Commit {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: crate::dedicated_rx::ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: BearerReleaseCommitError<
            DedicatedReleaseError<crate::rel99_admission::MediaReleaseError>,
        >,
    },
}

impl<T: Transport> CommonController<T> {
    pub(crate) fn begin_iu_radio_release(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut crate::radio_resources::RadioResources,
        launch: crate::rel99_admission::AdditionalLaunch,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedRelease,
            &crate::rel99_admission::ReleaseDelivery,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<IuReleaseLaunch, Box<crate::rel99_admission::RetainedReleaseBuild>> {
        use crate::rel99_admission::{
            CoreReleaseError, ReleaseAdmissionError, RetainedReleaseBuild,
        };
        let reject = |error| {
            Box::new(RetainedReleaseBuild {
                error,
                pool: None,
                change: None,
            })
        };
        let Some(previous) = self.radio.dedicated_route(request.key()) else {
            return Ok(
                if resources.slot_reserved(request.key().slot as usize)
                    || self
                        .admission
                        .as_ref()
                        .is_some_and(|admission| admission.has_ue(request.key()))
                {
                    IuReleaseLaunch::Pending
                } else {
                    IuReleaseLaunch::NoRadio
                },
            );
        };
        if !self.release_rab_ready(previous) {
            return Ok(IuReleaseLaunch::Pending);
        }
        let Some(admission) = self.admission.as_ref() else {
            return Ok(IuReleaseLaunch::Pending);
        };
        let authority = admission
            .prepare_iu_radio_release(previous, request, self.core.gateway())
            .map_err(|error| reject(ReleaseAdmissionError::Core(error)))?;
        if authority.radio_bearers().is_empty() {
            return Ok(IuReleaseLaunch::NoRadio);
        }
        match self.ensure_hsupa_dch(previous, launch, resources) {
            Ok(true) => (),
            Ok(false) => return Ok(IuReleaseLaunch::Pending),
            Err(error) => {
                return Err(reject(ReleaseAdmissionError::HsupaRestoration(format!(
                    "{error:#}"
                ))));
            }
        }
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Ok(IuReleaseLaunch::Pending);
        };
        let mut host = crate::rel99_admission::ReleaseRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        };
        let constructed =
            host.construct_release(previous, authority.radio_bearers(), launch, settings)?;
        match admission.begin_iu_radio_release(
            constructed.request,
            authority,
            self.core.gateway(),
            &self.radio,
            receive,
        ) {
            Ok(identity) => {
                admission.commit_hsdpa_feedback(constructed.feedback);
                admission.attach_iu_release_delivery(previous, constructed.delivery);
                Ok(IuReleaseLaunch::Scheduled(identity))
            }
            Err(rejected) => {
                let crate::rel99_admission::ScheduledReleaseRequest { pool, change, .. } =
                    *rejected.request;
                Err(Box::new(RetainedReleaseBuild {
                    error: ReleaseAdmissionError::Core(CoreReleaseError::Admission(rejected.error)),
                    pool: Some(pool),
                    change,
                }))
            }
        }
    }

    pub(crate) fn complete_iu_connection_release(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
        now: umts_iuh::session::ControlTime,
        reports: &[umts_iuh::ranap::release::VolumeReport],
    ) -> Result<crate::signalling::ConnectionReleaseProgress, IuCompletionError> {
        let progress = self
            .radio
            .notify_owned_connection_release(request)
            .map_err(IuCompletionError::Radio)?;
        if progress != crate::signalling::ConnectionReleaseProgress::Complete {
            return Ok(progress);
        }
        self.core
            .gateway_mut()
            .complete_iu_release(now, request, reports)
            .map_err(IuCompletionError::Core)?;
        self.radio.finish_core_connection_release(request);
        Ok(progress)
    }
    pub(crate) fn take_iu_radio_completion(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
    ) {
        if let Some(admission) = self.admission.as_mut() {
            admission.take_completed_iu_radio_release(request);
        }
    }

    pub fn prepare_unmapped_release(
        &self,
        pending: crate::rab_handoff::PendingRabAssignment,
    ) -> Result<crate::rel99_admission::RabReleaseRequest, crate::rel99_admission::RejectedRabRelease>
    {
        match self.admission.as_ref() {
            Some(admission) => admission.prepare_unmapped_release(pending, self.core.gateway()),
            None => Err(crate::rel99_admission::RejectedRabRelease {
                error: crate::rel99_admission::CoreReleaseError::Identity,
                pending: Box::new(pending),
            }),
        }
    }

    pub fn release_rab_ready(&self, identity: crate::dedicated_rx::ReceiveIdentity) -> bool {
        self.radio.additional_setup_ready(identity)
            && self
                .admission
                .as_ref()
                .is_some_and(|admission| admission.additional_request_ready(identity))
    }

    pub fn begin_release_rab_with_delivery(
        &mut self,
        pending: crate::rab_handoff::PendingRabAssignment,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut crate::radio_resources::RadioResources,
        launch: crate::rel99_admission::AdditionalLaunch,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedRelease,
            &crate::rel99_admission::ReleaseDelivery,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<
        crate::rel99_admission::ReleaseLaunchResult,
        Box<crate::rel99_admission::RejectedReleaseAssignment>,
    > {
        let reject = |pending| {
            Box::new(crate::rel99_admission::RejectedReleaseAssignment {
                error: crate::rel99_admission::ReleaseAdmissionError::Core(
                    crate::rel99_admission::CoreReleaseError::Identity,
                ),
                pending,
                pool: None,
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
        crate::rel99_admission::ReleaseRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        }
        .begin_with_delivery(previous, pending, launch, settings)
    }

    pub fn service_one_release<D: umts_user_plane::packet::BoundDatagrams>(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
        packets: &mut crate::packet_rabs::PacketRabs<D>,
        voices: &mut crate::voice_bearers::VoiceTraffic,
        now: umts_types::time::ChipInstant,
        control: umts_iuh::session::ControlTime,
    ) -> Result<Option<crate::rel99_admission::RabReleaseRequest>, ReleaseServiceError> {
        let (Some(admission), Some(receive)) = (&mut self.admission, &mut self.dedicated) else {
            return Err(ReleaseServiceError::Configuration);
        };
        let core = &mut self.core;
        let radio = &mut self.radio;
        let mut result = Ok(None);
        admission.visit_release_assignments(std::num::NonZeroUsize::MIN, |admission, previous| {
            result = (|| {
                let mut identity = previous;
                match admission.release_phase(identity) {
                    Some(crate::rel99_admission::ScheduledReleasePhase::ReadyToCommit) => {
                        identity = admission
                            .commit_core_release(
                                previous,
                                ReleaseOutputPolicy::DiscardReleased,
                                crate::rel99_admission::ReleaseServices {
                                    gateway: core.gateway(),
                                    resources,
                                    radio,
                                    receive,
                                    packets,
                                    voices,
                                },
                            )
                            .map_err(|error| ReleaseServiceError::Commit {
                                identity: previous,
                                error,
                            })?
                            .next;
                    }
                    Some(crate::rel99_admission::ScheduledReleasePhase::RecoveryRequired) => {
                        return Err(ReleaseServiceError::RecoveryRequired {
                            identity,
                            error: admission.release_failure(identity).cloned(),
                        });
                    }
                    Some(_) => (),
                    None => {
                        return Err(ReleaseServiceError::Admission {
                            identity,
                            error: crate::rel99_admission::AdmissionError::Phase,
                        });
                    }
                }
                admission.attach_release_volumes(identity, |rab| {
                    packets.take_release_volume(umts_user_plane::packet::RouteKey {
                        connection: umts_types::ConnectionKey {
                            ue: identity.ue,
                            domain: umts_types::CnDomain::Ps,
                        },
                        rab,
                    })
                });
                admission
                    .record_release_outcomes(identity, core, control, std::num::NonZeroUsize::MIN)
                    .map_err(|error| ReleaseServiceError::Core { identity, error })?;
                if admission.release_phase(identity)
                    != Some(crate::rel99_admission::ScheduledReleasePhase::Committed)
                {
                    return Ok(None);
                }
                match admission
                    .finish_release_request(identity, resources, now)
                    .map_err(|error| ReleaseServiceError::Admission { identity, error })?
                {
                    crate::rel99_admission::ReleaseFinish::Pending => Ok(None),
                    crate::rel99_admission::ReleaseFinish::Complete(owner) => Ok(owner),
                }
            })();
        });
        result
    }

    pub fn record_core_release_ready(
        &mut self,
        request: &mut crate::rel99_admission::RabReleaseRequest,
        now: umts_iuh::session::ControlTime,
        budget: std::num::NonZeroUsize,
    ) -> Result<usize, crate::rel99_admission::CoreReleaseError> {
        request.record_ready(&mut self.core, now, budget)
    }
}

pub(crate) enum IuReleaseLaunch {
    Pending,
    NoRadio,
    Scheduled(crate::dedicated_rx::ReceiveIdentity),
}

#[derive(Debug)]
pub enum IuCompletionError {
    Radio(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::signalling::SignallingError,
    ),
    Core(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_core::Error,
    ),
}
