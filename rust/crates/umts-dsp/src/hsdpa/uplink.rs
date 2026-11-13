// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, channel_codes::OvsfCode, dedicated::UplinkDpchCode};
use num_complex::{Complex32, Complex64};
use std::sync::Arc;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant};

const SPREADING: usize = 256;
const SYMBOLS: usize = 10;
const FEEDBACK_OFFSET: u64 = 101 * SPREADING as u64;

#[cfg(test)]
mod tests;

// TS 25.211 7.7 relates corresponding DL/UL DPCH frames at the UE.
// Propagation and receiver calibration belong to the caller's air-time mapping.
pub fn feedback_start(
    pdsch_start: ChipInstant,
    downlink_frame: ChipInstant,
    uplink_frame: ChipInstant,
) -> Result<ChipInstant, DspError> {
    if pdsch_start.epoch != downlink_frame.epoch || pdsch_start.epoch != uplink_frame.epoch {
        return Err(DspError::DiscontinuousTime);
    }
    let difference = pdsch_start
        .index
        .checked_sub(downlink_frame.index)
        .filter(|&value| value < CHIPS_PER_FRAME && value.is_multiple_of(SPREADING as u64))
        .ok_or(DspError::DiscontinuousTime)?;
    let index = uplink_frame
        .index
        .checked_add(difference)
        .and_then(|value| value.checked_add(FEEDBACK_OFFSET))
        .ok_or(DspError::DiscontinuousTime)?;
    Ok(ChipInstant {
        epoch: pdsch_start.epoch,
        index,
    })
}

pub struct FeedbackChannel {
    scrambling: Arc<UplinkDpchCode>,
    code: [f32; SPREADING],
    quadrature: bool,
    uplink_origin: ChipInstant,
}

impl FeedbackChannel {
    pub fn shares_timeline(&self, other: &Self) -> bool {
        self.uplink_origin == other.uplink_origin
            && Arc::ptr_eq(&self.scrambling, &other.scrambling)
    }

    pub fn despread_tracked_slot(
        &self,
        slot: &crate::dedicated::UplinkSlot<'_>,
    ) -> Result<[Complex64; SYMBOLS], DspError> {
        let start = slot.start();
        if !Arc::ptr_eq(slot.code(), &self.scrambling)
            || start.epoch != self.uplink_origin.epoch
            || start
                .index
                .checked_sub(self.uplink_origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(CHIPS_PER_SLOT))
        {
            return Err(DspError::DiscontinuousTime);
        }
        let mut output = [Complex64::default(); SYMBOLS];
        for (value, chips) in output
            .iter_mut()
            .zip(slot.corrected_samples().chunks_exact(SPREADING))
        {
            *value = chips
                .iter()
                .zip(self.code)
                .map(|(chip, code)| *chip * f64::from(code))
                .sum::<Complex64>()
                / SPREADING as f64;
            if self.quadrature {
                *value *= Complex64::new(0.0, -1.0);
            }
        }
        Ok(output)
    }

    pub fn new(
        scrambling: Arc<UplinkDpchCode>,
        maximum_dpdch: u8,
        uplink_origin: ChipInstant,
    ) -> Result<Self, DspError> {
        // TS 25.213 Table 1D and 4.2.1.2 depend on configured Nmax, not current activity.
        let (index, quadrature) = match maximum_dpdch {
            0 => (33, true),
            1 => (64, true),
            2 | 4 | 6 => (1, false),
            3 | 5 => (32, true),
            _ => return Err(DspError::InvalidCode),
        };
        let mut code = [0.0; SPREADING];
        OvsfCode::new(SPREADING as u16, index)?.write(&mut code)?;
        Ok(Self {
            scrambling,
            code,
            quadrature,
            uplink_origin,
        })
    }

    // Input is matched-filtered and carrier-corrected at one sample per chip.
    // Pilot channels must already be interpolated to these symbol times.
    pub fn despread_slot(
        &self,
        start: ChipInstant,
        input: &[Complex32],
        pilot_channels: &[Complex64; SYMBOLS],
    ) -> Result<[f32; SYMBOLS], DspError> {
        if start.epoch != self.uplink_origin.epoch {
            return Err(DspError::DiscontinuousTime);
        }
        let elapsed = start
            .index
            .checked_sub(self.uplink_origin.index)
            .ok_or(DspError::DiscontinuousTime)?;
        if !elapsed.is_multiple_of(SPREADING as u64) {
            return Err(DspError::DiscontinuousTime);
        }
        if input.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        if input
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
            || pilot_channels
                .iter()
                .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(DspError::NonFiniteEvidence);
        }
        let offset = (elapsed % CHIPS_PER_FRAME) as usize;
        let scrambling = self.scrambling.scrambling();
        let mut output = [0.0; SYMBOLS];
        for (symbol, chips) in input.chunks_exact(SPREADING).enumerate() {
            let mut value = Complex64::default();
            for (index, chip) in chips.iter().enumerate() {
                let sequence = scrambling[(offset + symbol * SPREADING + index) % scrambling.len()];
                value += Complex64::new(chip.re.into(), chip.im.into())
                    * Complex64::new(sequence.re.into(), sequence.im.into()).conj()
                    * f64::from(self.code[index]);
            }
            value /= 2.0 * SPREADING as f64;
            if self.quadrature {
                value *= Complex64::new(0.0, -1.0);
            }
            let soft = (value * pilot_channels[symbol].conj()).re;
            if !soft.is_finite() || soft.abs() > f64::from(f32::MAX) {
                return Err(DspError::NonFiniteEvidence);
            }
            output[symbol] = soft as f32;
        }
        Ok(output)
    }
}
