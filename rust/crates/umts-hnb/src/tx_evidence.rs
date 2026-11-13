// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use umts_dsp::waveform::WaveformWindow;
use umts_types::time::{ChipInstant, SampleInstant};

const FAILED: u64 = u64::MAX;

mod loss;

#[derive(Debug)]
struct Progress {
    first: SampleInstant,
    next: AtomicU64,
    losses: loss::History,
}

#[derive(Clone)]
pub(crate) struct Reader(Arc<Progress>);
pub(crate) struct Writer {
    progress: Arc<Progress>,
    next: SampleInstant,
}

#[derive(Clone, Debug)]
pub struct TransportSubmission {
    progress: Arc<Progress>,
    first: SampleInstant,
    next: SampleInstant,
    air_end: ChipInstant,
    plan: Option<crate::common_tx::PlanReceipt>,
    emission: Option<crate::dedicated_tx::evidence::EmissionReceipt>,
}

/// Observations can span concurrent progress and do not replace the delivery receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportSubmissionSnapshot {
    pub first: SampleInstant,
    pub next: SampleInstant,
    pub stream_first: SampleInstant,
    pub air_end: ChipInstant,
    pub resolved_until: Option<SampleInstant>,
    pub intersects_loss: Option<bool>,
    pub emission: Option<crate::dedicated_tx::EmissionProgress>,
    pub common_plan: Option<crate::common_tx::PlanStage>,
}

#[derive(Clone, Debug)]
pub struct TransportResolution {
    progress: Arc<Progress>,
    next: SampleInstant,
    air_end: ChipInstant,
}

impl TransportResolution {
    pub fn viable(&self) -> bool {
        self.progress.next.load(Ordering::Acquire) != FAILED
    }

    pub fn resolved(&self) -> bool {
        let next = self.progress.next.load(Ordering::Acquire);
        next != FAILED && next >= self.next.index
    }

    pub fn air_end(&self) -> ChipInstant {
        self.air_end
    }
}

pub(crate) fn channel(first: SampleInstant) -> Option<(Reader, Writer)> {
    if first.index == FAILED {
        return None;
    }
    let progress = Arc::new(Progress {
        first,
        next: AtomicU64::new(first.index),
        losses: loss::History::default(),
    });
    Some((
        Reader(progress.clone()),
        Writer {
            progress,
            next: first,
        },
    ))
}

impl Reader {
    pub(crate) fn paired_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn available(&self) -> bool {
        self.0.next.load(Ordering::Acquire) != FAILED
    }

    pub fn transport_receipt(
        &self,
        start: ChipInstant,
        rate: usize,
        group_delay: usize,
    ) -> Option<TransportSubmission> {
        let end = ChipInstant {
            epoch: start.epoch,
            index: start.index.checked_add(umts_types::time::CHIPS_PER_FRAME)?,
        };
        self.interval_receipt(start, end, rate, group_delay)
    }
    pub fn interval_receipt(
        &self,
        start: ChipInstant,
        end: ChipInstant,
        rate: usize,
        group_delay: usize,
    ) -> Option<TransportSubmission> {
        if rate == 0 || end.epoch != start.epoch || end.index <= start.index {
            return None;
        }
        let first = start.index.checked_mul(rate as u64)?;
        let next = end
            .index
            .checked_mul(rate as u64)?
            .checked_add((group_delay as u64).checked_mul(2)?)?;
        self.receipt(
            SampleInstant {
                epoch: start.epoch,
                index: first,
            },
            SampleInstant {
                epoch: start.epoch,
                index: next,
            },
            ChipInstant {
                epoch: start.epoch,
                index: next / rate as u64,
            },
        )
    }
    pub fn interval_resolution(
        &self,
        start: ChipInstant,
        end: ChipInstant,
        rate: usize,
        group_delay: usize,
    ) -> Option<TransportResolution> {
        self.interval_receipt(start, end, rate, group_delay)
            .map(|receipt| receipt.resolution())
    }
    pub fn origin(&self) -> SampleInstant {
        self.0.first
    }
    pub fn receipt(
        &self,
        first: SampleInstant,
        next: SampleInstant,
        air_end: ChipInstant,
    ) -> Option<TransportSubmission> {
        if first.epoch != self.0.first.epoch
            || next.epoch != first.epoch
            || air_end.epoch != first.epoch
            || first.index < self.0.first.index
            || next.index <= first.index
            || next.index == FAILED
        {
            return None;
        }
        Some(TransportSubmission {
            progress: self.0.clone(),
            first,
            next,
            air_end,
            plan: None,
            emission: None,
        })
    }
}
impl Writer {
    pub fn lost(&mut self, window: WaveformWindow) -> bool {
        if self.progress.next.load(Ordering::Acquire) == FAILED
            || window.first_sample != self.next
            || window.next_sample.epoch != self.next.epoch
            || window.next_sample.index <= self.next.index
            || window.next_sample.index == FAILED
        {
            self.fail();
            return false;
        }
        let recorded = self.progress.losses.record(
            window.first_sample.index,
            window.next_sample.index,
            || {
                self.progress
                    .next
                    .store(window.next_sample.index, Ordering::SeqCst)
            },
        );
        if !recorded {
            self.fail();
            return false;
        }
        self.next = window.next_sample;
        true
    }

