// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::coding::{DecodeProgress, DecodeWork};
use crate::turbo::CODE_RATE;

pub(crate) struct Crc24TurboDecoder {
    plan: TurboBlockPlan,
    decoders: Vec<TurboDecoder>,
    joined: Vec<Bit>,
    block: Vec<Bit>,
    iterations: NonZeroU8,
    progress: Option<Progress>,
}

#[cfg(test)]
mod tests;

enum Progress {
    Systematic(SystematicCheck),
    Iterative(Round),
}

struct SystematicCheck {
    next: usize,
    remainder: u32,
    crc_valid: bool,
}

struct Round {
    iteration: u8,
    block: usize,
    offset: usize,
    filler_valid: bool,
    observed: bool,
    checked: usize,
    remainder: u32,
    crc_valid: bool,
}

impl Round {
    fn new(iteration: u8) -> Self {
        Self {
            iteration,
            block: 0,
            offset: 0,
            filler_valid: true,
            observed: true,
            checked: 0,
            remainder: 0,
            crc_valid: true,
        }
    }
}

impl Crc24TurboDecoder {
    pub(crate) fn new(plans: &[TurboBlockPlan], iterations: NonZeroU8) -> Result<Self, DspError> {
        let plan = *plans.first().ok_or(DspError::InvalidLength)?;
        if plans
            .iter()
            .any(|plan| !plan.layout.is_single_crc24() || plan.count == 0)
        {
            return Err(DspError::InvalidCode);
        }
        let width = plans
            .iter()
            .map(|plan| plan.width)
            .max()
            .ok_or(DspError::Capacity)?;
        let count = plans
            .iter()
            .map(|plan| plan.count)
            .max()
            .ok_or(DspError::Capacity)?;
        let joined = plans
            .iter()
            .map(|plan| plan.layout.joined)
            .max()
            .ok_or(DspError::Capacity)?;
        let interleaver = plan.interleaver()?.ok_or(DspError::InvalidCode)?;
        let mut decoders = Vec::with_capacity(count);
        for _ in 0..count {
            let mut decoder = TurboDecoder::new(Arc::clone(&interleaver), iterations)?;
            decoder.reserve_width(width)?;
            decoders.push(decoder);
        }
        Ok(Self {
            plan,
            decoders,
            joined: vec![Bit::Zero; joined],
            block: vec![Bit::Zero; width],
            iterations,
            progress: None,
        })
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        (self.joined.capacity() + self.block.capacity()) * std::mem::size_of::<Bit>()
            + self.decoders.capacity() * std::mem::size_of::<TurboDecoder>()
            + self
                .decoders
                .iter()
                .map(TurboDecoder::heap_bytes)
                .sum::<usize>()
    }

    pub(crate) fn cancel(&mut self) {
        self.progress = None;
        for decoder in &mut self.decoders {
            decoder.cancel();
        }
    }

    pub(crate) fn configure(
        &mut self,
        plan: TurboBlockPlan,
        interleaver: Option<&Arc<TurboInterleaver>>,
    ) -> Result<(), DspError> {
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if !plan.layout.is_single_crc24() || plan.count == 0 {
            return Err(DspError::InvalidCode);
        }
        if plan.count > self.decoders.len()
            || plan.width > self.block.len()
            || plan.layout.joined > self.joined.len()
        {
            return Err(DspError::Capacity);
        }
        let interleaver = interleaver
            .filter(|value| value.len() == plan.width)
            .ok_or(DspError::InvalidCode)?;
        for decoder in &mut self.decoders {
            decoder.configure(Arc::clone(interleaver))?;
        }
        self.plan = plan;
        Ok(())
    }

    pub(crate) fn begin(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
    ) -> Result<DecodeProgress, DspError> {
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if input.len() != self.plan.coded || output.len() != self.plan.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        if input.iter().all(|value| *value == 0.0) {
            return Ok(DecodeProgress::Complete(BlockOutcome::Erased));
        }
        self.progress = Some(Progress::Systematic(SystematicCheck {
            next: 0,
            remainder: 0,
            crc_valid: true,
        }));
        Ok(DecodeProgress::Pending)
    }

