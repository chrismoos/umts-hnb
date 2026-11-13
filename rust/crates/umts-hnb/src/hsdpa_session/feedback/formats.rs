// SPDX-License-Identifier: AGPL-3.0-or-later

use super::FeedbackCounts;
use crate::hsdpa_packet::PlanFormat;
use std::fmt;
use std::time::{Duration, Instant};
use umts_dsp::hsdpa::Cqi;
use umts_mac::hsdpa::{Feedback, PackedPdu, ScheduledTransmission};
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};

const FORMAT_LIMIT: usize = 64;
const PROCESS_LIMIT: usize = 8;
const SUBFRAMES_PER_FRAME: usize = 5;
const CQI_FORMAT_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug)]
struct CqiFormatOutcome {
    reported: Cqi,
    transport_bits: u64,
    codes: u8,
    outcomes: FeedbackCounts,
}

#[derive(Clone, Copy, Debug)]
struct CqiOutcomes {
    rows: [Option<CqiFormatOutcome>; CQI_FORMAT_LIMIT],
    omitted: u64,
}

impl Default for CqiOutcomes {
    fn default() -> Self {
        Self {
            rows: [None; CQI_FORMAT_LIMIT],
            omitted: 0,
        }
    }
}

impl CqiOutcomes {
    fn record(&mut self, association: Association, classification: Option<Feedback>) {
        let Some(reported) = association.reported_cqi else {
            return;
        };
        if association.retry || !association.qam64 {
            return;
        }
        let index = self
            .rows
            .iter()
            .position(|row| {
                row.is_some_and(|row| {
                    row.reported == reported
                        && row.transport_bits == association.transport_bits
                        && row.codes == association.codes
                })
            })
            .or_else(|| self.rows.iter().position(Option::is_none));
        let Some(index) = index else {
            self.omitted = self.omitted.saturating_add(1);
            return;
        };
        self.rows[index]
            .get_or_insert(CqiFormatOutcome {
                reported,
                transport_bits: association.transport_bits,
                codes: association.codes,
                outcomes: FeedbackCounts::default(),
            })
            .outcomes
            .record(true, classification);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AirtimeSummary {
    pub new_blocks: u64,
    pub new_bits: u64,
    pub retry_blocks: u64,
    pub retry_bits: u64,
    pub acknowledged_bits: u64,
    pub qam64_new_blocks: u64,
    pub qam64_acknowledged_bits: u64,
    pub maximum_new_transport_bits: u64,
    // Chips from the scheduling decision to the packet's downlink start, summed.
    pub lead_chips: u64,
    pub lead_count: u64,
    // Chips between consecutive downlink starts on the same HARQ process, summed.
    pub reuse_chips: u64,
    pub reuse_count: u64,
}

#[derive(Clone, Copy, Debug)]
struct Row {
    format: PlanFormat,
    prepared_new: u64,
    prepared_retry: u64,
    new_outcomes: FeedbackCounts,
    retry_outcomes: FeedbackCounts,
    new_by_phase: [FeedbackCounts; SUBFRAMES_PER_FRAME],
}

#[derive(Clone, Copy)]
struct Association {
    transmission: ScheduledTransmission,
    row: Option<usize>,
    transport_bits: u64,
    qam64: bool,
    codes: u8,
    reported_cqi: Option<Cqi>,
    retry: bool,
    packet: Option<PackedPdu>,
    prepared_at: Option<Instant>,
    preparation_radio_time: Option<ChipInstant>,
    receiver_processed_until: Option<ChipInstant>,
    turnaround: Option<ProcessTurnaround>,
}

#[derive(Clone, Copy)]
struct ProcessTurnaround {
    previous: ScheduledTransmission,
    previous_feedback: Option<Feedback>,
    feedback_to_ready: Option<Duration>,
    ready_to_prepared: Option<Duration>,
    ready_to_selected: Option<Duration>,
    selected_to_prepared: Option<Duration>,
    feedback_to_prepared: Duration,
}

impl fmt::Debug for ProcessTurnaround {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessTurnaround")
            .field("previous", &self.previous)
            .field("previous_feedback", &self.previous_feedback)
            .field("feedback_to_ready", &self.feedback_to_ready)
            .field("ready_to_prepared", &self.ready_to_prepared)
            .field("ready_to_selected", &self.ready_to_selected)
            .field("selected_to_prepared", &self.selected_to_prepared)
            .field("feedback_to_prepared", &self.feedback_to_prepared)
            .finish()
    }
}

#[derive(Clone, Copy)]
struct ProcessObservation {
    transmission: ScheduledTransmission,
    classification: Option<Feedback>,
    admitted: Instant,
    ready: Option<Instant>,
    selected: Option<Instant>,
}

impl ProcessObservation {
    fn turnaround(self, now: Instant) -> ProcessTurnaround {
        ProcessTurnaround {
            previous: self.transmission,
            previous_feedback: self.classification,
            feedback_to_ready: self.ready.map(|ready| ready.duration_since(self.admitted)),
            ready_to_prepared: self.ready.map(|ready| now.duration_since(ready)),
            ready_to_selected: self
                .ready
                .zip(self.selected)
                .map(|(ready, selected)| selected.duration_since(ready)),
            selected_to_prepared: self.selected.map(|selected| now.duration_since(selected)),
            feedback_to_prepared: now.duration_since(self.admitted),
        }
    }
}

const PACKET_HISTORY_LIMIT: usize = 64;

#[derive(Clone, Copy)]
struct PacketOutcome {
    transmission: ScheduledTransmission,
    packet: PackedPdu,
    retry: bool,
    classification: Option<Feedback>,
    prepared_to_feedback: Option<Duration>,
    preparation_radio_time: Option<ChipInstant>,
    receiver_processed_until: Option<ChipInstant>,
    turnaround: Option<ProcessTurnaround>,
}

impl fmt::Debug for PacketOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketOutcome")
            .field("transmission", &self.transmission)
            .field("packet", &self.packet)
            .field("retry", &self.retry)
            .field("classification", &self.classification)
            .field("prepared_to_feedback", &self.prepared_to_feedback)
            .field("preparation_radio_time", &self.preparation_radio_time)
            .field("receiver_processed_until", &self.receiver_processed_until)
            .field("turnaround", &self.turnaround)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
struct PacketHistory {
    entries: [Option<PacketOutcome>; PACKET_HISTORY_LIMIT],
    recorded: u64,
}

#[derive(Clone, Copy)]
pub struct FormatStatistics {
    totals: AirtimeSummary,
    rows: [Option<Row>; FORMAT_LIMIT],
    processes: [Option<Association>; PROCESS_LIMIT],
    capacity_omissions: u64,
    unmatched_reports: u64,
    packets: Option<PacketHistory>,
    process_observations: [Option<ProcessObservation>; PROCESS_LIMIT],
    last_downlink: [Option<ChipInstant>; PROCESS_LIMIT],
    cqi_outcomes: Option<CqiOutcomes>,
}

impl Default for FormatStatistics {
    fn default() -> Self {
        Self {
            totals: AirtimeSummary::default(),
            rows: [None; FORMAT_LIMIT],
            processes: [None; PROCESS_LIMIT],
            capacity_omissions: 0,
            unmatched_reports: 0,
            packets: None,
            process_observations: [None; PROCESS_LIMIT],
            last_downlink: [None; PROCESS_LIMIT],
            cqi_outcomes: None,
        }
    }
}

impl fmt::Debug for FormatStatistics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Rows<'a>(&'a [Option<Row>]);
        impl fmt::Debug for Rows<'_> {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_list()
                    .entries(self.0.iter().flatten())
                    .finish()
            }
        }
        formatter
            .debug_struct("FormatStatistics")
            .field("rows", &Rows(&self.rows))
            .field("capacity_omissions", &self.capacity_omissions)
            .field("unmatched_reports", &self.unmatched_reports)
            .field("packet_history", &self.packets)
            .finish()
    }
}

