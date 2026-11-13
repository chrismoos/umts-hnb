// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{MatchedTransport, TransportId};
use crate::{
    DspError,
    bits::Bit,
    coding::{
        CodedBlockFormat, CodedBlockPlan, DecodeBudget, DecodeProgress, DecodeWork, DecoderBank,
        DecoderBankPlan,
    },
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
    soft_bits::normalize_soft_bits,
    transport_blocks::BlockOutcome,
};
use std::{num::NonZeroUsize, sync::Arc};
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};

const MAX_FORMAT_COMBINATIONS: usize = 1024;
const MAX_FORMATS: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct TransportPlanLimits {
    pub maximum_coded_bits: NonZeroUsize,
    pub maximum_mapping_entries: NonZeroUsize,
    pub decode: DecodeBudget,
}

struct Mapping {
    bits: usize,
    frames: Vec<RateMatching>,
}

enum Assembly {
    Idle,
    Receiving { start: ChipInstant, frames: usize },
    Ready { start: ChipInstant },
    Consumed { start: ChipInstant },
}
impl Assembly {
    fn start(&self) -> Option<ChipInstant> {
        match self {
            Self::Idle => None,
            Self::Receiving { start, .. } | Self::Ready { start } | Self::Consumed { start } => {
                Some(*start)
            }
        }
    }
}

pub struct UplinkTransportPlan {
    id: TransportId,
    interval: Rel99Tti,
    blocks: CodedBlockPlan,
    decode: DecodeBudget,
    mappings: Vec<Mapping>,
    first: Interleaver,
    frame_bits: usize,
}

pub struct DedicatedUplink {
    plan: Arc<UplinkTransportPlan>,
    plans: Vec<Arc<UplinkTransportPlan>>,
    frame_scratch: Vec<f64>,
    frames: Vec<f64>,
    equalized: Vec<f64>,
    normalized: Vec<f32>,
    decoder: DecoderBank,
    state: Assembly,
    selection: usize,
    pending: Option<PendingDecode>,
    decoded_payload: Vec<Bit>,
}

struct PendingDecode {
    start: ChipInstant,
    selection: usize,
    plan: Arc<UplinkTransportPlan>,
    progress: DecodeProgress,
}

pub struct CompletedUplink<'a> {
    pub start: ChipInstant,
    pub selection: usize,
    pub outcome: BlockOutcome,
    pub crc_counts: crate::transport_blocks::CrcCounts,
    pub payload: &'a [Bit],
}
impl UplinkTransportPlan {
    pub fn new(
        id: TransportId,
        format: impl Into<CodedBlockFormat>,
        interval: Rel99Tti,
        outputs: &[usize],
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        // TS 25.212 4.2 permits an 80 ms DCH only at SF512, which normal uplink DPDCH cannot provide.
        if matches!(interval, Rel99Tti::Ms80)
            || outputs.is_empty()
            || outputs.len() > MAX_FORMAT_COMBINATIONS
            || outputs.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(DspError::InvalidCode);
        }
        let count = interval.frames();
        let blocks = CodedBlockPlan::new(format.into(), limits.maximum_coded_bits)?;
        let frame_bits = blocks.coded_bits().div_ceil(count);
        let equalized = frame_bits.checked_mul(count).ok_or(DspError::Capacity)?;
        let entries = outputs
            .iter()
            .try_fold(0_usize, |total, bits| total.checked_add(*bits))
            .and_then(|total| total.checked_mul(count))
            .and_then(|total| total.checked_add(equalized))
            .ok_or(DspError::Capacity)?;
        if entries > limits.maximum_mapping_entries.get() {
            return Err(DspError::Capacity);
        }
        let mappings = outputs
            .iter()
            .map(|bits| {
                let frames = (0..count)
                    .map(|frame| {
                        blocks.matching_uplink(
                            interval,
                            *bits,
                            frame,
                            limits.maximum_mapping_entries,
                        )
                    })
                    .collect::<Result<_, _>>()?;
                Ok(Mapping {
                    bits: *bits,
                    frames,
                })
            })
            .collect::<Result<_, DspError>>()?;
        Ok(Self {
            id,
            interval,
            blocks,
            decode: limits.decode,
            mappings,
            first: Interleaver::first(interval, equalized)?,
            frame_bits,
        })
    }
}

