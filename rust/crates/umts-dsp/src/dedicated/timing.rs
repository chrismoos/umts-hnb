// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{UplinkControlFormat, UplinkDpchCode};
use crate::uplink_scrambling::UplinkScramblingCode;
use crate::{
    DspError,
    carrier::{CarrierCorrector, CarrierOffsetHz},
    fft_geometry::{CompensatedSum, smooth_fft_length},
    fractional_sampling::SamplePosition,
};
use num_complex::{Complex32, Complex64};
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;
use umts_types::time::{
    CHIPS_PER_SECOND, CHIPS_PER_SLOT, SLOTS_PER_FRAME, SampleInstant, SampleRate,
};

const CONTROL_SF: usize = 256;
const MAX_DELAY_CHIPS: usize = 8192;

fn pilot_reference(
    code: &UplinkDpchCode,
    format: UplinkControlFormat,
    slot: u8,
) -> Result<Vec<Complex64>, DspError> {
    let pilots = format.pilots();
    let word = pilots.word(slot).ok_or(DspError::InvalidCode)?;
    let offset = usize::from(slot) * CHIPS_PER_SLOT as usize;
    Ok(
        code.scrambling()[offset..offset + pilots.bits() * CONTROL_SF]
            .iter()
            .enumerate()
            .map(|(chip, value)| {
                let bit = chip / CONTROL_SF;
                let sign = if word & (1 << (pilots.bits() - 1 - bit)) == 0 {
                    1.0
                } else {
                    -1.0
                };
                (Complex64::new(-f64::from(value.im), f64::from(value.re)) * sign).conj()
            })
            .collect(),
    )
}

fn uses_direct_correlation(rate: usize, delays: usize) -> bool {
    delays <= if rate == 1 { 3 } else { 17 }
}

struct FftTimingPlan {
    forward: Arc<dyn Fft<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    reference: Vec<Complex64>,
}

impl FftTimingPlan {
    fn new(pilots: &[Complex64], rate: usize, input_length: usize) -> Self {
        let length = smooth_fft_length(input_length);
        let mut planner = FftPlanner::new();
        let forward = planner.plan_fft_forward(length);
        let inverse = planner.plan_fft_inverse(length);
        let mut reference = vec![Complex64::default(); length];
        for (chip, value) in pilots.iter().enumerate() {
            reference[chip * rate] = value.conj();
        }
        let mut scratch = vec![Complex64::default(); forward.get_inplace_scratch_len()];
        forward.process_with_scratch(&mut reference, &mut scratch);
        for value in &mut reference {
            *value = value.conj() / length as f64;
        }
        Self {
            forward,
            inverse,
            reference,
        }
    }
}

struct TimingIdentity {
    code: UplinkScramblingCode,
    format: UplinkControlFormat,
    slot: u8,
}

pub struct DedicatedTimingPlan {
    identity: TimingIdentity,
    fft: Option<FftTimingPlan>,
    pilot_reference: Vec<Complex64>,
    following_pilot_reference: Vec<Complex64>,
    third_pilot_reference: Vec<Complex64>,
    energy: f64,
    pilot_chips: usize,
    rate: usize,
    delays: usize,
    input_length: usize,
}
impl DedicatedTimingPlan {
    // TS 25.211 Tables 2/3 and TS 25.213 Figure 1A. Search only the configured slot's known pilot prefix.
    pub fn new(
        code: &UplinkDpchCode,
        format: UplinkControlFormat,
        slot: u8,
        samples_per_chip: usize,
        delays: usize,
    ) -> Result<Self, DspError> {
        Self::new_inner(
            code,
            format,
            slot,
            samples_per_chip,
            delays,
            !uses_direct_correlation(samples_per_chip, delays),
        )
    }

