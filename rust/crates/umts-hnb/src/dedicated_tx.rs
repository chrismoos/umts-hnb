// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::Arc,
};

use num_complex::Complex32;
use umts_dsp::{
    DspError,
    bits::{Bit, TxBit},
    channel_codes::OvsfCode,
    dedicated::{DownlinkDpch, DpchAmplitudes, DpchRole, DpchTiming},
    dedicated_transport::{
        DownlinkComposite, DownlinkTransportFrame, FixedDownlink, FlexibleDownlink, TransportId,
    },
    scrambling::{DownlinkScramblingCode, PrimaryScramblingCode},
    tfci::Tfci,
};
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant, SLOTS_PER_FRAME};

use crate::{
    cell::CellInformation,
    dedicated_rx::{FrameAnchor, ReceiveIdentity},
    rel99_plan::{CompiledRel99, DownlinkPlans},
};

mod downlink_power;
pub(crate) mod evidence;
mod runtime;
mod tpc_estimate;
use downlink_power::DownlinkPower;
pub(crate) use downlink_power::{DownlinkCommand, DownlinkControl};
pub use downlink_power::{DownlinkPowerCounters, DownlinkPowerSettings};
pub(crate) use evidence::DedicatedFrameSource;
pub use evidence::EmissionProgress;
pub(crate) use tpc_estimate::ESTIMATE_SLOTS;
#[cfg(test)]
pub use tpc_estimate::TpcExpiryEvidence;
pub use tpc_estimate::{TpcEstimate, TpcEstimateCounters, TpcGroupCounters};
#[cfg(test)]
mod tests;
mod transitions;
#[cfg(test)]
pub use runtime::TpcPlan;
pub use runtime::{
    DedicatedTpcPort, DedicatedTransmitPort, DedicatedTransmitPorts, DedicatedTransmitRuntime,
    FrameInput, OwnedTransportPayload, TransmitCommandId, TransmitCommandOutcome, TransmitEvent,
    TransmitRuntimeLimits,
};
pub(crate) use runtime::{FramePoolTag, FrameStorage};

