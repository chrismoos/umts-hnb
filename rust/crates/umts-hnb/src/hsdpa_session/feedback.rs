// SPDX-License-Identifier: AGPL-3.0-or-later

use super::adaptation::CQI_VALUES;
use super::*;
use crate::hsdpa_rx::{AckRejection, Evidence, FeedbackPolicy, Measurement};
use umts_dsp::hsdpa::Cqi;

#[cfg(test)]
mod adaptation_tests;
mod formats;
#[cfg(test)]
mod quality_tests;
#[cfg(test)]
mod rebind_tests;
#[cfg(test)]
mod resolution_tests;
pub use formats::{AirtimeSummary, FormatStatistics};

pub(super) fn record_admission(
    aggregate: &mut Option<Result<(), FeedbackError>>,
    result: Result<(), FeedbackError>,
) {
    if !aggregate.is_some_and(|previous| previous.is_err()) {
        *aggregate = Some(result);
    }
}

#[derive(Default)]
pub(super) struct Quality {
    last_report: Option<ChipInstant>,
    latest: Option<(ChipInstant, Cqi)>,
    next_pdsch: Option<ChipInstant>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CqiRequestStep {
    pub visited: usize,
    pub requested: usize,
    pub backpressure: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackSnapshot {
    pub acknowledgement: Option<Report>,
    pub cqi: Option<Report>,
    /// Accepted CQI reports by value, before outer-loop adjustment.
    pub cqi_reports: [u32; CQI_VALUES],
    /// Most recently updated offset in thousandths of a CQI step.
    pub cqi_offset_milli: i32,
    pub outcomes: FeedbackCounts,
    pub rejections: AckRejections,
    pub selection: SelectionCounts,
    pub staging: StagingCounts,
    pub formats: FormatStatistics,
}

#[derive(Clone, Copy, Default)]
pub struct AckRejections {
    counts: [u64; AckRejection::ALL.len()],
    last: Option<(AckRejection, Report)>,
}

impl AckRejections {
    fn record(&mut self, reason: AckRejection, report: Report) {
        let count = &mut self.counts[reason as usize];
        *count = count.saturating_add(1);
        self.last = Some((reason, report));
    }
}

impl std::fmt::Debug for AckRejections {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AckRejections")
            .field(
                "counts",
                &AckRejection::ALL.map(|reason| (reason, self.counts[reason as usize])),
            )
            .field("last", &self.last)
            .finish()
    }
}

/// Block-size demand decisions at staging time: the adjusted CQI offered, the CQI whose
/// block was chosen, and the MAC-hs bits that were pending when the choice was made.
#[derive(Clone, Copy, Debug)]
pub struct StagingCounts {
    pub calls: u64,
    pub offered_cqi: [u32; 31],
    pub demand_cqi: [u32; 31],
    pub pending_bits_last: u64,
    pub pending_bits_min: u64,
    pub pending_bits_max: u64,
}

impl Default for StagingCounts {
    fn default() -> Self {
        Self {
            calls: 0,
            offered_cqi: [0; 31],
            demand_cqi: [0; 31],
            pending_bits_last: 0,
            pending_bits_min: u64::MAX,
            pending_bits_max: 0,
        }
    }
}

impl StagingCounts {
    pub(super) fn record(&mut self, offered: Cqi, demand: Cqi, pending_bits: usize) {
        self.calls = self.calls.saturating_add(1);
        let offered = &mut self.offered_cqi[usize::from(offered.value())];
        *offered = offered.saturating_add(1);
        let demand = &mut self.demand_cqi[usize::from(demand.value())];
        *demand = demand.saturating_add(1);
        let pending = pending_bits as u64;
        self.pending_bits_last = pending;
        self.pending_bits_min = self.pending_bits_min.min(pending);
        self.pending_bits_max = self.pending_bits_max.max(pending);
    }
}

impl FeedbackSnapshot {
    pub(super) fn admit_with_adaptation(
        &mut self,
        adaptation: &mut super::adaptation::LinkAdaptation,
        transmission: umts_mac::hsdpa::ScheduledTransmission,
        classification: Option<Feedback>,
        processed_until: Option<ChipInstant>,
    ) {
        // admitted_format consumes the association needed for exact per-CQI calibration.
        let reported = self.formats.first_reported_cqi(transmission);
        self.formats
            .receiver_progress(transmission, processed_until);
        self.admitted_format(transmission, classification);
        adaptation.observe_feedback(transmission.key.process(), classification, reported);
        self.cqi_offset_milli = adaptation.offset_milli();
    }

    pub(super) fn prepared_format(
        &mut self,
        transmission: umts_mac::hsdpa::ScheduledTransmission,
        format: crate::hsdpa_packet::PlanFormat,
        retry: bool,
    ) {
        self.formats.prepared(transmission, format, retry);
    }

