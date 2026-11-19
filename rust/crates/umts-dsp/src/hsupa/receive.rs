// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CONTROL_BITS, ControlDecision, ControlDecoder, UplinkChannels, physical::SlotEvidence,
};
use crate::{
    DspError,
    dedicated::{LikelihoodSettings, UplinkDpchCode, UplinkSlot},
    pilots::PilotEstimate,
    soft_bits::normalize_soft_bits,
};
use num_complex::Complex64;
use std::{num::NonZeroUsize, sync::Arc};
use umts_types::{
    hsupa::{Etfci, Rsn, Tti},
    time::{CHIPS_PER_SLOT, ChipInstant},
};

const CONTROL_SYMBOLS_PER_SLOT: usize = 10;
const CONTROL_SLOTS: usize = CONTROL_BITS / CONTROL_SYMBOLS_PER_SLOT;
const EQUALIZER_SPAN: usize = 4;
const EQUALIZER_TAPS: usize = 2 * EQUALIZER_SPAN + 1;
const EQUALIZER_LOADING: f64 = 1e-3;

pub struct ChipEqualizer {
    received: Vec<Complex64>,
    target: Vec<Complex64>,
    equalized: Vec<Complex64>,
    scratch: Vec<Complex64>,
}

impl ChipEqualizer {
    pub fn new(interval: Tti) -> Self {
        let chips = interval.chips() as usize;
        Self {
            received: vec![Complex64::default(); chips],
            target: vec![Complex64::default(); chips],
            equalized: vec![Complex64::default(); chips],
            scratch: vec![Complex64::default(); CHIPS_PER_SLOT as usize / 2],
        }
    }
    pub fn retained_bytes(&self) -> usize {
        (self.received.capacity()
            + self.target.capacity()
            + self.equalized.capacity()
            + self.scratch.capacity())
            * std::mem::size_of::<Complex64>()
    }
    pub fn samples(&self) -> &[Complex64] {
        &self.equalized
    }
}

