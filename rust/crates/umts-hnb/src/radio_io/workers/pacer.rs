// SPDX-License-Identifier: AGPL-3.0-or-later

use std::time::Duration;

const FLOOR_NS: u64 = 50_000;
const MAX_NS: u64 = 2_000_000;
const OVERSLEEP_HEADROOM_NS: u64 = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SleepMargin {
    margin_ns: u64,
}

impl SleepMargin {
    pub(crate) const fn new() -> Self {
        Self {
            margin_ns: FLOOR_NS,
        }
    }

    pub(crate) fn margin(&self) -> Duration {
        Duration::from_nanos(self.margin_ns)
    }

    pub(crate) fn observe(&mut self, requested: Duration, actual: Duration) {
        let oversleep_ns = actual.saturating_sub(requested).as_nanos();
        let target = oversleep_ns
            .min(u128::from(MAX_NS))
            .saturating_add(u128::from(OVERSLEEP_HEADROOM_NS))
            .min(u128::from(MAX_NS)) as u64;
        if target > self.margin_ns {
            self.margin_ns = target;
            return;
        }
        let excess = self.margin_ns.saturating_sub(FLOOR_NS);
        if excess != 0 {
            self.margin_ns = self
                .margin_ns
                .saturating_sub((excess >> 8).max(1))
                .max(FLOOR_NS);
        }
    }

    pub(crate) fn sleep_duration(&self, permitted: Duration) -> Duration {
        permitted.saturating_sub(self.margin())
    }
}

impl Default for SleepMargin {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_margin_subtracts_from_permitted_duration() {
        let pacer = SleepMargin::new();
        assert_eq!(pacer.margin(), Duration::from_micros(50));
        assert_eq!(
            pacer.sleep_duration(Duration::from_micros(100)),
            Duration::from_micros(50)
        );
    }

    #[test]
    fn oversleep_raises_margin_with_cap_and_zero_floors_sleep() {
        let mut pacer = SleepMargin::new();
        pacer.observe(Duration::from_micros(100), Duration::from_millis(3));
        assert_eq!(pacer.margin(), Duration::from_millis(2));
        assert_eq!(
            pacer.sleep_duration(Duration::from_millis(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn small_positive_jitter_decays_toward_floor_without_crossing_it() {
        let mut pacer = SleepMargin::new();
        pacer.observe(Duration::from_micros(100), Duration::from_millis(3));
        let raised = pacer.margin();
        pacer.observe(Duration::from_micros(100), Duration::from_micros(105));
        assert!(pacer.margin() < raised);
        for _ in 0..4096 {
            pacer.observe(Duration::from_micros(100), Duration::from_micros(105));
        }
        assert_eq!(pacer.margin(), Duration::from_micros(50));
    }
}
