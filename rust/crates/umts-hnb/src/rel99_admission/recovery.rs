// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_rx::{DedicatedSampleReceiver, ReceiveCommandResult, ReceiveCommandTicket},
    dedicated_tx::TransmitDrain,
    rab_handoff::{FailedRabAssignment, PendingRabAssignment},
    radio_resources::{PreparedRadioChange, RadioResources},
};
use umts_iuh::{
    ranap::rab::Cause,
    session::{ControlTime, RabRequest},
};

pub enum FailedChangeRequest {
    Addition(Box<RejectedAdditionalAssignment>),
    Release(Box<RejectedReleaseAssignment>),
    Quarantined(QuarantinedAdmission),
}
#[must_use = "Worker retirement has not been proven; retain every original owner"]
pub struct QuarantinedAdmission {
    entry: Box<Entry>,
}
impl QuarantinedAdmission {
    pub fn identity(&self) -> ReceiveIdentity {
        self.entry.identity
    }
    pub fn release_request(&self) -> Option<RabRequest> {
        self.entry
            .lifecycle
            .recovery()
            .and_then(|recovery| recovery.failed.release_request())
            .or_else(|| release::failed_request(&self.entry))
    }
    pub fn error(&self) -> &AdmissionError {
        self.entry.quarantine_error.as_ref().unwrap_or_else(|| {
            &self
                .entry
                .lifecycle
                .recovery()
                .expect("quarantined recovery")
                .failed
                .error
        })
    }
    #[cfg(test)]
    pub fn retained_receivers(&self) -> usize {
        let Some(recovery) = self.entry.lifecycle.recovery() else {
            return release::quarantined_custody(&self.entry).0
                + addition::quarantined_custody(&self.entry).0;
        };
        usize::from(matches!(
            recovery.receiver,
            Receiver::Returned(_) | Receiver::Unexpected(_) | Receiver::Local(_)
        )) + usize::from(recovery.original_receiver.is_some())
    }
    #[cfg(test)]
    pub fn pending_receiver_commands(&self) -> usize {
        let Some(recovery) = self.entry.lifecycle.recovery() else {
            return release::quarantined_custody(&self.entry).1
                + addition::quarantined_custody(&self.entry).1;
        };
        usize::from(matches!(recovery.receiver, Receiver::Waiting(_)))
            + usize::from(recovery.original_pending.is_some())
    }
    #[cfg(test)]
    pub fn has_radio_change(&self) -> bool {
        self.entry
            .lifecycle
            .recovery()
            .is_some_and(|recovery| recovery.failed.change.is_some())
            || release::quarantined_custody(&self.entry).2
            || addition::quarantined_custody(&self.entry).2
    }
    #[cfg(test)]
    pub fn retained_transmit_results(&self) -> usize {
        usize::from(self.entry.quarantine_transmit.is_some())
            + usize::from(
                self.entry
                    .lifecycle
                    .recovery()
                    .is_some_and(|recovery| recovery.unexpected_transmit.is_some()),
            )
    }
}
#[derive(Debug)]
pub enum FailedChangeError {
    Admission(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdmissionError,
    ),
    Resources(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::radio_resources::Error,
    ),
    Disposition(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::rab_handoff::FailureDispositionError,
    ),
    Core(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_core::Error,
    ),
    Release(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        CoreReleaseError,
    ),
}
pub(super) enum Kind {
    Addition,
    Release,
}
#[derive(Debug)]
pub(super) enum Mode {
    Unpublished,
    Published,
    CurrentBankTeardown,
}
enum Receiver {
    Local(Box<DedicatedSampleReceiver>),
    Worker,
    Waiting(ReceiveCommandTicket),
    Returned(Box<ReceiveCommandResult>),
    Unexpected(Box<ReceiveCommandResult>),
}
enum Transmitter {
    Local(Box<DedicatedTransmitSession>),
    Worker,
    Returned(Box<DedicatedTransmitSession>),
}
#[derive(Debug)]
enum Phase {
    Removing,
    RollbackReady,
    MappingCancelled,
    Retired,
    Quarantined,
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub(super) enum FailedProcedure {
    Addition(Box<RadioBearerSetupProcedure>),
    Release(Box<umts_rrc::dedicated::RadioBearerReleaseProcedure>),
}
#[must_use = "Retain the original request and radio change through failure disposition"]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct FailedBearerChange {
    kind: Kind,
    pending: Option<PendingRabAssignment>,
    reporting: Option<FailedRabAssignment>,
    change: Option<PreparedRadioChange>,
    retired_codes: Option<crate::radio_resources::RetiredRadioCodes>,
    release: Option<Box<RabReleaseRequest>>,
    procedure: FailedProcedure,
    receiver: Option<Box<ReceiveCommandResult>>,
    transmitter: Option<Box<DedicatedTransmitSession>>,
    original_receiver: Option<Box<ReceiveCommandResult>>,
    error: AdmissionError,
}
impl FailedBearerChange {
    #[cfg(test)]
    pub fn key(&self) -> umts_types::UeKey {
        match &self.procedure {
            FailedProcedure::Addition(procedure) => procedure.key(),
            FailedProcedure::Release(procedure) => procedure.key(),
        }
    }
    #[cfg(test)]
    pub fn retained_receivers(&self) -> usize {
        usize::from(self.receiver.is_some()) + usize::from(self.original_receiver.is_some())
    }
    pub fn release_request(&self) -> Option<RabRequest> {
        if !matches!(self.kind, Kind::Release) {
            return None;
        }
        self.pending
            .as_ref()
            .map(PendingRabAssignment::request)
            .or_else(|| self.release.as_ref().map(|release| release.request()))
            .or_else(|| self.reporting.as_ref().map(FailedRabAssignment::request))
    }
    pub fn finish<C: umts_core::Transport>(
        &mut self,
        core: &mut crate::core_link::CoreLink<C>,
        resources: &mut RadioResources,
        now: ChipInstant,
        control: ControlTime,
        cause: Cause,
    ) -> Result<bool, FailedChangeError> {
        if let Some(transmit) = self.transmitter.as_ref()
            && !drained(transmit, now).map_err(FailedChangeError::Admission)?
        {
            return Ok(false);
        }
        if let Some(change) = self.change.take()
            && let Err(rejected) = resources.cancel_reconfiguration(change)
        {
            self.change = Some(rejected.change);
            return Err(FailedChangeError::Resources(rejected.error));
        }
        if let Some(proof) = self.retired_codes.take()
            && let Err(rejected) = resources.retire_reconfiguration_codes(proof)
        {
            self.retired_codes = Some(rejected.proof);
            return Err(FailedChangeError::Resources(rejected.error));
        }
        if let Some(release) = self.release.as_mut() {
            release
                .record_ready(core, control, NonZeroUsize::MIN)
                .map_err(FailedChangeError::Release)?;
            if !release.outcomes_recorded() {
                return Ok(false);
            }
            self.pending = Some(
                self.release
                    .take()
                    .expect("settled release owner")
                    .into_pending(),
            );
        }
        if self.reporting.is_none()
            && let Some(pending) = self.pending.take()
        {
            match FailedRabAssignment::new(pending, cause, core.gateway()) {
                Ok(reporting) => self.reporting = Some(reporting),
                Err(rejected) => {
                    self.pending = Some(rejected.pending);
                    return Err(FailedChangeError::Disposition(rejected.error));
                }
            }
        }
        if let Some(reporting) = self.reporting.as_mut() {
            reporting
                .record(core, control, NonZeroUsize::MIN)
                .map_err(FailedChangeError::Core)?;
            return Ok(reporting.is_complete());
        }
        Ok(true)
    }
}
pub(super) struct JoinedRecovery {
    previous: ReceiveIdentity,
    identity: ReceiveIdentity,
    mode: Mode,
    phase: Phase,
    receiver: Receiver,
    original_receiver: Option<Box<ReceiveCommandResult>>,
    original_pending: Option<ReceiveCommandTicket>,
    transmitter: Transmitter,
    unexpected_transmit: Option<Box<TransmitCommandOutcome>>,
    pool: Option<FramePool>,
    frame: Option<FrameInput>,
    output_retired: bool,
    recovery_marked: bool,
    failed: FailedBearerChange,
    stage: RecoveryStage,
}
#[derive(Debug)]
enum RecoveryStage {
    Starting,
    MarkingRadio,
    RemovingSuccessor,
    RetiringSuccessorOutput,
    RemovingCurrentBank,
    RemovingPredecessor,
    RetiringCurrentBank,
    RetiringMappingPair,
}
pub(super) struct Candidate {
    pub previous: ReceiveIdentity,
    pub identity: ReceiveIdentity,
    pub mode: Mode,
    pub receiver: Option<Box<DedicatedSampleReceiver>>,
    pub returned_receiver: Option<ReceiveCommandResult>,
    pub original_receiver: Option<ReceiveCommandResult>,
    pub transmitter: Option<Box<DedicatedTransmitSession>>,
    pub pool: Option<FramePool>,
    pub frame: Option<FrameInput>,
    pub change: Option<PreparedRadioChange>,
    pub retired_codes: Option<crate::radio_resources::RetiredRadioCodes>,
    pub release: Option<Box<RabReleaseRequest>>,
    pub pending: Option<PendingRabAssignment>,
    pub procedure: FailedProcedure,
    pub kind: Kind,
    pub error: AdmissionError,
}
impl JoinedRecovery {
    pub(super) fn is_quarantined(&self) -> bool {
        matches!(self.phase, Phase::Quarantined)
    }
    pub(super) fn quarantine(&mut self, error: AdmissionError) {
        tracing::warn!(
            "Bearer recovery quarantined: previous={:?} successor={:?} initiating_error={:?} terminal_error={error:?}",
            self.previous,
            self.identity,
            self.failed.error
        );
        tracing::debug!(
            "Bearer recovery quarantine state: mode={:?} phase={:?} stage={:?}",
            self.mode,
            self.phase,
            self.stage
        );
        self.failed.error = error;
        self.phase = Phase::Quarantined;
    }
    pub(super) fn new(candidate: Candidate) -> Self {
        Self {
            stage: RecoveryStage::Starting,
            previous: candidate.previous,
            identity: candidate.identity,
            mode: candidate.mode,
            phase: Phase::Removing,
            receiver: match candidate.returned_receiver {
                Some(result) => Receiver::Returned(Box::new(result)),
                None => candidate
                    .receiver
                    .map(Receiver::Local)
                    .unwrap_or(Receiver::Worker),
            },
            original_receiver: candidate.original_receiver.map(Box::new),
            original_pending: None,
            transmitter: candidate
                .transmitter
                .map(Transmitter::Local)
                .unwrap_or(Transmitter::Worker),
            unexpected_transmit: None,
            pool: candidate.pool,
            frame: candidate.frame,
            output_retired: false,
            recovery_marked: false,
            failed: FailedBearerChange {
                kind: candidate.kind,
                pending: candidate.pending,
                reporting: None,
                change: candidate.change,
                retired_codes: candidate.retired_codes,
                release: candidate.release,
                procedure: candidate.procedure,
                receiver: None,
                transmitter: None,
                original_receiver: None,
                error: candidate.error,
            },
        }
    }
    pub(super) fn ready_to_stop(&self) -> bool {
        matches!(self.phase, Phase::Retired)
    }
    pub(super) fn poll_transmit(&mut self, outcome: TransmitCommandOutcome) {
        match outcome {
            TransmitCommandOutcome::Removed {
                identity,
                result: Ok(session),
            } if identity == self.identity && session.identity() == identity => {
                self.transmitter = Transmitter::Returned(session)
            }
            TransmitCommandOutcome::Removed {
                identity,
                result: Err(error),
            } if identity == self.identity => {
                if error != TransmitError::Capacity {
                    self.quarantine(AdmissionError::Transmit(error));
                }
            }
            outcome => {
                self.unexpected_transmit = Some(Box::new(outcome));
                self.quarantine(AdmissionError::Protocol);
            }
        }
    }
    pub(super) fn into_failed(mut self) -> FailedBearerChange {
        self.failed.receiver = match self.receiver {
            Receiver::Returned(result) => Some(result),
            _ => None,
        };
        self.failed.original_receiver = self.original_receiver;
        self.failed.transmitter = match self.transmitter {
            Transmitter::Local(owner) | Transmitter::Returned(owner) => Some(owner),
            _ => None,
        };
        self.failed
    }
}
pub(super) fn retryable(error: &AdmissionError) -> bool {
    match error {
        AdmissionError::Capacity | AdmissionError::Receive(DedicatedControlError::Capacity) => true,
        AdmissionError::Signalling(error) => crate::signalling::scheduling::backpressure(error),
        AdmissionError::Resources(crate::radio_resources::Error::Capacity) => true,
        _ => false,
    }
}
fn drained(session: &DedicatedTransmitSession, now: ChipInstant) -> Result<bool, AdmissionError> {
    match session.drain() {
        TransmitDrain::LocalUnsubmitted | TransmitDrain::NoContribution => Ok(true),
        TransmitDrain::Unknown => Err(AdmissionError::Protocol),
        TransmitDrain::ResolvedTail(receipt) => {
            if !receipt.viable() {
                return Err(AdmissionError::Transmit(TransmitError::SubmissionLost));
            }
            if receipt.air_end().epoch != now.epoch {
                return Err(AdmissionError::Identity);
            }
            Ok(receipt.resolved() && now.index >= receipt.air_end().index)
        }
    }
}
pub(super) fn step(
    entry: &mut Entry,
    now: ChipInstant,
    port: &mut DedicatedTransmitPort,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
) -> Result<(), AdmissionError> {
    let recovery = entry
        .lifecycle
        .recovery_mut()
        .expect("joined recovery owner");
    if matches!(recovery.mode, Mode::Unpublished)
        && (radio.phase(entry.identity.ue).is_none() || entry.failure.is_some())
    {
        recovery.mode = Mode::CurrentBankTeardown;
        recovery.phase = Phase::Removing;
    }
    if !matches!(recovery.phase, Phase::Removing) {
        return Ok(());
    }
    if let Receiver::Unexpected(result) = &recovery.receiver {
        let _retained_result = &**result;
        return Err(AdmissionError::Protocol);
    }
    if matches!(recovery.mode, Mode::Published | Mode::CurrentBankTeardown)
        && !recovery.recovery_marked
    {
        if radio.phase(entry.identity.ue).is_some() {
            recovery.stage = RecoveryStage::MarkingRadio;
            radio
                .require_dedicated_recovery(entry.identity)
                .map_err(AdmissionError::Signalling)?;
        }
        recovery.recovery_marked = true;
        entry.setup_authority = false;
    }
    if let Receiver::Waiting(ticket) = &mut recovery.receiver
        && let Some(result) = ticket.try_result()
    {
        match &result {
            ReceiveCommandResult::Removed {
                identity,
                receiver: Some(owner),
            } if *identity == recovery.identity && owner.session().identity() == *identity => {
                recovery.receiver = Receiver::Returned(Box::new(result))
            }
            _ => {
                recovery.receiver = Receiver::Unexpected(Box::new(result));
                return Err(AdmissionError::Protocol);
            }
        }
    }
    if recovery.original_receiver.is_none()
        && let Some(ticket) = recovery.original_pending.as_mut()
        && let Some(result) = ticket.try_result()
    {
        match &result {
            ReceiveCommandResult::Removed {
                identity,
                receiver: Some(owner),
            } if *identity == recovery.previous && owner.session().identity() == *identity => {
                recovery.original_receiver = Some(Box::new(result));
                recovery.original_pending = None;
            }
            _ => {
                recovery.original_receiver = Some(Box::new(result));
                recovery.original_pending = None;
                return Err(AdmissionError::Protocol);
            }
        }
    }
    if matches!(recovery.receiver, Receiver::Worker) {
        recovery.stage = RecoveryStage::RemovingSuccessor;
        match receive.remove_mapping_receiver(recovery.previous, recovery.identity) {
            Ok(ticket) => recovery.receiver = Receiver::Waiting(ticket),
            Err(DedicatedControlError::Capacity) => (),
            Err(error) => return Err(AdmissionError::Receive(error)),
        }
    }
    if matches!(recovery.mode, Mode::CurrentBankTeardown)
        && !matches!(entry.receive, ReceiveOwnership::Returned(_))
    {
        if !recovery.output_retired {
            match &recovery.receiver {
                Receiver::Returned(result) => {
                    if matches!(**result, ReceiveCommandResult::Removed { .. }) {
                        recovery.stage = RecoveryStage::RetiringSuccessorOutput;
                        receive
                            .retire_removed_mapping_output(recovery.previous, result)
                            .map_err(AdmissionError::Receive)?;
                    }
                    recovery.output_retired = true;
                }
                Receiver::Local(_) => recovery.output_retired = true,
                _ => return Ok(()),
            }
        }
        recovery.stage = RecoveryStage::RemovingCurrentBank;
        match receive.phase(entry.identity) {
            Some(
                crate::dedicated_controller::DedicatedPhase::Active
                | crate::dedicated_controller::DedicatedPhase::Faulted,
            ) => match receive.remove(entry.identity) {
                Ok(()) | Err(DedicatedControlError::Capacity) => (),
                Err(error) => return Err(AdmissionError::Receive(error)),
            },
            Some(crate::dedicated_controller::DedicatedPhase::Retired) => (),
            _ => (),
        }
    }
    if matches!(recovery.mode, Mode::Published)
        && recovery.original_receiver.is_none()
        && recovery.original_pending.is_none()
    {
        recovery.stage = RecoveryStage::RemovingPredecessor;
        match receive.remove_mapping_receiver(recovery.previous, recovery.previous) {
            Ok(ticket) => recovery.original_pending = Some(ticket),
            Err(DedicatedControlError::Capacity) => (),
            Err(error) => return Err(AdmissionError::Receive(error)),
        }
    }
    if entry.pending.is_none() {
        if matches!(recovery.transmitter, Transmitter::Worker) {
            match port.remove(recovery.identity) {
                Ok(id) => {
                    entry.pending = Some(PendingCommand {
                        id,
                        kind: CommandKind::RecoveryRemove,
                    })
                }
                Err(_) if port.available() => (),
                Err(_) => return Err(AdmissionError::Closed),
            }
        } else if matches!(recovery.mode, Mode::Published | Mode::CurrentBankTeardown)
            && !matches!(entry.transmit, TransmitOwnership::Returned(_))
        {
            match port.remove(recovery.previous) {
                Ok(id) => {
                    entry.pending = Some(PendingCommand {
                        id,
                        kind: CommandKind::Remove,
                    })
                }
                Err(_) if port.available() => (),
                Err(_) => return Err(AdmissionError::Closed),
            }
        }
    }
    if entry.pending.is_some() {
        return Ok(());
    }
    let (Transmitter::Local(session) | Transmitter::Returned(session)) = &mut recovery.transmitter
    else {
        return Ok(());
    };
    // An unpublished replacement has no admitted Commit command and cannot contribute to RF.
    if !matches!(recovery.mode, Mode::Unpublished) && !drained(session, now)? {
        return Ok(());
    }
    let receive_ready = matches!(
        recovery.receiver,
        Receiver::Local(_) | Receiver::Returned(_)
    );
    if !receive_ready {
        return Ok(());
    }
    if let Some(mut frame) = recovery.frame.take() {
        if let Some(emission) = frame.emission.as_mut() {
            emission.fail()
        }
        if let Err(frame) = recovery
            .pool
            .as_mut()
            .expect("candidate pool")
            .recycle(frame)
        {
            recovery.frame = Some(frame);
            return Err(AdmissionError::Protocol);
        }
    }
    if session.phase() == TransmitPhase::Retired {
        if let Some(pool) = recovery.pool.as_mut() {
            pool.reclaim_retired(session)
                .map_err(|error| AdmissionError::Frame(FrameBuildError::Pool(error)))?;
        }
    } else if session.queued_frames() != 0 {
        return Err(AdmissionError::Protocol);
    }
    match recovery.mode {
        Mode::Unpublished => {
            if let Receiver::Local(owner) = &recovery.receiver
                && owner.session().identity() != recovery.identity
            {
                return Err(AdmissionError::Identity);
            }
            if !recovery.output_retired {
                if let Receiver::Returned(result) = &recovery.receiver {
                    recovery.stage = RecoveryStage::RetiringSuccessorOutput;
                    receive
                        .retire_removed_mapping_output(recovery.previous, result)
                        .map_err(AdmissionError::Receive)?;
                }
                recovery.output_retired = true;
            }
        }
        Mode::Published | Mode::CurrentBankTeardown => {
            let TransmitOwnership::Returned(original) = &entry.transmit else {
                return Ok(());
            };
            if !drained(original, now)? {
                return Ok(());
            }
            if !matches!(entry.receive, ReceiveOwnership::Returned(_)) {
                if matches!(recovery.mode, Mode::CurrentBankTeardown) {
                    if receive.phase(entry.identity)
                        != Some(crate::dedicated_controller::DedicatedPhase::Retired)
                    {
                        return Ok(());
                    }
                    recovery.stage = RecoveryStage::RetiringCurrentBank;
                    entry.receive = ReceiveOwnership::Returned(
                        receive
                            .take_retired(entry.identity)
                            .map_err(AdmissionError::Receive)?,
                    );
                } else {
                    let Some(old) = recovery.original_receiver.as_ref() else {
                        return Ok(());
                    };
                    let Receiver::Returned(new) = &recovery.receiver else {
                        return Err(AdmissionError::Protocol);
                    };
                    recovery.stage = RecoveryStage::RetiringMappingPair;
                    entry.receive = ReceiveOwnership::Returned(
                        receive
                            .take_removed_mapping_pair(recovery.previous, old, new)
                            .map_err(AdmissionError::Receive)?,
                    );
                }
            }
            if radio.phase(entry.identity.ue).is_some() {
                radio
                    .retire(entry.identity.ue)
                    .map_err(AdmissionError::Signalling)?;
            }
        }
    }
    if let Some(pool) = recovery.pool.take()
        && let Err(rejected) = radio.retire_frame_pool(pool)
    {
        recovery.pool = Some(*rejected.pool);
        if rejected.error == crate::dedicated_bearers::FramePoolError::Pending {
            return Ok(());
        }
        return Err(AdmissionError::Frame(FrameBuildError::Pool(rejected.error)));
    }
    if recovery.pool.is_some() {
        return Ok(());
    }
    match recovery.mode {
        Mode::Unpublished => recovery.phase = Phase::RollbackReady,
        Mode::Published | Mode::CurrentBankTeardown => {
            entry.recovery_marked = true;
            entry.setup_authority = false;
            entry.failure = Some(std::mem::replace(
                &mut recovery.failed.error,
                AdmissionError::Protocol,
            ));
            recovery.phase = Phase::Retired;
            entry.lifecycle.stop();
        }
    }
    Ok(())
}

