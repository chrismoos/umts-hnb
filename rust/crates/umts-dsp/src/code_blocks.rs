// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    convolutional::{self, CodeRate, TerminatedDecoder},
};

use crate::convolutional::{
    MAX_INFORMATION_BITS as MAX_CONVOLUTIONAL_BLOCK_BITS, MEMORY_BITS as TAIL_BITS,
};

#[derive(Clone, Copy, Debug)]
pub struct ConvolutionalBlocks {
    rate: CodeRate,
    input_bits: usize,
    block_count: usize,
    block_bits: usize,
    filler_bits: usize,
    coded_bits: usize,
}

impl ConvolutionalBlocks {
    // TS 25.212 4.2.2 places all segmentation filler before the first code block's input.
    pub fn new(input_bits: usize, rate: CodeRate) -> Result<Self, DspError> {
        let block_count = input_bits.div_ceil(MAX_CONVOLUTIONAL_BLOCK_BITS);
        let block_bits = if block_count == 0 {
            0
        } else {
            input_bits.div_ceil(block_count)
        };
        let padded = block_count
            .checked_mul(block_bits)
            .ok_or(DspError::Capacity)?;
        let coded_bits = if input_bits == 0 {
            0
        } else {
            block_bits
                .checked_add(TAIL_BITS)
                .and_then(|n| n.checked_mul(rate.denominator()))
                .and_then(|n| n.checked_mul(block_count))
                .ok_or(DspError::Capacity)?
        };
        Ok(Self {
            rate,
            input_bits,
            block_count,
            block_bits,
            filler_bits: padded - input_bits,
            coded_bits,
        })
    }

    pub fn block_count(self) -> usize {
        self.block_count
    }
    pub(crate) fn rate(self) -> CodeRate {
        self.rate
    }
    pub fn block_bits(self) -> usize {
        self.block_bits
    }
    pub fn filler_bits(self) -> usize {
        self.filler_bits
    }
    pub fn coded_bits(self) -> usize {
        self.coded_bits
    }

    pub fn encode(
        self,
        input: &[Bit],
        output: &mut [Bit],
        scratch: &mut [Bit],
    ) -> Result<(), DspError> {
        if input.len() != self.input_bits
            || output.len() != self.coded_bits
            || scratch.len() != self.block_bits
        {
            return Err(DspError::InvalidLength);
        }
        if self.block_count == 0 {
            return Ok(());
        }
        let coded_block = (self.block_bits + TAIL_BITS) * self.rate.denominator();
        let mut consumed = 0;
        for (index, block) in output.chunks_exact_mut(coded_block).enumerate() {
            let filler = if index == 0 { self.filler_bits } else { 0 };
            scratch[..filler].fill(Bit::Zero);
            let count = self.block_bits - filler;
            scratch[filler..].copy_from_slice(&input[consumed..consumed + count]);
            convolutional::encode(self.rate, scratch, block)?;
            consumed += count;
        }
        Ok(())
    }
}

pub struct SegmentedConvolutionalDecoder {
    plan: ConvolutionalBlocks,
    decoder: TerminatedDecoder,
    scratch: Vec<Bit>,
    progress: Option<SegmentationProgress>,
    unambiguous: bool,
}

struct SegmentationProgress {
    block: usize,
    offset: usize,
    filler_valid: bool,
}

impl SegmentedConvolutionalDecoder {
    pub(crate) fn active(&self) -> bool {
        self.progress.is_some()
    }
    pub(crate) fn cancel(&mut self) {
        self.progress = None;
        self.decoder.cancel();
    }
    pub fn new(plan: ConvolutionalBlocks) -> Self {
        Self {
            plan,
            decoder: TerminatedDecoder::new(plan.rate),
            scratch: vec![Bit::Zero; plan.block_bits],
            progress: None,
            unambiguous: true,
        }
    }
    pub(crate) fn reserve_for(&mut self, plans: &[ConvolutionalBlocks]) -> Result<(), DspError> {
        if plans
            .iter()
            .any(|plan| plan.rate.denominator() != self.plan.rate.denominator())
        {
            return Err(DspError::InvalidCode);
        }
        let maximum = plans
            .iter()
            .map(|plan| plan.block_bits)
            .max()
            .unwrap_or(self.scratch.len());
        self.scratch
            .resize(maximum.max(self.scratch.len()), Bit::Zero);
        Ok(())
    }
    pub(crate) fn configure(&mut self, plan: ConvolutionalBlocks) -> Result<(), DspError> {
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if plan.rate.denominator() != self.plan.rate.denominator()
            || plan.block_bits > self.scratch.len()
        {
            return Err(DspError::Capacity);
        }
        self.plan = plan;
        Ok(())
    }

