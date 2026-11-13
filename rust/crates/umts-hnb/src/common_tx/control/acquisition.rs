// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::access::{AccessTiming, AccessTxTarget, AcquisitionPublication, MAX_ACCESS_RESPONSES};
use crate::tx_evidence::TransportSubmission;
use umts_rrc::EncodedInformation;

#[cfg(test)]
mod tests;

pub struct ControlPorts {
    pub transport: CommonTxPort,
    pub acquisition: AcquisitionPort,
}

pub struct AcquisitionCancellation {
    state: Arc<PlanState>,
    alive: Option<Arc<AtomicBool>>,
}
impl AcquisitionCancellation {
    pub fn cancel(&mut self) -> PlanStage {
        self.state.terminate(CANCELLED)
    }
    pub(crate) fn available(&self) -> bool {
        self.alive
            .as_ref()
            .is_none_or(|alive| alive.load(Ordering::Acquire))
    }
}
impl Drop for AcquisitionCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AcquisitionEvent {
    pub id: PlanId,
    pub channel: usize,
    pub observation: AccessObservation,
    pub window: AccessWindow,
    pub stage: PlanStage,
}

struct Request {
    channel: usize,
    observation: AccessObservation,
    indicator: Acquisition,
    now: ChipInstant,
    minimum_lead: u64,
    window: AccessWindow,
}

struct AccessCommand {
    request: Request,
    writer: PlanWriter,
}

struct AccessEntry {
    state: Arc<PlanState>,
    channel: usize,
    observation: Option<AccessObservation>,
    window: Option<AccessWindow>,
    reported: PlanStage,
}

pub struct AcquisitionPort {
    information: Arc<CellInformation>,
    epoch: umts_types::RadioEpoch,
    cursor: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    progress: crate::tx_evidence::Reader,
    rate: usize,
    group_delay: usize,
    profiles: Vec<(EncodedInformation, AccessTiming)>,
    ready: Sender<AccessCommand>,
    entries: Vec<AccessEntry>,
    next_id: Option<NonZeroU64>,
    event_cursor: usize,
}

impl AcquisitionPort {
    pub(crate) fn belongs_to(&self, source: &CommonTx) -> bool {
        source
            .control
            .as_ref()
            .is_some_and(|bridge| Arc::ptr_eq(&bridge.alive, &self.alive))
    }
}

pub(super) struct AcquisitionBridge {
    ready: Receiver<AccessCommand>,
    capacity: usize,
    budget: usize,
}

pub(in crate::common_tx) struct PendingAcquisition {
    pub window: AccessWindow,
    pub observation: AccessObservation,
    pub writer: PlanWriter,
}

fn proof(
    progress: &crate::tx_evidence::Reader,
    window: AccessWindow,
    rate: usize,
    delay: usize,
) -> Result<TransportSubmission, CommonTxError> {
    let end = ChipInstant {
        epoch: window.indicator_start.epoch,
        index: window
            .indicator_start
            .index
            .checked_add(ACCESS_SLOT_CHIPS as u64)
            .ok_or(CommonTxError::Time)?,
    };
    progress
        .interval_receipt(window.indicator_start, end, rate, delay)
        .ok_or(CommonTxError::Time)
}

fn publication(
    window: AccessWindow,
    proof: TransportSubmission,
    state: &Arc<PlanState>,
    alive: Option<Arc<AtomicBool>>,
) -> AcquisitionPublication {
    AcquisitionPublication {
        window,
        transport: proof.with_plan(PlanReceipt(state.clone())),
        cancellation: AcquisitionCancellation {
            state: state.clone(),
            alive,
        },
    }
}

impl AcquisitionPort {
    pub fn available(&self) -> bool {
        self.alive.load(Ordering::Acquire) && self.progress.available()
    }

    pub(super) fn prepare(
        source: &CommonTx,
        capacity: NonZeroUsize,
        budget: NonZeroUsize,
        cursor: Arc<AtomicU64>,
        alive: Arc<AtomicBool>,
    ) -> Result<(Self, AcquisitionBridge), CommonTxError> {
        if capacity.get() > MAX_ACCESS_RESPONSES || budget.get() > capacity.get() {
            return Err(CommonTxError::Capacity);
        }
        if source.access.is_empty() {
            return Err(CommonTxError::Channel);
        }
        let next_id = source.next_acquisition_id.ok_or(CommonTxError::Capacity)?;
        let (ready, receive) = mailbox::bounded(capacity);
        let entries = (0..capacity.get())
            .map(|_| AccessEntry {
                state: Arc::new(PlanState {
                    id: PlanId {
                        epoch: source.next.epoch,
                        lane: PlanLane::Acquisition,
                        serial: next_id,
                    },
                    owner: None,
                    stage: AtomicU8::new(REJECTED + PlanRejection::Closed as u8),
                }),
                channel: 0,
                observation: None,
                window: None,
                reported: PlanStage::Rejected(PlanRejection::Closed),
            })
            .collect();
        Ok((
            Self {
                information: source.information.clone(),
                epoch: source.next.epoch,
                cursor,
                alive,
                progress: source.progress.clone(),
                rate: source.rate,
                group_delay: source.group_delay_samples(),
                profiles: source
                    .access
                    .iter()
                    .map(|stream| {
                        (
                            EncodedInformation {
                                bytes: stream.profile.bytes.clone(),
                                bit_length: stream.profile.bit_length,
                            },
                            stream.calendar.profile(),
                        )
                    })
                    .collect(),
                ready,
                entries,
                next_id: Some(next_id),
                event_cursor: 0,
            },
            AcquisitionBridge {
                ready: receive,
                capacity: capacity.get(),
                budget: budget.get(),
            },
        ))
    }

