// SPDX-License-Identifier: AGPL-3.0-or-later

use std::time::{Duration, Instant};

use umts_rrc_codec::EncodedRrc;
use umts_security::contexts::KeyOrigin;

use super::{Command, Completion, Error};
use crate::integrity::{AcceptedUplink, DownlinkIntegrity, StreamError};
use crate::integrity::{IntegritySchedule, IntegritySetup, ScheduleError};
use umts_security::{contexts::IntegrityContext, counters::CounterInitialization};

// TS 33.102 6.5.5/6.6.5 budgets key changes from reception of the CN command.
const MAX_PROCEDURE_DURATION: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExchangePhase {
    Prepared,
    AwaitingResponse,
    Completed,
    Aborted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortReason {
    Timeout,
    Cancelled,
    PeerFailure,
    RadioFailure,
    CellUpdate,
}

enum State {
    Prepared,
    AwaitingResponse,
    Completed(Box<Completion>),
    Aborted(AbortReason),
}

pub struct SecurityExchange {
    command: Command,
    changed_at: Instant,
    deadline: Instant,
    state: State,
}

impl SecurityExchange {
    pub fn prepare(
        command: Command,
        received_at: Instant,
        guard: Duration,
    ) -> Result<Self, ExchangeError<()>> {
        if guard.is_zero() || guard > MAX_PROCEDURE_DURATION {
            return Err(ExchangeError::Time);
        }
        let deadline = received_at.checked_add(guard).ok_or(ExchangeError::Time)?;
        Ok(Self {
            command,
            changed_at: received_at,
            deadline,
            state: State::Prepared,
        })
    }

    pub fn phase(&self) -> ExchangePhase {
        match self.state {
            State::Prepared => ExchangePhase::Prepared,
            State::AwaitingResponse => ExchangePhase::AwaitingResponse,
            State::Completed(_) => ExchangePhase::Completed,
            State::Aborted(_) => ExchangePhase::Aborted,
        }
    }

    pub fn origin(&self) -> KeyOrigin {
        self.command.origin()
    }
    pub fn transaction(&self) -> crate::establishment::TransactionId {
        self.command.transaction()
    }
    pub fn selection(&self) -> umts_security::negotiation::Selection {
        self.command.selection()
    }
    pub fn cipher_activation(&self) -> Option<super::ActivationSequences> {
        self.command.cipher_activation()
    }
    pub fn cipher_frame(&self) -> Option<u8> {
        self.command.cipher_frame()
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn abort_reason(&self) -> Option<AbortReason> {
        match self.state {
            State::Aborted(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn publish_with<T, E>(
        &mut self,
        now: Instant,
        integrity: &mut DownlinkIntegrity,
        publish_old_cipher: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ExchangeError<E>> {
        self.time(now)?;
        if !matches!(self.state, State::Prepared) {
            return Err(ExchangeError::Phase);
        }
        if now >= self.deadline {
            return Err(ExchangeError::Expired);
        }
        let result = self
            .command
            .publish_with(integrity, publish_old_cipher)
            .map_err(ExchangeError::Protection)?;
        self.changed_at = now;
        self.state = State::AwaitingResponse;
        Ok(result)
    }
    pub fn publish_scheduled_with<T, E>(
        &mut self,
        now: Instant,
        context: IntegrityContext,
        counters: CounterInitialization,
        schedule: &mut IntegritySchedule,
        publish_old_cipher: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, ScheduleError<ExchangeError<E>>> {
        if context.origin() != self.origin() {
            return Err(ScheduleError::Schedule(
                crate::integrity::ScheduleFailure::Owner,
            ));
        }
        schedule.begin_with(
            IntegritySetup {
                context,
                counters,
                change: self.command.integrity_change(),
                signalling: self.command.layout().signalling(),
                transaction: self.command.transaction(),
            },
            |integrity| self.publish_with(now, integrity, publish_old_cipher),
        )
    }

    /// The consumer must atomically admit bearer activation and its core response or leave both unchanged.
    pub fn complete_with<T, E>(
        &mut self,
        now: Instant,
        accepted: AcceptedUplink<'_>,
        activate_and_report: impl FnOnce(&Completion) -> Result<T, E>,
    ) -> Result<CompletionResult<T>, ExchangeError<E>> {
        self.time(now)?;
        if matches!(self.state, State::Prepared | State::Aborted(_)) {
            return Err(ExchangeError::Phase);
        }
        if matches!(self.state, State::AwaitingResponse) && now >= self.deadline {
            return Err(ExchangeError::Expired);
        }
        let completion = self
            .command
            .completion(accepted)
            .map_err(ExchangeError::Message)?;
        if let State::Completed(ref previous) = self.state {
            if previous.origin() == completion.origin()
                && previous.transaction() == completion.transaction()
                && previous.integrity() == completion.integrity()
                && previous.cipher() == completion.cipher()
                && previous.selection() == completion.selection()
            {
                return Ok(CompletionResult::Duplicate);
            }
            return Err(ExchangeError::ConflictingCompletion);
        }
        let result = activate_and_report(&completion).map_err(ExchangeError::Consumer)?;
        self.changed_at = now;
        self.state = State::Completed(Box::new(completion));
        Ok(CompletionResult::Activated(result))
    }

    /// Completion does not retire old keys. RLC acknowledgment and activation overlap belong to the bearer owner.
    pub fn completion(&self) -> Option<Completion> {
        match &self.state {
            State::Completed(completion) => Some(**completion),
            _ => None,
        }
    }

    pub fn abort_with<T, E>(
        &mut self,
        now: Instant,
        reason: AbortReason,
        restore_old: impl FnOnce(KeyOrigin, AbortReason) -> Result<T, E>,
    ) -> Result<T, ExchangeError<E>> {
        self.time(now)?;
        if !matches!(self.state, State::Prepared | State::AwaitingResponse) {
            return Err(ExchangeError::Phase);
        }
        let result = restore_old(self.origin(), reason).map_err(ExchangeError::Consumer)?;
        self.changed_at = now;
        self.state = State::Aborted(reason);
        Ok(result)
    }

    pub fn expire_with<T, E>(
        &mut self,
        now: Instant,
        restore_old: impl FnOnce(KeyOrigin, AbortReason) -> Result<T, E>,
    ) -> Result<Option<T>, ExchangeError<E>> {
        self.time(now)?;
        if !matches!(self.state, State::Prepared | State::AwaitingResponse) || now < self.deadline {
            return Ok(None);
        }
        self.abort_with(now, AbortReason::Timeout, restore_old)
            .map(Some)
    }

    fn time<E>(&self, now: Instant) -> Result<(), ExchangeError<E>> {
        if now < self.changed_at {
            Err(ExchangeError::Time)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompletionResult<T> {
    Activated(T),
    Duplicate,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ExchangeError<E> {
    Time,
    Phase,
    Expired,
    ConflictingCompletion,
    Message(Error),
    Protection(StreamError<E>),
    Consumer(E),
}
