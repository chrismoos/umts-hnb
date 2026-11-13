// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::clock_model::ClockBounds;
use std::{num::NonZeroU64, time::Duration};

const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug)]
pub struct LeadDurations {
    pub minimum_submission: Duration,
    pub maximum_submission: Duration,
    pub maximum_preparation: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadError {
    InvalidBudget,
    InvalidWindow,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Ready,
    WaitUntil(u64),
    Missed { shortfall_ticks: u128 },
}

#[derive(Clone, Copy, Debug)]
pub struct LeadBudget {
    minimum: u64,
    submission: u64,
    preparation: u64,
}

impl LeadBudget {
    pub fn new(
        durations: LeadDurations,
        tick_rate: NonZeroU64,
        maximum_window_ticks: NonZeroU64,
    ) -> Result<Self, LeadError> {
        let minimum = ticks(durations.minimum_submission, tick_rate, true)?;
        let submission = ticks(durations.maximum_submission, tick_rate, false)?;
        let preparation = ticks(durations.maximum_preparation, tick_rate, false)?;
        if minimum == 0
            || preparation < submission
            || minimum
                .checked_add(maximum_window_ticks.get())
                .is_none_or(|required| required > submission)
        {
            return Err(LeadError::InvalidBudget);
        }
        Ok(Self {
            minimum,
            submission,
            preparation,
        })
    }

    pub fn minimum_ticks(self) -> u64 {
        self.minimum
    }
    pub fn maximum_preparation_ticks(self) -> u64 {
        self.preparation
    }

    pub fn submission(self, now: u64, first: u64, end: u64) -> Result<Gate, LeadError> {
        self.submission_bounds(
            ClockBounds {
                earliest: now,
                latest: now,
            },
            first,
            end,
        )
    }

    pub fn preparation(self, now: u64, first: u64, end: u64) -> Result<Gate, LeadError> {
        self.preparation_bounds(
            ClockBounds {
                earliest: now,
                latest: now,
            },
            first,
            end,
        )
    }

    pub fn submission_bounds(
        self,
        now: ClockBounds,
        first: u64,
        end: u64,
    ) -> Result<Gate, LeadError> {
        self.gate(now, first, end, self.submission)
    }

    pub fn preparation_bounds(
        self,
        now: ClockBounds,
        first: u64,
        end: u64,
    ) -> Result<Gate, LeadError> {
        self.gate(now, first, end, self.preparation)
    }

    fn gate(self, now: ClockBounds, first: u64, end: u64, maximum: u64) -> Result<Gate, LeadError> {
        if now.earliest > now.latest || end <= first || end - first > maximum - self.minimum {
            return Err(LeadError::InvalidWindow);
        }
        let deadline = u128::from(now.latest) + u128::from(self.minimum);
        if u128::from(first) < deadline {
            return Ok(Gate::Missed {
                shortfall_ticks: deadline - u128::from(first),
            });
        }
        if end - now.earliest > maximum {
            return Ok(Gate::WaitUntil(end - maximum));
        }
        Ok(Gate::Ready)
    }
}

fn ticks(duration: Duration, rate: NonZeroU64, round_up: bool) -> Result<u64, LeadError> {
    let numerator = duration
        .as_nanos()
        .checked_mul(u128::from(rate.get()))
        .ok_or(LeadError::Overflow)?;
    let value = numerator / NANOSECONDS_PER_SECOND
        + u128::from(round_up && !numerator.is_multiple_of(NANOSECONDS_PER_SECOND));
    u64::try_from(value).map_err(|_| LeadError::Overflow)
}

#[cfg(test)]
mod tests;
