// SPDX-License-Identifier: AGPL-3.0-or-later

use super::Command;
use crate::dedicated_tx::{DedicatedTransmitPort, TransmitCommandId, TransmitError, TransmitEvent};
use std::num::NonZeroUsize;
use umts_types::RadioEpoch;

const MAX_COMMANDS: usize = 256;

#[cfg(test)]
mod tests;

pub trait Control {
    fn epoch(&self) -> RadioEpoch;
    fn available(&self) -> bool;
    fn queue_hsupa(&mut self, command: Command) -> Result<TransmitCommandId, Command>;
}

impl Control for DedicatedTransmitPort {
    fn epoch(&self) -> RadioEpoch {
        DedicatedTransmitPort::epoch(self)
    }
    fn available(&self) -> bool {
        DedicatedTransmitPort::available(self)
    }
    fn queue_hsupa(&mut self, command: Command) -> Result<TransmitCommandId, Command> {
        DedicatedTransmitPort::queue_hsupa(self, command)
    }
}

struct Pending {
    id: TransmitCommandId,
    result: Option<TransmitEvent>,
}

pub(crate) struct Commands {
    pending: Box<[Option<Pending>]>,
    head: usize,
    length: usize,
}

impl Commands {
    pub(crate) fn new(capacity: NonZeroUsize) -> Result<Self, TransmitError> {
        if capacity.get() > MAX_COMMANDS {
            return Err(TransmitError::Capacity);
        }
        Ok(Self {
            pending: (0..capacity.get()).map(|_| None).collect(),
            head: 0,
            length: 0,
        })
    }

    pub(crate) fn port<'a>(&'a mut self, port: &'a mut DedicatedTransmitPort) -> ControlPort<'a> {
        ControlPort {
            commands: self,
            port,
        }
    }

    pub(crate) fn route(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        let mut index = self.head;
        for _ in 0..self.length {
            let pending = self.pending[index].as_mut().expect("queued command owner");
            if pending.id == event.id && pending.result.is_none() {
                // Retain unexpected outcomes too so their resource owners cannot disappear during routing.
                pending.result = Some(event);
                return None;
            }
            index += 1;
            if index == self.pending.len() {
                index = 0;
            }
        }
        Some(event)
    }
}

pub struct ControlPort<'a> {
    commands: &'a mut Commands,
    port: &'a mut DedicatedTransmitPort,
}

impl Control for ControlPort<'_> {
    fn epoch(&self) -> RadioEpoch {
        self.port.epoch()
    }
    fn available(&self) -> bool {
        self.port.available()
    }
    fn queue_hsupa(&mut self, command: Command) -> Result<TransmitCommandId, Command> {
        if self.commands.length == self.commands.pending.len() {
            return Err(command);
        }
        let tail = (self.commands.head + self.commands.length) % self.commands.pending.len();
        let slot = &mut self.commands.pending[tail];
        debug_assert!(slot.is_none());
        let id = self.port.queue_hsupa(command)?;
        *slot = Some(Pending { id, result: None });
        self.commands.length += 1;
        Ok(id)
    }
}

impl ControlPort<'_> {
    #[cfg(test)]
    pub fn outstanding(&self) -> usize {
        self.commands.length
    }

    pub fn take_result(&mut self) -> Option<TransmitEvent> {
        if self.commands.length == 0 {
            return None;
        }
        // Admission replies must establish flight ownership before a later completion is delivered.
        let slot = &mut self.commands.pending[self.commands.head];
        let result = slot.as_mut().expect("oldest command owner").result.take()?;
        *slot = None;
        self.commands.head = (self.commands.head + 1) % self.commands.pending.len();
        self.commands.length -= 1;
        Some(result)
    }
}
