// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;

use crate::RadioEpoch;

pub const CHIPS_PER_SECOND: u64 = 3_840_000;
pub const CHIPS_PER_SLOT: u64 = 2_560;
pub const SLOTS_PER_FRAME: u64 = 15;
pub const CHIPS_PER_FRAME: u64 = CHIPS_PER_SLOT * SLOTS_PER_FRAME;
pub const SYSTEM_FRAME_MODULUS: u64 = 4_096;
pub const CONNECTION_FRAME_MODULUS: u64 = 256;
const MICRO_HZ_PER_HZ: u64 = 1_000_000;
const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeError {
    InvalidRate,
    Overflow,
    BeforeOrigin,
    WrongEpoch,
    OverlappingSamples,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleRate {
    numerator: NonZeroU64,
    denominator: NonZeroU64,
}

impl SampleRate {
    pub fn new(numerator: u64, denominator: u64) -> Result<Self, TimeError> {
        let numerator = NonZeroU64::new(numerator).ok_or(TimeError::InvalidRate)?;
        let denominator = NonZeroU64::new(denominator).ok_or(TimeError::InvalidRate)?;
        let common = gcd(numerator.get(), denominator.get());
        Ok(Self {
            numerator: NonZeroU64::new(numerator.get() / common)
                .expect("nonzero reduced numerator"),
            denominator: NonZeroU64::new(denominator.get() / common)
                .expect("nonzero reduced denominator"),
        })
    }

    /// Represents driver rate readback to the nearest microhertz.
    pub fn from_hz(hz: f64) -> Result<Self, TimeError> {
        let scaled = hz * MICRO_HZ_PER_HZ as f64;
        if !scaled.is_finite() || scaled < 1.0 || scaled >= u64::MAX as f64 {
            return Err(TimeError::InvalidRate);
        }
        Self::new(scaled.round() as u64, MICRO_HZ_PER_HZ)
    }

    pub fn hz(self) -> f64 {
        self.numerator.get() as f64 / self.denominator.get() as f64
    }

    pub fn multiplied_by(self, factor: NonZeroU64) -> Result<Self, TimeError> {
        let common = gcd(factor.get(), self.denominator.get());
        let numerator = self
            .numerator
            .get()
            .checked_mul(factor.get() / common)
            .ok_or(TimeError::Overflow)?;
        Self::new(numerator, self.denominator.get() / common)
    }

    pub fn divided_by(self, divisor: NonZeroU64) -> Result<Self, TimeError> {
        let common = gcd(self.numerator.get(), divisor.get());
        let denominator = self
            .denominator
            .get()
            .checked_mul(divisor.get() / common)
            .ok_or(TimeError::Overflow)?;
        Self::new(self.numerator.get() / common, denominator)
    }

    pub fn ticks_to_samples_floor(
        self,
        ticks: u64,
        tick_rate: NonZeroU64,
    ) -> Result<u64, TimeError> {
        let (numerator, denominator) = self.samples_at_ticks(ticks, tick_rate)?;
        u64::try_from(numerator / denominator).map_err(|_| TimeError::Overflow)
    }

    pub fn ticks_to_samples_ceil(
        self,
        ticks: u64,
        tick_rate: NonZeroU64,
    ) -> Result<u64, TimeError> {
        let (numerator, denominator) = self.samples_at_ticks(ticks, tick_rate)?;
        u64::try_from(numerator.div_ceil(denominator)).map_err(|_| TimeError::Overflow)
    }

    fn samples_at_ticks(
        self,
        ticks: u64,
        tick_rate: NonZeroU64,
    ) -> Result<(u128, u128), TimeError> {
        let numerator = u128::from(ticks)
            .checked_mul(u128::from(self.numerator.get()))
            .ok_or(TimeError::Overflow)?;
        let denominator = u128::from(tick_rate.get())
            .checked_mul(u128::from(self.denominator.get()))
            .ok_or(TimeError::Overflow)?;
        Ok((numerator, denominator))
    }

    pub fn samples_to_ticks(self, samples: u64, tick_rate: NonZeroU64) -> Result<u64, TimeError> {
        let product = u128::from(samples)
            .checked_mul(u128::from(tick_rate.get()))
            .and_then(|n| n.checked_mul(u128::from(self.denominator.get())))
            .ok_or(TimeError::Overflow)?;
        rounded_div(product, u128::from(self.numerator.get()))
    }
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn rounded_div(numerator: u128, denominator: u128) -> Result<u64, TimeError> {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    let round_up = remainder >= denominator.div_ceil(2);
    u64::try_from(quotient + u128::from(round_up)).map_err(|_| TimeError::Overflow)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleInstant {
    pub epoch: RadioEpoch,
    pub index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipInstant {
    pub epoch: RadioEpoch,
    pub index: u64,
}

impl ChipInstant {
    pub fn system_frame(self) -> u16 {
        ((self.index / CHIPS_PER_FRAME) % SYSTEM_FRAME_MODULUS) as u16
    }

    pub fn connection_frame(self) -> u8 {
        ((self.index / CHIPS_PER_FRAME) % CONNECTION_FRAME_MODULUS) as u8
    }

    pub fn slot(self) -> u8 {
        ((self.index / CHIPS_PER_SLOT) % SLOTS_PER_FRAME) as u8
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SampleClock {
    origin: SampleInstant,
    origin_tick: u64,
    sample_rate: SampleRate,
    tick_rate: NonZeroU64,
}

impl SampleClock {
    pub fn new(
        origin: SampleInstant,
        origin_tick: u64,
        sample_rate: SampleRate,
        tick_rate: NonZeroU64,
    ) -> Self {
        Self {
            origin,
            origin_tick,
            sample_rate,
            tick_rate,
        }
    }

    pub fn tick_at(self, instant: SampleInstant) -> Result<u64, TimeError> {
        if instant.epoch != self.origin.epoch {
            return Err(TimeError::WrongEpoch);
        }
        let samples = instant
            .index
            .checked_sub(self.origin.index)
            .ok_or(TimeError::BeforeOrigin)?;
        self.origin_tick
            .checked_add(self.sample_rate.samples_to_ticks(samples, self.tick_rate)?)
            .ok_or(TimeError::Overflow)
    }

    pub fn with_correction_ns(self, correction: i64) -> Result<Self, TimeError> {
        let ticks = rounded_div(
            u128::from(correction.unsigned_abs()) * u128::from(self.tick_rate.get()),
            NANOSECONDS_PER_SECOND,
        )?;
        let origin_tick = if correction < 0 {
            self.origin_tick.checked_sub(ticks)
        } else {
            self.origin_tick.checked_add(ticks)
        }
        .ok_or(TimeError::Overflow)?;
        Ok(Self {
            origin_tick,
            ..self
        })
    }

    pub fn sample_at_tick(self, tick: u64) -> Result<SampleInstant, TimeError> {
        let delta = tick
            .checked_sub(self.origin_tick)
            .ok_or(TimeError::BeforeOrigin)?;
        let samples = rounded_div(
            u128::from(delta) * u128::from(self.sample_rate.numerator.get()),
            u128::from(self.tick_rate.get()) * u128::from(self.sample_rate.denominator.get()),
        )?;
        Ok(SampleInstant {
            epoch: self.origin.epoch,
            index: self
                .origin
                .index
                .checked_add(samples)
                .ok_or(TimeError::Overflow)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(index: u64) -> SampleInstant {
        SampleInstant {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            index,
        }
    }

    #[test]
    fn equivalent_rates_have_one_canonical_representation() {
        assert_eq!(
            SampleRate::new(7_680_000, 1),
            SampleRate::new(30_720_000, 4)
        );
        assert_eq!(SampleRate::new(u64::MAX, u64::MAX), SampleRate::new(1, 1));
        assert_eq!(SampleRate::new(0, 1), Err(TimeError::InvalidRate));
        assert_eq!(SampleRate::new(1, 0), Err(TimeError::InvalidRate));
        assert_eq!(SampleRate::new(0, 0), Err(TimeError::InvalidRate));
    }

    #[test]
    fn scaling_cancels_before_multiplication_and_rejects_only_unrepresentable_rates() {
        let two = NonZeroU64::new(2).unwrap();
        let integer = SampleRate::new(u64::MAX, 1).unwrap();
        let fractional = SampleRate::new(1, u64::MAX).unwrap();
        assert_eq!(
            SampleRate::new(u64::MAX, 2).unwrap().multiplied_by(two),
            Ok(integer)
        );
        assert_eq!(
            SampleRate::new(2, u64::MAX).unwrap().divided_by(two),
            Ok(fractional)
        );
        assert_eq!(integer.multiplied_by(two), Err(TimeError::Overflow));
        assert_eq!(fractional.divided_by(two), Err(TimeError::Overflow));
        assert_eq!(integer.multiplied_by(NonZeroU64::MIN), Ok(integer));
        assert_eq!(fractional.divided_by(NonZeroU64::MIN), Ok(fractional));
        let rate = SampleRate::from_hz(7_680_000.005_983_839).unwrap();
        assert_eq!(rate.divided_by(two).unwrap().multiplied_by(two), Ok(rate));
        assert_eq!(rate.multiplied_by(two).unwrap().divided_by(two), Ok(rate));
    }

    #[test]
    fn tick_conversion_distinguishes_floor_ceil_and_exact_boundaries() {
        let rate = SampleRate::new(3, 2).unwrap();
        let ticks = NonZeroU64::new(10).unwrap();
        for (input, floor, ceil) in [(0, 0, 0), (19, 2, 3), (20, 3, 3), (21, 3, 4)] {
            assert_eq!(rate.ticks_to_samples_floor(input, ticks), Ok(floor));
            assert_eq!(rate.ticks_to_samples_ceil(input, ticks), Ok(ceil));
        }
        let wide = SampleRate::new(u64::MAX, 1).unwrap();
        let wide_ticks = NonZeroU64::new(u64::MAX).unwrap();
        assert_eq!(
            wide.ticks_to_samples_floor(u64::MAX, wide_ticks),
            Ok(u64::MAX)
        );
        assert_eq!(
            wide.ticks_to_samples_ceil(u64::MAX, wide_ticks),
            Ok(u64::MAX)
        );
        let slow = SampleRate::new(1, u64::MAX).unwrap();
        assert_eq!(slow.ticks_to_samples_floor(u64::MAX, wide_ticks), Ok(0));
        assert_eq!(slow.ticks_to_samples_ceil(u64::MAX, wide_ticks), Ok(1));
    }

    #[test]
    fn ceil_detects_overflow_even_when_the_floor_is_representable() {
        let rate = SampleRate::new(u64::MAX - 1, u64::MAX - 2).unwrap();
        assert_eq!(
            rate.ticks_to_samples_floor(u64::MAX - 1, NonZeroU64::MIN),
            Ok(u64::MAX)
        );
        assert_eq!(
            rate.ticks_to_samples_ceil(u64::MAX - 1, NonZeroU64::MIN),
            Err(TimeError::Overflow)
        );
        let fast = SampleRate::new(u64::MAX, 1).unwrap();
        assert_eq!(
            fast.ticks_to_samples_floor(2, NonZeroU64::MIN),
            Err(TimeError::Overflow)
        );
        assert_eq!(
            fast.ticks_to_samples_ceil(2, NonZeroU64::MIN),
            Err(TimeError::Overflow)
        );
    }

    #[test]
    fn actual_fractional_rate_preserves_microhertz_and_long_run_absolute_positions() {
        let rate = SampleRate::from_hz(7_680_000.005_983_839).unwrap();
        assert_eq!(rate, SampleRate::new(7_680_000_005_984, 1_000_000).unwrap());
        let tick_rate = NonZeroU64::new(30_720_000).unwrap();
        let ten_hours = 36_000 * tick_rate.get();
        assert_eq!(
            rate.ticks_to_samples_floor(ten_hours, tick_rate),
            Ok(276_480_000_215)
        );
        assert_eq!(
            rate.ticks_to_samples_ceil(ten_hours, tick_rate),
            Ok(276_480_000_216)
        );
        let clock = SampleClock::new(at(0), 0, rate, tick_rate);
        for samples in [0, 5_120, 7_680_000, 276_480_000_215, 1_000_000_000_000] {
            let tick = clock.tick_at(at(samples)).unwrap();
            assert_eq!(clock.sample_at_tick(tick), Ok(at(samples)));
            let floor = rate.ticks_to_samples_floor(tick, tick_rate).unwrap();
            let ceil = rate.ticks_to_samples_ceil(tick, tick_rate).unwrap();
            assert!(floor <= samples && samples <= ceil);
            assert!(ceil - floor <= 1);
        }
    }

    #[test]
    fn samples_and_master_clock_ticks_are_not_interchangeable() {
        let clock = SampleClock::new(
            at(0),
            100,
            SampleRate::new(7_680_000, 1).unwrap(),
            NonZeroU64::new(30_720_000).unwrap(),
        );
        assert_eq!(clock.tick_at(at(5_120)), Ok(20_580));
        assert_eq!(clock.tick_at(at(7_680_000)), Ok(30_720_100));
        assert_eq!(clock.sample_at_tick(20_580), Ok(at(5_120)));
        assert_eq!(
            clock.with_correction_ns(1_000_000).unwrap().tick_at(at(0)),
            Ok(30_820)
        );
        assert_eq!(
            clock.with_correction_ns(-1_000).unwrap().tick_at(at(0)),
            Ok(69)
        );
    }

    #[test]
    fn fractional_rate_uses_absolute_positions_without_accumulated_block_rounding() {
        let rate = SampleRate::new(3, 2).unwrap();
        let clock = SampleClock::new(at(0), 0, rate, NonZeroU64::new(10).unwrap());
        assert_eq!(clock.tick_at(at(1)), Ok(7));
        assert_eq!(clock.tick_at(at(2)), Ok(13));
        assert_eq!(clock.tick_at(at(3)), Ok(20));
        assert!(
            (SampleRate::from_hz(7_680_000.005_983_839).unwrap().hz() - 7_680_000.005_983_839)
                .abs()
                < 0.000_001
        );
    }

    #[test]
    fn invalid_epochs_rates_and_overflows_fail_closed() {
        for value in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::MAX] {
            assert_eq!(SampleRate::from_hz(value), Err(TimeError::InvalidRate));
        }
        let clock = SampleClock::new(
            at(10),
            u64::MAX,
            SampleRate::new(1, 1).unwrap(),
            NonZeroU64::new(1).unwrap(),
        );
        assert_eq!(clock.tick_at(at(11)), Err(TimeError::Overflow));
        assert_eq!(clock.tick_at(at(9)), Err(TimeError::BeforeOrigin));
        let mut other = at(10);
        other.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
        assert_eq!(clock.tick_at(other), Err(TimeError::WrongEpoch));
    }

    #[test]
    fn wire_frame_wrap_does_not_wrap_extended_time() {
        let stamp = ChipInstant {
            epoch: at(0).epoch,
            index: SYSTEM_FRAME_MODULUS * CHIPS_PER_FRAME + CHIPS_PER_SLOT,
        };
        assert_eq!(stamp.system_frame(), 0);
        assert_eq!(stamp.connection_frame(), 0);
        assert_eq!(stamp.slot(), 1);
        assert!(stamp.index > u64::from(stamp.system_frame()));
    }
}
