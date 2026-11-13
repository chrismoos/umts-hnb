// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{MAX_BLOCK_BITS, MIN_BLOCK_BITS, TurboDecoder, TurboInterleaver, coded_bits, encode};
use crate::{
    DspError,
    bits::Bit,
    crc::TransportCrc,
    transport_blocks::{BlockOutcome, CrcBlockLayout},
};
use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
};

mod early;
pub(crate) use early::Crc24TurboDecoder;

#[derive(Clone, Copy, Debug)]
pub struct TurboBlockFormat {
    pub block_bits: usize,
    pub blocks: usize,
    pub crc: TransportCrc,
}

#[derive(Clone, Copy, Debug)]
pub struct TurboBlockPlan {
    layout: CrcBlockLayout,
    count: usize,
    width: usize,
    filler: usize,
    coded: usize,
}

impl TurboBlockPlan {
    pub fn new(
        format: TurboBlockFormat,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        let layout = CrcBlockLayout::new(format.block_bits, format.blocks, format.crc)?;
        let count = layout.joined.div_ceil(MAX_BLOCK_BITS);
        let width = if count == 0 {
            0
        } else {
            layout.joined.div_ceil(count).max(MIN_BLOCK_BITS)
        };
        let filler = count.checked_mul(width).ok_or(DspError::Capacity)? - layout.joined;
        let coded = if count == 0 {
            0
        } else {
            coded_bits(width)?
                .checked_mul(count)
                .ok_or(DspError::Capacity)?
        };
        if coded > maximum_coded_bits.get() {
            return Err(DspError::Capacity);
        }
        Ok(Self {
            layout,
            count,
            width,
            filler,
            coded,
        })
    }
    pub fn payload_bits(self) -> usize {
        self.layout.payload
    }
    pub fn coded_bits(self) -> usize {
        self.coded
    }
    pub fn code_block_count(self) -> usize {
        self.count
    }
    pub fn code_block_bits(self) -> usize {
        self.width
    }
    pub fn filler_bits(self) -> usize {
        self.filler
    }

    // Matches the work charges in incremental::step, including tails and output.
    pub fn decode_state_updates(self, iterations: NonZeroU8) -> Result<usize, DspError> {
        let constituent = self
            .width
            .checked_add(super::TAIL_STEPS)
            .and_then(|steps| steps.checked_mul(2 * super::STATES))
            .and_then(|passes| passes.checked_add(self.width))
            .ok_or(DspError::Capacity)?;
        constituent
            .checked_mul(2)
            .and_then(|pair| pair.checked_mul(usize::from(iterations.get())))
            .and_then(|iterations| iterations.checked_add(self.width))
            .and_then(|block| block.checked_mul(self.count))
            .ok_or(DspError::Capacity)
    }

    pub(crate) fn crc_round_state_updates(self, iterations: NonZeroU8) -> Result<usize, DspError> {
        let output = self
            .width
            .checked_mul(self.count)
            .ok_or(DspError::Capacity)?;
        self.decode_state_updates(iterations)?
            .checked_add(output)
            .ok_or(DspError::Capacity)?
            .checked_add(
                output
                    .checked_mul(usize::from(iterations.get() - 1))
                    .ok_or(DspError::Capacity)?,
            )
            .and_then(|work| {
                self.layout
                    .joined
                    .checked_mul(usize::from(iterations.get()))
                    .and_then(|crc| work.checked_add(crc))
            })
            .ok_or(DspError::Capacity)
    }

    pub(crate) fn interleaver(self) -> Result<Option<Arc<TurboInterleaver>>, DspError> {
        if self.count == 0 {
            Ok(None)
        } else {
            TurboInterleaver::new(self.width).map(Arc::new).map(Some)
        }
    }
}

pub struct TurboBlockEncoder {
    plan: TurboBlockPlan,
    interleaver: Option<Arc<TurboInterleaver>>,
    joined: Vec<Bit>,
    block: Vec<Bit>,
}

impl TurboBlockEncoder {
    /// Requested retained storage, including an exclusively owned interleaver and its Arc counters.
    pub fn required_storage_bytes(plan: TurboBlockPlan) -> Result<usize, DspError> {
        let scratch = plan
            .layout
            .joined
            .checked_add(plan.width)
            .and_then(|bits| bits.checked_mul(std::mem::size_of::<Bit>()))
            .ok_or(DspError::Capacity)?;
        let interleaver = if plan.count == 0 {
            0
        } else {
            plan.width
                .checked_mul(std::mem::size_of::<usize>())
                .and_then(|bytes| bytes.checked_add(std::mem::size_of::<TurboInterleaver>()))
                .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
                .ok_or(DspError::Capacity)?
        };
        std::mem::size_of::<Self>()
            .checked_add(scratch)
            .and_then(|bytes| bytes.checked_add(interleaver))
            .ok_or(DspError::Capacity)
    }

