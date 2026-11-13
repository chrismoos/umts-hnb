// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{MAX_CODES, Modulation, SYMBOLS_PER_SUBFRAME};
use crate::{
    DspError,
    bits::Bit,
    channel_codes::OvsfCode,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
};
use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant};
use wide::f32x8;

const SPREADING_FACTOR: usize = 16;
const SUBFRAME_SLOTS: usize = 3;
const SYMBOLS_PER_SLOT: usize = CHIPS_PER_SLOT as usize / SPREADING_FACTOR;
const SCCH_SPREADING_FACTOR: usize = 128;
const SCCH_SLOT_BITS: usize = 40;
const MAX_SYMBOL_BITS: usize = 6;
const CONSTELLATION_SIZE: usize = 1 << MAX_SYMBOL_BITS;
const SCRAMBLING_VALUES: [Complex32; 4] = [
    Complex32::new(1.0, 1.0),
    Complex32::new(-1.0, 1.0),
    Complex32::new(1.0, -1.0),
    Complex32::new(-1.0, -1.0),
];

pub struct ScchSpreader {
    weights: Vec<Complex32>,
}

impl ScchSpreader {
    pub fn new(primary: PrimaryScramblingCode, code: u16) -> Result<Self, DspError> {
        let channel = OvsfCode::new(SCCH_SPREADING_FACTOR as u16, code)?;
        let mut weights = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(primary.sequence_number(), &mut weights)?;
        let mut chips = [0.0; SCCH_SPREADING_FACTOR];
        channel.write(&mut chips)?;
        for (index, weight) in weights.iter_mut().enumerate() {
            *weight *= chips[index % SCCH_SPREADING_FACTOR] * 0.5;
        }
        Ok(Self { weights })
    }

    pub fn add_slot(
        &self,
        bits: &[Bit; super::scch::SCCH_BITS],
        subframe_start: ChipInstant,
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if slot >= SUBFRAME_SLOTS || output.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        if !subframe_start.index.is_multiple_of(CHIPS_PER_SLOT) {
            return Err(DspError::DiscontinuousTime);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let start = subframe_start
            .index
            .checked_add(slot as u64 * CHIPS_PER_SLOT)
            .ok_or(DspError::DiscontinuousTime)?;
        start
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(DspError::DiscontinuousTime)?;
        let offset = (start % CHIPS_PER_FRAME) as usize;
        let weights = &self.weights[offset..offset + CHIPS_PER_SLOT as usize];
        for (symbol, values) in bits[slot * SCCH_SLOT_BITS..(slot + 1) * SCCH_SLOT_BITS]
            .chunks_exact(2)
            .enumerate()
        {
            let value = Complex32::new(sign(values[0]), sign(values[1])) * amplitude;
            let first = symbol * SCCH_SPREADING_FACTOR;
            for (chip, weight) in output[first..first + SCCH_SPREADING_FACTOR]
                .iter_mut()
                .zip(&weights[first..first + SCCH_SPREADING_FACTOR])
            {
                *chip += value * *weight;
            }
        }
        Ok(())
    }
}

pub struct PdschSpreader {
    codes: Vec<[f32; SPREADING_FACTOR]>,
    phases: Vec<u8>,
    #[cfg(test)]
    scrambling: Vec<Complex32>,
}

pub struct PdschView<'a> {
    codes: &'a [[f32; SPREADING_FACTOR]],
    phases: &'a [u8],
}

impl PdschSpreader {
    pub fn new(
        primary: PrimaryScramblingCode,
        first_code: u16,
        code_count: usize,
    ) -> Result<Self, DspError> {
        if !(1..=MAX_CODES).contains(&code_count)
            || usize::from(first_code) + code_count > SPREADING_FACTOR
        {
            return Err(DspError::InvalidCode);
        }
        let mut codes = vec![[0.0; SPREADING_FACTOR]; code_count];
        for (offset, chips) in codes.iter_mut().enumerate() {
            OvsfCode::new(SPREADING_FACTOR as u16, first_code + offset as u16)?.write(chips)?;
        }
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(primary.sequence_number(), &mut scrambling)?;
        let phases = scrambling
            .iter()
            .map(|value| u8::from(value.re < 0.0) | (u8::from(value.im < 0.0) << 1))
            .collect();
        Ok(Self {
            codes,
            phases,
            #[cfg(test)]
            scrambling,
        })
    }

