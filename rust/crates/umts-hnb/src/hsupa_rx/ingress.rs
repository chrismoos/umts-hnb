// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    FeedbackAssociation, FeedbackError, PacketAdmission, PacketDelivery, PacketError, ReceivePort,
    Report,
};
use crate::{
    dedicated_rx::ReceiveIdentity,
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitEvent},
    hsupa_tx::{Command, CommandOutcome, Completion, ControlId},
};
use std::num::NonZeroUsize;
use umts_types::time::ChipInstant;

const MAX_FEEDBACK: usize = 64;
const RSN_STATES: usize = 4;

mod history;
use history::FeedbackHistory;
pub use history::{AckDisposition, FeedbackHistoryCounters};

pub struct IngressInputs {
    pub receive: ReceivePort,
    pub packets: Box<PacketDelivery>,
    pub association: FeedbackAssociation,
}

pub struct IngressSettings {
    pub identity: ReceiveIdentity,
    pub activation: ChipInstant,
    pub feedback_amplitude: f32,
    pub feedback_capacity: NonZeroUsize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressError {
    Configuration,
    Identity,
    Time,
    Feedback(FeedbackError),
    Packet(PacketError),
    Closed,
}

pub struct RejectedIngress {
    pub error: IngressError,
    #[expect(
        dead_code,
        reason = "Owns the rejected operation until it is retried or dropped."
    )]
    pub inputs: IngressInputs,
}

#[derive(Clone, Copy, Debug, Default)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct IngressCounters {
    pub scheduling_reports: u64,
    pub last_scheduling: Option<SchedulingReport>,
    pub last_non_scheduling: Option<ChipInstant>,
    pub last_happy: Option<(ChipInstant, bool)>,
    pub unhappy_streak: u32,
    pub receiver_progress: super::ReceiveProgress,
    pub report_queue_samples: u64,
    pub report_queue_total_ns: u128,
    pub report_queue_maximum_ns: u128,
    pub feedback_history: FeedbackHistoryCounters,
    pub last_late_feedback: Option<LateFeedback>,
    pub last_decoded: Option<Report>,
    pub last_failed: Option<Report>,
    pub decoded_by_rsn: [u64; RSN_STATES],
    pub combined_by_rsn: [u64; RSN_STATES],
    pub reset_by_rsn: [u64; RSN_STATES],
    /// Reset reasons indexed by `umts_mac::hsupa::ResetReason::index`.
    pub reset_reasons: [u64; umts_mac::hsupa::ResetReason::COUNT],
    pub failed_by_rsn: [u64; RSN_STATES],
    pub decoded: u64,
    pub invalid_crc: u64,
    pub erased: u64,
    pub absent: u64,
    pub invalid_filler: u64,
    pub decoded_payload_bits: u64,
    pub stored_groups: u64,
    pub duplicate_groups: u64,
    pub old_groups: u64,
    pub reports_discarded_on_release: u64,
    pub reports: u64,
    pub feedback_published: u64,
    pub feedback_admitted: u64,
    pub feedback_rejected: u64,
    pub feedback_late: u64,
    pub feedback_submitted: u64,
    pub feedback_lost: u64,
    pub malformed_packets: u64,
    pub backpressure: u64,
}

/// Uplink activity the serving grant adaptation reads once per service pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct GrantActivity {
    pub data: Option<ChipInstant>,
    pub scheduling: Option<SchedulingReport>,
    pub happy: Option<(ChipInstant, bool)>,
    pub unhappy_streak: u32,
}

