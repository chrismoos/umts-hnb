// SPDX-License-Identifier: AGPL-3.0-or-later

use super::Error;
use crate::counters::{CipherMode, Start};

pub(super) const AM_SEQUENCE_MASK: u32 = (1 << 12) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Frame positions are extended CFNs, not SFNs or hardware sample timestamps.
pub enum Position {
    Sequence(u64),
    Frame(u64),
}

impl Position {
    pub fn get(self, mode: CipherMode) -> Result<u64, Error> {
        match (self, mode) {
            (Self::Frame(frame), CipherMode::Transparent) => Ok(frame),
            (Self::Sequence(sequence), CipherMode::Acknowledged | CipherMode::Unacknowledged) => {
                Ok(sequence)
            }
            _ => Err(Error::Mode),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CountAnchor {
    mode: CipherMode,
    position: Position,
    count: u32,
    progression: Progression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Progression {
    Running,
    FrozenHyperframe,
    Stopped,
}

impl CountAnchor {
    pub fn new(mode: CipherMode, position: Position, count: u32) -> Result<Self, Error> {
        let raw = position.get(mode)?;
        let mask = (1 << mode.sequence_bits()) - 1;
        if raw & u64::from(mask) != u64::from(count & mask) {
            return Err(Error::Sequence);
        }
        Ok(Self {
            mode,
            position,
            count,
            progression: Progression::Running,
        })
    }

    // TS 25.331 8.6.4.3 keeps HFN fixed until the first TM COUNT-C activation.
    pub fn transparent_frozen_hfn(frame: u64, start: Start) -> Result<Self, Error> {
        let mut anchor = Self::from_start(CipherMode::Transparent, Position::Frame(frame), start)?;
        anchor.progression = Progression::FrozenHyperframe;
        Ok(anchor)
    }

    // TS 25.331 8.6.4.3 does not advance COUNT-C while domain ciphering is not started.
    pub fn transparent_stopped(frame: u64, start: Start) -> Result<Self, Error> {
        let mut anchor = Self::transparent_frozen_hfn(frame, start)?;
        anchor.progression = Progression::Stopped;
        Ok(anchor)
    }

    pub(super) fn running(self) -> bool {
        self.progression == Progression::Running
    }

    pub(super) fn stopped(self) -> bool {
        self.progression == Progression::Stopped
    }

    pub(super) fn agrees_with(self, other: Self, position: Position) -> Result<bool, Error> {
        Ok(self.mode == other.mode
            && self.progression == other.progression
            && self.count_at(position)? == other.count_at(position)?)
    }

    pub fn from_start(mode: CipherMode, position: Position, start: Start) -> Result<Self, Error> {
        let raw = position.get(mode)?;
        let sequence = (raw & ((1 << mode.sequence_bits()) - 1)) as u32;
        Self::new(mode, position, start.initial_count() | sequence)
    }

    pub fn mode(self) -> CipherMode {
        self.mode
    }
    pub fn position(self) -> Position {
        self.position
    }

    // Transport ordering survives key changes. COUNT/HFN initialization must not reset RLC windows.
    pub fn count_at(self, position: Position) -> Result<u32, Error> {
        let delta = position
            .get(self.mode)?
            .checked_sub(self.position.get(self.mode)?)
            .ok_or(Error::Position)?;
        match self.progression {
            Progression::Running => self
                .count
                .checked_add(u32::try_from(delta).map_err(|_| Error::Exhausted)?)
                .ok_or(Error::Exhausted),
            Progression::FrozenHyperframe => {
                let mask = (1 << self.mode.sequence_bits()) - 1;
                Ok((self.count & !mask) | (position.get(self.mode)? & u64::from(mask)) as u32)
            }
            Progression::Stopped => Ok(self.count),
        }
    }
}

pub fn next_occurrence(
    mode: CipherMode,
    sequence: u16,
    floor: Position,
) -> Result<Position, Error> {
    let mask = (1 << mode.sequence_bits()) - 1;
    if u32::from(sequence) > mask {
        return Err(Error::Sequence);
    }
    let floor = floor.get(mode)?;
    let offset = (u64::from(sequence).wrapping_sub(floor)) & u64::from(mask);
    let raw = floor.checked_add(offset).ok_or(Error::Exhausted)?;
    Ok(match mode {
        CipherMode::Transparent => Position::Frame(raw),
        _ => Position::Sequence(raw),
    })
}

#[cfg(test)]
mod tests;
