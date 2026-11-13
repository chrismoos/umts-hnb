// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::Error;
use umts_types::{Direction, UeKey};

const START_BITS: u32 = 20;
const START_SHIFT: u32 = u32::BITS - START_BITS;
const INTEGRITY_SN_BITS: u32 = 4;
const AM_SN_BITS: u32 = 12;
const UM_SN_BITS: u32 = 7;
const CFN_BITS: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Start(u32);
impl Start {
    pub fn new(value: u32) -> Result<Self, Error> {
        if value < 1 << START_BITS {
            Ok(Self(value))
        } else {
            Err(Error::Count)
        }
    }
    pub fn initial_count(self) -> u32 {
        self.0 << START_SHIFT
    }
    pub fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CounterInitialization {
    NewKeys,
    ReusedKeys(Start),
}
impl CounterInitialization {
    pub fn start(self) -> Start {
        match self {
            Self::NewKeys => Start(0),
            Self::ReusedKeys(start) => start,
        }
    }
    pub fn integrity_first_count(self, sequence: u8, previous: Option<u8>) -> Result<u32, Error> {
        let modulus = 1 << INTEGRITY_SN_BITS;
        if u32::from(sequence) >= modulus || previous.is_some_and(|sn| u32::from(sn) >= modulus) {
            return Err(Error::Count);
        }
        // TS 25.331 8.5.10 increments HFN on a wrap even immediately after reinitialization.
        let base = self
            .start()
            .initial_count()
            .checked_add(if previous.is_some_and(|sn| sequence < sn) {
                modulus
            } else {
                0
            })
            .ok_or(Error::Exhausted)?;
        Ok(base | u32::from(sequence))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherMode {
    Acknowledged,
    Unacknowledged,
    Transparent,
}
impl CipherMode {
    pub fn sequence_bits(self) -> u32 {
        match self {
            Self::Acknowledged => AM_SN_BITS,
            Self::Unacknowledged => UM_SN_BITS,
            Self::Transparent => CFN_BITS,
        }
    }
    pub fn compose(self, hfn: u32, sequence: u16) -> Result<u32, Error> {
        let bits = self.sequence_bits();
        if hfn >= 1 << (u32::BITS - bits) || u32::from(sequence) >= 1 << bits {
            return Err(Error::Count);
        }
        Ok((hfn << bits) | u32::from(sequence))
    }
}

// The initial value is the first COUNT to publish, not an already transmitted COUNT.
pub struct IntegrityCounter {
    key: UeKey,
    direction: Direction,
    next: Option<u32>,
}
impl IntegrityCounter {
    pub fn new(key: UeKey, direction: Direction, start: Start) -> Self {
        Self::from_count(key, direction, start.initial_count())
    }
    pub fn from_count(key: UeKey, direction: Direction, first_count: u32) -> Self {
        Self {
            key,
            direction,
            next: Some(first_count),
        }
    }
    pub fn next(&self) -> Result<u32, Error> {
        self.next.ok_or(Error::Exhausted)
    }
    pub fn sequence(&self) -> Result<u8, Error> {
        Ok((self.next()? & ((1 << INTEGRITY_SN_BITS) - 1)) as u8)
    }
    pub fn publish_with<T, E>(
        &mut self,
        key: UeKey,
        direction: Direction,
        publish: impl FnOnce(u32) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        if key != self.key || direction != self.direction {
            return Err(PublishError::Counter(Error::Origin));
        }
        let count = self.next().map_err(PublishError::Counter)?;
        let result = publish(count).map_err(PublishError::Downstream)?;
        self.next = count.checked_add(1);
        Ok(result)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    Counter(Error),
    Downstream(E),
}

pub struct IntegrityReceiver {
    key: UeKey,
    direction: Direction,
    hfn: u32,
    last_sequence: Option<u8>,
    first_floor: Option<u32>,
}

impl IntegrityReceiver {
    pub fn from_next_count(key: UeKey, direction: Direction, next_count: u32) -> Self {
        let previous = next_count.checked_sub(1);
        Self {
            key,
            direction,
            hfn: previous.map_or(0, |count| count >> INTEGRITY_SN_BITS),
            last_sequence: previous.map(|count| (count & ((1 << INTEGRITY_SN_BITS) - 1)) as u8),
            first_floor: Some(next_count),
        }
    }

    pub fn last_count(&self) -> Option<u32> {
        self.last_sequence
            .map(|sequence| (self.hfn << INTEGRITY_SN_BITS) | u32::from(sequence))
    }

    /// `last_sequence` is the previous RRC SN, including across an HFN reinitialization.
    pub fn new(
        key: UeKey,
        direction: Direction,
        start: Start,
        last_sequence: Option<u8>,
    ) -> Result<Self, Error> {
        if last_sequence.is_some_and(|sequence| u32::from(sequence) >= 1 << INTEGRITY_SN_BITS) {
            return Err(Error::Count);
        }
        Ok(Self {
            key,
            direction,
            hfn: start.initial_count() >> INTEGRITY_SN_BITS,
            last_sequence,
            first_floor: None,
        })
    }

    pub fn last_sequence(&self) -> Option<u8> {
        self.last_sequence
    }

    // Failed authentication or upper-layer publication must not consume a sequence number.
    pub fn receive_with<T, E>(
        &mut self,
        key: UeKey,
        direction: Direction,
        sequence: u8,
        authenticate_and_accept: impl FnOnce(u32) -> Result<T, E>,
    ) -> Result<T, ReceiveError<E>> {
        if key != self.key || direction != self.direction {
            return Err(ReceiveError::Counter(Error::Origin));
        }
        if u32::from(sequence) >= 1 << INTEGRITY_SN_BITS {
            return Err(ReceiveError::Counter(Error::Count));
        }
        if self.first_floor.is_none() && self.last_sequence == Some(sequence) {
            return Err(ReceiveError::Duplicate);
        }
        // TS 25.331 8.5.10 increments HFN on SN wrap even after HFN reinitialization.
        let hfn = self
            .hfn
            .checked_add(u32::from(
                self.last_sequence.is_some_and(|last| sequence < last),
            ))
            .filter(|hfn| *hfn < 1 << (u32::BITS - INTEGRITY_SN_BITS))
            .ok_or(ReceiveError::Counter(Error::Exhausted))?;
        let count = if let Some(floor) = self.first_floor {
            let mask = (1 << INTEGRITY_SN_BITS) - 1;
            floor
                .checked_add(u32::from(sequence).wrapping_sub(floor) & mask)
                .ok_or(ReceiveError::Counter(Error::Exhausted))?
        } else {
            (hfn << INTEGRITY_SN_BITS) | u32::from(sequence)
        };
        let result = authenticate_and_accept(count).map_err(ReceiveError::Rejected)?;
        self.hfn = count >> INTEGRITY_SN_BITS;
        self.last_sequence = Some(sequence);
        self.first_floor = None;
        Ok(result)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReceiveError<E> {
    Counter(Error),
    Duplicate,
    Rejected(E),
}

#[cfg(test)]
mod tests;
