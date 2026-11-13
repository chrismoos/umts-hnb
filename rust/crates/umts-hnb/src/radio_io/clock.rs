// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AirClockOrigin, ClockFailure, pacing::ClockReader};
use umts_types::time::{ChipInstant, TimeError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RadioTime {
    pub tick: u64,
    pub chip: Option<ChipInstant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioClockError {
    Clock(ClockFailure),
    Time(TimeError),
}

pub struct RadioClock {
    pub(super) origin: AirClockOrigin,
    pub(super) observed: ClockReader,
}

impl RadioClock {
    #[cfg(test)]
    pub(crate) fn controlled(
        origin: AirClockOrigin,
        tick: u64,
    ) -> (Self, impl FnMut(u64) -> Result<(), ClockFailure>) {
        let (observed, mut writer) = super::pacing::clock_channel(tick).unwrap();
        (Self { origin, observed }, move |tick| writer.observe(tick))
    }
    pub fn origin(&self) -> AirClockOrigin {
        self.origin
    }

    /// Controller time uses the lower bound to avoid processing radio events before their device time.
    pub fn read(&self) -> Result<RadioTime, RadioClockError> {
        let tick = self
            .observed
            .read_bounds()
            .map_err(RadioClockError::Clock)?
            .earliest;
        self.at(tick)
    }

    /// Deadline and feedback admission use the upper bound, not controller time.
    pub fn read_upper_bound(&self) -> Result<RadioTime, RadioClockError> {
        let tick = self
            .observed
            .read_bounds()
            .map_err(RadioClockError::Clock)?
            .latest;
        self.at(tick)
    }

    fn at(&self, tick: u64) -> Result<RadioTime, RadioClockError> {
        let Some(elapsed) = tick.checked_sub(self.origin.tick) else {
            return Ok(RadioTime { tick, chip: None });
        };
        // Floor to the last elapsed chip. TX sample position and receive-filter progress are not the device clock.
        let chips = self
            .origin
            .chip_rate
            .ticks_to_samples_floor(elapsed, self.origin.tick_rate)
            .map_err(RadioClockError::Time)?;
        let index = self
            .origin
            .chip
            .index
            .checked_add(chips)
            .ok_or(RadioClockError::Time(TimeError::Overflow))?;
        Ok(RadioTime {
            tick,
            chip: Some(ChipInstant {
                index,
                ..self.origin.chip
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{radio_io::pacing::clock_channel, test_support::at};
    use std::num::NonZeroU64;
    use umts_types::time::{CHIPS_PER_SECOND, SampleRate};

    #[test]
    fn uncertainty_is_not_substituted_for_elapsed_controller_time() {
        let origin = AirClockOrigin {
            chip: at(0),
            chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
            tick: 1000,
            tick_rate: NonZeroU64::new(30_720_000).unwrap(),
        };
        let (observed, mut writer) = clock_channel(900).unwrap();
        let clock = RadioClock { origin, observed };
        writer
            .observe_bounds(umts_radio::clock_model::ClockBounds {
                earliest: 990,
                latest: 1020,
            })
            .unwrap();
        assert_eq!(clock.read().unwrap().chip, None);
        assert_eq!(clock.read_upper_bound().unwrap().chip, Some(at(2)));
        writer
            .observe_bounds(umts_radio::clock_model::ClockBounds {
                earliest: 1016,
                latest: 1032,
            })
            .unwrap();
        assert_eq!(clock.read().unwrap().chip, Some(at(2)));
        assert_eq!(clock.read_upper_bound().unwrap().chip, Some(at(4)));
    }

    #[test]
    fn device_tick_conversion_is_calibrated_floored_and_does_not_invent_preroll_time() {
        let (observed, mut writer) = clock_channel(999).unwrap();
        let clock = RadioClock {
            origin: AirClockOrigin {
                chip: at(38400),
                chip_rate: umts_types::time::SampleRate::new(umts_types::time::CHIPS_PER_SECOND, 1)
                    .unwrap(),
                tick: 1000,
                tick_rate: NonZeroU64::new(1_000_000_000).unwrap(),
            },
            observed,
        };
        assert_eq!(
            clock.read().unwrap(),
            RadioTime {
                tick: 999,
                chip: None
            }
        );
        for (tick, index) in [
            (1000, 38400),
            (1260, 38400),
            (1261, 38401),
            (10_001_000, 76800),
        ] {
            writer.observe(tick).unwrap();
            assert_eq!(
                clock.read().unwrap(),
                RadioTime {
                    tick,
                    chip: Some(at(index))
                }
            );
        }
        drop(writer);
        assert_eq!(
            clock.read(),
            Err(RadioClockError::Clock(ClockFailure::Unavailable))
        );
    }

    #[test]
    fn controller_clock_rejects_overflow_and_writer_reversal() {
        let (observed, mut writer) = clock_channel(0).unwrap();
        let clock = RadioClock {
            origin: AirClockOrigin {
                chip: at(u64::MAX),
                chip_rate: umts_types::time::SampleRate::new(umts_types::time::CHIPS_PER_SECOND, 1)
                    .unwrap(),
                tick: 0,
                tick_rate: NonZeroU64::MIN,
            },
            observed,
        };
        writer.observe(1).unwrap();
        assert_eq!(
            clock.read(),
            Err(RadioClockError::Time(TimeError::Overflow))
        );
        assert!(writer.observe(0).is_err());
        assert_eq!(
            clock.read(),
            Err(RadioClockError::Clock(ClockFailure::Unavailable))
        );
    }
}
