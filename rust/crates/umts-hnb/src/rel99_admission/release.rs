// SPDX-License-Identifier: AGPL-3.0-or-later

use super::transition_frames::{FrameLane, TransitionStep, schedule_frame};
use super::*;
use crate::{
    dedicated_bearers::{MappingActivation, MappingRelease, ReleaseOutputPolicy},
    dedicated_controller::DedicatedReleaseError,
    dedicated_rx::{DedicatedSampleReceiver, ReceiveCommandResult, ReceiveCommandTicket},
    radio_resources::{PreparedRadioChange, RadioResources, RetiredRadioCodes},
    signalling::{AcceptedBearerRelease, BearerReleaseCommitError},
};
use umts_rrc::dedicated::{
    RadioBearerReleaseProcedure, ReconfigurationPhase, ReconfigurationResponse,
};
mod authority;
mod core;

use authority::ReleaseOwner;
mod delivery;
mod host;
mod media;
#[cfg(test)]
pub use core::ReleaseOutcomeProgress;
pub use core::{CoreReleaseError, RabReleaseRequest, RejectedRabRelease, SetupAfterRelease};
pub use delivery::ReleaseDelivery;
pub(crate) use host::RetainedReleaseBuild;
pub use host::{
    RejectedReleaseAssignment, ReleaseAdmissionError, ReleaseLaunchResult, ReleaseRabHost,
    ReleaseRollbackError,
};
#[cfg(test)]
pub(crate) use media::TestReleaseServices;
pub use media::{MediaReleaseError, ReleaseServices};
pub(super) enum Authority<'a> {
    Initial(&'a crate::rab_handoff::PendingRabAssignment),
    Additional(&'a crate::rab_handoff::PendingRabAssignment),
    Release(&'a ReleaseOwner),
}
impl Authority<'_> {
    pub(super) fn authorized<C: umts_core::Transport>(
        &self,
        gateway: &umts_core::Gateway<C>,
    ) -> bool {
        match self {
            Self::Initial(pending) | Self::Additional(pending) => {
                pending.assignment().setup.iter().all(|item| {
                    gateway
                        .authorize_rab(
                            pending.request(),
                            item.first.id.get(),
                            umts_iuh::session::RabOperation::SetupOrModify,
                        )
                        .is_ok()
                })
            }
            Self::Release(request) => request.authorized(gateway),
        }
    }
}

pub struct ScheduledReleaseRequest {
    pub receiver: Box<DedicatedSampleReceiver>,
    pub transmit: Box<DedicatedTransmitSession>,
    pub procedure: Box<RadioBearerReleaseProcedure>,
    pub pool: FramePool,
    pub activation: MappingActivation,
    pub change: Option<PreparedRadioChange>,
}
pub struct RejectedScheduledRelease {
    pub error: AdmissionError,
    pub request: Box<ScheduledReleaseRequest>,
}
impl std::fmt::Debug for RejectedScheduledRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedScheduledRelease")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduledReleasePhase {
    Preparing,
    Ready,
    Switching,
    AwaitingComplete,
    Draining,
    ReadyToCommit,
    Committed,
    RecoveryRequired,
}
#[must_use = "Retain the original completed request until every remaining setup item is resolved"]
pub enum ReleaseFinish {
    Pending,
    Complete(Option<RabReleaseRequest>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg(test)]
pub struct ReleaseProgress {
    pub phase: ScheduledReleasePhase,
    pub previous_pool_pending: bool,
    pub receipt_released: bool,
    pub resources_retired: bool,
    pub outcomes_recorded: bool,
    pub tail_resolved: bool,
    pub tail_air_end: Option<ChipInstant>,
}
enum ReceiveAction {
    Prepare,
    Commit,
    Retire,
}
enum ReleaseResources {
    Unmanaged,
    Prepared(PreparedRadioChange),
    Retiring(RetiredRadioCodes),
    Retired,
}
pub(super) struct CompletedRelease {
    procedure: Box<RadioBearerReleaseProcedure>,
    accepted: AcceptedBearerRelease,
    iu: Option<umts_iuh::session::IuReleaseRequest>,
}
pub(super) struct ScheduledRelease {
    delivery: Option<ReleaseDelivery>,
    pub(super) core: Option<ReleaseOwner>,
    identity: ReceiveIdentity,
    pub(super) phase: ScheduledReleasePhase,
    procedure: Box<RadioBearerReleaseProcedure>,
    activation: MappingActivation,
    receiver: Option<Box<DedicatedSampleReceiver>>,
    transmit: Option<Box<DedicatedTransmitSession>>,
    pool: Option<FramePool>,
    frame: Option<FrameInput>,
    next_frame: ChipInstant,
    idle: Tfci,
    prepared_frames: usize,
    receive_pending: Option<(ReceiveAction, ReceiveCommandTicket)>,
    receive_prepared: bool,
    transmit_prepared: bool,
    receive_committed: bool,
    transmit_committed: bool,
    receive_retired: Option<ReceiveCommandResult>,
    unclaimed_receive: Option<ReceiveCommandResult>,
    transmit_retired: Option<TransmitCommandOutcome>,
    accepted: Option<AcceptedBearerRelease>,
    failure: Option<AdmissionError>,
    previous_pool: Option<FramePool>,
    receipt_released: bool,
    resources: ReleaseResources,
}
impl ScheduledRelease {
    pub(super) fn uplink_handoff_pending(&self, identity: ReceiveIdentity) -> bool {
        self.identity == identity
            && matches!(
                self.phase,
                ScheduledReleasePhase::Switching
                    | ScheduledReleasePhase::AwaitingComplete
                    | ScheduledReleasePhase::Draining
                    | ScheduledReleasePhase::ReadyToCommit
            )
    }