impl FormatStatistics {
    pub(in crate::hsdpa_session) fn selection_cqi(
        &mut self,
        transmission: ScheduledTransmission,
        reported: Option<Cqi>,
    ) {
        // Calibration requires the transmission tag even when optional statistics are disabled.
        if tracing::enabled!(target: "umts_hnb::hsdpa_feedback", tracing::Level::DEBUG) {
            self.cqi_outcomes.get_or_insert_with(CqiOutcomes::default);
        }
        if let Some(Some(association)) = self
            .processes
            .get_mut(usize::from(transmission.key.process().value()))
            && association.transmission == transmission
        {
            association.reported_cqi = reported;
        }
    }

    pub(in crate::hsdpa_session) fn first_reported_cqi(
        &self,
        transmission: ScheduledTransmission,
    ) -> Option<Cqi> {
        self.processes
            .get(usize::from(transmission.key.process().value()))
            .and_then(Option::as_ref)
            .filter(|association| association.transmission == transmission && !association.retry)
            .and_then(|association| association.reported_cqi)
    }

    pub fn airtime_summary(&self) -> AirtimeSummary {
        self.totals
    }

    pub(in crate::hsdpa_session) fn preparation_time(
        &mut self,
        transmission: ScheduledTransmission,
        now: ChipInstant,
    ) {
        if now.epoch != transmission.downlink.epoch {
            return;
        }
        self.record_cycle(transmission, now);
        if self.packets.is_none() {
            return;
        }
        if let Some(Some(association)) = self
            .processes
            .get_mut(usize::from(transmission.key.process().value()))
            && association.transmission == transmission
        {
            association.preparation_radio_time = Some(now);
        }
    }

