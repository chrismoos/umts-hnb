// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_packet::{BlockBuffer, EncoderBank, PlanBank},
    hsdpa_tx::{AllocationCalendar, WaveformBuffer, WaveformPreparer},
};

const MAX_HARQ_PROCESSES: u8 = 8;
const MAX_STAGED_QUEUES: usize = 8;
mod admission;
mod budget;
mod staging;
#[cfg(test)]
mod staging_tests;
#[cfg(test)]
mod tests;
pub(crate) use admission::SchedulingPreparation;
pub use admission::{SchedulingAdmission, SchedulingBudget};
pub use budget::CellSchedulingBudget;

#[derive(Clone, Copy)]
pub struct SchedulingSettings {
    pub power: DownlinkPower,
    pub maximum_cqi_age: NonZeroU64,
    pub selection_budget: NonZeroUsize,
    pub cqi_offset_limit: u8,
    pub cqi_target_bler_percent: u8,
}

pub struct SchedulingResources {
    retained_bytes: usize,
    encoders: EncoderBank,
    plans: PlanBank,
    buffers: Box<[Option<BlockBuffer>]>,
    pub(super) staging_buffers: [Option<BlockBuffer>; MAX_STAGED_QUEUES],
    staging_cursor: usize,
    settings: SchedulingSettings,
    _reservation: budget::Reservation,
}

impl SchedulingResources {
    pub(super) fn matches_parameters(
        &self,
        parameters: &umts_rrc::hsdpa::ActivationParameters,
    ) -> bool {
        self.plans.h_rnti() == parameters.h_rnti
            && self.buffers.len() == parameters.harq.process_memory().len()
            && self
                .plans
                .formats()
                .all(|format| parameters.harq.process_memory().contains(&format.soft_bits))
    }

    pub(super) fn adaptation(
        &self,
        category: umts_types::hsdpa::Category,
    ) -> adaptation::LinkAdaptation {
        let make = if category.limits().qam64 {
            adaptation::LinkAdaptation::per_cqi
        } else {
            adaptation::LinkAdaptation::new
        };
        make(
            self.settings.cqi_offset_limit,
            self.settings.cqi_target_bler_percent,
        )
    }

    pub(super) fn order_control_amplitude(&self) -> f32 {
        self.settings.power.control_amplitude()
    }

    fn new(
        encoders: EncoderBank,
        plans: PlanBank,
        processes: std::num::NonZeroU8,
        settings: SchedulingSettings,
        maximum_bytes: NonZeroUsize,
        mut reservation: budget::Reservation,
    ) -> Result<Self, umts_dsp::DspError> {
        if processes.get() > MAX_HARQ_PROCESSES {
            return Err(umts_dsp::DspError::Capacity);
        }
        let buffer_bytes = encoders
            .maximum_coded_bits()
            .get()
            .checked_mul(std::mem::size_of::<umts_dsp::bits::Bit>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Option<BlockBuffer>>()))
            .and_then(|bytes| bytes.checked_mul(usize::from(processes.get()) + MAX_STAGED_QUEUES))
            .and_then(|bytes| {
                bytes.checked_add(crate::hsdpa_packet::PacketOwner::staging_metadata_bytes())
            })
            .ok_or(umts_dsp::DspError::Capacity)?;
        let retained_bytes = encoders
            .retained_bytes()
            .checked_add(plans.retained_bytes())
            .and_then(|bytes| bytes.checked_add(buffer_bytes))
            .and_then(|bytes| {
                bytes.checked_add(
                    std::mem::size_of::<Self>()
                        - std::mem::size_of::<EncoderBank>()
                        - std::mem::size_of::<PlanBank>(),
                )
            })
            .ok_or(umts_dsp::DspError::Capacity)?;
        if retained_bytes > maximum_bytes.get() || !reservation.retain(retained_bytes) {
            return Err(umts_dsp::DspError::Capacity);
        }
        let buffers = (0..processes.get())
            .map(|_| Some(encoders.buffer()))
            .collect();
        let staging_buffers = std::array::from_fn(|_| Some(encoders.buffer()));
        Ok(Self {
            _reservation: reservation,
            retained_bytes,
            encoders,
            plans,
            buffers,
            staging_buffers,
            staging_cursor: 0,
            settings,
        })
    }

    /// The admission charge remains reserved while coded buffers reside in HARQ.
    #[cfg(test)]
    pub fn plan_formats(&self) -> impl Iterator<Item = crate::hsdpa_packet::PlanFormat> + '_ {
        self.plans.formats()
    }

    #[cfg(test)]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