impl IngressCounters {
    fn observe_happy(&mut self, at: ChipInstant, happy: bool, interval: u64) {
        let consecutive = self.last_happy.is_some_and(|(previous, was_happy)| {
            !was_happy
                && previous.epoch == at.epoch
                && at.index.checked_sub(previous.index) == Some(interval)
        });
        self.unhappy_streak = if happy {
            0
        } else if consecutive {
            self.unhappy_streak.saturating_add(1)
        } else {
            1
        };
        self.last_happy = Some((at, happy));
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SchedulingReport {
    pub start: ChipInstant,
    pub information: umts_mac::hsupa::scheduling::SchedulingInformation,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct LateFeedback {
    pub decode_timing: Option<super::DecodeTiming>,
    pub uplink_start: ChipInstant,
    pub observed_at: ChipInstant,
    pub expected: ChipInstant,
    pub next_transmit: ChipInstant,
    pub report_queue_age: Option<std::time::Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressProgress {
    Idle,
    Backpressure,
    Admitted,
    Malformed,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct IngressStep {
    pub progress: IngressProgress,
    pub report: Option<Report>,
    pub packet: Option<PacketAdmission>,
}

#[derive(Clone, Copy)]
enum FeedbackState {
    Command(TransmitCommandId),
    Air(ControlId),
}
struct Flight {
    tti: u64,
    state: FeedbackState,
}

pub struct Ingress {
    inputs: IngressInputs,
    settings: IngressSettings,
    flights: Box<[Option<Flight>]>,
    feedback_handled: Option<u64>,
    last_released: Option<u64>,
    now: Option<ChipInstant>,
    counters: IngressCounters,
    history: FeedbackHistory,
}

impl Ingress {
    pub(crate) fn reschedule_before_service(
        &mut self,
        activation: ChipInstant,
    ) -> Result<(), IngressError> {
        if activation.epoch != self.settings.identity.ue.epoch {
            return Err(IngressError::Identity);
        }
        if self.now.is_some()
            || self.last_released.is_some()
            || self.feedback_handled.is_some()
            || self.flights.iter().any(Option::is_some)
            || self.inputs.receive.peek().is_some()
        {
            return Err(IngressError::Configuration);
        }
        self.settings.activation = activation;
        Ok(())
    }

    // The serving owner must commit the matching protected activation before servicing reports.
    pub fn new(
        settings: IngressSettings,
        inputs: IngressInputs,
    ) -> Result<Self, Box<RejectedIngress>> {
        let error = if settings.identity != inputs.association.identity()
            || settings.identity != inputs.packets.identity()
            || settings.activation.epoch != settings.identity.ue.epoch
        {
            Some(IngressError::Identity)
        } else if settings.feedback_capacity.get() > MAX_FEEDBACK
            || !settings.feedback_amplitude.is_finite()
            || !(0.0..=1.0).contains(&settings.feedback_amplitude)
        {
            Some(IngressError::Configuration)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(Box::new(RejectedIngress { error, inputs }));
        }
        Ok(Self {
            history: FeedbackHistory::new(inputs.association.interval()),
            flights: (0..settings.feedback_capacity.get())
                .map(|_| None)
                .collect(),
            inputs,
            settings,
            feedback_handled: None,
            last_released: None,
            now: None,
            counters: IngressCounters::default(),
        })
    }

    #[cfg(test)]
    pub fn identity(&self) -> ReceiveIdentity {
        self.settings.identity
    }
    #[cfg(test)]
    pub fn activation(&self) -> ChipInstant {
        self.settings.activation
    }
    #[cfg(test)]
    pub fn interval(&self) -> umts_types::hsupa::Tti {
        self.inputs.association.interval()
    }
    pub fn counters(&self) -> IngressCounters {
        IngressCounters {
            receiver_progress: self.inputs.receive.progress(),
            feedback_history: self.history.counters(),
            ..self.counters
        }
    }
    pub(crate) fn grant_activity(&self) -> GrantActivity {
        GrantActivity {
            data: self.counters.last_non_scheduling,
            scheduling: self.counters.last_scheduling,
            happy: self.counters.last_happy,
            unhappy_streak: self.counters.unhappy_streak,
        }
    }
    pub fn packets(&mut self) -> &mut PacketDelivery {
        &mut self.inputs.packets
    }
    pub fn outstanding_feedback(&self) -> usize {
        self.flights.iter().flatten().count()
    }

    pub(crate) fn drained(&mut self) -> bool {
        self.inputs.receive.peek().is_none()
            && self.inputs.packets.is_empty()
            && self.outstanding_feedback() == 0
    }

    // Keep the returned RX owner alive until its output buffers have been recycled.
    pub(crate) fn discard_one_for_release(&mut self) -> bool {
        if self.inputs.receive.peek().is_some() {
            self.inputs.receive.release();
            self.counters.reports_discarded_on_release =
                self.counters.reports_discarded_on_release.saturating_add(1);
            false
        } else {
            true
        }
    }

    pub fn service_one(
        &mut self,
        now: ChipInstant,
        next_transmit: ChipInstant,
        transmit: &mut impl crate::hsupa_tx::Control,
    ) -> Result<IngressStep, IngressError> {
        if now.epoch != self.settings.identity.ue.epoch
            || next_transmit.epoch != now.epoch
            || transmit.epoch() != now.epoch
            || self.now.is_some_and(|old| old.index > now.index)
        {
            return Err(IngressError::Time);
        }
        self.now = Some(now);
        let Some((input, report_queue_age)) = self.inputs.receive.peek_with_age() else {
            return if !self.inputs.receive.closed_and_empty() {
                Ok(IngressStep {
                    progress: IngressProgress::Idle,
                    report: None,
                    packet: None,
                })
            } else {
                Err(IngressError::Closed)
            };
        };
        let report = input.report;
        if report.identity != self.settings.identity {
            return Err(IngressError::Identity);
        }
        if report.start.epoch != now.epoch
            || report.start.index < self.settings.activation.index
            || report
                .start
                .index
                .checked_add(self.inputs.association.interval().chips())
                .is_none_or(|end| end > now.index)
            || self
                .last_released
                .is_some_and(|last| report.absolute_tti <= last)
        {
            return Err(IngressError::Time);
        }
        let step = |progress, packet| IngressStep {
            progress,
            report: Some(report),
            packet,
        };
        if self.feedback_handled != Some(report.absolute_tti) {
            let mut disposition = AckDisposition::Queued;
            match self.inputs.association.request(report, next_transmit) {
                Err(FeedbackError::Late {
                    expected,
                    next_transmit,
                }) => {
                    disposition = AckDisposition::Late;
                    self.counters.feedback_late = self.counters.feedback_late.saturating_add(1);
                    self.counters.last_late_feedback = Some(LateFeedback {
                        decode_timing: report.timing,
                        uplink_start: report.start,
                        observed_at: now,
                        expected,
                        next_transmit,
                        report_queue_age: Some(report_queue_age),
                    });
                }
                Err(error) => return Err(IngressError::Feedback(error)),
                Ok(request) => {
                    let Some(slot) = self.flights.iter_mut().find(|slot| slot.is_none()) else {
                        self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                        return Ok(step(IngressProgress::Backpressure, None));
                    };
                    match transmit.queue_hsupa(Command::Feedback {
                        request,
                        amplitude: self.settings.feedback_amplitude,
                    }) {
                        Ok(id) => {
                            *slot = Some(Flight {
                                tti: report.absolute_tti,
                                state: FeedbackState::Command(id),
                            });
                            self.counters.feedback_published =
                                self.counters.feedback_published.saturating_add(1);
                        }
                        Err(_) if !transmit.available() => return Err(IngressError::Closed),
                        Err(_) => {
                            self.counters.backpressure =
                                self.counters.backpressure.saturating_add(1);
                            return Ok(step(IngressProgress::Backpressure, None));
                        }
                    }
                }
            }
            self.history.observe(report, disposition);
            self.counters.report_queue_samples =
                self.counters.report_queue_samples.saturating_add(1);
            self.counters.report_queue_total_ns = self
                .counters
                .report_queue_total_ns
                .saturating_add(report_queue_age.as_nanos());
            self.counters.report_queue_maximum_ns = self
                .counters
                .report_queue_maximum_ns
                .max(report_queue_age.as_nanos());
            self.feedback_handled = Some(report.absolute_tti);
        }
        // A CRC ACK is independent of MAC parsing and must not be republished on a retained-report retry.
        let result = match self.inputs.packets.admit(input, now.index) {
            Ok(admission) => step(IngressProgress::Admitted, Some(admission)),
            Err(PacketError::Backpressure) => {
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                return Ok(step(IngressProgress::Backpressure, None));
            }
            Err(PacketError::Payload) => {
                self.counters.malformed_packets = self.counters.malformed_packets.saturating_add(1);
                step(IngressProgress::Malformed, None)
            }
            Err(error) => return Err(IngressError::Packet(error)),
        };
        assert!(self.inputs.receive.release(), "retained decoded report");
        use umts_dsp::transport_blocks::BlockOutcome;
        let rsn = usize::from(report.control.information.sequence.value());
        // E-TFCI zero specifies scheduling information only, including during data stalls.
        if report.control.information.format.value() != 0 {
            self.counters.last_non_scheduling = Some(report.start);
        }
        self.counters.observe_happy(
            report.start,
            report.control.information.happy,
            self.inputs.association.interval().chips(),
        );
        let continuity = if report.combined {
            &mut self.counters.combined_by_rsn[rsn]
        } else {
            &mut self.counters.reset_by_rsn[rsn]
        };
        *continuity = continuity.saturating_add(1);
        if let Some(reason) = report.reset_reason {
            let slot = &mut self.counters.reset_reasons[reason.index()];
            *slot = slot.saturating_add(1);
        }
        if report.outcome == BlockOutcome::Decoded {
            self.counters.last_decoded = Some(report);
            self.counters.decoded_by_rsn[rsn] = self.counters.decoded_by_rsn[rsn].saturating_add(1);
        } else {
            self.counters.last_failed = Some(report);
            self.counters.failed_by_rsn[rsn] = self.counters.failed_by_rsn[rsn].saturating_add(1);
        }
        let outcome = match report.outcome {
            BlockOutcome::Decoded => &mut self.counters.decoded,
            BlockOutcome::InvalidCrc { .. } => &mut self.counters.invalid_crc,
            BlockOutcome::Erased => &mut self.counters.erased,
            BlockOutcome::Absent => &mut self.counters.absent,
            BlockOutcome::InvalidFiller => &mut self.counters.invalid_filler,
        };
        *outcome = outcome.saturating_add(1);
        self.counters.decoded_payload_bits = self
            .counters
            .decoded_payload_bits
            .saturating_add(report.payload_bits as u64);
        if let Some(packet) = result.packet {
            if let Some(word) = packet.scheduling_information {
                self.counters.scheduling_reports =
                    self.counters.scheduling_reports.saturating_add(1);
                self.counters.last_scheduling = Some(SchedulingReport {
                    start: report.start,
                    information: umts_mac::hsupa::scheduling::SchedulingInformation::from_word(
                        word,
                    )
                    .expect("parsed eighteen-bit scheduling information"),
                });
            }
            self.counters.stored_groups = self
                .counters
                .stored_groups
                .saturating_add(packet.stored as u64);
            self.counters.duplicate_groups = self
                .counters
                .duplicate_groups
                .saturating_add(packet.duplicates as u64);
            self.counters.old_groups = self.counters.old_groups.saturating_add(packet.old as u64);
        }
        self.feedback_handled = None;
        self.last_released = Some(report.absolute_tti);
        self.counters.reports = self.counters.reports.saturating_add(1);
        Ok(result)
    }

    pub fn route_command(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        let Some(index) = self.flights.iter().position(|flight| {
            flight.as_ref().is_some_and(
                |flight| matches!(flight.state, FeedbackState::Command(id) if id == event.id),
            )
        }) else {
            return Some(event);
        };
        let TransmitCommandOutcome::Hsupa(CommandOutcome::Queued { identity, result }) =
            &event.outcome
        else {
            return Some(event);
        };
        if *identity != self.settings.identity {
            return Some(event);
        }
        match result {
            Ok(id) => {
                self.flights[index]
                    .as_mut()
                    .expect("matched feedback command")
                    .state = FeedbackState::Air(*id);
                self.counters.feedback_admitted = self.counters.feedback_admitted.saturating_add(1);
            }
            Err(_) => {
                self.history.disposition(
                    self.flights[index]
                        .as_ref()
                        .expect("matched feedback command")
                        .tti,
                    AckDisposition::Rejected,
                );
                self.flights[index] = None;
                self.counters.feedback_rejected = self.counters.feedback_rejected.saturating_add(1);
            }
        }
        None
    }

    pub fn accept_completion(&mut self, completion: Completion) -> Result<(), Completion> {
        if completion.identity != self.settings.identity {
            return Err(completion);
        }
        let Some(index) = self.flights.iter().position(|flight| {
            flight.as_ref().is_some_and(|flight| {
                completion.feedback_tti == Some(flight.tti)
                    && matches!(flight.state, FeedbackState::Air(id) if id == completion.id)
            })
        }) else {
            return Err(completion);
        };
        self.history.disposition(
            self.flights[index]
                .as_ref()
                .expect("matched feedback completion")
                .tti,
            if completion.submitted {
                AckDisposition::Submitted
            } else {
                AckDisposition::Lost
            },
        );
        self.flights[index] = None;
        if completion.submitted {
            self.counters.feedback_submitted = self.counters.feedback_submitted.saturating_add(1);
        } else {
            self.counters.feedback_lost = self.counters.feedback_lost.saturating_add(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