    pub fn next_event(&mut self) -> Option<AcquisitionEvent> {
        for _ in 0..self.entries.len() {
            let index = self.event_cursor;
            self.event_cursor = (index + 1) % self.entries.len();
            let entry = &mut self.entries[index];
            if !self.alive.load(Ordering::Acquire) {
                entry
                    .state
                    .terminate(REJECTED + PlanRejection::Closed as u8);
            }
            let stage = entry.state.stage();
            if stage != entry.reported {
                entry.reported = stage;
                return Some(AcquisitionEvent {
                    id: entry.state.id,
                    channel: entry.channel,
                    observation: entry.observation.expect("published observation"),
                    window: entry.window.expect("published access window"),
                    stage,
                });
            }
        }
        None
    }
}

impl AccessTxTarget for AcquisitionPort {
    fn information(&self) -> &CellInformation {
        &self.information
    }
    fn next_chip(&self) -> ChipInstant {
        ChipInstant {
            epoch: self.epoch,
            index: self.cursor.load(Ordering::Acquire),
        }
    }
    fn access_profile(&self, channel: usize) -> Option<&EncodedInformation> {
        self.profiles.get(channel).map(|(profile, _)| profile)
    }
    fn access_timing(&self, channel: usize) -> Option<AccessTiming> {
        self.profiles.get(channel).map(|(_, timing)| *timing)
    }
    fn publish_acquisition(
        &mut self,
        channel: usize,
        observation: AccessObservation,
        indicator: Acquisition,
        now: ChipInstant,
        minimum_lead: u64,
    ) -> Result<AcquisitionPublication, CommonTxError> {
        if !self.alive.load(Ordering::Acquire) || !self.progress.available() {
            return Err(CommonTxError::Poisoned);
        }
        let (_, timing) = self.profiles.get(channel).ok_or(CommonTxError::Channel)?;
        let window = timing
            .preview(observation, indicator, now, self.next_chip(), minimum_lead)
            .map_err(CommonTxError::Access)?;
        if self.entries.iter().any(|entry| {
            entry.channel == channel
                && entry
                    .window
                    .is_some_and(|existing| existing.indicator_start == window.indicator_start)
                && entry
                    .observation
                    .is_some_and(|existing| existing.signature == observation.signature)
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
        let serial = self.next_id.ok_or(CommonTxError::Capacity)?;
        let evidence = proof(&self.progress, window, self.rate, self.group_delay)?;
        let entry = &mut self.entries[index];
        let state = Arc::get_mut(&mut entry.state).expect("unreferenced access plan");
        state.id = PlanId {
            epoch: self.epoch,
            lane: PlanLane::Acquisition,
            serial,
        };
        state.stage.store(QUEUED, Ordering::Release);
        entry.channel = channel;
        entry.observation = Some(observation);
        entry.window = Some(window);
        entry.reported = PlanStage::Queued;
        self.next_id = serial.checked_add(1);
        let command = AccessCommand {
            request: Request {
                channel,
                observation,
                indicator,
                now,
                minimum_lead,
                window,
            },
            writer: PlanWriter(entry.state.clone()),
        };
        if let Err(command) = self.ready.try_send(command) {
            command.writer.reject(PlanRejection::Capacity);
            entry.reported = PlanStage::Rejected(PlanRejection::Capacity);
            return Err(CommonTxError::Capacity);
        }
        // Consumer close must either drain this command or make this publication terminal.
        if !self.alive.fetch_and(true, Ordering::AcqRel) {
            entry
                .state
                .terminate(REJECTED + PlanRejection::Closed as u8);
        }
        Ok(publication(
            window,
            evidence,
            &entry.state,
            Some(self.alive.clone()),
        ))
    }
}

impl Drop for AcquisitionPort {
    fn drop(&mut self) {
        self.alive.swap(false, Ordering::AcqRel);
        for entry in &self.entries {
            entry
                .state
                .terminate(REJECTED + PlanRejection::Closed as u8);
        }
    }
}

impl AcquisitionBridge {
    pub(super) fn service(&mut self, source: &mut CommonTx) {
        for _ in 0..self.budget {
            let Some(command) = self.ready.try_recv() else {
                break;
            };
            if command.writer.schedule() {
                source.accept_acquisition(command);
            }
        }
    }
}
impl Drop for AcquisitionBridge {
    fn drop(&mut self) {
        for _ in 0..self.capacity {
            if self.ready.try_recv().is_none() {
                break;
            }
        }
    }
}

impl CommonTx {
    fn accept_acquisition(&mut self, command: AccessCommand) {
        let AccessCommand { request, writer } = command;
        let result = self.admit_acquisition(&request);
        match result {
            Ok(index) => {
                self.access[request.channel].plans[index] = Some(PendingAcquisition {
                    window: request.window,
                    observation: request.observation,
                    writer,
                })
            }
            Err(error) => writer.reject(rejection(error)),
        }
    }

    fn admit_acquisition(&mut self, request: &Request) -> Result<usize, CommonTxError> {
        let stream = self
            .access
            .get_mut(request.channel)
            .ok_or(CommonTxError::Channel)?;
        let preview = stream
            .calendar
            .profile()
            .preview(
                request.observation,
                request.indicator,
                request.now,
                self.next,
                request.minimum_lead,
            )
            .map_err(CommonTxError::Access)?;
        if preview != request.window {
            return Err(CommonTxError::Conflict);
        }
        if stream.plans.iter().flatten().any(|plan| {
            plan.window.indicator_start == preview.indicator_start
                && plan.observation.signature == request.observation.signature
        }) {
            return Err(CommonTxError::Conflict);
        }
        let index = stream
            .plans
            .iter()
            .position(Option::is_none)
            .ok_or(CommonTxError::Capacity)?;
        let window = stream
            .calendar
            .schedule(
                request.observation,
                request.indicator,
                request.now,
                self.next,
                request.minimum_lead,
            )
            .map_err(CommonTxError::Access)?;
        if window.duplicate {
            return Err(CommonTxError::Conflict);
        }
        Ok(index)
    }

    pub(in crate::common_tx) fn clean_acquisitions(&mut self) -> Result<(), CommonTxError> {
        for stream in &mut self.access {
            for plan in &mut stream.plans {
                if plan
                    .as_ref()
                    .is_some_and(|plan| plan.writer.0.stage() != PlanStage::Scheduled)
                {
                    let pending = plan.take().expect("terminal access plan");
                    stream
                        .calendar
                        .cancel(
                            pending.window.indicator_start,
                            pending.observation.signature,
                        )
                        .map_err(CommonTxError::Access)?;
                }
            }
        }
        Ok(())
    }
}

impl AccessTxTarget for CommonTx {
    fn information(&self) -> &CellInformation {
        self.information()
    }
    fn next_chip(&self) -> ChipInstant {
        self.next_chip()
    }
    fn access_profile(&self, channel: usize) -> Option<&EncodedInformation> {
        self.access_profile(channel)
    }
    fn access_timing(&self, channel: usize) -> Option<AccessTiming> {
        self.access
            .get(channel)
            .map(|stream| stream.calendar.profile())
    }
    fn publish_acquisition(
        &mut self,
        channel: usize,
        observation: AccessObservation,
        indicator: Acquisition,
        now: ChipInstant,
        minimum_lead: u64,
    ) -> Result<AcquisitionPublication, CommonTxError> {
        if self.poisoned || !self.progress.available() {
            return Err(CommonTxError::Poisoned);
        }
        if self.control.is_some() {
            return Err(CommonTxError::Conflict);
        }
        self.clean_acquisitions()?;
        let stream = self.access.get(channel).ok_or(CommonTxError::Channel)?;
        let window = stream
            .calendar
            .profile()
            .preview(observation, indicator, now, self.next, minimum_lead)
            .map_err(CommonTxError::Access)?;
        let evidence = proof(
            &self.progress,
            window,
            self.rate,
            self.group_delay_samples(),
        )?;
        let serial = self.next_acquisition_id.ok_or(CommonTxError::Capacity)?;
        let state = Arc::new(PlanState {
            id: PlanId {
                epoch: self.next.epoch,
                lane: PlanLane::Acquisition,
                serial,
            },
            owner: None,
            stage: AtomicU8::new(SCHEDULED),
        });
        self.next_acquisition_id = serial.checked_add(1);
        let request = Request {
            channel,
            observation,
            indicator,
            now,
            minimum_lead,
            window,
        };
        let index = self.admit_acquisition(&request)?;
        self.access[channel].plans[index] = Some(PendingAcquisition {
            window,
            observation,
            writer: PlanWriter(state.clone()),
        });
        Ok(publication(window, evidence, &state, None))
    }
}
