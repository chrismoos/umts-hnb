// SPDX-License-Identifier: AGPL-3.0-or-later

use super::output::{self, LaneConsumer, LanePublisher, OutputBudget, Publication};
use super::shard::{CONFIGURATIONS_PER_UE, PhysicalChange};
use super::{
    DedicatedFeedback, DedicatedRxShard, DedicatedSampleReceiver, PilotTpcStatus, ReceiveError,
    ReceiveIdentity, ReceiveOwner, SampleProgress, ShardLimits,
};
use crate::dedicated_tx::TransmitCommandOutcome;
use crate::radio_io::MatchedSamples;
use num_complex::Complex32;
use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use umts_dsp::sample_validation::{FiniteSampleBuffer, FiniteSamples};
use umts_dsp::{fractional_sampling::FractionalSampler, pulse_shaping::SamplesPerChip};
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_types::{RadioEpoch, time::CHIPS_PER_FRAME};

const MAX_COMMANDS: usize = 256;
const MAX_HISTORY_FRAMES: usize = 8;
const MAX_OUTPUT_BITS: usize = 1 << 20;

pub(super) struct SampleHistory {
    epoch: RadioEpoch,
    samples: FiniteSampleBuffer,
    capacity: usize,
    maximum_block: usize,
    mirrored_prefix: usize,
    first: Option<u64>,
    end: u64,
    generation: Option<u64>,
    continuity: u64,
    last_input: Option<u64>,
}
#[cfg(test)]
mod history_tests;

impl SampleHistory {
    fn new(epoch: RadioEpoch, capacity: usize, maximum_block: usize) -> Self {
        Self {
            epoch,
            samples: FiniteSampleBuffer::zeroed(capacity * 2),
            capacity,
            maximum_block,
            mirrored_prefix: 0,
            first: None,
            end: 0,
            generation: None,
            continuity: 0,
            last_input: None,
        }
    }
    pub(super) fn bounds(&self) -> Option<(u64, u64, u64)> {
        Some((self.first?, self.end, self.continuity))
    }
    pub(super) fn latest_input(&self) -> Option<u64> {
        self.last_input
    }
    pub(super) fn provenance(&self) -> Option<(u64, u64)> {
        Some((self.generation?, self.continuity))
    }
    fn ensure_window(&mut self, count: usize) {
        let prefix = count.min(self.capacity).saturating_sub(1);
        if prefix > self.mirrored_prefix {
            // Admission can widen the readable window after samples arrived.
            // Catch up only the new prefix, using already reserved storage.
            self.samples
                .copy_within(
                    self.mirrored_prefix..prefix,
                    self.capacity + self.mirrored_prefix,
                )
                .expect("admitted mirror prefix fits reserved history");
            self.mirrored_prefix = prefix;
        }
    }
    fn feed(&mut self, input: MatchedSamples<'_>) -> Result<bool, ReceiveError> {
        if input.window.next_input.epoch != self.epoch
            || input.generation == 0
            || input.window.sample_stride != 1
        {
            return Err(ReceiveError::Identity);
        }
        if input.samples.len() > self.maximum_block || input.samples.len() > self.capacity {
            return Err(ReceiveError::Capacity);
        }
        let samples = FiniteSamples::new(input.samples)?;
        if self.generation.is_some_and(|old| input.generation < old) {
            return Err(ReceiveError::Time);
        }
        if self
            .last_input
            .is_some_and(|old| input.window.next_input.index < old)
        {
            return Err(ReceiveError::Time);
        }
        let changed =
            self.generation.is_some_and(|old| old != input.generation) || input.discontinuity;
        let Some(first) = input.window.first_center else {
            if !input.samples.is_empty() {
                return Err(ReceiveError::Time);
            }
            if changed {
                self.continuity = self.continuity.checked_add(1).ok_or(ReceiveError::Time)?;
                self.first = None;
            }
            self.generation = Some(input.generation);
            self.last_input = Some(input.window.next_input.index);
            return Ok(changed);
        };
        if first.epoch != self.epoch || input.samples.is_empty() {
            return Err(ReceiveError::Time);
        }
        let end = first
            .index
            .checked_add(input.samples.len() as u64)
            .ok_or(ReceiveError::Time)?;
        if end > input.window.next_input.index {
            return Err(ReceiveError::Time);
        }
        // A discontinuity invalidates retained samples, not the epoch's already-observed coordinates.
        if self.continuity != 0 && first.index < self.end {
            return Err(ReceiveError::Time);
        }
        let gap = changed || self.first.is_some() && first.index != self.end;
        if gap || self.first.is_none() {
            self.continuity = self.continuity.checked_add(1).ok_or(ReceiveError::Time)?;
        }
        if gap || self.first.is_none() {
            self.first = Some(first.index);
        }
        let offset = (first.index % self.capacity as u64) as usize;
        let prefix = input.samples.len().min(self.capacity - offset);
        self.samples
            .write(offset, samples.range(0..prefix).ok_or(ReceiveError::Time)?)?;
        let mirrored = prefix.min(self.mirrored_prefix.saturating_sub(offset));
        self.samples.write(
            offset + self.capacity,
            samples.range(0..mirrored).ok_or(ReceiveError::Time)?,
        )?;
        let tail = input.samples.len() - prefix;
        self.samples.write(
            0,
            samples
                .range(prefix..prefix + tail)
                .ok_or(ReceiveError::Time)?,
        )?;
        let mirrored = tail.min(self.mirrored_prefix);
        self.samples.write(
            self.capacity,
            samples
                .range(prefix..prefix + mirrored)
                .ok_or(ReceiveError::Time)?,
        )?;
        self.end = end;
        self.first = Some(
            self.first
                .expect("started history")
                .max(end.saturating_sub(self.capacity as u64)),
        );
        self.generation = Some(input.generation);
        self.last_input = Some(input.window.next_input.index);
        Ok(gap)
    }
    pub(super) fn window(&self, first: u64, count: usize) -> Option<&[Complex32]> {
        self.samples.samples().get(self.window_range(first, count)?)
    }
    pub(super) fn evidence_window(&self, first: u64, count: usize) -> Option<FiniteSamples<'_>> {
        self.samples.window(self.window_range(first, count)?)
    }
    fn window_range(&self, first: u64, count: usize) -> Option<std::ops::Range<usize>> {
        if first < self.first?
            || count > self.capacity
            || first.checked_add(count as u64)? > self.end
        {
            return None;
        }
        let offset = (first % self.capacity as u64) as usize;
        if offset + count > self.capacity + self.mirrored_prefix {
            return None;
        }
        Some(offset..offset + count)
    }
}

