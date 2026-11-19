// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::SampleInstant;

mod vector;
use vector::Coefficients;

use crate::{
    DspError,
    pulse_shaping::{SamplesPerChip, pulse_taps},
};

const INPUT_BLOCK_SAMPLES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilteredWindow {
    // Receive-filter delay is removed, but transmitter/channel delay is not.
    pub first_center: Option<SampleInstant>,
    pub sample_stride: usize,
    pub next_input: SampleInstant,
}

pub struct ReceiveFilter {
    next: SampleInstant,
    stride: usize,
    phase: u64,
    taps: Vec<f32>,
    coefficients: Coefficients,
    history: Vec<Complex32>,
    filled: usize,
}

impl ReceiveFilter {
    // TS 25.101 6.8.1. Phase selects an absolute sample-index residue, not a chip epoch.
    pub fn new(
        rate: SamplesPerChip,
        span_chips: usize,
        phase: usize,
        origin: SampleInstant,
    ) -> Result<Self, DspError> {
        if phase >= rate.get() {
            return Err(DspError::InvalidFilter);
        }
        let prototype = pulse_taps(rate, span_chips)?;
        let mut taps = Vec::new();
        taps.try_reserve_exact(prototype.len())
            .map_err(|_| DspError::Capacity)?;
        // Unit chip gain for the matched transmit pulse, without AGC or integer quantization.
        taps.extend(
            prototype
                .into_iter()
                .rev()
                .map(|tap| (tap / rate.get() as f64) as f32),
        );
        if !taps.iter().zip(taps.iter().rev()).all(|(a, b)| a == b) {
            return Err(DspError::InvalidFilter);
        }
        let coefficients = Coefficients::new(&taps)?;
        let storage = taps
            .len()
            .checked_add(INPUT_BLOCK_SAMPLES - 1)
            .ok_or(DspError::InvalidLength)?;
        let mut history = Vec::new();
        history
            .try_reserve_exact(storage)
            .map_err(|_| DspError::Capacity)?;
        history.resize(storage, Complex32::default());
        Ok(Self {
            next: origin,
            stride: rate.get(),
            phase: phase as u64,
            taps,
            coefficients,
            history,
            filled: 0,
        })
    }

    pub fn group_delay_samples(&self) -> usize {
        (self.taps.len() - 1) / 2
    }
    pub fn full_rate(
        rate: SamplesPerChip,
        span_chips: usize,
        origin: SampleInstant,
    ) -> Result<Self, DspError> {
        let mut filter = Self::new(rate, span_chips, 0, origin)?;
        filter.stride = 1;
        Ok(filter)
    }

    pub fn next_input(&self) -> SampleInstant {
        self.next
    }

    pub fn reset(&mut self, origin: SampleInstant) {
        self.next = origin;
        self.history.fill(Complex32::default());
        self.filled = 0;
    }

    fn layout(&self, count: usize) -> Result<(u64, Option<u64>, usize), DspError> {
        let end = self
            .next
            .index
            .checked_add(count as u64)
            .ok_or(DspError::InvalidLength)?;
        let warmup = (self.taps.len() - 1).saturating_sub(self.filled);
        if count <= warmup {
            return Ok((end, None, 0));
        }
        let ready = self
            .next
            .index
            .checked_add(warmup as u64)
            .ok_or(DspError::InvalidLength)?;
        let stride = self.stride as u64;
        let offset = (self.phase + stride - ready % stride) % stride;
        if offset >= end - ready {
            return Ok((end, None, 0));
        }
        let first = ready + offset;
        Ok((end, Some(first), ((end - 1 - first) / stride + 1) as usize))
    }

    pub fn output_length(&self, samples: usize) -> Result<usize, DspError> {
        Ok(self.layout(samples)?.2)
    }

