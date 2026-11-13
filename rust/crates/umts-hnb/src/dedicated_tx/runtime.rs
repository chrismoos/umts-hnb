// SPDX-License-Identifier: AGPL-3.0-or-later

use super::tpc_estimate::EstimateLane;
use super::*;
use std::{
    num::NonZeroU64,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, Thread},
};
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_types::RadioEpoch;

const MAX_COMMANDS: usize = 256;
const TPC_SLOTS: usize = super::ESTIMATE_SLOTS;

pub(super) mod grouped_tpc;
use grouped_tpc::GroupedTpc;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub struct TransmitRuntimeLimits {
    pub command_capacity: NonZeroUsize,
    pub commands_per_step: NonZeroUsize,
    pub feedback_capacity: NonZeroUsize,
    pub feedback_per_step: NonZeroUsize,
    pub maximum_frame_payload_bits: NonZeroUsize,
    pub maximum_inflight_retained_bits: NonZeroUsize,
}

pub struct OwnedTransportPayload {
    pub transport: TransportId,
    pub bits: Vec<Bit>,
}
#[must_use]
pub struct FrameInput {
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub tfci: Tfci,
    pub transports: Vec<OwnedTransportPayload>,
    pub(crate) emission: Option<Box<evidence::Emission>>,
    pub(crate) storage: Box<FrameStorage>,
    pub(crate) retry_after: Option<ChipInstant>,
}
pub(crate) struct FramePoolTag {
    pub owner: Arc<()>,
    pub slot: usize,
}
#[derive(Default)]
pub(crate) struct FrameStorage {
    pub spare: Vec<OwnedTransportPayload>,
    pub reclaimed: Option<Box<evidence::Emission>>,
    pub pool: Option<FramePoolTag>,
}
impl FrameInput {}
pub struct TpcPlan {
    pub identity: ReceiveIdentity,
    pub first_slot: ChipInstant,
    pub commands: Vec<Bit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpcDecision {
    pub identity: ReceiveIdentity,
    pub physical_slot_start: ChipInstant,
    pub command: Bit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Control,
    Feedback,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransmitCommandId {
    epoch: RadioEpoch,
    lane: Lane,
    serial: NonZeroU64,
}
impl TransmitCommandId {}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub enum TransmitCommandOutcome {
    Hsupa(crate::hsupa_tx::CommandOutcome),
    HsdpaQueued {
        key: umts_mac::hsdpa::TransmissionKey,
        result: Result<(), crate::hsdpa_tx::RejectedTransmission>,
    },
    HsdpaCompletion(Option<crate::hsdpa_tx::CompletedTransmission>),
    HsdpaOrderQueued {
        key: crate::hsdpa_tx::OrderIdentity,
        result: Result<(), crate::hsdpa_tx::RejectedOrder>,
    },
    HsdpaOrderCompletion(Option<crate::hsdpa_tx::CompletedOrder>),
    HsdpaRetirementStarted {
        ue: umts_types::UeKey,
        result: Result<(), crate::hsdpa_tx::Error>,
    },
    HsdpaRetirementFinished {
        ue: umts_types::UeKey,
        result: Result<bool, crate::hsdpa_tx::Error>,
    },
    Prepared(ReceiveIdentity),
    ReplacementPrepared {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    },
    ReplacementCommitted {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
        result: Result<FrameAnchor, TransmitError>,
    },
    Retired {
        identity: ReceiveIdentity,
        result: Result<RetiredTransmit, TransmitError>,
    },
    PrepareRejected {
        error: TransmitError,
        session: Box<DedicatedTransmitSession>,
    },
    Frame {
        input: FrameInput,
        result: Result<PreparedFrame, TransmitError>,
    },
    Armed {
        identity: ReceiveIdentity,
        result: Result<(), TransmitError>,
    },
    Removed {
        identity: ReceiveIdentity,
        result: Result<Box<DedicatedTransmitSession>, TransmitError>,
    },
    Feedback {
        plan: TpcPlan,
        result: Result<(), TransmitError>,
    },
    Decision {
        decision: TpcDecision,
        result: Result<(), TransmitError>,
    },
    Estimate {
        estimate: TpcEstimate,
        result: Result<(), TransmitError>,
    },
    Observed {
        identity: ReceiveIdentity,
        state: Option<(TransmitPhase, TransmitCounters, Option<TransmitError>)>,
        tpc: Option<super::TpcGroupCounters>,
    },
    Closed,
}
pub struct RetiredTransmit {
    pub session: Box<DedicatedTransmitSession>,
    pub submission: crate::tx_evidence::TransportResolution,
    previous: ReceiveIdentity,
    old_anchor: FrameAnchor,
    successor: ReceiveIdentity,
    boundary: ChipInstant,
}
impl RetiredTransmit {
    pub fn previous(&self) -> ReceiveIdentity {
        self.previous
    }
    pub fn old_anchor(&self) -> FrameAnchor {
        self.old_anchor
    }
    pub fn successor(&self) -> ReceiveIdentity {
        self.successor
    }
    pub fn boundary(&self) -> ChipInstant {
        self.boundary
    }
}
pub struct TransmitEvent {
    pub id: TransmitCommandId,
    pub outcome: TransmitCommandOutcome,
}
enum Action {
    Hsupa(crate::hsupa_tx::Command),
    Hsdpa(crate::hsdpa_tx::PreparedTransmission),
    TakeHsdpaCompletion,
    HsdpaOrder(crate::hsdpa_tx::PreparedOrder),
    TakeHsdpaOrderCompletion,
    BeginHsdpaRetirement(umts_types::UeKey),
    FinishHsdpaRetirement(umts_types::UeKey),
    Prepare(Box<DedicatedTransmitSession>),
    Stage {
        previous: ReceiveIdentity,
        session: Box<DedicatedTransmitSession>,
    },
    Commit {
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    },
    Retire(ReceiveIdentity),
    Frame(FrameInput),
    Arm(ReceiveIdentity),
    Remove(ReceiveIdentity),
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Feedback(TpcPlan),
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Decision(TpcDecision),
    Estimate(TpcEstimate),
    Observe(ReceiveIdentity),
}
impl Action {
    fn interrupts_wait(&self) -> bool {
        !matches!(
            self,
            Self::Observe(_)
                | Self::TakeHsdpaCompletion
                | Self::TakeHsdpaOrderCompletion
                | Self::Hsupa(crate::hsupa_tx::Command::TakeCompletion)
        )
    }
}
struct Command {
    id: TransmitCommandId,
    action: Action,
}
#[derive(Debug)]
pub(super) struct Link {
    producer: AtomicBool,
    consumer: AtomicBool,
    wake: OnceLock<Thread>,
    prepared_through: AtomicU64,
}
impl Link {
    fn wake(&self) {
        if let Some(worker) = self.wake.get() {
            worker.unpark();
        }
    }
    pub(super) fn available(&self) -> bool {
        self.producer.load(Ordering::Acquire) && self.consumer.load(Ordering::Acquire)
    }
}
#[derive(Clone, Copy)]
struct Pending {
    id: TransmitCommandId,
    retained: usize,
}
struct Producer {
    link: Arc<Link>,
    commands: Sender<Command>,
    events: Receiver<TransmitEvent>,
    pending: Vec<Option<Pending>>,
    epoch: RadioEpoch,
    lane: Lane,
    next: Option<NonZeroU64>,
    retained: usize,
    maximum_retained: usize,
    maximum_payload: usize,
}
struct Consumer {
    link: Arc<Link>,
    commands: Receiver<Command>,
    events: Sender<TransmitEvent>,
    budget: usize,
}
fn endpoints(
    epoch: RadioEpoch,
    lane: Lane,
    capacity: NonZeroUsize,
    budget: NonZeroUsize,
    maximum_retained: usize,
    maximum_payload: usize,
) -> (Producer, Consumer) {
    let (send, commands) = mailbox::bounded(capacity);
    let (events, receive) = mailbox::bounded(capacity);
    let link = Arc::new(Link {
        producer: AtomicBool::new(true),
        consumer: AtomicBool::new(true),
        wake: OnceLock::new(),
        prepared_through: AtomicU64::new(0),
    });
    (
        Producer {
            link: link.clone(),
            commands: send,
            events: receive,
            pending: vec![None; capacity.get()],
            epoch,
            lane,
            next: Some(NonZeroU64::MIN),
            retained: 0,
            maximum_retained,
            maximum_payload,
        },
        Consumer {
            link,
            commands,
            events,
            budget: budget.get(),
        },
    )
}
impl Producer {
    fn available(&self) -> bool {
        self.link.producer.load(Ordering::Acquire) && self.link.consumer.load(Ordering::Acquire)
    }
    fn post(&mut self, action: Action, retained: usize) -> Result<TransmitCommandId, Action> {
        let Some(serial) = self.next else {
            return Err(action);
        };
        let Some(slot) = self.pending.iter().position(Option::is_none) else {
            return Err(action);
        };
        let Some(total) = self
            .retained
            .checked_add(retained)
            .filter(|total| *total <= self.maximum_retained)
        else {
            return Err(action);
        };
        if !self.available() {
            return Err(action);
        }
        let id = TransmitCommandId {
            epoch: self.epoch,
            lane: self.lane,
            serial,
        };
        let interrupt = self.lane == Lane::Control && action.interrupts_wait();
        match self.commands.try_send(Command { id, action }) {
            Ok(()) => {
                self.pending[slot] = Some(Pending { id, retained });
                self.retained = total;
                self.next = serial.get().checked_add(1).and_then(NonZeroU64::new);
                if interrupt {
                    self.link.wake();
                }
                Ok(id)
            }
            Err(command) => Err(command.action),
        }
    }
    fn event(&mut self) -> Option<TransmitEvent> {
        let event = self.events.try_recv().or_else(|| {
            if self.link.consumer.load(Ordering::Acquire) {
                return None;
            }
            // Recheck after the close acquire so a last published result wins over synthetic closure.
            self.events.try_recv().or_else(|| {
                self.pending
                    .iter()
                    .flatten()
                    .next()
                    .map(|pending| TransmitEvent {
                        id: pending.id,
                        outcome: TransmitCommandOutcome::Closed,
                    })
            })
        })?;
        let slot = self
            .pending
            .iter()
            .position(|pending| pending.is_some_and(|pending| pending.id == event.id))
            .expect("one result for an admitted command");
        self.retained -= self.pending[slot].take().expect("pending command").retained;
        Some(event)
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        self.link.producer.store(false, Ordering::Release);
        self.link.wake();
    }
}

pub struct DedicatedTransmitPort(Producer, DedicatedFrameSource);
pub struct DedicatedTpcPort(Producer, Sender<DownlinkControl>);
pub struct DedicatedTransmitPorts {
    pub control: DedicatedTransmitPort,
    pub feedback: DedicatedTpcPort,
}
impl DedicatedTransmitPort {
    pub fn queue_hsupa(
        &mut self,
        command: crate::hsupa_tx::Command,
    ) -> Result<TransmitCommandId, crate::hsupa_tx::Command> {
        if !command.matches_epoch(self.0.epoch) {
            return Err(command);
        }
        let retained = command.retained_bits();
        self.0
            .post(Action::Hsupa(command), retained)
            .map_err(|action| match action {
                Action::Hsupa(command) => command,
                _ => unreachable!("HSUPA publication"),
            })
    }

    pub fn queue_hsdpa(
        &mut self,
        transmission: crate::hsdpa_tx::PreparedTransmission,
    ) -> Result<TransmitCommandId, crate::hsdpa_tx::PreparedTransmission> {
        if transmission.transmission().key.ue().epoch != self.0.epoch {
            return Err(transmission);
        }
        let retained = transmission.retained_bits();
        self.0
            .post(Action::Hsdpa(transmission), retained)
            .map_err(|action| match action {
                Action::Hsdpa(transmission) => transmission,
                _ => unreachable!("HSDPA publication"),
            })
    }

    pub fn queue_hsdpa_order(
        &mut self,
        order: crate::hsdpa_tx::PreparedOrder,
    ) -> Result<TransmitCommandId, crate::hsdpa_tx::PreparedOrder> {
        if order.identity().identity.ue.epoch != self.0.epoch {
            return Err(order);
        }
        let retained = order.retained_bits();
        self.0
            .post(Action::HsdpaOrder(order), retained)
            .map_err(|action| match action {
                Action::HsdpaOrder(order) => order,
                _ => unreachable!("HS-SCCH order publication"),
            })
    }

    pub fn take_hsdpa_order_completion(&mut self) -> Result<TransmitCommandId, TransmitError> {
        self.0
            .post(
                Action::TakeHsdpaOrderCompletion,
                crate::hsdpa_tx::retained_order_bits(),
            )
            .map_err(|_| TransmitError::Capacity)
    }

    pub fn take_hsdpa_completion(&mut self) -> Result<TransmitCommandId, TransmitError> {
        // The returned waveform uses the same retained-memory budget as publication.
        let retained = crate::hsdpa_tx::retained_transmission_bits();
        self.0
            .post(Action::TakeHsdpaCompletion, retained)
            .map_err(|_| TransmitError::Capacity)
    }

    pub fn begin_hsdpa_retirement(
        &mut self,
        ue: umts_types::UeKey,
    ) -> Result<TransmitCommandId, TransmitError> {
        if ue.epoch != self.0.epoch {
            return Err(TransmitError::Time);
        }
        self.0
            .post(Action::BeginHsdpaRetirement(ue), 0)
            .map_err(|_| TransmitError::Capacity)
    }

    pub fn finish_hsdpa_retirement(
        &mut self,
        ue: umts_types::UeKey,
    ) -> Result<TransmitCommandId, TransmitError> {
        if ue.epoch != self.0.epoch {
            return Err(TransmitError::Time);
        }
        self.0
            .post(Action::FinishHsdpaRetirement(ue), 0)
            .map_err(|_| TransmitError::Capacity)
    }

    pub(crate) fn accepts_source(&self, emission: &super::evidence::Emission) -> bool {
        emission.paired_runtime(&self.0.link) && self.1.paired_progress(&emission.source)
    }
    pub fn epoch(&self) -> RadioEpoch {
        self.0.epoch
    }
    pub(crate) fn belongs_to(&self, source: &crate::common_tx::CommonTx) -> bool {
        self.1.belongs_to(source)
    }
    pub(crate) fn frame_source(&self) -> &DedicatedFrameSource {
        &self.1
    }
    pub fn maximum_frame_payload_bits(&self) -> usize {
        self.0.maximum_payload
    }
    pub fn available(&self) -> bool {
        self.0.available()
    }
    pub fn try_event(&mut self) -> Option<TransmitEvent> {
        self.0.event()
    }
    pub fn outstanding(&self) -> usize {
        self.0.pending.iter().flatten().count()
    }
    #[cfg(test)]
    pub fn retained_bits(&self) -> usize {
        self.0.retained
    }
    pub fn prepare(
        &mut self,
        session: Box<DedicatedTransmitSession>,
    ) -> Result<TransmitCommandId, Box<DedicatedTransmitSession>> {
        if session.identity.ue.epoch != self.0.epoch {
            return Err(session);
        }
        let retained = session.resources.retained_bits;
        self.0
            .post(Action::Prepare(session), retained)
            .map_err(|action| match action {
                Action::Prepare(session) => session,
                _ => unreachable!("prepare publication"),
            })
    }
    pub fn frame(&mut self, input: FrameInput) -> Result<TransmitCommandId, FrameInput> {
        if input.storage.reclaimed.is_some() {
            return Err(input);
        }
        if input.retry_after.is_some_and(|at| {
            at.epoch != self.0.epoch
                || self.0.link.prepared_through.load(Ordering::Acquire) < at.index
        }) {
            return Err(input);
        }
        let retained = input
            .transports
            .iter()
            .try_fold(0_usize, |total, transport| {
                total.checked_add(transport.bits.len())
            });
        let Some(retained) = retained.filter(|bits| *bits <= self.0.maximum_payload) else {
            return Err(input);
        };
        if input.transports.len() > MAX_TRANSPORTS
            || input.identity.ue.epoch != self.0.epoch
            || input.start.epoch != self.0.epoch
        {
            return Err(input);
        }
        self.0
            .post(Action::Frame(input), retained)
            .map_err(|action| match action {
                Action::Frame(input) => input,
                _ => unreachable!("frame publication"),
            })
    }
    pub fn stage_replacement(
        &mut self,
        previous: ReceiveIdentity,
        session: Box<DedicatedTransmitSession>,
    ) -> Result<TransmitCommandId, Box<DedicatedTransmitSession>> {
        if previous.ue != session.identity.ue || previous.ue.epoch != self.0.epoch {
            return Err(session);
        }
        let retained = session.resources.retained_bits;
        self.0
            .post(Action::Stage { previous, session }, retained)
            .map_err(|action| match action {
                Action::Stage { session, .. } => session,
                _ => unreachable!("stage publication"),
            })
    }
    pub fn commit_replacement(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<TransmitCommandId, ReceiveIdentity> {
        if previous.ue != identity.ue || identity.ue.epoch != self.0.epoch {
            return Err(identity);
        }
        self.0
            .post(Action::Commit { previous, identity }, 0)
            .map_err(|_| identity)
    }
    pub fn retire_submitted(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<TransmitCommandId, ReceiveIdentity> {
        if identity.ue.epoch != self.0.epoch {
            return Err(identity);
        }
        self.0
            .post(Action::Retire(identity), 0)
            .map_err(|_| identity)
    }
    pub fn arm(&mut self, identity: ReceiveIdentity) -> Result<TransmitCommandId, ReceiveIdentity> {
        self.identity_command(identity, false)
    }
    pub fn remove(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<TransmitCommandId, ReceiveIdentity> {
        self.identity_command(identity, true)
    }
    fn identity_command(
        &mut self,
        identity: ReceiveIdentity,
        remove: bool,
    ) -> Result<TransmitCommandId, ReceiveIdentity> {
        if identity.ue.epoch != self.0.epoch {
            return Err(identity);
        }
        self.0
            .post(
                if remove {
                    Action::Remove(identity)
                } else {
                    Action::Arm(identity)
                },
                0,
            )
            .map_err(|_| identity)
    }
    pub fn observe(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<TransmitCommandId, ReceiveIdentity> {
        if identity.ue.epoch != self.0.epoch {
            return Err(identity);
        }
        self.0
            .post(Action::Observe(identity), 0)
            .map_err(|_| identity)
    }
    pub(crate) fn paired_with(&self, runtime: &DedicatedTransmitRuntime) -> bool {
        Arc::ptr_eq(&self.0.link, &runtime.control.link)
    }
}
impl DedicatedTpcPort {
    pub(crate) fn publish_downlink_control(
        &mut self,
        observation: DownlinkControl,
    ) -> Result<(), DownlinkControl> {
        if observation.identity.ue.epoch != self.0.epoch || !self.available() {
            return Err(observation);
        }
        self.1.try_send(observation)
    }
    pub fn publish_estimate(
        &mut self,
        estimate: TpcEstimate,
    ) -> Result<TransmitCommandId, TpcEstimate> {
        if estimate.identity().ue.epoch != self.0.epoch {
            return Err(estimate);
        }
        self.0
            .post(Action::Estimate(estimate), 1)
            .map_err(|action| match action {
                Action::Estimate(estimate) => estimate,
                _ => unreachable!("TPC estimate publication"),
            })
    }
    pub(crate) fn paired_with(&self, runtime: &DedicatedTransmitRuntime) -> bool {
        Arc::ptr_eq(&self.0.link, &runtime.feedback.link)
    }
    pub fn epoch(&self) -> RadioEpoch {
        self.0.epoch
    }
    pub fn outstanding(&self) -> usize {
        self.0.pending.iter().flatten().count()
    }
    pub fn available(&self) -> bool {
        self.0.available()
    }
    pub fn try_event(&mut self) -> Option<TransmitEvent> {
        self.0.event()
    }
    #[cfg(test)]
    pub fn publish_decision(
        &mut self,
        decision: TpcDecision,
    ) -> Result<TransmitCommandId, TpcDecision> {
        if decision.identity.ue.epoch != self.0.epoch
            || decision.physical_slot_start.epoch != self.0.epoch
        {
            return Err(decision);
        }
        self.0
            .post(Action::Decision(decision), 1)
            .map_err(|action| match action {
                Action::Decision(decision) => decision,
                _ => unreachable!("TPC decision publication"),
            })
    }
    #[cfg(test)]
    pub fn publish(&mut self, plan: TpcPlan) -> Result<TransmitCommandId, TpcPlan> {
        if plan.commands.is_empty()
            || plan.commands.len() > TPC_SLOTS
            || plan.identity.ue.epoch != self.0.epoch
            || plan.first_slot.epoch != self.0.epoch
        {
            return Err(plan);
        }
        let retained = plan.commands.len();
        self.0
            .post(Action::Feedback(plan), retained)
            .map_err(|action| match action {
                Action::Feedback(plan) => plan,
                _ => unreachable!("TPC publication"),
            })
    }
}

#[derive(Clone, Copy)]
struct Decision {
    start: ChipInstant,
    command: Bit,
}
struct TpcLane {
    identity: Option<ReceiveIdentity>,
    anchor: ChipInstant,
    anchor_cfn: u8,
    initialization: Option<TpcInitialization>,
    strict_from: Option<ChipInstant>,
    decisions: [Option<Decision>; TPC_SLOTS],
    estimates: EstimateLane,
    grouped: Option<GroupedTpc>,
}
impl TpcLane {
    fn index(&self, start: ChipInstant) -> Option<usize> {
        let offset = start.index.checked_sub(self.anchor.index)?;
        (start.epoch == self.anchor.epoch && offset.is_multiple_of(CHIPS_PER_SLOT))
            .then_some((offset / CHIPS_PER_SLOT % TPC_SLOTS as u64) as usize)
    }
    fn apply(&mut self, plan: &TpcPlan, now: ChipInstant) -> Result<(), TransmitError> {
        self.apply_commands(plan.identity, plan.first_slot, &plan.commands, now)
    }
    fn apply_commands(
        &mut self,
        identity: ReceiveIdentity,
        first_slot: ChipInstant,
        commands: &[Bit],
        now: ChipInstant,
    ) -> Result<(), TransmitError> {
        if commands.is_empty() || commands.len() > TPC_SLOTS {
            return Err(TransmitError::Payload);
        }
        if self.identity != Some(identity) {
            return Err(TransmitError::Identity);
        }
        let end = first_slot
            .index
            .checked_add(commands.len() as u64 * CHIPS_PER_SLOT)
            .ok_or(TransmitError::Time)?;
        let horizon = now
            .index
            .checked_add(TPC_SLOTS as u64 * CHIPS_PER_SLOT)
            .ok_or(TransmitError::Time)?;
        if first_slot.index < now.index || end > horizon {
            return Err(TransmitError::Time);
        }
        for offset in 0..commands.len() {
            let start = ChipInstant {
                epoch: first_slot.epoch,
                index: first_slot.index + offset as u64 * CHIPS_PER_SLOT,
            };
            let slot = self.index(start).ok_or(TransmitError::Time)?;
            if self.decisions[slot].is_some_and(|decision| decision.start.index >= now.index) {
                return Err(TransmitError::Conflict);
            }
        }
        self.estimates.admit_explicit()?;
        for (offset, &command) in commands.iter().enumerate() {
            let start = ChipInstant {
                epoch: first_slot.epoch,
                index: first_slot.index + offset as u64 * CHIPS_PER_SLOT,
            };
            let index = self.index(start).expect("validated slot");
            self.decisions[index] = Some(Decision { start, command });
        }
        self.strict_from = Some(self.strict_from.map_or(first_slot, |previous| {
            if previous.index <= first_slot.index {
                previous
            } else {
                first_slot
            }
        }));
        Ok(())
    }
    fn apply_estimate(
        &mut self,
        estimate: TpcEstimate,
        now: ChipInstant,
    ) -> Result<(), TransmitError> {
        if self.identity != Some(estimate.identity()) {
            return Err(TransmitError::Identity);
        }
        if now.epoch != self.anchor.epoch {
            return Err(TransmitError::Time);
        }
        let delta = estimate
            .observed_at()
            .index
            .max(now.index)
            .saturating_sub(self.anchor.index)
            .div_ceil(CHIPS_PER_SLOT);
        let eligible = self
            .anchor
            .index
            .checked_add(
                delta
                    .checked_mul(CHIPS_PER_SLOT)
                    .ok_or(TransmitError::Time)?,
            )
            .ok_or(TransmitError::Time)?;
        let first = ChipInstant {
            index: eligible,
            ..now
        };
        self.estimates.stage(estimate, now)?;
        self.strict_from =
            Some(self.strict_from.map_or(
                first,
                |old| if old.index < first.index { old } else { first },
            ));
        Ok(())
    }
    fn initial_command(&self, start: ChipInstant) -> Option<Bit> {
        let settings = self.initialization?;
        if self
            .strict_from
            .is_some_and(|strict| start.index >= strict.index)
        {
            return None;
        }
        let elapsed = start.index.checked_sub(self.anchor.index)? / CHIPS_PER_SLOT;
        if elapsed >= u64::from(settings.maximum_slots.get()) {
            return None;
        }
        if !settings.first_radio_link_set || settings.pattern_01_count == 0 {
            return Some(Bit::One);
        }
        // TS 25.214 5.1.2.2.1.2 restarts the pattern at frames whose CFN mod 4 is zero.
        const RESET_FRAMES: u64 = 4;
        let frames_to_reset =
            (RESET_FRAMES - u64::from(self.anchor_cfn) % RESET_FRAMES) % RESET_FRAMES;
        let first_reset = frames_to_reset * SLOTS_PER_FRAME;
        let phase = if elapsed < first_reset {
            elapsed
        } else {
            (elapsed - first_reset) % (RESET_FRAMES * SLOTS_PER_FRAME)
        };
        let paired_slots = u64::from(settings.pattern_01_count) * 2;
        let position = phase % (paired_slots + 1);
        Some(Bit::from(
            position >= paired_slots || !position.is_multiple_of(2),
        ))
    }
    fn take(&mut self, request: TpcRequest) -> Result<Bit, TransmitError> {
        if self.identity != Some(request.identity) {
            return Err(TransmitError::Identity);
        }
        let index = self
            .index(request.physical_slot_start)
            .ok_or(TransmitError::Time)?;
        if let Some(decision) =
            self.decisions[index].filter(|decision| decision.start == request.physical_slot_start)
        {
            self.decisions[index] = None;
            return Ok(decision.command);
        }
        let estimate = self.estimates.take(request.physical_slot_start);
        let initial = self.initial_command(request.physical_slot_start);
        if let Some(grouped) = &mut self.grouped {
            if let Some(command) = initial {
                return Ok(command);
            }
            if self.estimates.has_measurement()
                && self
                    .strict_from
                    .is_some_and(|first| request.physical_slot_start.index >= first.index)
            {
                let elapsed =
                    (request.physical_slot_start.index - self.anchor.index) / CHIPS_PER_SLOT;
                return Ok(grouped.take(
                    elapsed,
                    self.anchor_cfn,
                    request.physical_slot_start,
                    estimate,
                    &mut self.estimates.counters,
                ));
            }
        } else if let Some(estimate) = estimate {
            return Ok(estimate.command());
        }
        initial.ok_or(TransmitError::TpcUnavailable)
    }
}

pub struct DedicatedTransmitRuntime {
    bank: DedicatedTransmitBank,
    hsdpa: Vec<crate::hsdpa_tx::TransmitBank>,
    hsdpa_completion_cursor: usize,
    hsdpa_order_completion_cursor: usize,
    hsupa: Option<Box<crate::hsupa_tx::TransmitBank>>,
    control: Consumer,
    feedback: Consumer,
    downlink_controls: Receiver<DownlinkControl>,
    tpc: Vec<TpcLane>,
    progress: crate::tx_evidence::Reader,
    rate: usize,
    group_delay: usize,
    blocked_result: Option<TransmitEvent>,
    closed: bool,
    retired_estimates: TpcEstimateCounters,
}
impl DedicatedTransmitRuntime {
    pub(crate) fn register_preparation_worker(&self) -> Result<(), TransmitError> {
        if !self.available() {
            return Err(TransmitError::Closed);
        }
        self.control
            .link
            .wake
            .set(thread::current())
            .map_err(|_| TransmitError::Phase)
    }
    pub fn new(
        source: &crate::common_tx::CommonTx,
        maximum_owners: NonZeroUsize,
        maximum_channels: NonZeroUsize,
        maximum_retained_bits: NonZeroUsize,
        limits: TransmitRuntimeLimits,
    ) -> Result<(Self, DedicatedTransmitPorts), TransmitError> {
        if [
            limits.command_capacity,
            limits.commands_per_step,
            limits.feedback_capacity,
            limits.feedback_per_step,
        ]
        .iter()
        .any(|value| value.get() > MAX_COMMANDS)
        {
            return Err(TransmitError::Capacity);
        }
        let bank = DedicatedTransmitBank::new(
            source.information(),
            source.next_chip(),
            maximum_owners,
            maximum_channels,
            maximum_retained_bits,
        )?;
        let (port, control) = endpoints(
            source.next_chip().epoch,
            Lane::Control,
            limits.command_capacity,
            limits.commands_per_step,
            limits.maximum_inflight_retained_bits.get(),
            limits.maximum_frame_payload_bits.get(),
        );
        control
            .link
            .prepared_through
            .store(source.next_chip().index, Ordering::Release);
        let (tpc_port, feedback) = endpoints(
            source.next_chip().epoch,
            Lane::Feedback,
            limits.feedback_capacity,
            limits.feedback_per_step,
            limits.feedback_capacity.get() * TPC_SLOTS,
            TPC_SLOTS,
        );
        let (downlink_sender, downlink_controls) = mailbox::bounded(limits.feedback_capacity);
        let tpc = (0..maximum_owners.get() * CONFIGURATIONS_PER_UE)
            .map(|_| TpcLane {
                identity: None,
                anchor: source.next_chip(),
                anchor_cfn: 0,
                initialization: None,
                strict_from: None,
                decisions: [None; TPC_SLOTS],
                estimates: EstimateLane::default(),
                grouped: None,
            })
            .collect();
        Ok((
            Self {
                bank,
                hsdpa: Vec::with_capacity(2),
                hsdpa_completion_cursor: 0,
                hsdpa_order_completion_cursor: 0,
                hsupa: None,
                control,
                feedback,
                downlink_controls,
                tpc,
                progress: source.submission_progress(),
                rate: source.samples_per_slot() / CHIPS_PER_SLOT as usize,
                group_delay: source.group_delay_samples(),
                blocked_result: None,
                closed: false,
                retired_estimates: TpcEstimateCounters::default(),
            },
            DedicatedTransmitPorts {
                control: {
                    let frame_source = source.dedicated_frame_source().with_link(port.link.clone());
                    DedicatedTransmitPort(port, frame_source)
                },
                feedback: DedicatedTpcPort(tpc_port, downlink_sender),
            },
        ))
    }
    pub fn close(&mut self) {
        for session in self.bank.sessions.iter_mut().flatten() {
            session.fail_emissions();
        }
        self.closed = true;
        self.control.link.consumer.store(false, Ordering::Release);
        self.feedback.link.consumer.store(false, Ordering::Release);
    }
    pub fn available(&self) -> bool {
        !self.closed
            && self.control.link.producer.load(Ordering::Acquire)
            && self.feedback.link.producer.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub fn bank(&self) -> &DedicatedTransmitBank {
        &self.bank
    }
    pub fn service(&mut self) -> Result<usize, TransmitError> {
        if !self.available() {
            self.close();
            return Err(TransmitError::Closed);
        }
        let mut serviced = 0;
        for _ in 0..self.control.budget {
            let Some(command) = self.control.commands.try_recv() else {
                break;
            };
            let outcome = self.control_action(command.action);
            self.finish(command.id, outcome)?;
            serviced += 1;
        }
        for _ in 0..self.feedback.budget {
            let Some(command) = self.feedback.commands.try_recv() else {
                break;
            };
            let outcome = match command.action {
                Action::Feedback(plan) => {
                    let now = self.bank.next_chip();
                    let result = self
                        .validate_feedback(plan.identity, plan.first_slot, plan.commands.len())
                        .and_then(|()| self.tpc_lane_mut(plan.identity)?.apply(&plan, now));
                    TransmitCommandOutcome::Feedback { plan, result }
                }
                Action::Decision(decision) => {
                    let now = self.bank.next_chip();
                    let result = self
                        .validate_feedback(decision.identity, decision.physical_slot_start, 1)
                        .and_then(|()| {
                            self.tpc_lane_mut(decision.identity)?.apply_commands(
                                decision.identity,
                                decision.physical_slot_start,
                                std::slice::from_ref(&decision.command),
                                now,
                            )
                        });
                    TransmitCommandOutcome::Decision { decision, result }
                }
                Action::Estimate(estimate) => {
                    let now = self.bank.next_chip();
                    let result = self
                        .validate_feedback(estimate.identity(), now, 1)
                        .and_then(|()| {
                            self.tpc_lane_mut(estimate.identity())?
                                .apply_estimate(estimate, now)
                        });
                    TransmitCommandOutcome::Estimate { estimate, result }
                }
                _ => unreachable!("feedback producer"),
            };
            self.finish(command.id, outcome)?;
            serviced += 1;
        }
        for _ in 0..self.feedback.budget {
            let Some(observation) = self.downlink_controls.try_recv() else {
                break;
            };
            let now = self.bank.next_chip();
            let mut successor = None;
            let mut applied = false;
            if let Some(session) = self.bank.session_mut(observation.identity)
                && session.emits()
                && let Some(power) = session.downlink_power.as_mut()
            {
                power.observe(observation, now);
                session.counters.downlink_power = power.counters;
                applied = true;
                if let TransmitPhase::Draining { next, .. } = session.phase {
                    successor = Some(next);
                }
            }
            if let Some(identity) = successor
                && let Some(session) = self.bank.session_mut(identity)
                && let Some(power) = session.downlink_power.as_mut()
            {
                power.observe(observation, now);
                session.counters.downlink_power = power.counters;
            }
            serviced += usize::from(applied);
        }
        Ok(serviced)
    }
    fn finish(
        &mut self,
        id: TransmitCommandId,
        outcome: TransmitCommandOutcome,
    ) -> Result<(), TransmitError> {
        let events = match id.lane {
            Lane::Control => &self.control.events,
            Lane::Feedback => &self.feedback.events,
        };
        if let Err(event) = events.try_send(TransmitEvent { id, outcome }) {
            self.blocked_result = Some(event);
            self.close();
            return Err(TransmitError::Closed);
        }
        Ok(())
    }
    fn control_action(&mut self, action: Action) -> TransmitCommandOutcome {
        match action {
            Action::Hsupa(command) => {
                TransmitCommandOutcome::Hsupa(command.execute(self.hsupa.as_deref_mut()))
            }
            Action::BeginHsdpaRetirement(ue) => TransmitCommandOutcome::HsdpaRetirementStarted {
                ue,
                result: if self.hsdpa.is_empty() {
                    Err(crate::hsdpa_tx::Error::Unavailable)
                } else {
                    self.hsdpa
                        .iter()
                        .try_for_each(|bank| bank.can_begin_retirement(ue))
                        .and_then(|()| {
                            self.hsdpa
                                .iter_mut()
                                .try_for_each(|bank| bank.begin_retirement(ue))
                        })
                },
            },
            Action::FinishHsdpaRetirement(ue) => TransmitCommandOutcome::HsdpaRetirementFinished {
                ue,
                result: if self.hsdpa.is_empty() {
                    Err(crate::hsdpa_tx::Error::Unavailable)
                } else {
                    let ready = self.hsdpa.iter().try_fold(true, |ready, bank| {
                        bank.retirement_ready(ue).map(|current| ready && current)
                    });
                    match ready {
                        Ok(true) => self
                            .hsdpa
                            .iter_mut()
                            .try_for_each(|bank| bank.finish_retirement(ue).map(|_| ()))
                            .map(|_| true),
                        other => other,
                    }
                },
            },
            Action::HsdpaOrder(order) => {
                let key = order.identity();
                let result = match self
                    .hsdpa
                    .iter_mut()
                    .find(|bank| bank.primary_code() == order.primary_code())
                {
                    Some(bank) => bank.enqueue_order(order),
                    None => Err(crate::hsdpa_tx::RejectedOrder {
                        error: crate::hsdpa_tx::Error::Unavailable,
                        order,
                    }),
                };
                TransmitCommandOutcome::HsdpaOrderQueued { key, result }
            }
            Action::TakeHsdpaOrderCompletion => {
                let mut completed = None;
                for _ in 0..self.hsdpa.len() {
                    let index = self.hsdpa_order_completion_cursor % self.hsdpa.len();
                    self.hsdpa_order_completion_cursor = (index + 1) % self.hsdpa.len();
                    completed = self.hsdpa[index].take_completed_order();
                    if completed.is_some() {
                        break;
                    }
                }
                TransmitCommandOutcome::HsdpaOrderCompletion(completed)
            }
            Action::Hsdpa(transmission) => {
                let key = transmission.transmission().key;
                let result = match self
                    .hsdpa
                    .iter_mut()
                    .find(|bank| bank.primary_code() == transmission.primary_code())
                {
                    Some(bank) => bank.enqueue(transmission),
                    None => Err(crate::hsdpa_tx::RejectedTransmission {
                        error: crate::hsdpa_tx::Error::Unavailable,
                        transmission,
                        late_chips: None,
                    }),
                };
                TransmitCommandOutcome::HsdpaQueued { key, result }
            }
            Action::TakeHsdpaCompletion => {
                let mut completed = None;
                for _ in 0..self.hsdpa.len() {
                    let index = self.hsdpa_completion_cursor % self.hsdpa.len();
                    self.hsdpa_completion_cursor = (index + 1) % self.hsdpa.len();
                    completed = self.hsdpa[index].take_completed();
                    if completed.is_some() {
                        break;
                    }
                }
                TransmitCommandOutcome::HsdpaCompletion(completed)
            }
            Action::Prepare(session) => self.admit_session(None, session),
            Action::Stage { previous, session } => self.admit_session(Some(previous), session),
            Action::Commit { previous, identity } => {
                let result = self
                    .tpc_lane_mut(previous)
                    .map(|_| ())
                    .and_then(|()| self.tpc_lane_mut(identity).map(|_| ()))
                    .and_then(|()| self.bank.commit_replacement(previous, identity));
                if let Ok(boundary) = result {
                    let lane = self
                        .tpc_lane_mut(previous)
                        .expect("validated predecessor feedback owner");
                    for decision in &mut lane.decisions {
                        if decision
                            .is_some_and(|decision| decision.start.index >= boundary.time.index)
                        {
                            *decision = None;
                        }
                    }
                }
                TransmitCommandOutcome::ReplacementCommitted {
                    previous,
                    identity,
                    result,
                }
            }
            Action::Retire(identity) => {
                let result = self.retire_submitted(identity);
                if result.is_ok() {
                    self.clear_tpc(identity);
                }
                TransmitCommandOutcome::Retired { identity, result }
            }
            Action::Frame(mut input) => {
                let dummy = TransportId::new(1).expect("valid transport identity");
                let mut payloads: [TransportPayload<'_>; MAX_TRANSPORTS] =
                    std::array::from_fn(|_| TransportPayload {
                        transport: dummy,
                        bits: &[],
                    });
                for (output, transport) in payloads.iter_mut().zip(&input.transports) {
                    *output = TransportPayload {
                        transport: transport.transport,
                        bits: &transport.bits,
                    };
                }
                let reserved = input
                    .emission
                    .as_ref()
                    .map(|emission| {
                        if !emission.source.paired_with(&self.progress)
                            || !emission.paired_runtime(&self.control.link)
                            || emission.identity != input.identity
                            || emission.start != input.start
                            || emission.tfci != input.tfci
                        {
                            return Err(TransmitError::Identity);
                        }
                        self.bank
                            .session(input.identity)
                            .ok_or(TransmitError::Identity)?
                            .validate_emission(emission)
                    })
                    .transpose();
                let result = reserved.and_then(|slot| {
                    let result = self.bank.prepare_frame(
                        input.identity,
                        input.start,
                        input.tfci,
                        &payloads[..input.transports.len()],
                    );
                    if result.is_ok()
                        && let Some(slot) = slot
                    {
                        let mut emission = input.emission.take().expect("reserved emission");
                        emission.prepared();
                        let session = self
                            .bank
                            .session_mut(input.identity)
                            .expect("prepared session");
                        input.storage.reclaimed = session.emissions[slot].replace(emission);
                        if !session.emission_bound {
                            session.emission_pool = session.emissions[slot]
                                .as_ref()
                                .expect("prepared emission")
                                .pool()
                                .cloned();
                            session.emission_bound = true;
                        }
                    }
                    result
                });
                input.retry_after = if result == Err(TransmitError::Capacity) {
                    self.bank
                        .session(input.identity)
                        .and_then(|session| session.capacity_retry_at(self.bank.next_chip()))
                } else {
                    None
                };
                if let Err(error) = &result
                    && *error != TransmitError::Capacity
                {
                    let cursor = self.bank.next_chip();
                    if let Some(session) = self.bank.session_mut(input.identity) {
                        session.record_failure(
                            TransmitOperation::Frame,
                            cursor,
                            input.start,
                            error,
                        );
                    }
                }
                if result
                    .as_ref()
                    .is_err_and(|error| *error != TransmitError::Capacity)
                    && let Some(emission) = &mut input.emission
                {
                    emission.fail();
                }
                TransmitCommandOutcome::Frame { input, result }
            }
            Action::Arm(identity) => {
                let cursor = self.bank.next_chip();
                let result = self.bank.arm(identity);
                if let Err(error) = &result
                    && let Some(session) = self.bank.session_mut(identity)
                {
                    session.record_failure(
                        TransmitOperation::Arm,
                        cursor,
                        session.anchor.time,
                        error,
                    );
                }
                TransmitCommandOutcome::Armed { identity, result }
            }
            Action::Remove(identity) => {
                let result = self.remove_with_drain(identity);
                if result.is_ok() {
                    self.clear_tpc(identity);
                }
                TransmitCommandOutcome::Removed { identity, result }
            }
            Action::Observe(identity) => TransmitCommandOutcome::Observed {
                identity,
                tpc: self
                    .tpc_lane_mut(identity)
                    .ok()
                    .map(|lane| lane.estimates.counters.groups),
                state: self.bank.session(identity).map(|session| {
                    (
                        session.phase(),
                        session.counters(),
                        session.failure().cloned(),
                    )
                }),
            },
            Action::Feedback(_) | Action::Decision(_) | Action::Estimate(_) => {
                unreachable!("control producer")
            }
        }
    }
    pub fn generate_slot(
        &mut self,
        source: &mut crate::common_tx::CommonTx,
        samples: &mut [Complex32],
    ) -> Result<umts_dsp::waveform::WaveformWindow, crate::common_tx::CommonTxError> {
        self.generate_slot_disposition(source, samples, false)
    }

    pub fn attach_hsdpa(
        &mut self,
        bank: crate::hsdpa_tx::TransmitBank,
    ) -> Result<(), Box<crate::hsdpa_tx::TransmitBank>> {
        if !self.available()
            || self.hsdpa.len() == 2
            || self
                .hsdpa
                .iter()
                .any(|existing| existing.primary_code() == bank.primary_code())
            || bank.next_chip() != self.bank.next_chip()
            || !bank.matches_progress(&self.progress)
        {
            // Attachment runs during setup. Preserve rejected ownership outside the RT path.
            return Err(Box::new(bank));
        }
        self.hsdpa.push(bank);
        Ok(())
    }

    #[cfg(test)]
    pub fn hsdpa_mut(&mut self) -> Option<&mut crate::hsdpa_tx::TransmitBank> {
        self.hsdpa.first_mut()
    }

    pub fn attach_hsupa(
        &mut self,
        bank: Box<crate::hsupa_tx::TransmitBank>,
    ) -> Result<(), Box<crate::hsupa_tx::TransmitBank>> {
        if !self.available()
            || self.hsupa.is_some()
            || bank.next_chip() != self.bank.next_chip()
            || !bank.matches_progress(&self.progress)
        {
            return Err(bank);
        }
        self.hsupa = Some(bank);
        Ok(())
    }
    #[cfg(test)]
    pub fn take_closed_hsupa(&mut self) -> Option<Box<crate::hsupa_tx::TransmitBank>> {
        if self.closed { self.hsupa.take() } else { None }
    }

    #[cfg(test)]
    pub fn take_closed_hsdpa(&mut self) -> Option<crate::hsdpa_tx::TransmitBank> {
        if self.closed && !self.hsdpa.is_empty() {
            Some(self.hsdpa.remove(0))
        } else {
            None
        }
    }

    pub(crate) fn generate_lost_slot(
        &mut self,
        source: &mut crate::common_tx::CommonTx,
        samples: &mut [Complex32],
    ) -> Result<umts_dsp::waveform::WaveformWindow, crate::common_tx::CommonTxError> {
        self.generate_slot_disposition(source, samples, true)
    }

    fn generate_slot_disposition(
        &mut self,
        source: &mut crate::common_tx::CommonTx,
        samples: &mut [Complex32],
        lost: bool,
    ) -> Result<umts_dsp::waveform::WaveformWindow, crate::common_tx::CommonTxError> {
        if !self.available() || !source.submission_paired_with(&self.progress) {
            self.close();
            return Err(crate::common_tx::CommonTxError::Poisoned);
        }
        let tpc = &mut self.tpc;
        let feedback =
            |request: super::TpcRequest| match lane_mut(tpc, request.identity)?.take(request) {
                Err(TransmitError::TpcUnavailable) if lost => Ok(Bit::Zero),
                result => result,
            };
        let result = source.generate_slot_with_carriers(
            samples,
            &mut self.bank,
            &mut self.hsdpa,
            self.hsupa.as_deref_mut(),
            feedback,
        );
        if result.is_ok() {
            self.control
                .link
                .prepared_through
                .store(self.bank.next_chip().index, Ordering::Release);
        }
        result
    }
    fn tpc_lane_mut(&mut self, identity: ReceiveIdentity) -> Result<&mut TpcLane, TransmitError> {
        lane_mut(&mut self.tpc, identity)
    }
    fn clear_tpc(&mut self, identity: ReceiveIdentity) {
        if let Ok(lane) = self.tpc_lane_mut(identity) {
            let counters = lane.estimates.counters;
            self.retired_estimates.add(counters);
        }
        let lane = self
            .tpc_lane_mut(identity)
            .expect("removed admitted feedback owner");
        lane.identity = None;
        lane.decisions.fill(None);
        lane.initialization = None;
        lane.strict_from = None;
        lane.anchor_cfn = 0;
        lane.estimates = EstimateLane::default();
        lane.grouped = None;
    }
    pub fn estimate_counters(&self) -> TpcEstimateCounters {
        let mut counters = self.retired_estimates;
        for lane in &self.tpc {
            counters.add(lane.estimates.counters);
        }
        counters
    }
    fn admit_session(
        &mut self,
        previous: Option<ReceiveIdentity>,
        session: Box<DedicatedTransmitSession>,
    ) -> TransmitCommandOutcome {
        let identity = session.identity;
        let anchor = session.anchor.time;
        let anchor_cfn = session.anchor.cfn;
        let initialization = session.initial_tpc;
        let grouped = match session.plan.configuration().physical().power_control {
            umts_rrc::dedicated::UplinkPowerControl::Algorithm1 { .. } => None,
            umts_rrc::dedicated::UplinkPowerControl::Algorithm2 => {
                Some(GroupedTpc::new(session.algorithm2_hold_groups))
            }
        };
        let base = identity.ue.slot as usize * CONFIGURATIONS_PER_UE;
        let slot = self
            .tpc
            .get(base..base + CONFIGURATIONS_PER_UE)
            .and_then(|lanes| lanes.iter().position(|lane| lane.identity.is_none()))
            .map(|index| base + index);
        let Some(slot) = slot else {
            return TransmitCommandOutcome::PrepareRejected {
                error: TransmitError::Capacity,
                session,
            };
        };
        let result = match previous {
            Some(previous) => self.bank.stage_replacement(previous, session),
            None => self.bank.insert_boxed(session),
        };
        match result {
            Ok(()) => {
                self.tpc[slot] = TpcLane {
                    identity: Some(identity),
                    anchor,
                    anchor_cfn,
                    initialization,
                    strict_from: None,
                    decisions: [None; TPC_SLOTS],
                    estimates: EstimateLane::default(),
                    grouped,
                };
                previous.map_or(TransmitCommandOutcome::Prepared(identity), |previous| {
                    TransmitCommandOutcome::ReplacementPrepared { previous, identity }
                })
            }
            Err((error, mut session)) => {
                session.record_failure(
                    TransmitOperation::Admission,
                    self.bank.next_chip(),
                    anchor,
                    &error,
                );
                TransmitCommandOutcome::PrepareRejected { error, session }
            }
        }
    }
    fn validate_feedback(
        &self,
        identity: ReceiveIdentity,
        first_slot: ChipInstant,
        commands: usize,
    ) -> Result<(), TransmitError> {
        let session = self.bank.session(identity).ok_or(TransmitError::Identity)?;
        match session.phase {
            TransmitPhase::Failed | TransmitPhase::Retired | TransmitPhase::Stopped { .. } => {
                Err(session.failure().cloned().unwrap_or(TransmitError::Phase))
            }
            TransmitPhase::Draining { boundary, .. } => {
                let end = first_slot
                    .index
                    .checked_add(commands as u64 * CHIPS_PER_SLOT)
                    .ok_or(TransmitError::Time)?;
                if end > boundary.index {
                    Err(TransmitError::Time)
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
    fn remove_with_drain(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<Box<DedicatedTransmitSession>, TransmitError> {
        self.bank.session(identity).ok_or(TransmitError::Identity)?;
        let origin = self.progress.origin();
        let cutoff = self.bank.next_chip();
        if cutoff.epoch != origin.epoch || !origin.index.is_multiple_of(self.rate as u64) {
            return Err(TransmitError::Time);
        }
        let first = ChipInstant {
            epoch: origin.epoch,
            index: origin.index / self.rate as u64,
        };
        let drain = if cutoff == first {
            super::TransmitDrain::NoContribution
        } else {
            let receipt = self
                .progress
                .interval_resolution(first, cutoff, self.rate, self.group_delay)
                .ok_or(TransmitError::Time)?;
            if !receipt.viable() {
                return Err(TransmitError::SubmissionLost);
            }
            // The shared synthesis cutoff and full pulse tail conservatively cover every prior contribution.
            super::TransmitDrain::ResolvedTail(receipt)
        };
        let mut session = self.bank.remove_boxed(identity)?;
        session.drain = drain;
        Ok(session)
    }

    fn retire_submitted(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<RetiredTransmit, TransmitError> {
        let (first, end) = self.bank.synthesized_retirement(identity)?;
        let old = self.bank.session(identity).ok_or(TransmitError::Identity)?;
        let TransmitPhase::Stopped { next, boundary } = old.phase() else {
            return Err(TransmitError::Phase);
        };
        let old_anchor = old.frame_anchor();
        let submission = self
            .progress
            .interval_resolution(first, end, self.rate, self.group_delay)
            .ok_or(TransmitError::Time)?;
        if !submission.viable() {
            return Err(TransmitError::SubmissionLost);
        }
        if !submission.resolved() {
            return Err(TransmitError::SubmissionPending);
        }
        let mut session = self.bank.remove_boxed(identity)?;
        session.drain = super::TransmitDrain::ResolvedTail(submission.clone());
        Ok(RetiredTransmit {
            session,
            submission,
            previous: identity,
            old_anchor,
            successor: next,
            boundary,
        })
    }
}
fn lane_mut(
    lanes: &mut [TpcLane],
    identity: ReceiveIdentity,
) -> Result<&mut TpcLane, TransmitError> {
    let base = identity.ue.slot as usize * CONFIGURATIONS_PER_UE;
    lanes
        .get_mut(base..base + CONFIGURATIONS_PER_UE)
        .and_then(|lanes| {
            lanes
                .iter_mut()
                .find(|lane| lane.identity == Some(identity))
        })
        .ok_or(TransmitError::Identity)
}
impl Drop for DedicatedTransmitRuntime {
    fn drop(&mut self) {
        self.close();
    }
}
