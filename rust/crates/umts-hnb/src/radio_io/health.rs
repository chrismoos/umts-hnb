// SPDX-License-Identifier: AGPL-3.0-or-later

use sdr_hal::TxRadioHealth;
use std::{num::NonZeroU64, time::Duration};

const NANOS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug)]
pub struct TxHealthSettings {
    pub poll_interval: Duration,
    pub maximum_underflows: u64,
    pub maximum_late_packets: u64,
    pub maximum_unknown_events: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxHealthCounter {
    Underflow,
    LatePacket,
    SequenceError,
    BurstAck,
    DroppedPacket,
    UnknownEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxHealthFailure {
    InvalidInterval,
    InvalidState,
    CounterReset(TxHealthCounter),
    Budget {
        counter: TxHealthCounter,
        observed: u64,
        maximum: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxHealthSnapshot {
    pub underflows: u64,
    pub late_packets: u64,
    pub sequence_errors: u64,
    pub burst_acks: u64,
    pub dropped_packets: u64,
    pub unknown_events: u64,
}

impl From<TxRadioHealth> for TxHealthSnapshot {
    fn from(value: TxRadioHealth) -> Self {
        Self {
            underflows: value.underflows,
            late_packets: value.late_packets,
            sequence_errors: value.sequence_errors,
            burst_acks: value.burst_acks,
            dropped_packets: value.dropped_packets,
            unknown_events: value.unknown_events,
        }
    }
}

pub(super) struct HealthMonitor {
    settings: TxHealthSettings,
    interval_ticks: u64,
    sampled_tick: Option<u64>,
    pub last: TxHealthSnapshot,
}

impl HealthMonitor {
    pub fn new(
        settings: TxHealthSettings,
        rate: NonZeroU64,
        initial: TxHealthSnapshot,
    ) -> Result<Self, TxHealthFailure> {
        let ticks = settings
            .poll_interval
            .as_nanos()
            .checked_mul(u128::from(rate.get()))
            .map(|ticks| ticks.div_ceil(NANOS_PER_SECOND))
            .and_then(|ticks| u64::try_from(ticks).ok())
            .filter(|ticks| *ticks != 0)
            .ok_or(TxHealthFailure::InvalidInterval)?;
        Ok(Self {
            settings,
            interval_ticks: ticks,
            sampled_tick: None,
            last: initial,
        })
    }
    pub fn due(&self, tick: u64) -> bool {
        self.sampled_tick
            .is_none_or(|previous| tick.saturating_sub(previous) >= self.interval_ticks)
    }
    pub fn observe(&mut self, tick: u64, current: TxHealthSnapshot) -> Result<(), TxHealthFailure> {
        use TxHealthCounter::*;
        let counters = [
            (
                Underflow,
                self.last.underflows,
                current.underflows,
                Some(self.settings.maximum_underflows),
            ),
            (
                LatePacket,
                self.last.late_packets,
                current.late_packets,
                Some(self.settings.maximum_late_packets),
            ),
            (
                SequenceError,
                self.last.sequence_errors,
                current.sequence_errors,
                Some(0),
            ),
            (BurstAck, self.last.burst_acks, current.burst_acks, None),
            (
                DroppedPacket,
                self.last.dropped_packets,
                current.dropped_packets,
                Some(0),
            ),
            (
                UnknownEvent,
                self.last.unknown_events,
                current.unknown_events,
                Some(self.settings.maximum_unknown_events),
            ),
        ];
        let result = counters
            .into_iter()
            .try_for_each(|(counter, previous, observed, maximum)| {
                let delta = observed
                    .checked_sub(previous)
                    .ok_or(TxHealthFailure::CounterReset(counter))?;
                if let Some(maximum) = maximum
                    && delta > maximum
                {
                    return Err(TxHealthFailure::Budget {
                        counter,
                        observed: delta,
                        maximum,
                    });
                }
                Ok(())
            });
        self.last = current;
        self.sampled_tick = Some(tick);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> TxHealthSettings {
        TxHealthSettings {
            poll_interval: Duration::from_millis(10),
            maximum_underflows: 1,
            maximum_late_packets: 1,
            maximum_unknown_events: 0,
        }
    }
    #[test]
    fn polling_is_tick_bounded_and_isolated_underflows_are_not_sustained_failure() {
        let mut monitor = HealthMonitor::new(
            settings(),
            NonZeroU64::new(1_000_000).unwrap(),
            TxHealthSnapshot::default(),
        )
        .unwrap();
        assert!(monitor.due(100));
        let first = TxHealthSnapshot {
            underflows: 1,
            burst_acks: 9,
            ..TxHealthSnapshot::default()
        };
        monitor.observe(100, first).unwrap();
        assert!(!monitor.due(10099));
        assert!(monitor.due(10100));
        monitor
            .observe(
                10100,
                TxHealthSnapshot {
                    underflows: 2,
                    ..first
                },
            )
            .unwrap();
        assert_eq!(
            monitor.observe(
                20100,
                TxHealthSnapshot {
                    underflows: 4,
                    ..first
                }
            ),
            Err(TxHealthFailure::Budget {
                counter: TxHealthCounter::Underflow,
                observed: 2,
                maximum: 1
            })
        );
    }
    #[test]
    fn sequence_loss_drops_unknown_events_and_counter_reset_are_explicit() {
        for counter in [
            TxHealthCounter::SequenceError,
            TxHealthCounter::DroppedPacket,
            TxHealthCounter::UnknownEvent,
        ] {
            let mut monitor =
                HealthMonitor::new(settings(), NonZeroU64::MIN, TxHealthSnapshot::default())
                    .unwrap();
            let mut value = TxHealthSnapshot::default();
            match counter {
                TxHealthCounter::SequenceError => value.sequence_errors = 1,
                TxHealthCounter::DroppedPacket => value.dropped_packets = 1,
                _ => value.unknown_events = 1,
            }
            assert_eq!(
                monitor.observe(1, value),
                Err(TxHealthFailure::Budget {
                    counter,
                    observed: 1,
                    maximum: 0
                })
            );
        }
        let initial = TxHealthSnapshot {
            burst_acks: 10,
            ..TxHealthSnapshot::default()
        };
        let mut monitor = HealthMonitor::new(settings(), NonZeroU64::MIN, initial).unwrap();
        assert_eq!(
            monitor.observe(1, TxHealthSnapshot::default()),
            Err(TxHealthFailure::CounterReset(TxHealthCounter::BurstAck))
        );
        assert!(
            HealthMonitor::new(
                TxHealthSettings {
                    poll_interval: Duration::ZERO,
                    ..settings()
                },
                NonZeroU64::MIN,
                initial
            )
            .is_err()
        );
        assert!(
            HealthMonitor::new(
                TxHealthSettings {
                    poll_interval: Duration::MAX,
                    ..settings()
                },
                NonZeroU64::new(u64::MAX).unwrap(),
                initial
            )
            .is_err()
        );
    }
}
