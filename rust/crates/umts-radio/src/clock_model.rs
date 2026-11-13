// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{num::NonZeroU64, time::Duration};

const PARTS_PER_MILLION: u32 = 1_000_000;
const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;
const SCALED_NANOSECONDS_PER_SECOND: u128 = NANOSECONDS_PER_SECOND * PARTS_PER_MILLION as u128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSettings {
    pub maximum_age: Option<Duration>,
    pub maximum_query: Duration,
    pub maximum_drift_ppm: u32,
}

/// Host times are elapsed durations from one unchanged monotonic origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub started: Duration,
    pub finished: Duration,
    pub tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockBounds {
    pub earliest: u64,
    pub latest: u64,
}

/// The device clock had reached `tick` by host time `host`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockFloor {
    pub host: Duration,
    pub tick: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObserveResult {
    Accepted,
    Estimated,
    RejectedSlow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockError {
    InvalidSettings,
    HostOrder,
    TickReversed,
    InvalidTick,
    Unobserved,
    Stale,
    Discontinuity,
    Overflow,
}

#[derive(Clone, Copy)]
pub struct ClockModel {
    tick_rate: NonZeroU64,
    settings: ClockSettings,
    anchor: Option<Anchor>,
    previous: Option<Observation>,
    anchor_estimated: bool,
    slow_readings: u32,
    estimate_conflicts: u32,
}

const SLOW_READINGS_BEFORE_ESTIMATE: u32 = 5;
const CONFLICTS_BEFORE_REANCHOR: u32 = 5;

#[derive(Clone, Copy)]
struct Anchor {
    observation: Observation,
    at_finished: ClockBounds,
}

impl ClockModel {
    pub fn new(tick_rate: NonZeroU64, settings: ClockSettings) -> Result<Self, ClockError> {
        if settings.maximum_query.is_zero()
            || settings.maximum_drift_ppm >= PARTS_PER_MILLION
            || settings
                .maximum_age
                .is_some_and(|age| age.is_zero() || settings.maximum_query > age)
        {
            return Err(ClockError::InvalidSettings);
        }
        Ok(Self {
            tick_rate,
            settings,
            anchor: None,
            previous: None,
            anchor_estimated: false,
            slow_readings: 0,
            estimate_conflicts: 0,
        })
    }

    pub fn observe(&mut self, observation: Observation) -> Result<ObserveResult, ClockError> {
        let fast = observation
            .finished
            .checked_sub(observation.started)
            .is_some_and(|elapsed| elapsed <= self.settings.maximum_query);
        let mut result = self.observe_window(observation);
        if fast && self.anchor_estimated && result == Err(ClockError::Discontinuity) {
            self.anchor = None;
            result = self.observe_window(observation);
        }
        if result == Ok(ObserveResult::Accepted) {
            self.anchor_estimated = false;
            self.slow_readings = 0;
            self.estimate_conflicts = 0;
        }
        result
    }

    /// A query slower than `maximum_query` is placed at its midpoint, an assumption and not a bound.
    pub fn observe_estimated(
        &mut self,
        observation: Observation,
    ) -> Result<ObserveResult, ClockError> {
        let elapsed = observation
            .finished
            .checked_sub(observation.started)
            .ok_or(ClockError::HostOrder)?;
        if elapsed <= self.settings.maximum_query {
            return self.observe(observation);
        }
        self.slow_readings = self.slow_readings.saturating_add(1);
        let bounded_anchor = self.anchor.is_some() && !self.anchor_estimated;
        if bounded_anchor && self.slow_readings < SLOW_READINGS_BEFORE_ESTIMATE {
            return self.observe_window(observation);
        }
        let window = self.settings.maximum_query;
        let started = observation.started + (elapsed - window) / 2;
        let estimate = Observation {
            started,
            finished: started + window,
            tick: observation.tick,
        };
        let mut trial = *self;
        if bounded_anchor {
            trial.anchor = None;
        }
        let mut result = trial.observe_window(estimate);
        if result == Err(ClockError::Discontinuity) {
            self.estimate_conflicts = self.estimate_conflicts.saturating_add(1);
            if self.estimate_conflicts < CONFLICTS_BEFORE_REANCHOR {
                return Ok(ObserveResult::RejectedSlow);
            }
            trial = *self;
            trial.anchor = None;
            result = trial.observe_window(estimate);
        }
        result?;
        *self = trial;
        self.anchor_estimated = true;
        self.estimate_conflicts = 0;
        Ok(ObserveResult::Estimated)
    }

    fn observe_window(&mut self, observation: Observation) -> Result<ObserveResult, ClockError> {
        let elapsed = observation
            .finished
            .checked_sub(observation.started)
            .ok_or(ClockError::HostOrder)?;
        if observation.tick == u64::MAX {
            return Err(ClockError::InvalidTick);
        }
        if let Some(previous) = self.previous {
            if observation.started < previous.finished {
                return Err(ClockError::HostOrder);
            }
            if observation.tick < previous.tick {
                return Err(ClockError::TickReversed);
            }
        }
        let query_upper = self.elapsed_ticks(
            elapsed,
            PARTS_PER_MILLION + self.settings.maximum_drift_ppm,
            true,
        )?;
        let mut at_finished = ClockBounds {
            earliest: observation.tick.saturating_sub(1),
            latest: observation
                .tick
                .checked_add(query_upper)
                .and_then(|tick| tick.checked_add(1))
                .ok_or(ClockError::Overflow)?,
        };
        if let Some(anchor) = self.anchor {
            let earliest = self.bounds_from(anchor, observation.started)?.earliest;
            let predicted = self.bounds_from(anchor, observation.finished)?;
            if observation.tick < earliest || observation.tick > predicted.latest {
                return Err(ClockError::Discontinuity);
            }
            at_finished.earliest = at_finished.earliest.max(predicted.earliest);
            at_finished.latest = at_finished.latest.min(predicted.latest);
            if at_finished.earliest > at_finished.latest {
                return Err(ClockError::Discontinuity);
            }
        }
        self.previous = Some(observation);
        if elapsed > self.settings.maximum_query {
            return Ok(ObserveResult::RejectedSlow);
        }
        self.anchor = Some(Anchor {
            observation,
            at_finished,
        });
        Ok(ObserveResult::Accepted)
    }

    pub fn settings(&self) -> ClockSettings {
        self.settings
    }

    pub fn bounds(&self, now: Duration) -> Result<ClockBounds, ClockError> {
        self.bounds_from(self.anchor.ok_or(ClockError::Unobserved)?, now)
    }

    /// An estimated anchor has no trusted upper bound, so its whole window follows the floor.
    pub fn bounds_above(
        &self,
        now: Duration,
        floor: ClockFloor,
    ) -> Result<ClockBounds, ClockError> {
        let mut bounds = self.bounds(now)?;
        let Some(since) = now.checked_sub(floor.host) else {
            return Ok(bounds);
        };
        let earliest = floor
            .tick
            .checked_add(self.elapsed_ticks(
                since,
                PARTS_PER_MILLION - self.settings.maximum_drift_ppm,
                false,
            )?)
            .ok_or(ClockError::Overflow)?;
        if let Some(raised) = earliest.checked_sub(bounds.earliest) {
            bounds.earliest = earliest;
            bounds.latest = if self.anchor_estimated {
                bounds
                    .latest
                    .checked_add(raised)
                    .ok_or(ClockError::Overflow)?
            } else {
                bounds.latest.max(earliest)
            };
        }
        Ok(bounds)
    }

    pub fn nominal_ticks(&self, duration: Duration) -> Result<u64, ClockError> {
        self.elapsed_ticks(duration, PARTS_PER_MILLION, false)
    }

    fn bounds_from(&self, anchor: Anchor, now: Duration) -> Result<ClockBounds, ClockError> {
        let since_finish = now
            .checked_sub(anchor.observation.finished)
            .ok_or(ClockError::HostOrder)?;
        let since_start = now
            .checked_sub(anchor.observation.started)
            .ok_or(ClockError::HostOrder)?;
        if self
            .settings
            .maximum_age
            .is_some_and(|maximum_age| since_start > maximum_age)
        {
            return Err(ClockError::Stale);
        }
        let lower = self.elapsed_ticks(
            since_finish,
            PARTS_PER_MILLION - self.settings.maximum_drift_ppm,
            false,
        )?;
        let upper = self.elapsed_ticks(
            since_finish,
            PARTS_PER_MILLION + self.settings.maximum_drift_ppm,
            true,
        )?;
        let earliest = anchor
            .at_finished
            .earliest
            .checked_add(lower)
            .ok_or(ClockError::Overflow)?;
        let latest = anchor
            .at_finished
            .latest
            .checked_add(upper)
            .ok_or(ClockError::Overflow)?;
        Ok(ClockBounds { earliest, latest })
    }

    fn elapsed_ticks(
        &self,
        duration: Duration,
        ppm: u32,
        ceiling: bool,
    ) -> Result<u64, ClockError> {
        let scaled = duration
            .as_nanos()
            .checked_mul(u128::from(self.tick_rate.get()))
            .and_then(|ticks| ticks.checked_mul(u128::from(ppm)))
            .ok_or(ClockError::Overflow)?;
        let ticks = if ceiling {
            scaled.div_ceil(SCALED_NANOSECONDS_PER_SECOND)
        } else {
            scaled / SCALED_NANOSECONDS_PER_SECOND
        };
        u64::try_from(ticks).map_err(|_| ClockError::Overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ClockSettings {
        ClockSettings {
            maximum_age: Some(Duration::from_secs(2)),
            maximum_query: Duration::from_millis(10),
            maximum_drift_ppm: 0,
        }
    }
    fn model(hz: u64) -> ClockModel {
        ClockModel::new(NonZeroU64::new(hz).unwrap(), settings()).unwrap()
    }
    fn obs(start_ms: u64, end_ms: u64, tick: u64) -> Observation {
        Observation {
            started: Duration::from_millis(start_ms),
            finished: Duration::from_millis(end_ms),
            tick,
        }
    }

    #[test]
    fn query_bracket_and_fractional_ticks_are_rounded_outward() {
        let mut model = model(1000);
        assert_eq!(model.bounds(Duration::ZERO), Err(ClockError::Unobserved));
        assert_eq!(model.observe(obs(10, 12, 100)), Ok(ObserveResult::Accepted));
        assert_eq!(
            model.bounds(Duration::from_millis(12)),
            Ok(ClockBounds {
                earliest: 99,
                latest: 103
            })
        );
        assert_eq!(
            model.bounds(Duration::from_micros(12_500)),
            Ok(ClockBounds {
                earliest: 99,
                latest: 104
            })
        );
        assert_eq!(
            model.bounds(Duration::from_millis(20)),
            Ok(ClockBounds {
                earliest: 107,
                latest: 111
            })
        );
        assert_eq!(
            model.bounds(Duration::from_millis(11)),
            Err(ClockError::HostOrder)
        );
    }

    #[test]
    fn drift_expands_both_sides_and_exact_age_endpoint_is_fresh() {
        let mut settings = settings();
        settings.maximum_drift_ppm = 1000;
        let mut model = ClockModel::new(NonZeroU64::new(1_000_000).unwrap(), settings).unwrap();
        model.observe(obs(0, 0, 10)).unwrap();
        assert_eq!(
            model.bounds(Duration::from_secs(1)),
            Ok(ClockBounds {
                earliest: 999_009,
                latest: 1_001_011
            })
        );
        assert!(model.bounds(Duration::from_secs(2)).is_ok());
        assert_eq!(
            model.bounds(Duration::from_secs(2) + Duration::from_nanos(1)),
            Err(ClockError::Stale)
        );
    }

    #[test]
    fn successive_measurements_allow_bracket_endpoints_but_reject_jumps_and_stalls() {
        for tick in [199, 202] {
            let mut model = model(1000);
            model.observe(obs(0, 0, 100)).unwrap();
            assert_eq!(
                model.observe(obs(100, 101, tick)),
                Ok(ObserveResult::Accepted)
            );
        }
        for (tick, error) in [
            (203, ClockError::Discontinuity),
            (198, ClockError::Discontinuity),
            (100, ClockError::Discontinuity),
            (99, ClockError::TickReversed),
        ] {
            let mut model = model(1000);
            model.observe(obs(0, 0, 100)).unwrap();
            assert_eq!(model.observe(obs(100, 101, tick)), Err(error));
        }
    }

    #[test]
    fn slow_queries_retain_anchor_and_cannot_hide_host_tick_or_clock_errors() {
        let mut model = model(1000);
        model.observe(obs(0, 0, 100)).unwrap();
        assert_eq!(
            model.observe(obs(10, 30, 120)),
            Ok(ObserveResult::RejectedSlow)
        );
        assert_eq!(
            model.bounds(Duration::from_millis(30)),
            Ok(ClockBounds {
                earliest: 129,
                latest: 131
            })
        );
        assert_eq!(model.observe(obs(29, 50, 140)), Err(ClockError::HostOrder));
        assert_eq!(
            model.observe(obs(30, 51, 119)),
            Err(ClockError::TickReversed)
        );
        assert_eq!(
            model.observe(obs(30, 51, 500)),
            Err(ClockError::Discontinuity)
        );
        assert_eq!(model.observe(obs(30, 31, 130)), Ok(ObserveResult::Accepted));
    }

    #[test]
    fn no_age_expiry_keeps_anchor_across_long_idle_and_slow_query() {
        let mut settings = settings();
        settings.maximum_age = None;
        let mut model = ClockModel::new(NonZeroU64::new(1000).unwrap(), settings).unwrap();
        model.observe(obs(0, 0, 100)).unwrap();

        assert!(model.bounds(Duration::from_secs(3_600)).is_ok());
        assert_eq!(
            model.observe(obs(3_600_000, 3_600_020, 3_600_120)),
            Ok(ObserveResult::RejectedSlow)
        );
        assert!(model.bounds(Duration::from_millis(3_600_020)).is_ok());
        assert_eq!(
            model.observe(obs(3_600_020, 3_600_021, 3_600_121)),
            Ok(ObserveResult::Accepted)
        );
    }

    #[test]
    fn slow_first_query_estimates_the_clock_at_its_midpoint() {
        let mut model = model(1000);
        assert_eq!(
            model.observe_estimated(obs(0, 30, 115)),
            Ok(ObserveResult::Estimated)
        );
        assert_eq!(
            model.bounds(Duration::from_millis(20)),
            Ok(ClockBounds {
                earliest: 114,
                latest: 126
            })
        );
    }

    #[test]
    fn bounded_anchor_ignores_slow_queries_until_they_persist() {
        let mut model = model(1000);
        model.observe(obs(0, 0, 100)).unwrap();
        for index in 1..SLOW_READINGS_BEFORE_ESTIMATE as u64 {
            let start = 100 * index;
            assert_eq!(
                model.observe_estimated(obs(start, start + 30, 115 + start)),
                Ok(ObserveResult::RejectedSlow)
            );
        }
        assert_eq!(
            model.observe_estimated(obs(500, 530, 615)),
            Ok(ObserveResult::Estimated)
        );
        assert_eq!(
            model.bounds(Duration::from_millis(520)),
            Ok(ClockBounds {
                earliest: 614,
                latest: 626
            })
        );
    }

    #[test]
    fn estimated_anchor_is_replaced_only_by_persistent_contradiction() {
        let mut model = model(1000);
        model.observe_estimated(obs(0, 30, 115)).unwrap();
        for index in 1..CONFLICTS_BEFORE_REANCHOR as u64 {
            let start = 100 * index;
            assert_eq!(
                model.observe_estimated(obs(start, start + 30, 100 + start)),
                Ok(ObserveResult::RejectedSlow)
            );
            assert_eq!(
                model.bounds(Duration::from_millis(20)),
                Ok(ClockBounds {
                    earliest: 114,
                    latest: 126
                })
            );
        }
        assert_eq!(
            model.observe_estimated(obs(500, 530, 600)),
            Ok(ObserveResult::Estimated)
        );
        assert_eq!(
            model.bounds(Duration::from_millis(520)),
            Ok(ClockBounds {
                earliest: 599,
                latest: 611
            })
        );
    }

    #[test]
    fn bounded_reading_overrides_a_contradicting_estimate_but_not_a_bounded_anchor() {
        let mut model = model(1000);
        model.observe_estimated(obs(0, 30, 115)).unwrap();
        assert_eq!(
            model.observe(obs(100, 100, 150)),
            Ok(ObserveResult::Accepted)
        );
        assert_eq!(
            model.bounds(Duration::from_millis(100)),
            Ok(ClockBounds {
                earliest: 149,
                latest: 151
            })
        );
        assert_eq!(
            model.observe(obs(200, 200, 400)),
            Err(ClockError::Discontinuity)
        );
    }

    #[test]
    fn slow_first_query_is_not_an_anchor_but_still_orders_later_observations() {
        let mut model = model(1000);
        assert_eq!(
            model.observe(obs(0, 20, 100)),
            Ok(ObserveResult::RejectedSlow)
        );
        assert_eq!(
            model.bounds(Duration::from_millis(20)),
            Err(ClockError::Unobserved)
        );
        assert_eq!(model.observe(obs(19, 20, 101)), Err(ClockError::HostOrder));
        assert_eq!(
            model.observe(obs(20, 20, 99)),
            Err(ClockError::TickReversed)
        );
        assert_eq!(model.observe(obs(20, 20, 100)), Ok(ObserveResult::Accepted));
    }

    #[test]
    fn expired_anchor_is_not_silently_reacquired_even_by_a_slow_query() {
        let mut model = model(1000);
        model.observe(obs(0, 0, 0)).unwrap();
        assert_eq!(model.observe(obs(2000, 2020, 2010)), Err(ClockError::Stale));
        assert_eq!(model.observe(obs(2001, 2001, 2001)), Err(ClockError::Stale));
    }

    #[test]
    fn zero_tick_lower_bound_saturates_but_upper_bound_and_arithmetic_overflow_fail() {
        let mut model = model(1000);
        model.observe(obs(0, 0, 0)).unwrap();
        assert_eq!(
            model.bounds(Duration::ZERO),
            Ok(ClockBounds {
                earliest: 0,
                latest: 1
            })
        );
        assert_eq!(
            model.observe(obs(0, 0, u64::MAX)),
            Err(ClockError::InvalidTick)
        );
        let mut near_limit = self::model(1000);
        near_limit.observe(obs(0, 0, u64::MAX - 2)).unwrap();
        assert_eq!(
            near_limit.bounds(Duration::from_millis(2)),
            Err(ClockError::Overflow)
        );
        let mut settings = settings();
        settings.maximum_age = Some(Duration::MAX);
        let mut huge = ClockModel::new(NonZeroU64::new(u64::MAX).unwrap(), settings).unwrap();
        huge.observe(obs(0, 0, 0)).unwrap();
        assert_eq!(huge.bounds(Duration::MAX), Err(ClockError::Overflow));
    }

    #[test]
    fn renewal_intersects_prior_uncertainty_and_propagates_from_its_finished_bracket() {
        let mut settings = settings();
        settings.maximum_query = Duration::from_millis(150);
        let mut model = ClockModel::new(NonZeroU64::new(1000).unwrap(), settings).unwrap();
        model.observe(obs(0, 0, 100)).unwrap();
        let previous = model.bounds(Duration::from_millis(200)).unwrap();
        assert_eq!(
            previous,
            ClockBounds {
                earliest: 299,
                latest: 301
            }
        );
        assert_eq!(
            model.observe(obs(100, 200, 200)),
            Ok(ObserveResult::Accepted)
        );
        assert_eq!(model.bounds(Duration::from_millis(200)), Ok(previous));
        assert_eq!(
            model.bounds(Duration::from_millis(201)),
            Ok(ClockBounds {
                earliest: 300,
                latest: 302
            })
        );
        assert!(model.bounds(Duration::from_millis(2100)).is_ok());
        assert_eq!(
            model.bounds(Duration::from_millis(2100) + Duration::from_nanos(1)),
            Err(ClockError::Stale)
        );
    }

    #[test]
    fn invalid_settings_host_order_and_query_threshold_are_explicit() {
        for settings in [
            ClockSettings {
                maximum_age: Some(Duration::ZERO),
                ..settings()
            },
            ClockSettings {
                maximum_query: Duration::ZERO,
                ..settings()
            },
            ClockSettings {
                maximum_query: Duration::from_secs(3),
                ..settings()
            },
            ClockSettings {
                maximum_drift_ppm: PARTS_PER_MILLION,
                ..settings()
            },
        ] {
            assert!(matches!(
                ClockModel::new(NonZeroU64::MIN, settings),
                Err(ClockError::InvalidSettings)
            ));
        }
        let mut model = model(1000);
        assert_eq!(model.observe(obs(2, 1, 0)), Err(ClockError::HostOrder));
        assert_eq!(model.observe(obs(0, 10, 5)), Ok(ObserveResult::Accepted));
        assert_eq!(model.observe(obs(10, 20, 15)), Ok(ObserveResult::Accepted));
    }

    fn floor(host_ms: u64, tick: u64) -> ClockFloor {
        ClockFloor {
            host: Duration::from_millis(host_ms),
            tick,
        }
    }

    #[test]
    fn floor_raises_only_the_lower_bound_of_a_bounded_clock() {
        let mut clock = model(1_000);
        clock.observe(obs(0, 1, 100)).unwrap();
        let now = Duration::from_millis(5);
        let plain = clock.bounds(now).unwrap();
        assert_eq!(clock.bounds_above(now, floor(4, 50)).unwrap(), plain);
        assert_eq!(
            clock.bounds_above(now, floor(4, 104)).unwrap(),
            ClockBounds {
                earliest: 105,
                latest: plain.latest
            }
        );
        assert_eq!(
            clock.bounds_above(now, floor(4, 110)).unwrap(),
            ClockBounds {
                earliest: 111,
                latest: 111
            }
        );
        assert_eq!(clock.bounds_above(now, floor(6, 200)).unwrap(), plain);
    }

    #[test]
    fn floor_moves_the_window_of_an_estimated_clock() {
        let mut clock = model(1_000);
        assert_eq!(
            clock.observe_estimated(obs(0, 30, 100)),
            Ok(ObserveResult::Estimated)
        );
        let now = Duration::from_millis(25);
        let plain = clock.bounds(now).unwrap();
        let raised = clock.bounds_above(now, floor(24, 110)).unwrap();
        assert_eq!(raised.earliest, 111);
        assert_eq!(
            raised.latest - raised.earliest,
            plain.latest - plain.earliest
        );
    }
}
