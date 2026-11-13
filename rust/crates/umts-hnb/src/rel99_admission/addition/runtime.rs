// SPDX-License-Identifier: AGPL-3.0-or-later

#[cfg(test)]
use super::media::NoMedia;
use super::media::{CommitFailure, MediaCommit};
use super::workers::ReceiveAction;
use super::*;
use crate::rel99_admission::transition_frames::{FrameLane, TransitionStep, schedule_frame};

pub(in crate::rel99_admission) fn step_addition(
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
    let addition = entry
        .lifecycle
        .addition_mut()
        .expect("scheduled addition owner");
    {
        // Bounded progress trace, so a bearer change that never completes is visible.
        use std::sync::atomic::{AtomicU64, Ordering};
        static CALLS: AtomicU64 = AtomicU64::new(0);
        if CALLS.fetch_add(1, Ordering::Relaxed).is_multiple_of(2000) {
            let receive = match &addition.receive {
                ReceiveProgress::Local(_) => "local",
                ReceiveProgress::Waiting {
                    action: ReceiveAction::Prepare,
                    ..
                } => "waiting-prepare",
                ReceiveProgress::Waiting {
                    action: ReceiveAction::Commit,
                    ..
                } => "waiting-commit",
                ReceiveProgress::Waiting {
                    action: ReceiveAction::Retire,
                    ..
                } => "waiting-retire",
                ReceiveProgress::Prepared => "prepared",
                ReceiveProgress::Scheduled => "scheduled",
                ReceiveProgress::Retired(_) => "retired",
                ReceiveProgress::Unavailable => "unavailable",
            };
            let transmit = match &addition.transmit {
                TransmitProgress::Local(_) => "local",
                TransmitProgress::Preparing => "preparing",
                TransmitProgress::Prepared => "prepared",
                TransmitProgress::Committing => "committing",
                TransmitProgress::Scheduled => "scheduled",
                TransmitProgress::Retiring => "retiring",
                TransmitProgress::Retired(_) => "retired",
                TransmitProgress::Unavailable => "unavailable",
            };
            let transmission = addition
                .procedure
                .receipt()
                .map(|receipt| radio.bearer_setup_transmission(entry.identity.ue, receipt));
            tracing::debug!(
                "Addition progress: previous={:?} identity={:?} phase={:?} receive={receive} transmit={transmit} accepted={} pending={} now={now:?} activation={:?} transmission={transmission:?}",
                entry.identity,
                addition.identity,
                addition.phase,
                addition.accepted.is_some(),
                entry.pending.is_some(),
                addition.procedure.activation(),
            );
        }
    }
    addition.poll_receive(entry.identity);
    if !port.available() || radio.phase(entry.identity.ue).is_none() {
        addition.fail(AdmissionError::Closed);
    }
    if matches!(
        addition.phase,
        AdditionPhase::Committed | AdditionPhase::RecoveryRequired
    ) {
        return Ok(0);
    }
    if addition
        .procedure
        .expire(now)
        .map_err(AdmissionError::Procedure)?
    {
        let transmission = addition
            .procedure
            .receipt()
            .map(|receipt| radio.bearer_setup_transmission(entry.identity.ue, receipt));
        tracing::warn!(
            "Addition expired: previous={:?} identity={:?} now={now:?} activation={:?} transmission={transmission:?} phase={:?}",
            entry.identity,
            addition.identity,
            addition.procedure.activation(),
            addition.phase,
        );
        addition.fail(AdmissionError::Time);
        return Ok(0);
    }
    if matches!(addition.receive, ReceiveProgress::Local(_)) {
        let ReceiveProgress::Local(receiver) =
            std::mem::replace(&mut addition.receive, ReceiveProgress::Unavailable)
        else {
            unreachable!("local receive owner")
        };
        addition.receive =
            match receive.stage_mapping_receiver(entry.identity, addition.identity, receiver) {
                Ok(ticket) => ReceiveProgress::Waiting {
                    action: ReceiveAction::Prepare,
                    ticket,
                },
                Err(receiver) => ReceiveProgress::Local(receiver),
            };
    }
    if entry.pending.is_some() {
        return Ok(0);
    }
    if matches!(addition.transmit, TransmitProgress::Local(_)) {
        let TransmitProgress::Local(session) =
            std::mem::replace(&mut addition.transmit, TransmitProgress::Unavailable)
        else {
            unreachable!("local transmit owner")
        };
        addition.transmit = match port.stage_replacement(entry.identity, session) {
            Ok(id) => {
                entry.pending = Some(PendingCommand {
                    id,
                    kind: CommandKind::ReplacementPrepare,
                });
                TransmitProgress::Preparing
            }
            Err(session) => TransmitProgress::Local(session),
        };
        return Ok(0);
    }
    if addition.phase == AdditionPhase::Preparing
        && matches!(addition.receive, ReceiveProgress::Prepared)
        && matches!(addition.transmit, TransmitProgress::Prepared)
        && addition.prepared_frames >= limits.prefill_frames.get()
    {
        addition.phase = AdditionPhase::Ready;
    }
    if addition.phase == AdditionPhase::Ready {
        let ready = match super::delivery::validate_publication(
            entry.identity,
            entry.next_frame,
            entry.frame.as_ref(),
            addition,
            now,
            radio,
            receive,
        ) {
            Ok(()) => true,
            Err(AdmissionError::Signalling(ref error))
                if crate::signalling::scheduling::backpressure(error) =>
            {
                false
            }
            Err(error) => {
                addition.fail(error);
                return Ok(0);
            }
        };
        if ready {
            match radio.publish_additional_setup(
                &mut addition.procedure,
                entry.identity,
                addition.identity,
                now,
            ) {
                Ok(_) => {
                    if addition.delivery.is_some() {
                        radio
                            .reserve_transition_delivery(entry.identity)
                            .map_err(AdmissionError::Signalling)?;
                    }
                    addition.phase = AdditionPhase::Switching;
                }
                Err(error) if crate::signalling::scheduling::backpressure(&error) => (),
                Err(error) => return Err(AdmissionError::Signalling(error)),
            }
        }
    }
    let delivery_confirmed = if addition.phase == AdditionPhase::Switching {
        if let Some(delivery) = addition.delivery.as_mut() {
            match delivery.acknowledgement.poll(
                entry.identity,
                addition.procedure.receipt().ok_or(AdmissionError::Phase)?,
                now,
                radio,
            ) {
                Ok(confirmed) => confirmed,
                Err(error) => {
                    addition.fail(error);
                    return Ok(0);
                }
            }
        } else {
            true
        }
    } else {
        false
    };
    if addition.phase == AdditionPhase::Switching && delivery_confirmed {
        if matches!(addition.receive, ReceiveProgress::Prepared) {
            match receive.commit_mapping_receiver(entry.identity, addition.identity) {
                Ok(ticket) => {
                    addition.receive = ReceiveProgress::Waiting {
                        action: ReceiveAction::Commit,
                        ticket,
                    }
                }
                Err(DedicatedControlError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Receive(error)),
            }
        }
        if matches!(addition.transmit, TransmitProgress::Prepared)
            && let Ok(id) = port.commit_replacement(entry.identity, addition.identity)
        {
            entry.pending = Some(PendingCommand {
                id,
                kind: CommandKind::ReplacementCommit,
            });
            addition.transmit = TransmitProgress::Committing;
            return Ok(0);
        }
        if addition.receive_scheduled() && addition.transmit_scheduled() {
            receive
                .commit_mapping(addition.identity)
                .map_err(AdmissionError::Receive)?;
            addition.phase = AdditionPhase::AwaitingComplete;
        }
    }
    let horizon = common
        .next_chip()
        .index
        .max(now.index)
        .checked_add(limits.prefill_frames.get() as u64 * CHIPS_PER_FRAME)
        .ok_or(AdmissionError::Time)?;
    if entry.next_frame.index < addition.activation.downlink.time.index
        && let Some(id) = schedule_frame(
            FrameLane {
                identity: entry.identity,
                pool: entry.pool.as_mut().expect("original pool"),
                frame: &mut entry.frame,
                next: &mut entry.next_frame,
                idle: entry.idle,
                signalling: true,
            },
            now,
            horizon.min(addition.activation.downlink.time.index),
            port,
            radio,
            receive,
            policy,
        )?
    {
        entry.pending = Some(PendingCommand {
            id,
            kind: CommandKind::Frame,
        });
        return Ok(1);
    }
    if matches!(addition.transmit, TransmitProgress::Prepared) || addition.transmit_scheduled() {
        let scheduled = addition.receive_scheduled() && addition.transmit_scheduled();
        let horizon = if scheduled {
            horizon
        } else {
            addition
                .activation
                .downlink
                .time
                .index
                .checked_add(limits.prefill_frames.get() as u64 * CHIPS_PER_FRAME)
                .ok_or(AdmissionError::Time)?
        };
        if let Some(id) = schedule_frame(
            FrameLane {
                identity: addition.identity,
                pool: addition.pool.as_mut().expect("successor pool"),
                frame: &mut addition.frame,
                next: &mut addition.next_frame,
                idle: addition.idle,
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
    if addition.receive_scheduled() && addition.transmit_scheduled() {
        if matches!(addition.receive, ReceiveProgress::Scheduled) {
            match receive.retire_mapping_receiver(entry.identity) {
                Ok(ticket) => {
                    addition.receive = ReceiveProgress::Waiting {
                        action: ReceiveAction::Retire,
                        ticket,
                    }
                }
                Err(DedicatedControlError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Receive(error)),
            }
        }
        if matches!(addition.transmit, TransmitProgress::Scheduled)
            && let Ok(id) = port.retire_submitted(entry.identity)
        {
            entry.pending = Some(PendingCommand {
                id,
                kind: CommandKind::RetirePrevious,
            });
            addition.transmit = TransmitProgress::Retiring;
        }
        if addition.accepted.is_some()
            && matches!(addition.receive, ReceiveProgress::Retired(_))
            && matches!(addition.transmit, TransmitProgress::Retired(_))
        {
            addition.phase = AdditionPhase::ReadyToCommit;
        }
    }
    Ok(0)
}

impl Rel99Admission {
    #[cfg(test)]
    pub fn commit_addition(
        &mut self,
        previous: ReceiveIdentity,
        resources: &mut RadioResources,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<ReceiveIdentity, AdmissionError> {
        let entry = self.entry(previous).ok_or(AdmissionError::Identity)?;
        if entry.circuit_assignment.is_some()
            || entry.packet_assignment.is_some()
            || entry
                .lifecycle
                .addition()
                .is_some_and(|addition| addition.assignment.is_some())
        {
            return Err(AdmissionError::Phase);
        }
        self.commit_addition_owner(previous, resources, radio, receive, NoMedia)
            .map_err(|error| match error {
                CommitFailure::Admission(error) => error,
                CommitFailure::Media(never) => match never {},
            })
    }

    pub(super) fn commit_addition_owner<M: MediaCommit>(
        &mut self,
        previous: ReceiveIdentity,
        resources: &mut RadioResources,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        media: M,
    ) -> Result<ReceiveIdentity, CommitFailure<M::Error>> {
        let entry = self
            .entry_mut(previous)
            .ok_or(CommitFailure::Admission(AdmissionError::Identity))?;
        if entry.pending.is_some() || entry.frame.is_some() {
            return Err(CommitFailure::Admission(AdmissionError::Phase));
        }
        let mut addition = entry
            .lifecycle
            .take_addition()
            .ok_or(CommitFailure::Admission(AdmissionError::Phase))?;
        let result = commit(entry, &mut addition, resources, radio, receive, media);
        *entry
            .lifecycle
            .transition_mut()
            .expect("bearer change owner") = Transition::Addition(Box::new(addition));
        result
    }

    /// now must be elapsed lower-bound radio time, not the future TX preparation cursor.
    pub fn finish_addition(
        &mut self,
        identity: ReceiveIdentity,
        resources: &mut RadioResources,
        radio: &mut CommonSignalling,
        now: ChipInstant,
    ) -> Result<bool, AdmissionError> {
        let entry = self.entry_mut(identity).ok_or(AdmissionError::Identity)?;
        let addition = entry
            .lifecycle
            .addition_mut()
            .ok_or(AdmissionError::Phase)?;
        if addition.phase != AdditionPhase::Committed || now.epoch != identity.ue.epoch {
            return Err(AdmissionError::Phase);
        }
        let TransmitProgress::Retired(outcome) = &mut addition.transmit else {
            return Err(AdmissionError::Phase);
        };
        let TransmitCommandOutcome::Retired {
            result: Ok(retired),
            ..
        } = outcome.as_mut()
        else {
            return Err(AdmissionError::Phase);
        };
        if !retired.submission.viable() {
            return Err(AdmissionError::Protocol);
        }
        if retired.submission.air_end().epoch != now.epoch {
            return Err(AdmissionError::Identity);
        }
        if !retired.submission.resolved() || now.index < retired.submission.air_end().index {
            return Ok(false);
        }
        if let Some(pool) = addition.previous_pool.as_mut() {
            pool.reclaim_retired(&mut retired.session)
                .map_err(|error| AdmissionError::Frame(FrameBuildError::Pool(error)))?;
        }
        if let Some(pool) = addition.previous_pool.take()
            && let Err(rejected) = radio.retire_frame_pool(pool)
        {
            addition.previous_pool = Some(*rejected.pool);
            if rejected.error == crate::dedicated_bearers::FramePoolError::Pending {
                return Ok(false);
            }
            return Err(AdmissionError::Frame(FrameBuildError::Pool(rejected.error)));
        }
        let accepted = addition.accepted.as_ref().ok_or(AdmissionError::Phase)?;
        if !addition.receipt_released {
            match radio.release_additional_setup_receipt(
                accepted,
                addition.procedure.receipt().ok_or(AdmissionError::Phase)?,
            ) {
                Ok(()) => addition.receipt_released = true,
                Err(SignallingError::Capacity) => return Ok(false),
                Err(error) => return Err(AdmissionError::Signalling(error)),
            }
        }
        match std::mem::replace(&mut addition.resources, ResourceHandoff::Retired) {
            ResourceHandoff::Retiring(proof) => {
                if let Err(rejected) = resources.retire_reconfiguration_codes(proof) {
                    addition.resources = ResourceHandoff::Retiring(rejected.proof);
                    return Err(AdmissionError::Resources(rejected.error));
                }
            }
            ResourceHandoff::Retired => (),
            prepared @ ResourceHandoff::Prepared(_) => {
                addition.resources = prepared;
                return Err(AdmissionError::Phase);
            }
        }
        if let Some(assignment) = addition.assignment.as_ref() {
            if !assignment.outcomes_recorded() {
                return Ok(false);
            }
            if entry.additional_assignments.len() == entry.additional_assignments.capacity() {
                return Err(AdmissionError::Capacity);
            }
        }
        let addition = entry
            .lifecycle
            .take_addition()
            .expect("completed addition owner");
        if let Some(assignment) = addition.assignment {
            entry.additional_assignments.push(assignment);
        }
        entry.addition_history = Some(CompletedAddition {
            procedure: addition.procedure,
            accepted: addition.accepted.expect("accepted setup"),
        });
        Ok(true)
    }
}

fn commit<M: MediaCommit>(
    entry: &mut Entry,
    addition: &mut ScheduledAddition,
    resources: &mut RadioResources,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
    media: M,
) -> Result<ReceiveIdentity, CommitFailure<M::Error>> {
    use crate::{
        dedicated_controller::DedicatedMappingCommitError, signalling::AdditionalSetupCommitError,
    };
    if addition.phase != AdditionPhase::ReadyToCommit
        || !matches!(addition.resources, ResourceHandoff::Prepared(_))
        || entry.resources.is_none()
    {
        return Err(CommitFailure::Admission(AdmissionError::Phase));
    }
    let accepted = addition
        .accepted
        .as_ref()
        .ok_or(CommitFailure::Admission(AdmissionError::Phase))?;
    let ReceiveProgress::Retired(rx) = &addition.receive else {
        return Err(CommitFailure::Admission(AdmissionError::Phase));
    };
    let TransmitProgress::Retired(tx) = &addition.transmit else {
        return Err(CommitFailure::Admission(AdmissionError::Phase));
    };
    let plan = addition
        .pool
        .as_ref()
        .ok_or(CommitFailure::Admission(AdmissionError::Phase))?
        .plan()
        .clone();
    radio
        .commit_additional_setup_with(accepted, |_| {
            receive.retire_mapping_with(entry.identity, rx, tx, |bank| {
                media.commit(accepted, plan, bank, || {
                    let active = entry.resources.take().expect("validated active lease");
                    let ResourceHandoff::Prepared(change) =
                        std::mem::replace(&mut addition.resources, ResourceHandoff::Retired)
                    else {
                        unreachable!("validated staged resource change")
                    };
                    match resources.commit_reconfiguration(active, change) {
                        Ok(committed) => {
                            let (lease, _, retired) = committed.into_parts();
                            entry.resources = Some(lease);
                            addition.resources = ResourceHandoff::Retiring(retired);
                            Ok(())
                        }
                        Err(rejected) => {
                            entry.resources = Some(rejected.active);
                            addition.resources = ResourceHandoff::Prepared(rejected.change);
                            Err(AdmissionError::Resources(rejected.error))
                        }
                    }
                })
            })?;
            addition.previous_pool = entry
                .pool
                .replace(addition.pool.take().expect("successor pool"));
            entry.identity = addition.identity;
            entry.next_frame = addition.next_frame;
            entry.frame = addition.frame.take();
            entry.idle = addition.idle;
            entry.prepared_frames = addition.prepared_frames;
            entry.first = None;
            entry.setup_authority = false;
            entry.next_health = addition.next_frame;
            addition.phase = AdditionPhase::Committed;
            Ok::<_, DedicatedMappingCommitError<CommitFailure<M::Error>>>(addition.identity)
        })
        .map_err(|error| match error {
            AdditionalSetupCommitError::Signalling(error) => {
                CommitFailure::Admission(AdmissionError::Signalling(error))
            }
            AdditionalSetupCommitError::Commit(DedicatedMappingCommitError::Control(error)) => {
                CommitFailure::Admission(AdmissionError::Receive(error))
            }
            AdditionalSetupCommitError::Commit(DedicatedMappingCommitError::Commit(error)) => error,
        })
}
