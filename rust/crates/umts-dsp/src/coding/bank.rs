// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CodedBlockDecoder, CodedBlockPlan, DecodeBudget};
use crate::{DspError, bits::Bit, transport_blocks::BlockOutcome, turbo::TurboInterleaver};
use std::sync::Arc;

const MAX_FORMATS: usize = 32;

pub struct DecoderBankPlan {
    formats: Vec<CodedBlockPlan>,
    interleavers: Vec<Option<Arc<TurboInterleaver>>>,
    largest: usize,
}
impl DecoderBankPlan {
    pub fn new(formats: Vec<CodedBlockPlan>) -> Result<Self, DspError> {
        if formats.is_empty()
            || formats.len() > MAX_FORMATS
            || formats
                .iter()
                .any(|plan| identity(*plan) != identity(formats[0]))
        {
            return Err(DspError::InvalidCode);
        }
        let largest = formats
            .iter()
            .enumerate()
            .max_by_key(|(_, plan)| width(**plan))
            .expect("nonempty formats")
            .0;
        let mut interleavers: Vec<Option<Arc<TurboInterleaver>>> =
            Vec::with_capacity(formats.len());
        for (index, plan) in formats.iter().enumerate() {
            let cached = formats[..index]
                .iter()
                .position(|previous| width(*previous) == width(*plan));
            let interleaver = if let Some(previous) = cached {
                interleavers[previous].clone()
            } else if let CodedBlockPlan::Turbo(plan) = plan {
                plan.interleaver()?
            } else {
                None
            };
            interleavers.push(interleaver);
        }
        Ok(Self {
            formats,
            interleavers,
            largest,
        })
    }
}

pub struct DecoderBank {
    plan: Arc<DecoderBankPlan>,
    decoder: CodedBlockDecoder,
}
impl DecoderBank {
    pub(crate) fn crc_counts(&self) -> crate::transport_blocks::CrcCounts {
        self.decoder.crc_counts()
    }
    pub(crate) fn cancel(&mut self) {
        self.decoder.cancel();
    }
    pub fn new(plan: Arc<DecoderBankPlan>, budget: DecodeBudget) -> Result<Self, DspError> {
        let mut decoder = CodedBlockDecoder::new(plan.formats[plan.largest], budget)?;
        match &mut decoder {
            CodedBlockDecoder::Convolutional(decoder) => {
                let formats: Vec<_> = plan
                    .formats
                    .iter()
                    .map(|plan| match plan {
                        CodedBlockPlan::Convolutional(plan) => *plan,
                        _ => unreachable!("validated coding"),
                    })
                    .collect();
                decoder.reserve_for(&formats)?;
            }
            CodedBlockDecoder::Turbo(decoder) => {
                let formats: Vec<_> = plan
                    .formats
                    .iter()
                    .map(|plan| match plan {
                        CodedBlockPlan::Turbo(plan) => *plan,
                        _ => unreachable!("validated coding"),
                    })
                    .collect();
                decoder.reserve_for(&formats)?;
            }
        }
        let mut bank = Self { plan, decoder };
        bank.select(0)?;
        Ok(bank)
    }
    pub fn select(&mut self, index: usize) -> Result<(), DspError> {
        let plan = self.plan.formats.get(index).ok_or(DspError::InvalidCode)?;
        match (&mut self.decoder, plan) {
            (CodedBlockDecoder::Convolutional(decoder), CodedBlockPlan::Convolutional(plan)) => {
                decoder.configure(*plan)
            }
            (CodedBlockDecoder::Turbo(decoder), CodedBlockPlan::Turbo(plan)) => {
                decoder.configure(*plan, self.plan.interleavers[index].as_ref())
            }
            _ => Err(DspError::InvalidCode),
        }
    }
    pub fn decode(&mut self, input: &[f32], output: &mut [Bit]) -> Result<BlockOutcome, DspError> {
        self.decoder.decode(input, output)
    }
    pub(crate) fn begin(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
    ) -> Result<super::DecodeProgress, DspError> {
        self.decoder.begin(input, output)
    }
    pub(crate) fn step(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
        work: &mut super::DecodeWork,
    ) -> Result<super::DecodeProgress, DspError> {
        self.decoder.step(input, output, work)
    }
}

fn width(plan: CodedBlockPlan) -> usize {
    match plan {
        CodedBlockPlan::Convolutional(plan) => plan.block_width(),
        CodedBlockPlan::Turbo(plan) => plan.code_block_bits(),
    }
}
fn identity(plan: CodedBlockPlan) -> usize {
    match plan {
        CodedBlockPlan::Convolutional(plan) => plan.code_rate().denominator(),
        CodedBlockPlan::Turbo(_) => 0,
    }
}

#[cfg(test)]
mod tests;