const MAX_FRAME_SLOTS: usize = 16;
const MAX_TRANSPORTS: usize = 32;
const MAX_OWNERS: usize = 256;
const CONFIGURATIONS_PER_UE: usize = 2;
const MAX_TTI_FRAMES: usize = 8;
pub(crate) const MAX_TRACKED_FRAMES: usize = MAX_FRAME_SLOTS + MAX_TTI_FRAMES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransmitError {
    Identity,
    Time,
    Late(TransmitTimingStage),
    Phase,
    Capacity,
    Mapping,
    Conflict,
    Payload,
    FormatChangedWithinTti,
    Underrun,
    TpcUnavailable,
    FeedbackLate,
    FeedbackFuture,
    FeedbackOrder,
    Closed,
    TimingChange,
    SubmissionPending,
    SubmissionLost,
    Dsp(DspError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransmitTimingStage {
    Admission,
    Frame,
    Arm,
}
impl From<DspError> for TransmitError {
    fn from(error: DspError) -> Self {
        Self::Dsp(error)
    }
}
impl From<umts_types::channel_codes::ChannelCodeError> for TransmitError {
    fn from(error: umts_types::channel_codes::ChannelCodeError) -> Self {
        Self::Dsp(error.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransmitPhase {
    Prepared,
    Staged {
        previous: ReceiveIdentity,
    },
    Armed,
    Scheduled {
        previous: ReceiveIdentity,
    },
    Draining {
        next: ReceiveIdentity,
        boundary: ChipInstant,
    },
    Stopped {
        next: ReceiveIdentity,
        boundary: ChipInstant,
    },
    Failed,
    Retired,
}

#[derive(Clone, Copy)]
pub struct TransmitLimits {
    pub frame_slots: NonZeroUsize,
    pub maximum_frame_bits: NonZeroUsize,
    pub maximum_retained_bits: NonZeroUsize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransmitResources {
    pub physical_frame_bits: usize,
    pub retained_bits: usize,
    pub channels: usize,
    pub encoder_instances: usize,
}

pub struct TransportPayload<'a> {
    pub transport: TransportId,
    pub bits: &'a [Bit],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedFrame {
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub cfn: u8,
    pub tfci: Tfci,
}

#[derive(Clone, Copy, Debug)]
pub struct TpcRequest {
    pub identity: ReceiveIdentity,
    pub physical_slot_start: ChipInstant,
    pub synthesis_at: ChipInstant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransmitCounters {
    pub prepared_frames: u64,
    pub consumed_frames: u64,
    pub physical_slots: u64,
    pub mixed_chips: u64,
    pub underruns: u64,
    pub downlink_power: DownlinkPowerCounters,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransmitOperation {
    Admission,
    Frame,
    Arm,
    Synthesis,
}

#[derive(Clone, Debug)]
pub struct TransmitFailureEvidence {
    pub identity: ReceiveIdentity,
    pub operation: TransmitOperation,
    pub error: TransmitError,
    pub render_cursor: ChipInstant,
    pub requested: ChipInstant,
    pub anchor: FrameAnchor,
    pub next_prepare: ChipInstant,
    pub phase: TransmitPhase,
    pub counters: TransmitCounters,
    pub initial_tpc_maximum_slots: Option<NonZeroU32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpcInitialization {
    pub first_radio_link_set: bool,
    pub pattern_01_count: u8,
    pub maximum_slots: NonZeroU32,
}

enum Encoder {
    Fixed(FixedDownlink),
    Flexible(FlexibleDownlink),
}
impl Encoder {
    fn frame_bits(&self) -> usize {
        match self {
            Self::Fixed(encoder) => encoder.frame_bits(),
            Self::Flexible(encoder) => encoder.frame_bits(),
        }
    }
    fn tti_bits(&self) -> usize {
        match self {
            Self::Fixed(encoder) => encoder.tti_bits(),
            Self::Flexible(encoder) => encoder.tti_bits(),
        }
    }
}
struct Transport {
    id: TransportId,
    frames: usize,
    encoders: Vec<Option<Encoder>>,
    encoded: Vec<TxBit>,
    flexible: Vec<Bit>,
    active_format: Option<usize>,
    frame_bits: usize,
}
struct Frame {
    metadata: Option<PreparedFrame>,
    data: Vec<TxBit>,
}

#[derive(Clone, Debug)]
pub enum TransmitDrain {
    LocalUnsubmitted,
    Unknown,
    NoContribution,
    /// Reuse also requires elapsed lower-bound radio time past air_end, which conservatively includes an extra filter group delay.
    ResolvedTail(crate::tx_evidence::TransportResolution),
}

pub struct DedicatedTransmitSession {
    identity: ReceiveIdentity,
    anchor: FrameAnchor,
    plan: Arc<CompiledRel99>,
    transports: Vec<Transport>,
    composite: DownlinkComposite,
    physical: Vec<DownlinkDpch>,
    frames: Vec<Frame>,
    head: usize,
    queued: usize,
    next_prepare: ChipInstant,
    next_cfn: u8,
    phase: TransmitPhase,
    initial_tpc: Option<TpcInitialization>,
    algorithm2_hold_groups: u8,
    slot_chips: Vec<Complex32>,
    cached_slot: Option<ChipInstant>,
    cached_slot_gain: f32,
    downlink_power: Option<DownlinkPower>,
    resources: TransmitResources,
    counters: TransmitCounters,
    failure: Option<TransmitError>,
    failure_evidence: Option<TransmitFailureEvidence>,
    emissions: Vec<Option<Box<evidence::Emission>>>,
    emission_frames: u64,
    emission_pool: Option<Arc<()>>,
    emission_bound: bool,
    drain: TransmitDrain,
}

impl DedicatedTransmitSession {
    pub fn new(
        identity: ReceiveIdentity,
        anchor: FrameAnchor,
        plan: Arc<CompiledRel99>,
        amplitudes: DpchAmplitudes,
        limits: TransmitLimits,
    ) -> Result<Self, TransmitError> {
        let configured = plan.downlink_configured();
        let settings = plan.configuration().physical();
        let timing = DpchTiming::new(settings.frame_offset_256_chips)?;
        let longest = configured
            .transport_ids()
            .map(|id| {
                configured
                    .definition(id)
                    .expect("configured transport")
                    .interval
                    .frames()
            })
            .max()
            .ok_or(TransmitError::Mapping)?;
        // TS 25.211 7.1 and TS 25.212 4.2.14 constrain the physical offset and initial TTI boundary independently.
        if identity.ue.epoch != anchor.time.epoch
            || anchor.time.index % CHIPS_PER_FRAME != timing.chips()
            || !usize::from(anchor.cfn).is_multiple_of(longest)
        {
            return Err(TransmitError::Time);
        }
        if limits.frame_slots.get() > MAX_FRAME_SLOTS {
            return Err(TransmitError::Capacity);
        }
        let (format, capacity) = match plan.downlink() {
            DownlinkPlans::Fixed(plans) => (plans.physical(), plans.frame_capacity()),
            DownlinkPlans::Flexible(plans) => (plans.physical(), plans.frame_capacity()),
        };
        if capacity > limits.maximum_frame_bits.get() {
            return Err(TransmitError::Capacity);
        }
        let mut retained = capacity
            .checked_mul(limits.frame_slots.get())
            .and_then(|value| value.checked_add(capacity))
            .ok_or(TransmitError::Capacity)?;
        let mut transports = Vec::with_capacity(configured.transport_count());
        let mut encoder_instances = 0;
        for id in configured.transport_ids() {
            let definition = configured.definition(id).expect("configured transport");
            let mut encoders = Vec::with_capacity(definition.formats.len());
            let mut maximum = 0;
            for index in 0..definition.formats.len() {
                let used = configured
                    .combinations()
                    .iter()
                    .any(|row| row.formats[transports.len()] == index);
                let encoder = if !used {
                    None
                } else {
                    encoder_instances += 1;
                    Some(match plan.downlink() {
                        DownlinkPlans::Fixed(plans) => Encoder::Fixed(plans.encoder(id, index)?),
                        DownlinkPlans::Flexible(plans) => {
                            Encoder::Flexible(plans.encoder(id, index)?)
                        }
                    })
                };
                if let Some(encoder) = &encoder {
                    maximum = maximum.max(encoder.tti_bits());
                }
                encoders.push(encoder);
            }
            retained = retained
                .checked_add(maximum.checked_mul(2).ok_or(TransmitError::Capacity)?)
                .filter(|value| *value <= limits.maximum_retained_bits.get())
                .ok_or(TransmitError::Capacity)?;
            transports.push(Transport {
                id,
                frames: definition.interval.frames(),
                encoders,
                encoded: vec![TxBit::Dtx; maximum],
                flexible: vec![Bit::Zero; maximum],
                active_format: None,
                frame_bits: 0,
            });
        }
        if retained > limits.maximum_retained_bits.get() {
            return Err(TransmitError::Capacity);
        }
        let physical = settings
            .downlink_codes
            .iter()
            .enumerate()
            .map(|(index, &number)| {
                DownlinkDpch::new(
                    DownlinkScramblingCode::new(
                        PrimaryScramblingCode::new(settings.primary_scrambling)?,
                        settings.secondary_scrambling,
                    )?,
                    OvsfCode::new(settings.downlink_sf, number)?,
                    format,
                    timing,
                    if index == 0 {
                        DpchRole::Primary
                    } else {
                        DpchRole::Additional
                    },
                    amplitudes,
                )
            })
            .collect::<Result<Vec<_>, DspError>>()?;
        let ids: Vec<_> = configured.transport_ids().collect();
        let composite = DownlinkComposite::new(
            format,
            NonZeroUsize::new(physical.len()).ok_or(TransmitError::Mapping)?,
            &ids,
            limits.maximum_frame_bits,
        )?;
        let channels = physical.len();
        Ok(Self {
            identity,
            anchor,
            plan,
            transports,
            composite,
            physical,
            frames: (0..limits.frame_slots.get())
                .map(|_| Frame {
                    metadata: None,
                    data: vec![TxBit::Dtx; capacity],
                })
                .collect(),
            head: 0,
            queued: 0,
            next_prepare: anchor.time,
            next_cfn: anchor.cfn,
            phase: TransmitPhase::Prepared,
            initial_tpc: None,
            algorithm2_hold_groups: 0,
            slot_chips: vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            cached_slot: None,
            cached_slot_gain: 1.0,
            downlink_power: None,
            resources: TransmitResources {
                physical_frame_bits: capacity,
                retained_bits: retained,
                channels,
                encoder_instances,
            },
            counters: TransmitCounters::default(),
            failure: None,
            failure_evidence: None,
            emissions: (0..MAX_TRACKED_FRAMES).map(|_| None).collect(),
            emission_frames: 0,
            emission_pool: None,
            emission_bound: false,
            drain: TransmitDrain::LocalUnsubmitted,
        })
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub fn configure_downlink_power(
        &mut self,
        settings: DownlinkPowerSettings,
    ) -> Result<(), TransmitError> {
        if self.phase != TransmitPhase::Prepared || self.downlink_power.is_some() {
            return Err(TransmitError::Phase);
        }
        self.downlink_power = Some(DownlinkPower::new(settings)?);
        Ok(())
    }
    pub fn drain(&self) -> &TransmitDrain {
        &self.drain
    }
    pub fn configure_initial_tpc(
        &mut self,
        settings: TpcInitialization,
    ) -> Result<(), TransmitError> {
        if self.phase != TransmitPhase::Prepared
            || self.counters.prepared_frames != 0
            || self.queued != 0
            || self.initial_tpc.is_some()
        {
            return Err(TransmitError::Phase);
        }
        self.anchor
            .time
            .index
            .checked_add(u64::from(settings.maximum_slots.get()) * CHIPS_PER_SLOT)
            .ok_or(TransmitError::Time)?;
        self.initial_tpc = Some(settings);
        Ok(())
    }
    pub fn configure_algorithm2_hold_groups(&mut self, groups: u8) -> Result<(), TransmitError> {
        if self.phase != TransmitPhase::Prepared
            || self.counters.prepared_frames != 0
            || self.queued != 0
        {
            return Err(TransmitError::Phase);
        }
        if groups > runtime::grouped_tpc::MAXIMUM_HOLD_GROUPS {
            return Err(TransmitError::Payload);
        }
        self.algorithm2_hold_groups = groups;
        Ok(())
    }
    pub fn frame_anchor(&self) -> FrameAnchor {
        self.anchor
    }
    pub fn plan(&self) -> &Arc<CompiledRel99> {
        &self.plan
    }
    pub(crate) fn validate_emission(
        &self,
        emission: &evidence::Emission,
    ) -> Result<usize, TransmitError> {
        if emission.identity != self.identity || !Arc::ptr_eq(&emission.plan, &self.plan) {
            return Err(TransmitError::Identity);
        }
        if self.emission_bound && !same_pool(self.emission_pool.as_ref(), emission.pool()) {
            return Err(TransmitError::Identity);
        }
        if !emission.viable() || emission.start != self.next_prepare {
            return Err(TransmitError::Time);
        }
        let row = self
            .plan
            .downlink_configured()
            .combination(emission.tfci)
            .ok_or(TransmitError::Mapping)?;
        let mut frames = 1;
        for (transport, format) in self.transports.iter().zip(&row.formats) {
            if usize::from(self.next_cfn).is_multiple_of(transport.frames) {
                let block = self
                    .plan
                    .downlink_configured()
                    .block_plan(transport.id, *format)
                    .ok_or(TransmitError::Mapping)?;
                if block.coded_bits() != 0 {
                    frames = frames.max(transport.frames);
                }
            }
        }
        if emission.end.epoch != self.identity.ue.epoch
            || emission.end.index
                != emission
                    .start
                    .index
                    .checked_add(frames as u64 * CHIPS_PER_FRAME)
                    .ok_or(TransmitError::Time)?
        {
            return Err(TransmitError::Time);
        }
        self.emissions
            .iter()
            .position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|emission| emission.generated_state())
            })
            .or_else(|| self.emissions.iter().position(Option::is_none))
            .ok_or(TransmitError::Capacity)
    }
    fn fail_emissions(&mut self) {
        for emission in self.emissions.iter_mut().flatten() {
            emission.fail();
        }
    }
    pub(crate) fn reclaim_emissions(
        &mut self,
        pool: &Arc<()>,
        mut accept: impl FnMut(Box<evidence::Emission>) -> Result<(), Box<evidence::Emission>>,
    ) -> Result<usize, TransmitError> {
        if self.phase != TransmitPhase::Retired
            || self
                .emission_pool
                .as_ref()
                .is_some_and(|owner| !Arc::ptr_eq(owner, pool))
        {
            return Err(TransmitError::Identity);
        }
        let mut count = 0;
        for entry in &mut self.emissions {
            if let Some(emission) = entry.take() {
                if let Err(emission) = accept(emission) {
                    *entry = Some(emission);
                    return Err(TransmitError::Capacity);
                }
                count += 1;
            }
        }
        Ok(count)
    }
    fn complete_emissions(&mut self) {
        if self.emission_frames == self.counters.consumed_frames {
            return;
        }
        self.emission_frames = self.counters.consumed_frames;
        let end = self.anchor.time.index + self.emission_frames * CHIPS_PER_FRAME;
        for entry in &mut self.emissions {
            if entry
                .as_ref()
                .is_some_and(|emission| emission.end.index <= end && !emission.generated_state())
            {
                entry.as_mut().expect("completed emission").generated();
            }
        }
    }
    pub fn phase(&self) -> TransmitPhase {
        self.phase
    }
    pub fn resources(&self) -> TransmitResources {
        self.resources
    }
    pub fn counters(&self) -> TransmitCounters {
        self.counters
    }
    pub fn next_prepare(&self) -> ChipInstant {
        self.next_prepare
    }
    pub fn next_cfn(&self) -> u8 {
        self.next_cfn
    }
    pub fn queued_frames(&self) -> usize {
        self.queued
    }
    pub fn failure(&self) -> Option<&TransmitError> {
        self.failure.as_ref()
    }

    pub fn failure_evidence(&self) -> Option<&TransmitFailureEvidence> {
        self.failure_evidence.as_ref()
    }

    fn record_failure(
        &mut self,
        operation: TransmitOperation,
        render_cursor: ChipInstant,
        requested: ChipInstant,
        error: &TransmitError,
    ) {
        if self.failure_evidence.is_none() {
            self.failure_evidence = Some(TransmitFailureEvidence {
                identity: self.identity,
                operation,
                error: error.clone(),
                render_cursor,
                requested,
                anchor: self.anchor,
                next_prepare: self.next_prepare,
                phase: self.phase,
                counters: self.counters,
                initial_tpc_maximum_slots: self.initial_tpc.map(|settings| settings.maximum_slots),
            });
        }
    }

    fn capacity_retry_at(&self, cursor: ChipInstant) -> Option<ChipInstant> {
        let next_slot = cursor.index.checked_add(CHIPS_PER_SLOT)?;
        let released = if self.queued == self.frames.len() {
            self.frames[self.head]
                .metadata?
                .start
                .index
                .checked_add(CHIPS_PER_FRAME)?
        } else {
            next_slot
        };
        Some(ChipInstant {
            epoch: cursor.epoch,
            index: released.max(next_slot),
        })
    }

    pub fn prepare_frame(
        &mut self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        tfci: Tfci,
        payloads: &[TransportPayload<'_>],
    ) -> Result<PreparedFrame, TransmitError> {
        if identity != self.identity {
            return Err(TransmitError::Identity);
        }
        if !matches!(
            self.phase,
            TransmitPhase::Prepared
                | TransmitPhase::Staged { .. }
                | TransmitPhase::Armed
                | TransmitPhase::Scheduled { .. }
                | TransmitPhase::Draining { .. }
        ) {
            return Err(self.failure.clone().unwrap_or(TransmitError::Phase));
        }
        if let TransmitPhase::Draining { boundary, .. } = self.phase
            && start.index >= boundary.index
        {
            return Err(TransmitError::Time);
        }
        if start != self.next_prepare {
            return Err(TransmitError::Time);
        }
        if self.queued == self.frames.len() {
            return Err(TransmitError::Capacity);
        }
        let next = start
            .index
            .checked_add(CHIPS_PER_FRAME)
            .ok_or(TransmitError::Time)?;
        let row = self
            .plan
            .downlink_configured()
            .combination(tfci)
            .ok_or(TransmitError::Mapping)?;
        let mut input_index = 0;
        for (transport, &format) in self.transports.iter().zip(&row.formats) {
            if usize::from(self.next_cfn).is_multiple_of(transport.frames) {
                let input = payloads.get(input_index).ok_or(TransmitError::Payload)?;
                let block = self
                    .plan
                    .downlink_configured()
                    .block_plan(transport.id, format)
                    .ok_or(TransmitError::Mapping)?;
                if input.transport != transport.id || input.bits.len() != block.payload_bits() {
                    return Err(TransmitError::Payload);
                }
                input_index += 1;
            } else if transport.active_format != Some(format) {
                return Err(TransmitError::FormatChangedWithinTti);
            }
        }
        if input_index != payloads.len() {
            return Err(TransmitError::Payload);
        }
        let metadata = PreparedFrame {
            identity,
            start,
            cfn: self.next_cfn,
            tfci,
        };
        let frame = (self.head + self.queued) % self.frames.len();
        let result = self.encode_frame(tfci, payloads, frame);
        if let Err(error) = result {
            self.fail_emissions();
            self.phase = TransmitPhase::Failed;
            self.failure = Some(error.clone());
            return Err(error);
        }
        self.frames[frame].metadata = Some(metadata);
        self.next_prepare.index = next;
        self.next_cfn = self.next_cfn.wrapping_add(1);
        self.queued += 1;
        self.counters.prepared_frames = self.counters.prepared_frames.saturating_add(1);
        Ok(metadata)
    }

    fn encode_frame(
        &mut self,
        tfci: Tfci,
        payloads: &[TransportPayload<'_>],
        output: usize,
    ) -> Result<(), TransmitError> {
        let row = self
            .plan
            .downlink_configured()
            .combination(tfci)
            .expect("validated TFCI");
        let mut input_index = 0;
        for (transport, &format) in self.transports.iter_mut().zip(&row.formats) {
            if usize::from(self.next_cfn).is_multiple_of(transport.frames) {
                let encoder = transport.encoders[format]
                    .as_mut()
                    .expect("used format encoder");
                transport.frame_bits = encoder.frame_bits();
                let length = encoder.tti_bits();
                match encoder {
                    Encoder::Fixed(encoder) => {
                        encoder
                            .encode(payloads[input_index].bits, &mut transport.encoded[..length])?;
                    }
                    Encoder::Flexible(encoder) => {
                        encoder.encode(
                            payloads[input_index].bits,
                            &mut transport.flexible[..length],
                        )?;
                        for (output, &bit) in transport.encoded[..length]
                            .iter_mut()
                            .zip(&transport.flexible)
                        {
                            *output = TxBit::Data(bit);
                        }
                    }
                }
                transport.active_format = Some(format);
                input_index += 1;
            }
        }
        let dummy = TransportId::new(1).expect("valid transport identity");
        let mut frames: [DownlinkTransportFrame<'_, TxBit>; MAX_TRANSPORTS] =
            std::array::from_fn(|_| DownlinkTransportFrame {
                id: dummy,
                bits: &[],
            });
        for (frame, transport) in frames.iter_mut().zip(&self.transports) {
            let offset = usize::from(self.next_cfn) % transport.frames * transport.frame_bits;
            *frame = DownlinkTransportFrame {
                id: transport.id,
                bits: &transport.encoded[offset..offset + transport.frame_bits],
            };
        }
        self.composite.compose(
            &frames[..self.transports.len()],
            &mut self.frames[output].data,
        )?;
        Ok(())
    }

    fn arm(&mut self, now: ChipInstant) -> Result<(), TransmitError> {
        if self.phase != TransmitPhase::Prepared {
            return Err(TransmitError::Phase);
        }
        if now.epoch != self.anchor.time.epoch {
            return Err(TransmitError::Time);
        }
        if now.index > self.anchor.time.index {
            return Err(TransmitError::Late(TransmitTimingStage::Arm));
        }
        if self.queued == 0 {
            return Err(TransmitError::Underrun);
        }
        self.phase = TransmitPhase::Armed;
        Ok(())
    }

    fn preflight(&self, start: ChipInstant, end: u64) -> Result<(), TransmitError> {
        let end = self.synthesis_end(end);
        if !self.emits() || end <= self.anchor.time.index || end <= start.index {
            return Ok(());
        }
        let first = start.index.max(self.anchor.time.index);
        let covered = self.next_prepare.index;
        if self.queued == 0 || covered < end {
            return Err(TransmitError::Underrun);
        }
        let head = self.frames[self.head]
            .metadata
            .ok_or(TransmitError::Underrun)?;
        if first < head.start.index {
            return Err(TransmitError::Time);
        }
        Ok(())
    }
    fn emits(&self) -> bool {
        matches!(
            self.phase,
            TransmitPhase::Armed | TransmitPhase::Scheduled { .. } | TransmitPhase::Draining { .. }
        )
    }
    fn synthesis_end(&self, end: u64) -> u64 {
        match self.phase {
            TransmitPhase::Draining { boundary, .. } => end.min(boundary.index),
            _ => end,
        }
    }

    #[cfg(test)]
    fn mix(
        &mut self,
        start: ChipInstant,
        output: &mut [Complex32],
        tpc: &mut impl FnMut(TpcRequest) -> Bit,
    ) -> Result<(), TransmitError> {
        self.mix_checked(start, output, &mut |request| Ok(tpc(request)))
    }
    fn mix_checked(
        &mut self,
        start: ChipInstant,
        output: &mut [Complex32],
        tpc: &mut impl FnMut(TpcRequest) -> Result<Bit, TransmitError>,
    ) -> Result<(), TransmitError> {
        if !self.emits() {
            return Ok(());
        }
        let window_end = start
            .index
            .checked_add(output.len() as u64)
            .ok_or(TransmitError::Time)?;
        let end = self.synthesis_end(window_end);
        self.preflight(start, end)?;
        let mut at = start.index.max(self.anchor.time.index);
        while at < end {
            let frame = self.frames[self.head]
                .metadata
                .ok_or(TransmitError::Underrun)?;
            let frame_offset = at - frame.start.index;
            let slot = (frame_offset / CHIPS_PER_SLOT) as u8;
            let slot_start = frame.start.index + u64::from(slot) * CHIPS_PER_SLOT;
            let slot_time = ChipInstant {
                epoch: start.epoch,
                index: slot_start,
            };
            if self.cached_slot != Some(slot_time) {
                self.slot_chips.fill(Complex32::default());
                let command = tpc(TpcRequest {
                    identity: self.identity,
                    physical_slot_start: slot_time,
                    synthesis_at: start,
                })?;
                let bits_per_slot = self.composite.format().data_bits();
                let bits_per_frame = bits_per_slot * SLOTS_PER_FRAME as usize;
                for (index, physical) in self.physical.iter().enumerate() {
                    let first = index * bits_per_frame + usize::from(slot) * bits_per_slot;
                    physical.add_slot(
                        slot,
                        &self.frames[self.head].data[first..first + bits_per_slot],
                        (index == 0 && self.composite.format().tfci()).then_some(frame.tfci),
                        command,
                        &mut self.slot_chips,
                    )?;
                }
                self.cached_slot_gain = self
                    .downlink_power
                    .as_mut()
                    .map_or(1.0, |power| power.latch_gain(slot_time));
                if let Some(power) = &self.downlink_power {
                    self.counters.downlink_power = power.counters;
                }
                self.cached_slot = Some(slot_time);
                self.counters.physical_slots = self.counters.physical_slots.saturating_add(1);
            }
            let until = end.min(slot_start + CHIPS_PER_SLOT);
            let count = (until - at) as usize;
            let destination = (at - start.index) as usize;
            let source = (at - slot_start) as usize;
            for (output, &chip) in output[destination..destination + count]
                .iter_mut()
                .zip(&self.slot_chips[source..source + count])
            {
                *output += chip * self.cached_slot_gain;
            }
            self.counters.mixed_chips = self.counters.mixed_chips.saturating_add(count as u64);
            at = until;
            if at == frame.start.index + CHIPS_PER_FRAME {
                self.frames[self.head].metadata = None;
                self.head = (self.head + 1) % self.frames.len();
                self.queued -= 1;
                self.counters.consumed_frames = self.counters.consumed_frames.saturating_add(1);
            }
        }
        if let TransmitPhase::Draining { next, boundary } = self.phase
            && window_end >= boundary.index
        {
            if self.queued != 0 {
                return Err(TransmitError::Underrun);
            }
            self.phase = TransmitPhase::Stopped { next, boundary };
        }
        Ok(())
    }
}

fn same_pool(left: Option<&Arc<()>>, right: Option<&Arc<()>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

pub struct DedicatedTransmitBank {
    next: ChipInstant,
    primary: PrimaryScramblingCode,
    reserved: Vec<OvsfCode>,
    sessions: Vec<Option<Box<DedicatedTransmitSession>>>,
    generations: Vec<Option<ReceiveIdentity>>,
    maximum_retained_bits: usize,
    retained_bits: usize,
    maximum_channels: usize,
    channels: usize,
    contribution: Vec<Complex32>,
}

impl DedicatedTransmitBank {
    /// The owner count bounds UE slots. Each UE may retain two separately charged configurations.
    pub fn new(
        information: &CellInformation,
        next: ChipInstant,
        maximum_owners: NonZeroUsize,
        maximum_channels: NonZeroUsize,
        maximum_retained_bits: NonZeroUsize,
    ) -> Result<Self, TransmitError> {
        if !next.index.is_multiple_of(CHIPS_PER_SLOT) {
            return Err(TransmitError::Time);
        }
        if maximum_owners.get() > MAX_OWNERS {
            return Err(TransmitError::Capacity);
        }
        let reserved = common_codes(information).collect();
        Ok(Self {
            next,
            primary: information.primary_code(),
            reserved,
            sessions: (0..maximum_owners.get() * CONFIGURATIONS_PER_UE)
                .map(|_| None)
                .collect(),
            generations: vec![None; maximum_owners.get()],
            maximum_retained_bits: maximum_retained_bits.get(),
            retained_bits: 0,
            maximum_channels: maximum_channels.get(),
            channels: 0,
            contribution: vec![Complex32::default(); CHIPS_PER_SLOT as usize],
        })
    }
    pub fn next_chip(&self) -> ChipInstant {
        self.next
    }
    pub fn primary_code(&self) -> PrimaryScramblingCode {
        self.primary
    }
    pub(crate) fn matches_cell(&self, information: &CellInformation) -> bool {
        self.primary == information.primary_code()
            && common_codes(information).eq(self.reserved.iter().copied())
    }
    pub fn session(&self, identity: ReceiveIdentity) -> Option<&DedicatedTransmitSession> {
        self.sessions
            .iter()
            .flatten()
            .find(|session| session.identity == identity)
            .map(Box::as_ref)
    }
    fn session_mut(&mut self, identity: ReceiveIdentity) -> Option<&mut DedicatedTransmitSession> {
        self.sessions
            .iter_mut()
            .flatten()
            .find(|session| session.identity == identity)
            .map(Box::as_mut)
    }
    pub fn insert(
        &mut self,
        session: DedicatedTransmitSession,
    ) -> Result<(), (TransmitError, Box<DedicatedTransmitSession>)> {
        self.insert_boxed(Box::new(session))
    }
    pub fn insert_boxed(
        &mut self,
        session: Box<DedicatedTransmitSession>,
    ) -> Result<(), (TransmitError, Box<DedicatedTransmitSession>)> {
        self.insert_prepared(None, session)
    }
    fn insert_prepared(
        &mut self,
        previous: Option<ReceiveIdentity>,
        mut session: Box<DedicatedTransmitSession>,
    ) -> Result<(), (TransmitError, Box<DedicatedTransmitSession>)> {
        if let Err(error) = self.admit(previous, &session) {
            return Err((error, session));
        }
        if let Some(previous) = previous {
            session.phase = TransmitPhase::Staged { previous };
        }
        let slot = self
            .sessions
            .iter()
            .position(Option::is_none)
            .expect("admitted owner slot");
        let generation_slot = self
            .generations
            .iter()
            .position(|old| old.is_some_and(|old| old.ue.slot == session.identity.ue.slot))
            .or_else(|| self.generations.iter().position(Option::is_none))
            .expect("admitted generation slot");
        self.generations[generation_slot] = Some(session.identity);
        self.retained_bits += session.resources.retained_bits;
        self.channels += session.resources.channels;
        session.drain = TransmitDrain::Unknown;
        self.sessions[slot] = Some(session);
        Ok(())
    }
    fn admit(
        &self,
        previous: Option<ReceiveIdentity>,
        session: &DedicatedTransmitSession,
    ) -> Result<(), TransmitError> {
        if session.phase != TransmitPhase::Prepared {
            return Err(TransmitError::Phase);
        }
        let identity = session.identity;
        if identity.ue.epoch != self.next.epoch
            || session.plan.configuration().physical().primary_scrambling != self.primary.index()
        {
            return Err(TransmitError::Identity);
        }
        if session.anchor.time.index < self.next.index {
            return Err(TransmitError::Late(TransmitTimingStage::Admission));
        }
        if self.generations.iter().flatten().any(|old| {
            old.ue.slot == identity.ue.slot
                && (old.ue.generation > identity.ue.generation
                    || old.ue.generation == identity.ue.generation
                        && old.configuration >= identity.configuration)
        }) {
            return Err(TransmitError::Identity);
        }
        if !self.sessions.iter().any(Option::is_none)
            || !self
                .generations
                .iter()
                .any(|old| old.is_none_or(|old| old.ue.slot == identity.ue.slot))
            || self
                .retained_bits
                .checked_add(session.resources.retained_bits)
                .is_none_or(|value| value > self.maximum_retained_bits)
            || self
                .channels
                .checked_add(session.resources.channels)
                .is_none_or(|value| value > self.maximum_channels)
        {
            return Err(TransmitError::Capacity);
        }
        if let Some(previous) = previous {
            self.validate_replacement(previous, session)?;
            if self
                .sessions
                .iter()
                .flatten()
                .filter(|old| old.identity.ue.slot == identity.ue.slot)
                .count()
                >= CONFIGURATIONS_PER_UE
            {
                return Err(TransmitError::Capacity);
            }
        } else if self
            .sessions
            .iter()
            .flatten()
            .any(|old| old.identity.ue.slot == identity.ue.slot)
        {
            return Err(TransmitError::Conflict);
        }
        let settings = session.plan.configuration().physical();
        for &number in &settings.downlink_codes {
            let code = OvsfCode::new(settings.downlink_sf, number)?;
            if (settings.secondary_scrambling.is_none()
                && self.reserved.iter().any(|other| code.overlaps(*other)))
                || self.sessions.iter().flatten().any(|old| {
                    if previous == Some(old.identity) {
                        return false;
                    }
                    let other = old.plan.configuration().physical();
                    other.secondary_scrambling == settings.secondary_scrambling
                        && other.downlink_codes.iter().any(|&number| {
                            code.overlaps(
                                OvsfCode::new(other.downlink_sf, number).expect("validated code"),
                            )
                        })
                })
            {
                return Err(TransmitError::Conflict);
            }
        }
        Ok(())
    }
    pub fn arm(&mut self, identity: ReceiveIdentity) -> Result<(), TransmitError> {
        let now = self.next;
        self.session_mut(identity)
            .ok_or(TransmitError::Identity)?
            .arm(now)
    }
    pub fn prepare_frame(
        &mut self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        tfci: Tfci,
        payloads: &[TransportPayload<'_>],
    ) -> Result<PreparedFrame, TransmitError> {
        if start.epoch != self.next.epoch {
            return Err(TransmitError::Time);
        }
        if start.index < self.next.index {
            return Err(TransmitError::Late(TransmitTimingStage::Frame));
        }
        self.session_mut(identity)
            .ok_or(TransmitError::Identity)?
            .prepare_frame(identity, start, tfci, payloads)
    }
    pub fn remove(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<DedicatedTransmitSession, TransmitError> {
        self.remove_boxed(identity).map(|session| *session)
    }
    pub fn remove_boxed(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<Box<DedicatedTransmitSession>, TransmitError> {
        let slot = self
            .sessions
            .iter()
            .position(|session| {
                session
                    .as_ref()
                    .is_some_and(|session| session.identity == identity)
            })
            .ok_or(TransmitError::Identity)?;
        let mut session = self.sessions[slot].take().expect("matched owner");
        session.fail_emissions();
        session.phase = TransmitPhase::Retired;
        self.retained_bits -= session.resources.retained_bits;
        self.channels -= session.resources.channels;
        Ok(session)
    }
    pub fn mix_slot(
        &mut self,
        start: ChipInstant,
        output: &mut [Complex32],
        mut tpc: impl FnMut(TpcRequest) -> Bit,
    ) -> Result<usize, TransmitError> {
        self.mix_slot_checked(start, output, |request| Ok(tpc(request)))
    }
    pub(crate) fn mix_slot_checked(
        &mut self,
        start: ChipInstant,
        output: &mut [Complex32],
        mut tpc: impl FnMut(TpcRequest) -> Result<Bit, TransmitError>,
    ) -> Result<usize, TransmitError> {
        if start != self.next || output.len() != CHIPS_PER_SLOT as usize {
            return Err(TransmitError::Time);
        }
        let next = start
            .index
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(TransmitError::Time)?;
        let mut failed = 0;
        // Boost is limited to the dedicated power the other sessions leave unused.
        let budget = self.generations.len() as f32;
        let used: f32 = self
            .sessions
            .iter()
            .flatten()
            .filter(|session| session.emits())
            .map(|session| {
                session
                    .downlink_power
                    .as_ref()
                    .map_or(1.0, DownlinkPower::level)
            })
            .sum();
        for session in self.sessions.iter_mut().flatten() {
            if !session.emits() || next <= session.anchor.time.index {
                continue;
            }
            if let Some(power) = session.downlink_power.as_mut() {
                let available = (budget - (used - power.level())).max(1.0);
                power.limit_boost((10.0 * available.log10()).floor() as u8);
            }
            if let Err(error) = session.preflight(start, next) {
                session.record_failure(TransmitOperation::Synthesis, start, start, &error);
                session.phase = TransmitPhase::Failed;
                if error == TransmitError::Underrun {
                    session.counters.underruns = session.counters.underruns.saturating_add(1);
                }
                session.failure = Some(error);
                session.fail_emissions();
                failed += 1;
                continue;
            }
            self.contribution.fill(Complex32::default());
            if let Err(error) = session.mix_checked(start, &mut self.contribution, &mut tpc) {
                session.record_failure(TransmitOperation::Synthesis, start, start, &error);
                session.phase = TransmitPhase::Failed;
                session.failure = Some(error);
                session.fail_emissions();
                failed += 1;
            } else {
                for (output, &chip) in output.iter_mut().zip(&self.contribution) {
                    *output += chip;
                }
                session.complete_emissions();
            }
        }
        self.next.index = next;
        Ok(failed)
    }
}

fn common_codes(information: &CellInformation) -> impl Iterator<Item = OvsfCode> + '_ {
    [
        OvsfCode::new(256, 0).expect("primary CPICH code"),
        OvsfCode::new(256, 1).expect("primary CCPCH code"),
    ]
    .into_iter()
    .chain(
        information
            .sib5()
            .secondary_channels()
            .iter()
            .flat_map(|secondary| {
                std::iter::once(secondary.settings().code).chain(
                    secondary.settings().pich.as_ref().map(|paging| {
                        OvsfCode::new(256, u16::from(paging.code256)).expect("validated PICH code")
                    }),
                )
            }),
    )
    .chain(information.sib5().prach().iter().map(|access| {
        OvsfCode::new(256, u16::from(access.settings().aich.code256)).expect("validated AICH code")
    }))
}