    fn new_inner(
        code: &UplinkDpchCode,
        format: UplinkControlFormat,
        slot: u8,
        samples_per_chip: usize,
        delays: usize,
        build_fft: bool,
    ) -> Result<Self, DspError> {
        if u64::from(slot) >= SLOTS_PER_FRAME || !matches!(samples_per_chip, 1 | 2 | 4 | 8) {
            return Err(DspError::InvalidCode);
        }
        if delays == 0 || delays > MAX_DELAY_CHIPS * samples_per_chip {
            return Err(DspError::InvalidLength);
        }
        let pilots = format.pilots();
        let pilot_chips = pilots.bits() * CONTROL_SF;
        let input_length = (pilot_chips - 1) * samples_per_chip + delays;
        let pilot_reference = pilot_reference(code, format, slot)?;
        let following_pilot_reference =
            self::pilot_reference(code, format, (slot + 1) % SLOTS_PER_FRAME as u8)?;
        let third_pilot_reference =
            self::pilot_reference(code, format, (slot + 2) % SLOTS_PER_FRAME as u8)?;
        let mut energy = 0.0;
        for value in &pilot_reference {
            let value = value.conj();
            energy += value.norm_sqr();
        }
        let fft =
            build_fft.then(|| FftTimingPlan::new(&pilot_reference, samples_per_chip, input_length));
        Ok(Self {
            identity: TimingIdentity {
                code: code.code(),
                format,
                slot,
            },
            fft,
            pilot_reference,
            following_pilot_reference,
            third_pilot_reference,
            energy,
            pilot_chips,
            rate: samples_per_chip,
            delays,
            input_length,
        })
    }
    pub fn input_length(&self) -> usize {
        self.input_length
    }
    pub fn slot(&self) -> u8 {
        self.identity.slot
    }
    pub fn delay_count(&self) -> usize {
        self.delays
    }
    pub fn samples_per_chip(&self) -> usize {
        self.rate
    }
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<TimingIdentity>()
            + (self.fft.as_ref().map_or(0, |fft| fft.reference.len())
                + self.pilot_reference.capacity()
                + self.following_pilot_reference.capacity()
                + self.third_pilot_reference.capacity())
                * std::mem::size_of::<Complex64>()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PilotTiming {
    pub position: SamplePosition,
    pub amplitude: Complex64,
    pub coherent_fraction: f64,
    pub despread_coherent_fraction: f64,
    pub fractional_offset_samples: f64,
}

pub struct DedicatedTimingSearch {
    plan: Arc<DedicatedTimingPlan>,
    spectrum: Vec<Complex64>,
    product: Vec<Complex64>,
    scratch: Vec<Complex64>,
    energies: Vec<f64>,
    corrected: Vec<Complex64>,
}
impl DedicatedTimingSearch {
    pub fn new(plan: Arc<DedicatedTimingPlan>) -> Self {
        let (length, products, scratch) = plan.fft.as_ref().map_or((0, plan.delays, 0), |fft| {
            (
                fft.reference.len(),
                fft.reference.len(),
                fft.forward
                    .get_inplace_scratch_len()
                    .max(fft.inverse.get_inplace_scratch_len()),
            )
        });
        Self {
            spectrum: vec![Complex64::default(); length],
            product: vec![Complex64::default(); products],
            scratch: vec![Complex64::default(); scratch],
            energies: vec![0.0; plan.delays],
            corrected: vec![Complex64::default(); plan.input_length],
            plan,
        }
    }
    pub fn storage_bytes(&self) -> usize {
        self.plan.storage_bytes()
            + (self.spectrum.len() + self.product.len() + self.scratch.len() + self.corrected.len())
                * std::mem::size_of::<Complex64>()
            + self.energies.len() * std::mem::size_of::<f64>()
    }

    pub fn slot(&self) -> u8 {
        self.plan.slot()
    }

    /// Exchange a pre-admitted slot template, returning the previous plan to
    /// its owner without allocating, dropping plans or clearing work buffers.
    /// Code, control format and search geometry must match. Callers must invalidate cached
    /// observations.
    pub fn exchange_plan(
        &mut self,
        prepared: &mut Arc<DedicatedTimingPlan>,
    ) -> Result<(), DspError> {
        let current = &self.plan;
        if current.identity.code != prepared.identity.code
            || current.identity.format != prepared.identity.format
        {
            return Err(DspError::InvalidCode);
        }
        if current.rate != prepared.rate
            || current.delays != prepared.delays
            || current.input_length != prepared.input_length
            || current.pilot_chips != prepared.pilot_chips
            || current.fft.is_some() != prepared.fft.is_some()
            || prepared.input_length != self.corrected.len()
            || prepared.delays > self.energies.len()
            || prepared.delays > self.product.len()
        {
            return Err(DspError::InvalidLength);
        }
        if let Some(fft) = &prepared.fft
            && (fft.reference.len() != self.spectrum.len()
                || fft.reference.len() != self.product.len()
                || fft
                    .forward
                    .get_inplace_scratch_len()
                    .max(fft.inverse.get_inplace_scratch_len())
                    > self.scratch.len())
        {
            return Err(DspError::InvalidLength);
        }
        std::mem::swap(&mut self.plan, prepared);
        Ok(())
    }
    // Input is already interpolated to one sample per chip at the selected timing peak.
    pub fn interpolated_pilot_coherence(
        &mut self,
        start: SampleInstant,
        chips: &[Complex32],
        offset: CarrierOffsetHz,
    ) -> Result<f64, DspError> {
        self.pilot_coherence(start, chips, offset, 0)
    }

