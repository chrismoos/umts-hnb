// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    rab_handoff::PendingRabAssignment,
    rel99_planner::{ConfiguredPolicy, InitialOwnerSettings, OwnerBuildError, PreparedRelease},
};
use umts_core::{Gateway, Transport};

#[derive(Debug)]
pub enum ReleaseAdmissionError {
    HsupaRestoration(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        String,
    ),
    Core(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        CoreReleaseError,
    ),
    Planning(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::rel99_planner::Error,
    ),
    Owners(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        OwnerBuildError,
    ),
    Admission(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdmissionError,
    ),
}

#[derive(Debug)]
pub enum ReleaseRollbackError {
    Pool(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::dedicated_bearers::FramePoolError,
    ),
    Resources(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::radio_resources::Error,
    ),
}

pub(crate) struct RetainedReleaseBuild {
    pub error: ReleaseAdmissionError,
    pub pool: Option<FramePool>,
    pub change: Option<PreparedRadioChange>,
}

impl RetainedReleaseBuild {
    pub(crate) fn rollback(
        &mut self,
        radio: &mut CommonSignalling,
        resources: &mut RadioResources,
    ) -> Result<(), ReleaseRollbackError> {
        if let Some(pool) = self.pool.take()
            && let Err(rejected) = radio.retire_frame_pool(pool)
        {
            self.pool = Some(*rejected.pool);
            return Err(ReleaseRollbackError::Pool(rejected.error));
        }
        if let Some(change) = self.change.take()
            && let Err(rejected) = resources.cancel_reconfiguration(change)
        {
            self.change = Some(rejected.change);
            return Err(ReleaseRollbackError::Resources(rejected.error));
        }
        Ok(())
    }
}

#[must_use = "Retain the original request and unpublished owners until rollback completes"]
pub struct RejectedReleaseAssignment {
    pub error: ReleaseAdmissionError,
    pub pending: PendingRabAssignment,
    pub pool: Option<FramePool>,
    pub change: Option<PreparedRadioChange>,
    pub rollback_error: Option<ReleaseRollbackError>,
}

impl std::fmt::Debug for RejectedReleaseAssignment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedReleaseAssignment")
            .field("error", &self.error)
            .field("request", &self.pending.request())
            .field("rollback_error", &self.rollback_error)
            .finish_non_exhaustive()
    }
}

impl RejectedReleaseAssignment {
    pub fn rollback(
        &mut self,
        radio: &mut CommonSignalling,
        resources: &mut RadioResources,
    ) -> Result<(), ReleaseRollbackError> {
        if let Some(pool) = self.pool.take()
            && let Err(rejected) = radio.retire_frame_pool(pool)
        {
            self.pool = Some(*rejected.pool);
            return Err(ReleaseRollbackError::Pool(rejected.error));
        }
        if let Some(change) = self.change.take()
            && let Err(rejected) = resources.cancel_reconfiguration(change)
        {
            self.change = Some(rejected.change);
            return Err(ReleaseRollbackError::Resources(rejected.error));
        }
        self.rollback_error = None;
        Ok(())
    }
}

#[must_use]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub enum ReleaseLaunchResult {
    Scheduled(ReceiveIdentity),
    OutcomeOnly(RabReleaseRequest),
}

pub struct ReleaseRabHost<'a, C: Transport> {
    pub gateway: &'a Gateway<C>,
    pub policy: &'a ConfiguredPolicy,
    pub radio: &'a mut CommonSignalling,
    pub resources: &'a mut RadioResources,
    pub receive: &'a mut DedicatedController,
    pub admission: &'a mut Rel99Admission,
}

pub(crate) struct ConstructedRelease {
    pub request: Box<ScheduledReleaseRequest>,
    pub delivery: ReleaseDelivery,
    pub feedback: Option<crate::rel99_admission::hsdpa::PreparedFeedback>,
}