    pub(in crate::hsdpa_session) fn receiver_progress(
        &mut self,
        transmission: ScheduledTransmission,
        processed_until: Option<ChipInstant>,
    ) {
        if self.packets.is_none() {
            return;
        }
        if let Some(Some(association)) = self
            .processes
            .get_mut(usize::from(transmission.key.process().value()))
            && association.transmission == transmission
        {
            association.receiver_processed_until =
                processed_until.filter(|time| time.epoch == transmission.downlink.epoch);
        }
    }

    fn record_cycle(&mut self, transmission: ScheduledTransmission, now: ChipInstant) {
        let downlink = transmission.downlink;
        self.totals.lead_chips = self
            .totals
            .lead_chips
            .saturating_add(downlink.index.saturating_sub(now.index));
        self.totals.lead_count = self.totals.lead_count.saturating_add(1);
        let Some(last) = self
            .last_downlink
            .get_mut(usize::from(transmission.key.process().value()))
        else {
            return;
        };
        if let Some(previous) = last.replace(downlink)
            && previous.epoch == downlink.epoch
        {
            self.totals.reuse_chips = self
                .totals
                .reuse_chips
                .saturating_add(downlink.index.saturating_sub(previous.index));
            self.totals.reuse_count = self.totals.reuse_count.saturating_add(1);
        }
    }

    pub(in crate::hsdpa_session) fn process_ready(&mut self, process: umts_mac::hsdpa::ProcessId) {
        if let Some(observation) = &mut self.process_observations[usize::from(process.value())] {
            observation.ready.get_or_insert_with(Instant::now);
        }
    }

    pub(in crate::hsdpa_session) fn process_selected(
        &mut self,
        process: umts_mac::hsdpa::ProcessId,
    ) {
        if let Some(observation) = &mut self.process_observations[usize::from(process.value())] {
            observation.selected = Some(Instant::now());
        }
    }