    pub fn accepted(&mut self, window: WaveformWindow) -> bool {
        if self.progress.next.load(Ordering::Acquire) == FAILED
            || window.first_sample != self.next
            || window.next_sample.epoch != self.next.epoch
            || window.next_sample.index <= self.next.index
            || window.next_sample.index == FAILED
        {
            self.fail();
            return false;
        }
        self.next = window.next_sample;
        self.progress.next.store(self.next.index, Ordering::Release);
        true
    }
    pub fn fail(&self) {
        self.progress.next.store(FAILED, Ordering::Release);
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.fail();
    }
}

impl TransportSubmission {
    pub fn snapshot(&self) -> TransportSubmissionSnapshot {
        let next = self.progress.next.load(Ordering::Acquire);
        TransportSubmissionSnapshot {
            first: self.first,
            next: self.next,
            stream_first: self.progress.first,
            air_end: self.air_end,
            resolved_until: (next != FAILED).then_some(SampleInstant {
                epoch: self.first.epoch,
                index: next,
            }),
            intersects_loss: self
                .progress
                .losses
                .intersects(self.first.index, self.next.index),
            emission: self.emission.as_ref().map(|emission| emission.snapshot()),
            common_plan: self.plan.as_ref().map(|plan| plan.stage()),
        }
    }
    fn resolution(&self) -> TransportResolution {
        TransportResolution {
            progress: self.progress.clone(),
            next: self.next,
            air_end: self.air_end,
        }
    }

    pub fn viable(&self) -> bool {
        self.progress.next.load(Ordering::Acquire) != FAILED
            && self
                .progress
                .losses
                .intersects(self.first.index, self.next.index)
                != Some(true)
            && self
                .emission
                .as_ref()
                .is_none_or(|emission| emission.viable())
            && self.plan.as_ref().is_none_or(|plan| {
                matches!(
                    plan.stage(),
                    crate::common_tx::PlanStage::Queued
                        | crate::common_tx::PlanStage::Scheduled
                        | crate::common_tx::PlanStage::Committed
                )
            })
    }
    /// Driver acceptance includes the complete pulse tail, not RF transmission or peer reception.
    pub fn submitted(&self) -> bool {
        let next = self.progress.next.load(Ordering::Acquire);
        next != FAILED
            && self
                .progress
                .losses
                .intersects(self.first.index, self.next.index)
                == Some(false)
            && self
                .emission
                .as_ref()
                .is_none_or(|emission| emission.generated())
            && next >= self.next.index
            && self.first.index >= self.progress.first.index
            && self
                .plan
                .as_ref()
                .is_none_or(|plan| plan.stage() == crate::common_tx::PlanStage::Committed)
    }
    pub fn air_end(&self) -> ChipInstant {
        self.air_end
    }
    pub(crate) fn detach_submitted_emission(&mut self) -> bool {
        if self.emission.is_none() || !self.submitted() {
            return false;
        }
        // Generated state is irreversible before pool reuse. Keep driver-prefix and plan evidence.
        self.emission = None;
        true
    }
    pub(crate) fn with_emission(
        mut self,
        emission: crate::dedicated_tx::evidence::EmissionReceipt,
    ) -> Self {
        self.emission = Some(emission);
        self
    }
    pub(crate) fn with_plan(mut self, plan: crate::common_tx::PlanReceipt) -> Self {
        self.plan = Some(plan);
        self
    }
    pub fn plan_stage(&self) -> Option<crate::common_tx::PlanStage> {
        self.plan.as_ref().map(|plan| plan.stage())
    }
    pub fn plan_id(&self) -> Option<crate::common_tx::PlanId> {
        self.plan.as_ref().map(|plan| plan.id())
    }
}

#[cfg(test)]
mod tests;