fn solve_taps(
    mut matrix: [[Complex64; EQUALIZER_TAPS]; EQUALIZER_TAPS],
    mut vector: [Complex64; EQUALIZER_TAPS],
) -> Option<[Complex64; EQUALIZER_TAPS]> {
    for column in 0..EQUALIZER_TAPS {
        let pivot = (column..EQUALIZER_TAPS).max_by(|a, b| {
            matrix[*a][column]
                .norm_sqr()
                .total_cmp(&matrix[*b][column].norm_sqr())
        })?;
        let magnitude = matrix[pivot][column].norm_sqr();
        if !magnitude.is_finite() || magnitude <= 0.0 {
            return None;
        }
        matrix.swap(column, pivot);
        vector.swap(column, pivot);
        for row in column + 1..EQUALIZER_TAPS {
            let factor = matrix[row][column] / matrix[column][column];
            let pivot_row = matrix[column];
            for (cell, value) in matrix[row][column..].iter_mut().zip(&pivot_row[column..]) {
                *cell -= factor * value;
            }
            let value = vector[column];
            vector[row] -= factor * value;
        }
    }
    let mut taps = [Complex64::default(); EQUALIZER_TAPS];
    for row in (0..EQUALIZER_TAPS).rev() {
        let mut value = vector[row];
        for index in row + 1..EQUALIZER_TAPS {
            value -= matrix[row][index] * taps[index];
        }
        taps[row] = value / matrix[row][row];
        if !taps[row].re.is_finite() || !taps[row].im.is_finite() {
            return None;
        }
    }
    Some(taps)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DataMeasurements {
    pub gain_ratio: f64,
    pub symbols: usize,
    pub in_phase_power: f64,
    pub quadrature_power: f64,
    pub absolute_in_phase: f64,
    pub squared_mean: Complex64,
    pub fourth_mean: Complex64,
}

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionProgress {
    AwaitBoundary,
    Collecting,
    Complete,
}

pub struct TtiCollector {
    code: Arc<UplinkDpchCode>,
    origin: ChipInstant,
    interval: Tti,
    control: UplinkChannels,
    gain_ratio: f64,
    likelihood: LikelihoodSettings,
    samples: Vec<Complex64>,
    pilots: Vec<PilotEstimate>,
    pilot_counts: Vec<usize>,
    frame_slots: Vec<usize>,
    control_soft: [f64; CONTROL_BITS],
    presence_correlation: [f64; CONTROL_BITS],
    presence_expected: f64,
    presence_valid: bool,
    start: Option<ChipInstant>,
    last: Option<ChipInstant>,
    collected: usize,
}

impl TtiCollector {
    pub fn uses_code(&self, code: &Arc<UplinkDpchCode>) -> bool {
        Arc::ptr_eq(&self.code, code)
    }
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.samples.capacity() * std::mem::size_of::<Complex64>()
            + self.pilots.capacity() * std::mem::size_of::<PilotEstimate>()
            + (self.pilot_counts.capacity() + self.frame_slots.capacity())
                * std::mem::size_of::<usize>()
            + self.control.retained_bytes()
            - std::mem::size_of::<UplinkChannels>()
    }
    pub fn required_storage_bytes(interval: Tti) -> usize {
        interval.chips() as usize * std::mem::size_of::<Complex64>()
            + (interval.chips() / CHIPS_PER_SLOT) as usize
                * (std::mem::size_of::<PilotEstimate>() + 2 * std::mem::size_of::<usize>())
    }

    pub fn new(
        code: Arc<UplinkDpchCode>,
        origin: ChipInstant,
        interval: Tti,
        gain_ratio: f64,
        likelihood: LikelihoodSettings,
        maximum_storage_bytes: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if !gain_ratio.is_finite() || gain_ratio <= 0.0 {
            return Err(DspError::InvalidAmplitude);
        }
        if Self::required_storage_bytes(interval) > maximum_storage_bytes.get() {
            return Err(DspError::Capacity);
        }
        let control = UplinkChannels::new(Arc::clone(&code), origin, 0, false, &[256])?;
        let slots = (interval.chips() / CHIPS_PER_SLOT) as usize;
        Ok(Self {
            code,
            origin,
            interval,
            control,
            gain_ratio,
            likelihood,
            samples: vec![Complex64::default(); interval.chips() as usize],
            pilots: vec![
                PilotEstimate {
                    channel: Complex64::default(),
                    observed_power: 0.0,
                    residual_power: 0.0
                };
                slots
            ],
            pilot_counts: vec![0; slots],
            frame_slots: vec![0; slots],
            control_soft: [0.0; CONTROL_BITS],
            presence_correlation: [0.0; CONTROL_BITS],
            presence_expected: 0.0,
            presence_valid: true,
            start: None,
            last: None,
            collected: 0,
        })
    }

    // A completed interval remains owned until release. A gap cannot manufacture missing samples.
    pub fn feed(&mut self, slot: &UplinkSlot<'_>) -> Result<CollectionProgress, DspError> {
        self.control.validate_at(slot.code(), slot.start())?;
        let time = slot.start();
        if self.last.is_some_and(|last| time.index <= last.index) {
            return Err(DspError::DiscontinuousTime);
        }
        if self.collected == self.pilots.len() {
            return Err(DspError::Capacity);
        }
        let delta = time
            .index
            .checked_sub(self.origin.index)
            .ok_or(DspError::DiscontinuousTime)?;
        let boundary = delta.is_multiple_of(self.interval.chips());
        let contiguous = self
            .last
            .is_some_and(|last| last.index.checked_add(CHIPS_PER_SLOT) == Some(time.index));
        if !contiguous {
            self.abandon();
        }
        if boundary {
            self.abandon();
            self.start = Some(time);
        }
        if self.start.is_none() {
            self.last = Some(time);
            return Ok(CollectionProgress::AwaitBoundary);
        }
        let evidence =
            match self
                .control
                .control_slot_evidence(slot, self.gain_ratio, self.likelihood)
            {
                Ok(llrs) => llrs,
                Err(error) => {
                    self.abandon();
                    self.last = Some(time);
                    return Err(error);
                }
            };
        let llrs = evidence.llrs;
        let control_offset = (self.collected % CONTROL_SLOTS) * CONTROL_SYMBOLS_PER_SLOT;
        if llrs
            .iter()
            .enumerate()
            .any(|(index, value)| !(self.control_soft[control_offset + index] + value).is_finite())
        {
            self.abandon();
            self.last = Some(time);
            return Err(DspError::NonFiniteEvidence);
        }
        let first = self.collected * CHIPS_PER_SLOT as usize;
        self.samples[first..first + CHIPS_PER_SLOT as usize]
            .copy_from_slice(slot.corrected_samples());
        self.pilots[self.collected] = slot.control().pilot;
        self.pilot_counts[self.collected] = slot.pilot_count();
        self.frame_slots[self.collected] = slot.frame_slot();
        for (index, value) in llrs.into_iter().enumerate() {
            self.control_soft[control_offset + index] += value;
        }
        if let Some((correlation, expected)) = evidence.presence {
            self.presence_expected += expected;
            for (index, value) in correlation.into_iter().enumerate() {
                self.presence_correlation[control_offset + index] += value;
                self.presence_valid &=
                    self.presence_correlation[control_offset + index].is_finite();
            }
            self.presence_valid &= self.presence_expected.is_finite();
        } else {
            self.presence_valid = false;
        }
        self.collected += 1;
        self.last = Some(time);
        Ok(if self.collected == self.pilots.len() {
            CollectionProgress::Complete
        } else {
            CollectionProgress::Collecting
        })
    }

    pub fn complete(&self) -> Option<CollectedTti<'_>> {
        (self.collected == self.pilots.len()).then_some(CollectedTti {
            collector: self,
            samples: &self.samples,
        })
    }

    pub fn release(&mut self) {
        self.abandon();
    }

    fn abandon(&mut self) {
        self.start = None;
        self.collected = 0;
        self.control_soft.fill(0.0);
        self.presence_correlation.fill(0.0);
        self.presence_expected = 0.0;
        self.presence_valid = true;
    }
}