    pub(crate) fn step(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
        work: &mut DecodeWork,
    ) -> Result<DecodeProgress, DspError> {
        if input.len() != self.plan.coded || output.len() != self.plan.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        let mut round = match self.progress.take().ok_or(DspError::DiscontinuousTime)? {
            Progress::Iterative(round) => round,
            Progress::Systematic(mut check) => {
                let width = coded_bits(self.plan.width)?;
                let information = self.plan.width * self.plan.count;
                'systematic: while check.next < information {
                    let block = check.next / self.plan.width;
                    let bit = check.next % self.plan.width;
                    let start = block * width + CODE_RATE * bit;
                    let end = block * width + CODE_RATE * self.plan.width;
                    for triple in input[start..end].chunks_exact(CODE_RATE) {
                        if !work.consume(1) {
                            self.progress = Some(Progress::Systematic(check));
                            return Ok(DecodeProgress::Pending);
                        }
                        // TS 25.212 4.2.3.2.1 orders each information triple as x, z, z'.
                        let value = triple[0];
                        if value == 0.0 || !value.is_finite() {
                            break 'systematic;
                        }
                        if check.next < self.plan.filler {
                            if value < 0.0 {
                                break 'systematic;
                            }
                        } else {
                            let index = check.next - self.plan.filler;
                            self.joined[index] = Bit::from(value < 0.0);
                            if index < self.plan.payload_bits() {
                                check.remainder = TransportCrc::Crc24.extend_remainder(
                                    check.remainder,
                                    &self.joined[index..index + 1],
                                );
                            } else {
                                check.crc_valid &= self.joined[index] as u32
                                    == (check.remainder >> (index - self.plan.payload_bits())) & 1;
                            }
                        }
                        check.next += 1;
                    }
                }
                if check.next == information && check.crc_valid {
                    output.copy_from_slice(&self.joined[..self.plan.payload_bits()]);
                    return Ok(DecodeProgress::Complete(BlockOutcome::Decoded));
                }
                for (decoder, evidence) in self.decoders[..self.plan.count]
                    .iter_mut()
                    .zip(input.chunks_exact(width))
                {
                    decoder.begin(evidence)?;
                }
                Round::new(1)
            }
        };
        loop {
            while round.block < self.plan.count {
                if !self.decoders[round.block].step_to_iteration(
                    &mut self.block[..self.plan.width],
                    work,
                    round.iteration,
                )? {
                    self.progress = Some(Progress::Iterative(round));
                    return Ok(DecodeProgress::Pending);
                }
                let filler = if round.block == 0 {
                    self.plan.filler
                } else {
                    0
                };
                round.filler_valid &= self.block[..filler].iter().all(|bit| *bit == Bit::Zero);
                round.observed &= self.decoders[round.block].unobserved_until <= filler;
                let count = self.plan.width - filler;
                self.joined[round.offset..round.offset + count]
                    .copy_from_slice(&self.block[filler..self.plan.width]);
                round.offset += count;
                round.block += 1;
            }
            // No segment is publishable until the complete transport CRC and filler both pass.
            while round.checked < self.plan.layout.joined {
                if work.remaining() == 0 {
                    self.progress = Some(Progress::Iterative(round));
                    return Ok(DecodeProgress::Pending);
                }
                let index = round.checked;
                if index < self.plan.payload_bits() {
                    let count = (self.plan.payload_bits() - index).min(work.remaining());
                    let consumed = work.consume(count);
                    debug_assert!(consumed);
                    round.remainder = TransportCrc::Crc24
                        .extend_remainder(round.remainder, &self.joined[index..index + count]);
                    round.checked += count;
                } else {
                    let consumed = work.consume(1);
                    debug_assert!(consumed);
                    round.crc_valid &= self.joined[index] as u32
                        == (round.remainder >> (index - self.plan.payload_bits())) & 1;
                    round.checked += 1;
                }
            }
            let outcome = if !round.filler_valid {
                BlockOutcome::InvalidFiller
            } else if !round.crc_valid {
                BlockOutcome::InvalidCrc { block: 0 }
            } else if !round.observed {
                // CRC cannot qualify arbitrary zero-valued posterior tie breaks.
                // Known filler is excluded when recording each block's evidence.
                BlockOutcome::Erased
            } else {
                BlockOutcome::Decoded
            };
            if outcome == BlockOutcome::Decoded || round.iteration == self.iterations.get() {
                if outcome == BlockOutcome::Decoded {
                    output.copy_from_slice(&self.joined[..self.plan.payload_bits()]);
                }
                self.cancel();
                return Ok(DecodeProgress::Complete(outcome));
            }
            round = Round::new(round.iteration + 1);
        }
    }

    pub(crate) fn decode(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
    ) -> Result<BlockOutcome, DspError> {
        match self.begin(input, output)? {
            DecodeProgress::Complete(outcome) => Ok(outcome),
            DecodeProgress::Pending => {
                match self.step(input, output, &mut DecodeWork::new(NonZeroUsize::MAX))? {
                    DecodeProgress::Complete(outcome) => Ok(outcome),
                    DecodeProgress::Pending => Err(DspError::Capacity),
                }
            }
        }
    }
}