    pub fn subset(&self, codes: std::ops::Range<usize>) -> Result<PdschView<'_>, DspError> {
        let codes = self
            .codes
            .get(codes)
            .filter(|codes| !codes.is_empty())
            .ok_or(DspError::InvalidCode)?;
        Ok(PdschView {
            codes,
            phases: &self.phases,
        })
    }

    pub fn add_slot(
        &self,
        modulation: Modulation,
        physical_bits: &[Bit],
        subframe_start: ChipInstant,
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        self.subset(0..self.codes.len())?.add_slot(
            modulation,
            physical_bits,
            subframe_start,
            slot,
            amplitude,
            output,
        )
    }
}

impl PdschView<'_> {
    // Amplitude is per-code RMS complex-chip magnitude, matching the common carrier.
    // TS 25.213 5.1.1.2 aligns HS-PDSCH scrambling to P-CCPCH, not the subframe.
    pub fn add_slot(
        &self,
        modulation: Modulation,
        physical_bits: &[Bit],
        subframe_start: ChipInstant,
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if physical_bits.len() != self.codes.len() * modulation.bits_per_code()
            || output.len() != CHIPS_PER_SLOT as usize
            || slot >= SUBFRAME_SLOTS
        {
            return Err(DspError::InvalidLength);
        }
        if !subframe_start.index.is_multiple_of(CHIPS_PER_SLOT) {
            return Err(DspError::DiscontinuousTime);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let first_chip = subframe_start
            .index
            .checked_add(slot as u64 * CHIPS_PER_SLOT)
            .ok_or(DspError::DiscontinuousTime)?;
        first_chip
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(DspError::DiscontinuousTime)?;
        let frame_offset = (first_chip % CHIPS_PER_FRAME) as usize;
        let phases = &self.phases[frame_offset..frame_offset + CHIPS_PER_SLOT as usize];
        if self.codes.len() < 8 {
            self.add_sparse_slot(modulation, physical_bits, slot, amplitude, output, phases);
            return Ok(());
        }
        let scale = amplitude * 0.5;
        let symbol_bits = modulation.bits_per_symbol();
        let mut constellation = [Complex32::default(); CONSTELLATION_SIZE];
        for (label, entry) in constellation.iter_mut().enumerate().take(1 << symbol_bits) {
            let bits: [Bit; MAX_SYMBOL_BITS] =
                std::array::from_fn(|bit| Bit::from(label & (1 << bit) != 0));
            *entry = map_symbol(modulation, &bits) * scale;
        }
        let mut rows = [0; MAX_CODES];
        for (code, row) in self.codes.iter().zip(&mut rows) {
            // Walsh row bits are the OVSF signs at power-of-two chip offsets.
            *row = (0..4).fold(0, |row, bit| {
                row | (usize::from(code[1 << bit] < 0.0) << bit)
            });
        }
        for (symbol, (chips, phases)) in output
            .chunks_exact_mut(SPREADING_FACTOR)
            .zip(phases.chunks_exact(SPREADING_FACTOR))
            .enumerate()
        {
            let mut values = [Complex32::default(); SPREADING_FACTOR];
            let first = (slot * SYMBOLS_PER_SLOT + symbol) * symbol_bits;
            for (bits, &row) in physical_bits
                .chunks_exact(modulation.bits_per_code())
                .zip(&rows)
            {
                let bits = &bits[first..first + symbol_bits];
                let pair = bits[0] as usize | (bits[1] as usize) << 1;
                let label = match modulation {
                    Modulation::Qpsk => pair,
                    Modulation::Qam16 => pair | (bits[2] as usize) << 2 | (bits[3] as usize) << 3,
                    Modulation::Qam64 => {
                        pair | (bits[2] as usize) << 2
                            | (bits[3] as usize) << 3
                            | (bits[4] as usize) << 4
                            | (bits[5] as usize) << 5
                    }
                };
                values[row] = constellation[label];
            }
            for width in [1, 2, 4, 8] {
                for block in values.chunks_exact_mut(2 * width) {
                    let (left, right) = block.split_at_mut(width);
                    for (left, right) in left.iter_mut().zip(right) {
                        let first = *left;
                        *left = first + *right;
                        *right = first - *right;
                    }
                }
            }
            for ((chip, value), &phase) in chips.iter_mut().zip(values).zip(phases) {
                *chip += value * SCRAMBLING_VALUES[usize::from(phase)];
            }
        }
        Ok(())
    }
    fn add_sparse_slot(
        &self,
        modulation: Modulation,
        physical_bits: &[Bit],
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
        phases: &[u8],
    ) {
        let scale = amplitude * 0.5;
        let symbol_bits = modulation.bits_per_symbol();
        let mut constellation = [[Complex32::default(); 4]; CONSTELLATION_SIZE];
        for (label, entry) in constellation.iter_mut().enumerate().take(1 << symbol_bits) {
            let bits: [Bit; MAX_SYMBOL_BITS] =
                std::array::from_fn(|bit| Bit::from(label & (1 << bit) != 0));
            let value = map_symbol(modulation, &bits) * scale;
            *entry = SCRAMBLING_VALUES.map(|weight| value * weight);
        }
        for (code, bits) in self
            .codes
            .iter()
            .zip(physical_bits.chunks_exact(modulation.bits_per_code()))
        {
            let first = slot * SYMBOLS_PER_SLOT * symbol_bits;
            let end = first + SYMBOLS_PER_SLOT * symbol_bits;
            for ((bits, chips), phases) in bits[first..end]
                .chunks_exact(symbol_bits)
                .zip(output.chunks_exact_mut(SPREADING_FACTOR))
                .zip(phases.chunks_exact(SPREADING_FACTOR))
            {
                let label = bits
                    .iter()
                    .enumerate()
                    .fold(0, |label, (index, &bit)| label | (bit as usize) << index);
                let scrambled = &constellation[label];
                for ((chips, phases), code) in chips
                    .chunks_exact_mut(4)
                    .zip(phases.chunks_exact(4))
                    .zip(code.chunks_exact(4))
                {
                    let values: [Complex32; 4] =
                        std::array::from_fn(|index| scrambled[usize::from(phases[index])]);
                    let values = f32x8::new([
                        values[0].re,
                        values[0].im,
                        values[1].re,
                        values[1].im,
                        values[2].re,
                        values[2].im,
                        values[3].re,
                        values[3].im,
                    ]);
                    let weights = f32x8::new([
                        code[0], code[0], code[1], code[1], code[2], code[2], code[3], code[3],
                    ]);
                    let previous = f32x8::new([
                        chips[0].re,
                        chips[0].im,
                        chips[1].re,
                        chips[1].im,
                        chips[2].re,
                        chips[2].im,
                        chips[3].re,
                        chips[3].im,
                    ]);
                    let result = (previous + values * weights).to_array();
                    for (chip, pair) in chips.iter_mut().zip(result.chunks_exact(2)) {
                        *chip = Complex32::new(pair[0], pair[1]);
                    }
                }
            }
        }
    }
    /// Code-major labels store physical symbol bit i in label bit i.
    /// Labels outside the modulation constellation are rejected before modifying output.
    pub fn add_slot_labels(
        &self,
        modulation: Modulation,
        labels: &[u8],
        subframe_start: ChipInstant,
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if labels.len() != self.codes.len() * SYMBOLS_PER_SUBFRAME
            || output.len() != CHIPS_PER_SLOT as usize
            || slot >= SUBFRAME_SLOTS
        {
            return Err(DspError::InvalidLength);
        }
        if !subframe_start.index.is_multiple_of(CHIPS_PER_SLOT) {
            return Err(DspError::DiscontinuousTime);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let first_chip = subframe_start
            .index
            .checked_add(slot as u64 * CHIPS_PER_SLOT)
            .ok_or(DspError::DiscontinuousTime)?;
        first_chip
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(DspError::DiscontinuousTime)?;
        if labels
            .iter()
            .any(|&label| usize::from(label) >= (1 << modulation.bits_per_symbol()))
        {
            return Err(DspError::InvalidCode);
        }
        let frame_offset = (first_chip % CHIPS_PER_FRAME) as usize;
        let phases = &self.phases[frame_offset..frame_offset + CHIPS_PER_SLOT as usize];
        if self.codes.len() < 8 {
            self.add_sparse_slot_labels(modulation, labels, slot, amplitude, output, phases);
            return Ok(());
        }
        let scale = amplitude * 0.5;
        let symbol_bits = modulation.bits_per_symbol();
        let mut constellation = [Complex32::default(); CONSTELLATION_SIZE];
        for (label, entry) in constellation.iter_mut().enumerate().take(1 << symbol_bits) {
            let bits: [Bit; MAX_SYMBOL_BITS] =
                std::array::from_fn(|bit| Bit::from(label & (1 << bit) != 0));
            *entry = map_symbol(modulation, &bits) * scale;
        }
        let mut rows = [0; MAX_CODES];
        for (code, row) in self.codes.iter().zip(&mut rows) {
            // Walsh row bits are the OVSF signs at power-of-two chip offsets.
            *row = (0..4).fold(0, |row, bit| {
                row | (usize::from(code[1 << bit] < 0.0) << bit)
            });
        }
        for (symbol, (chips, phases)) in output
            .chunks_exact_mut(SPREADING_FACTOR)
            .zip(phases.chunks_exact(SPREADING_FACTOR))
            .enumerate()
        {
            let mut values = [Complex32::default(); SPREADING_FACTOR];
            let first = slot * SYMBOLS_PER_SLOT + symbol;
            for (labels, &row) in labels.chunks_exact(SYMBOLS_PER_SUBFRAME).zip(&rows) {
                values[row] = constellation[usize::from(labels[first])];
            }
            for width in [1, 2, 4, 8] {
                for block in values.chunks_exact_mut(2 * width) {
                    let (left, right) = block.split_at_mut(width);
                    for (left, right) in left.iter_mut().zip(right) {
                        let first = *left;
                        *left = first + *right;
                        *right = first - *right;
                    }
                }
            }
            for ((chip, value), &phase) in chips.iter_mut().zip(values).zip(phases) {
                *chip += value * SCRAMBLING_VALUES[usize::from(phase)];
            }
        }
        Ok(())
    }
    fn add_sparse_slot_labels(
        &self,
        modulation: Modulation,
        labels: &[u8],
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
        phases: &[u8],
    ) {
        let scale = amplitude * 0.5;
        let symbol_bits = modulation.bits_per_symbol();
        let mut constellation = [[Complex32::default(); 4]; CONSTELLATION_SIZE];
        for (label, entry) in constellation.iter_mut().enumerate().take(1 << symbol_bits) {
            let bits: [Bit; MAX_SYMBOL_BITS] =
                std::array::from_fn(|bit| Bit::from(label & (1 << bit) != 0));
            let value = map_symbol(modulation, &bits) * scale;
            *entry = SCRAMBLING_VALUES.map(|weight| value * weight);
        }
        for (code, labels) in self
            .codes
            .iter()
            .zip(labels.chunks_exact(SYMBOLS_PER_SUBFRAME))
        {
            let first = slot * SYMBOLS_PER_SLOT;
            let end = first + SYMBOLS_PER_SLOT;
            for ((&label, chips), phases) in labels[first..end]
                .iter()
                .zip(output.chunks_exact_mut(SPREADING_FACTOR))
                .zip(phases.chunks_exact(SPREADING_FACTOR))
            {
                let scrambled = &constellation[usize::from(label)];
                for ((chips, phases), code) in chips
                    .chunks_exact_mut(4)
                    .zip(phases.chunks_exact(4))
                    .zip(code.chunks_exact(4))
                {
                    let values: [Complex32; 4] =
                        std::array::from_fn(|index| scrambled[usize::from(phases[index])]);
                    let values = f32x8::new([
                        values[0].re,
                        values[0].im,
                        values[1].re,
                        values[1].im,
                        values[2].re,
                        values[2].im,
                        values[3].re,
                        values[3].im,
                    ]);
                    let weights = f32x8::new([
                        code[0], code[0], code[1], code[1], code[2], code[2], code[3], code[3],
                    ]);
                    let previous = f32x8::new([
                        chips[0].re,
                        chips[0].im,
                        chips[1].re,
                        chips[1].im,
                        chips[2].re,
                        chips[2].im,
                        chips[3].re,
                        chips[3].im,
                    ]);
                    let result = (previous + values * weights).to_array();
                    for (chip, pair) in chips.iter_mut().zip(result.chunks_exact(2)) {
                        *chip = Complex32::new(pair[0], pair[1]);
                    }
                }
            }
        }
    }
}

fn sign(bit: Bit) -> f32 {
    if bit == Bit::Zero { 1.0 } else { -1.0 }
}

// TS 25.213 Tables 3B–3C normalize each quadrature branch to unit mean power.
fn map_symbol(modulation: Modulation, bits: &[Bit]) -> Complex32 {
    match modulation {
        Modulation::Qpsk => Complex32::new(sign(bits[0]), sign(bits[1])),
        Modulation::Qam16 => {
            let normalization = 1.0 / 5.0_f32.sqrt();
            Complex32::new(
                sign(bits[0]) * (1.0 + 2.0 * bits[2] as u8 as f32) * normalization,
                sign(bits[1]) * (1.0 + 2.0 * bits[3] as u8 as f32) * normalization,
            )
        }
        Modulation::Qam64 => {
            let normalization = 1.0 / 21.0_f32.sqrt();
            Complex32::new(
                sign(bits[0]) * (4.0 - sign(bits[2]) * (2.0 - sign(bits[4]))) * normalization,
                sign(bits[1]) * (4.0 - sign(bits[3]) * (2.0 - sign(bits[5]))) * normalization,
            )
        }
    }
}

const _: () = assert!(SYMBOLS_PER_SUBFRAME == SYMBOLS_PER_SLOT * SUBFRAME_SLOTS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_labels_preserve_bit_path_waveforms_for_all_codes_and_modulations() {
        let primary = PrimaryScramblingCode::new(469).unwrap();
        let pool = PdschSpreader::new(primary, 1, MAX_CODES).unwrap();
        for count in 1..=MAX_CODES {
            let view = pool.subset(MAX_CODES - count..MAX_CODES).unwrap();
            for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                let width = modulation.bits_per_symbol();
                let labels: Vec<u8> = (0..count * SYMBOLS_PER_SUBFRAME)
                    .map(|index| {
                        ((index + 17 * (index / SYMBOLS_PER_SUBFRAME)) % (1 << width)) as u8
                    })
                    .collect();
                let bits: Vec<_> = labels
                    .iter()
                    .flat_map(|&label| {
                        (0..width).map(move |bit| Bit::from(label & (1 << bit) != 0))
                    })
                    .collect();
                for amplitude in [0.0, 1e-12, 0.13, 1.0] {
                    for frame_slot in [0, 14, 29] {
                        let start = ChipInstant {
                            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                            index: frame_slot * CHIPS_PER_SLOT,
                        };
                        for slot in 0..SUBFRAME_SLOTS {
                            let mut expected =
                                vec![Complex32::new(0.031, -0.07); CHIPS_PER_SLOT as usize];
                            let mut actual = expected.clone();
                            view.add_slot(modulation, &bits, start, slot, amplitude, &mut expected)
                                .unwrap();
                            view.add_slot_labels(
                                modulation,
                                &labels,
                                start,
                                slot,
                                amplitude,
                                &mut actual,
                            )
                            .unwrap();
                            for (actual, expected) in actual.iter().zip(&expected) {
                                assert_eq!(actual.re.to_bits(), expected.re.to_bits());
                                assert_eq!(actual.im.to_bits(), expected.im.to_bits());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_labels_and_slot_parameters_leave_output_unchanged() {
        let pool =
            PdschSpreader::new(PrimaryScramblingCode::new(469).unwrap(), 1, MAX_CODES).unwrap();
        for count in [1, MAX_CODES] {
            let view = pool.subset(0..count).unwrap();
            for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                let mut labels = vec![0; count * SYMBOLS_PER_SUBFRAME];
                let start = ChipInstant {
                    epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                    index: 0,
                };
                let initial = vec![Complex32::new(0.031, -0.07); CHIPS_PER_SLOT as usize];
                let mut output = initial.clone();
                *labels.last_mut().unwrap() = 1 << modulation.bits_per_symbol();
                assert_eq!(
                    view.add_slot_labels(modulation, &labels, start, 0, 0.13, &mut output),
                    Err(DspError::InvalidCode)
                );
                assert_eq!(output, initial);
                *labels.last_mut().unwrap() = 0;
                assert_eq!(
                    view.add_slot_labels(
                        modulation,
                        &labels[..labels.len() - 1],
                        start,
                        0,
                        0.13,
                        &mut output
                    ),
                    Err(DspError::InvalidLength)
                );
                assert_eq!(output, initial);
                assert_eq!(
                    view.add_slot_labels(
                        modulation,
                        &labels,
                        start,
                        SUBFRAME_SLOTS,
                        0.13,
                        &mut output
                    ),
                    Err(DspError::InvalidLength)
                );
                assert_eq!(output, initial);
                assert_eq!(
                    view.add_slot_labels(modulation, &labels, start, 0, f32::NAN, &mut output),
                    Err(DspError::InvalidAmplitude)
                );
                assert_eq!(output, initial);
                for index in [1, u64::MAX - u64::MAX % CHIPS_PER_SLOT] {
                    assert_eq!(
                        view.add_slot_labels(
                            modulation,
                            &labels,
                            ChipInstant { index, ..start },
                            0,
                            0.13,
                            &mut output
                        ),
                        Err(DspError::DiscontinuousTime)
                    );
                    assert_eq!(output, initial);
                }
                assert_eq!(
                    view.add_slot_labels(
                        modulation,
                        &labels,
                        start,
                        0,
                        0.13,
                        &mut output[..CHIPS_PER_SLOT as usize - 1]
                    ),
                    Err(DspError::InvalidLength)
                );
                assert_eq!(output, initial);
            }
        }
    }

    #[test]
    fn qam64_table_three_c_mapping_and_average_power() {
        let origin = map_symbol(Modulation::Qam64, &[Bit::Zero; 6]);
        assert!((origin.re - 3.0 / 21.0_f32.sqrt()).abs() < 1e-7);
        let mut energy = 0.0;
        for label in 0..64 {
            let bits = std::array::from_fn::<_, 6, _>(|bit| Bit::from(label & (1 << bit) != 0));
            let value = map_symbol(Modulation::Qam64, &bits);
            energy += value.norm_sqr();
        }
        assert!((energy / 64.0 - 2.0).abs() < 1e-6);
    }

    fn reference_pdsch(
        spreader: &PdschSpreader,
        modulation: Modulation,
        bits: &[Bit],
        start: ChipInstant,
        slot: usize,
        amplitude: f32,
        output: &mut [Complex32],
    ) {
        let frame_offset = (start.index + slot as u64 * CHIPS_PER_SLOT) as usize;
        let width = modulation.bits_per_symbol();
        for (code, bits) in spreader
            .codes
            .iter()
            .zip(bits.chunks_exact(modulation.bits_per_code()))
        {
            for symbol in 0..SYMBOLS_PER_SLOT {
                let first = (slot * SYMBOLS_PER_SLOT + symbol) * width;
                let value = map_symbol(modulation, &bits[first..first + width]) * (amplitude * 0.5);
                for (offset, code) in code.iter().enumerate() {
                    let chip = symbol * SPREADING_FACTOR + offset;
                    output[chip] += value
                        * spreader.scrambling[(frame_offset + chip) % spreader.scrambling.len()]
                        * *code;
                }
            }
        }
    }

    fn assert_chips_close(actual: &[Complex32], expected: &[Complex32]) {
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual - expected).norm() <= 0.000005,
                "{actual:?} != {expected:?}"
            );
        }
    }

    #[test]
    fn ovsf_codes_are_distinct_walsh_rows() {
        let mut seen = [false; SPREADING_FACTOR];
        for index in 0..SPREADING_FACTOR {
            let mut code = [0.0; SPREADING_FACTOR];
            OvsfCode::new(SPREADING_FACTOR as u16, index as u16)
                .unwrap()
                .write(&mut code)
                .unwrap();
            let row = (0..4).fold(0_usize, |row, bit| {
                row | (usize::from(code[1 << bit] < 0.0) << bit)
            });
            assert!(!seen[row]);
            seen[row] = true;
            for (chip, &weight) in code.iter().enumerate() {
                let expected = if (row & chip).count_ones().is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                assert_eq!(weight, expected);
            }
        }
        assert!(seen.into_iter().all(|present| present));
    }

    #[test]
    fn code_subsets_preserve_all_constellation_labels_and_additive_output() {
        let primary = PrimaryScramblingCode::new(469).unwrap();
        let pool = PdschSpreader::new(primary, 1, 15).unwrap();
        for range in [0..1, 0..15, 3..8, 14..15, 3..15] {
            let reference =
                PdschSpreader::new(primary, 1 + range.start as u16, range.len()).unwrap();
            let view = pool.subset(range.clone()).unwrap();
            for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                let width = modulation.bits_per_symbol();
                let bits: Vec<_> = (0..range.len() * modulation.bits_per_code())
                    .map(|index| {
                        let label = index / width + 17 * (index / modulation.bits_per_code());
                        Bit::from(label & (1 << (index % width)) != 0)
                    })
                    .collect();
                for amplitude in [0.0, 0.13, 1.0] {
                    for slot in 0..SUBFRAME_SLOTS {
                        let start = ChipInstant {
                            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                            index: 29 * CHIPS_PER_SLOT,
                        };
                        let mut expected =
                            vec![Complex32::new(0.01, -0.02); CHIPS_PER_SLOT as usize];
                        let mut actual = expected.clone();
                        reference_pdsch(
                            &reference,
                            modulation,
                            &bits,
                            start,
                            slot,
                            amplitude,
                            &mut expected,
                        );
                        view.add_slot(modulation, &bits, start, slot, amplitude, &mut actual)
                            .unwrap();
                        assert_chips_close(&actual, &expected);
                    }
                }
            }
        }
    }

    #[test]
    fn slot_slices_match_scalar_scrambling_across_frame_wrap() {
        let primary = PrimaryScramblingCode::new(469).unwrap();
        for count in [1, 5, 7, 8, 15] {
            let spreader = PdschSpreader::new(primary, 1, count).unwrap();
            for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                let bits: Vec<_> = (0..count * modulation.bits_per_code())
                    .map(|index| Bit::from(index % 7 < 3))
                    .collect();
                for frame_slot in 0..15 {
                    let start = ChipInstant {
                        epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                        index: frame_slot * CHIPS_PER_SLOT,
                    };
                    for slot in 0..SUBFRAME_SLOTS {
                        let mut expected =
                            vec![Complex32::new(0.01, -0.02); CHIPS_PER_SLOT as usize];
                        let mut actual = expected.clone();
                        reference_pdsch(
                            &spreader,
                            modulation,
                            &bits,
                            start,
                            slot,
                            0.13,
                            &mut expected,
                        );
                        spreader
                            .add_slot(modulation, &bits, start, slot, 0.13, &mut actual)
                            .unwrap();
                        assert_chips_close(&actual, &expected);
                    }
                }
            }
        }
        let spreader = ScchSpreader::new(primary, 2).unwrap();
        let bits = std::array::from_fn(|index| Bit::from(index % 3 == 0));
        for frame_slot in 0..15 {
            let start = ChipInstant {
                epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                index: frame_slot * CHIPS_PER_SLOT,
            };
            for slot in 0..SUBFRAME_SLOTS {
                let mut actual = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
                let mut expected = actual.clone();
                for (chip, output) in expected.iter_mut().enumerate() {
                    let symbol = slot * SCCH_SLOT_BITS + (chip / SCCH_SPREADING_FACTOR) * 2;
                    let value = Complex32::new(sign(bits[symbol]), sign(bits[symbol + 1])) * 0.13;
                    let index = start.index as usize + slot * CHIPS_PER_SLOT as usize + chip;
                    *output += value * spreader.weights[index % spreader.weights.len()];
                }
                spreader
                    .add_slot(&bits, start, slot, 0.13, &mut actual)
                    .unwrap();
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn borrowed_code_subset_matches_independently_constructed_spreader() {
        let primary = PrimaryScramblingCode::new(469).unwrap();
        let pool = PdschSpreader::new(primary, 3, 10).unwrap();
        let selected = pool.subset(4..7).unwrap();
        let reference = PdschSpreader::new(primary, 7, 3).unwrap();
        let start = ChipInstant {
            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
            index: 14 * CHIPS_PER_SLOT,
        };
        for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
            let bits: Vec<_> = (0..3 * modulation.bits_per_code())
                .map(|index| Bit::from(index % 7 < 3))
                .collect();
            for slot in 0..SUBFRAME_SLOTS {
                let mut expected = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
                let mut actual = expected.clone();
                reference
                    .add_slot(modulation, &bits, start, slot, 0.1, &mut expected)
                    .unwrap();
                selected
                    .add_slot(modulation, &bits, start, slot, 0.1, &mut actual)
                    .unwrap();
                assert_eq!(actual, expected);
            }
        }
        assert!(pool.subset(0..0).is_err());
        assert!(pool.subset(9..11).is_err());
    }

    #[test]
    fn walsh_spreading_matches_f64_reference_for_every_label_and_amplitude() {
        let primary = PrimaryScramblingCode::new(469).unwrap();
        for count in [1, 7, 8, 12, 15] {
            let spreader = PdschSpreader::new(primary, 1, count).unwrap();
            for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
                let width = modulation.bits_per_symbol();
                for pattern in 0..3 {
                    let bits: Vec<_> = (0..count * modulation.bits_per_code())
                        .map(|index| {
                            let code = index / modulation.bits_per_code();
                            let peak = (1 << width) - 4;
                            let label = match pattern {
                                0 => index / width + 17 * code,
                                1 => peak,
                                _ => peak ^ (3 * (code % 2)),
                            };
                            Bit::from(label & (1 << (index % width)) != 0)
                        })
                        .collect();
                    for amplitude in [0.0, 1e-12, 0.13, 1.0] {
                        for slot in 0..SUBFRAME_SLOTS {
                            let start = ChipInstant {
                                epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::MIN),
                                index: 14 * CHIPS_PER_SLOT,
                            };
                            let mut expected =
                                vec![Complex32::new(0.031, -0.07); CHIPS_PER_SLOT as usize];
                            let mut actual = expected.clone();
                            reference_pdsch(
                                &spreader,
                                modulation,
                                &bits,
                                start,
                                slot,
                                amplitude,
                                &mut expected,
                            );
                            spreader
                                .add_slot(modulation, &bits, start, slot, amplitude, &mut actual)
                                .unwrap();
                            assert_chips_close(&actual, &expected);
                            let mut precise = vec![
                                num_complex::Complex64::new(
                                    f64::from(0.031_f32),
                                    f64::from(-0.07_f32)
                                );
                                CHIPS_PER_SLOT as usize
                            ];
                            for (code, bits) in spreader
                                .codes
                                .iter()
                                .zip(bits.chunks_exact(modulation.bits_per_code()))
                            {
                                for symbol in 0..SYMBOLS_PER_SLOT {
                                    let first = (slot * SYMBOLS_PER_SLOT + symbol) * width;
                                    let value = map_symbol(modulation, &bits[first..first + width])
                                        * (amplitude * 0.5);
                                    let value = num_complex::Complex64::new(
                                        f64::from(value.re),
                                        f64::from(value.im),
                                    );
                                    for (offset, &weight) in code.iter().enumerate() {
                                        let chip = symbol * SPREADING_FACTOR + offset;
                                        let phase = spreader.scrambling[(start.index as usize
                                            + slot * CHIPS_PER_SLOT as usize
                                            + chip)
                                            % spreader.scrambling.len()];
                                        precise[chip] += value
                                            * num_complex::Complex64::new(
                                                f64::from(phase.re),
                                                f64::from(phase.im),
                                            )
                                            * f64::from(weight);
                                    }
                                }
                            }
                            for (actual, expected) in actual.iter().zip(precise) {
                                let actual = num_complex::Complex64::new(
                                    f64::from(actual.re),
                                    f64::from(actual.im),
                                );
                                assert!(
                                    (actual - expected).norm() <= 0.000005,
                                    "{actual:?} != {expected:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn table_3b_labels_and_average_energy() {
        let axes = [1.0, 3.0, -1.0, -3.0];
        let mut energy = 0.0;
        for label in 0..16_u8 {
            let bits =
                std::array::from_fn::<_, 4, _>(|index| Bit::from(label & (1 << (3 - index)) != 0));
            let actual = map_symbol(Modulation::Qam16, &bits);
            let i = ((label >> 2) & 2 | (label >> 1) & 1) as usize;
            let q = ((label >> 1) & 2 | label & 1) as usize;
            let expected = Complex32::new(axes[i], axes[q]) / 5.0_f32.sqrt();
            assert!((actual - expected).norm() < 1e-6);
            energy += actual.norm_sqr();
        }
        assert!((energy / 16.0 - 2.0).abs() < 1e-6);
    }
}
