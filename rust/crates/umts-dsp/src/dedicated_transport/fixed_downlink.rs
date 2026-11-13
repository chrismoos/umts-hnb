// SPDX-License-Identifier: AGPL-3.0-or-later

use super::TransportPlanLimits;
use crate::{
    DspError,
    bits::{Bit, TxBit},
    coding::{CodedBlockEncoder, CodedBlockFormat, CodedBlockPlan},
    dedicated::DownlinkFormat,
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct FixedDownlinkSettings<F = CodedBlockFormat> {
    pub blocks: F,
    pub interval: Rel99Tti,
    pub maximum_coded_tti_bits: usize,
    pub reserved_frame_bits: usize,
    pub physical: DownlinkFormat,
}

pub struct FixedDownlinkPlan {
    blocks: CodedBlockPlan,
    matching: RateMatching,
    first: Interleaver,
    tti_bits: usize,
    frame_bits: usize,
}
pub struct FixedDownlink {
    plan: Arc<FixedDownlinkPlan>,
    encoder: CodedBlockEncoder,
    coded: Vec<Bit>,
    matched: Vec<Bit>,
    padded: Vec<TxBit>,
}
impl FixedDownlinkPlan {
    pub fn matched_tti_bits(&self) -> usize {
        self.matching.output_bits()
    }
    pub fn new(
        settings: FixedDownlinkSettings<impl Into<CodedBlockFormat>>,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        // TS 25.212 4.2 limits 80 ms DCH to SF512. Fixed-position DTX precedes first interleaving in 4.2.9.1.
        if matches!(settings.interval, Rel99Tti::Ms80)
            && settings.physical.spreading_factor() != 512
        {
            return Err(DspError::InvalidCode);
        }
        let total = settings
            .reserved_frame_bits
            .checked_mul(settings.interval.frames())
            .filter(|total| {
                total
                    .checked_mul(2)
                    .is_some_and(|entries| entries <= limits.maximum_mapping_entries.get())
            })
            .ok_or(DspError::Capacity)?;
        if settings.maximum_coded_tti_bits > limits.maximum_coded_bits.get() {
            return Err(DspError::Capacity);
        }
        let plan = CodedBlockPlan::new(settings.blocks.into(), limits.maximum_coded_bits)?;
        let maximum = settings.maximum_coded_tti_bits;
        let delta =
            i64::try_from(total as i128 - maximum as i128).map_err(|_| DspError::Capacity)?;
        let matching = plan.matching_fixed(maximum, delta, limits.maximum_mapping_entries)?;
        if matching.output_bits() > total {
            return Err(DspError::InvalidRateMatching);
        }
        Ok(Self {
            blocks: plan,
            matching,
            first: Interleaver::first(settings.interval, total)?,
            tti_bits: total,
            frame_bits: settings.reserved_frame_bits,
        })
    }
}
impl FixedDownlink {
    pub fn new(
        settings: FixedDownlinkSettings<impl Into<CodedBlockFormat>>,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        Self::from_plan(Arc::new(FixedDownlinkPlan::new(settings, limits)?))
    }
    pub fn from_plan(plan: Arc<FixedDownlinkPlan>) -> Result<Self, DspError> {
        Ok(Self {
            encoder: CodedBlockEncoder::new(plan.blocks)?,
            coded: vec![Bit::Zero; plan.blocks.coded_bits()],
            matched: vec![Bit::Zero; plan.matching.output_bits()],
            padded: vec![TxBit::Dtx; plan.tti_bits],
            plan,
        })
    }
    pub fn frame_bits(&self) -> usize {
        self.plan.frame_bits
    }
    pub fn tti_bits(&self) -> usize {
        self.padded.len()
    }
    pub fn encode(&mut self, payload: &[Bit], frames: &mut [TxBit]) -> Result<(), DspError> {
        if frames.len() != self.padded.len() {
            return Err(DspError::InvalidLength);
        }
        self.encoder.encode(payload, &mut self.coded)?;
        self.plan.matching.apply(&self.coded, &mut self.matched)?;
        for (output, bit) in self.padded.iter_mut().zip(&self.matched) {
            *output = TxBit::Data(*bit);
        }
        self.padded[self.matched.len()..].fill(TxBit::Dtx);
        self.plan.first.interleave(&self.padded, frames)
    }
}
