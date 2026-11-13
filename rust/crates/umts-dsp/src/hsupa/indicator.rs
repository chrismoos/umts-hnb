// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant};

use crate::{
    DspError,
    channel_codes::OvsfCode,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
};

mod tables;

const SPREADING: usize = 128;
const SIGNATURE_BITS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature(u8);

impl Signature {
    pub fn new(index: u8) -> Result<Self, DspError> {
        if usize::from(index) >= tables::SIGNS.len() {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(index))
    }

    // Slot is relative to the indicator channel frame, not P-CCPCH.
    pub fn slot(self, slot: u8, indicator: Indicator) -> Result<[i8; SIGNATURE_BITS], DspError> {
        if slot >= 15 {
            return Err(DspError::InvalidLength);
        }
        let row = tables::HOPPING[usize::from(self.0)][usize::from(slot % 3)];
        Ok(tables::SIGNS[row].map(|sign| sign * indicator as i8))
    }
}

// TS 25.212 Tables 17/18 use these signs for relative grants and ACK/NACK.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i8)]
pub enum Indicator {
    Negative = -1,
    Silent = 0,
    Positive = 1,
}

pub struct IndicatorSpreader {
    weights: Vec<Complex32>,
}

impl IndicatorSpreader {
    pub fn new(primary: PrimaryScramblingCode, channel_code: u16) -> Result<Self, DspError> {
        let mut code = [0.0; SPREADING];
        OvsfCode::new(SPREADING as u16, channel_code)?.write(&mut code)?;
        let mut weights = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(primary.sequence_number(), &mut weights)?;
        for (index, weight) in weights.iter_mut().enumerate() {
            *weight *= 0.5 * code[index % SPREADING];
        }
        Ok(Self { weights })
    }

    pub fn add_slot(
        &self,
        start: ChipInstant,
        signature: Signature,
        indicator_slot: u8,
        indicator: Indicator,
        amplitude: f32,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if output.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        if !start.index.is_multiple_of(SPREADING as u64)
            || start.index.checked_add(CHIPS_PER_SLOT).is_none()
        {
            return Err(DspError::DiscontinuousTime);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let signs = signature.slot(indicator_slot, indicator)?;
        let offset = (start.index % CHIPS_PER_FRAME) as usize;
        for (symbol, chips) in output.chunks_exact_mut(SPREADING).enumerate() {
            let value = Complex32::new(
                f32::from(signs[2 * symbol]),
                f32::from(signs[2 * symbol + 1]),
            ) * amplitude;
            for (chip, output) in chips.iter_mut().enumerate() {
                *output +=
                    value * self.weights[(offset + symbol * SPREADING + chip) % self.weights.len()];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_signature_rows_are_orthogonal_and_hopping_preserves_assignments() {
        for (i, a) in tables::SIGNS.iter().enumerate() {
            for (j, b) in tables::SIGNS.iter().enumerate() {
                let inner: i32 = a
                    .iter()
                    .zip(b)
                    .map(|(a, b)| i32::from(*a) * i32::from(*b))
                    .sum();
                assert_eq!(inner, if i == j { SIGNATURE_BITS as i32 } else { 0 });
            }
        }
        for phase in 0..3 {
            let mut seen = [false; 40];
            for row in tables::HOPPING {
                assert!(!seen[row[phase]]);
                seen[row[phase]] = true;
            }
        }
        let signature = Signature::new(0).unwrap();
        assert_eq!(
            signature.slot(1, Indicator::Positive).unwrap(),
            tables::SIGNS[2]
        );
        assert_eq!(
            signature.slot(4, Indicator::Negative).unwrap(),
            tables::SIGNS[2].map(|x| -x)
        );
        assert_eq!(
            signature.slot(0, Indicator::Silent).unwrap(),
            [0; SIGNATURE_BITS]
        );
        assert!(Signature::new(40).is_err());
    }
}