    // Gaps require an explicit reset. No output is published with incomplete filter history.
    pub fn process(
        &mut self,
        start: SampleInstant,
        input: &[Complex32],
        output: &mut [Complex32],
    ) -> Result<FilteredWindow, DspError> {
        if start != self.next {
            return Err(DspError::DiscontinuousTime);
        }
        let (end, first, count) = self.layout(input.len())?;
        if output.len() != count {
            return Err(DspError::InvalidLength);
        }
        if input
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(DspError::NonFiniteEvidence);
        }
        let length = self.taps.len();
        #[cfg(target_feature = "avx2")]
        let vector_eight = self.stride == 1 && length == vector::SPECIALIZED_TAPS;
        #[cfg(target_feature = "avx2")]
        let vector_short = self.stride == 1 && length == 33;
        let mut destination = output;
        let mut emit = first;
        let mut block_start = start.index;
        for block in input.chunks(INPUT_BLOCK_SAMPLES) {
            self.history.copy_within(..length - 1, block.len());
            for (stored, &sample) in self.history[..block.len()]
                .iter_mut()
                .zip(block.iter().rev())
            {
                *stored = sample;
            }
            let block_end = block_start + block.len() as u64;
            #[cfg(target_feature = "avx2")]
            if vector_eight
                && emit.is_some_and(|index| {
                    block_end.saturating_sub(index) >= vector::PlanarBlock::OUTPUTS as u64
                })
            {
                let planar = vector::PlanarBlock::new(&self.history);
                while let Some(index) = emit.filter(|&index| index < block_end) {
                    let head = (block_end - index - 1) as usize;
                    if head < vector::PlanarBlock::OUTPUTS - 1 {
                        break;
                    }
                    let values = planar
                        .consecutive_eight(head + 1 - vector::PlanarBlock::OUTPUTS, &self.taps);
                    destination[..vector::PlanarBlock::OUTPUTS].copy_from_slice(&values);
                    destination = &mut destination[vector::PlanarBlock::OUTPUTS..];
                    emit = index.checked_add(vector::PlanarBlock::OUTPUTS as u64);
                }
            }
            #[cfg(target_feature = "avx2")]
            if vector_short
                && emit.is_some_and(|index| {
                    block_end.saturating_sub(index) >= vector::PlanarBlock33::OUTPUTS as u64
                })
            {
                let planar = vector::PlanarBlock33::new(&self.history);
                while let Some(index) = emit.filter(|&index| index < block_end) {
                    let head = (block_end - index - 1) as usize;
                    if head < vector::PlanarBlock33::OUTPUTS - 1 {
                        break;
                    }
                    let values = planar
                        .consecutive_eight(head + 1 - vector::PlanarBlock33::OUTPUTS, &self.taps);
                    destination[..vector::PlanarBlock33::OUTPUTS].copy_from_slice(&values);
                    destination = &mut destination[vector::PlanarBlock33::OUTPUTS..];
                    emit = index.checked_add(vector::PlanarBlock33::OUTPUTS as u64);
                }
            }
            while let Some(index) = emit.filter(|&index| index < block_end) {
                let head = (block_end - index - 1) as usize;
                destination[0] = self
                    .coefficients
                    .dot(&self.history[head..head + length], &self.taps);
                destination = &mut destination[1..];
                emit = index.checked_add(self.stride as u64);
            }
            block_start = block_end;
        }
        self.filled = self.filled.saturating_add(input.len()).min(length);
        self.next.index = end;
        Ok(FilteredWindow {
            first_center: first.map(|index| SampleInstant {
                epoch: start.epoch,
                index: index - self.group_delay_samples() as u64,
            }),
            sample_stride: self.stride,
            next_input: self.next,
        })
    }
}

#[cfg(test)]
fn filter_dot_scalar(values: &[Complex32], taps: &[f32]) -> Complex32 {
    const LANES: usize = 4;
    let half = taps.len() / 2;
    let mut sums = [Complex32::default(); LANES];
    let mut front = values[..half].chunks_exact(LANES);
    let mut back = values[half + 1..].rchunks_exact(LANES);
    let mut coefficients = taps[..half].chunks_exact(LANES);
    for ((front, back), taps) in front.by_ref().zip(back.by_ref()).zip(coefficients.by_ref()) {
        for lane in 0..LANES {
            sums[lane] += (front[lane] + back[LANES - 1 - lane]) * taps[lane];
        }
    }
    let mut sum = (sums[0] + sums[1]) + (sums[2] + sums[3]);
    for ((&front, &back), &tap) in front
        .remainder()
        .iter()
        .zip(back.remainder().iter().rev())
        .zip(coefficients.remainder())
    {
        sum += (front + back) * tap;
    }
    sum + values[half] * taps[half]
}

#[cfg(test)]
mod tests;
