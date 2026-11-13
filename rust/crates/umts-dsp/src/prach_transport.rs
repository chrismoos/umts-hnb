// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    convolutional::CodeRate,
    crc::TransportCrc,
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
    transport_blocks::{BlockDecoder, BlockFormat, BlockOutcome, BlockPlan},
};
use umts_types::time::CHIPS_PER_FRAME;

const MAX_BLOCK_BITS: usize = 4968;
const MAX_BLOCKS: usize = 512;
const MIN_PUNCTURING_PERCENT: u8 = 40;
const MAX_PUNCTURING_PERCENT: u8 = 100;
const PUNCTURING_STEP: u8 = 4;
const DATA_SFS: [u16; 4] = [256, 128, 64, 32];

#[derive(Clone, Copy, Debug)]
pub struct PrachFormat {
    pub block_bits: usize,
    pub blocks: usize,
    pub crc: TransportCrc,
    pub rate: CodeRate,
    pub interval: Rel99Tti,
}

pub struct PrachTransportPlan {
    format: PrachFormat,
    blocks: BlockPlan,
    sf: u16,
    coded_frame_bits: usize,
    physical_frame_bits: usize,
    first: Interleaver,
    second: Interleaver,
    matching: Vec<RateMatching>,
}

impl PrachTransportPlan {
    // TS 25.212 4.2.13.2 permits one TrCH, one PhCH and a 10/20 ms TTI.
    pub fn new(
        format: PrachFormat,
        minimum_sf: u16,
        puncturing_percent: u8,
    ) -> Result<Self, DspError> {
        if !matches!(format.interval, Rel99Tti::Ms10 | Rel99Tti::Ms20)
            || format.block_bits == 0
            || format.block_bits > MAX_BLOCK_BITS
            || format.blocks == 0
            || format.blocks > MAX_BLOCKS
            || !DATA_SFS.contains(&minimum_sf)
            || !(MIN_PUNCTURING_PERCENT..=MAX_PUNCTURING_PERCENT).contains(&puncturing_percent)
            || !(puncturing_percent - MIN_PUNCTURING_PERCENT).is_multiple_of(PUNCTURING_STEP)
        {
            return Err(DspError::InvalidLength);
        }
        let frames = format.interval.frames();
        let blocks = BlockPlan::new(
            BlockFormat {
                block_bits: format.block_bits,
                blocks: format.blocks,
                crc: format.crc,
                rate: format.rate,
            },
            std::num::NonZeroUsize::MAX,
        )?;
        let coded_frame_bits = blocks.coded_bits().div_ceil(frames);
        let capacity = |sf| CHIPS_PER_FRAME as usize / usize::from(sf);
        // TS 25.212 4.2.7.1.1 picks the smallest unpunctured SET1 entry, or the largest SET2 entry for one PhCH.
        let sf = if let Some(sf) = DATA_SFS
            .iter()
            .copied()
            .find(|sf| *sf >= minimum_sf && capacity(*sf) >= coded_frame_bits)
        {
            sf
        } else if capacity(minimum_sf) * usize::from(MAX_PUNCTURING_PERCENT)
            >= coded_frame_bits * usize::from(puncturing_percent)
        {
            minimum_sf
        } else {
            return Err(DspError::Capacity);
        };
        let physical_frame_bits = capacity(sf);
        let matching = (0..frames)
            .map(|frame| {
                RateMatching::uplink_convolutional(
                    coded_frame_bits,
                    physical_frame_bits,
                    format.interval,
                    frame,
                )
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            format,
            blocks,
            sf,
            coded_frame_bits,
            physical_frame_bits,
            first: Interleaver::first(format.interval, coded_frame_bits * frames)?,
            second: Interleaver::second(physical_frame_bits)?,
            matching,
        })
    }
    pub fn spreading_factor(&self) -> u16 {
        self.sf
    }
    pub fn coded_bits(&self) -> usize {
        self.blocks.coded_bits()
    }
    pub fn frame_bits(&self) -> usize {
        self.physical_frame_bits
    }
    pub fn frames(&self) -> usize {
        self.format.interval.frames()
    }
    pub fn payload_bits(&self) -> usize {
        self.format.block_bits * self.format.blocks
    }
}

pub struct PrachTransportDecoder {
    plan: PrachTransportPlan,
    decoder: BlockDecoder,
    physical: Vec<f32>,
    frames: Vec<f32>,
    equalized: Vec<f32>,
}

impl PrachTransportDecoder {
    pub fn new(plan: PrachTransportPlan) -> Self {
        let equalized = plan.coded_frame_bits * plan.frames();
        Self {
            decoder: BlockDecoder::new(plan.blocks),
            physical: vec![0.0; plan.physical_frame_bits],
            frames: vec![0.0; equalized],
            equalized: vec![0.0; equalized],
            plan,
        }
    }

    // Input LLRs are ordered physical data bits. No output is published until every transport CRC and filler check passes.
    pub fn decode(&mut self, input: &[f32], output: &mut [Bit]) -> Result<bool, DspError> {
        let plan = &self.plan;
        if input.len() != plan.physical_frame_bits * plan.frames()
            || output.len() != plan.payload_bits()
        {
            return Err(DspError::InvalidLength);
        }
        let mut has_evidence = false;
        for value in input {
            if !value.is_finite() {
                return Err(DspError::NonFiniteEvidence);
            }
            has_evidence |= *value != 0.0;
        }
        if !has_evidence {
            return Ok(false);
        }
        for (frame, input) in input.chunks_exact(plan.physical_frame_bits).enumerate() {
            plan.second.deinterleave(input, &mut self.physical)?;
            plan.matching[frame].combine_llrs(
                &self.physical,
                &mut self.frames
                    [frame * plan.coded_frame_bits..(frame + 1) * plan.coded_frame_bits],
            )?;
        }
        plan.first.deinterleave(&self.frames, &mut self.equalized)?;
        self.decoder
            .decode(&self.equalized[..plan.blocks.coded_bits()], output)
            .map(|outcome| outcome == BlockOutcome::Decoded)
    }
}

#[cfg(test)]
pub(crate) mod tests;
