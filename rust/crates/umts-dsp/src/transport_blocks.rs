// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    code_blocks::{ConvolutionalBlocks, SegmentedConvolutionalDecoder},
    convolutional::CodeRate,
    crc::TransportCrc,
};
use std::num::NonZeroUsize;

const MAX_BLOCKS: usize = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CrcCounts {
    pub passed: usize,
    pub failed: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CrcBlockLayout {
    block_bits: usize,
    blocks: usize,
    crc: TransportCrc,
    pub(crate) payload: usize,
    pub(crate) joined: usize,
}

impl CrcBlockLayout {
    pub(crate) fn is_single_crc24(self) -> bool {
        self.blocks == 1 && matches!(self.crc, TransportCrc::Crc24)
    }

    pub(crate) fn new(
        block_bits: usize,
        blocks: usize,
        crc: TransportCrc,
    ) -> Result<Self, DspError> {
        if blocks > MAX_BLOCKS {
            return Err(DspError::InvalidLength);
        }
        let payload = block_bits.checked_mul(blocks).ok_or(DspError::Capacity)?;
        let joined = block_bits
            .checked_add(crc.width())
            .and_then(|bits| bits.checked_mul(blocks))
            .ok_or(DspError::Capacity)?;
        Ok(Self {
            block_bits,
            blocks,
            crc,
            payload,
            joined,
        })
    }

    pub(crate) fn attach(self, input: &[Bit], joined: &mut [Bit]) -> Result<(), DspError> {
        if input.len() != self.payload || joined.len() != self.joined {
            return Err(DspError::InvalidLength);
        }
        let width = self.block_bits + self.crc.width();
        for index in 0..self.blocks {
            let input = &input[index * self.block_bits..(index + 1) * self.block_bits];
            let block = &mut joined[index * width..(index + 1) * width];
            block[..self.block_bits].copy_from_slice(input);
            self.crc.write(input, &mut block[self.block_bits..])?;
        }
        Ok(())
    }

    pub(crate) fn publish(
        self,
        joined: &[Bit],
        output: &mut [Bit],
        counts: &mut CrcCounts,
    ) -> Result<BlockOutcome, DspError> {
        if joined.len() != self.joined || output.len() != self.payload {
            return Err(DspError::InvalidLength);
        }
        *counts = CrcCounts::default();
        if self.blocks == 0 {
            return Ok(BlockOutcome::Absent);
        }
        let width = self.block_bits + self.crc.width();
        let mut checked = CrcCounts::default();
        let mut first_failed = None;
        for index in 0..self.blocks {
            let block = &joined[index * width..(index + 1) * width];
            if !self
                .crc
                .verify(&block[..self.block_bits], &block[self.block_bits..])?
            {
                checked.failed += 1;
                first_failed.get_or_insert(index);
            } else if self.crc.width() != 0 {
                checked.passed += 1;
            }
        }
        *counts = checked;
        if let Some(block) = first_failed {
            return Ok(BlockOutcome::InvalidCrc { block });
        }
        // Publishing is atomic across transport blocks, not just individually CRC-valid blocks.
        for index in 0..self.blocks {
            let output = &mut output[index * self.block_bits..(index + 1) * self.block_bits];
            let block = &joined[index * width..(index + 1) * width];
            output.copy_from_slice(&block[..self.block_bits]);
        }
        Ok(BlockOutcome::Decoded)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BlockFormat {
    pub block_bits: usize,
    pub blocks: usize,
    pub crc: TransportCrc,
    pub rate: CodeRate,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockPlan {
    layout: CrcBlockLayout,
    segmentation: ConvolutionalBlocks,
}
impl BlockPlan {
    pub fn new(format: BlockFormat, maximum_coded_bits: NonZeroUsize) -> Result<Self, DspError> {
        let layout = CrcBlockLayout::new(format.block_bits, format.blocks, format.crc)?;
        let segmentation = ConvolutionalBlocks::new(layout.joined, format.rate)?;
        if segmentation.coded_bits() > maximum_coded_bits.get() {
            return Err(DspError::Capacity);
        }
        Ok(Self {
            layout,
            segmentation,
        })
    }
    pub fn payload_bits(self) -> usize {
        self.layout.payload
    }
    pub fn coded_bits(self) -> usize {
        self.segmentation.coded_bits()
    }
    pub(crate) fn block_width(self) -> usize {
        self.segmentation.block_bits()
    }
    pub(crate) fn code_rate(self) -> CodeRate {
        self.segmentation.rate()
    }
}

pub struct BlockEncoder {
    plan: BlockPlan,
    joined: Vec<Bit>,
    scratch: Vec<Bit>,
}
impl BlockEncoder {
    pub fn new(plan: BlockPlan) -> Self {
        Self {
            plan,
            joined: vec![Bit::Zero; plan.layout.joined],
            scratch: vec![Bit::Zero; plan.segmentation.block_bits()],
        }
    }
    pub fn encode(&mut self, input: &[Bit], output: &mut [Bit]) -> Result<(), DspError> {
        if input.len() != self.plan.payload_bits() || output.len() != self.plan.coded_bits() {
            return Err(DspError::InvalidLength);
        }
        self.plan.layout.attach(input, &mut self.joined)?;
        self.plan
            .segmentation
            .encode(&self.joined, output, &mut self.scratch)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockOutcome {
    Absent,
    Erased,
    InvalidFiller,
    InvalidCrc { block: usize },
    Decoded,
}

pub struct BlockDecoder {
    plan: BlockPlan,
    decoder: SegmentedConvolutionalDecoder,
    joined: Vec<Bit>,
    crc_counts: CrcCounts,
}
impl BlockDecoder {
    pub(crate) fn crc_counts(&self) -> CrcCounts {
        self.crc_counts
    }
    pub(crate) fn cancel(&mut self) {
        self.decoder.cancel();
    }
    pub fn new(plan: BlockPlan) -> Self {
        Self {
            plan,
            decoder: SegmentedConvolutionalDecoder::new(plan.segmentation),
            joined: vec![Bit::Zero; plan.layout.joined],
            crc_counts: CrcCounts::default(),
        }
    }
    pub(crate) fn reserve_for(&mut self, plans: &[BlockPlan]) -> Result<(), DspError> {
        let segmentation: Vec<_> = plans.iter().map(|plan| plan.segmentation).collect();
        self.decoder.reserve_for(&segmentation)?;
        let maximum = plans
            .iter()
            .map(|plan| plan.layout.joined)
            .max()
            .unwrap_or(self.joined.len());
        self.joined
            .resize(maximum.max(self.joined.len()), Bit::Zero);
        Ok(())
    }
    pub(crate) fn configure(&mut self, plan: BlockPlan) -> Result<(), DspError> {
        if plan.layout.joined > self.joined.len() {
            return Err(DspError::Capacity);
        }
        self.decoder.configure(plan.segmentation)?;
        self.plan = plan;
        Ok(())
    }
    pub fn decode(&mut self, input: &[f32], output: &mut [Bit]) -> Result<BlockOutcome, DspError> {
        match self.begin(input, output)? {
            crate::coding::DecodeProgress::Complete(outcome) => Ok(outcome),
            crate::coding::DecodeProgress::Pending => {
                let mut work = crate::coding::DecodeWork::new(NonZeroUsize::MAX);
                match self.step(input, output, &mut work)? {
                    crate::coding::DecodeProgress::Complete(outcome) => Ok(outcome),
                    crate::coding::DecodeProgress::Pending => Err(DspError::Capacity),
                }
            }
        }
    }
    pub(crate) fn begin(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
    ) -> Result<crate::coding::DecodeProgress, DspError> {
        if self.decoder.active() {
            return Err(DspError::DiscontinuousTime);
        }
        self.crc_counts = CrcCounts::default();
        if input.len() != self.plan.coded_bits() || output.len() != self.plan.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        if self.plan.layout.blocks == 0 {
            return Ok(crate::coding::DecodeProgress::Complete(
                BlockOutcome::Absent,
            ));
        }
        if self.plan.coded_bits() == 0 {
            return self
                .plan
                .layout
                .publish(
                    &self.joined[..self.plan.layout.joined],
                    output,
                    &mut self.crc_counts,
                )
                .map(crate::coding::DecodeProgress::Complete);
        }
        if input.iter().all(|value| *value == 0.0) {
            return Ok(crate::coding::DecodeProgress::Complete(
                BlockOutcome::Erased,
            ));
        }
        self.decoder.begin(input)?;
        Ok(crate::coding::DecodeProgress::Pending)
    }
    pub(crate) fn step(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
        work: &mut crate::coding::DecodeWork,
    ) -> Result<crate::coding::DecodeProgress, DspError> {
        if output.len() != self.plan.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        match self
            .decoder
            .step(input, &mut self.joined[..self.plan.layout.joined], work)?
        {
            None => return Ok(crate::coding::DecodeProgress::Pending),
            Some(false) => {
                return Ok(crate::coding::DecodeProgress::Complete(
                    BlockOutcome::InvalidFiller,
                ));
            }
            Some(true) => (),
        }
        if !self.decoder.unambiguous() {
            return Ok(crate::coding::DecodeProgress::Complete(
                BlockOutcome::Erased,
            ));
        }
        self.plan
            .layout
            .publish(
                &self.joined[..self.plan.layout.joined],
                output,
                &mut self.crc_counts,
            )
            .map(crate::coding::DecodeProgress::Complete)
    }
}

#[cfg(test)]
mod tests;
