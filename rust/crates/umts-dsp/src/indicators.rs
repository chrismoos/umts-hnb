// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant, SYSTEM_FRAME_MODULUS};

use crate::{
    DspError,
    bits::{Bit, TxBit},
    channel_codes::OvsfCode,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
};

const INDICATOR_SF: usize = 256;
pub const ACCESS_SLOT_CHIPS: usize = 5120;
pub const ACQUISITION_ACTIVE_CHIPS: usize = 4096;
pub const ACQUISITION_SIGNATURES: usize = 16;
const ACQUISITION_SIGNALS: usize = 32;
pub const PAGING_FRAME_BITS: usize = 300;
pub const PAGING_ACTIVE_BITS: usize = 288;
const PAGING_GAP_CHIPS: u64 = 7680;
pub const PAGING_LEAD_CHIPS: u64 = CHIPS_PER_FRAME + PAGING_GAP_CHIPS;
const PAGING_ROTATION_BASE: usize = 144;
const PAGING_ROTATION_MULTIPLIER: usize = 18;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i8)]
pub enum Acquisition {
    Negative = -1,
    #[default]
    None = 0,
    Positive = 1,
}

pub struct AcquisitionIndicators {
    available: u16,
    values: [Acquisition; ACQUISITION_SIGNATURES],
}

impl AcquisitionIndicators {
    pub fn new(available: u16) -> Result<Self, DspError> {
        if available == 0 {
            return Err(DspError::InvalidCode);
        }
        Ok(Self {
            available,
            values: [Acquisition::None; ACQUISITION_SIGNATURES],
        })
    }

    pub fn set(&mut self, signature: u8, value: Acquisition) -> Result<(), DspError> {
        if usize::from(signature) >= ACQUISITION_SIGNATURES
            || self.available & (1_u16 << signature) == 0
        {
            return Err(DspError::InvalidCode);
        }
        self.values[usize::from(signature)] = value;
        Ok(())
    }

    // TS 25.211 Table 22 repeats each Walsh element on both QPSK branches.
    pub fn signals(&self) -> [i16; ACQUISITION_SIGNALS] {
        let symbols: [i16; ACQUISITION_SIGNALS / 2] = std::array::from_fn(|symbol| {
            self.values
                .iter()
                .enumerate()
                .map(|(signature, value)| {
                    let polarity = if (signature & symbol).count_ones().is_multiple_of(2) {
                        1
                    } else {
                        -1
                    };
                    *value as i16 * polarity
                })
                .sum()
        });
        std::array::from_fn(|signal| symbols[signal / 2])
    }
}

struct IndicatorSpreading {
    weights: Vec<Complex32>,
}

impl IndicatorSpreading {
    fn new(primary: PrimaryScramblingCode, code256: u8, amplitude: f32) -> Result<Self, DspError> {
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let mut weights = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(primary.sequence_number(), &mut weights)?;
        let mut code = [0.0; INDICATOR_SF];
        OvsfCode::new(INDICATOR_SF as u16, u16::from(code256))?.write(&mut code)?;
        for (index, weight) in weights.iter_mut().enumerate() {
            *weight *= code[index % INDICATOR_SF] * amplitude * 0.5;
        }
        Ok(Self { weights })
    }

    fn add_symbol(&self, origin: usize, offset: usize, value: Complex32, output: &mut [Complex32]) {
        // The 256-chip alignment keeps each symbol within one scrambling-code frame.
        let start = (origin + offset) % self.weights.len();
        for (chip, weight) in output
            .iter_mut()
            .zip(&self.weights[start..start + INDICATOR_SF])
        {
            *chip += value * weight;
        }
    }
}

pub struct AcquisitionDownlink {
    spreading: IndicatorSpreading,
}

#[cfg(test)]
mod tests;

impl AcquisitionDownlink {
    // Amplitude is per-signature active-chip RMS. Simultaneous indicators add without normalization or clipping.
    pub fn new(
        primary: PrimaryScramblingCode,
        code256: u8,
        amplitude: f32,
    ) -> Result<Self, DspError> {
        Ok(Self {
            spreading: IndicatorSpreading::new(primary, code256, amplitude)?,
        })
    }

