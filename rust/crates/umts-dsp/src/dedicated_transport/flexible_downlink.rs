// SPDX-License-Identifier: AGPL-3.0-or-later

use super::TransportPlanLimits;
use crate::{
    DspError,
    bits::Bit,
    coding::{CodedBlockEncoder, CodedBlockPlan},
    dedicated::DownlinkFormat,
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
};
use std::sync::Arc;

pub struct FlexibleDownlinkPlan {
    blocks: CodedBlockPlan,
    matching: RateMatching,
    first: Interleaver,
    output_bits: usize,
    frame_bits: usize,
}
pub struct FlexibleDownlink {
    plan: Arc<FlexibleDownlinkPlan>,
    encoder: CodedBlockEncoder,
    coded: Vec<Bit>,
    matched: Vec<Bit>,
}
impl FlexibleDownlinkPlan {
    /// The matched TTI length must be compiled from the entire TFCS, not chosen from current free space.
    pub fn new(
        plan: CodedBlockPlan,
        interval: Rel99Tti,
        output_bits: usize,
        physical: DownlinkFormat,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        if !output_bits.is_multiple_of(interval.frames())
            || matches!(interval, Rel99Tti::Ms80) && physical.spreading_factor() != 512
        {
            return Err(DspError::InvalidCode);
        }
        if plan.coded_bits() > limits.maximum_coded_bits.get()
            || output_bits
                .checked_mul(2)
                .is_none_or(|count| count > limits.maximum_mapping_entries.get())
        {
            return Err(DspError::Capacity);
        }
        let matching = plan.matching_flexible(output_bits, limits.maximum_mapping_entries)?;
        Ok(Self {
            blocks: plan,
            matching,
            first: Interleaver::first(interval, output_bits)?,
            output_bits,
            frame_bits: output_bits / interval.frames(),
        })
    }
}
impl FlexibleDownlink {
    pub fn new(
        plan: CodedBlockPlan,
        interval: Rel99Tti,
        output_bits: usize,
        physical: DownlinkFormat,
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        Self::from_plan(Arc::new(FlexibleDownlinkPlan::new(
            plan,
            interval,
            output_bits,
            physical,
            limits,
        )?))
    }
    pub fn from_plan(plan: Arc<FlexibleDownlinkPlan>) -> Result<Self, DspError> {
        Ok(Self {
            encoder: CodedBlockEncoder::new(plan.blocks)?,
            coded: vec![Bit::Zero; plan.blocks.coded_bits()],
            matched: vec![Bit::Zero; plan.output_bits],
            plan,
        })
    }
    pub fn frame_bits(&self) -> usize {
        self.plan.frame_bits
    }
    pub fn tti_bits(&self) -> usize {
        self.matched.len()
    }
    pub fn encode(&mut self, payload: &[Bit], frames: &mut [Bit]) -> Result<(), DspError> {
        if frames.len() != self.matched.len() {
            return Err(DspError::InvalidLength);
        }
        self.encoder.encode(payload, &mut self.coded)?;
        self.plan.matching.apply(&self.coded, &mut self.matched)?;
        self.plan.first.interleave(&self.matched, frames)
    }
}