impl Rel99Admission {
    pub fn visit_failed_changes(
        &mut self,
        maximum: NonZeroUsize,
        mut visit: impl FnMut(&mut Self, ReceiveIdentity),
    ) -> usize {
        let mut visited = 0;
        for _ in 0..maximum.get().min(self.entries.len()) {
            let slot = self.recovery_owner_cursor;
            self.recovery_owner_cursor = (slot + 1) % self.entries.len();
            let identity = self.entries[slot]
                .as_ref()
                .filter(|entry| {
                    entry.quarantine_error.is_some()
                        || entry.lifecycle.recovery().is_some_and(|recovery| {
                            matches!(
                                recovery.phase,
                                Phase::RollbackReady | Phase::MappingCancelled | Phase::Quarantined
                            )
                        })
                })
                .map(|entry| entry.identity);
            if let Some(identity) = identity {
                visit(self, identity);
                visited += 1
            }
        }
        visited
    }
    pub fn take_failed_change(
        &mut self,
        identity: ReceiveIdentity,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        resources: &mut RadioResources,
    ) -> Result<Option<FailedChangeRequest>, AdmissionError> {
        let slot = self
            .entries
            .iter()
            .position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|entry| entry.identity == identity)
            })
            .ok_or(AdmissionError::Identity)?;
        if self.entries[slot].as_ref().is_some_and(|entry| {
            entry.quarantine_error.is_some()
                || entry
                    .lifecycle
                    .recovery()
                    .is_some_and(|recovery| matches!(recovery.phase, Phase::Quarantined))
        }) {
            if self.entries[slot]
                .as_ref()
                .is_some_and(|entry| entry.pending.is_some())
            {
                return Ok(None);
            }
            if radio.dedicated_route(identity.ue) == Some(identity) {
                radio
                    .require_dedicated_recovery(identity)
                    .map_err(AdmissionError::Signalling)?
            }
            let entry = Box::new(self.entries[slot].take().expect("quarantined admission"));
            return Ok(Some(FailedChangeRequest::Quarantined(
                QuarantinedAdmission { entry },
            )));
        }
        let entry = self.entries[slot].as_mut().expect("matching admission");
        let recovery = entry
            .lifecycle
            .recovery_mut()
            .ok_or(AdmissionError::Phase)?;
        if !matches!(recovery.mode, Mode::Unpublished)
            || !matches!(
                recovery.phase,
                Phase::RollbackReady | Phase::MappingCancelled
            )
            || entry.pending.is_some()
        {
            return Err(AdmissionError::Phase);
        }
        if matches!(recovery.phase, Phase::RollbackReady) {
            match recovery.failed.kind {
                Kind::Addition => {
                    drop(
                        receive
                            .cancel_addition(identity, recovery.identity)
                            .map_err(AdmissionError::Receive)?,
                    );
                }
                Kind::Release => {
                    drop(
                        receive
                            .cancel_release(identity, recovery.identity)
                            .map_err(AdmissionError::Receive)?,
                    );
                }
            }
            recovery.phase = Phase::MappingCancelled;
        }
        if let Some(change) = recovery.failed.change.take()
            && let Err(rejected) = resources.cancel_reconfiguration(change)
        {
            recovery.failed.change = Some(rejected.change);
            return Err(AdmissionError::Resources(rejected.error));
        }
        if radio.dedicated_route(identity.ue) != Some(identity) {
            return Err(AdmissionError::Identity);
        }
        let failed = entry
            .lifecycle
            .take_recovery()
            .expect("completed unpublished rollback")
            .into_failed();
        let Some(pending) = failed.pending else {
            return Ok(None);
        };
        Ok(Some(match failed.kind {
            Kind::Addition => {
                FailedChangeRequest::Addition(Box::new(RejectedAdditionalAssignment {
                    error: RabAdmissionError::Admission(failed.error),
                    pending,
                    change: None,
                    rollback_error: None,
                }))
            }
            Kind::Release => FailedChangeRequest::Release(Box::new(RejectedReleaseAssignment {
                error: ReleaseAdmissionError::Admission(failed.error),
                pending,
                pool: None,
                change: None,
                rollback_error: None,
            })),
        }))
    }
}