    fn fail(&mut self, error: AdmissionError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.phase = ScheduledReleasePhase::RecoveryRequired;
    }
}

pub(super) fn failed_request(entry: &Entry) -> Option<umts_iuh::session::RabRequest> {
    entry
        .lifecycle
        .release()?
        .core
        .as_ref()
        .and_then(ReleaseOwner::rab)
        .map(RabReleaseRequest::request)
}
#[cfg(test)]
pub(super) fn quarantined_custody(entry: &Entry) -> (usize, usize, bool) {
    entry.lifecycle.release().map_or((0, 0, false), |release| {
        (
            usize::from(release.receive_retired.is_some())
                + usize::from(release.unclaimed_receive.is_some()),
            usize::from(release.receive_pending.is_some()),
            matches!(release.resources, ReleaseResources::Prepared(_)),
        )
    })
}
pub(super) fn extract_failed(
    entry: &mut Entry,
) -> Result<Option<super::recovery::JoinedRecovery>, AdmissionError> {
    use super::recovery::{Candidate, FailedProcedure, JoinedRecovery, Kind, Mode};
    let Some(release) = entry.lifecycle.release() else {
        return Ok(None);
    };
    if (release.phase != ScheduledReleasePhase::RecoveryRequired && entry.failure.is_none())
        || entry.pending.is_some()
        || release.receive_pending.is_some()
    {
        return Ok(None);
    }
    if release.unclaimed_receive.is_some() {
        return Err(AdmissionError::Protocol);
    }
    let promoted = entry.identity == release.identity && release.accepted.is_some();
    if promoted
        && (release.receive_retired.is_none()
            || !matches!(
                release.transmit_retired,
                Some(TransmitCommandOutcome::Retired { result: Ok(_), .. })
            ))
    {
        return Err(AdmissionError::Phase);
    }
    if promoted
        && !matches!(
            release.resources,
            ReleaseResources::Retiring(_) | ReleaseResources::Retired | ReleaseResources::Unmanaged
        )
        || !promoted
            && !matches!(
                release.resources,
                ReleaseResources::Prepared(_) | ReleaseResources::Unmanaged
            )
    {
        return Err(AdmissionError::Phase);
    }
    let mut release = entry.lifecycle.take_release().expect("failed release");
    if promoted {
        let Some(TransmitCommandOutcome::Retired {
            identity: retired_identity,
            result: Ok(transmit),
        }) = release.transmit_retired.take()
        else {
            unreachable!("validated retirement")
        };
        let retired_codes = match release.resources {
            ReleaseResources::Retiring(proof) => Some(proof),
            ReleaseResources::Retired | ReleaseResources::Unmanaged => None,
            _ => unreachable!("promoted resource owner"),
        };
        return Ok(Some(JoinedRecovery::new(Candidate {
            previous: entry.identity,
            identity: retired_identity,
            mode: Mode::CurrentBankTeardown,
            receiver: None,
            returned_receiver: release.receive_retired.take(),
            original_receiver: None,
            transmitter: Some(transmit.session),
            pool: release.previous_pool.take(),
            frame: None,
            change: None,
            retired_codes,
            release: release
                .core
                .take()
                .and_then(ReleaseOwner::into_rab)
                .map(Box::new),
            pending: None,
            procedure: FailedProcedure::Release(release.procedure),
            kind: Kind::Release,
            error: release
                .failure
                .or_else(|| entry.failure.take())
                .unwrap_or(AdmissionError::Protocol),
        })));
    }
    let mode = if entry.failure.is_none()
        && release.procedure.receipt().is_none()
        && !release.receive_committed
        && !release.transmit_committed
    {
        Mode::Unpublished
    } else {
        Mode::Published
    };
    if let Some(TransmitCommandOutcome::Retired {
        result: Ok(retired),
        ..
    }) = release.transmit_retired.take()
    {
        entry.transmit = TransmitOwnership::Returned(retired.session);
    }
    let change = match release.resources {
        ReleaseResources::Prepared(change) => Some(change),
        ReleaseResources::Unmanaged => None,
        _ => unreachable!("validated resource handoff"),
    };
    Ok(Some(JoinedRecovery::new(Candidate {
        previous: entry.identity,
        identity: release.identity,
        mode,
        receiver: release.receiver.take(),
        returned_receiver: None,
        original_receiver: release.receive_retired.take(),
        transmitter: release.transmit.take(),
        pool: release.pool.take(),
        frame: release.frame.take(),
        change,
        retired_codes: None,
        release: None,
        pending: release
            .core
            .take()
            .and_then(ReleaseOwner::into_rab)
            .map(RabReleaseRequest::into_pending),
        procedure: FailedProcedure::Release(release.procedure),
        kind: Kind::Release,
        error: release.failure.unwrap_or(AdmissionError::Protocol),
    })))
}
impl Rel99Admission {
    #[cfg(test)]
    pub fn allocate_release_pool(
        &self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        plan: Arc<crate::rel99_plan::CompiledRel99>,
        radio: &mut CommonSignalling,
        limits: crate::dedicated_bearers::FramePoolLimits,
    ) -> Result<FramePool, SignallingError> {
        let entry = self.entry(previous).ok_or(SignallingError::Mapping)?;
        if entry.lifecycle.phase() != AdmissionPhase::Active
            || !entry.lifecycle.is_idle()
            || identity.ue != previous.ue
            || identity.configuration <= previous.configuration
        {
            return Err(SignallingError::Phase);
        }
        radio.allocate_frame_pool(identity, plan, &self.port, limits)
    }
    pub fn release_phase(&self, previous: ReceiveIdentity) -> Option<ScheduledReleasePhase> {
        Some(self.entry(previous)?.lifecycle.release()?.phase)
    }
    #[cfg(test)]
    pub fn release_progress(&self, identity: ReceiveIdentity) -> Option<ReleaseProgress> {
        let release = self.entry(identity)?.lifecycle.release()?;
        let tail = match release.transmit_retired.as_ref() {
            Some(TransmitCommandOutcome::Retired {
                result: Ok(retired),
                ..
            }) => Some(&retired.submission),
            _ => None,
        };
        Some(ReleaseProgress {
            phase: release.phase,
            previous_pool_pending: release.previous_pool.is_some(),
            receipt_released: release.receipt_released,
            resources_retired: matches!(
                release.resources,
                ReleaseResources::Unmanaged | ReleaseResources::Retired
            ),
            outcomes_recorded: release
                .core
                .as_ref()
                .is_none_or(ReleaseOwner::outcomes_recorded),
            tail_resolved: tail.is_some_and(|tail| tail.resolved()),
            tail_air_end: tail.map(|tail| tail.air_end()),
        })
    }
    pub fn release_failure(&self, previous: ReceiveIdentity) -> Option<&AdmissionError> {
        self.entry(previous)?.lifecycle.release()?.failure.as_ref()
    }
    #[cfg(test)]
    pub fn release_procedure(
        &self,
        identity: ReceiveIdentity,
    ) -> Option<&RadioBearerReleaseProcedure> {
        Some(&self.entry(identity)?.lifecycle.release()?.procedure)
    }
    /// now is elapsed lower-bound radio time, not a TX preparation cursor.
    #[cfg(test)]
    pub fn finish_release(
        &mut self,
        identity: ReceiveIdentity,
        resources: &mut RadioResources,
        now: ChipInstant,
    ) -> Result<bool, AdmissionError> {
        if self
            .entry(identity)
            .and_then(|entry| entry.lifecycle.release())
            .and_then(|release| release.core.as_ref())
            .and_then(ReleaseOwner::rab)
            .is_some_and(RabReleaseRequest::has_setup)
        {
            return Err(AdmissionError::Phase);
        }
        self.finish_release_request(identity, resources, now)
            .map(|result| matches!(result, ReleaseFinish::Complete(_)))
    }

