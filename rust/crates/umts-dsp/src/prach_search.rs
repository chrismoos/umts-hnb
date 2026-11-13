// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{ops::Range, sync::Arc};

use num_complex::{Complex32, Complex64};
use rustfft::{Fft, FftPlanner};
use umts_types::time::ChipInstant;

use crate::{
    DspError,
    fft_geometry::{CompensatedSum, smooth_fft_length},
    prach::{PREAMBLE_CHIPS, PrachCode, PreambleSignature},
};

pub const MAX_SEARCH_DELAYS: usize = 8192;
const SIGNATURE_COUNT: usize = 16;
const PROBE_BLOCK_CHIPS: usize = 256;

pub struct PreambleSearchPlan {
    code: PrachCode,
    span: Range<usize>,
    delays: usize,
    fft_length: usize,
    input_length: usize,
    forward: Arc<dyn Fft<f64>>,
    inverse: Arc<dyn Fft<f64>>,
    spectra: Vec<(PreambleSignature, Vec<Complex64>)>,
    rows: [Option<usize>; SIGNATURE_COUNT],
}

impl PreambleSearchPlan {
    pub fn new(
        code: PrachCode,
        available: u16,
        span: Range<usize>,
        delays: usize,
    ) -> Result<Self, DspError> {
        if available == 0 {
            return Err(DspError::InvalidCode);
        }
        if span.start >= span.end
            || span.end > PREAMBLE_CHIPS
            || !(span.end - span.start).is_multiple_of(PROBE_BLOCK_CHIPS)
            || delays == 0
            || delays > MAX_SEARCH_DELAYS
        {
            return Err(DspError::InvalidLength);
        }
        let input_length = span.len() + delays - 1;
        // Valid correlation lags never wrap, so padding to input length is sufficient.
        let fft_length = smooth_fft_length(input_length);
        let mut planner = FftPlanner::new();
        let forward = planner.plan_fft_forward(fft_length);
        let inverse = planner.plan_fft_inverse(fft_length);
        let mut scratch = vec![Complex64::default(); forward.get_inplace_scratch_len()];
        let mut spectra = Vec::with_capacity(available.count_ones() as usize);
        let mut rows = [None; SIGNATURE_COUNT];
        let mut preamble = vec![Complex32::default(); PREAMBLE_CHIPS];
        for (index, row) in rows.iter_mut().enumerate() {
            if available & (1 << index) == 0 {
                continue;
            }
            let signature = PreambleSignature::new(index as u8)?;
            code.write_preamble(signature, &mut preamble)?;
            let mut spectrum = vec![Complex64::default(); fft_length];
            for (output, input) in spectrum.iter_mut().zip(&preamble[span.clone()]) {
                *output = Complex64::new(f64::from(input.re), f64::from(input.im));
            }
            forward.process_with_scratch(&mut spectrum, &mut scratch);
            for value in &mut spectrum {
                *value = value.conj() / fft_length as f64;
            }
            *row = Some(spectra.len());
            spectra.push((signature, spectrum));
        }
        Ok(Self {
            code,
            span,
            delays,
            fft_length,
            input_length,
            forward,
            inverse,
            spectra,
            rows,
        })
    }

