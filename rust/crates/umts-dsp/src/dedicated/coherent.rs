// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    carrier::{CarrierCorrector, CarrierOffsetHz},
    pilots::PilotEstimate,
};
use num_complex::Complex64;
use umts_types::time::{CHIPS_PER_SECOND, ChipInstant, SampleInstant, SampleRate};

pub(super) const CONTROL_SPREADING: usize = 256;
const MAX_GAIN: u8 = 15;

#[derive(Clone, Copy, Debug)]
pub struct UplinkGains {
    control: u8,
    data: u8,
}
impl UplinkGains {
    pub fn new(control: u8, data: u8) -> Result<Self, DspError> {
        // TS 25.213 4.2.1.1 Table 1: beta = signaled/15 and at least one amplitude is unity.
        if control == 0 || control > MAX_GAIN || data > MAX_GAIN || control.max(data) != MAX_GAIN {
            return Err(DspError::InvalidAmplitude);
        }
        Ok(Self { control, data })
    }
    pub fn control(self) -> f64 {
        f64::from(self.control) / f64::from(MAX_GAIN)
    }
    pub fn data(self) -> f64 {
        f64::from(self.data) / f64::from(MAX_GAIN)
    }
    pub fn ratio(self) -> f64 {
        f64::from(self.data) / f64::from(self.control)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct LikelihoodSettings {
    maximum: f64,
}
impl LikelihoodSettings {
    pub fn new(maximum: f64) -> Result<Self, DspError> {
        if !maximum.is_finite() || maximum <= 0.0 || maximum > f64::from(f32::MAX) {
            return Err(DspError::InvalidAmplitude);
        }
        Ok(Self { maximum })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DpcchStatistics {
    pub signal_code_power: f64,
    pub disturbance_symbol_power: f64,
    pub disturbance_chip_power: f64,
    pub channel_error_power: f64,
}
impl DpcchStatistics {
    /// The unbiased corrections assume independent equal-variance pilot errors and a constant corrected channel.
    pub fn new(pilot: PilotEstimate, count: usize) -> Result<Self, DspError> {
        if !(2..=8).contains(&count)
            || !pilot.observed_power.is_finite()
            || !pilot.residual_power.is_finite()
            || pilot.observed_power < 0.0
            || pilot.residual_power < 0.0
        {
            return Err(DspError::NonFiniteEvidence);
        }
        let noise = pilot.residual_power * count as f64 / (count - 1) as f64;
        let chip_noise = CONTROL_SPREADING as f64 * noise;
        if !noise.is_finite() || !chip_noise.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        Ok(Self {
            signal_code_power: pilot.observed_power - noise,
            disturbance_symbol_power: noise,
            disturbance_chip_power: chip_noise,
            channel_error_power: noise / count as f64,
        })
    }
    /// TS 25.215 5.2.2: (RSCP/ISCP)*SF. Absolute antenna power calibration is not provided here.
    pub fn sir_linear(self) -> Option<f64> {
        if self.signal_code_power <= 0.0 || self.disturbance_symbol_power <= 0.0 {
            return None;
        }
        let ratio = self.signal_code_power / self.disturbance_symbol_power;
        ratio.is_finite().then_some(ratio)
    }
    pub(crate) fn llr(
        self,
        projection: f64,
        ratio: f64,
        sf: usize,
        settings: LikelihoodSettings,
    ) -> f64 {
        if ratio == 0.0 {
            return 0.0;
        }
        // Complex AWGN gives real projection variance N0/2. Include pilot-mean channel uncertainty.
        let variance =
            self.disturbance_chip_power / sf as f64 + ratio * ratio * self.channel_error_power;
        if variance == 0.0 {
            return if projection == 0.0 {
                0.0
            } else {
                projection.signum() * settings.maximum
            };
        }
        (4.0 * ratio * projection / variance).clamp(-settings.maximum, settings.maximum)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CarrierTrackingSettings {
    pub initial: CarrierOffsetHz,
    pub maximum_absolute_hz: f64,
    pub update_fraction: f64,
    pub minimum_coherence: f64,
}
impl CarrierTrackingSettings {
    fn validate(self) -> Result<(), DspError> {
        let nyquist = CHIPS_PER_SECOND as f64 / (2 * CONTROL_SPREADING) as f64;
        if !self.maximum_absolute_hz.is_finite()
            || self.maximum_absolute_hz <= 0.0
            || 2.0 * self.maximum_absolute_hz >= nyquist
            || self.initial.hz().abs() > self.maximum_absolute_hz
            || !self.update_fraction.is_finite()
            || !(0.0..=1.0).contains(&self.update_fraction)
            || self.update_fraction == 0.0
            || !self.minimum_coherence.is_finite()
            || !(0.0..=1.0).contains(&self.minimum_coherence)
            || self.minimum_coherence == 0.0
        {
            return Err(DspError::InvalidFrequency);
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct CarrierObservation {
    pub applied_hz: f64,
    pub next_hz: f64,
    pub residual_hz: Option<f64>,
    pub coherence: Option<f64>,
    pub updated: bool,
}
pub(super) struct PilotCarrier {
    settings: CarrierTrackingSettings,
    corrector: Option<CarrierCorrector>,
}
impl PilotCarrier {
    pub(super) fn new(settings: CarrierTrackingSettings) -> Result<Self, DspError> {
        settings.validate()?;
        Ok(Self {
            settings,
            corrector: None,
        })
    }
    pub(super) fn begin(&mut self, start: ChipInstant) -> Result<(), DspError> {
        let origin = SampleInstant {
            epoch: start.epoch,
            index: start.index,
        };
        if let Some(corrector) = &mut self.corrector {
            if corrector.next_input() != origin {
                let changed_epoch = corrector.next_input().epoch != origin.epoch;
                corrector.reset(origin, 0.0)?;
                if changed_epoch {
                    corrector.set_offset(self.settings.initial)?;
                }
            }
        } else {
            self.corrector = Some(CarrierCorrector::new(
                SampleRate::new(CHIPS_PER_SECOND, 1).map_err(|_| DspError::InvalidFrequency)?,
                self.settings.initial,
                0.0,
                origin,
            )?);
        }
        Ok(())
    }
    pub(super) fn correct(
        &mut self,
        start: ChipInstant,
        samples: &mut [Complex64],
    ) -> Result<(), DspError> {
        self.corrector
            .as_mut()
            .ok_or(DspError::DiscontinuousTime)?
            .process64(
                SampleInstant {
                    epoch: start.epoch,
                    index: start.index,
                },
                samples,
            )
    }
    pub(super) fn restore_confirmed_offset(
        &mut self,
        offset: CarrierOffsetHz,
    ) -> Result<(), DspError> {
        if offset.hz().abs() > self.settings.maximum_absolute_hz {
            return Err(DspError::InvalidFrequency);
        }
        self.corrector
            .as_mut()
            .ok_or(DspError::DiscontinuousTime)?
            .set_offset(offset)
    }
    pub(super) fn update(&mut self, pilots: &[Complex64]) -> Result<CarrierObservation, DspError> {
        let corrector = self.corrector.as_mut().ok_or(DspError::DiscontinuousTime)?;
        let mut result = CarrierObservation {
            applied_hz: corrector.offset().hz(),
            next_hz: corrector.offset().hz(),
            ..CarrierObservation::default()
        };
        let mut correlation = Complex64::default();
        let mut magnitude = 0.0;
        for pair in pilots.windows(2) {
            let value = pair[1] * pair[0].conj();
            correlation += value;
            magnitude += value.norm();
        }
        if magnitude == 0.0 {
            return Ok(result);
        }
        let coherence = (correlation.norm() / magnitude).clamp(0.0, 1.0);
        let residual = correlation.arg() * CHIPS_PER_SECOND as f64
            / (std::f64::consts::TAU * CONTROL_SPREADING as f64);
        result.coherence = Some(coherence);
        result.residual_hz = Some(residual);
        let next = result.applied_hz + self.settings.update_fraction * residual;
        if coherence >= self.settings.minimum_coherence
            && next.abs() <= self.settings.maximum_absolute_hz
        {
            corrector.set_offset(CarrierOffsetHz::new(next)?)?;
            result.next_hz = next;
            result.updated = true;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
