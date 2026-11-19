// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::sync::atomic::{AtomicU64, Ordering};

const BLOCK: usize = 64;
const STRIP_CENTRE_CHIP_RATES: f64 = 0.78;
const BLOCKS_PER_MEASUREMENT: u32 = 1200;
const SMOOTHING: f64 = 1.0 / 16.0;
const QUIET_RATIO_FALL: f64 = 1.0 / 8.0;
const QUIET_RATIO_RISE_PER_MEASUREMENT: f64 = 1.000_001;

static STRIP_NOISE: AtomicU64 = AtomicU64::new(0);
static RECEIVED_POWER: AtomicU64 = AtomicU64::new(0);
static QUIET_RATIO: AtomicU64 = AtomicU64::new(0);

fn load(cell: &AtomicU64) -> Option<f64> {
    let value = f64::from_bits(cell.load(Ordering::Relaxed));
    (value.is_finite() && value > 0.0).then_some(value)
}

#[cfg(test)]
thread_local! {
    static TEST_STRIP_NOISE: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_strip_noise_for_test(value: Option<f64>) {
    TEST_STRIP_NOISE.with(|cell| cell.set(value));
}

pub(crate) fn strip_noise() -> Option<f64> {
    #[cfg(test)]
    return TEST_STRIP_NOISE.with(std::cell::Cell::get);
    #[cfg(not(test))]
    load(&STRIP_NOISE)
}

pub(crate) fn received_power() -> Option<f64> {
    load(&RECEIVED_POWER)
}

pub(crate) fn rise_over_thermal() -> Option<f64> {
    Some(received_power()? / (strip_noise()? * load(&QUIET_RATIO)?))
}

pub(crate) struct NoiseMeter {
    upper: [Complex32; BLOCK],
    lower: [Complex32; BLOCK],
    window_power: f64,
    pending: [Complex32; BLOCK],
    filled: usize,
    upper_sum: f64,
    lower_sum: f64,
    power_sum: f64,
    blocks: u32,
}

impl NoiseMeter {
    pub(crate) fn new(samples_per_chip: usize) -> Self {
        let bin = (BLOCK as f64 * STRIP_CENTRE_CHIP_RATES / samples_per_chip.max(1) as f64).round();
        let mut upper = [Complex32::default(); BLOCK];
        let mut lower = [Complex32::default(); BLOCK];
        let mut window_power = 0.0;
        for index in 0..BLOCK {
            let taper =
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * index as f64 / BLOCK as f64).cos();
            let angle = -2.0 * std::f64::consts::PI * bin * index as f64 / BLOCK as f64;
            upper[index] =
                Complex32::new((taper * angle.cos()) as f32, (taper * angle.sin()) as f32);
            lower[index] = upper[index].conj();
            window_power += taper * taper;
        }
        Self {
            upper,
            lower,
            window_power,
            pending: [Complex32::default(); BLOCK],
            filled: 0,
            upper_sum: 0.0,
            lower_sum: 0.0,
            power_sum: 0.0,
            blocks: 0,
        }
    }

    pub(crate) fn feed(&mut self, samples: &[Complex32]) {
        for sample in samples {
            self.pending[self.filled] = *sample;
            self.filled += 1;
            if self.filled == BLOCK {
                self.filled = 0;
                self.block();
            }
        }
    }

    fn block(&mut self) {
        let mut upper = Complex32::default();
        let mut lower = Complex32::default();
        let mut power = 0.0f32;
        for ((sample, up), down) in self.pending.iter().zip(&self.upper).zip(&self.lower) {
            upper += sample * up;
            lower += sample * down;
            power += sample.norm_sqr();
        }
        if !(upper.norm_sqr() + lower.norm_sqr() + power).is_finite() {
            return;
        }
        self.upper_sum += f64::from(upper.norm_sqr());
        self.lower_sum += f64::from(lower.norm_sqr());
        self.power_sum += f64::from(power);
        self.blocks += 1;
        if self.blocks == BLOCKS_PER_MEASUREMENT {
            self.publish();
        }
    }

    fn take(&mut self) -> Option<(f64, f64)> {
        let blocks = f64::from(self.blocks);
        let strip = self.upper_sum.min(self.lower_sum) / (blocks * self.window_power);
        let power = self.power_sum / (blocks * BLOCK as f64);
        self.upper_sum = 0.0;
        self.lower_sum = 0.0;
        self.power_sum = 0.0;
        self.blocks = 0;
        (strip.is_finite() && strip > 0.0 && power.is_finite() && power > 0.0)
            .then_some((strip, power))
    }

    fn publish(&mut self) {
        let Some((strip, power)) = self.take() else {
            return;
        };
        let smooth = |cell: &AtomicU64, value: f64| {
            let next = load(cell).map_or(value, |old| old + (value - old) * SMOOTHING);
            cell.store(next.to_bits(), Ordering::Relaxed);
            next
        };
        let strip = smooth(&STRIP_NOISE, strip);
        let power = smooth(&RECEIVED_POWER, power);
        let ratio = power / strip;
        let quiet = match load(&QUIET_RATIO) {
            Some(old) if ratio < old => old + (ratio - old) * QUIET_RATIO_FALL,
            Some(old) => old * QUIET_RATIO_RISE_PER_MEASUREMENT,
            None => ratio,
        };
        QUIET_RATIO.store(quiet.to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u64) -> Complex32 {
        let mut next = || {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            (*seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let radius = (-2.0 * next().max(1e-12).ln()).sqrt();
        let angle = 2.0 * std::f64::consts::PI * next();
        Complex32::new((radius * angle.cos()) as f32, (radius * angle.sin()) as f32)
            * std::f32::consts::FRAC_1_SQRT_2
    }

    fn measure(amplitude: f32, tone: f32) -> (f64, f64) {
        let mut meter = NoiseMeter::new(2);
        let mut seed = 0x1234_5678_9abc_def1;
        let samples: Vec<Complex32> = (0..BLOCK * (BLOCKS_PER_MEASUREMENT as usize - 1))
            .map(|index| {
                let phase = 2.0 * std::f32::consts::PI * 0.1 * index as f32;
                noise(&mut seed) * amplitude + Complex32::new(phase.cos(), phase.sin()) * tone
            })
            .collect();
        meter.feed(&samples);
        meter.take().unwrap()
    }

    #[test]
    fn strip_reads_the_noise_and_ignores_in_channel_signal() {
        let (quiet_strip, quiet_power) = measure(0.1, 0.0);
        assert!((quiet_strip / 0.01 - 1.0).abs() < 0.15, "{quiet_strip}");
        assert!((quiet_power / 0.01 - 1.0).abs() < 0.05, "{quiet_power}");
        let (loaded_strip, loaded_power) = measure(0.1, 1.0);
        assert!(loaded_power > 50.0 * quiet_power);
        assert!(
            (loaded_strip / quiet_strip - 1.0).abs() < 0.15,
            "{loaded_strip}"
        );
        let (raised_strip, _) = measure(0.2, 0.0);
        assert!((raised_strip / quiet_strip / 4.0 - 1.0).abs() < 0.15);
    }
}
