// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const MAXIMUM_ATTENUATION_DB: usize = 60;
const TPC_STEP_DB: f32 = 1.0;
// The nominal power fits every handset the cell can hold. Fewer handsets leave the rest unused.
pub(crate) const MAXIMUM_BOOST_DB: u8 = 6;

#[derive(Clone, Copy, Debug)]
pub struct DownlinkPowerSettings {
    pub initial_offset_db: f32,
    pub minimum_offset_db: f32,
    pub maximum_age_chips: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownlinkCommand {
    Up,
    Down,
    Hold,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DownlinkControl {
    pub identity: ReceiveIdentity,
    pub measurement_end: ChipInstant,
    pub command: DownlinkCommand,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DownlinkPowerCounters {
    pub up: u64,
    pub down: u64,
    pub hold: u64,
    pub clamped: u64,
    pub stale: u64,
    pub attenuation_db: u8,
    pub applied_attenuation_db: u8,
    pub boost_db: u8,
    pub maximum_measurement_to_preparation_chips: u64,
}

#[derive(Clone, Copy)]
pub(super) struct DownlinkPower {
    gains: [f32; MAXIMUM_ATTENUATION_DB + 1],
    maximum_attenuation: u8,
    boost_limit: u8,
    maximum_age_chips: u64,
    last: Option<ChipInstant>,
    last_applied: Option<ChipInstant>,
    pub counters: DownlinkPowerCounters,
}

impl DownlinkPower {
    pub fn new(settings: DownlinkPowerSettings) -> Result<Self, TransmitError> {
        let range = settings.initial_offset_db - settings.minimum_offset_db;
        if !settings.initial_offset_db.is_finite()
            || !settings.minimum_offset_db.is_finite()
            || !(0.0..=MAXIMUM_ATTENUATION_DB as f32).contains(&range)
            || settings.maximum_age_chips == 0
        {
            return Err(TransmitError::Mapping);
        }
        Ok(Self {
            gains: std::array::from_fn(|index| 10.0_f32.powf(-(index as f32) * TPC_STEP_DB / 20.0)),
            maximum_attenuation: (range / TPC_STEP_DB).floor() as u8,
            boost_limit: 0,
            maximum_age_chips: settings.maximum_age_chips,
            last: None,
            last_applied: None,
            counters: DownlinkPowerCounters::default(),
        })
    }

    pub fn observe(&mut self, observation: DownlinkControl, now: ChipInstant) {
        let at = observation.measurement_end;
        if at.epoch != now.epoch
            || at.index > now.index
            || now.index - at.index > self.maximum_age_chips
            || self.last.is_some_and(|last| last.index >= at.index)
        {
            self.counters.stale = self.counters.stale.saturating_add(1);
            return;
        }
        self.last = Some(at);
        let attenuation = self.counters.attenuation_db;
        // TS 25.214 5.2.1.2.2 uses one 1 dB update per slot in DPC_MODE 0.
        match observation.command {
            DownlinkCommand::Up => {
                self.counters.up = self.counters.up.saturating_add(1);
                self.counters.attenuation_db = attenuation.saturating_sub(1);
                if attenuation == 0 {
                    if self.counters.boost_db < self.boost_limit {
                        self.counters.boost_db += 1;
                    } else {
                        self.counters.clamped = self.counters.clamped.saturating_add(1);
                    }
                }
            }
            DownlinkCommand::Down => {
                self.counters.down = self.counters.down.saturating_add(1);
                if self.counters.boost_db > 0 {
                    self.counters.boost_db -= 1;
                } else {
                    self.counters.attenuation_db =
                        attenuation.saturating_add(1).min(self.maximum_attenuation);
                    if attenuation == self.maximum_attenuation {
                        self.counters.clamped = self.counters.clamped.saturating_add(1);
                    }
                }
            }
            DownlinkCommand::Hold => self.counters.hold = self.counters.hold.saturating_add(1),
        }
    }

    pub fn latch_gain(&mut self, slot: ChipInstant) -> f32 {
        if let Some(last) = self.last
            && self.last_applied != Some(last)
        {
            self.counters.maximum_measurement_to_preparation_chips = self
                .counters
                .maximum_measurement_to_preparation_chips
                .max(slot.index.saturating_sub(last.index));
            self.last_applied = Some(last);
        }
        self.counters.applied_attenuation_db = self.counters.attenuation_db;
        self.gains[usize::from(self.counters.attenuation_db)]
            / self.gains[usize::from(self.counters.boost_db)]
    }

    pub fn limit_boost(&mut self, limit: u8) {
        self.boost_limit = limit.min(MAXIMUM_BOOST_DB);
        self.counters.boost_db = self.counters.boost_db.min(self.boost_limit);
    }

    pub fn inherit(&mut self, previous: Self) {
        self.last = previous.last;
        self.last_applied = previous.last_applied;
        self.counters = previous.counters;
        self.counters.attenuation_db = self.counters.attenuation_db.min(self.maximum_attenuation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dedicated_tx::tests::identity, test_support::at};

    #[test]
    fn boost_above_nominal_is_bounded_by_its_limit_and_returns_first() {
        let mut power = DownlinkPower::new(DownlinkPowerSettings {
            initial_offset_db: 0.0,
            minimum_offset_db: -6.0,
            maximum_age_chips: 10_000,
        })
        .unwrap();
        let mut slot = 0;
        let mut step = |power: &mut DownlinkPower, command| {
            slot += 1;
            power.observe(
                DownlinkControl {
                    identity: identity(0, 1),
                    measurement_end: at(slot),
                    command,
                },
                at(slot),
            );
        };
        step(&mut power, DownlinkCommand::Up);
        assert_eq!((power.counters.boost_db, power.counters.clamped), (0, 1));
        power.limit_boost(9);
        for _ in 0..8 {
            step(&mut power, DownlinkCommand::Up);
        }
        assert_eq!(power.counters.boost_db, MAXIMUM_BOOST_DB);
        assert!((power.latch_gain(at(100)) - 10.0_f32.powf(6.0 / 20.0)).abs() < 1e-5);
        step(&mut power, DownlinkCommand::Down);
        assert_eq!(
            (power.counters.boost_db, power.counters.attenuation_db),
            (5, 0)
        );
        power.limit_boost(2);
        assert_eq!(power.counters.boost_db, 2);
    }

    #[test]
    fn commands_clamp_hold_and_reject_replay_without_absolute_power_thresholds() {
        let mut power = DownlinkPower::new(DownlinkPowerSettings {
            initial_offset_db: -6.0,
            minimum_offset_db: -8.0,
            maximum_age_chips: CHIPS_PER_FRAME,
        })
        .unwrap();
        for (index, command) in [
            DownlinkCommand::Down,
            DownlinkCommand::Down,
            DownlinkCommand::Down,
            DownlinkCommand::Up,
            DownlinkCommand::Hold,
        ]
        .into_iter()
        .enumerate()
        {
            power.observe(
                DownlinkControl {
                    identity: identity(0, 1),
                    measurement_end: at(index as u64 + 1),
                    command,
                },
                at(10),
            );
        }
        assert_eq!(power.counters.attenuation_db, 1);
        assert_eq!(power.counters.clamped, 1);
        assert_eq!(power.counters.hold, 1);
        assert!((power.latch_gain(at(20)) - 10.0_f32.powf(-1.0 / 20.0)).abs() < 1e-6);
        power.observe(
            DownlinkControl {
                identity: identity(0, 1),
                measurement_end: at(5),
                command: DownlinkCommand::Up,
            },
            at(20),
        );
        assert_eq!(power.counters.stale, 1);
        assert_eq!(power.counters.attenuation_db, 1);
    }
}
