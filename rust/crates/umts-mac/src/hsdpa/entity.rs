// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::{NonZeroU8, NonZeroUsize};
use umts_types::hsdpa::{CapabilityError, Category};

const MAX_ACK_REPETITIONS: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityError {
    Repetition,
    Capability(CapabilityError),
    Harq(HarqError),
}

#[derive(Debug)]
pub struct RejectedPayload<T> {
    pub error: EntityError,
    pub payload: T,
}

struct Process<T> {
    harq: HarqProcess<T>,
    soft_bits: NonZeroUsize,
}

pub struct HarqEntity<T> {
    category: Category,
    origin: ChipInstant,
    processes: Box<[Process<T>]>,
    last_reserved: Option<ChipInstant>,
    ack_repetitions: NonZeroU8,
}

impl<T> HarqEntity<T> {
    pub fn new(
        ue: UeKey,
        incarnation: NonZeroU64,
        category: Category,
        origin: ChipInstant,
        memory: &[NonZeroUsize],
        ack_repetitions: NonZeroU8,
    ) -> Result<Self, EntityError> {
        if ack_repetitions.get() > MAX_ACK_REPETITIONS {
            return Err(EntityError::Repetition);
        }
        category
            .validate_process_memory(memory)
            .map_err(EntityError::Capability)?;
        if origin.epoch != ue.epoch {
            return Err(EntityError::Harq(HarqError::Time));
        }
        let processes = memory
            .iter()
            .enumerate()
            .map(|(index, &soft_bits)| Process {
                harq: HarqProcess::new(ue, incarnation, ProcessId(index as u8)),
                soft_bits,
            })
            .collect();
        Ok(Self {
            category,
            origin,
            processes,
            last_reserved: None,
            ack_repetitions,
        })
    }

    pub fn category(&self) -> Category {
        self.category
    }
    pub fn ack_repetitions(&self) -> NonZeroU8 {
        self.ack_repetitions
    }

    pub fn soft_bits(&self, id: ProcessId) -> Result<NonZeroUsize, EntityError> {
        Ok(self.process(id)?.soft_bits)
    }

    pub fn payload(&self, id: ProcessId) -> Result<Option<&T>, EntityError> {
        Ok(self.process(id)?.harq.payload())
    }

    pub fn transmitted_count(&self, id: ProcessId) -> Result<u64, EntityError> {
        Ok(self.process(id)?.harq.transmitted_count())
    }

    pub fn process_state(&self, id: ProcessId) -> Result<ProcessState, EntityError> {
        Ok(match self.process(id)?.harq.state {
            State::Empty => ProcessState::Empty,
            State::Ready => ProcessState::Ready,
            State::Scheduled(_) | State::Awaiting(_) => ProcessState::Busy,
        })
    }

    pub fn next_downlink(&self, not_before: ChipInstant) -> Result<ChipInstant, EntityError> {
        if not_before.epoch != self.origin.epoch {
            return Err(EntityError::Harq(HarqError::Time));
        }
        let mut earliest = not_before.index.max(self.origin.index);
        if let Some(previous) = self.last_reserved {
            let spacing = u64::from(
                self.category
                    .limits()
                    .minimum_tti_interval
                    .max(self.ack_repetitions.get()),
            ) * CHIPS_PER_HS_TTI;
            earliest = earliest.max(
                previous
                    .index
                    .checked_add(spacing)
                    .ok_or(EntityError::Harq(HarqError::Time))?,
            );
        }
        let offset = earliest - self.origin.index;
        let rounded = offset
            .div_ceil(CHIPS_PER_HS_TTI)
            .checked_mul(CHIPS_PER_HS_TTI)
            .and_then(|offset| self.origin.index.checked_add(offset))
            .ok_or(EntityError::Harq(HarqError::Time))?;
        Ok(ChipInstant {
            epoch: self.origin.epoch,
            index: rounded,
        })
    }

    pub fn reservation(&self, id: ProcessId) -> Result<Option<ScheduledTransmission>, EntityError> {
        Ok(match self.process(id)?.harq.state {
            State::Scheduled(transmission) => Some(transmission),
            _ => None,
        })
    }

    pub fn feedback_end(&self, key: TransmissionKey) -> Result<ChipInstant, EntityError> {
        self.process(key.process)?
            .harq
            .feedback_end(key, self.ack_repetitions)
            .map_err(EntityError::Harq)
    }