    pub fn following_pilot_coherence(
        &mut self,
        start: SampleInstant,
        chips: &[Complex32],
        offset: CarrierOffsetHz,
    ) -> Result<f64, DspError> {
        self.pilot_coherence(start, chips, offset, 1)
    }

    pub fn third_pilot_coherence(
        &mut self,
        start: SampleInstant,
        chips: &[Complex32],
        offset: CarrierOffsetHz,
    ) -> Result<f64, DspError> {
        self.pilot_coherence(start, chips, offset, 2)
    }

    fn pilot_coherence(
        &mut self,
        start: SampleInstant,
        chips: &[Complex32],
        offset: CarrierOffsetHz,
        distance: usize,
    ) -> Result<f64, DspError> {
        let width = self.plan.pilot_chips;
        if chips.len() < width {
            return Err(DspError::InvalidLength);
        }
        let corrected = &mut self.corrected[..width];
        for (output, input) in corrected.iter_mut().zip(&chips[..width]) {
            if !input.re.is_finite() || !input.im.is_finite() {
                return Err(DspError::NonFiniteEvidence);
            }
            *output = Complex64::new(f64::from(input.re), f64::from(input.im));
        }
        let rate = SampleRate::new(CHIPS_PER_SECOND, 1).map_err(|_| DspError::InvalidFrequency)?;
        CarrierCorrector::new(rate, offset, 0.0, start)?.process64(start, corrected)?;
        let mut sum = Complex64::default();
        let mut energy = 0.0;
        let reference = match distance {
            0 => &self.plan.pilot_reference,
            1 => &self.plan.following_pilot_reference,
            2 => &self.plan.third_pilot_reference,
            _ => return Err(DspError::InvalidCode),
        };
        for (samples, reference) in corrected
            .chunks_exact(CONTROL_SF)
            .zip(reference.chunks_exact(CONTROL_SF))
        {
            let symbol = samples
                .iter()
                .zip(reference)
                .map(|(a, b)| a * b)
                .sum::<Complex64>();
            sum += symbol;
            energy += symbol.norm_sqr();
        }
        Ok(if energy > 0.0 {
            (sum.norm_sqr() / ((width / CONTROL_SF) as f64 * energy)).clamp(0.0, 1.0)
        } else {
            0.0
        })
    }
    // Input retains the configured sampling rate after matched filtering. One CFO seed, no blind grid or CRC-selected hypotheses.
    pub fn search(
        &mut self,
        start: SampleInstant,
        input: &[Complex32],
        offset: CarrierOffsetHz,
    ) -> Result<PilotTimingWindow<'_>, DspError> {
        if input.len() != self.plan.input_length {
            return Err(DspError::InvalidLength);
        }
        start
            .index
            .checked_add(input.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        if input.iter().any(|v| !v.re.is_finite() || !v.im.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        let rate = SampleRate::new(CHIPS_PER_SECOND * self.plan.rate as u64, 1)
            .map_err(|_| DspError::InvalidFrequency)?;
        let mut carrier = CarrierCorrector::new(rate, offset, 0.0, start)?;
        for (dest, source) in self.corrected.iter_mut().zip(input) {
            *dest = Complex64::new(f64::from(source.re), f64::from(source.im));
        }
        carrier.process64(start, &mut self.corrected)?;
        let step = self.plan.rate;
        let width = self.plan.pilot_chips;
        for phase in 0..step.min(self.plan.delays) {
            let mut energy = CompensatedSum::default();
            for chip in 0..width {
                energy.add(self.corrected[phase + chip * step].norm_sqr());
            }
            self.energies[phase] = energy.value();
            for delay in (phase + step..self.plan.delays).step_by(step) {
                energy.add(-self.corrected[delay - step].norm_sqr());
                energy.add(self.corrected[delay + (width - 1) * step].norm_sqr());
                self.energies[delay] = energy.value();
            }
        }
        // Narrow oversampled tracking windows cost less as direct correlations.
        // Keep the same CFO-corrected samples, pilot reference and delay set.
        if uses_direct_correlation(self.plan.rate, self.plan.delays) {
            for (delay, output) in self.product[..self.plan.delays].iter_mut().enumerate() {
                *output = self.corrected[delay..]
                    .iter()
                    .step_by(self.plan.rate)
                    .zip(&self.plan.pilot_reference)
                    .fold(Complex64::default(), |sum, (sample, reference)| {
                        sum + sample * reference
                    });
            }
        } else {
            self.spectrum.fill(Complex64::default());
            self.spectrum[..input.len()].copy_from_slice(&self.corrected);
            self.correlate_fft();
        }
        Ok(PilotTimingWindow {
            start,
            search: self,
        })
    }

    fn correlate_fft(&mut self) {
        let fft = self
            .plan
            .fft
            .as_ref()
            .expect("wide timing search has admitted FFT resources");
        fft.forward
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for ((dest, value), reference) in self
            .product
            .iter_mut()
            .zip(&self.spectrum)
            .zip(&fft.reference)
        {
            *dest = value * reference;
        }
        fft.inverse
            .process_with_scratch(&mut self.product, &mut self.scratch);
    }
}

pub struct PilotTimingWindow<'a> {
    start: SampleInstant,
    search: &'a DedicatedTimingSearch,
}
impl PilotTimingWindow<'_> {
    pub fn at(&self, delay: usize) -> Option<PilotTiming> {
        if delay >= self.search.plan.delays {
            return None;
        }
        let dot = self.search.product[delay];
        let energy = self.search.energies[delay] * self.search.plan.energy;
        let coherent_fraction = if energy > 0.0 {
            (dot.norm_sqr() / energy).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let mut pilot_sum = Complex64::default();
        let mut pilot_energy = 0.0;
        for (symbol, reference) in self
            .search
            .plan
            .pilot_reference
            .chunks_exact(CONTROL_SF)
            .enumerate()
        {
            let first = delay + symbol * CONTROL_SF * self.search.plan.rate;
            let value = reference
                .iter()
                .enumerate()
                .map(|(chip, reference)| {
                    self.search.corrected[first + chip * self.search.plan.rate] * reference
                })
                .sum::<Complex64>();
            pilot_sum += value;
            pilot_energy += value.norm_sqr();
        }
        let pilots = self.search.plan.pilot_chips / CONTROL_SF;
        let despread_coherent_fraction = if pilot_energy > 0.0 {
            (pilot_sum.norm_sqr() / (pilots as f64 * pilot_energy)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Some(PilotTiming {
            position: SamplePosition::new(
                SampleInstant {
                    index: self.start.index + delay as u64,
                    ..self.start
                },
                0.0,
            )
            .expect("validated sample position"),
            amplitude: dot / self.search.plan.energy,
            coherent_fraction,
            despread_coherent_fraction,
            fractional_offset_samples: 0.0,
        })
    }
    // Largest correlation is a measurement, not an acquisition decision or proof of decoding.
    pub fn strongest(&self) -> PilotTiming {
        let delay = (0..self.search.plan.delays)
            .max_by(|&a, &b| {
                self.search.product[a]
                    .norm_sqr()
                    .total_cmp(&self.search.product[b].norm_sqr())
            })
            .expect("nonempty delays");
        let mut result = self.at(delay).expect("validated delay");
        if delay > 0 && delay + 1 < self.search.plan.delays {
            let left = self.search.product[delay - 1].norm_sqr();
            let center = self.search.product[delay].norm_sqr();
            let right = self.search.product[delay + 1].norm_sqr();
            let curvature = left - 2.0 * center + right;
            if curvature < 0.0 {
                result.fractional_offset_samples =
                    (0.5 * (left - right) / curvature).clamp(-0.5, 0.5);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests;
