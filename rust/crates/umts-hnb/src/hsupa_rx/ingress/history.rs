// SPDX-License-Identifier: AGPL-3.0-or-later

use super::Report;
use umts_dsp::transport_blocks::BlockOutcome;
use umts_mac::hsupa::Reception;
use umts_types::hsupa::Tti;

const MAX_PROCESSES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckDisposition {
    Queued,
    Late,
    Rejected,
    Submitted,
    Lost,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct RetryEvidence {
    pub previous: Report,
    pub next: Report,
    pub acknowledgement: AckDisposition,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackHistoryCounters {
    pub observed_process_reuses: u64,
    pub retry_after_submitted_ack: u64,
    pub retry_after_late_ack: u64,
    pub retry_after_rejected_ack: u64,
    pub retry_after_lost_ack: u64,
    pub retry_before_completion: u64,
    pub new_after_submitted_ack: u64,
    pub last_retry: Option<RetryEvidence>,
}

#[derive(Clone, Copy)]
struct Entry {
    report: Report,
    disposition: AckDisposition,
}

pub(super) struct FeedbackHistory {
    processes: u64,
    entries: [Option<Entry>; MAX_PROCESSES],
    counters: FeedbackHistoryCounters,
}

impl FeedbackHistory {
    pub fn new(interval: Tti) -> Self {
        Self {
            processes: u64::from(interval.processes()),
            entries: [None; MAX_PROCESSES],
            counters: FeedbackHistoryCounters::default(),
        }
    }

    pub fn counters(&self) -> FeedbackHistoryCounters {
        self.counters
    }

    pub fn observe(&mut self, report: Report, disposition: AckDisposition) {
        let index = (report.absolute_tti % self.processes) as usize;
        if let Some(previous) = self.entries[index].filter(|entry| {
            entry.report.absolute_tti.checked_add(self.processes) == Some(report.absolute_tti)
        }) {
            self.counters.observed_process_reuses =
                self.counters.observed_process_reuses.saturating_add(1);
            let old = previous.report.control.information;
            let new = report.control.information;
            if new.sequence.value() != 0 && new.format == old.format {
                let count = match previous.disposition {
                    AckDisposition::Submitted => &mut self.counters.retry_after_submitted_ack,
                    AckDisposition::Late => &mut self.counters.retry_after_late_ack,
                    AckDisposition::Rejected => &mut self.counters.retry_after_rejected_ack,
                    AckDisposition::Lost => &mut self.counters.retry_after_lost_ack,
                    AckDisposition::Queued => &mut self.counters.retry_before_completion,
                };
                *count = count.saturating_add(1);
                self.counters.last_retry = Some(RetryEvidence {
                    previous: previous.report,
                    next: report,
                    acknowledgement: previous.disposition,
                });
            } else if new.sequence.value() == 0 && previous.disposition == AckDisposition::Submitted
            {
                self.counters.new_after_submitted_ack =
                    self.counters.new_after_submitted_ack.saturating_add(1);
            }
        }
        self.entries[index] = (report.outcome == BlockOutcome::Decoded
            && matches!(report.feedback, Reception::Ack { .. }))
        .then_some(Entry {
            report,
            disposition,
        });
    }

    // A delayed completion must never update a newer reuse of the same HARQ process.
    pub fn disposition(&mut self, tti: u64, disposition: AckDisposition) {
        if let Some(entry) = &mut self.entries[(tti % self.processes) as usize]
            && entry.report.absolute_tti == tti
        {
            entry.disposition = disposition;
        }
    }
}
