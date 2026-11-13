// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_session::adaptation::FeedbackTraining;

impl Session {
    pub(super) fn match_staged_payload_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        resources: &mut SchedulingResources,
        selected: SelectedPacket,
        now: ChipInstant,
    ) -> Result<bool, DispatchError> {
        let PacketWork::New { queue } = selected.candidate.work else {
            return Ok(true);
        };
        let Some(packet) = self.queues.staged_packet(queue) else {
            return Ok(true);
        };
        let index = usize::from(queue);
        if packet.transport_bits != selected.format.transport_bits.get() {
            let Some(encoder) = resources.encoders.get_mut(selected.format.transport_bits) else {
                return Err(DispatchError::Configuration);
            };
            // A staged queue prefix is not committed until it enters a HARQ process.
            let buffer = self
                .queues
                .cancel_staged(queue)
                .map_err(|error| DispatchError::Selection(Error::Packet(error)))?
                .ok_or(DispatchError::Configuration)?;
            match self.queues.prepare_queue(
                encoder,
                self.links[carrier.index()].packets.category(),
                queue,
                buffer,
            ) {
                // Re-encoding can outlive the selected control publication window.
                Ok(None) => return Ok(false),
                Ok(Some(buffer)) => {
                    self.return_staged_buffer_on(carrier, resources, queue, buffer);
                    return Ok(false);
                }
                Err(rejected) => {
                    self.return_staged_buffer_on(carrier, resources, queue, rejected.buffer);
                    return Err(DispatchError::Encoding(rejected.error));
                }
            }
        }
        // The shared queue still owns the uncommitted prefix. Use its full backlog, including
        // packets beyond this packed block.
        // Transport padding is not demand.
        let pending_bits = self.queues.pending_packet_bits(queue);
        let training = self
            .latest_cqi_on(carrier, now, resources.settings.maximum_cqi_age)
            .map_or(FeedbackTraining::None, |cqi| {
                self.links[carrier.index()].rates.feedback_training(
                    cqi,
                    self.excess_rate_steps_on(carrier, now, resources.settings.maximum_cqi_age),
                    selected.rate,
                    pending_bits,
                )
            });
        if let Some(staged_training) = self.links[carrier.index()]
            .staged_feedback_training
            .get_mut(index)
        {
            *staged_training = training;
        }
        Ok(true)
    }

    pub(super) fn stage_one_payload_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        resources: &mut SchedulingResources,
        now: ChipInstant,
    ) -> Result<bool, DispatchError> {
        if self.phase() != Phase::Active
            || self.links[carrier.index()].pending.is_some()
            || self.retirement.started()
        {
            return Ok(false);
        }
        for index in 0..self.links[carrier.index()].queue_commands.len() {
            let process =
                umts_mac::hsdpa::ProcessId::new(index as u8).expect("admitted HARQ process");
            if self.links[carrier.index()].queue_commands[index].is_none()
                && self.links[carrier.index()].packets.process_state(process)
                    == Ok(umts_mac::hsdpa::ProcessState::Ready)
            {
                return Ok(false);
            }
        }
        let Some(cqi) = self.latest_cqi_on(carrier, now, resources.settings.maximum_cqi_age) else {
            return Ok(false);
        };
        let excess_steps =
            self.excess_rate_steps_on(carrier, now, resources.settings.maximum_cqi_age);
        let queue_count = self
            .procedure
            .construction()
            .parameters()
            .queues
            .queues()
            .len();
        for _ in 0..queue_count {
            let cursor = resources.staging_cursor % queue_count;
            resources.staging_cursor = (cursor + 1) % queue_count;
            let queue = self.procedure.construction().parameters().queues.queues()[cursor].id();
            let index = usize::from(queue);
            if let Some(packet) = self.queues.staged_packet(queue) {
                if self.links[carrier.index()]
                    .rates
                    .adapted_candidates(cqi, excess_steps)
                    .any(|rate| rate.reference.transport_bits.get() == packet.transport_bits)
                {
                    continue;
                }
                let can_bind_new = self.links[carrier.index()]
                    .queue_commands
                    .iter()
                    .enumerate()
                    .any(|(index, command)| {
                        let process = umts_mac::hsdpa::ProcessId::new(index as u8)
                            .expect("admitted HARQ process");
                        command.is_none()
                            && self.links[carrier.index()].packets.process_state(process)
                                == Ok(umts_mac::hsdpa::ProcessState::Empty)
                    });
                // A busy carrier must not replace a shared prefix its sibling can transmit.
                if !can_bind_new {
                    continue;
                }
                let buffer = self
                    .queues
                    .cancel_staged(queue)
                    .map_err(|error| DispatchError::Selection(Error::Packet(error)))?
                    .expect("observed staged packet owns a buffer");
                self.return_staged_buffer_on(carrier, resources, queue, buffer);
            }
            if self.queues.queued(queue) == 0 {
                continue;
            }
            let Some(buffer) = resources.staging_buffers[index].take() else {
                continue;
            };
            let pending_bits = self.queues.pending_packet_bits(queue);
            let Some(rate) = self.links[carrier.index()].rates.demand_rate_within(
                cqi,
                excess_steps,
                pending_bits,
                self.shared_tti_transport_bits(queue),
            ) else {
                resources.staging_buffers[index] = Some(buffer);
                return Ok(false);
            };
            self.links[carrier.index()]
                .feedback_snapshot
                .staging
                .record(cqi, rate.cqi, pending_bits);
            let Some(encoder) = resources.encoders.get_mut(rate.reference.transport_bits) else {
                resources.staging_buffers[index] = Some(buffer);
                return Err(DispatchError::Configuration);
            };
            match self.queues.prepare_queue(
                encoder,
                self.links[carrier.index()].packets.category(),
                queue,
                buffer,
            ) {
                Ok(unused) => {
                    if unused.is_none() {
                        assert!(self.staged_buffer_carriers[index].is_none());
                        self.staged_buffer_carriers[index] = Some(carrier);
                    }
                    let staged = unused.is_none();
                    resources.staging_buffers[index] = unused;
                    let link = &mut self.links[carrier.index()];
                    if let Some(staged_training) = link.staged_feedback_training.get_mut(index) {
                        *staged_training =
                            link.rates
                                .feedback_training(cqi, excess_steps, rate, pending_bits);
                    }
                    return Ok(staged);
                }
                Err(rejected) => {
                    resources.staging_buffers[index] = Some(rejected.buffer);
                    return Err(DispatchError::Encoding(rejected.error));
                }
            }
        }
        Ok(false)
    }
}