impl DedicatedUplink {
    pub fn new(
        id: TransportId,
        format: impl Into<CodedBlockFormat>,
        interval: Rel99Tti,
        outputs: &[usize],
        limits: TransportPlanLimits,
    ) -> Result<Self, DspError> {
        UplinkTransportPlan::new(id, format, interval, outputs, limits)
            .and_then(|plan| Self::from_plan(Arc::new(plan)))
    }

    pub fn from_plan(plan: Arc<UplinkTransportPlan>) -> Result<Self, DspError> {
        Self::for_plans(vec![plan])
    }
    pub fn for_plans(plans: Vec<Arc<UplinkTransportPlan>>) -> Result<Self, DspError> {
        let plan = Arc::clone(plans.first().ok_or(DspError::InvalidCode)?);
        if plans.len() > MAX_FORMATS
            || plans.iter().any(|other| {
                other.id != plan.id
                    || other.interval.frames() != plan.interval.frames()
                    || other.decode != plan.decode
            })
        {
            return Err(DspError::InvalidCode);
        }
        let frame_bits = plans
            .iter()
            .map(|plan| plan.frame_bits)
            .max()
            .expect("nonempty formats");
        let equalized = frame_bits * plan.interval.frames();
        let coded = plans
            .iter()
            .map(|plan| plan.blocks.coded_bits())
            .max()
            .expect("nonempty formats");
        let decoder_plan = DecoderBankPlan::new(plans.iter().map(|plan| plan.blocks).collect())?;
        let decoder = DecoderBank::new(Arc::new(decoder_plan), plan.decode)?;
        let payload = plans
            .iter()
            .map(|plan| plan.blocks.payload_bits())
            .max()
            .expect("nonempty formats");
        Ok(Self {
            plan,
            plans,
            frame_scratch: vec![0.0; frame_bits],
            frames: vec![0.0; equalized],
            equalized: vec![0.0; equalized],
            normalized: vec![0.0; coded],
            decoder,
            state: Assembly::Idle,
            selection: 0,
            pending: None,
            decoded_payload: vec![Bit::Zero; payload],
        })
    }
    pub fn select(&mut self, index: usize) -> Result<(), DspError> {
        let plan = self.plans.get(index).ok_or(DspError::InvalidCode)?;
        if matches!(
            self.state,
            Assembly::Receiving { .. } | Assembly::Ready { .. }
        ) {
            return Err(DspError::DiscontinuousTime);
        }
        self.selection = index;
        self.plan = Arc::clone(plan);
        Ok(())
    }
    pub fn discard_incomplete(&mut self) -> bool {
        if let Assembly::Receiving { start, .. } = self.state {
            self.state = Assembly::Consumed { start };
            true
        } else {
            false
        }
    }
    pub fn ready_start(&self) -> Option<ChipInstant> {
        if let Assembly::Ready { start } = self.state {
            Some(start)
        } else {
            None
        }
    }

    pub fn frame_input_bits(&self) -> usize {
        self.plan.frame_bits
    }
    pub fn payload_bits(&self) -> usize {
        self.plan.blocks.payload_bits()
    }

    pub fn pending_decode_count(&self) -> usize {
        usize::from(self.pending.is_some())
    }
    pub fn cancel_decode(&mut self) -> bool {
        let cancelled = self.pending.take().is_some();
        self.decoder.cancel();
        cancelled
    }
    pub fn decode_storage_bytes(&self) -> usize {
        self.decoded_payload.len() * std::mem::size_of::<Bit>()
    }

    pub fn begin_decode(&mut self) -> Result<bool, DspError> {
        if self.pending.is_some() {
            return Ok(false);
        }
        let Assembly::Ready { start } = self.state else {
            return Ok(false);
        };
        self.plan.first.deinterleave(
            &self.frames[..self.plan.frame_bits * self.plan.interval.frames()],
            &mut self.equalized[..self.plan.frame_bits * self.plan.interval.frames()],
        )?;
        normalize_soft_bits(
            &self.equalized[..self.plan.blocks.coded_bits()],
            &mut self.normalized[..self.plan.blocks.coded_bits()],
        )?;
        self.decoder.select(self.selection)?;
        let progress = self.decoder.begin(
            &self.normalized[..self.plan.blocks.coded_bits()],
            &mut self.decoded_payload[..self.plan.blocks.payload_bits()],
        )?;
        self.pending = Some(PendingDecode {
            start,
            selection: self.selection,
            plan: Arc::clone(&self.plan),
            progress,
        });
        self.state = Assembly::Consumed { start };
        Ok(true)
    }

