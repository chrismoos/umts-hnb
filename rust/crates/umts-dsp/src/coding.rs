// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    convolutional::CodeRate,
    crc::TransportCrc,
    interleaving::Rel99Tti,
    rate_matching::RateMatching,
    transport_blocks::{BlockDecoder, BlockEncoder, BlockFormat, BlockOutcome, BlockPlan},
    turbo::{TurboBlockDecoder, TurboBlockEncoder, TurboBlockFormat, TurboBlockPlan},
};
use std::num::{NonZeroU8, NonZeroUsize};

mod bank;
pub use bank::{DecoderBank, DecoderBankPlan};

/// One unit advances one trellis state or one traceback, extrinsic, or CRC bit.
#[derive(Clone, Copy, Debug)]
pub struct DecodeWork {
    remaining: usize,
}

impl DecodeWork {
    pub fn new(state_updates: NonZeroUsize) -> Self {
        Self {
            remaining: state_updates.get(),
        }
    }
    pub fn remaining(self) -> usize {
        self.remaining
    }
    pub fn with_limit<R>(&mut self, maximum: NonZeroUsize, run: impl FnOnce(&mut Self) -> R) -> R {
        let granted = self.remaining.min(maximum.get());
        self.remaining -= granted;
        struct Refund<'a> {
            parent: &'a mut DecodeWork,
            local: DecodeWork,
            granted: usize,
        }
        impl Drop for Refund<'_> {
            fn drop(&mut self) {
                self.parent.remaining += self.local.remaining.min(self.granted);
            }
        }
        let mut scope = Refund {
            parent: self,
            local: DecodeWork { remaining: granted },
            granted,
        };
        run(&mut scope.local)
    }
    pub(crate) fn consume(&mut self, state_updates: usize) -> bool {
        if self.remaining < state_updates {
            return false;
        }
        self.remaining -= state_updates;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeProgress {
    Pending,
    Complete(BlockOutcome),
}

#[derive(Clone, Copy, Debug)]
pub enum ChannelCoding {
    Convolutional(CodeRate),
    Turbo,
}

#[derive(Clone, Copy, Debug)]
pub struct CodedBlockFormat {
    pub block_bits: usize,
    pub blocks: usize,
    pub crc: TransportCrc,
    pub coding: ChannelCoding,
}

impl From<BlockFormat> for CodedBlockFormat {
    fn from(format: BlockFormat) -> Self {
        Self {
            block_bits: format.block_bits,
            blocks: format.blocks,
            crc: format.crc,
            coding: ChannelCoding::Convolutional(format.rate),
        }
    }
}
impl From<TurboBlockFormat> for CodedBlockFormat {
    fn from(format: TurboBlockFormat) -> Self {
        Self {
            block_bits: format.block_bits,
            blocks: format.blocks,
            crc: format.crc,
            coding: ChannelCoding::Turbo,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeBudget {
    turbo_iterations: NonZeroU8,
}
impl DecodeBudget {
    pub fn new(turbo_iterations: NonZeroU8) -> Result<Self, DspError> {
        if turbo_iterations.get() > crate::turbo::MAX_ITERATIONS {
            return Err(DspError::Capacity);
        }
        Ok(Self { turbo_iterations })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CodedBlockPlan {
    Convolutional(BlockPlan),
    Turbo(TurboBlockPlan),
}
impl CodedBlockPlan {
    pub fn new(format: CodedBlockFormat, maximum: NonZeroUsize) -> Result<Self, DspError> {
        match format.coding {
            ChannelCoding::Convolutional(rate) => BlockPlan::new(
                BlockFormat {
                    block_bits: format.block_bits,
                    blocks: format.blocks,
                    crc: format.crc,
                    rate,
                },
                maximum,
            )
            .map(Self::Convolutional),
            ChannelCoding::Turbo => TurboBlockPlan::new(
                TurboBlockFormat {
                    block_bits: format.block_bits,
                    blocks: format.blocks,
                    crc: format.crc,
                },
                maximum,
            )
            .map(Self::Turbo),
        }
    }
    pub fn payload_bits(self) -> usize {
        match self {
            Self::Convolutional(plan) => plan.payload_bits(),
            Self::Turbo(plan) => plan.payload_bits(),
        }
    }
    pub fn coded_bits(self) -> usize {
        match self {
            Self::Convolutional(plan) => plan.coded_bits(),
            Self::Turbo(plan) => plan.coded_bits(),
        }
    }
    pub fn matching_uplink(
        self,
        interval: Rel99Tti,
        output: usize,
        frame: usize,
        budget: NonZeroUsize,
    ) -> Result<RateMatching, DspError> {
        let input = self.coded_bits().div_ceil(interval.frames());
        match self {
            Self::Convolutional(_) => {
                RateMatching::uplink_convolutional(input, output, interval, frame)
            }
            Self::Turbo(_) => RateMatching::uplink_turbo(input, output, interval, frame, budget),
        }
    }
    pub fn matching_fixed(
        self,
        maximum: usize,
        delta: i64,
        budget: NonZeroUsize,
    ) -> Result<RateMatching, DspError> {
        if maximum == 0 {
            return if self.coded_bits() == 0 && delta == 0 {
                RateMatching::downlink_convolutional_flexible(0, 0)
            } else {
                Err(DspError::InvalidRateMatching)
            };
        }
        match self {
            Self::Convolutional(_) => RateMatching::downlink_convolutional_fixed(
                self.coded_bits(),
                maximum,
                delta,
                budget,
            ),
            Self::Turbo(_) => {
                RateMatching::downlink_turbo_fixed(self.coded_bits(), maximum, delta, budget)
            }
        }
    }
    pub fn matching_flexible(
        self,
        output: usize,
        budget: NonZeroUsize,
    ) -> Result<RateMatching, DspError> {
        if output > budget.get() {
            return Err(DspError::Capacity);
        }
        match self {
            Self::Convolutional(_) => {
                RateMatching::downlink_convolutional_flexible(self.coded_bits(), output)
            }
            Self::Turbo(_) => {
                RateMatching::downlink_turbo_flexible(self.coded_bits(), output, budget)
            }
        }
    }
}

pub enum CodedBlockEncoder {
    Convolutional(BlockEncoder),
    Turbo(TurboBlockEncoder),
}
impl CodedBlockEncoder {
    pub fn new(plan: CodedBlockPlan) -> Result<Self, DspError> {
        match plan {
            CodedBlockPlan::Convolutional(plan) => Ok(Self::Convolutional(BlockEncoder::new(plan))),
            CodedBlockPlan::Turbo(plan) => TurboBlockEncoder::new(plan).map(Self::Turbo),
        }
    }
    pub fn encode(&mut self, input: &[Bit], output: &mut [Bit]) -> Result<(), DspError> {
        match self {
            Self::Convolutional(encoder) => encoder.encode(input, output),
            Self::Turbo(encoder) => encoder.encode(input, output),
        }
    }
}

pub enum CodedBlockDecoder {
    Convolutional(Box<BlockDecoder>),
    Turbo(Box<TurboBlockDecoder>),
}
impl CodedBlockDecoder {
    pub(crate) fn crc_counts(&self) -> crate::transport_blocks::CrcCounts {
        match self {
            Self::Convolutional(decoder) => decoder.crc_counts(),
            Self::Turbo(decoder) => decoder.crc_counts(),
        }
    }
    pub(crate) fn cancel(&mut self) {
        match self {
            Self::Convolutional(decoder) => decoder.cancel(),
            Self::Turbo(decoder) => decoder.cancel(),
        }
    }
    pub fn new(plan: CodedBlockPlan, budget: DecodeBudget) -> Result<Self, DspError> {
        match plan {
            CodedBlockPlan::Convolutional(plan) => {
                Ok(Self::Convolutional(Box::new(BlockDecoder::new(plan))))
            }
            CodedBlockPlan::Turbo(plan) => TurboBlockDecoder::new(plan, budget.turbo_iterations)
                .map(Box::new)
                .map(Self::Turbo),
        }
    }
    pub fn decode(&mut self, input: &[f32], output: &mut [Bit]) -> Result<BlockOutcome, DspError> {
        match self {
            Self::Convolutional(decoder) => decoder.decode(input, output),
            Self::Turbo(decoder) => decoder.decode(input, output),
        }
    }
    pub(crate) fn begin(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
    ) -> Result<DecodeProgress, DspError> {
        match self {
            Self::Convolutional(decoder) => decoder.begin(input, output),
            Self::Turbo(decoder) => decoder.begin(input, output),
        }
    }
    pub(crate) fn step(
        &mut self,
        input: &[f32],
        output: &mut [Bit],
        work: &mut DecodeWork,
    ) -> Result<DecodeProgress, DspError> {
        match self {
            Self::Convolutional(decoder) => decoder.step(input, output, work),
            Self::Turbo(decoder) => decoder.step(input, output, work),
        }
    }
}

#[cfg(test)]
mod work_budget_tests {
    use super::*;
    fn n(v: usize) -> NonZeroUsize {
        NonZeroUsize::new(v).unwrap()
    }
    #[test]
    fn partial_nested_exhausted_and_error_budgets() {
        let mut p = DecodeWork::new(n(20));
        p.with_limit(n(7), |c| {
            assert_eq!(c.remaining(), 7);
            assert!(!c.consume(8));
            assert!(c.consume(3));
            c.with_limit(n(9), |d| {
                assert_eq!(d.remaining(), 4);
                assert!(d.consume(2));
            });
            assert_eq!(c.remaining(), 2);
        });
        assert_eq!(p.remaining(), 15);
        let result: Result<(), u8> = p.with_limit(n(99), |c| {
            assert_eq!(c.remaining(), 15);
            assert!(c.consume(5));
            Err(7)
        });
        assert_eq!(result, Err(7));
        assert_eq!(p.remaining(), 10);
        assert!(p.consume(10));
        p.with_limit(n(3), |c| {
            assert_eq!(c.remaining(), 0);
            assert!(!c.consume(1));
        });
        assert_eq!(p.remaining(), 0);
    }
    #[test]
    fn unwinding_refunds_only_unused_work() {
        let mut p = DecodeWork::new(n(50));
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            p.with_limit(n(12), |c| {
                assert!(c.consume(5));
                panic!("expected")
            })
        }));
        assert!(got.is_err());
        assert_eq!(p.remaining(), 45);
    }
}
