// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    collections::VecDeque,
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::{
    DspError,
    bits::Bit,
    coding::DecodeWork,
    dedicated_transport::{DedicatedUplink, TransportId, UplinkDemultiplexer, UplinkTfcs},
    tfci::Tfci,
    transport_blocks::BlockOutcome,
};
use umts_types::{
    UeKey,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

mod chips;
mod control_transition;
mod dch_selection;
pub use dch_selection::PreparedDchSelection;
mod feedback;
mod hsupa_attachment;
mod outer_loop;
mod output;
mod power_control;
mod sample_runtime;
mod samples;
mod shard;
mod tti;
pub(crate) use chips::maximum_physical_bits;
pub use chips::{
    ChipProgress, ChipReceiveCounters, ChipReceiveSettings, DedicatedChipReceiver, TfciAcceptance,
};
pub use control_transition::{ControlSearchGeometry, PreparedControlSearch};
pub use feedback::{DedicatedFeedback, PilotTpcCounters, PilotTpcStatus, RejectedPilotObservation};
pub use hsupa_attachment::HsupaAttachment;
pub use outer_loop::{LinkCrcObservation, LinkPowerRegistry, OuterLoopSettings, OuterLoopStatus};
pub use power_control::{
    PilotFeedback, PilotObservation, PilotSirTarget, PilotTpcController, PilotTpcSettings,
    PowerControlError, TpcProvenance,
};
#[cfg(test)]
pub use sample_runtime::ReceivePlanPhase;
pub use sample_runtime::{
    DedicatedReceivePort, DedicatedSampleRuntime, HsupaReceiveSource, ReceiveCommandResult,
    ReceiveCommandTicket, ReceivePlanStatus, RetiredPilotFeedback, SampleRuntimeCounters,
    SampleRuntimeFailure, SampleRuntimeLimits,
};
pub(crate) use samples::storage_requirements;
pub use samples::{
    DedicatedSampleReceiver, RadioLinkSyncSettings, RadioLinkSyncState, SampleProgress,
    SampleReceiveCounters, SampleReceiveSettings,
};
pub use shard::{DedicatedRxShard, ReceiveOwner, ShardLimits};

const MAX_OUTPUT_SLOTS: usize = 256;
const MAX_FORMATS: usize = 32;
pub const MAX_TRANSPORT_CHANNELS: usize = 32;
const CFN_MODULUS: u64 = 256;
const DECODE_STATE_UPDATES_PER_SERVICE: usize = 96 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveIdentity {
    pub ue: UeKey,
    pub configuration: NonZeroU64,
}
#[derive(Clone, Copy, Debug)]
pub struct FrameAnchor {
    pub time: ChipInstant,
    pub cfn: u8,
}
#[derive(Clone, Copy, Debug)]
pub struct ReceiveLimits {
    pub output_slots: NonZeroUsize,
    pub maximum_output_bits: NonZeroUsize,
    pub maximum_output_bytes: usize,
    pub maximum_coded_bits: NonZeroUsize,
    pub maximum_frame_bits: NonZeroUsize,
    pub maximum_decodes_per_step: NonZeroUsize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveResources {
    pub physical_sample_bytes: usize,
    pub coded_bits: usize,
    pub output_bytes: usize,
    pub demultiplexed_bits: usize,
    pub channels: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveResource {
    PhysicalSampleBytes,
    CodedBits,
    OutputBytes,
    DemultiplexedBits,
    Channels,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    Identity,
    Configuration,
    Capacity,
    ResourceBudget {
        resource: ReceiveResource,
        required: usize,
        maximum: usize,
    },
    ResourceOverflow(ReceiveResource),
    HistoryCapacity {
        required_samples: usize,
        available_samples: usize,
    },
    OutputWidth {
        required_bits: usize,
        maximum_bits: usize,
    },
    Time,
    FormatChangedWithinTti,
    Occupied,
    Retired,
    Failed,
    PowerControl(PowerControlError),
    Feedback(crate::dedicated_tx::TransmitError),
    FeedbackClosed,
    Hsupa(crate::hsupa_rx::ReceiveError),
    HsupaOutputClosed,
    Dsp(DspError),
}
impl From<DspError> for ReceiveError {
    fn from(error: DspError) -> Self {
        Self::Dsp(error)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveCounters {
    pub frames: u64,
    pub missing_frames: u64,
    pub interrupted_ttis: u64,
    pub skipped_ttis: u64,
    pub decoded: u64,
    pub absent: u64,
    pub erased: u64,
    pub failed_crc: u64,
    pub invalid_filler: u64,
    pub delivered: u64,
    pub retired_results: u64,
    pub retired_completed_ttis: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportReceiveCounters {
    pub nonempty: u64,
    pub decoded: u64,
    pub absent: u64,
    pub erased: u64,
    pub failed_crc: u64,
    pub invalid_filler: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedTransport {
    pub identity: ReceiveIdentity,
    pub transport: TransportId,
    pub format: usize,
    pub start: ChipInstant,
    pub block_bits: usize,
    pub blocks: usize,
    pub crc_checked: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct TransportDecodeObservation {
    pub transport: DecodedTransport,
    pub outcome: BlockOutcome,
    pub crc_counts: umts_dsp::transport_blocks::CrcCounts,
}
#[derive(Clone, Copy, Debug)]
pub enum UplinkDecodeObservation {
    Dch(TransportDecodeObservation),
    Edch {
        identity: ReceiveIdentity,
        start: ChipInstant,
        control: umts_dsp::hsupa::ControlDecision,
        presence_log_likelihood: Option<f64>,
        feedback: umts_mac::hsupa::Reception,
        outcome: BlockOutcome,
    },
}
pub struct TransportDelivery<'a> {
    pub decoded: DecodedTransport,
    pub payload: &'a [Bit],
}
struct OutputSlot {
    decoded: Option<DecodedTransport>,
    payload: Vec<Bit>,
}
#[derive(Clone, Copy)]
struct ActiveTti {
    format: usize,
}
struct Channel {
    id: TransportId,
    frames: usize,
    lookup: [Option<usize>; MAX_FORMATS],
    receiver: DedicatedUplink,
    active: Option<ActiveTti>,
    selected: usize,
    decoding: Option<ActiveTti>,
    counters: TransportReceiveCounters,
}

pub struct DedicatedReceiveSession {
    identity: ReceiveIdentity,
    anchor: FrameAnchor,
    tfcs: Arc<UplinkTfcs>,
    demultiplexer: UplinkDemultiplexer,
    channels: Vec<Channel>,
    output: Vec<OutputSlot>,
    head: usize,
    queued: usize,
    service_cursor: usize,
    decode_cursor: usize,
    decode_order: VecDeque<usize>,
    decode_budget: usize,
    last: Option<ChipInstant>,
    failed: bool,
    retired: bool,
    resources: ReceiveResources,
    counters: ReceiveCounters,
}
impl DedicatedReceiveSession {
    pub(crate) fn pending_output_storage_bytes(
        configured: &umts_dsp::dedicated_transport::ConfiguredTfcs,
    ) -> Result<usize, ReceiveError> {
        configured.transport_ids().try_fold(0usize, |bytes, id| {
            let definition = configured
                .definition(id)
                .ok_or(ReceiveError::Configuration)?;
            let maximum = definition
                .formats
                .iter()
                .try_fold(0usize, |maximum, format| {
                    format
                        .block_bits
                        .checked_mul(format.blocks)
                        .map(|v| maximum.max(v))
                })
                .ok_or(ReceiveError::Capacity)?;
            maximum
                .checked_mul(std::mem::size_of::<Bit>())
                .and_then(|v| v.checked_add(std::mem::size_of::<usize>()))
                .and_then(|v| bytes.checked_add(v))
                .ok_or(ReceiveError::Capacity)
        })
    }
    pub(crate) fn output_storage_bytes(
        slots: NonZeroUsize,
        maximum_payload_bits: usize,
    ) -> Result<usize, ReceiveError> {
        maximum_payload_bits
            .checked_mul(std::mem::size_of::<Bit>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<OutputSlot>()))
            .and_then(|bytes| bytes.checked_mul(slots.get()))
            .ok_or(ReceiveError::Capacity)
    }

    pub fn new(
        identity: ReceiveIdentity,
        anchor: FrameAnchor,
        tfcs: Arc<UplinkTfcs>,
        limits: ReceiveLimits,
    ) -> Result<Self, ReceiveError> {
        if identity.ue.epoch != anchor.time.epoch {
            return Err(ReceiveError::Identity);
        }
        let config = tfcs.configured();
        let longest = config
            .transport_ids()
            .map(|id| {
                config
                    .definition(id)
                    .expect("configured transport")
                    .interval
                    .frames()
            })
            .max()
            .ok_or(ReceiveError::Configuration)?;
        // TS 25.212 4.2.14 coordinates new CCTrCH activation at CFN mod Fmax = 0.
        if !usize::from(anchor.cfn).is_multiple_of(longest) {
            return Err(ReceiveError::Time);
        }
        anchor
            .time
            .index
            .checked_add(CHIPS_PER_FRAME * longest as u64)
            .ok_or(ReceiveError::Time)?;
        let maximum_payload = config
            .transport_ids()
            .flat_map(|id| {
                config
                    .definition(id)
                    .expect("configured transport")
                    .formats
                    .iter()
            })
            .try_fold(0_usize, |maximum, format| {
                format
                    .block_bits
                    .checked_mul(format.blocks)
                    .map(|bits| bits.max(maximum))
            })
            .ok_or(ReceiveError::Capacity)?;
        let bytes = Self::output_storage_bytes(limits.output_slots, maximum_payload)?
            .checked_add(Self::pending_output_storage_bytes(config)?)
            .ok_or(ReceiveError::Capacity)?;
        if maximum_payload > limits.maximum_output_bits.get()
            || bytes > limits.maximum_output_bytes
            || limits.output_slots.get() > MAX_OUTPUT_SLOTS
            || config.total_coded_bits() > limits.maximum_coded_bits.get()
        {
            return Err(ReceiveError::Capacity);
        }
        let demultiplexer = UplinkDemultiplexer::new(&tfcs, limits.maximum_frame_bits)?;
        let mut channels = Vec::with_capacity(config.transport_count());
        for id in config.transport_ids() {
            let definition = config.definition(id).expect("configured transport");
            let mut lookup = [None; MAX_FORMATS];
            let mut plans = Vec::new();
            for (format, entry) in lookup.iter_mut().enumerate().take(definition.formats.len()) {
                if let Some(plan) = tfcs.receiver_plan(id, format) {
                    *entry = Some(plans.len());
                    plans.push(Arc::clone(plan));
                }
            }
            let receiver = DedicatedUplink::for_plans(plans)?;
            channels.push(Channel {
                id,
                frames: definition.interval.frames(),
                lookup,
                receiver,
                active: None,
                selected: 0,
                decoding: None,
                counters: TransportReceiveCounters::default(),
            });
        }
        let resources = ReceiveResources {
            physical_sample_bytes: 0,
            coded_bits: config.total_coded_bits(),
            output_bytes: bytes,
            demultiplexed_bits: demultiplexer.maximum_frame_bits(),
            channels: channels.len(),
        };
        let decode_order = VecDeque::with_capacity(channels.len());
        Ok(Self {
            identity,
            anchor,
            tfcs,
            demultiplexer,
            channels,
            output: (0..limits.output_slots.get())
                .map(|_| OutputSlot {
                    decoded: None,
                    payload: vec![Bit::Zero; maximum_payload],
                })
                .collect(),
            head: 0,
            queued: 0,
            service_cursor: 0,
            decode_cursor: 0,
            decode_order,
            decode_budget: limits.maximum_decodes_per_step.get(),
            last: None,
            failed: false,
            retired: false,
            resources,
            counters: ReceiveCounters::default(),
        })
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub fn frame_anchor(&self) -> FrameAnchor {
        self.anchor
    }
    pub fn configured(&self) -> &Arc<umts_dsp::dedicated_transport::ConfiguredTfcs> {
        self.tfcs.configured()
    }
    pub fn resources(&self) -> ReceiveResources {
        self.resources
    }
    pub fn counters(&self) -> ReceiveCounters {
        self.counters
    }

    pub fn transport_counters(
        &self,
    ) -> [Option<(TransportId, TransportReceiveCounters)>; MAX_TRANSPORT_CHANNELS] {
        let mut counters = [None; MAX_TRANSPORT_CHANNELS];
        for (entry, channel) in counters.iter_mut().zip(&self.channels) {
            *entry = Some((channel.id, channel.counters));
        }
        counters
    }
    pub fn queued(&self) -> usize {
        self.queued
    }
    pub fn pending_completed_ttis(&self) -> usize {
        self.channels
            .iter()
            .map(|channel| {
                usize::from(channel.receiver.ready_start().is_some())
                    + channel.receiver.pending_decode_count()
            })
            .sum()
    }
    pub(super) fn discard_incomplete_ttis(&mut self) {
        for channel in &mut self.channels {
            if channel.receiver.discard_incomplete() {
                self.counters.interrupted_ttis = self.counters.interrupted_ttis.saturating_add(1);
            }
            channel.active = None;
        }
    }

    /// Physical inputs are ordered, second-interleaved PhCH soft bits with comparable calibrated reliability.
    pub fn receive(
        &mut self,
        identity: ReceiveIdentity,
        time: ChipInstant,
        cfn: u8,
        tfci: Tfci,
        physical: &[&[f64]],
    ) -> Result<(), ReceiveError> {
        if identity != self.identity {
            return Err(ReceiveError::Identity);
        }
        if self.retired {
            return Err(ReceiveError::Retired);
        }
        if self.failed {
            return Err(ReceiveError::Failed);
        }
        if time.epoch != self.anchor.time.epoch
            || time.index < self.anchor.time.index
            || !(time.index - self.anchor.time.index).is_multiple_of(CHIPS_PER_FRAME)
            || self.last.is_some_and(|last| time.index <= last.index)
        {
            return Err(ReceiveError::Time);
        }
        let frame = (time.index - self.anchor.time.index) / CHIPS_PER_FRAME;
        if self.anchor.cfn.wrapping_add((frame % CFN_MODULUS) as u8) != cfn {
            return Err(ReceiveError::Time);
        }
        let span = self.last.map_or(frame, |last| {
            (time.index - last.index) / CHIPS_PER_FRAME - 1
        });
        let combination = self
            .tfcs
            .configured()
            .combination(tfci)
            .ok_or(ReceiveError::Configuration)?;
        if span == 0
            && self
                .channels
                .iter()
                .zip(&combination.formats)
                .any(|(channel, &format)| {
                    channel.active.is_some_and(|active| active.format != format)
                })
        {
            return Err(ReceiveError::FormatChangedWithinTti);
        }
        let allocation = self
            .tfcs
            .allocation(tfci)
            .ok_or(ReceiveError::Configuration)?;
        // This numerical bound prevents repeated-evidence overflow. It is not an RF amplitude threshold.
        let maximum_soft = f64::MAX / (self.demultiplexer.maximum_frame_bits() as f64 + 1.0);
        if physical
            .iter()
            .any(|input| input.iter().any(|value| value.abs() > maximum_soft))
        {
            return Err(ReceiveError::Dsp(DspError::NonFiniteEvidence));
        }
        let frame = self.demultiplexer.receive(allocation, physical)?;
        self.counters.missing_frames = self.counters.missing_frames.saturating_add(span);
        for (channel, (&format, matched)) in self
            .channels
            .iter_mut()
            .zip(combination.formats.iter().zip(allocation.channels()))
        {
            if span != 0 {
                if channel.receiver.discard_incomplete() {
                    self.counters.interrupted_ttis =
                        self.counters.interrupted_ttis.saturating_add(1);
                }
                channel.active = None;
            }
            if usize::from(cfn).is_multiple_of(channel.frames) {
                if channel.receiver.ready_start().is_some() {
                    self.counters.skipped_ttis = self.counters.skipped_ttis.saturating_add(1);
                    channel.active = None;
                    continue;
                }
                let selection =
                    channel.lookup[format].expect("selected format has a compiled plan");
                let start = channel
                    .receiver
                    .select(selection)
                    .and_then(|()| channel.receiver.begin(time, cfn));
                if let Err(error) = start {
                    self.failed = true;
                    return Err(error.into());
                }
                channel.selected = format;
                channel.active = Some(ActiveTti { format });
            }
            if channel.active.is_none() {
                continue;
            }
            if let Err(error) = channel.receiver.receive(
                time,
                *matched,
                frame.get(channel.id).expect("configured transport range"),
            ) {
                self.failed = true;
                return Err(error.into());
            }
            if channel.receiver.ready_start().is_some() {
                channel.active = None;
            }
        }
        self.last = Some(time);
        self.counters.frames = self.counters.frames.saturating_add(1);
        Ok(())
    }

    /// Completes selected transports. Real-time sample owners must supply a work budget instead.
    #[cfg(test)]
    pub fn service(&mut self) -> Result<usize, ReceiveError> {
        self.service_with_work(&mut DecodeWork::new(NonZeroUsize::MAX))
    }
    pub(crate) fn decode_work_budget() -> DecodeWork {
        DecodeWork::new(
            NonZeroUsize::new(DECODE_STATE_UPDATES_PER_SERVICE).expect("nonzero decode budget"),
        )
    }
    #[cfg(test)]
    pub fn service_with_work(&mut self, work: &mut DecodeWork) -> Result<usize, ReceiveError> {
        self.service_with_observer(work, |_| ())
    }
    pub fn service_with_observer(
        &mut self,
        work: &mut DecodeWork,
        mut observe: impl FnMut(TransportDecodeObservation),
    ) -> Result<usize, ReceiveError> {
        if self.failed {
            return Err(ReceiveError::Failed);
        }
        if self.retired {
            return Err(ReceiveError::Retired);
        }
        let mut admitted = 0;
        for _ in 0..self.channels.len() {
            let index = self.service_cursor;
            self.service_cursor = (self.service_cursor + 1) % self.channels.len();
            let channel = &mut self.channels[index];
            if channel.decoding.is_none() {
                match channel.receiver.begin_decode() {
                    Ok(true) => {
                        channel.decoding = Some(ActiveTti {
                            format: channel.selected,
                        });
                        assert!(self.decode_order.len() < self.decode_order.capacity());
                        self.decode_order.push_back(index);
                        admitted += 1;
                    }
                    Ok(false) => (),
                    Err(error) => {
                        self.failed = true;
                        return Err(error.into());
                    }
                }
            }
            if admitted == self.decode_budget {
                break;
            }
        }
        let mut next_cursor = None;
        for offset in 0..self.channels.len() {
            let index = (self.decode_cursor + offset) % self.channels.len();
            let channel = &mut self.channels[index];
            let before = work.remaining();
            if channel.decoding.is_some()
                && let Err(error) = channel.receiver.decode_work(work)
            {
                self.failed = true;
                return Err(error.into());
            }
            if next_cursor.is_none() && work.remaining() < before {
                next_cursor = Some((index + 1) % self.channels.len());
            }
        }
        self.decode_cursor = next_cursor.unwrap_or(self.decode_cursor);
        let mut decoded = 0;
        for _ in 0..self.decode_budget {
            let Some(index) = self.decode_order.front().copied() else {
                break;
            };
            let channel = &mut self.channels[index];
            let Some(completed) = channel.receiver.completed_decode() else {
                break;
            };
            let format_index = channel.decoding.expect("queued decode").format;
            let start = completed.start;
            let format = self
                .tfcs
                .configured()
                .definition(channel.id)
                .expect("configured transport")
                .formats[format_index];
            let bits = format.block_bits * format.blocks;
            if self.queued == self.output.len() && format.blocks != 0 {
                break;
            }
            let tail = (self.head + self.queued) % self.output.len();
            let outcome = completed.outcome;
            let crc_counts = completed.crc_counts;
            let transport = DecodedTransport {
                identity: self.identity,
                transport: channel.id,
                format: format_index,
                start,
                block_bits: format.block_bits,
                blocks: format.blocks,
                crc_checked: format.crc.width() != 0,
            };
            decoded += 1;
            if bits != 0 {
                channel.counters.nonempty = channel.counters.nonempty.saturating_add(1);
            }
            match outcome {
                BlockOutcome::Decoded => {
                    self.output[tail].payload[..bits].copy_from_slice(completed.payload);
                    self.output[tail].decoded = Some(transport);
                    self.queued += 1;
                    self.counters.decoded = self.counters.decoded.saturating_add(1);
                    channel.counters.decoded = channel.counters.decoded.saturating_add(1);
                }
                BlockOutcome::Absent => {
                    self.counters.absent = self.counters.absent.saturating_add(1);
                    channel.counters.absent = channel.counters.absent.saturating_add(1);
                }
                BlockOutcome::Erased => {
                    self.counters.erased = self.counters.erased.saturating_add(1);
                    channel.counters.erased = channel.counters.erased.saturating_add(1);
                }
                BlockOutcome::InvalidFiller => {
                    self.counters.invalid_filler = self.counters.invalid_filler.saturating_add(1);
                    channel.counters.invalid_filler =
                        channel.counters.invalid_filler.saturating_add(1);
                }
                BlockOutcome::InvalidCrc { .. } => {
                    self.counters.failed_crc = self.counters.failed_crc.saturating_add(1);
                    channel.counters.failed_crc = channel.counters.failed_crc.saturating_add(1);
                }
            }
            channel.receiver.finish_decode()?;
            channel.decoding = None;
            self.decode_order.pop_front();
            observe(TransportDecodeObservation {
                transport,
                outcome,
                crc_counts,
            });
        }
        Ok(decoded)
    }
    pub fn process_one(&mut self, consume: impl FnOnce(TransportDelivery<'_>) -> bool) -> bool {
        if self.queued == 0 {
            return false;
        }
        let slot = &self.output[self.head];
        let decoded = slot.decoded.expect("queued result");
        if !consume(TransportDelivery {
            decoded,
            payload: &slot.payload[..decoded.block_bits * decoded.blocks],
        }) {
            return false;
        }
        self.output[self.head].decoded = None;
        self.head = (self.head + 1) % self.output.len();
        self.queued -= 1;
        self.counters.delivered = self.counters.delivered.saturating_add(1);
        true
    }
    #[cfg(test)]
    pub fn retire(&mut self) {
        if self.retired {
            return;
        }
        self.retired = true;
        self.counters.retired_results = self
            .counters
            .retired_results
            .saturating_add(self.queued as u64);
        self.queued = 0;
        for slot in &mut self.output {
            slot.decoded = None;
        }
        for channel in &mut self.channels {
            if channel.receiver.ready_start().is_some() {
                self.counters.retired_completed_ttis =
                    self.counters.retired_completed_ttis.saturating_add(1);
            }
            if channel.receiver.cancel_decode() {
                self.counters.retired_completed_ttis =
                    self.counters.retired_completed_ttis.saturating_add(1);
            }
            if channel.receiver.discard_incomplete() {
                self.counters.interrupted_ttis = self.counters.interrupted_ttis.saturating_add(1);
            }
            channel.active = None;
            channel.decoding = None;
        }
        self.decode_order.clear();
    }
}

#[cfg(test)]
mod tests;
