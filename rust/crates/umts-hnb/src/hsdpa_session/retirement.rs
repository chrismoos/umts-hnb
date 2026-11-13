// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitError},
    hsdpa_packet::{CommandResult, ControlPort, Operation},
    hsdpa_tx,
};
use umts_types::UeKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Running,
    NeedBegin,
    BeginPending(TransmitCommandId),
    Draining,
    FinishPending(TransmitCommandId),
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirementProgress {
    Running,
    Pending,
    Draining,
    TransmitComplete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetirementError {
    Identity,
    Phase,
    Closed,
    Transmit(hsdpa_tx::Error),
}

pub(crate) struct Retirement {
    ue: UeKey,
    state: State,
}

impl Retirement {
    pub(crate) fn new(ue: UeKey) -> Self {
        Self {
            ue,
            state: State::Running,
        }
    }

    pub(crate) fn started(&self) -> bool {
        self.state != State::Running
    }

    pub(crate) fn complete(&self) -> bool {
        self.state == State::Complete
    }

    pub(crate) fn begin(&mut self) {
        if self.state == State::Running {
            self.state = State::NeedBegin;
        }
    }

    fn progress(&self) -> RetirementProgress {
        match self.state {
            State::Running => RetirementProgress::Running,
            State::NeedBegin | State::BeginPending(_) | State::FinishPending(_) => {
                RetirementProgress::Pending
            }
            State::Draining => RetirementProgress::Draining,
            State::Complete => RetirementProgress::TransmitComplete,
        }
    }

    pub(crate) fn service(
        &mut self,
        control: &mut ControlPort<'_>,
    ) -> Result<RetirementProgress, TransmitError> {
        match self.state {
            State::NeedBegin => {
                self.state = State::BeginPending(control.begin_retirement(self.ue)?);
            }
            State::Draining => {
                self.state = State::FinishPending(control.finish_retirement(self.ue)?);
            }
            _ => (),
        }
        Ok(self.progress())
    }

    /// A command result is borrowed so malformed or closed outcomes retain their original custody.
    pub(crate) fn result(
        &mut self,
        result: &CommandResult,
    ) -> Result<RetirementProgress, RetirementError> {
        let (id, operation) = match self.state {
            State::BeginPending(id) => (id, Operation::BeginRetirement(self.ue)),
            State::FinishPending(id) => (id, Operation::FinishRetirement(self.ue)),
            _ => return Err(RetirementError::Phase),
        };
        if result.event.id != id || result.operation != operation || !result.matches_request() {
            return Err(RetirementError::Identity);
        }
        match &result.event.outcome {
            TransmitCommandOutcome::HsdpaRetirementStarted { result, .. } => {
                self.state = if result.is_ok() {
                    State::Draining
                } else {
                    State::NeedBegin
                };
                result
                    .as_ref()
                    .map_err(|error| RetirementError::Transmit(*error))?;
            }
            TransmitCommandOutcome::HsdpaRetirementFinished { result, .. } => {
                self.state = if matches!(result, Ok(true)) {
                    State::Complete
                } else {
                    State::Draining
                };
                result
                    .as_ref()
                    .map_err(|error| RetirementError::Transmit(*error))?;
            }
            // Worker closure is not evidence that already accepted samples have left the radio.
            TransmitCommandOutcome::Closed => return Err(RetirementError::Closed),
            _ => return Err(RetirementError::Identity),
        }
        Ok(self.progress())
    }
}
