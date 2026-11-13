// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::dedicated_tx::{
    DedicatedTransmitPort, TransmitCommandId, TransmitCommandOutcome, TransmitError, TransmitEvent,
};
use crate::hsdpa_tx::PreparedTransmission;
use std::num::NonZeroUsize;
use umts_mac::hsdpa::TransmissionKey;
use umts_types::UeKey;

const MAX_COMMANDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Queue(TransmissionKey),
    Order(crate::hsdpa_tx::OrderIdentity),
    OrderCompletion,
    Completion,
    BeginRetirement(UeKey),
    FinishRetirement(UeKey),
}

pub struct CommandResult {
    pub operation: Operation,
    pub event: TransmitEvent,
}

impl CommandResult {
    pub fn matches_request(&self) -> bool {
        match (&self.operation, &self.event.outcome) {
            (Operation::Queue(expected), TransmitCommandOutcome::HsdpaQueued { key, result }) => {
                expected == key
                    && result
                        .as_ref()
                        .err()
                        .is_none_or(|rejected| rejected.transmission.transmission().key == *key)
            }
            (Operation::Completion, TransmitCommandOutcome::HsdpaCompletion(_)) => true,
            (
                Operation::Order(expected),
                TransmitCommandOutcome::HsdpaOrderQueued { key, result },
            ) => {
                expected == key
                    && result
                        .as_ref()
                        .err()
                        .is_none_or(|rejected| rejected.order.identity() == *key)
            }
            (Operation::OrderCompletion, TransmitCommandOutcome::HsdpaOrderCompletion(_)) => true,
            (
                Operation::BeginRetirement(expected),
                TransmitCommandOutcome::HsdpaRetirementStarted { ue, .. },
            )
            | (
                Operation::FinishRetirement(expected),
                TransmitCommandOutcome::HsdpaRetirementFinished { ue, .. },
            ) => expected == ue,
            (_, TransmitCommandOutcome::Closed) => true,
            _ => false,
        }
    }
}

struct Pending {
    id: TransmitCommandId,
    operation: Operation,
    event: Option<TransmitEvent>,
}

pub(crate) struct Commands {
    pending: Box<[Option<Pending>]>,
    cursor: usize,
}

impl Commands {
    pub(crate) fn new(capacity: NonZeroUsize) -> Result<Self, TransmitError> {
        if capacity.get() > MAX_COMMANDS {
            return Err(TransmitError::Capacity);
        }
        Ok(Self {
            pending: (0..capacity.get()).map(|_| None).collect(),
            cursor: 0,
        })
    }

    pub(crate) fn port<'a>(&'a mut self, port: &'a mut DedicatedTransmitPort) -> ControlPort<'a> {
        ControlPort {
            commands: self,
            port,
        }
    }

    pub(crate) fn route(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        let Some(pending) = self
            .pending
            .iter_mut()
            .flatten()
            .find(|pending| pending.id == event.id && pending.event.is_none())
        else {
            return Some(event);
        };
        // Even a malformed result retains its original command and resource ownership.
        pending.event = Some(event);
        None
    }
}

pub struct ControlPort<'a> {
    commands: &'a mut Commands,
    port: &'a mut DedicatedTransmitPort,
}

impl ControlPort<'_> {
    pub fn available(&self) -> bool {
        self.port.available()
    }

    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.commands.pending.iter().flatten().count()
    }

    pub fn queue(
        &mut self,
        transmission: PreparedTransmission,
    ) -> Result<TransmitCommandId, PreparedTransmission> {
        let Some(slot) = self.commands.pending.iter_mut().find(|slot| slot.is_none()) else {
            return Err(transmission);
        };
        let operation = Operation::Queue(transmission.transmission().key);
        let id = self.port.queue_hsdpa(transmission)?;
        *slot = Some(Pending {
            id,
            operation,
            event: None,
        });
        Ok(id)
    }

    pub fn queue_order(
        &mut self,
        order: crate::hsdpa_tx::PreparedOrder,
    ) -> Result<TransmitCommandId, crate::hsdpa_tx::PreparedOrder> {
        let Some(slot) = self.commands.pending.iter_mut().find(|slot| slot.is_none()) else {
            return Err(order);
        };
        let operation = Operation::Order(order.identity());
        let id = self.port.queue_hsdpa_order(order)?;
        *slot = Some(Pending {
            id,
            operation,
            event: None,
        });
        Ok(id)
    }

    pub fn request_order_completion(&mut self) -> Result<TransmitCommandId, TransmitError> {
        if self
            .commands
            .pending
            .iter()
            .flatten()
            .any(|pending| pending.operation == Operation::OrderCompletion)
        {
            return Err(TransmitError::Phase);
        }
        self.post(Operation::OrderCompletion)
    }

    pub fn request_completion(&mut self) -> Result<TransmitCommandId, TransmitError> {
        if self
            .commands
            .pending
            .iter()
            .flatten()
            .any(|pending| pending.operation == Operation::Completion)
        {
            return Err(TransmitError::Phase);
        }
        self.post(Operation::Completion)
    }

    pub fn begin_retirement(&mut self, ue: UeKey) -> Result<TransmitCommandId, TransmitError> {
        self.post(Operation::BeginRetirement(ue))
    }

    pub fn finish_retirement(&mut self, ue: UeKey) -> Result<TransmitCommandId, TransmitError> {
        self.post(Operation::FinishRetirement(ue))
    }

    fn post(&mut self, operation: Operation) -> Result<TransmitCommandId, TransmitError> {
        let slot = self
            .commands
            .pending
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(TransmitError::Capacity)?;
        let id = match operation {
            Operation::Completion => self.port.take_hsdpa_completion()?,
            Operation::OrderCompletion => self.port.take_hsdpa_order_completion()?,
            Operation::BeginRetirement(ue) => self.port.begin_hsdpa_retirement(ue)?,
            Operation::FinishRetirement(ue) => self.port.finish_hsdpa_retirement(ue)?,
            Operation::Queue(_) | Operation::Order(_) => {
                unreachable!("queue publication retains the waveform")
            }
        };
        *slot = Some(Pending {
            id,
            operation,
            event: None,
        });
        Ok(id)
    }

    pub fn take_result(&mut self) -> Option<CommandResult> {
        self.take_matching(false)
    }

    pub fn take_order_result(&mut self) -> Option<CommandResult> {
        self.take_matching(true)
    }

    fn take_matching(&mut self, orders: bool) -> Option<CommandResult> {
        for _ in 0..self.commands.pending.len() {
            let index = self.commands.cursor;
            self.commands.cursor = (index + 1) % self.commands.pending.len();
            let slot = &mut self.commands.pending[index];
            if slot.as_ref().is_some_and(|pending| {
                pending.event.is_some()
                    && matches!(
                        pending.operation,
                        Operation::Order(_) | Operation::OrderCompletion
                    ) == orders
            }) {
                let pending = slot.take().expect("completed command");
                return Some(CommandResult {
                    operation: pending.operation,
                    event: pending.event.expect("completed result"),
                });
            }
        }
        None
    }
}
