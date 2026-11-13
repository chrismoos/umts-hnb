// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::CHIPS_PER_FRAME;

use crate::DspError;

const REGISTER_BITS: u32 = 18;
const PERIOD: u32 = (1 << REGISTER_BITS) - 1;
const QUADRATURE_OFFSET: usize = 131_072;
const X_TAPS: u32 = (1 << 7) | 1;
const Y_TAPS: u32 = (1 << 10) | (1 << 7) | (1 << 5) | 1;
const PRIMARY_COUNT: u16 = 512;
const CODES_PER_PRIMARY: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrimaryScramblingCode(u16);

impl PrimaryScramblingCode {
    pub fn new(index: u16) -> Result<Self, DspError> {
        if index >= PRIMARY_COUNT {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(index))
    }

    pub fn index(self) -> u16 {
        self.0
    }

    pub fn group(self) -> u8 {
        (self.0 / 8) as u8
    }

    pub fn sequence_number(self) -> u32 {
        u32::from(self.0) * CODES_PER_PRIMARY
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownlinkScramblingCode {
    primary: PrimaryScramblingCode,
    secondary: Option<u8>,
}

impl DownlinkScramblingCode {
    pub fn new(primary: PrimaryScramblingCode, secondary: Option<u8>) -> Result<Self, DspError> {
        if secondary.is_some_and(|index| !(1..CODES_PER_PRIMARY as u8).contains(&index)) {
            return Err(DspError::InvalidCode);
        }
        Ok(Self { primary, secondary })
    }

    pub fn sequence_number(self) -> u32 {
        self.primary.sequence_number() + u32::from(self.secondary.unwrap_or(0))
    }
}

impl From<PrimaryScramblingCode> for DownlinkScramblingCode {
    fn from(primary: PrimaryScramblingCode) -> Self {
        Self {
            primary,
            secondary: None,
        }
    }
}

fn advance(state: &mut u32, taps: u32) -> f32 {
    let value = if *state & 1 == 0 { 1.0 } else { -1.0 };
    let feedback = (*state & taps).count_ones() & 1;
    *state = (*state >> 1) | (feedback << (REGISTER_BITS - 1));
    value
}

// TS 25.213 5.2.2. Components are +/-1, not unit-magnitude complex samples.
pub fn downlink_sequence(number: u32, output: &mut [Complex32]) -> Result<(), DspError> {
    if number >= PERIOD {
        return Err(DspError::InvalidCode);
    }
    if output.len() != CHIPS_PER_FRAME as usize {
        return Err(DspError::InvalidLength);
    }
    let mut x = 1;
    let mut y = PERIOD;
    for _ in 0..number {
        advance(&mut x, X_TAPS);
    }
    let mut x_quadrature = x;
    let mut y_quadrature = y;
    for _ in 0..QUADRATURE_OFFSET {
        advance(&mut x_quadrature, X_TAPS);
        advance(&mut y_quadrature, Y_TAPS);
    }
    for chip in output {
        *chip = Complex32::new(
            advance(&mut x, X_TAPS) * advance(&mut y, Y_TAPS),
            advance(&mut x_quadrature, X_TAPS) * advance(&mut y_quadrature, Y_TAPS),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_generator_matches_independent_full_period_recurrence() {
        let mut x = vec![0_u8; PERIOD as usize];
        let mut y = vec![1_u8; PERIOD as usize];
        x[0] = 1;
        for i in 0..PERIOD as usize - REGISTER_BITS as usize {
            x[i + 18] = x[i + 7] ^ x[i];
            y[i + 18] = y[i + 10] ^ y[i + 7] ^ y[i + 5] ^ y[i];
        }
        let mut output = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        for number in [0, 16, 8191, 16384, PERIOD - 1] {
            downlink_sequence(number, &mut output).unwrap();
            let sign = |index: usize| {
                if x[(index + number as usize) % x.len()] ^ y[index % y.len()] == 0 {
                    1.0
                } else {
                    -1.0
                }
            };
            for (index, &actual) in output.iter().enumerate() {
                assert_eq!(
                    actual,
                    Complex32::new(sign(index), sign(index + QUADRATURE_OFFSET))
                );
            }
        }
    }

    #[test]
    fn primary_index_is_not_the_gold_sequence_number() {
        let primary = PrimaryScramblingCode::new(511).unwrap();
        assert_eq!(primary.index(), 511);
        assert_eq!(primary.sequence_number(), 8176);
        assert_eq!(primary.group(), 63);
        assert!(PrimaryScramblingCode::new(512).is_err());
        assert_eq!(
            downlink_sequence(PERIOD, &mut []),
            Err(DspError::InvalidCode)
        );
        assert_eq!(downlink_sequence(0, &mut []), Err(DspError::InvalidLength));
    }

    #[test]
    fn associated_secondary_codes_use_the_primary_set_without_aliasing() {
        let primary = PrimaryScramblingCode::new(511).unwrap();
        assert_eq!(
            DownlinkScramblingCode::from(primary).sequence_number(),
            8176
        );
        for secondary in 1..=15 {
            assert_eq!(
                DownlinkScramblingCode::new(primary, Some(secondary))
                    .unwrap()
                    .sequence_number(),
                8176 + u32::from(secondary)
            );
        }
        assert!(DownlinkScramblingCode::new(primary, Some(0)).is_err());
        assert!(DownlinkScramblingCode::new(primary, Some(16)).is_err());
    }
}