#[derive(Clone, Copy)]
pub struct SchedulingTime {
    pub now: ChipInstant,
    pub not_before: ChipInstant,
    pub feedback: FeedbackTiming,
}

#[derive(Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct SchedulingProgress {
    pub phase: Phase,
    pub ready: bool,
    pub pending: bool,
    pub queued_pdus: usize,
    pub staged_queues: usize,
    pub empty_processes: usize,
    pub ready_processes: usize,
    pub busy_processes: usize,
    pub recycled_buffers: usize,
    pub selection: SelectionCounts,
    pub feedback: super::FeedbackCounts,
    pub cqi_offset_milli: i32,
    pub airtime: super::AirtimeSummary,
}

#[must_use = "Return unused waveform storage to the cell owner"]
pub enum SchedulingOutcome {
    Prepared,
    Deferred {
        waveform: WaveformBuffer,
        budget_exhausted: bool,
    },
    Failed {
        waveform: WaveformBuffer,
        error: DispatchError,
    },
}

impl Session {
    pub fn scheduling_progress_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
    ) -> SchedulingProgress {
        let mut progress = SchedulingProgress {
            phase: self.phase(),
            ready: self.scheduler_ready_on(carrier),
            pending: self.links[carrier.index()].pending.is_some(),
            queued_pdus: 0,
            staged_queues: 0,
            empty_processes: 0,
            ready_processes: 0,
            busy_processes: 0,
            recycled_buffers: self.links[carrier.index()]
                .recycled_blocks
                .iter()
                .filter(|slot| slot.is_some())
                .count(),
            selection: self.links[carrier.index()].feedback_snapshot.selection,
            feedback: self.links[carrier.index()].feedback_snapshot.outcomes,
            cqi_offset_milli: self.links[carrier.index()]
                .feedback_snapshot
                .cqi_offset_milli,
            airtime: self.links[carrier.index()]
                .feedback_snapshot
                .formats
                .airtime_summary(),
        };
        for queue in self.procedure.construction().parameters().queues.queues() {
            progress.queued_pdus += self.queues.queued(queue.id());
            progress.staged_queues += usize::from(self.queues.staged_packet(queue.id()).is_some());
        }
        for index in 0..self.links[carrier.index()].queue_commands.len() {
            let process = umts_mac::hsdpa::ProcessId::new(index as u8).expect("admitted process");
            match self.links[carrier.index()].packets.process_state(process) {
                Ok(umts_mac::hsdpa::ProcessState::Empty) => progress.empty_processes += 1,
                Ok(umts_mac::hsdpa::ProcessState::Ready) => progress.ready_processes += 1,
                _ => progress.busy_processes += 1,
            }
        }
        progress
    }

    #[cfg(test)]
    pub fn scheduler_ready(&self) -> bool {
        self.scheduler_ready_on(umts_types::hsdpa::CarrierId::Primary)
    }

    pub fn scheduler_ready_on(&self, carrier: umts_types::hsdpa::CarrierId) -> bool {
        if matches!(
            self.order_progress(),
            OrderProgress::Ready | OrderProgress::Waiting | OrderProgress::Exhausted
        ) {
            return false;
        }
        if !self.has_carrier(carrier) {
            return false;
        }
        if carrier == umts_types::hsdpa::CarrierId::Secondary
            && self.secondary_state() != Some(SecondaryState::Active)
        {
            return false;
        }

        self.phase() == Phase::Active
            && self.links[carrier.index()].scheduling.is_some()
            && self.links[carrier.index()].feedback_policy.is_some()
            && self.links[carrier.index()].pending.is_none()
    }
    pub fn configure_scheduler(
        &mut self,
        resources: Box<SchedulingResources>,
        feedback: crate::hsdpa_rx::FeedbackPolicy,
    ) -> Result<(), Box<SchedulingResources>> {
        self.configure_scheduler_on(umts_types::hsdpa::CarrierId::Primary, resources, feedback)
    }

    pub fn configure_scheduler_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        resources: Box<SchedulingResources>,
        feedback: crate::hsdpa_rx::FeedbackPolicy,
    ) -> Result<(), Box<SchedulingResources>> {
        if !self.has_carrier(carrier) {
            return Err(resources);
        }

        let parameters = self.procedure.construction().parameters();
        if self.links[carrier.index()].scheduling.is_some()
            || self.links[carrier.index()].feedback_policy.is_some()
            || self.retirement.started()
            || !resources.matches_parameters(parameters)
            || self.links[carrier.index()]
                .recycled_blocks
                .iter()
                .any(Option::is_some)
            || (0..resources.buffers.len()).any(|index| {
                self.links[carrier.index()].packets.process_state(
                    umts_mac::hsdpa::ProcessId::new(index as u8).expect("admitted process"),
                ) != Ok(umts_mac::hsdpa::ProcessState::Empty)
            })
        {
            return Err(resources);
        }
        let calibrate = if self
            .procedure
            .construction()
            .parameters()
            .harq
            .category()
            .limits()
            .qam64
        {
            super::adaptation::LinkAdaptation::per_cqi
        } else {
            super::adaptation::LinkAdaptation::new
        };
        self.links[carrier.index()].adaptation = calibrate(
            resources.settings.cqi_offset_limit,
            resources.settings.cqi_target_bler_percent,
        );
        self.links[carrier.index()].scheduling = Some(resources);
        self.links[carrier.index()].feedback_policy = Some(feedback);
        Ok(())
    }

    pub fn schedule_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        calendar: &mut AllocationCalendar,
        preparer: &mut WaveformPreparer,
        time: SchedulingTime,
        waveform: WaveformBuffer,
        mut transmit_cursor: impl FnMut() -> ChipInstant,
    ) -> SchedulingOutcome {
        if !self.has_carrier(carrier) {
            return SchedulingOutcome::Deferred {
                waveform,
                budget_exhausted: false,
            };
        }

        if carrier == umts_types::hsdpa::CarrierId::Secondary
            && self.secondary_state() != Some(SecondaryState::Active)
        {
            return SchedulingOutcome::Deferred {
                waveform,
                budget_exhausted: false,
            };
        }
        let Some(mut resources) = self.links[carrier.index()].scheduling.take() else {
            return SchedulingOutcome::Deferred {
                waveform,
                budget_exhausted: false,
            };
        };
        let prepared_time = self
            .stage_one_payload_on(carrier, &mut resources, time.now)
            .and_then(|staged| {
                if !staged {
                    return Ok(time);
                }
                // Encoding can consume the original lead. Refresh the packet floor
                // before selection without delaying the block to another visit.
                let next = transmit_cursor();
                if next.epoch != time.now.epoch || next.epoch != time.not_before.epoch {
                    return Err(DispatchError::Configuration);
                }
                let floor = next
                    .index
                    .checked_add(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                    .ok_or(DispatchError::Configuration)?;
                Ok(SchedulingTime {
                    not_before: ChipInstant {
                        index: time.not_before.index.max(floor),
                        ..time.not_before
                    },
                    ..time
                })
            });
        let outcome = match prepared_time {
            Ok(time) => self.schedule_with_resources_on(
                carrier,
                &mut resources,
                calendar,
                preparer,
                time,
                waveform,
            ),
            Err(error) => SchedulingOutcome::Failed { waveform, error },
        };
        self.links[carrier.index()].scheduling = Some(resources);
        outcome
    }

    fn schedule_with_resources_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        resources: &mut SchedulingResources,
        calendar: &mut AllocationCalendar,
        preparer: &mut WaveformPreparer,
        time: SchedulingTime,
        waveform: WaveformBuffer,
    ) -> SchedulingOutcome {
        let selection = match self.select_packet_on(
            carrier,
            SelectionRequest {
                now: time.now,
                not_before: time.not_before,
                maximum_cqi_age: resources.settings.maximum_cqi_age,
                plans: &resources.plans,
                calendar,
                budget: resources.settings.selection_budget,
            },
        ) {
            Ok(selection) => selection,
            Err(error) => {
                return SchedulingOutcome::Failed {
                    waveform,
                    error: DispatchError::Selection(error),
                };
            }
        };
        let Some(selected) = selection.selected else {
            return SchedulingOutcome::Deferred {
                waveform,
                budget_exhausted: selection.budget_exhausted,
            };
        };
        match self.match_staged_payload_on(carrier, resources, selected, time.now) {
            Ok(true) => (),
            Ok(false) => {
                return SchedulingOutcome::Deferred {
                    waveform,
                    budget_exhausted: false,
                };
            }
            Err(error) => return SchedulingOutcome::Failed { waveform, error },
        }
        let index = usize::from(selected.candidate.process.value());
        self.links[carrier.index()]
            .feedback_snapshot
            .formats
            .process_selected(selected.candidate.process);
        let staged_queue = match selected.candidate.work {
            PacketWork::New { queue } if self.queues.staged_packet(queue).is_some() => Some(queue),
            _ => None,
        };
        if resources.buffers[index].is_none() {
            resources.buffers[index] =
                self.take_coded_buffer_on(carrier, selected.candidate.process);
        }
        let coded = if matches!(selected.candidate.work, PacketWork::New { .. }) {
            resources.buffers[index].take()
        } else {
            None
        };
        let reported_cqi = self.links[carrier.index()]
            .quality
            .value(time.now, resources.settings.maximum_cqi_age);
        let result = self.prepare_scheduled_packet_on(
            carrier,
            calendar,
            PacketPreparation {
                candidate: selected.candidate,
                timing: time.feedback,
                encoders: &mut resources.encoders,
                plans: &mut resources.plans,
                format: selected.format,
                preparer,
                control_code: selected.control_code,
                power: resources.settings.power,
                rate: selected.rate,
            },
            PacketBuffers { coded, waveform },
        );
        match result {
            Ok(PreparationOutcome::Prepared { unused_coded }) => {
                if let Some(transmission) = self.pending_transmission_on(carrier) {
                    let packet_id = self.links[carrier.index()]
                        .packets
                        .payload(transmission.key.process())
                        .ok()
                        .flatten()
                        .and_then(|payload| payload.delivery_trace());
                    if crate::hsdpa_packet::delivery_trace::event_enabled(packet_id) {
                        tracing::debug!(target: "umts_hnb::delivery_trace",
                            packet_id, transmission = ?transmission, prepared_at = ?time.now,
                            work = ?selected.candidate.work, "Downlink delivery prepared");
                    }
                    self.links[carrier.index()]
                        .feedback_snapshot
                        .formats
                        .selection_cqi(transmission, reported_cqi);
                    self.links[carrier.index()]
                        .feedback_snapshot
                        .formats
                        .preparation_time(transmission, time.now);
                }
                if let Some(buffer) = unused_coded {
                    self.return_spare_buffer_on(carrier, resources, index, staged_queue, buffer);
                }
                SchedulingOutcome::Prepared
            }
            Ok(PreparationOutcome::NoPacket(buffers)) => {
                if let Some(buffer) = buffers.coded {
                    self.return_spare_buffer_on(carrier, resources, index, staged_queue, buffer);
                }
                SchedulingOutcome::Deferred {
                    waveform: buffers.waveform,
                    budget_exhausted: false,
                }
            }
            Err(rejected) => {
                if let Some(buffer) = rejected.buffers.coded {
                    self.return_spare_buffer_on(carrier, resources, index, staged_queue, buffer);
                }
                SchedulingOutcome::Failed {
                    waveform: rejected.buffers.waveform,
                    error: rejected.error,
                }
            }
        }
    }

    /// Rotate each UE independently, including when another UE exhausts waveform capacity.
    pub(crate) fn next_scheduling_carrier(&mut self, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        let first = self.scheduling_carrier_cursor % count;
        self.scheduling_carrier_cursor = (first + 1) % count;
        first
    }

    fn return_staged_buffer_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        resources: &mut SchedulingResources,
        queue: u8,
        buffer: BlockBuffer,
    ) {
        let index = usize::from(queue);
        let owner = self.staged_buffer_carriers[index]
            .take()
            .expect("shared staged payload retains its buffer origin");
        let owner_resources = if owner == carrier {
            resources
        } else {
            self.links[owner.index()]
                .scheduling
                .as_mut()
                .expect("other carrier retains its admitted scheduling resources")
        };
        let slot = &mut owner_resources.staging_buffers[index];
        assert!(
            slot.is_none(),
            "originating staging slot is reserved until bind or cancel"
        );
        *slot = Some(buffer);
    }

    fn return_spare_buffer_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        resources: &mut SchedulingResources,
        process: usize,
        staged_queue: Option<u8>,
        buffer: BlockBuffer,
    ) {
        if let Some(queue) =
            staged_queue.filter(|&queue| self.queues.staged_packet(queue).is_none())
        {
            self.return_staged_buffer_on(carrier, resources, queue, buffer);
        } else {
            assert!(
                resources.buffers[process].is_none(),
                "process spare has one owner"
            );
            resources.buffers[process] = Some(buffer);
        }
    }
}