    pub(super) fn admitted_format(
        &mut self,
        transmission: umts_mac::hsdpa::ScheduledTransmission,
        classification: Option<Feedback>,
    ) {
        self.formats.admitted(transmission, classification);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackCounts {
    pub ack: u64,
    pub nack: u64,
    pub dtx: u64,
    pub missing: u64,
    pub rejected: u64,
}

impl FeedbackCounts {
    pub(super) fn record(&mut self, admitted: bool, classification: Option<Feedback>) {
        let counter = if !admitted {
            &mut self.rejected
        } else {
            match classification {
                Some(Feedback::Ack) => &mut self.ack,
                Some(Feedback::Nack) => &mut self.nack,
                Some(Feedback::Dtx) => &mut self.dtx,
                None => &mut self.missing,
            }
        };
        *counter = counter.saturating_add(1);
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn rejected_reports_do_not_count_their_classification() {
        let mut counts = FeedbackCounts::default();
        for value in [Feedback::Ack, Feedback::Nack, Feedback::Dtx] {
            counts.record(true, Some(value));
            counts.record(false, Some(value));
        }
        counts.record(true, None);
        counts.record(false, None);
        assert_eq!(
            (
                counts.ack,
                counts.nack,
                counts.dtx,
                counts.missing,
                counts.rejected
            ),
            (1, 1, 1, 1, 4)
        );
        counts.ack = u64::MAX;
        counts.record(true, Some(Feedback::Ack));
        assert_eq!(counts.ack, u64::MAX);
    }
}

// Counts selection calls, not air TTIs or scheduler occupancy.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectionCounts {
    pub calls: u64,
    pub no_policy: u64,
    pub no_fresh_cqi: u64,
    pub no_candidate: u64,
    pub empty_packet_queues: u64,
    pub no_available_process: u64,
    pub budget_exhausted: u64,
    pub calendar_capacity: u64,
    pub no_format: u64,
    pub selected: u64,
}

impl FeedbackSnapshot {
    pub(super) fn record(&mut self, report: Report) {
        let (slot, start) = match report.request {
            Request::Ack { transmission, .. } => {
                (&mut self.acknowledgement, transmission.expected_ack)
            }
            Request::Cqi { start, .. } | Request::CqiCells { start, .. } => (&mut self.cqi, start),
            Request::AckCells { start, .. } | Request::OrderAck { start, .. } => {
                (&mut self.acknowledgement, start)
            }
        };
        let previous = slot.map(|report| match report.request {
            Request::Ack { transmission, .. } => transmission.expected_ack,
            Request::Cqi { start, .. }
            | Request::AckCells { start, .. }
            | Request::CqiCells { start, .. }
            | Request::OrderAck { start, .. } => start,
        });
        if previous
            .is_none_or(|previous| previous.epoch == start.epoch && previous.index < start.index)
        {
            *slot = Some(report);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FeedbackServiceStep {
    pub delivered: usize,
    pub retry: usize,
    pub not_transmitted: usize,
    pub requested: usize,
    pub backpressure: bool,
    pub reports: usize,
    pub failures: usize,
}

impl Quality {
    pub(super) fn begin_trial(&mut self, effective: ChipInstant) {
        self.latest = None;
        self.last_report = Some(effective);
    }

    /// Returns the newly accepted CQI when this report advances the latest observation.
    fn observe(&mut self, report: &Report, policy: &FeedbackPolicy) -> Option<Cqi> {
        let Request::Cqi { start, .. } = report.request else {
            return None;
        };
        let index = start.index.checked_add(umts_types::time::CHIPS_PER_SLOT)?;
        let start = ChipInstant { index, ..start };
        if self
            .last_report
            .is_some_and(|previous| previous.epoch != start.epoch || previous.index >= start.index)
        {
            return None;
        }
        self.last_report = Some(start);
        let accepted = match &report.result {
            Ok(Measurement::Cqi(Evidence::Measured(observation))) => {
                policy.classify_cqi(observation)
            }
            _ => None,
        };
        if let Some(value) = accepted {
            self.latest = Some((start, value));
        }
        accepted
    }

    pub(super) fn value(&self, now: ChipInstant, maximum_symbol_age: NonZeroU64) -> Option<Cqi> {
        let (start, value) = self.latest?;
        if now.epoch != start.epoch
            || now
                .index
                .checked_sub(start.index)
                .is_none_or(|age| age > maximum_symbol_age.get())
        {
            return None;
        }
        Some(value)
    }
}

impl Session {
    pub fn feedback_snapshot(&self) -> FeedbackSnapshot {
        self.feedback_snapshot_on(umts_types::hsdpa::CarrierId::Primary)
    }

    pub fn feedback_snapshot_on(&self, carrier: umts_types::hsdpa::CarrierId) -> FeedbackSnapshot {
        if !self.has_carrier(carrier) {
            return FeedbackSnapshot::default();
        }

        FeedbackSnapshot {
            ..self.links[carrier.index()].feedback_snapshot
        }
    }

    pub fn feedback_timing(&self, bank: &DedicatedBearers) -> Result<FeedbackTiming, Error> {
        if bank.identity() != self.identity {
            return Err(Error::Identity);
        }
        let downlink = bank.downlink_clock(self.identity).map_err(Error::Bearer)?;
        FeedbackTiming::new(downlink.anchor, bank.frame_anchor(), self.downlink_origin)
            .map_err(Error::Timing)
    }

    pub fn service_feedback(
        &mut self,
        bank: &DedicatedBearers,
        now: ChipInstant,
        horizon: ChipInstant,
    ) -> Result<FeedbackServiceStep, Error> {
        if bank.identity() != self.identity || self.phase() != Phase::Active {
            return Err(Error::Identity);
        }
        let timing = self.feedback_timing(bank)?;
        self.ensure_secondary_feedback(timing)?;
        // Each carrier has its own ACK request, with a shared CQI report.
        let budget =
            NonZeroUsize::new(self.carriers().count() + 1).expect("feedback report budget");
        let mut step = self.service_feedback_reports(budget)?;
        let resolved = self.resolve_packets(budget)?;
        step.delivered += resolved.delivered;
        step.retry += resolved.retry;
        step.not_transmitted += resolved.not_transmitted;
        let requested = self.request_periodic_cqi(timing, now, horizon, budget)?;
        step.requested = requested.requested;
        step.backpressure = requested.backpressure;
        Ok(step)
    }

    fn ensure_secondary_feedback(&mut self, timing: FeedbackTiming) -> Result<(), Error> {
        if !self.has_carrier(umts_types::hsdpa::CarrierId::Secondary) {
            return Ok(());
        }
        match self.secondary_control {
            // finish_radio_change owns rebasing an existing procedure. Preserve
            // its settled or pending order state and exact codebook schedule.
            SecondaryControl::Dual { .. } => Ok(()),
            SecondaryControl::Recovery => Err(Error::Phase),
            SecondaryControl::Single => {
                let origin = timing
                    .first_acknowledgement(self.procedure.construction().activation().time())
                    .map_err(Error::Timing)?;
                self.configure_secondary_control(origin)
            }
        }
    }

    fn service_feedback_reports(
        &mut self,
        budget: NonZeroUsize,
    ) -> Result<FeedbackServiceStep, Error> {
        let mut step = FeedbackServiceStep::default();
        let mut acknowledged = ResolutionStep::default();
        for _ in 0..budget.get() {
            let report = if let Some(policy) = self.links[0].feedback_policy {
                self.poll_classified_feedback(&policy)
            } else {
                self.poll_feedback(|_| Feedback::Dtx)
            };
            let Some(report) = report else { break };
            step.reports += 1;
            step.failures += usize::from(
                report.report.result.is_err()
                    || report.admission.is_some_and(|admission| admission.is_err()),
            );
            self.resolve_admitted_feedback(&report, &mut acknowledged)?;
        }
        step.delivered = acknowledged.delivered;
        step.retry = acknowledged.retry;
        step.not_transmitted = acknowledged.not_transmitted;
        Ok(step)
    }

    fn resolve_admitted_feedback(
        &mut self,
        report: &FeedbackStep,
        resolved: &mut ResolutionStep,
    ) -> Result<(), Error> {
        // A composite report can admit one carrier and reject the other. Never
        // use the aggregate status or infer a process from the sibling carrier.
        if let Some(transmission) = report.admitted_transmissions[0] {
            self.resolve_process(transmission.key.process(), resolved)?;
        }
        if let Some(transmission) = report.admitted_transmissions[1] {
            self.resolve_process_on(
                umts_types::hsdpa::CarrierId::Secondary,
                transmission.key.process(),
                resolved,
            )?;
        }
        Ok(())
    }

    pub fn request_periodic_cqi(
        &mut self,
        timing: FeedbackTiming,
        now: ChipInstant,
        horizon: ChipInstant,
        budget: NonZeroUsize,
    ) -> Result<CqiRequestStep, Error> {
        if self.phase() != Phase::Active {
            return Err(Error::Phase);
        }
        let activation = self.procedure.construction().activation().time();
        if now.epoch != activation.epoch || horizon.epoch != now.epoch || horizon.index < now.index
        {
            return Err(Error::Bearer(BearerError::Time));
        }
        let measurement = self.procedure.construction().parameters().measurement;
        let mut step = CqiRequestStep::default();
        if measurement.cycle().period_ttis().is_none() {
            return Ok(step);
        }
        let first = ChipInstant {
            index: now.index.max(activation.index).max(
                self.links[0]
                    .quality
                    .next_pdsch
                    .map_or(0, |next| next.index),
            ),
            ..now
        };
        let mut pdsch = timing.next_pdsch(first).map_err(Error::Timing)?;
        for _ in 0..budget.get() {
            if pdsch.index > horizon.index {
                break;
            }
            let next = ChipInstant {
                index: pdsch
                    .index
                    .checked_add(3 * umts_types::time::CHIPS_PER_SLOT)
                    .ok_or(Error::Bearer(BearerError::Time))?,
                ..pdsch
            };
            step.visited += 1;
            if let Some(start) = timing
                .cqi_subframe(pdsch, measurement.cycle())
                .map_err(Error::Timing)?
            {
                if self
                    .feedback
                    .request(match self.feedback_mode {
                        Some(schedule) => Request::CqiCells {
                            start,
                            repetitions: measurement.repetition().get(),
                            schedule,
                        },
                        None => Request::Cqi {
                            start,
                            repetitions: measurement.repetition().get(),
                        },
                    })
                    .is_err()
                {
                    self.links[0].quality.next_pdsch = Some(pdsch);
                    step.backpressure = true;
                    return Ok(step);
                }
                step.requested += 1;
            }
            pdsch = next;
            self.links[0].quality.next_pdsch = Some(next);
        }
        Ok(step)
    }

    pub fn poll_classified_feedback(&mut self, policy: &FeedbackPolicy) -> Option<FeedbackStep> {
        let mut rejection = None;
        let step = self.poll_feedback(|observation| {
            let (feedback, reason) = policy.classify_ack_with_reason(observation);
            rejection = reason;
            feedback
        })?;
        if step.admission.is_some_and(|result| result.is_ok())
            && let Some(reason) = rejection
        {
            self.links[0]
                .feedback_snapshot
                .rejections
                .record(reason, step.report);
        }
        if let Some(accepted) = self.links[0].quality.observe(&step.report, policy) {
            let counter =
                &mut self.links[0].feedback_snapshot.cqi_reports[usize::from(accepted.value())];
            *counter = counter.saturating_add(1);
        }
        if let Request::CqiCells { start, .. } = step.report.request {
            let observed = ChipInstant {
                index: start.index.checked_add(umts_types::time::CHIPS_PER_SLOT)?,
                ..start
            };
            if let Ok(Measurement::CqiCells(Evidence::Measured(observation))) = step.report.result {
                let values = policy.classify_cqi_cells(&observation);
                for link in &mut self.links {
                    if link.quality.last_report.is_some_and(|previous| {
                        previous.epoch != observed.epoch || previous.index >= observed.index
                    }) {
                        continue;
                    }
                    link.quality.last_report = Some(observed);
                    link.feedback_snapshot.record(step.report);
                    if let Some(value) = values[link.carrier.index()] {
                        link.quality.latest = Some((observed, value));
                        link.unusable_cqi_reports = if value.value() == 0 {
                            link.unusable_cqi_reports.saturating_add(1)
                        } else {
                            0
                        };
                        let counter =
                            &mut link.feedback_snapshot.cqi_reports[usize::from(value.value())];
                        *counter = counter.saturating_add(1);
                    }
                }
            }
        }
        Some(step)
    }

    // Age is measured from the requested CQI symbol window, not a fabricated RF observation time.
    pub fn latest_cqi_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
        now: ChipInstant,
        maximum_symbol_age: NonZeroU64,
    ) -> Option<Cqi> {
        if !self.has_carrier(carrier) {
            return None;
        }

        if self.phase() != Phase::Active {
            return None;
        }
        self.links[carrier.index()]
            .quality
            .value(now, maximum_symbol_age)
            .map(|reported| {
                if self
                    .procedure
                    .construction()
                    .parameters()
                    .harq
                    .category()
                    .limits()
                    .qam64
                {
                    self.links[carrier.index()]
                        .adaptation
                        .fine_position(reported)
                        .0
                } else {
                    self.links[carrier.index()].adaptation.adjust(reported)
                }
            })
    }

    pub(super) fn excess_rate_steps_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
        now: ChipInstant,
        maximum_symbol_age: NonZeroU64,
    ) -> u8 {
        if !self.has_carrier(carrier) {
            return 0;
        }

        if self.phase() != Phase::Active {
            return 0;
        }
        self.links[carrier.index()]
            .quality
            .value(now, maximum_symbol_age)
            .map_or(0, |reported| {
                if self
                    .procedure
                    .construction()
                    .parameters()
                    .harq
                    .category()
                    .limits()
                    .qam64
                {
                    self.links[carrier.index()]
                        .adaptation
                        .fine_position(reported)
                        .1
                } else {
                    self.links[carrier.index()]
                        .adaptation
                        .excess_steps(reported)
                }
            })
    }
}
