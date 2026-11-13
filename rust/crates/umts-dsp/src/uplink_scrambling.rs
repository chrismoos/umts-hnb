// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::DspError;
use num_complex::Complex32;

const REGISTER_BITS: u32 = 25;
const CODE_BITS: u32 = 24;
const PERIOD: u32 = (1 << REGISTER_BITS) - 1;
const SECOND_OFFSET: u32 = 16_777_232;
const X_TAPS: u32 = (1 << 3) | 1;
const Y_TAPS: u32 = (1 << 3) | (1 << 2) | (1 << 1) | 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UplinkScramblingCode(u32);

impl UplinkScramblingCode {
    pub fn new(number: u32) -> Result<Self, DspError> {
        if number >= 1 << CODE_BITS {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(number))
    }
    pub fn number(self) -> u32 {
        self.0
    }
}

fn step(state: u32, taps: u32) -> u32 {
    (state >> 1) | (((state & taps).count_ones() & 1) << (REGISTER_BITS - 1))
}

fn transform(columns: &[u32; REGISTER_BITS as usize], mut value: u32) -> u32 {
    let mut output = 0;
    while value != 0 {
        let index = value.trailing_zeros();
        output ^= columns[index as usize];
        value &= value - 1;
    }
    output
}

fn jump(mut state: u32, taps: u32, mut distance: u32) -> u32 {
    let mut columns = std::array::from_fn(|index| step(1 << index, taps));
    while distance != 0 {
        if distance & 1 != 0 {
            state = transform(&columns, state);
        }
        distance >>= 1;
        if distance != 0 {
            columns = std::array::from_fn(|index| transform(&columns, columns[index]));
        }
    }
    state
}

fn sign(x: u32, y: u32) -> f32 {
    if (x ^ y) & 1 == 0 { 1.0 } else { -1.0 }
}

#[derive(Clone)]
pub struct UplinkLongSequence {
    x: u32,
    y: u32,
    x_second: u32,
    y_second: u32,
    position: u32,
    paired_sign: f32,
}

impl UplinkLongSequence {
    // TS 25.213 4.3.2.2. Components are +/-1, so complex chip power is two.
    pub fn new(code: UplinkScramblingCode, offset: u32) -> Result<Self, DspError> {
        if offset >= PERIOD {
            return Err(DspError::InvalidLength);
        }
        let initial_x = code.0 | (1 << CODE_BITS);
        let paired_offset = (offset - offset % 2 + SECOND_OFFSET) % PERIOD;
        let mut x_second = jump(initial_x, X_TAPS, paired_offset);
        let mut y_second = jump(PERIOD, Y_TAPS, paired_offset);
        let paired_sign = sign(x_second, y_second);
        if !offset.is_multiple_of(2) {
            x_second = step(x_second, X_TAPS);
            y_second = step(y_second, Y_TAPS);
        }
        Ok(Self {
            x: jump(initial_x, X_TAPS, offset),
            y: jump(PERIOD, Y_TAPS, offset),
            x_second,
            y_second,
            position: offset,
            paired_sign,
        })
    }

    pub fn write(&mut self, output: &mut [Complex32]) {
        for chip in output {
            let even = self.position.is_multiple_of(2);
            if even {
                self.paired_sign = sign(self.x_second, self.y_second);
            }
            let real = sign(self.x, self.y);
            *chip = Complex32::new(
                real,
                real * self.paired_sign * if even { 1.0 } else { -1.0 },
            );
            self.x = step(self.x, X_TAPS);
            self.y = step(self.y, Y_TAPS);
            self.x_second = step(self.x_second, X_TAPS);
            self.y_second = step(self.y_second, Y_TAPS);
            self.position += 1;
            if self.position == PERIOD {
                self.position = 0;
            }
        }
    }
}

pub(crate) fn preamble_real(code: UplinkScramblingCode, output: &mut [f32]) {
    let mut x = code.0 | (1 << CODE_BITS);
    let mut y = PERIOD;
    for value in output {
        *value = sign(x, y);
        x = step(x, X_TAPS);
        y = step(y, Y_TAPS);
    }
}

#[cfg(test)]
mod tests;
