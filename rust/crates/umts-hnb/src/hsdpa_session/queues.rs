// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{BearerId, TransmitError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueFeedStep {
    pub visited: usize,
    pub enqueued: usize,
    pub empty: usize,
    pub backpressure: usize,
}

impl Session {
    pub fn feed_queues(
        &mut self,
        bank: &mut DedicatedBearers,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        budget: NonZeroUsize,
    ) -> Result<QueueFeedStep, TransmitError<MacError>> {
        if self.phase() != Phase::Active || bank.identity() != self.identity {
            return Err(BearerError::Phase.into());
        }
        if matches!(
            bank.mapping_phase(self.identity)?,
            crate::dedicated_bearers::MappingPhase::Draining { .. }
        ) {
            return Ok(QueueFeedStep::default());
        }
        let activation = self.procedure.construction().activation().time();
        if now.epoch != activation.epoch {
            return Err(BearerError::Time.into());
        }
        if now.index < activation.index {
            return Ok(QueueFeedStep::default());
        }
        let mappings = self.procedure.construction().mappings();
        let mut report = QueueFeedStep::default();
        let mut stalled = 0;
        for _ in 0..budget.get() {
            let mapping = &mappings[self.queue_cursor];
            self.queue_cursor = (self.queue_cursor + 1) % mappings.len();
            let bearer = BearerId::new(mapping.bearer)?;
            if !bank.has_active_hsdpa_bearer(self.identity, bearer)? {
                stalled += 1;
                if stalled == mappings.len() {
                    break;
                }
                continue;
            }
            let grant = bank.hsdpa_grant(self.identity, bearer, now)?;
            report.visited += 1;
            match self.queues.enqueue(bank, grant, now, ciphers) {
                Ok(Some(())) => {
                    report.enqueued += 1;
                    stalled = 0;
                }
                Ok(None) => {
                    report.empty += 1;
                    stalled += 1;
                }
                Err(TransmitError::Admission(MacError::Capacity)) => {
                    report.backpressure += 1;
                    stalled += 1;
                }
                Err(error) => return Err(error),
            }
            if stalled == mappings.len() {
                break;
            }
        }
        Ok(report)
    }
}
