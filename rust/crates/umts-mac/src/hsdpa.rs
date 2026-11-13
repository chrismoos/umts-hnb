// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::{NonZeroU8, NonZeroU64};
use umts_types::{
    UeKey,
    time::{CHIPS_PER_SLOT, ChipInstant},
};

mod ehs;
mod packet;
pub use ehs::{EhsPackedPdu, EhsReorderingPdu, EhsSdu, EhsSegmentation, EhsTsnWidth, pack_ehs};
const CHIPS_PER_HS_TTI: u64 = 3 * CHIPS_PER_SLOT;
pub use packet::{DecodedPdu, MacdPdu, PackedPdu, PacketQueue, QueueFormat, QueuePublishError};
mod entity;
pub use entity::{EntityError, HarqEntity, RejectedPayload};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessId(u8);

impl ProcessId {
    pub fn new(value: u8) -> Result<Self, HarqError> {
        if value >= 8 {
            return Err(HarqError::Process);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransmissionKey {
    ue: UeKey,
    incarnation: NonZeroU64,
    process: ProcessId,
    sequence: u64,
}

impl TransmissionKey {
    pub fn ue(self) -> UeKey {
        self.ue
    }

    pub fn process(self) -> ProcessId {
        self.process
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarqError {
    Process,
    State,
    Time,
    Stale,
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feedback {
    Ack,
    Nack,
    Dtx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessState {
    Empty,
    Ready,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledTransmission {
    pub key: TransmissionKey,
    pub new_data: bool,
    pub downlink: ChipInstant,
    pub expected_ack: ChipInstant,
}

enum State {
    Empty,
    Ready,
    Scheduled(ScheduledTransmission),
    Awaiting(ScheduledTransmission),
}

pub struct HarqProcess<T> {
    ue: UeKey,
    incarnation: NonZeroU64,
    process: ProcessId,
    next_sequence: u64,
    last_transmitted_ndi: Option<bool>,
    transmitted_count: u64,
    block_ndi: bool,
    payload: Option<T>,
    state: State,
}

impl<T> HarqProcess<T> {
    // The owner must use a fresh incarnation when recreating processes for the same UE.
    pub fn new(ue: UeKey, incarnation: NonZeroU64, process: ProcessId) -> Self {
        Self {
            ue,
            incarnation,
            process,
            next_sequence: 0,
            last_transmitted_ndi: None,
            transmitted_count: 0,
            block_ndi: false,
            payload: None,
            state: State::Empty,
        }
    }

    // TS 25.321 11.6.1 permits replacement, but the displaced block must retain an owner.
    pub fn replace(&mut self, payload: T) -> Option<T> {
        self.transmitted_count = 0;
        self.block_ndi = self.last_transmitted_ndi.is_some_and(|value| !value);
        self.state = State::Ready;
        self.payload.replace(payload)
    }

    pub fn payload(&self) -> Option<&T> {
        self.payload.as_ref()
    }

    pub fn transmitted_count(&self) -> u64 {
        self.transmitted_count
    }

    pub fn schedule(
        &mut self,
        downlink: ChipInstant,
        expected_ack: ChipInstant,
    ) -> Result<ScheduledTransmission, HarqError> {
        if !matches!(self.state, State::Ready) {
            return Err(HarqError::State);
        }
        if downlink.epoch != self.ue.epoch
            || expected_ack.epoch != self.ue.epoch
            || expected_ack.index <= downlink.index
        {
            return Err(HarqError::Time);
        }
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(HarqError::Exhausted)?;
        let transmission = ScheduledTransmission {
            key: TransmissionKey {
                ue: self.ue,
                incarnation: self.incarnation,
                process: self.process,
                sequence,
            },
            new_data: self.block_ndi,
            downlink,
            expected_ack,
        };
        self.next_sequence = sequence;
        self.state = State::Scheduled(transmission);
        Ok(transmission)
    }

    // A scheduler reservation alone is not evidence that the radio accepted the transmission.
    pub fn transmitted(&mut self, key: TransmissionKey) -> Result<(), HarqError> {
        let State::Scheduled(transmission) = self.state else {
            return Err(HarqError::State);
        };
        if transmission.key != key {
            return Err(HarqError::Stale);
        }
        self.transmitted_count = self
            .transmitted_count
            .checked_add(1)
            .ok_or(HarqError::Exhausted)?;
        self.last_transmitted_ndi = Some(self.block_ndi);
        self.state = State::Awaiting(transmission);
        Ok(())
    }

    pub fn lost(&mut self, key: TransmissionKey) -> Result<(), HarqError> {
        let transmission = match self.state {
            State::Scheduled(value) | State::Awaiting(value) => value,
            _ => return Err(HarqError::State),
        };
        if transmission.key != key {
            return Err(HarqError::Stale);
        }
        self.state = State::Ready;
        Ok(())
    }

    pub fn feedback(
        &mut self,
        key: TransmissionKey,
        observed_ack: ChipInstant,
        feedback: Feedback,
    ) -> Result<Option<T>, HarqError> {
        let State::Awaiting(transmission) = self.state else {
            return Err(HarqError::State);
        };
        if transmission.key != key {
            return Err(HarqError::Stale);
        }
        if observed_ack != transmission.expected_ack {
            return Err(HarqError::Time);
        }
        match feedback {
            Feedback::Ack => {
                self.state = State::Empty;
                self.transmitted_count = 0;
                Ok(self.payload.take())
            }
            Feedback::Nack | Feedback::Dtx => {
                self.state = State::Ready;
                Ok(None)
            }
        }
    }

    pub fn cancel(&mut self) -> Option<T> {
        self.state = State::Empty;
        self.transmitted_count = 0;
        self.payload.take()
    }

    // A processed-receive watermark, not wall clock, proves the ACK window has closed.
    pub fn feedback_missing(
        &mut self,
        key: TransmissionKey,
        processed_through: ChipInstant,
    ) -> Result<(), HarqError> {
        self.feedback_missing_repeated(key, processed_through, NonZeroU8::MIN)
    }

    fn feedback_missing_repeated(
        &mut self,
        key: TransmissionKey,
        processed_through: ChipInstant,
        repetitions: NonZeroU8,
    ) -> Result<(), HarqError> {
        if !matches!(self.state, State::Awaiting(_)) {
            return Err(HarqError::State);
        }
        let end = self.feedback_end(key, repetitions)?;
        if processed_through.epoch != self.ue.epoch || processed_through.index < end.index {
            return Err(HarqError::Time);
        }
        self.state = State::Ready;
        Ok(())
    }

    fn feedback_end(
        &self,
        key: TransmissionKey,
        repetitions: NonZeroU8,
    ) -> Result<ChipInstant, HarqError> {
        let transmission = match self.state {
            State::Scheduled(transmission) | State::Awaiting(transmission) => transmission,
            _ => return Err(HarqError::State),
        };
        if transmission.key != key {
            return Err(HarqError::Stale);
        }
        let end = transmission
            .expected_ack
            .index
            .checked_add((u64::from(repetitions.get()) - 1) * CHIPS_PER_HS_TTI + CHIPS_PER_SLOT)
            .ok_or(HarqError::Time)?;
        Ok(ChipInstant {
            epoch: self.ue.epoch,
            index: end,
        })
    }
}