#[derive(Clone, Copy)]
pub struct SampleRuntimeLimits {
    pub shard: ShardLimits,
    pub history_samples: NonZeroUsize,
    pub maximum_shared_bytes: usize,
    pub command_capacity: NonZeroUsize,
    pub commands_per_step: NonZeroUsize,
    pub output_slots_per_ue: NonZeroUsize,
    pub maximum_output_bits: NonZeroUsize,
    pub maximum_output_bytes: usize,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleRuntimeCounters {
    pub input_blocks: u64,
    pub discontinuities: u64,
    pub commands: u64,
    pub serviced: u64,
    pub sampled_slots: u64,
    pub power_feedback_waits: u64,
    pub failed_owners: u64,
    pub published: u64,
    pub backpressure: u64,
    pub feedback_submitted: u64,
    pub feedback_accepted: u64,
    pub feedback_rejected: u64,
    pub feedback_retired: u64,
    pub feedback_protocol_errors: u64,
    pub decode_state_updates: u64,
    pub maximum_decode_state_updates: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleRuntimeFailure {
    pub identity: ReceiveIdentity,
    pub error: ReceiveError,
    pub latest_input_sample: Option<u64>,
    pub sample_provenance: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct RetiredPilotFeedback {
    pub identity: ReceiveIdentity,
    pub latest_input_sample: Option<u64>,
    pub counters: super::PilotTpcCounters,
    pub last_accepted: Option<super::TpcProvenance>,
    pub last_sir_linear: Option<f64>,
    pub last_discard: Option<super::RejectedPilotObservation>,
    pub failed_observation: Option<super::RejectedPilotObservation>,
    pub maximum_observation_age_chips: u64,
    pub maximum_publication_age_chips: u64,
}

pub struct HsupaReceiveSource {
    pub code: Arc<umts_dsp::dedicated::UplinkDpchCode>,
    pub anchor: super::FrameAnchor,
    pub control_search: super::ControlSearchGeometry,
}

// Inline snapshots keep observation replies allocation-free on the receive worker.
#[allow(clippy::large_enum_variant)]
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub enum ReceiveCommandResult {
    RestoredHsupaDetached {
        identity: ReceiveIdentity,
        restored: umts_types::time::ChipInstant,
        receiver: super::HsupaAttachment,
    },
    RestoredHsupaDetachRejected {
        identity: ReceiveIdentity,
        restored: umts_types::time::ChipInstant,
        error: ReceiveError,
    },
    DchRestorationScheduled {
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
        result: Result<(), ReceiveError>,
    },
    HsupaSource {
        identity: ReceiveIdentity,
        source: Option<HsupaReceiveSource>,
        /// A missing source that a later query may still supply: the receiver exists but
        /// is not active yet. A failed, retired or already attached receiver is final.
        transient: bool,
    },
    HsupaCancelled {
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
        receiver: super::HsupaAttachment,
    },
    HsupaCancellationRejected {
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
        error: ReceiveError,
    },
    HsupaAttached {
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
    },
    HsupaRejected {
        error: ReceiveError,
        receiver: super::HsupaAttachment,
    },
    Installed(ReceiveIdentity),
    Armed(ReceiveIdentity),
    ArmRejected {
        identity: ReceiveIdentity,
        error: ReceiveError,
    },
    InstallRejected {
        error: ReceiveError,
        receiver: Box<DedicatedSampleReceiver>,
    },
    Removed {
        identity: ReceiveIdentity,
        receiver: Option<Box<DedicatedSampleReceiver>>,
    },
    ReplacementPrepared {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    },
    ReplacementCommitted {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        boundary: super::FrameAnchor,
    },
    CommitRejected {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        error: ReceiveError,
    },
    Retired {
        identity: ReceiveIdentity,
        receiver: Box<DedicatedSampleReceiver>,
        retirement: ReceiveRetirement,
    },
    RetireRejected {
        identity: ReceiveIdentity,
        error: ReceiveError,
    },
    Observed {
        identity: ReceiveIdentity,
        status: Option<ReceivePlanStatus>,
    },
    Closed,
}
#[derive(Clone, Copy, Debug)]
pub struct ReceiveRetirement {
    previous: ReceiveIdentity,
    old_anchor: super::FrameAnchor,
    successor: ReceiveIdentity,
    boundary: umts_types::time::ChipInstant,
}
impl ReceiveRetirement {
    pub fn previous(&self) -> ReceiveIdentity {
        self.previous
    }
    pub fn old_anchor(&self) -> super::FrameAnchor {
        self.old_anchor
    }
    pub fn successor(&self) -> ReceiveIdentity {
        self.successor
    }
    pub fn boundary(&self) -> umts_types::time::ChipInstant {
        self.boundary
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceivePlanPhase {
    Prepared,
    Active,
    Staged {
        previous: ReceiveIdentity,
    },
    Committed {
        previous: ReceiveIdentity,
    },
    Draining {
        next: ReceiveIdentity,
        boundary: umts_types::time::ChipInstant,
    },
    Stopped {
        next: ReceiveIdentity,
        boundary: umts_types::time::ChipInstant,
    },
}
#[derive(Clone, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct ReceivePlanStatus {
    pub restored_dch_at: Option<umts_types::time::ChipInstant>,
    pub phase: ReceivePlanPhase,
    pub next_frame: umts_types::time::ChipInstant,
    pub last_consumed_frame: Option<umts_types::time::ChipInstant>,
    pub completed_ttis: usize,
    pub queued_transports: usize,
    pub chips: super::ChipReceiveCounters,
    pub transports: super::ReceiveCounters,
    pub transport_details: [Option<(
        umts_dsp::dedicated_transport::TransportId,
        super::TransportReceiveCounters,
    )>; super::MAX_TRANSPORT_CHANNELS],
    pub power_control: Option<PilotTpcStatus>,
    pub outer_loop: Option<super::OuterLoopStatus>,
    pub hsupa_pending: bool,
    pub hsupa_dropped_intervals: u64,
    pub samples: super::samples::SampleReceiveCounters,
    pub tracking: super::samples::SampleTrackingStatus,
}
pub struct ReceiveCommandTicket {
    result: Receiver<ReceiveCommandResult>,
    alive: Arc<AtomicBool>,
    finished: bool,
}
impl ReceiveCommandTicket {
    pub fn try_result(&mut self) -> Option<ReceiveCommandResult> {
        if self.finished {
            return None;
        }
        // Publication can race teardown after its availability check and leave an orphaned command in the ring.
        let result = self.result.try_recv().or_else(|| {
            (!self.alive.load(Ordering::Acquire)).then_some(ReceiveCommandResult::Closed)
        });
        if result.is_some() {
            self.finished = true;
        }
        result
    }
}
struct Reply(Option<Sender<ReceiveCommandResult>>);
impl Reply {
    fn finish(mut self, result: ReceiveCommandResult) {
        let _ = self.0.take().expect("one command reply").try_send(result);
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.try_send(ReceiveCommandResult::Closed);
        }
    }
}
enum Command {
    DetachRestoredHsupa {
        identity: ReceiveIdentity,
        restored: umts_types::time::ChipInstant,
        reply: Reply,
    },
    RestoreDch {
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
        reply: Reply,
    },
    HsupaSource {
        identity: ReceiveIdentity,
        reply: Reply,
    },
    CancelHsupa {
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
        reply: Reply,
    },
    AttachHsupa {
        receiver: super::HsupaAttachment,
        activation: umts_types::time::ChipInstant,
        maximum_bytes: NonZeroUsize,
        reply: Reply,
    },
    Install {
        receiver: Box<DedicatedSampleReceiver>,
        output: LanePublisher,
        armed: bool,
        previous: Option<ReceiveIdentity>,
        reply: Reply,
    },
    Arm {
        identity: ReceiveIdentity,
        reply: Reply,
    },
    Remove {
        identity: ReceiveIdentity,
        reply: Reply,
    },
    Commit {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        reply: Reply,
    },
    RetireDrained {
        identity: ReceiveIdentity,
        reply: Reply,
    },
    Observe {
        identity: ReceiveIdentity,
        reply: Reply,
    },
}
pub struct DedicatedReceivePort {
    alive: Arc<AtomicBool>,
    commands: Sender<Command>,
    epoch: RadioEpoch,
    lanes: Vec<Option<LaneConsumer>>,
    budget: Arc<OutputBudget>,
    output_slots: NonZeroUsize,
    output_bits: NonZeroUsize,
    cursor: usize,
    scan_budget: usize,
    retry_slot: Option<usize>,
}
impl DedicatedReceivePort {
    pub fn detach_restored_hsupa(
        &mut self,
        identity: ReceiveIdentity,
        restored: umts_types::time::ChipInstant,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        if restored.epoch != identity.ue.epoch {
            return Err(identity);
        }
        self.control(identity, |reply| Command::DetachRestoredHsupa {
            identity,
            restored,
            reply,
        })
    }
    pub fn restore_dch_at(
        &mut self,
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        if activation.epoch != identity.ue.epoch {
            return Err(identity);
        }
        self.control(identity, |reply| Command::RestoreDch {
            identity,
            activation,
            reply,
        })
    }
    // Only cancel an activation never published to the UE. A signaled activation requires protocol recovery even if RX is behind.
    pub fn cancel_hsupa(
        &mut self,
        identity: ReceiveIdentity,
        activation: umts_types::time::ChipInstant,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        if activation.epoch != identity.ue.epoch {
            return Err(identity);
        }
        self.control(identity, |reply| Command::CancelHsupa {
            identity,
            activation,
            reply,
        })
    }
    pub fn attach_hsupa_at(
        &mut self,
        receiver: impl Into<super::HsupaAttachment>,
        activation: umts_types::time::ChipInstant,
        maximum_bytes: NonZeroUsize,
    ) -> Result<ReceiveCommandTicket, super::HsupaAttachment> {
        let receiver = receiver.into();
        if !self.available()
            || receiver.identity().ue.epoch != self.epoch
            || activation.epoch != self.epoch
        {
            return Err(receiver);
        }
        let (sender, result) = mailbox::bounded(NonZeroUsize::MIN);
        match self.commands.try_send(Command::AttachHsupa {
            receiver,
            activation,
            maximum_bytes,
            reply: Reply(Some(sender)),
        }) {
            Ok(()) => Ok(ReceiveCommandTicket {
                result,
                alive: self.alive.clone(),
                finished: false,
            }),
            Err(Command::AttachHsupa { receiver, .. }) => Err(receiver),
            Err(_) => unreachable!("HSUPA attachment command"),
        }
    }

    pub fn hsupa_source(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        self.control(identity, |reply| Command::HsupaSource { identity, reply })
    }
    pub fn available(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub fn install(
        &mut self,
        receiver: Box<DedicatedSampleReceiver>,
    ) -> Result<ReceiveCommandTicket, Box<DedicatedSampleReceiver>> {
        self.submit(receiver, true, None)
    }
    pub fn prepare(
        &mut self,
        receiver: Box<DedicatedSampleReceiver>,
    ) -> Result<ReceiveCommandTicket, Box<DedicatedSampleReceiver>> {
        self.submit(receiver, false, None)
    }
    pub fn stage_replacement(
        &mut self,
        previous: ReceiveIdentity,
        receiver: Box<DedicatedSampleReceiver>,
    ) -> Result<ReceiveCommandTicket, Box<DedicatedSampleReceiver>> {
        self.submit(receiver, false, Some(previous))
    }
    #[cfg(test)]
    pub fn reserved_output_bytes(&self) -> usize {
        self.budget.used()
    }
    pub fn epoch(&self) -> RadioEpoch {
        self.epoch
    }
    pub fn capacity(&self) -> usize {
        self.lanes.len() / CONFIGURATIONS_PER_UE
    }
    pub(crate) fn paired_with(&self, runtime: &DedicatedSampleRuntime) -> bool {
        Arc::ptr_eq(&self.alive, &runtime.alive)
    }
    fn submit(
        &mut self,
        receiver: Box<DedicatedSampleReceiver>,
        armed: bool,
        previous: Option<ReceiveIdentity>,
    ) -> Result<ReceiveCommandTicket, Box<DedicatedSampleReceiver>> {
        let identity = receiver.session().identity();
        let base = identity.ue.slot as usize * CONFIGURATIONS_PER_UE;
        if !self.available() || identity.ue.epoch != self.epoch || base >= self.lanes.len() {
            return Err(receiver);
        }
        for lane in &mut self.lanes[base..base + CONFIGURATIONS_PER_UE] {
            if lane.as_mut().is_some_and(|lane| lane.closed_and_empty()) {
                *lane = None;
            }
        }
        let lanes = &self.lanes[base..base + CONFIGURATIONS_PER_UE];
        if let Some(previous) = previous {
            if previous.ue != identity.ue
                || !lanes
                    .iter()
                    .flatten()
                    .any(|lane| lane.identity() == previous)
            {
                return Err(receiver);
            }
        } else if lanes.iter().any(Option::is_some) {
            return Err(receiver);
        }
        let Some(index) = lanes
            .iter()
            .position(Option::is_none)
            .map(|index| base + index)
        else {
            return Err(receiver);
        };
        let payload_bits = receiver
            .session()
            .output
            .iter()
            .map(|slot| slot.payload.len())
            .max()
            .unwrap_or(0)
            .min(self.output_bits.get());
        let (output, consume) = match output::channel(
            identity,
            previous,
            self.output_slots,
            NonZeroUsize::new(payload_bits).unwrap_or(NonZeroUsize::MIN),
            &self.budget,
        ) {
            Ok(lane) => lane,
            Err(_) => return Err(receiver),
        };
        let (sender, result) = mailbox::bounded(NonZeroUsize::MIN);
        match self.commands.try_send(Command::Install {
            receiver,
            output,
            armed,
            previous,
            reply: Reply(Some(sender)),
        }) {
            Ok(()) => {
                self.lanes[index] = Some(consume);
                Ok(ReceiveCommandTicket {
                    result,
                    alive: self.alive.clone(),
                    finished: false,
                })
            }
            Err(Command::Install { receiver, .. }) => Err(receiver),
            Err(_) => unreachable!("install command"),
        }
    }
    pub fn arm(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        if !self.available() {
            return Err(identity);
        }
        let (sender, result) = mailbox::bounded(NonZeroUsize::MIN);
        match self.commands.try_send(Command::Arm {
            identity,
            reply: Reply(Some(sender)),
        }) {
            Ok(()) => Ok(ReceiveCommandTicket {
                result,
                alive: self.alive.clone(),
                finished: false,
            }),
            Err(_) => Err(identity),
        }
    }
    pub fn retire_output(&mut self, identity: ReceiveIdentity) -> Result<usize, ReceiveError> {
        let index = self
            .lanes
            .iter()
            .position(|lane| {
                lane.as_ref()
                    .is_some_and(|lane| lane.identity() == identity)
            })
            .ok_or(ReceiveError::Identity)?;
        let slot = self.lanes.get_mut(index).ok_or(ReceiveError::Identity)?;
        let lane = slot
            .as_ref()
            .filter(|lane| lane.identity() == identity)
            .ok_or(ReceiveError::Identity)?;
        if !lane.closed() {
            return Err(ReceiveError::Occupied);
        }
        if self.retry_slot == Some(index) {
            self.retry_slot = None;
        }
        Ok(slot.take().expect("closed output lane").discard())
    }
    pub fn remove(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        if !self.available() {
            return Err(identity);
        }
        let (sender, result) = mailbox::bounded(NonZeroUsize::MIN);
        match self.commands.try_send(Command::Remove {
            identity,
            reply: Reply(Some(sender)),
        }) {
            Ok(()) => Ok(ReceiveCommandTicket {
                result,
                alive: self.alive.clone(),
                finished: false,
            }),
            Err(_) => Err(identity),
        }
    }
    pub fn commit_replacement(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        self.control(identity, |reply| Command::Commit {
            previous,
            identity,
            reply,
        })
    }
    pub fn retire_drained(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        self.control(identity, |reply| Command::RetireDrained { identity, reply })
    }
    pub fn observe(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        self.control(identity, |reply| Command::Observe { identity, reply })
    }
    fn control(
        &mut self,
        identity: ReceiveIdentity,
        command: impl FnOnce(Reply) -> Command,
    ) -> Result<ReceiveCommandTicket, ReceiveIdentity> {
        if !self.available() || identity.ue.epoch != self.epoch {
            return Err(identity);
        }
        let (sender, result) = mailbox::bounded(NonZeroUsize::MIN);
        self.commands
            .try_send(command(Reply(Some(sender))))
            .map_err(|_| identity)?;
        Ok(ReceiveCommandTicket {
            result,
            alive: self.alive.clone(),
            finished: false,
        })
    }
    #[cfg(test)]
    pub fn process_one(
        &mut self,
        accept: impl FnOnce(super::TransportDelivery<'_>) -> bool,
    ) -> bool {
        if let Some(index) = self.retry_slot {
            if !self.output_eligible(index) {
                self.retry_slot = None;
                return false;
            }
            if let Some(lane) = &mut self.lanes[index] {
                if let Some(accepted) = lane.process_one(accept) {
                    if accepted {
                        self.retry_slot = None;
                    }
                    return accepted;
                }
                self.retry_slot = None;
                return false;
            }
            self.retry_slot = None;
        }
        let mut accept = Some(accept);
        for _ in 0..self.scan_budget {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.lanes.len();
            if !self.output_eligible(index) {
                continue;
            }
            let Some(lane) = &mut self.lanes[index] else {
                continue;
            };
            if let Some(accepted) = lane
                .process_one(|delivery| accept.take().expect("one transport consumer")(delivery))
            {
                if !accepted {
                    self.retry_slot = Some(index);
                }
                return accepted;
            }
        }
        false
    }
    pub fn route_one(
        &mut self,
        mut accept: impl FnMut(super::TransportDelivery<'_>) -> bool,
    ) -> bool {
        for _ in 0..self.scan_budget {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.lanes.len();
            if !self.output_eligible(index) {
                continue;
            }
            let Some(lane) = &mut self.lanes[index] else {
                continue;
            };
            if lane.process_one(&mut accept) == Some(true) {
                if self.retry_slot == Some(index) {
                    self.retry_slot = None;
                }
                return true;
            }
        }
        false
    }
    fn output_eligible(&mut self, index: usize) -> bool {
        // New-generation transport must not overtake retained old-generation RLC input.
        let Some(previous) = self.lanes[index].as_ref().and_then(LaneConsumer::previous) else {
            return true;
        };
        let base = previous.ue.slot as usize * CONFIGURATIONS_PER_UE;
        for lane in self.lanes[base..base + CONFIGURATIONS_PER_UE]
            .iter_mut()
            .flatten()
        {
            if lane.identity() == previous {
                return lane.closed_and_empty();
            }
        }
        true
    }
}
impl Drop for DedicatedReceivePort {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

pub struct DedicatedSampleRuntime {
    epoch: RadioEpoch,
    rate: usize,
    history: SampleHistory,
    sampler: FractionalSampler,
    shard: DedicatedRxShard<RoutedOwner>,
    power_links: super::LinkPowerRegistry,
    commands: Receiver<Command>,
    alive: Arc<AtomicBool>,
    command_budget: usize,
    maximum_output_bits: usize,
    publish_cursor: usize,
    maximum_ues: usize,
    publish_budget: usize,
    counters: SampleRuntimeCounters,
    first_failure: Option<SampleRuntimeFailure>,
    first_retired_feedback: Option<RetiredPilotFeedback>,
    feedback: Option<Box<DedicatedFeedback>>,
    feedback_queue: VecDeque<ReceiveIdentity>,
    feedback_queue_capacity: usize,
    service_results: Vec<(ReceiveIdentity, Result<SampleProgress, ReceiveError>)>,
    decode_cursor: usize,
    decode_after: Option<ReceiveIdentity>,
}
struct RoutedOwner {
    receiver: Box<DedicatedSampleReceiver>,
    output: LanePublisher,
    resources: super::ReceiveResources,
    phase: ReceivePlanPhase,
}
impl ReceiveOwner for RoutedOwner {
    fn session(&self) -> &super::DedicatedReceiveSession {
        self.receiver.session()
    }
    fn session_mut(&mut self) -> &mut super::DedicatedReceiveSession {
        self.receiver.session_mut()
    }
    fn service_enabled(&self) -> bool {
        match self.phase {
            ReceivePlanPhase::Prepared | ReceivePlanPhase::Staged { .. } => false,
            ReceivePlanPhase::Stopped { .. } => {
                self.session().pending_completed_ttis() != 0
                    && self.session().queued() < self.session().output.len()
            }
            _ => true,
        }
    }
    fn resources(&self) -> super::ReceiveResources {
        self.resources
    }
    fn last_consumed_frame(&self) -> Option<umts_types::time::ChipInstant> {
        self.receiver.last_consumed_frame()
    }
}
impl DedicatedSampleRuntime {
    pub(crate) fn shared_queue_storage_bytes(configurations: usize) -> Option<usize> {
        configurations.checked_mul(
            std::mem::size_of::<ReceiveIdentity>()
                + std::mem::size_of::<(ReceiveIdentity, Result<SampleProgress, ReceiveError>)>(),
        )
    }

    pub fn new(
        epoch: RadioEpoch,
        rate: SamplesPerChip,
        limits: SampleRuntimeLimits,
    ) -> Result<(Self, DedicatedReceivePort), ReceiveError> {
        let capacity = limits.history_samples.get();
        if limits.command_capacity.get() > MAX_COMMANDS
            || limits.commands_per_step.get() > MAX_COMMANDS
            || limits.output_slots_per_ue.get() > MAX_COMMANDS
            || limits.maximum_output_bits.get() > MAX_OUTPUT_BITS
            || capacity < umts_types::time::CHIPS_PER_SLOT as usize * rate.get()
            || capacity > CHIPS_PER_FRAME as usize * rate.get() * MAX_HISTORY_FRAMES
        {
            return Err(ReceiveError::Capacity);
        }
        let sampler = FractionalSampler::new();
        let feedback_queue_capacity = limits
            .shard
            .maximum_ues
            .get()
            .checked_mul(CONFIGURATIONS_PER_UE)
            .ok_or(ReceiveError::Capacity)?;
        let feedback_queue_bytes = Self::shared_queue_storage_bytes(feedback_queue_capacity)
            .ok_or(ReceiveError::Capacity)?;
        if capacity
            .checked_mul(2 * std::mem::size_of::<Complex32>())
            .and_then(|v| v.checked_add(sampler.storage_bytes()))
            .and_then(|v| v.checked_add(feedback_queue_bytes))
            .and_then(|v| {
                super::LinkPowerRegistry::storage_bytes(limits.shard.maximum_ues)
                    .and_then(|bytes| v.checked_add(bytes))
            })
            .is_none_or(|v| v > limits.maximum_shared_bytes)
        {
            return Err(ReceiveError::Capacity);
        }
        let (send, commands) = mailbox::bounded(limits.command_capacity);
        if limits
            .output_slots_per_ue
            .get()
            .checked_mul(limits.maximum_output_bits.get())
            .is_none_or(|v| v > limits.maximum_output_bytes)
        {
            return Err(ReceiveError::Capacity);
        }
        let shard = DedicatedRxShard::new(epoch, limits.shard)?;
        let budget = OutputBudget::new(limits.maximum_output_bytes);
        let alive = Arc::new(AtomicBool::new(true));
        let port = DedicatedReceivePort {
            commands: send,
            alive: alive.clone(),
            epoch,
            lanes: (0..limits.shard.maximum_ues.get() * CONFIGURATIONS_PER_UE)
                .map(|_| None)
                .collect(),
            budget,
            output_slots: limits.output_slots_per_ue,
            output_bits: limits.maximum_output_bits,
            cursor: 0,
            scan_budget: limits
                .shard
                .maximum_sessions_per_step
                .get()
                .min(limits.shard.maximum_ues.get())
                * CONFIGURATIONS_PER_UE,
            retry_slot: None,
        };
        Ok((
            Self {
                epoch,
                rate: rate.get(),
                history: SampleHistory::new(
                    epoch,
                    capacity,
                    umts_types::time::CHIPS_PER_SLOT as usize * rate.get(),
                ),
                sampler,
                shard,
                power_links: super::LinkPowerRegistry::new(limits.shard.maximum_ues)
                    .map_err(ReceiveError::PowerControl)?,
                commands,
                alive,
                command_budget: limits.commands_per_step.get(),
                counters: SampleRuntimeCounters::default(),
                first_failure: None,
                first_retired_feedback: None,
                feedback: None,
                feedback_queue: VecDeque::with_capacity(feedback_queue_capacity),
                feedback_queue_capacity,
                service_results: Vec::with_capacity(feedback_queue_capacity),
                decode_cursor: 0,
                decode_after: None,
                maximum_output_bits: limits.maximum_output_bits.get(),
                publish_cursor: 0,
                maximum_ues: limits.shard.maximum_ues.get() * CONFIGURATIONS_PER_UE,
                publish_budget: limits
                    .shard
                    .maximum_sessions_per_step
                    .get()
                    .min(limits.shard.maximum_ues.get())
                    * CONFIGURATIONS_PER_UE,
            },
            port,
        ))
    }
    pub fn first_failure(&self) -> Option<&SampleRuntimeFailure> {
        self.first_failure.as_ref()
    }
    pub fn first_retired_feedback(&self) -> Option<RetiredPilotFeedback> {
        self.first_retired_feedback
    }
    pub fn attach_feedback(
        &mut self,
        source: Box<DedicatedFeedback>,
    ) -> Result<(), Box<DedicatedFeedback>> {
        if self.feedback.is_some()
            || self.counters.commands != 0
            || self.counters.input_blocks != 0
            || source.port.epoch() != self.epoch
            || source.clock.origin().chip.epoch != self.epoch
            || source.port.outstanding() != 0
            || !source.port.available()
        {
            return Err(source);
        }
        self.feedback = Some(source);
        Ok(())
    }
    fn reconcile_feedback(&mut self) {
        let Some(source) = &mut self.feedback else {
            return;
        };
        for _ in 0..self.command_budget {
            let Some(event) = source.port.try_event() else {
                break;
            };
            match event.outcome {
                TransmitCommandOutcome::Estimate { estimate, result } => {
                    let Some(owner) = self.shard.session_mut(estimate.identity()) else {
                        self.counters.feedback_retired =
                            self.counters.feedback_retired.saturating_add(1);
                        continue;
                    };
                    let Some(power) = &mut owner.receiver.power else {
                        source.failed = true;
                        self.counters.feedback_protocol_errors =
                            self.counters.feedback_protocol_errors.saturating_add(1);
                        continue;
                    };
                    match power.reconcile(event.id, estimate, result) {
                        Ok(true) => {
                            self.counters.feedback_accepted =
                                self.counters.feedback_accepted.saturating_add(1)
                        }
                        Ok(false) => {
                            self.counters.feedback_retired =
                                self.counters.feedback_retired.saturating_add(1)
                        }
                        Err(error) => {
                            self.counters.feedback_rejected =
                                self.counters.feedback_rejected.saturating_add(1);
                            self.first_failure
                                .get_or_insert_with(|| SampleRuntimeFailure {
                                    identity: estimate.identity(),
                                    error,
                                    latest_input_sample: self.history.latest_input(),
                                    sample_provenance: self.history.provenance(),
                                });
                        }
                    }
                }
                _ => {
                    source.failed = true;
                    self.counters.feedback_protocol_errors =
                        self.counters.feedback_protocol_errors.saturating_add(1);
                }
            }
        }
        if !source.port.available() {
            source.failed = true;
        }
    }
    pub fn counters(&self) -> SampleRuntimeCounters {
        self.counters
    }

    pub fn radio_decode_work(&self) -> Result<umts_dsp::coding::DecodeWork, ReceiveError> {
        let base = super::DedicatedReceiveSession::decode_work_budget().remaining();
        let updates = self
            .shard
            .owners()
            .filter(|owner| {
                !matches!(
                    owner.phase,
                    ReceivePlanPhase::Prepared | ReceivePlanPhase::Staged { .. }
                )
            })
            .try_fold(base, |total, owner| {
                total.checked_add(owner.receiver.hsupa_decode_work_per_slot())
            })
            .ok_or(ReceiveError::Capacity)?;
        Ok(umts_dsp::coding::DecodeWork::new(
            NonZeroUsize::new(updates).ok_or(ReceiveError::Capacity)?,
        ))
    }
    pub fn feed(&mut self, input: MatchedSamples<'_>) -> Result<(), ReceiveError> {
        if self.history.feed(input)? {
            self.counters.discontinuities = self.counters.discontinuities.saturating_add(1);
        }
        self.counters.input_blocks = self.counters.input_blocks.saturating_add(1);
        Ok(())
    }
    #[cfg(test)]
    pub fn service(
        &mut self,
        observe: impl FnMut(ReceiveIdentity, Result<SampleProgress, ReceiveError>),
    ) -> usize {
        let mut work = super::DedicatedReceiveSession::decode_work_budget();
        self.service_with_work(&mut work, observe)
    }
    pub fn service_with_work(
        &mut self,
        work: &mut umts_dsp::coding::DecodeWork,
        observe: impl FnMut(ReceiveIdentity, Result<SampleProgress, ReceiveError>),
    ) -> usize {
        self.service_with_decode_observer(work, |_| (), observe)
    }

    pub fn service_with_decode_observer(
        &mut self,
        work: &mut umts_dsp::coding::DecodeWork,
        mut decoded: impl FnMut(super::UplinkDecodeObservation),
        observe: impl FnMut(ReceiveIdentity, Result<SampleProgress, ReceiveError>),
    ) -> usize {
        let available = work.remaining();
        let mut first_worked = None;
        let serviced = self.service_with_owner_decoder(
            |receiver, power_links| {
                let before = work.remaining();
                let configured = receiver.outer_loop_settings.is_some();
                let mut power_error = None;
                let result = receiver.service_decodes(work, |observation| {
                    if configured && power_error.is_none() {
                        power_error = power_links
                            .observe(super::LinkCrcObservation::from_decode(observation))
                            .err();
                    }
                    decoded(observation);
                });
                if first_worked.is_none() && work.remaining() < before {
                    first_worked = Some(receiver.session().identity());
                }
                result.and_then(|count| {
                    power_error.map_or(Ok(count), |error| Err(ReceiveError::PowerControl(error)))
                })
            },
            observe,
        );
        if first_worked.is_some() {
            self.decode_after = first_worked;
        }
        let used = available - work.remaining();
        self.counters.decode_state_updates = self
            .counters
            .decode_state_updates
            .saturating_add(used as u64);
        self.counters.maximum_decode_state_updates =
            self.counters.maximum_decode_state_updates.max(used);
        serviced
    }
    #[cfg(test)]
    pub(super) fn service_with_decoder(
        &mut self,
        mut decode: impl FnMut(&mut super::DedicatedReceiveSession) -> Result<usize, ReceiveError>,
        observe: impl FnMut(ReceiveIdentity, Result<SampleProgress, ReceiveError>),
    ) -> usize {
        self.service_with_owner_decoder(|receiver, _| decode(receiver.session_mut()), observe)
    }

    fn service_with_owner_decoder(
        &mut self,
        mut decode: impl FnMut(
            &mut DedicatedSampleReceiver,
            &mut super::LinkPowerRegistry,
        ) -> Result<usize, ReceiveError>,
        mut observe: impl FnMut(ReceiveIdentity, Result<SampleProgress, ReceiveError>),
    ) -> usize {
        self.reconcile_feedback();
        if !self.alive.load(Ordering::Acquire) {
            for _ in 0..self.command_budget {
                if self.commands.try_recv().is_none() {
                    break;
                }
            }
            return 0;
        }
        for _ in 0..self.command_budget {
            let Some(command) = self.commands.try_recv() else {
                break;
            };
            self.counters.commands = self.counters.commands.saturating_add(1);
            match command {
                Command::DetachRestoredHsupa {
                    identity,
                    restored,
                    reply,
                } => {
                    let mut returned = None;
                    let bytes = self
                        .shard
                        .session_mut(identity)
                        .ok_or(ReceiveError::Identity)
                        .and_then(|owner| owner.receiver.detachable_hsupa_bytes(restored));
                    let result = bytes.and_then(|bytes| {
                        self.shard.change_physical_with(
                            identity,
                            PhysicalChange::Shrink(bytes),
                            |owner| {
                                let receiver = owner.receiver.detach_restored_hsupa(restored)?;
                                owner.resources.physical_sample_bytes -= bytes;
                                returned = Some(receiver);
                                Ok(())
                            },
                        )
                    });
                    reply.finish(match result {
                        Ok(()) => ReceiveCommandResult::RestoredHsupaDetached {
                            identity,
                            restored,
                            receiver: returned.expect("detached HSUPA resources"),
                        },
                        Err(error) => ReceiveCommandResult::RestoredHsupaDetachRejected {
                            identity,
                            restored,
                            error,
                        },
                    });
                }
                Command::RestoreDch {
                    identity,
                    activation,
                    reply,
                } => {
                    let result = self
                        .shard
                        .session_mut(identity)
                        .ok_or(ReceiveError::Identity)
                        .and_then(|owner| owner.receiver.schedule_dch_restoration(activation));
                    reply.finish(ReceiveCommandResult::DchRestorationScheduled {
                        identity,
                        activation,
                        result,
                    });
                }
                Command::CancelHsupa {
                    identity,
                    activation,
                    reply,
                } => {
                    let mut returned = None;
                    let bytes = self
                        .shard
                        .session_mut(identity)
                        .ok_or(ReceiveError::Identity)
                        .and_then(|owner| owner.receiver.cancellable_hsupa_bytes(activation));
                    let result = bytes.and_then(|bytes| {
                        self.shard.change_physical_with(
                            identity,
                            PhysicalChange::Shrink(bytes),
                            |owner| {
                                let receiver = owner.receiver.cancel_hsupa(activation)?;
                                owner.resources.physical_sample_bytes -= bytes;
                                returned = Some(receiver);
                                Ok(())
                            },
                        )
                    });
                    reply.finish(match result {
                        Ok(()) => ReceiveCommandResult::HsupaCancelled {
                            identity,
                            activation,
                            receiver: returned.expect("canceled HSUPA owner"),
                        },
                        Err(error) => ReceiveCommandResult::HsupaCancellationRejected {
                            identity,
                            activation,
                            error,
                        },
                    });
                }
                Command::AttachHsupa {
                    receiver,
                    activation,
                    maximum_bytes,
                    reply,
                } => {
                    let identity = receiver.identity();
                    let bytes = receiver.retained_bytes();
                    let mut retained = Some(receiver);
                    let result = bytes.and_then(|bytes| {
                        self.shard.change_physical_with(
                            identity,
                            PhysicalChange::Grow(bytes),
                            |owner| {
                                if owner.phase != ReceivePlanPhase::Active
                                    || owner.session().failed
                                    || owner.session().retired
                                {
                                    return Err(ReceiveError::Configuration);
                                }
                                match owner.receiver.attach_prepared_hsupa_at(
                                    retained.take().expect("owned HSUPA candidate"),
                                    activation,
                                    maximum_bytes,
                                ) {
                                    Ok(()) => {
                                        owner.resources.physical_sample_bytes += bytes;
                                        Ok(())
                                    }
                                    Err((error, receiver)) => {
                                        retained = Some(receiver);
                                        Err(error)
                                    }
                                }
                            },
                        )
                    });
                    reply.finish(match result {
                        Ok(()) => ReceiveCommandResult::HsupaAttached {
                            identity,
                            activation,
                        },
                        Err(error) => ReceiveCommandResult::HsupaRejected {
                            error,
                            receiver: retained.expect("rejected HSUPA ownership"),
                        },
                    });
                }
                Command::Install {
                    receiver,
                    output,
                    armed,
                    previous,
                    reply,
                } => {
                    let identity = receiver.session().identity();
                    let timing = previous.map_or(Ok(()), |previous| {
                        self.replacement_timing(
                            previous,
                            receiver.session().frame_anchor().time,
                            receiver.first_receive(),
                            receiver.search_radius(),
                        )
                    });
                    let invalid_feedback = receiver.power.as_ref().is_some_and(|power| {
                        self.feedback.as_ref().is_none_or(|source| {
                            source.failed || power.minimum_lead() < source.minimum_lead_chips()
                        })
                    });
                    let result = if receiver.rate() != self.rate || invalid_feedback {
                        Err((ReceiveError::Configuration, receiver))
                    } else if receiver.required_history(self.sampler.guard_samples())
                        > self.history.capacity
                    {
                        let error = ReceiveError::HistoryCapacity {
                            required_samples: receiver
                                .required_history(self.sampler.guard_samples()),
                            available_samples: self.history.capacity,
                        };
                        Err((error, receiver))
                    } else if let Some(required_bits) = receiver
                        .session()
                        .output
                        .iter()
                        .map(|slot| slot.payload.len())
                        .max()
                        .filter(|bits| *bits > self.maximum_output_bits)
                    {
                        let error = ReceiveError::OutputWidth {
                            required_bits,
                            maximum_bits: self.maximum_output_bits,
                        };
                        Err((error, receiver))
                    } else if identity != output.identity() {
                        Err((ReceiveError::Identity, receiver))
                    } else if let Err(error) = timing {
                        Err((error, receiver))
                    } else {
                        let mut resources = receiver.resources();
                        match resources.output_bytes.checked_add(output.bytes()) {
                            None => Err((ReceiveError::Capacity, receiver)),
                            Some(bytes) => {
                                resources.output_bytes = bytes;
                                let owner = Box::new(RoutedOwner {
                                    receiver,
                                    output,
                                    resources,
                                    phase: previous.map_or(
                                        if armed {
                                            ReceivePlanPhase::Active
                                        } else {
                                            ReceivePlanPhase::Prepared
                                        },
                                        |previous| ReceivePlanPhase::Staged { previous },
                                    ),
                                });
                                self.install_owner(owner, previous, armed)
                                    .map_err(|(error, owner)| (error, owner.receiver))
                            }
                        }
                    };
                    reply.finish(match result {
                        Ok(()) => {
                            previous.map_or(ReceiveCommandResult::Installed(identity), |previous| {
                                ReceiveCommandResult::ReplacementPrepared { previous, identity }
                            })
                        }
                        Err((error, receiver)) => {
                            ReceiveCommandResult::InstallRejected { error, receiver }
                        }
                    });
                }
                Command::Arm { identity, reply } => {
                    let outcome = match self.shard.session_mut(identity) {
                        None => ReceiveCommandResult::ArmRejected {
                            identity,
                            error: ReceiveError::Identity,
                        },
                        Some(owner) if owner.phase == ReceivePlanPhase::Active => {
                            ReceiveCommandResult::Armed(identity)
                        }
                        Some(owner) if owner.phase != ReceivePlanPhase::Prepared => {
                            ReceiveCommandResult::ArmRejected {
                                identity,
                                error: ReceiveError::Configuration,
                            }
                        }
                        Some(owner)
                            if self.history.latest_input().is_some_and(|latest| {
                                latest > owner.receiver.first_receive().sample().index
                            }) =>
                        {
                            ReceiveCommandResult::ArmRejected {
                                identity,
                                error: ReceiveError::Time,
                            }
                        }
                        Some(owner) => {
                            let activated = if owner.receiver.outer_loop_settings.is_some() {
                                self.power_links
                                    .activate(identity, owner.session().frame_anchor().time)
                                    .map_err(ReceiveError::PowerControl)
                            } else {
                                Ok(())
                            };
                            match activated {
                                Ok(()) => {
                                    owner.phase = ReceivePlanPhase::Active;
                                    ReceiveCommandResult::Armed(identity)
                                }
                                Err(error) => ReceiveCommandResult::ArmRejected { identity, error },
                            }
                        }
                    };
                    reply.finish(outcome);
                }
                Command::Remove { identity, reply } => {
                    let receiver = self.shard.remove(identity).map(|owner| owner.receiver);
                    if receiver
                        .as_ref()
                        .is_some_and(|owner| owner.outer_loop_settings.is_some())
                    {
                        self.power_links
                            .retire(identity)
                            .expect("installed power-link owner");
                    }
                    if self.first_retired_feedback.is_none()
                        && let Some(status) = receiver
                            .as_ref()
                            .and_then(|receiver| receiver.power_control_status())
                        && (status.counters.produced != 0
                            || status.counters.stale_observations != 0
                            || status.counters.unusable_observations != 0)
                    {
                        let power = receiver
                            .as_ref()
                            .and_then(|receiver| receiver.power.as_ref())
                            .expect("power status owner");
                        self.first_retired_feedback = Some(RetiredPilotFeedback {
                            identity,
                            latest_input_sample: self.history.latest_input(),
                            counters: status.counters,
                            last_accepted: status.last_accepted,
                            last_sir_linear: status.last_sir_linear,
                            last_discard: power.last_discard,
                            failed_observation: power.failed_observation,
                            maximum_observation_age_chips: power.maximum_observation_age_chips,
                            maximum_publication_age_chips: power.maximum_publication_age_chips,
                        });
                    }
                    reply.finish(ReceiveCommandResult::Removed { identity, receiver })
                }
                Command::Commit {
                    previous,
                    identity,
                    reply,
                } => {
                    reply.finish(match self.commit_replacement(previous, identity) {
                        Ok(boundary) => ReceiveCommandResult::ReplacementCommitted {
                            previous,
                            identity,
                            boundary,
                        },
                        Err(error) => ReceiveCommandResult::CommitRejected {
                            previous,
                            identity,
                            error,
                        },
                    });
                }
                Command::RetireDrained { identity, reply } => {
                    let result = self
                        .shard
                        .session_mut(identity)
                        .ok_or(ReceiveError::Identity)
                        .and_then(|owner| {
                            let ReceivePlanPhase::Stopped { next, boundary } = owner.phase else {
                                return Err(ReceiveError::Occupied);
                            };
                            if owner.receiver.next_frame().index < boundary.index
                                || owner.session().pending_completed_ttis() != 0
                                || owner.session().queued() != 0
                                || owner.receiver.hsupa_pending()
                            {
                                tracing::debug!(
                                    "Drained receiver still occupied: identity={identity:?} before_boundary={} completed_ttis={} queued={} hsupa_pending={}",
                                    owner.receiver.next_frame().index < boundary.index,
                                    owner.session().pending_completed_ttis(),
                                    owner.session().queued(),
                                    owner.receiver.hsupa_pending()
                                );
                                return Err(ReceiveError::Occupied);
                            }
                            Ok(ReceiveRetirement {
                                previous: identity,
                                old_anchor: owner.session().frame_anchor(),
                                successor: next,
                                boundary,
                            })
                        });
                    reply.finish(match result {
                        Ok(retirement) => {
                            let receiver = self
                                .shard
                                .remove(identity)
                                .expect("validated drained owner")
                                .receiver;
                            if receiver.outer_loop_settings.is_some() {
                                self.power_links
                                    .retire(identity)
                                    .expect("drained power-link owner");
                            }
                            // The successor owns the uplink alone now: it leaves the committed
                            // replacement state and becomes the active plan, so later changes
                            // and E-DCH attachment treat it like any other active receiver.
                            if let Some(successor) = self.shard.session_mut(retirement.successor)
                                && successor.phase
                                    == (ReceivePlanPhase::Committed { previous: identity })
                            {
                                successor.phase = ReceivePlanPhase::Active;
                            }
                            ReceiveCommandResult::Retired {
                                identity,
                                retirement,
                                receiver,
                            }
                        }
                        Err(error) => ReceiveCommandResult::RetireRejected { identity, error },
                    });
                }
                Command::Observe { identity, reply } => {
                    reply.finish(ReceiveCommandResult::Observed {
                        identity,
                        status: self
                            .shard
                            .session_mut(identity)
                            .map(|owner| ReceivePlanStatus {
                                restored_dch_at: owner.receiver.restored_dch_at(),
                                phase: owner.phase,
                                next_frame: owner.receiver.next_frame(),
                                last_consumed_frame: owner.receiver.last_consumed_frame(),
                                completed_ttis: owner.session().pending_completed_ttis(),
                                queued_transports: owner.session().queued(),
                                chips: owner.receiver.chip_counters(),
                                transports: owner.session().counters(),
                                transport_details: owner.session().transport_counters(),
                                power_control: owner.receiver.power_control_status(),
                                outer_loop: self.power_links.status(identity).ok(),
                                hsupa_pending: owner.receiver.hsupa_pending(),
                                samples: owner.receiver.counters(),
                                tracking: owner.receiver.tracking_status(),
                                hsupa_dropped_intervals: owner.receiver.hsupa_dropped_intervals(),
                            }),
                    });
                }
                Command::HsupaSource { identity, reply } => {
                    let transient = self.shard.session_mut(identity).is_some_and(|owner| {
                        owner.phase != ReceivePlanPhase::Active
                            && !owner.session().failed
                            && !owner.session().retired
                            && owner.receiver.hsupa_attachment().is_none()
                    });
                    let source = self
                        .shard
                        .session_mut(identity)
                        .filter(|owner| {
                            owner.phase == ReceivePlanPhase::Active
                                && !owner.session().failed
                                && !owner.session().retired
                                && owner.receiver.hsupa_attachment().is_none()
                        })
                        .map(|owner| HsupaReceiveSource {
                            code: owner.receiver.shared_uplink_code(),
                            anchor: owner.receiver.session().frame_anchor(),
                            control_search: owner.receiver.control_search_geometry(),
                        });
                    if source.is_none() {
                        let owner = self.shard.session_mut(identity);
                        tracing::debug!(
                            "HSUPA source unavailable: identity={identity:?} phase={:?} failed={:?} retired={:?} attached={:?}",
                            owner.as_ref().map(|owner| owner.phase),
                            owner.as_ref().map(|owner| owner.session().failed),
                            owner.as_ref().map(|owner| owner.session().retired),
                            owner
                                .as_ref()
                                .map(|owner| owner.receiver.hsupa_attachment().is_some()),
                        );
                    }
                    reply.finish(ReceiveCommandResult::HsupaSource {
                        identity,
                        source,
                        transient,
                    });
                }
            }
        }
        let feedback = &mut self.feedback;
        let feedback_queue = &mut self.feedback_queue;
        let feedback_queue_capacity = self.feedback_queue_capacity;
        let latest_input_sample = self.history.latest_input();
        let sample_provenance = self.history.provenance();
        self.service_results.clear();
        let service_results = &mut self.service_results;
        let power_links = &self.power_links;
        let serviced = self.shard.service_with(
            |owner| {
                let result: Result<SampleProgress, ReceiveError> = (|| {
                    if matches!(
                        owner.phase,
                        ReceivePlanPhase::Prepared | ReceivePlanPhase::Staged { .. }
                    ) {
                        return Ok(SampleProgress::AwaitSamples);
                    }
                    let at_boundary = match owner.phase {
                        ReceivePlanPhase::Stopped { .. } => true,
                        ReceivePlanPhase::Draining { boundary, .. } => {
                            owner.receiver.next_frame().index >= boundary.index
                        }
                        _ => false,
                    };
                    if owner.receiver.outer_loop_settings.is_some() {
                        let status = power_links
                            .status(owner.session().identity())
                            .map_err(ReceiveError::PowerControl)?;
                        owner
                            .receiver
                            .power
                            .as_mut()
                            .ok_or(ReceiveError::Configuration)?
                            .controller
                            .set_target(status.target);
                    }
                    if !at_boundary && let Some(power) = &mut owner.receiver.power {
                        let Some(source) = feedback.as_mut().filter(|source| !source.failed) else {
                            power.fail(ReceiveError::FeedbackClosed);
                            return Err(ReceiveError::FeedbackClosed);
                        };
                        let _ = source.now()?;
                    }
                    let clock = feedback
                        .as_ref()
                        .filter(|source| !source.failed)
                        .map(|source| &source.clock);
                    let progress = match owner.phase {
                        ReceivePlanPhase::Draining { boundary, .. } => {
                            owner.receiver.step_with_feedback(
                                &self.history,
                                &self.sampler,
                                Some(boundary),
                                clock,
                            )?
                        }
                        ReceivePlanPhase::Stopped { boundary, .. } => {
                            SampleProgress::Boundary(boundary)
                        }
                        _ => owner.receiver.step_with_feedback(
                            &self.history,
                            &self.sampler,
                            None,
                            clock,
                        )?,
                    };
                    if let Some(observation) = owner.receiver.downlink_control.take()
                        && let Some(source) = feedback.as_mut().filter(|source| !source.failed)
                        && source.port.publish_downlink_control(observation).is_err()
                    {
                        owner.receiver.downlink_feedback_dropped();
                    }
                    if matches!(progress, SampleProgress::Boundary(_))
                        && let ReceivePlanPhase::Draining { next, boundary } = owner.phase
                    {
                        owner.session_mut().discard_incomplete_ttis();
                        owner.receiver.discard_incomplete_hsupa();
                        owner.phase = ReceivePlanPhase::Stopped { next, boundary };
                    }
                    Ok(progress)
                })();
                if let Err(error) = &result
                    && let Some(power) = &mut owner.receiver.power
                {
                    power.fail(error.clone());
                }
                let identity = owner.receiver.session().identity();
                if let Some(power) = &mut owner.receiver.power
                    && power.needs_queue()
                    && feedback_queue.len() < feedback_queue_capacity
                {
                    feedback_queue.push_back(identity);
                    power.queued = true;
                }
                result
            },
            |identity, result| {
                if let Err(error) = &result {
                    self.first_failure
                        .get_or_insert_with(|| SampleRuntimeFailure {
                            identity,
                            error: error.clone(),
                            latest_input_sample,
                            sample_provenance,
                        });
                }
                assert!(service_results.len() < service_results.capacity());
                service_results.push((identity, result));
            },
        );
        let mut feedback_submitted = 0u64;
        for _ in 0..self.command_budget {
            let Some(identity) = self.feedback_queue.front().copied() else {
                break;
            };
            let Some(owner) = self.shard.session_mut(identity) else {
                self.feedback_queue.pop_front();
                continue;
            };
            let Some(power) = &mut owner.receiver.power else {
                self.feedback_queue.pop_front();
                continue;
            };
            if power.pending.is_none() || power.failed() {
                power.queued = false;
                self.feedback_queue.pop_front();
                continue;
            }
            let result = (|| {
                let provenance = power.pending.expect("queued feedback").provenance;
                if self.history.provenance()
                    != Some((provenance.sample_generation, provenance.continuity))
                {
                    power.retire_pending();
                    return Ok(Some(false));
                }
                let source = self.feedback.as_mut().ok_or(ReceiveError::FeedbackClosed)?;
                let Some(now) = source.now()? else {
                    return Ok(None);
                };
                power.publish(&mut source.port, now).map(Some)
            })();
            match result {
                Ok(Some(true)) => {
                    feedback_submitted += 1;
                    power.queued = false;
                    self.feedback_queue.pop_front();
                }
                Ok(Some(false)) if power.pending.is_none() => {
                    power.queued = false;
                    self.feedback_queue.pop_front();
                }
                Ok(_) => break,
                Err(error) => {
                    power.fail(error.clone());
                    power.queued = false;
                    self.feedback_queue.pop_front();
                    if self.shard.mark_failed(identity) {
                        if let Some((_, result)) = self
                            .service_results
                            .iter_mut()
                            .find(|(serviced, _)| *serviced == identity)
                        {
                            *result = Err(error);
                            continue;
                        }
                        self.counters.failed_owners = self.counters.failed_owners.saturating_add(1);
                        self.first_failure
                            .get_or_insert_with(|| SampleRuntimeFailure {
                                identity,
                                error: error.clone(),
                                latest_input_sample,
                                sample_provenance,
                            });
                        observe(identity, Err(error));
                    }
                }
            }
        }
        // Pilot feedback must reach TX before transport FEC can occupy this worker.
        if !self.service_results.is_empty() {
            let length = self.service_results.len();
            let offset = self
                .decode_after
                .and_then(|identity| {
                    self.service_results
                        .iter()
                        .position(|(serviced, _)| *serviced == identity)
                })
                .map_or(self.decode_cursor % length, |index| (index + 1) % length);
            self.service_results.rotate_left(offset);
            self.decode_cursor = (offset + 1) % length;
        }
        for (identity, mut result) in self.service_results.drain(..) {
            if result.is_ok() {
                let owner = self
                    .shard
                    .session_mut(identity)
                    .expect("serviced receive owner");
                if let Err(error) = decode(&mut owner.receiver, &mut self.power_links) {
                    if let Some(power) = &mut owner.receiver.power {
                        power.fail(error.clone());
                    }
                    self.shard.mark_failed(identity);
                    result = Err(error);
                }
            }
            if matches!(result, Ok(SampleProgress::Slot(_))) {
                self.counters.sampled_slots = self.counters.sampled_slots.saturating_add(1);
            }
            if matches!(result, Ok(SampleProgress::AwaitFeedback)) {
                self.counters.power_feedback_waits =
                    self.counters.power_feedback_waits.saturating_add(1);
            }
            if let Err(error) = &result {
                self.counters.failed_owners = self.counters.failed_owners.saturating_add(1);
                self.first_failure
                    .get_or_insert_with(|| SampleRuntimeFailure {
                        identity,
                        error: error.clone(),
                        latest_input_sample,
                        sample_provenance,
                    });
            }
            observe(identity, result);
        }
        self.counters.serviced = self.counters.serviced.saturating_add(serviced as u64);
        self.counters.feedback_submitted = self
            .counters
            .feedback_submitted
            .saturating_add(feedback_submitted);
        self.publish_one();
        serviced
    }
    fn install_owner(
        &mut self,
        owner: Box<RoutedOwner>,
        previous: Option<ReceiveIdentity>,
        armed: bool,
    ) -> Result<(), (ReceiveError, Box<RoutedOwner>)> {
        let identity = owner.session().identity();
        // Any control format fits within one slot plus its search delays.
        // Keep enough mirror for later E-DCH plan exchange as well as sampling.
        let history_window = owner
            .receiver
            .required_history(self.sampler.guard_samples())
            .max(
                umts_types::time::CHIPS_PER_SLOT as usize * self.rate
                    + 2 * owner.receiver.search_radius()
                    + 2 * self.sampler.guard_samples()
                    + 1,
            );
        let policy = owner.receiver.outer_loop_settings;
        if let Some(settings) = policy {
            if let Err(error) = self.power_links.attach(identity, settings) {
                return Err((ReceiveError::PowerControl(error), owner));
            }
            if armed
                && previous.is_none()
                && let Err(error) = self
                    .power_links
                    .activate(identity, owner.session().frame_anchor().time)
            {
                self.power_links
                    .retire(identity)
                    .expect("new power-link owner");
                return Err((ReceiveError::PowerControl(error), owner));
            }
        }
        let result = match previous {
            Some(previous) => self.shard.stage_replacement(previous, owner),
            None => self.shard.install(owner),
        };
        if result.is_ok() {
            self.history.ensure_window(history_window);
        }
        if result.is_err() && policy.is_some() {
            self.power_links
                .retire(identity)
                .expect("rejected power-link owner");
        }
        result
    }

    fn commit_replacement(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<super::FrameAnchor, ReceiveError> {
        if previous.ue != identity.ue {
            return Err(ReceiveError::Identity);
        }
        let next = self
            .shard
            .session_mut(identity)
            .ok_or(ReceiveError::Identity)?;
        if next.phase != (ReceivePlanPhase::Staged { previous }) {
            return Err(ReceiveError::Configuration);
        }
        let boundary = next.session().frame_anchor();
        let position = next.receiver.first_receive();
        let radius = next.receiver.search_radius();
        let next_policy = next.receiver.outer_loop_settings;
        let feedback_boundary = next
            .receiver
            .power
            .as_ref()
            .map(|power| power.downlink_anchor());
        if self
            .history
            .latest_input()
            .is_some_and(|latest| latest > next.receiver.first_receive().sample().index)
        {
            return Err(ReceiveError::Time);
        }
        self.replacement_timing(previous, boundary.time, position, radius)?;
        let old = self
            .shard
            .session_mut(previous)
            .ok_or(ReceiveError::Identity)?;
        if !matches!(
            old.phase,
            ReceivePlanPhase::Active | ReceivePlanPhase::Committed { .. }
        ) {
            return Err(ReceiveError::Configuration);
        }
        if old.receiver.outer_loop_settings != next_policy {
            return Err(ReceiveError::Configuration);
        }
        if next_policy.is_some() {
            self.power_links
                .validate_replacement(previous, identity, boundary.time)
                .map_err(ReceiveError::PowerControl)?;
        }
        if old
            .receiver
            .last_consumed_frame()
            .is_some_and(|last| last.index >= boundary.time.index)
        {
            return Err(ReceiveError::Time);
        }
        if let Some(power) = &mut old.receiver.power {
            power.stop_at(feedback_boundary.ok_or(ReceiveError::Configuration)?)?;
        }
        if next_policy.is_some() {
            self.power_links
                .replace(previous, identity, boundary.time)
                .expect("validated power-link replacement");
        }
        old.phase = ReceivePlanPhase::Draining {
            next: identity,
            boundary: boundary.time,
        };
        self.shard
            .session_mut(identity)
            .expect("validated pending owner")
            .phase = ReceivePlanPhase::Committed { previous };
        Ok(boundary)
    }
    fn replacement_timing(
        &mut self,
        previous: ReceiveIdentity,
        boundary: umts_types::time::ChipInstant,
        position: umts_dsp::fractional_sampling::SamplePosition,
        radius: usize,
    ) -> Result<(), ReceiveError> {
        let old = self
            .shard
            .session_mut(previous)
            .ok_or(ReceiveError::Identity)?;
        let expected = old.receiver.predicted_receive(boundary)?;
        let tolerance = old
            .receiver
            .search_radius()
            .checked_add(radius)
            .ok_or(ReceiveError::Capacity)?;
        let delta = (i128::from(position.sample().index) - i128::from(expected.sample().index))
            as f64
            + position.fraction()
            - expected.fraction();
        if position.sample().epoch != expected.sample().epoch || delta.abs() > tolerance as f64 {
            return Err(ReceiveError::Time);
        }
        Ok(())
    }
    fn publish_one(&mut self) {
        for _ in 0..self.publish_budget {
            let index = self.publish_cursor;
            self.publish_cursor = (self.publish_cursor + 1) % self.maximum_ues;
            let Some(owner) = self.shard.owner_at_index_mut(index) else {
                continue;
            };
            if matches!(
                owner.phase,
                ReceivePlanPhase::Prepared | ReceivePlanPhase::Staged { .. }
            ) {
                continue;
            }
            match owner.output.publish_one(owner.receiver.session_mut()) {
                Publication::Published => {
                    self.counters.published = self.counters.published.saturating_add(1);
                    return;
                }
                Publication::Backpressure => {
                    self.counters.backpressure = self.counters.backpressure.saturating_add(1)
                }
                Publication::Idle | Publication::Closed => {}
            }
        }
    }
}
impl Drop for DedicatedSampleRuntime {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        for _ in 0..MAX_COMMANDS {
            if self.commands.try_recv().is_none() {
                break;
            }
        }
    }
}