    pub(in crate::hsdpa_session) fn associate_packet(
        &mut self,
        transmission: ScheduledTransmission,
        packet: PackedPdu,
    ) {
        if self.packets.is_none() {
            return;
        }
        if let Some(Some(association)) = self
            .processes
            .get_mut(usize::from(transmission.key.process().value()))
            && association.transmission == transmission
        {
            association.packet = Some(packet);
        }
    }
    pub(super) fn prepared(
        &mut self,
        transmission: ScheduledTransmission,
        format: PlanFormat,
        retry: bool,
    ) {
        let process = usize::from(transmission.key.process().value());
        let prepared_at = self.packets.as_ref().map(|_| Instant::now());
        let previous = self
            .process_observations
            .get_mut(process)
            .and_then(Option::take);
        let turnaround = previous
            .zip(prepared_at)
            .map(|(previous, now)| previous.turnaround(now));
        let Some(slot) = self.processes.get_mut(process) else {
            self.capacity_omissions = self.capacity_omissions.saturating_add(1);
            return;
        };
        *slot = None;
        let bits = format.transport_bits.get() as u64;
        let qam64 = format.modulation == umts_dsp::hsdpa::Modulation::Qam64;
        if retry {
            self.totals.retry_blocks = self.totals.retry_blocks.saturating_add(1);
            self.totals.retry_bits = self.totals.retry_bits.saturating_add(bits);
        } else {
            self.totals.new_blocks = self.totals.new_blocks.saturating_add(1);
            self.totals.new_bits = self.totals.new_bits.saturating_add(bits);
            self.totals.maximum_new_transport_bits =
                self.totals.maximum_new_transport_bits.max(bits);
            if qam64 {
                self.totals.qam64_new_blocks = self.totals.qam64_new_blocks.saturating_add(1);
            }
        }
        let row = self
            .rows
            .iter()
            .position(|row| row.is_some_and(|row| row.format == format))
            .or_else(|| self.rows.iter().position(Option::is_none));
        if let Some(index) = row {
            let row = self.rows[index].get_or_insert(Row {
                format,
                prepared_new: 0,
                prepared_retry: 0,
                new_outcomes: FeedbackCounts::default(),
                retry_outcomes: FeedbackCounts::default(),
                new_by_phase: [FeedbackCounts::default(); SUBFRAMES_PER_FRAME],
            });
            let count = if retry {
                &mut row.prepared_retry
            } else {
                &mut row.prepared_new
            };
            *count = count.saturating_add(1);
        } else {
            self.capacity_omissions = self.capacity_omissions.saturating_add(1);
        }
        *slot = Some(Association {
            transmission,
            row,
            transport_bits: bits,
            qam64,
            codes: format.codes,
            reported_cqi: None,
            retry,
            packet: None,
            prepared_at,
            preparation_radio_time: None,
            receiver_processed_until: None,
            turnaround,
        });
    }