    pub fn decode_work(&mut self, work: &mut DecodeWork) -> Result<DecodeProgress, DspError> {
        let pending = self.pending.as_mut().ok_or(DspError::DiscontinuousTime)?;
        if pending.progress == DecodeProgress::Pending {
            pending.progress = self.decoder.step(
                &self.normalized[..pending.plan.blocks.coded_bits()],
                &mut self.decoded_payload[..pending.plan.blocks.payload_bits()],
                work,
            )?;
        }
        Ok(pending.progress)
    }

    pub fn completed_decode(&self) -> Option<CompletedUplink<'_>> {
        let pending = self.pending.as_ref()?;
        let DecodeProgress::Complete(outcome) = pending.progress else {
            return None;
        };
        Some(CompletedUplink {
            start: pending.start,
            selection: pending.selection,
            outcome,
            crc_counts: self.decoder.crc_counts(),
            payload: if outcome == BlockOutcome::Decoded {
                &self.decoded_payload[..pending.plan.blocks.payload_bits()]
            } else {
                &[]
            },
        })
    }

    pub fn finish_decode(&mut self) -> Result<(), DspError> {
        if self.completed_decode().is_none() {
            return Err(DspError::DiscontinuousTime);
        }
        self.pending = None;
        Ok(())
    }

    pub fn begin(&mut self, start: ChipInstant, cfn: u8) -> Result<(), DspError> {
        if matches!(self.state, Assembly::Ready { .. }) {
            return Err(DspError::DiscontinuousTime);
        }
        let span = CHIPS_PER_FRAME * self.plan.interval.frames() as u64;
        start
            .index
            .checked_add(span)
            .ok_or(DspError::InvalidLength)?;
        if !usize::from(cfn).is_multiple_of(self.plan.interval.frames())
            || self.state.start().is_some_and(|previous| {
                previous.epoch != start.epoch || start.index < previous.index + span
            })
        {
            return Err(DspError::DiscontinuousTime);
        }
        self.state = Assembly::Receiving { start, frames: 0 };
        Ok(())
    }

    pub fn receive(
        &mut self,
        time: ChipInstant,
        allocation: MatchedTransport,
        input: &[f64],
    ) -> Result<(), DspError> {
        let Assembly::Receiving { start, frames } = self.state else {
            return Err(DspError::DiscontinuousTime);
        };
        if time.epoch != start.epoch || time.index != start.index + frames as u64 * CHIPS_PER_FRAME
        {
            return Err(DspError::DiscontinuousTime);
        }
        if allocation.id != self.plan.id
            || allocation.input_bits != self.plan.frame_bits
            || allocation.output_bits != input.len()
        {
            return Err(DspError::InvalidLength);
        }
        let mapping = self
            .plan
            .mappings
            .iter()
            .find(|mapping| mapping.bits == input.len())
            .ok_or(DspError::InvalidRateMatching)?;
        mapping.frames[frames]
            .combine_soft(input, &mut self.frame_scratch[..self.plan.frame_bits])?;
        self.frames[frames * self.plan.frame_bits..(frames + 1) * self.plan.frame_bits]
            .copy_from_slice(&self.frame_scratch[..self.plan.frame_bits]);
        self.state = if frames + 1 == self.plan.interval.frames() {
            Assembly::Ready { start }
        } else {
            Assembly::Receiving {
                start,
                frames: frames + 1,
            }
        };
        Ok(())
    }

    pub fn decode(&mut self, output: &mut [Bit]) -> Result<Option<BlockOutcome>, DspError> {
        if output.len() != self.plan.blocks.payload_bits() {
            return Err(DspError::InvalidLength);
        }
        if self.pending.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        match self.state {
            Assembly::Receiving { .. } => return Ok(None),
            Assembly::Ready { .. } => (),
            _ => return Err(DspError::DiscontinuousTime),
        };
        self.begin_decode()?;
        let mut work = DecodeWork::new(NonZeroUsize::MAX);
        let DecodeProgress::Complete(outcome) = self.decode_work(&mut work)? else {
            return Err(DspError::Capacity);
        };
        if outcome == BlockOutcome::Decoded {
            output.copy_from_slice(self.completed_decode().expect("completed decode").payload);
        }
        self.finish_decode()?;
        Ok(Some(outcome))
    }
}