    // The returned flag verifies known zero filler independently of transport CRCs.
    pub fn decode(&mut self, input: &[f32], output: &mut [Bit]) -> Result<bool, DspError> {
        if output.len() != self.plan.input_bits {
            return Err(DspError::InvalidLength);
        }
        self.begin(input)?;
        let mut work = crate::coding::DecodeWork::new(std::num::NonZeroUsize::MAX);
        self.step(input, output, &mut work)?
            .ok_or(DspError::Capacity)
    }
    pub(crate) fn begin(&mut self, input: &[f32]) -> Result<(), DspError> {
        let plan = self.plan;
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if input.len() != plan.coded_bits {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        if plan.block_count != 0 {
            let width = (plan.block_bits + TAIL_BITS) * plan.rate.denominator();
            self.decoder.begin(&input[..width], plan.block_bits)?;
            self.decoder.known_filler(plan.filler_bits);
        }
        self.unambiguous = true;
        self.progress = Some(SegmentationProgress {
            block: 0,
            offset: 0,
            filler_valid: true,
        });
        Ok(())
    }
    pub(crate) fn step(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
        work: &mut crate::coding::DecodeWork,
    ) -> Result<Option<bool>, DspError> {
        let plan = self.plan;
        if input.len() != plan.coded_bits || output.len() != plan.input_bits {
            return Err(DspError::InvalidLength);
        }
        let mut progress = self.progress.take().ok_or(DspError::DiscontinuousTime)?;
        let scratch = &mut self.scratch[..plan.block_bits];
        let width = (plan.block_bits + TAIL_BITS) * plan.rate.denominator();
        while progress.block < plan.block_count {
            let block = &input[progress.block * width..(progress.block + 1) * width];
            if !self.decoder.step(block, scratch, work)? {
                self.progress = Some(progress);
                return Ok(None);
            }
            let filler = if progress.block == 0 {
                plan.filler_bits
            } else {
                0
            };
            progress.filler_valid &= scratch[..filler].iter().all(|bit| *bit == Bit::Zero);
            self.unambiguous &= self.decoder.unambiguous();
            let count = plan.block_bits - filler;
            output[progress.offset..progress.offset + count].copy_from_slice(&scratch[filler..]);
            progress.offset += count;
            progress.block += 1;
            if progress.block < plan.block_count {
                self.decoder.begin(
                    &input[progress.block * width..(progress.block + 1) * width],
                    plan.block_bits,
                )?;
            }
        }
        Ok(Some(progress.filler_valid))
    }
    pub(crate) fn unambiguous(&self) -> bool {
        self.unambiguous
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_segmentation_boundaries_include_each_blocks_tail() {
        for (input, count, width, filler, coded) in [
            (0, 0, 0, 0, 0),
            (504, 1, 504, 0, 1024),
            (505, 2, 253, 1, 1044),
            (544, 2, 272, 0, 1120),
            (1009, 3, 337, 2, 2070),
        ] {
            let plan = ConvolutionalBlocks::new(input, CodeRate::Half).unwrap();
            assert_eq!(
                (
                    plan.block_count(),
                    plan.block_bits(),
                    plan.filler_bits(),
                    plan.coded_bits()
                ),
                (count, width, filler, coded)
            );
        }
        assert!(matches!(
            ConvolutionalBlocks::new(usize::MAX, CodeRate::Third),
            Err(DspError::Capacity)
        ));
    }

    #[test]
    fn filler_precedes_only_the_first_block_and_does_not_mutate_input() {
        for rate in [CodeRate::Half, CodeRate::Third] {
            let input: Vec<_> = (0..505).map(|n| Bit::from(n % 3 == 0)).collect();
            let saved = input.clone();
            let plan = ConvolutionalBlocks::new(input.len(), rate).unwrap();
            let mut actual = vec![Bit::Zero; plan.coded_bits()];
            plan.encode(&input, &mut actual, &mut vec![Bit::Zero; 253])
                .unwrap();
            let mut first = vec![Bit::Zero];
            first.extend_from_slice(&input[..252]);
            let mut expected = vec![Bit::Zero; plan.coded_bits()];
            let split = (253 + 8) * rate.denominator();
            convolutional::encode(rate, &first, &mut expected[..split]).unwrap();
            convolutional::encode(rate, &input[252..], &mut expected[split..]).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(input, saved);
            assert!(
                plan.encode(&input, &mut actual, &mut [Bit::Zero; 252])
                    .is_err()
            );
        }
    }
}