    pub(super) fn admitted(
        &mut self,
        transmission: ScheduledTransmission,
        classification: Option<Feedback>,
    ) {
        let slot = self
            .processes
            .get_mut(usize::from(transmission.key.process().value()));
        let Some(slot) =
            slot.filter(|slot| slot.is_some_and(|value| value.transmission == transmission))
        else {
            self.unmatched_reports = self.unmatched_reports.saturating_add(1);
            return;
        };
        let association = slot.take().expect("matched transmission association");
        if let Some(outcomes) = &mut self.cqi_outcomes {
            outcomes.record(association, classification);
        }
        if matches!(classification, Some(Feedback::Ack)) {
            self.totals.acknowledged_bits = self
                .totals
                .acknowledged_bits
                .saturating_add(association.transport_bits);
            if association.qam64 {
                self.totals.qam64_acknowledged_bits = self
                    .totals
                    .qam64_acknowledged_bits
                    .saturating_add(association.transport_bits);
            }
        }
        if let (Some(history), Some(packet)) = (&mut self.packets, association.packet) {
            let now = Instant::now();
            self.process_observations[usize::from(transmission.key.process().value())] =
                Some(ProcessObservation {
                    transmission,
                    classification,
                    admitted: now,
                    ready: None,
                    selected: None,
                });
            let index = (history.recorded % PACKET_HISTORY_LIMIT as u64) as usize;
            history.entries[index] = Some(PacketOutcome {
                transmission,
                packet,
                retry: association.retry,
                classification,
                prepared_to_feedback: association
                    .prepared_at
                    .map(|start| now.duration_since(start)),
                preparation_radio_time: association.preparation_radio_time,
                receiver_processed_until: association.receiver_processed_until,
                turnaround: association.turnaround,
            });
            history.recorded = history.recorded.saturating_add(1);
        }
        let Some(index) = association.row else {
            return;
        };
        let row = self.rows[index].as_mut().expect("admitted format row");
        let counts = if association.retry {
            &mut row.retry_outcomes
        } else {
            &mut row.new_outcomes
        };
        counts.record(true, classification);
        if !association.retry {
            let phase = (transmission.downlink.index % CHIPS_PER_FRAME
                / (CHIPS_PER_FRAME / SUBFRAMES_PER_FRAME as u64)) as usize;
            row.new_by_phase[phase].record(true, classification);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count};
    use std::num::{NonZeroU8, NonZeroU64};
    use umts_dsp::hsdpa::{Modulation, TransmissionVersion};
    use umts_mac::hsdpa::{HarqEntity, ProcessId};
    use umts_types::{UeKey, hsdpa::Category};

    fn fixture() -> (ScheduledTransmission, PlanFormat) {
        let mut harq = HarqEntity::new(
            UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            NonZeroU64::MIN,
            Category::new(10).unwrap(),
            at(0),
            &[count(28_800); 6],
            NonZeroU8::MIN,
        )
        .unwrap();
        let process = ProcessId::new(0).unwrap();
        harq.insert(process, ()).unwrap();
        let transmission = harq.schedule(process, at(0), at(30_000)).unwrap();
        let format = PlanFormat {
            transport_bits: count(17_237),
            soft_bits: count(28_800),
            first_code: 4,
            codes: 12,
            modulation: Modulation::Qam16,
            size_index: 47,
            version: TransmissionVersion::new(0).unwrap(),
            table: umts_dsp::hsdpa::TransportTable::BitAligned,
            control_list_number: 1,
        };
        (transmission, format)
    }

    #[test]
    fn first_transmission_phases_keep_exact_outcomes_separate_from_retries() {
        let (mut transmission, format) = fixture();
        let mut stats = FormatStatistics::default();
        let outcomes = [
            Some(Feedback::Ack),
            Some(Feedback::Nack),
            Some(Feedback::Dtx),
            None,
            Some(Feedback::Ack),
        ];
        for (phase, outcome) in outcomes.into_iter().enumerate() {
            transmission.downlink.index = 2 * umts_types::time::CHIPS_PER_SLOT
                + phase as u64 * (CHIPS_PER_FRAME / SUBFRAMES_PER_FRAME as u64);
            stats.prepared(transmission, format, false);
            stats.admitted(transmission, outcome);
            stats.admitted(transmission, Some(Feedback::Ack));
        }
        let row = stats.rows[0].unwrap();
        assert_eq!(row.new_outcomes.ack, 2);
        assert_eq!(row.new_by_phase[0].ack, 1);
        assert_eq!(row.new_by_phase[1].nack, 1);
        assert_eq!(row.new_by_phase[2].dtx, 1);
        assert_eq!(row.new_by_phase[3].missing, 1);
        assert_eq!(row.new_by_phase[4].ack, 1);
        assert_eq!(stats.unmatched_reports, SUBFRAMES_PER_FRAME as u64);
        stats.prepared(transmission, format, true);
        stats.admitted(transmission, Some(Feedback::Nack));
        let row = stats.rows[0].unwrap();
        assert_eq!(row.retry_outcomes.nack, 1);
        assert_eq!(row.new_by_phase[4].nack, 0);
    }

    #[test]
    fn calibration_tag_requires_the_exact_first_transmission_not_a_retry() {
        let (transmission, format) = fixture();
        let mut stats = FormatStatistics::default();
        let cqi = Cqi::new(29).unwrap();
        stats.prepared(transmission, format, false);
        stats.selection_cqi(transmission, Some(cqi));
        assert_eq!(stats.first_reported_cqi(transmission), Some(cqi));
        let mut stale = transmission;
        stale.expected_ack.index += 1;
        assert_eq!(stats.first_reported_cqi(stale), None);
        stats.admitted(transmission, Some(Feedback::Nack));
        assert_eq!(stats.first_reported_cqi(transmission), None);
        stats.prepared(transmission, format, true);
        stats.selection_cqi(transmission, Some(cqi));
        assert_eq!(stats.first_reported_cqi(transmission), None);
    }

    #[test]
    fn preparation_time_accumulates_lead_and_process_reuse_without_history() {
        let (mut transmission, format) = fixture();
        transmission.downlink.index += 76_800;
        transmission.expected_ack.index += 76_800;
        let mut stats = FormatStatistics::default();
        stats.prepared(transmission, format, false);
        stats.preparation_time(transmission, at(transmission.downlink.index - 5000));
        let mut other_epoch = at(transmission.downlink.index - 100);
        other_epoch.epoch = umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
        stats.preparation_time(transmission, other_epoch);
        transmission.downlink.index += 7680 * 7;
        transmission.expected_ack.index += 7680 * 7;
        stats.prepared(transmission, format, false);
        stats.preparation_time(transmission, at(transmission.downlink.index - 6000));
        let totals = stats.airtime_summary();
        assert_eq!((totals.lead_chips, totals.lead_count), (11000, 2));
        assert_eq!((totals.reuse_chips, totals.reuse_count), (7680 * 7, 1));
        assert!(stats.packets.is_none());
    }

    #[test]
    fn exact_transmission_match_separates_new_and_retry_outcomes() {
        let (transmission, format) = fixture();
        let mut stats = FormatStatistics::default();
        stats.prepared(transmission, format, false);
        let mut stale = transmission;
        stale.downlink.index += 1;
        stats.admitted(stale, Some(Feedback::Ack));
        stats.admitted(transmission, Some(Feedback::Nack));
        stats.admitted(transmission, Some(Feedback::Ack));
        stats.prepared(transmission, format, true);
        stats.admitted(transmission, Some(Feedback::Ack));
        let row = stats.rows[0].unwrap();
        assert_eq!((row.prepared_new, row.prepared_retry), (1, 1));
        assert_eq!((row.new_outcomes.ack, row.new_outcomes.nack), (0, 1));
        assert_eq!(row.retry_outcomes.ack, 1);
        assert_eq!(stats.unmatched_reports, 2);
    }

    #[test]
    fn capacity_omission_retains_exact_aggregate_association() {
        let (transmission, mut format) = fixture();
        let mut stats = FormatStatistics::default();
        for bits in 1..=FORMAT_LIMIT + 1 {
            format.transport_bits = count(bits);
            stats.prepared(transmission, format, false);
        }
        stats.admitted(transmission, Some(Feedback::Ack));
        assert_eq!(stats.capacity_omissions, 1);
        assert_eq!(stats.unmatched_reports, 0);
        assert_eq!(
            stats.airtime_summary().new_blocks,
            (FORMAT_LIMIT + 1) as u64
        );
        assert_eq!(
            stats.airtime_summary().acknowledged_bits,
            (FORMAT_LIMIT + 1) as u64
        );
        assert!(
            stats
                .rows
                .iter()
                .flatten()
                .all(|row| row.new_outcomes.ack == 0)
        );
        format.transport_bits = count(42192);
        format.modulation = Modulation::Qam64;
        stats.prepared(transmission, format, false);
        let mut stale = transmission;
        stale.downlink.index += 1;
        stats.admitted(stale, Some(Feedback::Ack));
        stats.admitted(transmission, Some(Feedback::Nack));
        stats.prepared(transmission, format, true);
        stats.admitted(transmission, Some(Feedback::Ack));
        stats.admitted(transmission, Some(Feedback::Ack));
        let totals = stats.airtime_summary();
        assert_eq!(stats.capacity_omissions, 3);
        assert_eq!(stats.unmatched_reports, 2);
        assert_eq!(totals.maximum_new_transport_bits, 42192);
        assert_eq!(totals.qam64_new_blocks, 1);
        assert_eq!(totals.retry_blocks, 1);
        assert_eq!(totals.retry_bits, 42192);
        assert_eq!(totals.qam64_acknowledged_bits, 42192);
        assert_eq!(totals.acknowledged_bits, 42192 + (FORMAT_LIMIT + 1) as u64);
    }
}