impl<C: Transport> ReleaseRabHost<'_, C> {
    pub fn begin_with_delivery(
        &mut self,
        previous: ReceiveIdentity,
        pending: PendingRabAssignment,
        launch: crate::rel99_admission::AdditionalLaunch,
        settings: impl FnOnce(
            &PreparedRelease,
            &ReleaseDelivery,
        ) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ReleaseLaunchResult, Box<RejectedReleaseAssignment>> {
        let authority = match self
            .admission
            .prepare_core_release(previous, pending, self.gateway)
        {
            Ok(authority) => authority,
            Err(rejected) => {
                return Err(self.reject(
                    ReleaseAdmissionError::Core(rejected.error),
                    *rejected.pending,
                    None,
                    None,
                ));
            }
        };
        if !authority.needs_radio() {
            return Ok(ReleaseLaunchResult::OutcomeOnly(authority));
        }
        let ConstructedRelease {
            request,
            delivery,
            feedback,
        } = match self.construct_release(previous, authority.radio_bearers(), launch, settings) {
            Ok(value) => value,
            Err(rejected) => {
                return Err(self.reject(
                    rejected.error,
                    authority.into_pending(),
                    rejected.pool,
                    rejected.change,
                ));
            }
        };
        match self.admission.begin_core_release(
            request,
            authority,
            self.gateway,
            self.radio,
            self.receive,
        ) {
            Ok(identity) => {
                self.admission.commit_hsdpa_feedback(feedback);
                self.admission
                    .entry_mut(previous)
                    .expect("accepted release owner")
                    .lifecycle
                    .release_mut()
                    .expect("scheduled release")
                    .delivery = Some(delivery);
                Ok(ReleaseLaunchResult::Scheduled(identity))
            }
            Err(rejected) => {
                let ScheduledReleaseRequest { pool, change, .. } = *rejected.request;
                Err(self.reject(
                    ReleaseAdmissionError::Core(rejected.error),
                    rejected.authority.into_pending(),
                    Some(pool),
                    change,
                ))
            }
        }
    }

    pub(crate) fn construct_release(
        &mut self,
        previous: ReceiveIdentity,
        bearers: &[crate::dedicated_bearers::BearerId],
        launch: crate::rel99_admission::AdditionalLaunch,
        settings: impl FnOnce(
            &PreparedRelease,
            &ReleaseDelivery,
        ) -> Result<InitialOwnerSettings, OwnerBuildError>,
    ) -> Result<ConstructedRelease, Box<RetainedReleaseBuild>> {
        let mut change = None;
        let built = (|| {
            let active_rabs = self.admission.active_rabs(previous).ok_or_else(|| {
                ReleaseAdmissionError::Planning(crate::rel99_planner::trace_identity(
                    "release-host-no-active-rabs",
                ))
            })?;
            let active = self
                .admission
                .active_radio(previous)
                .map_err(ReleaseAdmissionError::Admission)?;
            let planned = crate::rel99_planner::plan_release_for_ue(
                &active,
                self.radio,
                &active_rabs,
                bearers,
                &self.policy.policy(),
            )
            .map_err(ReleaseAdmissionError::Planning)?;
            let activation = self
                .admission
                .provisional_release(previous, planned.compiled(), self.receive, launch)
                .map_err(ReleaseAdmissionError::Admission)?;
            let mut prepared =
                match planned.allocate(active, self.radio, self.resources, activation) {
                    Ok(prepared) => prepared,
                    Err(rejected) => {
                        change = rejected.change;
                        return Err(ReleaseAdmissionError::Planning(rejected.error));
                    }
                };
            let settings_result = (|| {
                let delivery = self
                    .admission
                    .plan_release_delivery(
                        previous,
                        &mut prepared,
                        self.radio,
                        self.receive,
                        launch,
                    )
                    .map_err(ReleaseAdmissionError::Admission)?;
                let settings =
                    settings(&prepared, &delivery).map_err(ReleaseAdmissionError::Owners)?;
                Ok((delivery, settings))
            })();
            let (delivery, settings) = match settings_result {
                Ok(value) => value,
                Err(error) => {
                    change = Some(prepared.change);
                    return Err(error);
                }
            };
            match self
                .admission
                .build_release_owners(previous, prepared, self.radio, settings)
            {
                Ok(built) => Ok((built.request, delivery)),
                Err(rejected) => {
                    change = Some(rejected.prepared.change);
                    Err(ReleaseAdmissionError::Owners(rejected.error))
                }
            }
        })();
        let (mut request, delivery) = built.map_err(|error| {
            Box::new(RetainedReleaseBuild {
                error,
                pool: None,
                change,
            })
        })?;
        if !delivery.matches(request.procedure.encoded()) {
            let ScheduledReleaseRequest { pool, change, .. } = *request;
            return Err(Box::new(RetainedReleaseBuild {
                error: ReleaseAdmissionError::Admission(AdmissionError::Configuration),
                pool: Some(pool),
                change,
            }));
        }
        let feedback = match self
            .admission
            .prepare_hsdpa_feedback(request.pool.identity(), |capacity, budget| {
                request.receiver.configure_hsdpa_feedback(capacity, budget)
            }) {
            Ok(feedback) => feedback,
            Err(error) => {
                let ScheduledReleaseRequest { pool, change, .. } = *request;
                return Err(Box::new(RetainedReleaseBuild {
                    error: ReleaseAdmissionError::Owners(OwnerBuildError::Receive(error)),
                    pool: Some(pool),
                    change,
                }));
            }
        };
        Ok(ConstructedRelease {
            request,
            delivery,
            feedback,
        })
    }

    fn reject(
        &mut self,
        error: ReleaseAdmissionError,
        pending: PendingRabAssignment,
        pool: Option<FramePool>,
        change: Option<PreparedRadioChange>,
    ) -> Box<RejectedReleaseAssignment> {
        let mut rejected = Box::new(RejectedReleaseAssignment {
            error,
            pending,
            pool,
            change,
            rollback_error: None,
        });
        rejected.rollback_error = rejected.rollback(self.radio, self.resources).err();
        rejected
    }
}