    pub fn new(plan: TurboBlockPlan) -> Result<Self, DspError> {
        Ok(Self {
            plan,
            interleaver: plan.interleaver()?,
            joined: vec![Bit::Zero; plan.layout.joined],
            block: vec![Bit::Zero; plan.width],
        })
    }
    pub fn encode(&mut self, input: &[Bit], output: &mut [Bit]) -> Result<(), DspError> {
        if input.len() != self.plan.payload_bits() || output.len() != self.plan.coded {
            return Err(DspError::InvalidLength);
        }
        self.plan.layout.attach(input, &mut self.joined)?;
        let Some(interleaver) = &self.interleaver else {
            return Ok(());
        };
        let mut offset = 0;
        for (index, block) in output
            .chunks_exact_mut(coded_bits(self.plan.width)?)
            .enumerate()
        {
            // TS 25.212 4.2.2.2 inserts transmitted zero filler only before the first block.
            let filler = if index == 0 { self.plan.filler } else { 0 };
            let count = self.plan.width - filler;
            self.block[..filler].fill(Bit::Zero);
            self.block[filler..].copy_from_slice(&self.joined[offset..offset + count]);
            encode(interleaver, &self.block, block)?;
            offset += count;
        }
        Ok(())
    }
}

pub struct TurboBlockDecoder {
    plan: TurboBlockPlan,
    decoder: Option<TurboDecoder>,
    joined: Vec<Bit>,
    block: Vec<Bit>,
    progress: Option<SegmentProgress>,
    crc_counts: crate::transport_blocks::CrcCounts,
}

struct SegmentProgress {
    block: usize,
    offset: usize,
}

impl TurboBlockDecoder {
    pub(crate) fn crc_counts(&self) -> crate::transport_blocks::CrcCounts {
        self.crc_counts
    }
    pub(crate) fn cancel(&mut self) {
        self.progress = None;
        if let Some(decoder) = &mut self.decoder {
            decoder.cancel();
        }
    }
    pub(crate) fn reserve_for(&mut self, plans: &[TurboBlockPlan]) -> Result<(), DspError> {
        let maximum = plans
            .iter()
            .map(|plan| plan.width)
            .max()
            .unwrap_or(self.plan.width);
        let joined = plans
            .iter()
            .map(|plan| plan.layout.joined)
            .max()
            .unwrap_or(self.joined.len());
        if maximum > 0 {
            if let Some(decoder) = &mut self.decoder {
                decoder.reserve_width(maximum)?;
            } else {
                return Err(DspError::InvalidCode);
            }
        }
        self.block.resize(maximum.max(self.block.len()), Bit::Zero);
        self.joined.resize(joined.max(self.joined.len()), Bit::Zero);
        Ok(())
    }
    pub(crate) fn configure(
        &mut self,
        plan: TurboBlockPlan,
        interleaver: Option<&Arc<TurboInterleaver>>,
    ) -> Result<(), DspError> {
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if plan.width > self.block.len() || plan.layout.joined > self.joined.len() {
            return Err(DspError::Capacity);
        }
        if plan.width > 0 {
            let interleaver = interleaver
                .filter(|interleaver| interleaver.len() == plan.width)
                .ok_or(DspError::InvalidCode)?;
            self.decoder
                .as_mut()
                .ok_or(DspError::InvalidCode)?
                .configure(Arc::clone(interleaver))?;
        }
        self.plan = plan;
        Ok(())
    }
    pub fn new(plan: TurboBlockPlan, iterations: NonZeroU8) -> Result<Self, DspError> {
        if iterations.get() > super::MAX_ITERATIONS {
            return Err(DspError::Capacity);
        }
        let decoder = plan
            .interleaver()?
            .map(|plan| TurboDecoder::new(plan, iterations))
            .transpose()?;
        Ok(Self {
            plan,
            decoder,
            joined: vec![Bit::Zero; plan.layout.joined],
            block: vec![Bit::Zero; plan.width],
            progress: None,
            crc_counts: crate::transport_blocks::CrcCounts::default(),
        })
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
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        self.crc_counts = crate::transport_blocks::CrcCounts::default();
        if input.len() != self.plan.coded || output.len() != self.plan.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        if self.plan.coded == 0 {
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
        self.decoder
            .as_mut()
            .ok_or(DspError::InvalidCode)?
            .begin(&input[..coded_bits(self.plan.width)?])?;
        self.progress = Some(SegmentProgress {
            block: 0,
            offset: 0,
        });
        Ok(crate::coding::DecodeProgress::Pending)
    }
    pub(crate) fn step(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
        work: &mut crate::coding::DecodeWork,
    ) -> Result<crate::coding::DecodeProgress, DspError> {
        if input.len() != self.plan.coded || output.len() != self.plan.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        let mut progress = self.progress.take().ok_or(DspError::DiscontinuousTime)?;
        let width = coded_bits(self.plan.width)?;
        let decoder = self.decoder.as_mut().ok_or(DspError::InvalidCode)?;
        while progress.block < self.plan.count {
            if !decoder.step(&mut self.block[..self.plan.width], work)? {
                self.progress = Some(progress);
                return Ok(crate::coding::DecodeProgress::Pending);
            }
            let filler = if progress.block == 0 {
                self.plan.filler
            } else {
                0
            };
            if self.block[..filler].iter().any(|bit| *bit != Bit::Zero) {
                return Ok(crate::coding::DecodeProgress::Complete(
                    BlockOutcome::InvalidFiller,
                ));
            }
            if decoder.unobserved_until > filler {
                // Later code blocks cannot resolve this block's missing evidence.
                return Ok(crate::coding::DecodeProgress::Complete(
                    BlockOutcome::Erased,
                ));
            }
            let count = self.plan.width - filler;
            self.joined[progress.offset..progress.offset + count]
                .copy_from_slice(&self.block[filler..self.plan.width]);
            progress.offset += count;
            progress.block += 1;
            if progress.block < self.plan.count {
                decoder.begin(&input[progress.block * width..(progress.block + 1) * width])?;
            }
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
