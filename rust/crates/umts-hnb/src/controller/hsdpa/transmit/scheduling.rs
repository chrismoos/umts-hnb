// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::common_tx::CommonTxTarget;
use crate::hsdpa_session::{SchedulingOutcome, SchedulingTime};

#[derive(Default, Debug)]
pub struct HsdpaSchedulingReport {
    pub visited: usize,
    pub prepared: usize,
    pub deferred: usize,
    pub waveform_backpressure: usize,
    pub selection_budget_exhausted: usize,
    pub failures: usize,
    pub first_failure: Option<(ReceiveIdentity, String)>,
}

impl Transmit {
    pub(super) fn schedule_session(
        &mut self,
        session: &mut Session,
        time: SchedulingTime,
        mut cursor: impl FnMut() -> ChipInstant,
        mut control: Option<&mut ControlPort<'_>>,
        report: &mut HsdpaSchedulingReport,
    ) -> Result<(), ControllerError> {
        if self.publication_report.failure.is_some() {
            return Ok(());
        }
        let count = self.carriers.len();
        let first = session.next_scheduling_carrier(count);
        for offset in 0..count {
            let index = (first + offset) % count;
            let carrier = &mut self.carriers[index];
            if !session.scheduler_ready_on(carrier.carrier) {
                continue;
            }
            let next = cursor();
            if next.epoch != time.now.epoch {
                return Err(ControllerError::Configuration);
            }
            // A preceding carrier may have consumed the original scheduling lead.
            // Preserve the guard while moving the next HS-SCCH past the fresh cursor.
            let not_before = ChipInstant {
                index: time.not_before.index.max(
                    next.index
                        .checked_add(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                        .ok_or(ControllerError::Configuration)?,
                ),
                ..time.not_before
            };
            let Some(waveform) = self.buffers.pop() else {
                report.waveform_backpressure += 1;
                break;
            };
            match session.schedule_packet_on(
                carrier.carrier,
                &mut carrier.calendar,
                &mut carrier.preparer,
                SchedulingTime { not_before, ..time },
                waveform,
                &mut cursor,
            ) {
                SchedulingOutcome::Prepared => {
                    report.prepared += 1;
                    if let Some(control) = control.as_deref_mut() {
                        // Waiting for another carrier's encoding can let TX overtake
                        // this carrier's HS-SCCH start and cancel an otherwise ready TTI.
                        self.publish_prepared_on(session, index, control, time.now, cursor());
                        if self.publication_report.failure.is_some() {
                            break;
                        }
                    }
                }
                SchedulingOutcome::Deferred {
                    waveform,
                    budget_exhausted,
                } => {
                    self.buffers.push(waveform);
                    report.deferred += 1;
                    report.selection_budget_exhausted += usize::from(budget_exhausted);
                }
                SchedulingOutcome::Failed { waveform, error } => {
                    self.buffers.push(waveform);
                    report.failures += 1;
                    if report.first_failure.is_none() {
                        report.first_failure = Some((
                            session.identity(),
                            format!("HSDPA packet preparation: {error:?}"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl<T: Transport> CommonController<T> {
    pub fn schedule_hsdpa_some(
        &mut self,
        now: ChipInstant,
        control_not_before: ChipInstant,
    ) -> Result<HsdpaSchedulingReport, ControllerError> {
        if now.epoch != self.epoch()
            || control_not_before.epoch != now.epoch
            || control_not_before.index < now.index
        {
            return Err(ControllerError::Configuration);
        }
        if let Err(error) = self.service_hsdpa_orders(now, control_not_before) {
            tracing::warn!("HS-SCCH order service: {error}");
        }
        let not_before = ChipInstant {
            index: control_not_before
                .index
                .checked_add(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                .ok_or(ControllerError::Configuration)?,
            ..control_not_before
        };
        let (Some(sessions), Some(receive)) = (&mut self.hsdpa, &mut self.dedicated) else {
            return Err(ControllerError::Configuration);
        };
        let transmit = sessions
            .transmit
            .as_mut()
            .ok_or(ControllerError::Configuration)?;
        if transmit.retained.is_some() {
            return Err(ControllerError::Configuration);
        }
        let mut report = HsdpaSchedulingReport::default();
        for _ in 0..sessions.budget.get().min(sessions.entries.len()) {
            let index = transmit.scheduling_cursor;
            transmit.scheduling_cursor = (index + 1) % sessions.entries.len();
            let Some(session) = sessions.entries[index].as_deref_mut() else {
                continue;
            };
            report.visited += 1;
            let identity = session.identity();
            if self
                .admission
                .as_ref()
                .and_then(|admission| admission.committed_identity_at(index))
                .is_some_and(|next| next.ue == identity.ue && next != identity)
            {
                report.deferred += 1;
                continue;
            }
            let feedback = receive
                .with_active_mapping(identity, |bank| session.feedback_timing(bank))
                .map_err(|error| format!("HSDPA active mapping: {error:?}"))
                .and_then(|result| {
                    result.map_err(|error| format!("HSDPA feedback coordinates: {error:?}"))
                });
            let feedback = match feedback {
                Ok(timing) => timing,
                Err(reason) => {
                    // A parked UE has already released its dedicated receiver.
                    if self.radio.parked(identity.ue).is_some() {
                        report.deferred += 1;
                        continue;
                    }
                    report.failures += 1;
                    if report.first_failure.is_none() {
                        report.first_failure = Some((identity, reason));
                    }
                    continue;
                }
            };
            let mut control = self
                .admission
                .as_mut()
                .and_then(|owner| owner.hsdpa_control());
            transmit.schedule_session(
                session,
                SchedulingTime {
                    now,
                    not_before,
                    feedback,
                },
                || self.port.next_chip(),
                control.as_mut(),
                &mut report,
            )?;
            if transmit.publication_report.failure.is_some() {
                break;
            }
        }
        Ok(report)
    }
}
