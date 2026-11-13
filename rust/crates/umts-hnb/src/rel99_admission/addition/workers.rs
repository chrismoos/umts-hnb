// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(super) enum ReceiveAction {
    Prepare,
    Commit,
    Retire,
}
pub(super) enum ReceiveProgress {
    Local(Box<DedicatedSampleReceiver>),
    Waiting {
        action: ReceiveAction,
        ticket: ReceiveCommandTicket,
    },
    Prepared,
    Scheduled,
    Retired(Box<ReceiveCommandResult>),
    Unavailable,
}
pub(super) enum TransmitProgress {
    Local(Box<DedicatedTransmitSession>),
    Preparing,
    Prepared,
    Committing,
    Scheduled,
    Retiring,
    Retired(Box<TransmitCommandOutcome>),
    Unavailable,
}

impl ScheduledAddition {
    pub(super) fn receive_scheduled(&self) -> bool {
        matches!(
            self.receive,
            ReceiveProgress::Scheduled
                | ReceiveProgress::Retired(_)
                | ReceiveProgress::Waiting {
                    action: ReceiveAction::Retire,
                    ..
                }
        )
    }
    pub(super) fn transmit_scheduled(&self) -> bool {
        matches!(
            self.transmit,
            TransmitProgress::Scheduled | TransmitProgress::Retiring | TransmitProgress::Retired(_)
        )
    }
    pub(super) fn poll_receive(&mut self, previous: ReceiveIdentity) {
        let ReceiveProgress::Waiting { ticket, .. } = &mut self.receive else {
            return;
        };
        let Some(result) = ticket.try_result() else {
            return;
        };
        let ReceiveProgress::Waiting { action, .. } =
            std::mem::replace(&mut self.receive, ReceiveProgress::Unavailable)
        else {
            unreachable!("completed receive ticket")
        };
        self.receive = match (action, result) {
            (
                ReceiveAction::Prepare,
                ReceiveCommandResult::ReplacementPrepared {
                    previous: old,
                    identity,
                },
            ) if old == previous && identity == self.identity => ReceiveProgress::Prepared,
            (
                ReceiveAction::Commit,
                ReceiveCommandResult::ReplacementCommitted {
                    previous: old,
                    identity,
                    boundary,
                },
            ) if old == previous
                && identity == self.identity
                && boundary.time == self.activation.uplink.time
                && boundary.cfn == self.activation.uplink.cfn =>
            {
                ReceiveProgress::Scheduled
            }
            (ReceiveAction::Retire, result @ ReceiveCommandResult::Retired { identity, .. })
                if identity == previous =>
            {
                ReceiveProgress::Retired(Box::new(result))
            }
            (
                ReceiveAction::Retire,
                ReceiveCommandResult::RetireRejected {
                    identity,
                    error: crate::dedicated_rx::ReceiveError::Occupied,
                },
            ) if identity == previous => ReceiveProgress::Scheduled,
            (ReceiveAction::Prepare, ReceiveCommandResult::InstallRejected { error, receiver }) => {
                self.fail(AdmissionError::Receive(DedicatedControlError::Receive(
                    error,
                )));
                ReceiveProgress::Local(receiver)
            }
            (_, result) => {
                self.unclaimed_receive = Some(result);
                self.fail(AdmissionError::Protocol);
                ReceiveProgress::Unavailable
            }
        };
    }
}

pub(in crate::rel99_admission) fn poll_transmit(
    entry: &mut Entry,
    kind: CommandKind,
    outcome: TransmitCommandOutcome,
    backpressure: &mut u64,
) -> Option<TransmitCommandOutcome> {
    let addition = entry.lifecycle.addition_mut()?;
    match (kind, outcome) {
        (_, TransmitCommandOutcome::Closed) => addition.fail(AdmissionError::Closed),
        (
            CommandKind::ReplacementPrepare,
            TransmitCommandOutcome::ReplacementPrepared { previous, identity },
        ) if previous == entry.identity
            && identity == addition.identity
            && matches!(addition.transmit, TransmitProgress::Preparing) =>
        {
            addition.transmit = TransmitProgress::Prepared;
        }
        (
            CommandKind::ReplacementPrepare,
            TransmitCommandOutcome::PrepareRejected { error, session },
        ) if session.identity() == addition.identity
            && matches!(addition.transmit, TransmitProgress::Preparing) =>
        {
            addition.transmit = TransmitProgress::Local(session);
            addition.fail(AdmissionError::Transmit(error));
        }
        (
            CommandKind::ReplacementCommit,
            TransmitCommandOutcome::ReplacementCommitted {
                previous,
                identity,
                result,
            },
        ) if previous == entry.identity
            && identity == addition.identity
            && matches!(addition.transmit, TransmitProgress::Committing) =>
        {
            match result {
                Ok(anchor)
                    if anchor.time == addition.activation.downlink.time
                        && anchor.cfn == addition.activation.downlink.cfn =>
                {
                    addition.transmit = TransmitProgress::Scheduled;
                }
                Ok(_) => addition.fail(AdmissionError::Protocol),
                Err(error) => addition.fail(AdmissionError::Transmit(error)),
            }
        }
        (CommandKind::SuccessorFrame, TransmitCommandOutcome::Frame { mut input, result })
            if input.identity == addition.identity =>
        {
            if result == Err(TransmitError::Capacity) {
                addition.next_frame = input.start;
                addition.frame = Some(input);
                *backpressure = backpressure.saturating_add(1);
                return None;
            }
            if result.is_err()
                && let Some(emission) = &mut input.emission
            {
                emission.fail();
            }
            if let Err(input) = addition
                .pool
                .as_mut()
                .expect("successor pool")
                .recycle(input)
            {
                addition.frame = Some(input);
                addition.fail(AdmissionError::Protocol);
            }
            match result {
                Ok(_) => addition.prepared_frames = addition.prepared_frames.saturating_add(1),
                Err(error) => addition.fail(AdmissionError::Transmit(error)),
            }
        }
        (
            CommandKind::RetirePrevious,
            outcome @ TransmitCommandOutcome::Retired {
                identity,
                result: Ok(_),
            },
        ) if identity == entry.identity
            && matches!(addition.transmit, TransmitProgress::Retiring) =>
        {
            addition.transmit = TransmitProgress::Retired(Box::new(outcome));
        }
        (
            CommandKind::RetirePrevious,
            TransmitCommandOutcome::Retired {
                identity,
                result: Err(error),
            },
        ) if identity == entry.identity
            && matches!(addition.transmit, TransmitProgress::Retiring) =>
        {
            addition.transmit = TransmitProgress::Scheduled;
            if !matches!(
                error,
                TransmitError::Phase | TransmitError::SubmissionPending
            ) {
                addition.fail(AdmissionError::Transmit(error));
            }
        }
        (_, outcome) => {
            addition.fail(AdmissionError::Protocol);
            return Some(outcome);
        }
    }
    None
}