    // TS 25.211 5.3.3.7 and 7.1 align AICH to a 5120-chip grid at the even-SFN origin.
    pub fn add_access_slot(
        &self,
        start: ChipInstant,
        indicators: &AcquisitionIndicators,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if output.len() != ACCESS_SLOT_CHIPS {
            return Err(DspError::InvalidLength);
        }
        if !start.index.is_multiple_of(ACCESS_SLOT_CHIPS as u64) {
            return Err(DspError::DiscontinuousTime);
        }
        start
            .index
            .checked_add(ACCESS_SLOT_CHIPS as u64)
            .ok_or(DspError::InvalidLength)?;
        let signals = indicators.signals();
        let origin = (start.index % CHIPS_PER_FRAME) as usize;
        for (symbol, pair) in signals.chunks_exact(2).enumerate() {
            let offset = symbol * INDICATOR_SF;
            self.spreading.add_symbol(
                origin,
                offset,
                Complex32::new(f32::from(pair[0]), f32::from(pair[1])),
                &mut output[offset..offset + INDICATOR_SF],
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagingCount {
    PerFrame18,
    PerFrame36,
    PerFrame72,
    PerFrame144,
}

impl PagingCount {
    pub fn get(self) -> usize {
        match self {
            Self::PerFrame18 => 18,
            Self::PerFrame36 => 36,
            Self::PerFrame72 => 72,
            Self::PerFrame144 => 144,
        }
    }

    // TS 25.211 5.3.3.10 associates higher-layer PI with q using the PICH-start SFN.
    pub fn position(self, indicator: usize, system_frame: u16) -> Result<usize, DspError> {
        let count = self.get();
        if indicator >= count || u64::from(system_frame) >= SYSTEM_FRAME_MODULUS {
            return Err(DspError::InvalidCode);
        }
        let sfn = usize::from(system_frame);
        let rotating = (PAGING_ROTATION_MULTIPLIER * (sfn + sfn / 8 + sfn / 64 + sfn / 512))
            % PAGING_ROTATION_BASE;
        Ok((indicator + rotating * count / PAGING_ROTATION_BASE) % count)
    }

    pub fn encode(
        self,
        system_frame: u16,
        indicators: &[bool],
        output: &mut [TxBit; PAGING_FRAME_BITS],
    ) -> Result<(), DspError> {
        if indicators.len() != self.get() || u64::from(system_frame) >= SYSTEM_FRAME_MODULUS {
            return Err(DspError::InvalidLength);
        }
        output.fill(TxBit::Dtx);
        let repeats = PAGING_ACTIVE_BITS / self.get();
        for (indicator, value) in indicators.iter().enumerate() {
            let start = self.position(indicator, system_frame)? * repeats;
            output[start..start + repeats].fill(TxBit::Data(Bit::from(*value)));
        }
        Ok(())
    }
}

// TS 25.211 7.2 Figure 30 places a complete PICH frame before the 7680-chip gap to PCH.
pub fn paging_start(pch_start: ChipInstant) -> Result<ChipInstant, DspError> {
    if !pch_start.index.is_multiple_of(INDICATOR_SF as u64) {
        return Err(DspError::DiscontinuousTime);
    }
    Ok(ChipInstant {
        epoch: pch_start.epoch,
        index: pch_start
            .index
            .checked_sub(PAGING_LEAD_CHIPS)
            .ok_or(DspError::DiscontinuousTime)?,
    })
}

pub struct PagingDownlink {
    spreading: IndicatorSpreading,
    count: PagingCount,
}

impl PagingDownlink {
    pub fn new(
        primary: PrimaryScramblingCode,
        code256: u8,
        count: PagingCount,
        amplitude: f32,
    ) -> Result<Self, DspError> {
        Ok(Self {
            spreading: IndicatorSpreading::new(primary, code256, amplitude)?,
            count,
        })
    }

    // Scrambling stays aligned to P-CCPCH, not the shifted PICH frame origin.
    pub fn add_frame(
        &self,
        start: ChipInstant,
        indicators: &[bool],
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if output.len() != CHIPS_PER_FRAME as usize {
            return Err(DspError::InvalidLength);
        }
        if !start.index.is_multiple_of(INDICATOR_SF as u64) {
            return Err(DspError::DiscontinuousTime);
        }
        start
            .index
            .checked_add(CHIPS_PER_FRAME)
            .ok_or(DspError::InvalidLength)?;
        let mut bits = [TxBit::Dtx; PAGING_FRAME_BITS];
        self.count
            .encode(start.system_frame(), indicators, &mut bits)?;
        let origin = (start.index % CHIPS_PER_FRAME) as usize;
        for (symbol, pair) in bits[..PAGING_ACTIVE_BITS].chunks_exact(2).enumerate() {
            let offset = symbol * INDICATOR_SF;
            self.spreading.add_symbol(
                origin,
                offset,
                Complex32::new(pair[0].amplitude(), pair[1].amplitude()),
                &mut output[offset..offset + INDICATOR_SF],
            );
        }
        Ok(())
    }
}