    pub fn code(&self) -> PrachCode {
        self.code
    }
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }
    pub fn delay_count(&self) -> usize {
        self.delays
    }
    pub fn input_length(&self) -> usize {
        self.input_length
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TimingCorrelation {
    pub code: PrachCode,
    pub signature: PreambleSignature,
    // The first observed reference chip, not an assumed complete-preamble origin.
    pub correlation_start: ChipInstant,
    pub preamble_offset: usize,
    pub amplitude: Complex64,
    pub coherent_fraction: f64,
    pub mean_input_power: f64,
}

pub struct PreambleTimingSearch {
    plan: Arc<PreambleSearchPlan>,
    spectrum: Vec<Complex64>,
    product: Vec<Complex64>,
    scratch: Vec<Complex64>,
    correlations: Vec<Complex64>,
    energies: Vec<f64>,
}

impl PreambleTimingSearch {
    pub fn new(plan: Arc<PreambleSearchPlan>) -> Self {
        let scratch_length = plan
            .forward
            .get_inplace_scratch_len()
            .max(plan.inverse.get_inplace_scratch_len());
        Self {
            spectrum: vec![Complex64::default(); plan.fft_length],
            product: vec![Complex64::default(); plan.fft_length],
            scratch: vec![Complex64::default(); scratch_length],
            correlations: vec![Complex64::default(); plan.spectra.len() * plan.delays],
            energies: vec![0.0; plan.delays],
            plan,
        }
    }

    // Chip-aligned, filtered input must already have sufficiently small residual CFO.
    pub fn search(
        &mut self,
        start: ChipInstant,
        input: &[Complex32],
    ) -> Result<TimingWindow<'_>, DspError> {
        if input.len() != self.plan.input_length {
            return Err(DspError::InvalidLength);
        }
        start
            .index
            .checked_add(input.len() as u64)
            .ok_or(DspError::InvalidLength)?;
        if input
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(DspError::NonFiniteEvidence);
        }
        self.spectrum.fill(Complex64::default());
        for (output, input) in self.spectrum.iter_mut().zip(input) {
            *output = Complex64::new(f64::from(input.re), f64::from(input.im));
        }
        let width = self.plan.span.len();
        let power = |value: Complex32| f64::from(value.re).powi(2) + f64::from(value.im).powi(2);
        let mut energy = CompensatedSum::default();
        for &sample in &input[..width] {
            energy.add(power(sample));
        }
        self.energies[0] = energy.value();
        for delay in 1..self.plan.delays {
            energy.add(-power(input[delay - 1]));
            energy.add(power(input[delay + width - 1]));
            self.energies[delay] = energy.value();
        }
        self.plan
            .forward
            .process_with_scratch(&mut self.spectrum, &mut self.scratch);
        for (row, (_, reference)) in self.plan.spectra.iter().enumerate() {
            for ((product, input), reference) in
                self.product.iter_mut().zip(&self.spectrum).zip(reference)
            {
                *product = input * reference;
            }
            self.plan
                .inverse
                .process_with_scratch(&mut self.product, &mut self.scratch);
            let destination =
                &mut self.correlations[row * self.plan.delays..(row + 1) * self.plan.delays];
            for (output, input) in destination.iter_mut().zip(&self.product) {
                *output = input / width as f64;
            }
        }
        Ok(TimingWindow {
            receiver: self,
            start,
        })
    }
}

pub struct TimingWindow<'a> {
    receiver: &'a PreambleTimingSearch,
    start: ChipInstant,
}

impl TimingWindow<'_> {
    pub fn correlation(
        &self,
        delay: usize,
        signature: PreambleSignature,
    ) -> Result<Option<TimingCorrelation>, DspError> {
        let plan = &self.receiver.plan;
        if delay >= plan.delays {
            return Err(DspError::InvalidLength);
        }
        let Some(row) = plan.rows[usize::from(signature.number())] else {
            return Ok(None);
        };
        let amplitude = self.receiver.correlations[row * plan.delays + delay];
        let energy = self.receiver.energies[delay];
        Ok(Some(TimingCorrelation {
            code: plan.code,
            signature,
            correlation_start: ChipInstant {
                epoch: self.start.epoch,
                index: self.start.index + delay as u64,
            },
            preamble_offset: plan.span.start,
            amplitude,
            coherent_fraction: if energy > 0.0 {
                amplitude.norm_sqr() * plan.span.len() as f64 / energy
            } else {
                0.0
            },
            mean_input_power: energy / plan.span.len() as f64,
        }))
    }

    // This is a measurement maximum, not a detection threshold or response admission.
    pub fn strongest(&self, signature: PreambleSignature) -> Option<TimingCorrelation> {
        (0..self.receiver.plan.delays)
            .filter_map(|delay| self.correlation(delay, signature).ok().flatten())
            .filter(|result| result.coherent_fraction > 0.0)
            .max_by(|a, b| a.coherent_fraction.total_cmp(&b.coherent_fraction))
    }
}

#[cfg(test)]
mod tests;
