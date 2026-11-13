// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::{
    num::NonZeroU64,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
};
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_types::UeKey;

mod acquisition;
use acquisition::AcquisitionBridge;
pub(super) use acquisition::PendingAcquisition;
pub use acquisition::{AcquisitionCancellation, AcquisitionEvent, AcquisitionPort, ControlPorts};

const QUEUED: u8 = 1;
const SCHEDULED: u8 = 2;
const COMMITTED: u8 = 3;
const CANCELLED: u8 = 4;
const REJECTED: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlanLane {
    Transport,
    Acquisition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlanId {
    epoch: umts_types::RadioEpoch,
    lane: PlanLane,
    serial: NonZeroU64,
}
impl PlanId {
    pub fn get(self) -> u64 {
        self.serial.get()
    }
    pub fn epoch(self) -> umts_types::RadioEpoch {
        self.epoch
    }
    pub fn lane(self) -> PlanLane {
        self.lane
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PlanRejection {
    Late,
    Capacity,
    Conflict,
    Time,
    Channel,
    Payload,
    Configuration,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStage {
    Queued,
    Scheduled,
    Committed,
    Cancelled,
    Rejected(PlanRejection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanEvent {
    pub id: PlanId,
    pub owner: Option<UeKey>,
    pub start: ChipInstant,
    pub channel: usize,
    pub stage: PlanStage,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cancellation {
    pub cancelled: usize,
    pub committed: usize,
}

#[derive(Debug)]
struct PlanState {
    id: PlanId,
    owner: Option<UeKey>,
    stage: AtomicU8,
}

impl PlanState {
    fn stage(&self) -> PlanStage {
        match self.stage.load(Ordering::Acquire) {
            QUEUED => PlanStage::Queued,
            SCHEDULED => PlanStage::Scheduled,
            COMMITTED => PlanStage::Committed,
            CANCELLED => PlanStage::Cancelled,
            value => PlanStage::Rejected(match value - REJECTED {
                0 => PlanRejection::Late,
                1 => PlanRejection::Capacity,
                2 => PlanRejection::Conflict,
                3 => PlanRejection::Time,
                4 => PlanRejection::Channel,
                5 => PlanRejection::Payload,
                6 => PlanRejection::Configuration,
                _ => PlanRejection::Closed,
            }),
        }
    }
    fn terminate(&self, terminal: u8) -> PlanStage {
        let mut previous = self.stage.load(Ordering::Acquire);
        for _ in 0..3 {
            if !matches!(previous, QUEUED | SCHEDULED) {
                break;
            }
            match self.stage.compare_exchange(
                previous,
                terminal,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => previous = current,
            }
        }
        self.stage()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PlanReceipt(Arc<PlanState>);
impl PlanReceipt {
    pub fn stage(&self) -> PlanStage {
        self.0.stage()
    }
    pub fn id(&self) -> PlanId {
        self.0.id
    }
}

pub(super) struct PlanWriter(Arc<PlanState>);
impl PlanWriter {
    fn schedule(&self) -> bool {
        self.0
            .stage
            .compare_exchange(QUEUED, SCHEDULED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    pub fn commit(&self) -> bool {
        match self.0.stage.compare_exchange(
            SCHEDULED,
            COMMITTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => true,
            Err(stage) => stage == COMMITTED,
        }
    }
    fn reject(&self, reason: PlanRejection) {
        self.0.terminate(REJECTED + reason as u8);
    }
    pub fn cancelled(&self) -> bool {
        self.0.stage() == PlanStage::Cancelled
    }
}
impl Drop for PlanWriter {
    fn drop(&mut self) {
        self.reject(PlanRejection::Closed);
    }
}

struct Command {
    start: ChipInstant,
    channel: usize,
    payload: Vec<Bit>,
    payload_bits: usize,
    indicators: [bool; MAX_PAGING_INDICATORS],
    indicator_count: usize,
    plan: PlanWriter,
}

struct Entry {
    state: Arc<PlanState>,
    start: ChipInstant,
    channel: usize,
    reported: PlanStage,
}

pub struct CommonTxPort {
    information: Arc<CellInformation>,
    epoch: umts_types::RadioEpoch,
    cursor: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    progress: crate::tx_evidence::Reader,
    rate: usize,
    group_delay: usize,
    ready: Sender<Command>,
    free: Receiver<Vec<Bit>>,
    recycle: Sender<Vec<Bit>>,
    entries: Vec<Entry>,
    next_id: Option<NonZeroU64>,
    event_cursor: usize,
    cancel_cursor: usize,
}

impl CommonTxPort {
    pub(crate) fn belongs_to(&self, source: &CommonTx) -> bool {
        source
            .control
            .as_ref()
            .is_some_and(|bridge| Arc::ptr_eq(&bridge.alive, &self.alive))
    }
}

pub(super) struct ControlBridge {
    cursor: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    ready: Receiver<Command>,
    free: Sender<Vec<Bit>>,
    capacity: usize,
    budget: usize,
    acquisition: Option<AcquisitionBridge>,
}

impl CommonTx {
    #[cfg(test)]
    pub(crate) fn control_port(
        &mut self,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
    ) -> Result<CommonTxPort, CommonTxError> {
        self.make_control_ports(capacity, budget, None)
            .map(|(transport, _)| transport)
    }

    pub(crate) fn control_ports(
        &mut self,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
        acquisition_capacity: NonZeroUsize,
        acquisition_budget: NonZeroUsize,
    ) -> Result<ControlPorts, CommonTxError> {
        let (transport, acquisition) = self.make_control_ports(
            capacity,
            budget,
            Some((acquisition_capacity, acquisition_budget)),
        )?;
        Ok(ControlPorts {
            transport,
            acquisition: acquisition.expect("requested access lane"),
        })
    }

    fn make_control_ports(
        &mut self,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
        acquisition_limits: Option<(NonZeroUsize, NonZeroUsize)>,
    ) -> Result<(CommonTxPort, Option<AcquisitionPort>), CommonTxError> {
        if self.poisoned || !self.progress.available() {
            return Err(CommonTxError::Poisoned);
        }
        if self.control.is_some()
            || self.submission_writer.is_some()
            || !self.next.index.is_multiple_of(CHIPS_PER_FRAME)
            || self.pending.iter().any(|entry| entry.occupied)
            || self.access.iter().any(|stream| {
                stream.calendar.pending() != 0 || stream.plans.iter().any(Option::is_some)
            })
        {
            return Err(CommonTxError::Conflict);
        }
        if capacity.get() > self.pending.len() || budget.get() > capacity.get() {
            return Err(CommonTxError::Capacity);
        }
        let alive = Arc::new(AtomicBool::new(true));
        let cursor = Arc::new(AtomicU64::new(self.next.index));
        let access = acquisition_limits
            .map(|(capacity, budget)| {
                AcquisitionPort::prepare(self, capacity, budget, cursor.clone(), alive.clone())
            })
            .transpose()?;
        let (ready, receive) = mailbox::bounded(capacity);
        let (recycle, free) = mailbox::bounded(capacity);
        let mut entries = Vec::with_capacity(capacity.get());
        for _ in 0..capacity.get() {
            recycle
                .try_send(vec![Bit::Zero; MAX_PAYLOAD_BITS])
                .map_err(|_| CommonTxError::Capacity)?;
            entries.push(Entry {
                state: Arc::new(PlanState {
                    id: PlanId {
                        epoch: self.next.epoch,
                        lane: PlanLane::Transport,
                        serial: NonZeroU64::MIN,
                    },
                    owner: None,
                    stage: AtomicU8::new(REJECTED + PlanRejection::Closed as u8),
                }),
                start: self.next,
                channel: 0,
                reported: PlanStage::Rejected(PlanRejection::Closed),
            });
        }
        let (acquisition, acquisition_bridge) = match access {
            Some((port, bridge)) => {
                self.next_acquisition_id = None;
                (Some(port), Some(bridge))
            }
            None => (None, None),
        };
        self.control = Some(ControlBridge {
            cursor: cursor.clone(),
            alive: alive.clone(),
            ready: receive,
            free: recycle.clone(),
            capacity: capacity.get(),
            budget: budget.get(),
            acquisition: acquisition_bridge,
        });
        Ok((
            CommonTxPort {
                information: self.information.clone(),
                epoch: self.next.epoch,
                cursor,
                alive,
                progress: self.progress.clone(),
                rate: self.rate,
                group_delay: self.group_delay_samples(),
                ready,
                free,
                recycle,
                entries,
                next_id: Some(NonZeroU64::MIN),
                event_cursor: 0,
                cancel_cursor: 0,
            },
            acquisition,
        ))
    }

    pub(super) fn service_control(&mut self) -> Result<(), CommonTxError> {
        if let Err(error) = self.clean_acquisitions() {
            self.poisoned = true;
            return Err(error);
        }
        let Some(mut bridge) = self.control.take() else {
            return Ok(());
        };
        let result = if !bridge.alive.load(Ordering::Acquire) {
            Err(CommonTxError::Poisoned)
        } else {
            if let Some(access) = &mut bridge.acquisition {
                access.service(self);
            }
            for entry in &mut self.pending {
                if entry.plan.as_ref().is_some_and(PlanWriter::cancelled) {
                    entry.occupied = false;
                    entry.plan = None;
                }
            }
            for _ in 0..bridge.budget {
                let Some(command) = bridge.ready.try_recv() else {
                    break;
                };
                let Command {
                    start,
                    channel,
                    payload,
                    payload_bits,
                    indicators,
                    indicator_count,
                    plan,
                } = command;
                if plan.schedule() {
                    match self.queue_transport_inner(
                        start,
                        channel,
                        &payload[..payload_bits],
                        &indicators[..indicator_count],
                    ) {
                        Ok(index) => self.pending[index].plan = Some(plan),
                        Err(error) => plan.reject(rejection(error)),
                    }
                }
                if bridge.free.try_send(payload).is_err() {
                    self.poisoned = true;
                    break;
                }
            }
            if self.poisoned {
                bridge.alive.store(false, Ordering::Release);
                Err(CommonTxError::Poisoned)
            } else {
                Ok(())
            }
        };
        self.control = Some(bridge);
        result
    }
}

impl ControlBridge {
    pub fn advance(&self, next: u64) {
        self.cursor.store(next, Ordering::Release);
    }
}
impl Drop for ControlBridge {
    fn drop(&mut self) {
        self.alive.swap(false, Ordering::AcqRel);
        for _ in 0..self.capacity {
            let Some(command) = self.ready.try_recv() else {
                break;
            };
            let _ = self.free.try_send(command.payload);
        }
    }
}

impl CommonTxPort {
    pub fn cancel_orphaned_some(
        &mut self,
        budget: NonZeroUsize,
        mut live: impl FnMut(UeKey) -> bool,
    ) -> usize {
        let mut cancelled = 0;
        for _ in 0..budget.get().min(self.entries.len()) {
            let index = self.cancel_cursor;
            self.cancel_cursor = (index + 1) % self.entries.len();
            let state = &self.entries[index].state;
            if state.owner.is_some_and(|key| !live(key))
                && matches!(state.stage(), PlanStage::Queued | PlanStage::Scheduled)
                && state.terminate(CANCELLED) == PlanStage::Cancelled
            {
                cancelled += 1;
            }
        }
        cancelled
    }

    pub fn next_event(&mut self) -> Option<PlanEvent> {
        for _ in 0..self.entries.len() {
            let index = self.event_cursor;
            self.event_cursor = (index + 1) % self.entries.len();
            let entry = &mut self.entries[index];
            let stage = entry.state.stage();
            if stage != entry.reported {
                entry.reported = stage;
                return Some(PlanEvent {
                    id: entry.state.id,
                    owner: entry.state.owner,
                    start: entry.start,
                    channel: entry.channel,
                    stage,
                });
            }
        }
        None
    }

    pub fn cancel_ue(&mut self, key: UeKey) -> Cancellation {
        let mut result = Cancellation::default();
        for entry in &self.entries {
            if entry.state.owner == Some(key) {
                let before = entry.state.stage();
                let after = entry.state.terminate(CANCELLED);
                if matches!(before, PlanStage::Queued | PlanStage::Scheduled)
                    && after == PlanStage::Cancelled
                {
                    result.cancelled += 1;
                } else if after == PlanStage::Committed {
                    result.committed += 1;
                }
            }
        }
        result
    }
}

impl CommonTxTarget for CommonTxPort {
    fn available(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.progress.available()
    }
    fn information(&self) -> &CellInformation {
        &self.information
    }
    fn next_chip(&self) -> ChipInstant {
        ChipInstant {
            epoch: self.epoch,
            index: self.cursor.load(Ordering::Acquire),
        }
    }
    fn publish_transport(
        &mut self,
        start: ChipInstant,
        channel: usize,
        payload: &[Bit],
        indicators: &[bool],
        owner: Option<UeKey>,
    ) -> Result<crate::tx_evidence::TransportSubmission, CommonTxError> {
        if !self.alive.load(Ordering::Acquire) || !self.progress.available() {
            return Err(CommonTxError::Poisoned);
        }
        let cursor = self.next_chip();
        if start.epoch != self.epoch
            || owner.is_some_and(|key| key.epoch != self.epoch)
            || !start.index.is_multiple_of(CHIPS_PER_FRAME)
        {
            return Err(CommonTxError::Time);
        }
        let stream = self
            .information
            .sib5()
            .secondary_channels()
            .get(channel)
            .ok_or(CommonTxError::Channel)?;
        let [transport] = stream.transports() else {
            return Err(CommonTxError::Channel);
        };
        if payload.len() != usize::from(transport.formats.groups[0].transport_block_bits) {
            return Err(CommonTxError::Payload);
        }
        let deadline = if let Some(pich) = &stream.settings().pich {
            let count = match pich.indicators {
                PagingIndicators::PerFrame18 => 18,
                PagingIndicators::PerFrame36 => 36,
                PagingIndicators::PerFrame72 => 72,
                PagingIndicators::PerFrame144 => 144,
            };
            if indicators.len() != count {
                return Err(CommonTxError::Payload);
            }
            paging_start(start)?.index
        } else {
            if !indicators.is_empty() {
                return Err(CommonTxError::Payload);
            }
            start.index
        };
        if deadline < cursor.index {
            return Err(CommonTxError::Late);
        }
        if start.index - cursor.index > MAX_PENDING_TRANSPORTS as u64 * CHIPS_PER_FRAME {
            return Err(CommonTxError::Capacity);
        }
        if self.entries.iter().any(|entry| {
            entry.start == start
                && entry.channel == channel
                && matches!(
                    entry.state.stage(),
                    PlanStage::Queued | PlanStage::Scheduled | PlanStage::Committed
                )
        }) {
            return Err(CommonTxError::Conflict);
        }
        let index = self
            .entries
            .iter()
            .position(|entry| {
                let stage = entry.state.stage();
                entry.reported == stage
                    && !matches!(stage, PlanStage::Queued | PlanStage::Scheduled)
                    && Arc::strong_count(&entry.state) == 1
            })
            .ok_or(CommonTxError::Capacity)?;
        let id = self.next_id.ok_or(CommonTxError::Capacity)?;
        let proof = self
            .progress
            .interval_receipt(
                ChipInstant {
                    index: deadline,
                    ..start
                },
                ChipInstant {
                    index: start
                        .index
                        .checked_add(CHIPS_PER_FRAME)
                        .ok_or(CommonTxError::Time)?,
                    ..start
                },
                self.rate,
                self.group_delay,
            )
            .ok_or(CommonTxError::Time)?;
        let mut buffer = self.free.try_recv().ok_or(CommonTxError::Capacity)?;
        buffer[..payload.len()].copy_from_slice(payload);
        let entry = &mut self.entries[index];
        let state = Arc::get_mut(&mut entry.state).expect("unreferenced plan slot");
        state.id = PlanId {
            epoch: self.epoch,
            lane: PlanLane::Transport,
            serial: id,
        };
        state.owner = owner;
        state.stage.store(QUEUED, Ordering::Release);
        entry.start = start;
        entry.channel = channel;
        entry.reported = PlanStage::Queued;
        self.next_id = id.checked_add(1);
        let receipt = PlanReceipt(entry.state.clone());
        let mut paging = [false; MAX_PAGING_INDICATORS];
        paging[..indicators.len()].copy_from_slice(indicators);
        let command = Command {
            start,
            channel,
            payload: buffer,
            payload_bits: payload.len(),
            indicators: paging,
            indicator_count: indicators.len(),
            plan: PlanWriter(entry.state.clone()),
        };
        if let Err(command) = self.ready.try_send(command) {
            command.plan.reject(PlanRejection::Capacity);
            let _ = self.recycle.try_send(command.payload);
            entry.reported = PlanStage::Rejected(PlanRejection::Capacity);
            return Err(CommonTxError::Capacity);
        }
        // This RMW pairs with consumer close: either its drain sees publication or this producer closes the plan.
        if !self.alive.fetch_and(true, Ordering::AcqRel) {
            entry
                .state
                .terminate(REJECTED + PlanRejection::Closed as u8);
        }
        Ok(proof.with_plan(receipt))
    }
}

impl Drop for CommonTxPort {
    fn drop(&mut self) {
        self.alive.swap(false, Ordering::AcqRel);
        for entry in &self.entries {
            entry
                .state
                .terminate(REJECTED + PlanRejection::Closed as u8);
        }
    }
}

fn rejection(error: CommonTxError) -> PlanRejection {
    match error {
        CommonTxError::Late => PlanRejection::Late,
        CommonTxError::Capacity => PlanRejection::Capacity,
        CommonTxError::Conflict => PlanRejection::Conflict,
        CommonTxError::Time => PlanRejection::Time,
        CommonTxError::Channel => PlanRejection::Channel,
        CommonTxError::Payload => PlanRejection::Payload,
        CommonTxError::Poisoned => PlanRejection::Closed,
        CommonTxError::Access(AccessScheduleError::Late) => PlanRejection::Late,
        CommonTxError::Access(AccessScheduleError::Capacity) => PlanRejection::Capacity,
        CommonTxError::Access(AccessScheduleError::Conflict) => PlanRejection::Conflict,
        CommonTxError::Access(
            AccessScheduleError::Epoch
            | AccessScheduleError::Occasion
            | AccessScheduleError::Arrival
            | AccessScheduleError::IncompletePreamble,
        ) => PlanRejection::Time,
        CommonTxError::Access(
            AccessScheduleError::Code
            | AccessScheduleError::Signature
            | AccessScheduleError::NoIndicator,
        ) => PlanRejection::Configuration,
        _ => PlanRejection::Configuration,
    }
}

#[cfg(test)]
mod tests;
