// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_packet::{BlockBuffer, Resolution};
use umts_mac::hsdpa::ProcessId;

#[derive(Clone, Copy, Debug, Default)]
pub struct ResolutionStep {
    pub visited: usize,
    pub delivered: usize,
    pub retry: usize,
    pub not_transmitted: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RetirementDrainStep {
    pub reports: usize,
    pub processes: usize,
    pub waiting: usize,
}

impl Session {
    pub fn drain_retirement(&mut self, budget: NonZeroUsize) -> Result<RetirementDrainStep, Error> {
        if !self.retirement.started() {
            return Err(Error::Phase);
        }
        let mut step = RetirementDrainStep::default();
        for _ in 0..budget.get() {
            let Some(report) = self.feedback.try_report() else {
                break;
            };
            self.links[0].feedback_snapshot.record(report);
            step.reports += 1;
        }
        let processes = self
            .links
            .iter()
            .map(|link| link.queue_commands.len())
            .sum::<usize>();
        for _ in 0..budget.get().min(processes) {
            let slot = self.resolution_link_cursor;
            self.resolution_link_cursor = (slot + 1) % self.links.len();
            let link = &mut self.links[slot];
            if link.pending.is_some() {
                step.waiting += 1;
                continue;
            }
            let index = link.resolution_cursor;
            link.resolution_cursor = (index + 1) % link.queue_commands.len();
            if link.queue_commands[index].is_some() {
                step.waiting += 1;
                continue;
            }
            let process = ProcessId::new(index as u8).expect("admitted HARQ process");
            if link
                .packets
                .retire_process(process)
                .map_err(Error::Packet)?
            {
                step.processes += 1;
            } else {
                step.waiting += 1;
            }
        }
        Ok(step)
    }

    pub fn take_coded_buffer_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        process: ProcessId,
    ) -> Option<BlockBuffer> {
        self.links[carrier.index()]
            .recycled_blocks
            .get_mut(usize::from(process.value()))?
            .take()
    }

    pub fn resolve_packets(&mut self, budget: NonZeroUsize) -> Result<ResolutionStep, Error> {
        if !matches!(
            self.phase(),
            Phase::Active | Phase::ChangingRadio | Phase::Retiring
        ) {
            return Err(Error::Phase);
        }
        let mut step = ResolutionStep::default();
        let processes = self
            .links
            .iter()
            .map(|link| link.recycled_blocks.len())
            .sum::<usize>();
        for _ in 0..budget.get().min(processes) {
            let slot = self.resolution_link_cursor;
            self.resolution_link_cursor = (slot + 1) % self.links.len();
            let link = &mut self.links[slot];
            let index = link.resolution_cursor;
            link.resolution_cursor = (index + 1) % link.recycled_blocks.len();
            let carrier = link.carrier;
            let process = ProcessId::new(index as u8).expect("admitted HARQ process");
            self.resolve_process_on(carrier, process, &mut step)?;
        }
        Ok(step)
    }

    pub(super) fn resolve_process(
        &mut self,
        process: ProcessId,
        step: &mut ResolutionStep,
    ) -> Result<(), Error> {
        self.resolve_process_on(umts_types::hsdpa::CarrierId::Primary, process, step)
    }

    pub(super) fn resolve_process_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        process: ProcessId,
        step: &mut ResolutionStep,
    ) -> Result<(), Error> {
        let link = &mut self.links[carrier.index()];
        let index = usize::from(process.value());
        let recycled = link.recycled_blocks.get_mut(index).ok_or(Error::Identity)?;
        if recycled.is_some() {
            return Ok(());
        }
        step.visited += 1;
        match link
            .packets
            .resolve_if_pending(process)
            .map_err(Error::Packet)?
        {
            Some(Resolution::Delivered(block)) => {
                link.feedback_snapshot.formats.process_ready(process);
                *recycled = Some(block.recycle());
                step.delivered += 1;
            }
            Some(Resolution::Retry) => {
                link.feedback_snapshot.formats.process_ready(process);
                step.retry += 1;
            }
            Some(Resolution::NotTransmitted) => step.not_transmitted += 1,
            None => (),
        }
        Ok(())
    }
}
