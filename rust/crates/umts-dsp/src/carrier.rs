// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::{Complex32, Complex64};
use umts_types::time::{SampleInstant, SampleRate};

use crate::DspError;

const PHASE_MODULUS: f64 = 18_446_744_073_709_551_616.0;
const REANCHOR_SAMPLES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CarrierOffsetHz(f64);

impl CarrierOffsetHz {
    pub fn new(hz: f64) -> Result<Self, DspError> {
        if !hz.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        Ok(Self(hz))
    }

    pub fn hz(self) -> f64 {
        self.0
    }
}

fn phase_word(radians: f64) -> Result<u64, DspError> {
    if !radians.is_finite() {
        return Err(DspError::NonFiniteEvidence);
    }
    Ok(
        ((radians.rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU * PHASE_MODULUS).round()
            as u128) as u64,
    )
}

fn rotation(phase: u64) -> Complex64 {
    Complex64::from_polar(1.0, phase as f64 / PHASE_MODULUS * std::f64::consts::TAU)
}

fn step(rate: SampleRate, offset: CarrierOffsetHz) -> Result<u64, DspError> {
    let turns = offset.hz() / rate.hz();
    if !turns.is_finite() || turns.abs() >= 0.5 {
        return Err(DspError::InvalidFrequency);
    }
    let magnitude = (turns.abs() * PHASE_MODULUS).round() as u64;
    Ok(if turns >= 0.0 {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}

pub struct CarrierCorrector {
    next: SampleInstant,
    rate: SampleRate,
    offset: CarrierOffsetHz,
    phase: u64,
    increment: u64,
    oscillator: Complex64,
    advance: Complex64,
    until_anchor: usize,
}

impl CarrierCorrector {
    pub fn new(
        rate: SampleRate,
        offset: CarrierOffsetHz,
        correction_phase_radians: f64,
        origin: SampleInstant,
    ) -> Result<Self, DspError> {
        let phase = phase_word(correction_phase_radians)?;
        let increment = step(rate, offset)?;
        Ok(Self {
            next: origin,
            rate,
            offset,
            phase,
            increment,
            oscillator: rotation(phase),
            advance: rotation(increment),
            until_anchor: REANCHOR_SAMPLES,
        })
    }

    pub fn next_input(&self) -> SampleInstant {
        self.next
    }

    pub fn offset(&self) -> CarrierOffsetHz {
        self.offset
    }

    pub fn set_offset(&mut self, offset: CarrierOffsetHz) -> Result<(), DspError> {
        let increment = step(self.rate, offset)?;
        self.offset = offset;
        self.increment = increment;
        self.advance = rotation(increment);
        Ok(())
    }

    pub fn reset(
        &mut self,
        origin: SampleInstant,
        correction_phase_radians: f64,
    ) -> Result<(), DspError> {
        let phase = phase_word(correction_phase_radians)?;
        self.next = origin;
        self.phase = phase;
        self.oscillator = rotation(phase);
        self.until_anchor = REANCHOR_SAMPLES;
        Ok(())
    }

    pub fn process(
        &mut self,
        start: SampleInstant,
        samples: &mut [Complex32],
    ) -> Result<(), DspError> {
        if start != self.next {
            return Err(DspError::DiscontinuousTime);
        }
        let end = start
            .index
            .checked_add(samples.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        let maximum_power = f64::from(f32::MAX).powi(2);
        for value in samples.iter() {
            if !value.re.is_finite() || !value.im.is_finite() {
                return Err(DspError::NonFiniteEvidence);
            }
            // Rotation must remain representable as Complex32 at every phase.
            if f64::from(value.re).powi(2) + f64::from(value.im).powi(2) > maximum_power {
                return Err(DspError::InvalidAmplitude);
            }
        }
        for value in samples {
            let corrected = self.rotate(Complex64::new(f64::from(value.re), f64::from(value.im)));
            *value = Complex32::new(corrected.re as f32, corrected.im as f32);
        }
        self.next.index = end;
        Ok(())
    }
    pub fn process64(
        &mut self,
        start: SampleInstant,
        samples: &mut [Complex64],
    ) -> Result<(), DspError> {
        if start != self.next {
            return Err(DspError::DiscontinuousTime);
        }
        let end = start
            .index
            .checked_add(samples.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        if samples
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(DspError::NonFiniteEvidence);
        }
        if samples
            .iter()
            .any(|value| value.re.abs() + value.im.abs() > f64::MAX / 2.0)
        {
            return Err(DspError::InvalidAmplitude);
        }
        for value in samples {
            *value = self.rotate(*value);
        }
        self.next.index = end;
        Ok(())
    }
    fn rotate(&mut self, value: Complex64) -> Complex64 {
        let corrected = value * self.oscillator;
        self.phase = self.phase.wrapping_add(self.increment);
        self.until_anchor -= 1;
        if self.until_anchor == 0 {
            self.oscillator = rotation(self.phase);
            self.until_anchor = REANCHOR_SAMPLES;
        } else {
            self.oscillator *= self.advance;
        }
        corrected
    }
}

#[cfg(test)]
mod tests;