    /// now is elapsed lower-bound radio time. The completed original core request remains owned by the caller.
    pub fn finish_release_request(
        &mut self,
        identity: ReceiveIdentity,
        resources: &mut RadioResources,
        now: ChipInstant,
    ) -> Result<ReleaseFinish, AdmissionError> {
        let entry = self.entry_mut(identity).ok_or(AdmissionError::Identity)?;
        let release = entry.lifecycle.release_mut().ok_or(AdmissionError::Phase)?;
        if release.phase != ScheduledReleasePhase::Committed
            || now.epoch != identity.ue.epoch
            || release.failure.is_some()
        {
            return Err(AdmissionError::Phase);
        }
        let Some(TransmitCommandOutcome::Retired {
            result: Ok(retired),
            ..
        }) = release.transmit_retired.as_ref()
        else {
            return Err(AdmissionError::Phase);
        };
        if !retired.submission.viable() {
            return Err(AdmissionError::Protocol);
        }
        if retired.submission.air_end().epoch != now.epoch {
            return Err(AdmissionError::Identity);
        }
        if !retired.submission.resolved()
            || now.index < retired.submission.air_end().index
            || release.previous_pool.is_some()
            || !release.receipt_released
        {
            return Ok(ReleaseFinish::Pending);
        }
        match std::mem::replace(&mut release.resources, ReleaseResources::Retired) {
            ReleaseResources::Retiring(proof) => {
                if let Err(rejected) = resources.retire_reconfiguration_codes(proof) {
                    release.resources = ReleaseResources::Retiring(rejected.proof);
                    return Err(AdmissionError::Resources(rejected.error));
                }
            }
            prepared @ ReleaseResources::Prepared(_) => {
                release.resources = prepared;
                return Err(AdmissionError::Phase);
            }
            ReleaseResources::Retired | ReleaseResources::Unmanaged => (),
        }
        if release
            .core
            .as_ref()
            .is_some_and(|core| !core.outcomes_recorded())
        {
            return Ok(ReleaseFinish::Pending);
        }
        let release = entry
            .lifecycle
            .take_release()
            .expect("completed release owner");
        let core = release.core;
        entry.release_history = Some(CompletedRelease {
            procedure: release.procedure,
            accepted: release.accepted.expect("completed release proof"),
            iu: core.as_ref().and_then(ReleaseOwner::iu),
        });
        Ok(ReleaseFinish::Complete(
            core.and_then(ReleaseOwner::into_rab),
        ))
    }
    #[cfg(test)]
    pub fn begin_release(
        &mut self,
        previous: ReceiveIdentity,
        request: Box<ScheduledReleaseRequest>,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedScheduledRelease> {
        self.begin_release_inner(previous, request, radio, receive, None)
    }
    fn begin_release_inner(
        &mut self,
        previous: ReceiveIdentity,
        request: Box<ScheduledReleaseRequest>,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
        core: Option<&ReleaseOwner>,
    ) -> Result<ReceiveIdentity, RejectedScheduledRelease> {
        let check = (|| {
            let entry = self.entry(previous).ok_or(AdmissionError::Identity)?;
            let identity = request.pool.identity();
            if entry.lifecycle.phase() != AdmissionPhase::Active
                || !entry.lifecycle.is_idle()
                || entry.failure.is_some()
                || (core.is_none()
                    && (entry.circuit_assignment.is_some()
                        || entry.packet_assignment.is_some()
                        || entry.rab_assignment.is_some()))
                || !matches!(entry.transmit, TransmitOwnership::Armed)
                || request.procedure.phase() != ReconfigurationPhase::Prepared
                || request.transmit.phase() != TransmitPhase::Prepared
                || request.transmit.queued_frames() != 0
            {
                return Err(AdmissionError::Phase);
            }
            if identity.ue != previous.ue
                || identity.configuration <= previous.configuration
                || request.receiver.session().identity() != identity
                || request.transmit.identity() != identity
                || request.procedure.key() != identity.ue
                || radio.dedicated_route(previous.ue) != Some(previous)
                || !radio.owns_frame_pool(&request.pool)
                || !request.pool.bound_to(&self.port)
            {
                return Err(AdmissionError::Identity);
            }
            let old = entry.pool.as_ref().ok_or(AdmissionError::Phase)?;
            let plan = request.pool.plan();
            if !Arc::ptr_eq(request.procedure.previous(), old.plan().configuration())
                || !Arc::ptr_eq(request.procedure.configuration(), plan.configuration())
                || !Arc::ptr_eq(request.transmit.plan(), plan)
                || !Arc::ptr_eq(
                    request.receiver.session().configured(),
                    plan.uplink_configured(),
                )
            {
                return Err(AdmissionError::Configuration);
            }
            match (&entry.resources, &request.change) {
                (Some(active), Some(change))
                    if active.receive_identity() == previous
                        && change.receive_identity() == identity
                        && Arc::ptr_eq(change.plan(), plan) => {}
                (None, None) => (),
                _ => return Err(AdmissionError::Configuration),
            }
            let activation = request.activation;
            let tx = request.transmit.frame_anchor();
            let rx = request.receiver.session().frame_anchor();
            if activation.downlink.time != tx.time
                || activation.downlink.cfn != tx.cfn
                || activation.uplink.time != rx.time
                || activation.uplink.cfn != rx.cfn
                || request.procedure.activation().time() != tx.time
                || entry.next_frame.epoch != tx.time.epoch
                || entry.next_frame.index > tx.time.index
                || entry.frame.as_ref().is_some_and(|input| {
                    input
                        .start
                        .index
                        .checked_add(CHIPS_PER_FRAME)
                        .is_none_or(|end| end > tx.time.index)
                })
            {
                return Err(AdmissionError::Time);
            }
            let longest = plan
                .downlink_configured()
                .transport_ids()
                .map(|id| {
                    plan.downlink_configured()
                        .definition(id)
                        .expect("configured transport")
                        .interval
                        .frames()
                })
                .max()
                .ok_or(AdmissionError::Configuration)?;
            if self.limits.prefill_frames.get() < longest {
                return Err(AdmissionError::Configuration);
            }
            let idle = plan
                .downlink_configured()
                .combinations()
                .iter()
                .find(|row| {
                    plan.downlink_configured()
                        .transport_ids()
                        .zip(&row.formats)
                        .all(|(id, index)| {
                            let format = plan
                                .downlink_configured()
                                .definition(id)
                                .expect("configured transport")
                                .formats[*index];
                            format.blocks == 0 || format.block_bits == 0
                        })
                })
                .map(|row| row.tfci)
                .ok_or(AdmissionError::Configuration)?;
            let mapping = receive
                .prepare_release_mapping(previous, identity, activation, plan.clone())
                .map_err(AdmissionError::Receive)?;
            Ok((identity, idle, mapping))
        })();
        let (identity, idle, mapping) = match check {
            Ok(value) => value,
            Err(error) => return Err(RejectedScheduledRelease { error, request }),
        };
        if let Err(rejected) = receive.stage_mapping(mapping) {
            return Err(RejectedScheduledRelease {
                error: AdmissionError::Receive(DedicatedControlError::Bearer(rejected.error)),
                request,
            });
        }
        let ScheduledReleaseRequest {
            receiver,
            transmit,
            procedure,
            pool,
            activation,
            change,
        } = *request;
        *self
            .entry_mut(previous)
            .expect("checked active admission")
            .lifecycle
            .transition_mut()
            .expect("active bearer change owner") =
            Transition::Release(Box::new(ScheduledRelease {
                core: None,
                delivery: None,
                identity,
                phase: ScheduledReleasePhase::Preparing,
                procedure,
                activation,
                receiver: Some(receiver),
                transmit: Some(transmit),
                pool: Some(pool),
                frame: None,
                next_frame: activation.downlink.time,
                idle,
                prepared_frames: 0,
                receive_pending: None,
                receive_prepared: false,
                transmit_prepared: false,
                receive_committed: false,
                transmit_committed: false,
                receive_retired: None,
                unclaimed_receive: None,
                transmit_retired: None,
                accepted: None,
                failure: None,
                previous_pool: None,
                receipt_released: false,
                resources: change.map_or(ReleaseResources::Unmanaged, ReleaseResources::Prepared),
            }));
        Ok(identity)
    }
    pub(super) fn release_response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
    ) -> Option<Result<(), SignallingError>> {
        let entry = self
            .entries
            .get_mut(received.key().slot as usize)?
            .as_mut()
            .filter(|entry| entry.identity.ue == received.key())?;
        if let Some(history) = entry.release_history.as_mut()
            && received
                .bearer_release_duplicate(&mut history.procedure, &history.accepted)
                .is_ok()
        {
            return Some(Ok(()));
        }
        let Some(release) = entry.lifecycle.release_mut() else {
            let history = entry.release_history.as_mut()?;
            return Some(
                received.bearer_release_duplicate(&mut history.procedure, &history.accepted),
            );
        };
        if release.phase == ScheduledReleasePhase::Committed {
            return Some(received.bearer_release_duplicate(
                &mut release.procedure,
                release.accepted.as_ref().expect("committed release"),
            ));
        }
        if release.failure.is_some() {
            return Some(Err(SignallingError::Phase));
        }
        if matches!(
            received.message().value.message,
            umts_rrc_schema::class_definitions::ULDCCHMessageType::radioBearerReleaseComplete(_)
        ) && !matches!(
            release.phase,
            ScheduledReleasePhase::AwaitingComplete
                | ScheduledReleasePhase::Draining
                | ScheduledReleasePhase::ReadyToCommit
                | ScheduledReleasePhase::Committed
        ) {
            return Some(Err(SignallingError::Capacity));
        }
        Some(
            received
                .bearer_release_response_with(&mut release.procedure, security, |counters, _| {
                    if counters.transparent_cfn().is_some() {
                        return Err(SignallingError::Mapping);
                    }
                    Ok(())
                })
                .map(|(response, accepted)| match response {
                    ReconfigurationResponse::Complete => {
                        release.accepted = accepted.map(|(proof, ())| proof);
                        release.phase = ScheduledReleasePhase::Draining;
                    }
                    ReconfigurationResponse::Failure => release.fail(AdmissionError::Protocol),
                    ReconfigurationResponse::Duplicate => (),
                }),
        )
    }
    #[cfg(test)]
    pub fn commit_release_with<R, E>(
        &mut self,
        previous: ReceiveIdentity,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        output: ReleaseOutputPolicy,
        cleanup: impl FnOnce(&[crate::dedicated_bearers::BearerId]) -> Result<R, E>,
    ) -> Result<MappingRelease<R>, BearerReleaseCommitError<DedicatedReleaseError<E>>> {
        self.commit_release_with_view(previous, radio, receive, output, |released, _, _| {
            cleanup(released)
        })
    }
    #[cfg(test)]
    pub(crate) fn commit_release_with_view<R, E>(
        &mut self,
        previous: ReceiveIdentity,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        output: ReleaseOutputPolicy,
        cleanup: impl FnOnce(
            &[crate::dedicated_bearers::BearerId],
            &AcceptedBearerRelease,
            &crate::dedicated_bearers::DedicatedBearers,
        ) -> Result<R, E>,
    ) -> Result<MappingRelease<R>, BearerReleaseCommitError<DedicatedReleaseError<E>>> {
        let entry = self.entry_mut(previous).ok_or_else(phase_error)?;
        if entry.circuit_assignment.is_some()
            || entry.packet_assignment.is_some()
            || entry.rab_assignment.is_some()
            || !entry.additional_assignments.is_empty()
            || entry
                .lifecycle
                .release()
                .is_some_and(|release| release.core.is_some())
        {
            return Err(phase_error());
        }
        commit_entry_with_view(
            entry,
            None,
            radio,
            receive,
            output,
            |released, accepted, bank, _, _, _, _| cleanup(released, accepted, bank),
        )
    }
}
fn phase_error<E>() -> BearerReleaseCommitError<DedicatedReleaseError<E>> {
    BearerReleaseCommitError::Commit(DedicatedReleaseError::Control(DedicatedControlError::Phase))
}
fn commit_entry_with_view<R, E>(
    entry: &mut Entry,
    mut resources: Option<&mut RadioResources>,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
    output: ReleaseOutputPolicy,
    cleanup: impl FnOnce(
        &[crate::dedicated_bearers::BearerId],
        &AcceptedBearerRelease,
        &crate::dedicated_bearers::DedicatedBearers,
        &mut Option<CircuitAssignment>,
        &mut Option<PacketAssignment>,
        &mut Vec<AdditionalAssignment>,
        &mut dyn FnMut() -> Result<(), crate::radio_resources::Error>,
    ) -> Result<R, E>,
) -> Result<MappingRelease<R>, BearerReleaseCommitError<DedicatedReleaseError<E>>> {
    let release = entry.lifecycle.release_mut().ok_or_else(phase_error)?;
    if release.phase != ScheduledReleasePhase::ReadyToCommit
        || entry.pending.is_some()
        || entry.frame.is_some()
        || !matches!(
            (&release.resources, &entry.resources, &resources),
            (ReleaseResources::Unmanaged, None, None)
                | (ReleaseResources::Prepared(_), Some(_), Some(_))
        )
    {
        return Err(phase_error());
    }
    let accepted = release.accepted.as_ref().ok_or_else(phase_error)?;
    let rx = release.receive_retired.as_ref().ok_or_else(phase_error)?;
    let tx = release.transmit_retired.as_ref().ok_or_else(phase_error)?;
    let inventory = entry
        .inventory
        .as_mut()
        .map(|inventory| inventory.prepare_release(release.procedure.released()))
        .transpose()
        .map_err(|_| phase_error())?;
    let result = radio.commit_bearer_release_with(accepted, |ciphers| {
        let result = receive.retire_release_mapping_with_view(
            accepted,
            rx,
            tx,
            output,
            ciphers,
            |released, bank| {
                let mut publish = || {
                    let handoff =
                        std::mem::replace(&mut release.resources, ReleaseResources::Retired);
                    match handoff {
                        ReleaseResources::Prepared(change) => {
                            let active = entry.resources.take().expect("validated active lease");
                            match resources
                                .as_mut()
                                .expect("validated resource owner")
                                .commit_reconfiguration(active, change)
                            {
                                Ok(committed) => {
                                    let (lease, _, retired) = committed.into_parts();
                                    entry.resources = Some(lease);
                                    release.resources = ReleaseResources::Retiring(retired);
                                    Ok(())
                                }
                                Err(rejected) => {
                                    entry.resources = Some(rejected.active);
                                    release.resources = ReleaseResources::Prepared(rejected.change);
                                    Err(rejected.error)
                                }
                            }
                        }
                        state => {
                            release.resources = state;
                            Ok(())
                        }
                    }
                };
                cleanup(
                    released,
                    accepted,
                    bank,
                    &mut entry.circuit_assignment,
                    &mut entry.packet_assignment,
                    &mut entry.additional_assignments,
                    &mut publish,
                )
            },
        )?;
        if let Some(removal) = inventory {
            removal.commit();
        }
        release.previous_pool = entry
            .pool
            .replace(release.pool.take().expect("successor pool"));
        entry.identity = release.identity;
        entry.next_frame = release.next_frame;
        entry.frame = release.frame.take();
        entry.idle = release.idle;
        entry.prepared_frames = release.prepared_frames;
        entry.first = None;
        entry.setup_authority = false;
        entry.next_health = release.next_frame;
        release.phase = ScheduledReleasePhase::Committed;
        if let Some(core) = release.core.as_mut() {
            core.committed();
        }
        Ok(result)
    })?;
    Ok(result)
}

pub(super) fn poll_transmit(
    entry: &mut Entry,
    kind: CommandKind,
    outcome: TransmitCommandOutcome,
    backpressure: &mut u64,
) -> Option<TransmitCommandOutcome> {
    let Some(release) = entry.lifecycle.release_mut() else {
        return Some(outcome);
    };
    match (kind, outcome) {
        (_, TransmitCommandOutcome::Closed) => release.fail(AdmissionError::Closed),
        (
            CommandKind::ReplacementPrepare,
            TransmitCommandOutcome::ReplacementPrepared { previous, identity },
        ) if previous == entry.identity && identity == release.identity => {
            release.transmit_prepared = true
        }
        (
            CommandKind::ReplacementPrepare,
            TransmitCommandOutcome::PrepareRejected { error, session },
        ) if session.identity() == release.identity => {
            release.transmit = Some(session);
            release.fail(AdmissionError::Transmit(error));
        }
        (
            CommandKind::ReplacementCommit,
            TransmitCommandOutcome::ReplacementCommitted {
                previous,
                identity,
                result,
            },
        ) if previous == entry.identity && identity == release.identity => match result {
            Ok(anchor)
                if anchor.time == release.activation.downlink.time
                    && anchor.cfn == release.activation.downlink.cfn =>
            {
                release.transmit_committed = true
            }
            Ok(_) => release.fail(AdmissionError::Protocol),
            Err(error) => release.fail(AdmissionError::Transmit(error)),
        },
        (CommandKind::SuccessorFrame, TransmitCommandOutcome::Frame { mut input, result })
            if input.identity == release.identity =>
        {
            if result == Err(TransmitError::Capacity) {
                release.next_frame = input.start;
                release.frame = Some(input);
                *backpressure = backpressure.saturating_add(1);
                return None;
            }
            if result.is_err()
                && let Some(emission) = &mut input.emission
            {
                emission.fail();
            }
            if let Err(input) = release
                .pool
                .as_mut()
                .expect("successor pool")
                .recycle(input)
            {
                release.frame = Some(input);
                release.fail(AdmissionError::Protocol);
            }
            match result {
                Ok(_) => release.prepared_frames += 1,
                Err(error) => release.fail(AdmissionError::Transmit(error)),
            }
        }
        (
            CommandKind::RetirePrevious,
            result @ TransmitCommandOutcome::Retired {
                identity,
                result: Ok(_),
            },
        ) if identity == entry.identity => release.transmit_retired = Some(result),
        (
            CommandKind::RetirePrevious,
            TransmitCommandOutcome::Retired {
                identity,
                result: Err(error),
            },
        ) if identity == entry.identity => {
            if !matches!(
                error,
                TransmitError::Phase | TransmitError::SubmissionPending
            ) {
                release.fail(AdmissionError::Transmit(error));
            }
        }
        (_, outcome) => {
            release.fail(AdmissionError::Protocol);
            return Some(outcome);
        }
    }
    None
}

fn poll_receive(release: &mut ScheduledRelease, previous: ReceiveIdentity) {
    let Some((_, ticket)) = release.receive_pending.as_mut() else {
        return;
    };
    let Some(result) = ticket.try_result() else {
        return;
    };
    let (action, _) = release
        .receive_pending
        .take()
        .expect("completed receive ticket");
    match (action, result) {
        (
            ReceiveAction::Prepare,
            ReceiveCommandResult::ReplacementPrepared {
                previous: old,
                identity,
            },
        ) if previous == old && identity == release.identity => release.receive_prepared = true,
        (ReceiveAction::Prepare, ReceiveCommandResult::InstallRejected { error, receiver }) => {
            release.receiver = Some(receiver);
            release.fail(AdmissionError::Receive(DedicatedControlError::Receive(
                error,
            )));
        }
        (
            ReceiveAction::Commit,
            ReceiveCommandResult::ReplacementCommitted {
                previous: old,
                identity,
                boundary,
            },
        ) if previous == old
            && identity == release.identity
            && boundary.time == release.activation.uplink.time
            && boundary.cfn == release.activation.uplink.cfn =>
        {
            release.receive_committed = true
        }
        (ReceiveAction::Retire, result @ ReceiveCommandResult::Retired { identity, .. })
            if previous == identity =>
        {
            release.receive_retired = Some(result)
        }
        (
            ReceiveAction::Retire,
            ReceiveCommandResult::RetireRejected {
                identity,
                error: crate::dedicated_rx::ReceiveError::Occupied,
            },
        ) if previous == identity => (),
        (_, ReceiveCommandResult::Closed) => release.fail(AdmissionError::Closed),
        (
            _,
            ReceiveCommandResult::CommitRejected { error, .. }
            | ReceiveCommandResult::RetireRejected { error, .. },
        ) => release.fail(AdmissionError::Receive(DedicatedControlError::Receive(
            error,
        ))),
        (_, result) => {
            release.unclaimed_receive = Some(result);
            release.fail(AdmissionError::Protocol);
        }
    }
}

pub(super) fn step_release(
    entry: &mut Entry,
    context: TransitionStep<'_, impl FramePolicy>,
) -> Result<usize, AdmissionError> {
    let TransitionStep {
        now,
        limits,
        port,
        radio,
        receive,
        common,
        policy,
    } = context;
    let release = entry
        .lifecycle
        .release_mut()
        .expect("scheduled release owner");
    poll_receive(release, entry.identity);
    {
        // Bounded progress trace, so a scheduled change that never completes is visible.
        use std::sync::atomic::{AtomicU64, Ordering};
        static CALLS: AtomicU64 = AtomicU64::new(0);
        if CALLS.fetch_add(1, Ordering::Relaxed).is_multiple_of(2000) {
            tracing::debug!(
                "Release progress: previous={:?} identity={:?} phase={:?} receive_prepared={} transmit_prepared={} receive_committed={} transmit_committed={} receive_pending={} receive_retired={} transmit_retired={} pending={}",
                entry.identity,
                release.identity,
                release.phase,
                release.receive_prepared,
                release.transmit_prepared,
                release.receive_committed,
                release.transmit_committed,
                release.receive_pending.is_some(),
                release.receive_retired.is_some(),
                release.transmit_retired.is_some(),
                entry.pending.is_some()
            );
        }
    }
    if !port.available() || radio.phase(entry.identity.ue).is_none() {
        release.fail(AdmissionError::Closed);
    }
    if release.failure.is_some() {
        return Ok(0);
    }
    if release.phase == ScheduledReleasePhase::Committed {
        if let Some(TransmitCommandOutcome::Retired {
            result: Ok(retired),
            ..
        }) = release.transmit_retired.as_mut()
            && let Some(pool) = release.previous_pool.as_mut()
        {
            pool.reclaim_retired(&mut retired.session)
                .map_err(|error| AdmissionError::Frame(FrameBuildError::Pool(error)))?;
        }
        if let Some(pool) = release.previous_pool.take()
            && let Err(rejected) = radio.retire_frame_pool(pool)
        {
            release.previous_pool = Some(*rejected.pool);
            if rejected.error != crate::dedicated_bearers::FramePoolError::Pending {
                return Err(AdmissionError::Frame(FrameBuildError::Pool(rejected.error)));
            }
        }
        let accepted = release.accepted.as_ref().expect("committed completion");
        if !release.receipt_released {
            let receipt = release.procedure.receipt().expect("published release");
            match radio.release_bearer_release_receipt(accepted, receipt) {
                Ok(()) => release.receipt_released = true,
                Err(SignallingError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Signalling(error)),
            }
        }
        if release.receipt_released
            && release.previous_pool.is_none()
            && matches!(
                release.resources,
                ReleaseResources::Unmanaged | ReleaseResources::Retired
            )
            && release.core.is_none()
        {
            let release = entry
                .lifecycle
                .take_release()
                .expect("completed scheduled release");
            entry.release_history = Some(CompletedRelease {
                procedure: release.procedure,
                accepted: release.accepted.expect("completed release proof"),
                iu: None,
            });
        }
        return Ok(0);
    }
    if release
        .procedure
        .expire(now)
        .map_err(AdmissionError::Procedure)?
    {
        release.fail(AdmissionError::Time);
        return Ok(0);
    }
    if release.receive_pending.is_none()
        && let Some(receiver) = release.receiver.take()
    {
        match receive.stage_mapping_receiver(entry.identity, release.identity, receiver) {
            Ok(ticket) => release.receive_pending = Some((ReceiveAction::Prepare, ticket)),
            Err(receiver) => release.receiver = Some(receiver),
        }
    }
    if entry.pending.is_some() {
        return Ok(0);
    }
    if let Some(session) = release.transmit.take() {
        match port.stage_replacement(entry.identity, session) {
            Ok(id) => {
                entry.pending = Some(PendingCommand {
                    id,
                    kind: CommandKind::ReplacementPrepare,
                })
            }
            Err(session) => release.transmit = Some(session),
        }
        return Ok(0);
    }
    if release.receive_prepared
        && release.transmit_prepared
        && release.prepared_frames >= limits.prefill_frames.get()
        && release.phase == ScheduledReleasePhase::Preparing
    {
        release.phase = ScheduledReleasePhase::Ready;
        return Ok(0);
    }
    if release.phase == ScheduledReleasePhase::Ready {
        let ready = delivery::validate_release_publication(
            entry.identity,
            entry.next_frame,
            entry.frame.as_ref(),
            release,
            now,
            radio,
            receive,
        );
        match ready {
            Err(AdmissionError::Signalling(ref error))
                if crate::signalling::scheduling::backpressure(error) => {}
            Err(error) => return Err(error),
            Ok(()) => match radio.publish_bearer_release_on(
                &mut release.procedure,
                entry.identity,
                release.identity,
                now,
            ) {
                Ok(_) => {
                    if release.delivery.is_some() {
                        radio
                            .reserve_transition_delivery(entry.identity)
                            .map_err(AdmissionError::Signalling)?;
                    }
                    release.phase = ScheduledReleasePhase::Switching;
                }
                Err(error) if crate::signalling::scheduling::backpressure(&error) => (),
                Err(error) => return Err(AdmissionError::Signalling(error)),
            },
        }
    }
    let delivery_confirmed = if release.phase == ScheduledReleasePhase::Switching {
        if let Some(delivery) = release.delivery.as_mut() {
            delivery.acknowledgement.poll(
                entry.identity,
                release.procedure.receipt().ok_or(AdmissionError::Phase)?,
                now,
                radio,
            )?
        } else {
            true
        }
    } else {
        false
    };
    if release.phase == ScheduledReleasePhase::Switching && delivery_confirmed {
        if !release.receive_committed && release.receive_pending.is_none() {
            match receive.commit_mapping_receiver(entry.identity, release.identity) {
                Ok(ticket) => release.receive_pending = Some((ReceiveAction::Commit, ticket)),
                Err(DedicatedControlError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Receive(error)),
            }
        }
        if !release.transmit_committed {
            if let Ok(id) = port.commit_replacement(entry.identity, release.identity) {
                entry.pending = Some(PendingCommand {
                    id,
                    kind: CommandKind::ReplacementCommit,
                });
            }
            return Ok(0);
        }
        if release.receive_committed {
            receive
                .commit_mapping(release.identity)
                .map_err(AdmissionError::Receive)?;
            release.phase = ScheduledReleasePhase::AwaitingComplete;
        }
    }
    let horizon = common
        .next_chip()
        .index
        .max(now.index)
        .checked_add(limits.prefill_frames.get() as u64 * CHIPS_PER_FRAME)
        .ok_or(AdmissionError::Time)?;
    if entry.next_frame.index < release.activation.downlink.time.index {
        let id = schedule_frame(
            FrameLane {
                identity: entry.identity,
                pool: entry.pool.as_mut().expect("original pool"),
                frame: &mut entry.frame,
                next: &mut entry.next_frame,
                idle: entry.idle,
                signalling: true,
            },
            now,
            horizon.min(release.activation.downlink.time.index),
            port,
            radio,
            receive,
            policy,
        )?;
        if let Some(id) = id {
            entry.pending = Some(PendingCommand {
                id,
                kind: CommandKind::Frame,
            });
            return Ok(1);
        }
    }
    if release.transmit_prepared {
        let scheduled = release.receive_committed && release.transmit_committed;
        let horizon = if scheduled {
            horizon
        } else {
            release
                .activation
                .downlink
                .time
                .index
                .checked_add(limits.prefill_frames.get() as u64 * CHIPS_PER_FRAME)
                .ok_or(AdmissionError::Time)?
        };
        if let Some(id) = schedule_frame(
            FrameLane {
                identity: release.identity,
                pool: release.pool.as_mut().expect("successor pool"),
                frame: &mut release.frame,
                next: &mut release.next_frame,
                idle: release.idle,
                signalling: scheduled,
            },
            now,
            horizon,
            port,
            radio,
            receive,
            policy,
        )? {
            entry.pending = Some(PendingCommand {
                id,
                kind: CommandKind::SuccessorFrame,
            });
            return Ok(1);
        }
    }
    if release.receive_committed && release.transmit_committed {
        if release.receive_retired.is_none() && release.receive_pending.is_none() {
            match receive.retire_mapping_receiver(entry.identity) {
                Ok(ticket) => release.receive_pending = Some((ReceiveAction::Retire, ticket)),
                Err(DedicatedControlError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Receive(error)),
            }
        }
        if release.transmit_retired.is_none()
            && let Ok(id) = port.retire_submitted(entry.identity)
        {
            entry.pending = Some(PendingCommand {
                id,
                kind: CommandKind::RetirePrevious,
            });
        }
        if release.accepted.is_some()
            && release.receive_retired.is_some()
            && release.transmit_retired.is_some()
        {
            release.phase = ScheduledReleasePhase::ReadyToCommit;
        }
    }
    Ok(0)
}

pub(super) fn fail_release(entry: &mut Entry, error: AdmissionError) {
    if let Some(release) = entry.lifecycle.release_mut() {
        release.fail(error);
    }
}

pub(super) fn blocks_frames(entry: &Entry) -> bool {
    entry.lifecycle.release().is_some_and(|release| {
        release.phase != ScheduledReleasePhase::Committed || release.failure.is_some()
    })
}
