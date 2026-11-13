// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use umts_radio::clock_model::ClockBounds;
use umts_radio::pacing::LeadBudget;
use umts_types::time::SampleClock;

const FAILED: u64 = u64::MAX;
const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockFailure {
    Unavailable,
    Reversed { previous: u64, observed: u64 },
    Exhausted,
    InvalidBounds,
}

#[derive(Clone)]
pub(super) struct ClockReader(Arc<PublishedClock>);
struct PublishedClock {
    earliest: AtomicU64,
    latest: AtomicU64,
}
pub(super) struct ClockWriter {
    published: Arc<PublishedClock>,
    last: u64,
    earliest: u64,
}

pub(super) fn clock_channel(first: u64) -> Result<(ClockReader, ClockWriter), ClockFailure> {
    if first == FAILED {
        return Err(ClockFailure::Exhausted);
    }
    let published = Arc::new(PublishedClock {
        earliest: AtomicU64::new(first),
        latest: AtomicU64::new(first),
    });
    Ok((
        ClockReader(published.clone()),
        ClockWriter {
            published,
            last: first,
            earliest: first,
        },
    ))
}

impl ClockReader {
    pub fn read(&self) -> Result<u64, ClockFailure> {
        match self.0.latest.load(Ordering::SeqCst) {
            FAILED => Err(ClockFailure::Unavailable),
            tick => Ok(tick),
        }
    }
    pub fn read_bounds(&self) -> Result<ClockBounds, ClockFailure> {
        // Lower is loaded first and published last. Mixed generations only widen the interval.
        let earliest = self.0.earliest.load(Ordering::SeqCst);
        let latest = self.read()?;
        Ok(ClockBounds { earliest, latest })
    }
    pub fn same_clock(&self, writer: &ClockWriter) -> bool {
        Arc::ptr_eq(&self.0, &writer.published)
    }
}

impl ClockWriter {
    pub(super) fn bounds(&self) -> ClockBounds {
        ClockBounds {
            earliest: self.earliest,
            latest: self.last,
        }
    }

    pub fn observe(&mut self, tick: u64) -> Result<(), ClockFailure> {
        if self.published.latest.load(Ordering::SeqCst) == FAILED {
            return Err(ClockFailure::Unavailable);
        }
        let result = if tick == FAILED {
            Err(ClockFailure::Exhausted)
        } else if tick < self.last {
            Err(ClockFailure::Reversed {
                previous: self.last,
                observed: tick,
            })
        } else {
            self.last = tick;
            self.earliest = tick;
            self.published.latest.store(tick, Ordering::SeqCst);
            self.published.earliest.store(tick, Ordering::SeqCst);
            return Ok(());
        };
        self.published.latest.store(FAILED, Ordering::SeqCst);
        result
    }

    pub fn observe_bounds(&mut self, bounds: ClockBounds) -> Result<(), ClockFailure> {
        if self.published.latest.load(Ordering::SeqCst) == FAILED {
            return Err(ClockFailure::Unavailable);
        }
        if bounds.earliest > bounds.latest || bounds.latest == FAILED {
            self.published.latest.store(FAILED, Ordering::SeqCst);
            return Err(ClockFailure::InvalidBounds);
        }
        // The upper bound never decreases, so a tighter calibration cannot shrink it.
        self.last = self.last.max(bounds.latest);
        self.earliest = self.earliest.max(bounds.earliest);
        self.published.latest.store(self.last, Ordering::SeqCst);
        self.published
            .earliest
            .store(self.earliest, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for ClockWriter {
    fn drop(&mut self) {
        self.published.latest.store(FAILED, Ordering::SeqCst);
    }
}

pub(super) struct PreparationPacing {
    pub clock: SampleClock,
    pub budget: LeadBudget,
    pub observed: ClockReader,
}

pub(super) struct SubmissionPacing {
    pub budget: LeadBudget,
    pub observed: ClockWriter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UncertainWaitError {
    InvalidBounds,
    InvalidWindow,
    DeadlinePassed,
    Overflow,
}

fn checked_wait_duration(
    ticks: u64,
    tick_rate: std::num::NonZeroU64,
    maximum: std::time::Duration,
) -> Result<std::time::Duration, UncertainWaitError> {
    if ticks == 0 {
        return Ok(std::time::Duration::ZERO);
    }
    let nanos = u128::from(ticks)
        .checked_mul(NANOSECONDS_PER_SECOND)
        .ok_or(UncertainWaitError::Overflow)?
        / u128::from(tick_rate.get());
    let capped = nanos.min(maximum.as_nanos());
    let nanos = u64::try_from(capped).map_err(|_| UncertainWaitError::Overflow)?;
    Ok(std::time::Duration::from_nanos(nanos))
}

pub(super) fn bounded_uncertain_wait_duration(
    readiness_tick: u64,
    deadline_tick: u64,
    bounds: ClockBounds,
    tick_rate: std::num::NonZeroU64,
    maximum: std::time::Duration,
) -> Result<std::time::Duration, UncertainWaitError> {
    if bounds.earliest > bounds.latest {
        return Err(UncertainWaitError::InvalidBounds);
    }
    if readiness_tick > deadline_tick {
        return Err(UncertainWaitError::InvalidWindow);
    }
    if bounds.latest > deadline_tick {
        return Err(UncertainWaitError::DeadlinePassed);
    }
    let readiness_remaining = readiness_tick.saturating_sub(bounds.earliest);
    let deadline_remaining = deadline_tick - bounds.latest;
    checked_wait_duration(
        readiness_remaining.min(deadline_remaining),
        tick_rate,
        maximum,
    )
}

#[cfg(test)]
mod tests;