pub struct CollectedTti<'a> {
    collector: &'a TtiCollector,
    samples: &'a [Complex64],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ControlMeasurements {
    pub pilot_power: f64,
    pub pilot_residual_power: f64,
    pub control_in_phase_power: f64,
    pub control_quadrature_power: f64,
}

impl<'a> CollectedTti<'a> {
    pub fn equalize(
        &self,
        channels: &UplinkChannels,
        gain_ratios: &[f64],
        equalizer: &mut ChipEqualizer,
    ) -> Result<bool, DspError> {
        let collector = self.collector;
        let chips = collector.samples.len();
        let slot_chips = CHIPS_PER_SLOT as usize;
        if equalizer.received.len() != chips
            || equalizer.target.len() != chips
            || equalizer.equalized.len() != chips
        {
            return Err(DspError::InvalidLength);
        }
        let scrambling = collector.code.scrambling();
        let code = |slot: usize, chip: usize| {
            let value = scrambling[collector.frame_slots[slot] * slot_chips + chip];
            Complex64::new(f64::from(value.re), f64::from(value.im))
        };
        for (index, pilot) in collector.pilots.iter().enumerate() {
            let first = index * slot_chips;
            let slot = SlotEvidence {
                code: &collector.code,
                start: ChipInstant {
                    index: self.start().index + index as u64 * CHIPS_PER_SLOT,
                    ..self.start()
                },
                samples: &collector.samples[first..first + slot_chips],
                pilot: *pilot,
                pilot_count: collector.pilot_counts[index],
            };
            channels.data_reference_chips(
                &slot,
                gain_ratios,
                &mut equalizer.scratch,
                &mut equalizer.target[first..first + slot_chips],
            )?;
            for chip in 0..slot_chips {
                let scramble = code(index, chip);
                equalizer.received[first + chip] = collector.samples[first + chip] * scramble;
                equalizer.target[first + chip] *= scramble * 0.5;
            }
        }
        let received = &equalizer.received;
        let target = &equalizer.target;
        let mut correlation = [Complex64::default(); EQUALIZER_TAPS];
        for (lag, value) in correlation.iter_mut().enumerate() {
            *value = received[..chips - lag]
                .iter()
                .zip(&received[lag..])
                .map(|(early, late)| early.conj() * late)
                .sum::<Complex64>()
                * 0.5;
        }
        let mut cross = [Complex64::default(); EQUALIZER_TAPS];
        for (tap, value) in cross.iter_mut().enumerate() {
            *value = if tap >= EQUALIZER_SPAN {
                let offset = tap - EQUALIZER_SPAN;
                received[offset..]
                    .iter()
                    .zip(&target[..chips - offset])
                    .map(|(sample, wanted)| sample.conj() * wanted)
                    .sum::<Complex64>()
            } else {
                let offset = EQUALIZER_SPAN - tap;
                received[..chips - offset]
                    .iter()
                    .zip(&target[offset..])
                    .map(|(sample, wanted)| sample.conj() * wanted)
                    .sum::<Complex64>()
            };
        }
        let loading = EQUALIZER_LOADING * correlation[0].re;
        if !loading.is_finite() || loading <= 0.0 {
            return Ok(false);
        }
        let mut matrix = [[Complex64::default(); EQUALIZER_TAPS]; EQUALIZER_TAPS];
        for (row, values) in matrix.iter_mut().enumerate() {
            for (column, value) in values.iter_mut().enumerate() {
                *value = if column >= row {
                    correlation[column - row]
                } else {
                    correlation[row - column].conj()
                };
            }
            values[row] += loading;
        }
        let Some(taps) = solve_taps(matrix, cross) else {
            return Ok(false);
        };
        let mut target_power = 0.0;
        let mut alignment = Complex64::default();
        for (chip, (reference, output)) in target[..chips]
            .iter()
            .zip(&mut equalizer.equalized[..chips])
            .enumerate()
        {
            let mut value = Complex64::default();
            for (tap, weight) in taps.iter().enumerate() {
                if let Some(sample) = (chip + tap)
                    .checked_sub(EQUALIZER_SPAN)
                    .and_then(|at| received.get(at))
                {
                    value += weight * sample;
                }
            }
            target_power += reference.norm_sqr();
            alignment += reference.conj() * value;
            *output = value;
        }
        let scale = Complex64::new(2.0 * target_power, 0.0) / alignment;
        let magnitude = scale.norm();
        if !magnitude.is_finite() || !(0.5..=4.0).contains(&magnitude) {
            return Ok(false);
        }
        for index in 0..collector.pilots.len() {
            let first = index * slot_chips;
            for chip in 0..slot_chips {
                let value = &mut equalizer.equalized[first + chip];
                *value = *value * scale * code(index, chip).conj() * 0.5;
                if !value.re.is_finite() || !value.im.is_finite() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub fn with_samples(self, samples: &'a [Complex64]) -> Result<Self, DspError> {
        if samples.len() != self.collector.samples.len() {
            return Err(DspError::InvalidLength);
        }
        Ok(Self { samples, ..self })
    }

    // Equal-variance complex Gaussian likelihood of the selected word versus silence.
    // Pilot uncertainty is included, but this model is not a calibrated RF false-alarm bound.
    pub fn control_presence_log_likelihood(&self, decision: ControlDecision) -> Option<f64> {
        if !self.collector.presence_valid {
            return None;
        }
        let correlation: f64 = decision
            .information
            .encode()
            .iter()
            .zip(self.collector.presence_correlation)
            .map(|(bit, value)| {
                if *bit == crate::bits::Bit::Zero {
                    value
                } else {
                    -value
                }
            })
            .sum();
        let likelihood = correlation - self.collector.presence_expected;
        likelihood.is_finite().then_some(likelihood)
    }
    // Diagnostic-only additional despreading. No estimate is noise-subtracted or calibrated to RF power.
    pub fn measure_control(&self) -> Result<ControlMeasurements, DspError> {
        let mut result = ControlMeasurements::default();
        let slots = self.collector.pilots.len();
        for (samples, pilot) in self
            .collector
            .samples
            .chunks_exact(CHIPS_PER_SLOT as usize)
            .zip(&self.collector.pilots)
        {
            let power = pilot.channel.norm_sqr();
            if !power.is_finite() || power <= 0.0 {
                return Err(DspError::NonFiniteEvidence);
            }
            result.pilot_power += power;
            result.pilot_residual_power += pilot.residual_power;
            let rotation = pilot.channel.conj() / power.sqrt();
            for symbol in self
                .collector
                .control
                .control_symbols_from_samples(samples)?
            {
                let aligned = symbol * rotation;
                result.control_in_phase_power += aligned.re * aligned.re;
                result.control_quadrature_power += aligned.im * aligned.im;
            }
        }
        result.pilot_power /= slots as f64;
        result.pilot_residual_power /= slots as f64;
        let symbols = (slots * CONTROL_SYMBOLS_PER_SLOT) as f64;
        result.control_in_phase_power /= symbols;
        result.control_quadrature_power /= symbols;
        Ok(result)
    }

    pub fn control_evidence(&self) -> &[f64; CONTROL_BITS] {
        &self.collector.control_soft
    }

    pub fn start(&self) -> ChipInstant {
        self.collector.start.expect("complete interval")
    }
    pub fn interval(&self) -> Tti {
        self.collector.interval
    }

    pub fn decode_control(
        &self,
        decoder: &ControlDecoder,
    ) -> Result<Option<ControlDecision>, DspError> {
        self.decode_control_with_context(decoder, None, 0.0)
    }

    pub fn decode_control_with_context(
        &self,
        decoder: &ControlDecoder,
        expected: Option<(Etfci, Rsn)>,
        tolerance: f64,
    ) -> Result<Option<ControlDecision>, DspError> {
        let mut soft = [0.0; CONTROL_BITS];
        normalize_soft_bits(&self.collector.control_soft, &mut soft)?;
        decoder.decode_with_context(&soft, expected, tolerance)
    }

    pub fn data_llrs(
        &self,
        channels: &UplinkChannels,
        channel: usize,
        gain_ratio: f64,
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: &mut [f64],
    ) -> Result<(), DspError> {
        let per_slot = channels
            .data_symbols_per_slot(channel)
            .ok_or(DspError::InvalidCode)?;
        if scratch.len() != per_slot || output.len() != per_slot * self.collector.pilots.len() {
            return Err(DspError::InvalidLength);
        }
        for (index, output) in output.chunks_exact_mut(per_slot).enumerate() {
            let first = index * CHIPS_PER_SLOT as usize;
            let slot = SlotEvidence {
                code: &self.collector.code,
                start: ChipInstant {
                    index: self.start().index + index as u64 * CHIPS_PER_SLOT,
                    ..self.start()
                },
                samples: &self.samples[first..first + CHIPS_PER_SLOT as usize],
                pilot: self.collector.pilots[index],
                pilot_count: self.collector.pilot_counts[index],
            };
            channels.data_evidence_llrs(&slot, channel, gain_ratio, settings, scratch, output)?;
        }
        Ok(())
    }

    /// Recover an adjacent I/Q E-DPDCH pair with one shared OVSF sum per symbol.
    pub fn data_pair_llrs(
        &self,
        channels: &UplinkChannels,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.pair_llrs::<false, false>(
            channels,
            first_channel,
            gain_ratios,
            settings,
            scratch,
            output,
        )
    }

    /// Refine an equal-gain QPSK pair's residual phase using the pilot as quadrant and confidence anchor.
    /// Unequal-gain pairs retain the unrefined projection.
    pub fn data_pair_tracked_llrs(
        &self,
        channels: &UplinkChannels,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.pair_llrs::<true, false>(
            channels,
            first_channel,
            gain_ratios,
            settings,
            scratch,
            output,
        )
    }

    /// Refine an equal-gain QPSK pair's phase and amplitude using received moments and pilot confidence.
    pub fn data_pair_calibrated_llrs(
        &self,
        channels: &UplinkChannels,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.pair_llrs::<true, true>(
            channels,
            first_channel,
            gain_ratios,
            settings,
            scratch,
            output,
        )
    }

    fn pair_llrs<const TRACK_PHASE: bool, const TRACK_GAIN: bool>(
        &self,
        channels: &UplinkChannels,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        let second_channel = first_channel.checked_add(1).ok_or(DspError::InvalidCode)?;
        let per_slot = channels
            .data_symbols_per_slot(first_channel)
            .ok_or(DspError::InvalidCode)?;
        if channels.data_symbols_per_slot(second_channel) != Some(per_slot) {
            return Err(DspError::InvalidCode);
        }
        let [first_output, second_output] = output;
        let length = per_slot * self.collector.pilots.len();
        if scratch.len() != per_slot
            || first_output.len() != length
            || second_output.len() != length
        {
            return Err(DspError::InvalidLength);
        }
        for (index, (first_output, second_output)) in first_output
            .chunks_exact_mut(per_slot)
            .zip(second_output.chunks_exact_mut(per_slot))
            .enumerate()
        {
            let first = index * CHIPS_PER_SLOT as usize;
            let slot = SlotEvidence {
                code: &self.collector.code,
                start: ChipInstant {
                    index: self.start().index + index as u64 * CHIPS_PER_SLOT,
                    ..self.start()
                },
                samples: &self.samples[first..first + CHIPS_PER_SLOT as usize],
                pilot: self.collector.pilots[index],
                pilot_count: self.collector.pilot_counts[index],
            };
            if TRACK_GAIN {
                channels.data_pair_evidence_calibrated_llrs(
                    &slot,
                    first_channel,
                    gain_ratios,
                    settings,
                    scratch,
                    [first_output, second_output],
                )?;
            } else if TRACK_PHASE {
                channels.data_pair_evidence_tracked_llrs(
                    &slot,
                    first_channel,
                    gain_ratios,
                    settings,
                    scratch,
                    [first_output, second_output],
                )?;
            } else {
                channels.data_pair_evidence_llrs(
                    &slot,
                    first_channel,
                    gain_ratios,
                    settings,
                    scratch,
                    [first_output, second_output],
                )?;
            }
        }
        Ok(())
    }

    /// Power is normalized by the signaled data gain and the slot's pilot channel estimate.
    pub fn measure_data(
        &self,
        channels: &UplinkChannels,
        channel: usize,
        gain_ratio: f64,
        scratch: &mut [Complex64],
    ) -> Result<DataMeasurements, DspError> {
        if !gain_ratio.is_finite() || gain_ratio <= 0.0 {
            return Err(DspError::InvalidAmplitude);
        }
        let per_slot = channels
            .data_symbols_per_slot(channel)
            .ok_or(DspError::InvalidCode)?;
        if scratch.len() != per_slot {
            return Err(DspError::InvalidLength);
        }
        let mut result = DataMeasurements {
            gain_ratio,
            ..DataMeasurements::default()
        };
        for (index, pilot) in self.collector.pilots.iter().enumerate() {
            let power = pilot.channel.norm_sqr();
            if !power.is_finite() || power <= 0.0 {
                return Err(DspError::NonFiniteEvidence);
            }
            let first = index * CHIPS_PER_SLOT as usize;
            let slot = SlotEvidence {
                code: &self.collector.code,
                start: ChipInstant {
                    index: self.start().index + index as u64 * CHIPS_PER_SLOT,
                    ..self.start()
                },
                samples: &self.samples[first..first + CHIPS_PER_SLOT as usize],
                pilot: *pilot,
                pilot_count: self.collector.pilot_counts[index],
            };
            channels.data_evidence_symbols(&slot, channel, scratch)?;
            let normalize = pilot.channel.conj() / (power * gain_ratio);
            for symbol in scratch.iter() {
                let value = *symbol * normalize;
                result.in_phase_power += value.re * value.re;
                result.quadrature_power += value.im * value.im;
                result.absolute_in_phase += value.re.abs();
                let squared = value * value;
                result.squared_mean += squared;
                result.fourth_mean += squared * squared;
                result.symbols += 1;
            }
        }
        let count = result.symbols as f64;
        result.in_phase_power /= count;
        result.quadrature_power /= count;
        result.absolute_in_phase /= count;
        result.squared_mean /= count;
        result.fourth_mean /= count;
        if !result.in_phase_power.is_finite()
            || !result.quadrature_power.is_finite()
            || !result.absolute_in_phase.is_finite()
            || !result.squared_mean.re.is_finite()
            || !result.squared_mean.im.is_finite()
            || !result.fourth_mean.re.is_finite()
            || !result.fourth_mean.im.is_finite()
        {
            return Err(DspError::NonFiniteEvidence);
        }
        Ok(result)
    }
}