    pub fn replace(&mut self, id: ProcessId, payload: T) -> Result<Option<T>, RejectedPayload<T>> {
        let process = match self.process_mut(id) {
            Ok(process) => process,
            Err(error) => return Err(RejectedPayload { error, payload }),
        };
        // Outstanding radio or receiver work still owns the old transmission identity.
        if matches!(process.harq.state, State::Scheduled(_) | State::Awaiting(_)) {
            return Err(RejectedPayload {
                error: EntityError::Harq(HarqError::State),
                payload,
            });
        }
        Ok(process.harq.replace(payload))
    }

    pub fn insert(&mut self, id: ProcessId, payload: T) -> Result<(), RejectedPayload<T>> {
        let process = match self.process_mut(id) {
            Ok(process) => process,
            Err(error) => return Err(RejectedPayload { error, payload }),
        };
        if !matches!(process.harq.state, State::Empty) {
            return Err(RejectedPayload {
                error: EntityError::Harq(HarqError::State),
                payload,
            });
        }
        let displaced = process.harq.replace(payload);
        debug_assert!(displaced.is_none());
        Ok(())
    }

    pub fn schedule(
        &mut self,
        id: ProcessId,
        downlink: ChipInstant,
        expected_ack: ChipInstant,
    ) -> Result<ScheduledTransmission, EntityError> {
        if downlink.epoch != self.origin.epoch
            || downlink
                .index
                .checked_sub(self.origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(CHIPS_PER_HS_TTI))
        {
            return Err(EntityError::Harq(HarqError::Time));
        }
        if let Some(previous) = self.last_reserved {
            self.category
                .validate_spacing(previous, downlink)
                .map_err(EntityError::Capability)?;
            // TS 25.214 6A.1.1 forbids a new HS reception during ACK repetitions.
            if downlink.index - previous.index
                < u64::from(self.ack_repetitions.get()) * CHIPS_PER_HS_TTI
            {
                return Err(EntityError::Harq(HarqError::Time));
            }
        }
        let transmission = self
            .process_mut(id)?
            .harq
            .schedule(downlink, expected_ack)
            .map_err(EntityError::Harq)?;
        // Lost reservations retain their spacing because later reservations may already exist.
        self.last_reserved = Some(downlink);
        Ok(transmission)
    }

    pub fn transmitted(&mut self, key: TransmissionKey) -> Result<(), EntityError> {
        self.process_mut(key.process)?
            .harq
            .transmitted(key)
            .map_err(EntityError::Harq)
    }

    pub fn lost(&mut self, key: TransmissionKey) -> Result<(), EntityError> {
        self.process_mut(key.process)?
            .harq
            .lost(key)
            .map_err(EntityError::Harq)
    }

    pub fn feedback(
        &mut self,
        key: TransmissionKey,
        observed_ack: ChipInstant,
        feedback: Feedback,
    ) -> Result<Option<T>, EntityError> {
        self.process_mut(key.process)?
            .harq
            .feedback(key, observed_ack, feedback)
            .map_err(EntityError::Harq)
    }

    pub fn feedback_missing(
        &mut self,
        key: TransmissionKey,
        processed_through: ChipInstant,
    ) -> Result<(), EntityError> {
        let repetitions = self.ack_repetitions;
        self.process_mut(key.process)?
            .harq
            .feedback_missing_repeated(key, processed_through, repetitions)
            .map_err(EntityError::Harq)
    }

    pub fn cancel(&mut self, id: ProcessId) -> Result<Option<T>, EntityError> {
        let process = self.process_mut(id)?;
        if matches!(process.harq.state, State::Scheduled(_) | State::Awaiting(_)) {
            return Err(EntityError::Harq(HarqError::State));
        }
        Ok(process.harq.cancel())
    }

    pub fn into_payloads(self) -> impl Iterator<Item = T> {
        self.processes
            .into_vec()
            .into_iter()
            .filter_map(|process| process.harq.payload)
    }

    fn process(&self, id: ProcessId) -> Result<&Process<T>, EntityError> {
        self.processes
            .get(usize::from(id.value()))
            .ok_or(EntityError::Harq(HarqError::Process))
    }

    fn process_mut(&mut self, id: ProcessId) -> Result<&mut Process<T>, EntityError> {
        self.processes
            .get_mut(usize::from(id.value()))
            .ok_or(EntityError::Harq(HarqError::Process))
    }
}
