// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod scheduling;
#[cfg(test)]
mod tests;
use crate::{
    dedicated_tx::{TransmitCommandOutcome, TransmitError},
    hsdpa_packet::{CommandResult, ControlPort, Operation},
    hsdpa_session::{CommandFailure, CommandProgress},
    hsdpa_tx::{AllocationCalendar, WaveformBuffer},
    radio_resources::HsdpaCodeSet,
};
pub use scheduling::HsdpaSchedulingReport;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransmitServiceError {
    Order(String),
    Identity,
    Capacity,
    Command(CommandFailure),
    Port(TransmitError),
    Allocation(crate::hsdpa_tx::Error),
    Publication(crate::hsdpa_session::PacketPublicationError),
    Cancellation(crate::hsdpa_session::PacketCancellationError),
}

#[derive(Default, Debug)]
pub struct TransmitStep {
    pub retirement_visited: usize,
    pub retirement_backpressure: usize,
    pub results: usize,
    pub recycled: usize,
    pub rejected: usize,
    pub last_rejection: Option<crate::hsdpa_tx::Error>,
    pub late_rejections: usize,
    pub late_chips_total: u64,
    pub late_chips_maximum: u64,
    pub failure: Option<TransmitServiceError>,
    pub published: usize,
    pub cancelled: usize,
    pub stale_cancelled: usize,
    pub last_stale_downlink_floor: Option<(ChipInstant, ChipInstant)>,
    pub publication_backpressure: usize,
}

pub(super) struct CarrierTransmit {
    carrier: umts_types::hsdpa::CarrierId,
    codes: HsdpaCodeSet,
    origin: ChipInstant,
    primary: umts_dsp::scrambling::PrimaryScramblingCode,
    calendar: AllocationCalendar,
    preparer: crate::hsdpa_tx::WaveformPreparer,
}

pub(super) struct Transmit {
    buffers: Vec<WaveformBuffer>,
    capacity: usize,
    retained: Option<(TransmitServiceError, CommandResult)>,
    carriers: Vec<CarrierTransmit>,
    publication_cursor: usize,
    scheduling_cursor: usize,
    retirement_cursor: usize,
    publication_report: TransmitStep,
}

impl Transmit {
    pub(super) fn prepare_order(
        &self,
        request: crate::hsdpa_tx::OrderPreparation,
        buffer: crate::hsdpa_tx::OrderBuffer,
    ) -> Result<crate::hsdpa_tx::PreparedOrder, crate::hsdpa_tx::OrderPreparationError> {
        match self
            .carriers
            .iter()
            .find(|carrier| carrier.carrier == umts_types::hsdpa::CarrierId::Primary)
        {
            Some(carrier) => carrier.preparer.prepare_order(request, buffer),
            None => Err(crate::hsdpa_tx::OrderPreparationError {
                error: crate::hsdpa_tx::Error::Unavailable,
                buffer,
            }),
        }
    }

    pub(super) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            buffers: (0..capacity.get())
                .map(|_| WaveformBuffer::default())
                .collect(),
            capacity: capacity.get(),
            retained: None,
            carriers: Vec::with_capacity(2),
            publication_cursor: 0,
            scheduling_cursor: 0,
            retirement_cursor: 0,
            publication_report: TransmitStep::default(),
        }
    }

    pub(super) fn ensure_calendar(
        &mut self,
        codes: HsdpaCodeSet,
        origin: ChipInstant,
        primary: umts_dsp::scrambling::PrimaryScramblingCode,
    ) -> Result<(), crate::hsdpa_tx::Error> {
        self.ensure_carrier_calendar(
            umts_types::hsdpa::CarrierId::Primary,
            codes,
            origin,
            primary,
        )
    }

    pub(super) fn ensure_carrier_calendar(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        codes: HsdpaCodeSet,
        origin: ChipInstant,
        primary: umts_dsp::scrambling::PrimaryScramblingCode,
    ) -> Result<(), crate::hsdpa_tx::Error> {
        if let Some(existing) = self.carriers.iter().find(|entry| entry.carrier == carrier) {
            return if existing.codes == codes
                && existing.origin == origin
                && existing.primary == primary
            {
                Ok(())
            } else {
                Err(crate::hsdpa_tx::Error::Identity)
            };
        }
        if self.carriers.len() == 2 || self.carriers.iter().any(|entry| entry.primary == primary) {
            return Err(crate::hsdpa_tx::Error::Identity);
        }
        let calendar = AllocationCalendar::new(
            codes,
            origin,
            NonZeroUsize::new(self.capacity).expect("admitted capacity"),
        )?;
        let preparer = crate::hsdpa_tx::WaveformPreparer::new(codes, primary)?;
        self.carriers.push(CarrierTransmit {
            carrier,
            codes,
            origin,
            primary,
            calendar,
            preparer,
        });
        Ok(())
    }

    fn publish_pending(
        &mut self,
        entries: &mut [Option<Box<Session>>],
        control: &mut ControlPort<'_>,
        now: ChipInstant,
        publication_floor: ChipInstant,
        budget: NonZeroUsize,
        step: &mut TransmitStep,
    ) {
        for _ in 0..budget.get().min(entries.len()) {
            let index = self.publication_cursor;
            self.publication_cursor = (index + 1) % entries.len();
            let Some(session) = entries[index].as_deref_mut() else {
                continue;
            };
            for carrier in 0..self.carriers.len() {
                self.publish_pending_on(session, carrier, control, publication_floor, step);
                if step.failure.is_some() {
                    return;
                }
            }
        }
        self.advance_calendars(now, step);
    }

    fn publish_pending_on(
        &mut self,
        session: &mut Session,
        carrier: usize,
        control: &mut ControlPort<'_>,
        publication_floor: ChipInstant,
        step: &mut TransmitStep,
    ) {
        let carrier = &mut self.carriers[carrier];
        let calendar = &mut carrier.calendar;
        let Some(pending) = session.pending_transmission_on(carrier.carrier) else {
            return;
        };
        let stale = pending.downlink.epoch != publication_floor.epoch
            || pending
                .downlink
                .index
                .checked_sub(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                .is_none_or(|start| start < publication_floor.index);
        if !session.data_publication_allowed(carrier.carrier) || stale {
            if self.buffers.len() == self.capacity {
                step.failure = Some(TransmitServiceError::Capacity);
                return;
            }
            match session.cancel_pending_packet_on(carrier.carrier, calendar) {
                Ok(Some(buffer)) => {
                    self.buffers.push(buffer);
                    step.cancelled += 1;
                    if stale {
                        step.stale_cancelled += 1;
                        step.last_stale_downlink_floor =
                            Some((pending.downlink, publication_floor));
                    }
                }
                Ok(None) => (),
                Err(error) => step.failure = Some(TransmitServiceError::Cancellation(error)),
            }
        } else {
            match session.publish_packet_on(carrier.carrier, control, calendar) {
                Ok(crate::hsdpa_session::PacketPublication::Queued { .. }) => step.published += 1,
                Ok(crate::hsdpa_session::PacketPublication::Backpressure) => {
                    step.publication_backpressure += 1
                }
                Ok(crate::hsdpa_session::PacketPublication::Idle) => (),
                Err(error) => step.failure = Some(TransmitServiceError::Publication(error)),
            }
        }
    }

    fn advance_calendars(&mut self, now: ChipInstant, step: &mut TransmitStep) {
        for carrier in &mut self.carriers {
            match carrier.calendar.advance(now) {
                Ok(()) | Err(crate::hsdpa_tx::Error::Unavailable) => (),
                Err(error) => step.failure = Some(TransmitServiceError::Allocation(error)),
            }
        }
    }

    fn publish_prepared_on(
        &mut self,
        session: &mut Session,
        carrier: usize,
        control: &mut ControlPort<'_>,
        now: ChipInstant,
        cursor: ChipInstant,
    ) {
        if self.retained.is_some() || self.publication_report.failure.is_some() {
            return;
        }
        let mut report = std::mem::take(&mut self.publication_report);
        let publication_floor = ChipInstant {
            index: cursor.index.max(now.index),
            ..cursor
        };
        self.publish_pending_on(session, carrier, control, publication_floor, &mut report);
        if report.failure.is_none() {
            // The producer cursor is a publication deadline, not elapsed radio time.
            self.advance_calendars(now, &mut report);
        }
        self.publication_report = report;
    }

    pub(super) fn service(
        &mut self,
        entries: &mut [Option<Box<Session>>],
        control: &mut ControlPort<'_>,
        budget: NonZeroUsize,
        now: ChipInstant,
    ) -> TransmitStep {
        let mut step = std::mem::take(&mut self.publication_report);
        if step.failure.is_some() {
            return step;
        }
        if let Some((error, _)) = self.retained.as_ref() {
            step.failure = Some(error.clone());
            return step;
        }
        for _ in 0..budget.get() {
            let Some(result) = control.take_result() else {
                break;
            };
            step.results += 1;
            let ue = match result.operation {
                Operation::Queue(key) => Some(key.ue()),
                Operation::BeginRetirement(ue) | Operation::FinishRetirement(ue) => Some(ue),
                Operation::Order(_) | Operation::OrderCompletion => None,
                Operation::Completion => match &result.event.outcome {
                    TransmitCommandOutcome::HsdpaCompletion(Some(completion)) => {
                        Some(completion.transmission.transmission().key.ue())
                    }
                    TransmitCommandOutcome::HsdpaCompletion(None) if result.matches_request() => {
                        continue;
                    }
                    _ => None,
                },
            };
            let session = ue.and_then(|ue| {
                entries
                    .get_mut(ue.slot as usize)?
                    .as_deref_mut()
                    .filter(|session| session.identity().ue == ue)
            });
            let Some(session) = session else {
                step.failure = Some(TransmitServiceError::Identity);
                self.retained = Some((TransmitServiceError::Identity, result));
                break;
            };
            let returns_buffer = matches!(
                &result.event.outcome,
                TransmitCommandOutcome::HsdpaCompletion(Some(_))
                    | TransmitCommandOutcome::HsdpaQueued { result: Err(_), .. }
            );
            if returns_buffer && self.buffers.len() == self.capacity {
                step.failure = Some(TransmitServiceError::Capacity);
                self.retained = Some((TransmitServiceError::Capacity, result));
                break;
            }
            let carrier = match result.operation {
                Operation::BeginRetirement(_) | Operation::FinishRetirement(_) => {
                    Some(umts_types::hsdpa::CarrierId::Primary)
                }
                _ => session.carrier_for_command(&result),
            };
            let Some(carrier) = carrier else {
                step.failure = Some(TransmitServiceError::Identity);
                self.retained = Some((TransmitServiceError::Identity, result));
                break;
            };
            match session.accept_command_on(carrier, result) {
                Ok(CommandProgress::RetirementBackpressure) => {
                    step.retirement_backpressure += 1;
                }
                Ok(CommandProgress::Recycled {
                    buffer,
                    rejection,
                    late_chips,
                }) => {
                    self.buffers.push(buffer);
                    step.recycled += 1;
                    step.rejected += usize::from(rejection.is_some());
                    if rejection.is_some() {
                        step.last_rejection = rejection;
                    }
                    if let Some(late) = late_chips {
                        step.late_rejections += 1;
                        step.late_chips_total = step.late_chips_total.saturating_add(late);
                        step.late_chips_maximum = step.late_chips_maximum.max(late);
                    }
                }
                Ok(_) => (),
                Err(rejected) => {
                    let error = TransmitServiceError::Command(rejected.error);
                    self.retained = Some((error.clone(), rejected.result));
                    step.failure = Some(error);
                    break;
                }
            }
        }
        if step.failure.is_none() && self.buffers.len() < self.capacity {
            match control.request_completion() {
                Ok(_) | Err(TransmitError::Capacity | TransmitError::Phase) => (),
                Err(error) => step.failure = Some(TransmitServiceError::Port(error)),
            }
        }
        if step.failure.is_none() {
            self.publish_pending(entries, control, now, now, budget, &mut step);
        }
        if step.failure.is_none() {
            for _ in 0..budget.get().min(entries.len()) {
                let index = self.retirement_cursor;
                self.retirement_cursor = (index + 1) % entries.len();
                let Some(session) = entries[index].as_deref_mut() else {
                    continue;
                };
                if session.phase() != Phase::Retiring || session.retirement_pending_any() {
                    continue;
                }
                step.retirement_visited += 1;
                match session.service_retirement(control) {
                    Ok(_) => (),
                    Err(TransmitError::Capacity | TransmitError::Phase) => {
                        step.retirement_backpressure += 1;
                    }
                    Err(error) => {
                        step.failure = Some(TransmitServiceError::Port(error));
                        break;
                    }
                }
            }
        }
        step
    }
}

impl Sessions {
    pub(in crate::controller) fn service_transmit(
        &mut self,
        admission: &mut crate::rel99_admission::Rel99Admission,
        now: ChipInstant,
    ) -> TransmitStep {
        let order_failure = self.service_order_results(admission).err();
        let mut step = match (&mut self.transmit, admission.hsdpa_control()) {
            (Some(transmit), Some(mut port)) => {
                transmit.service(&mut self.entries, &mut port, self.budget, now)
            }
            _ => TransmitStep::default(),
        };
        if step.failure.is_none() {
            step.failure = order_failure.map(TransmitServiceError::Order);
        }
        step
    }
}

impl<T: Transport> CommonController<T> {
    pub fn configure_hsdpa_waveforms(
        &mut self,
        capacity: NonZeroUsize,
    ) -> Result<(), ControllerError> {
        let sessions = self.hsdpa.as_mut().ok_or(ControllerError::Configuration)?;
        if sessions.transmit.is_some()
            || crate::hsdpa_tx::TransmitBank::required_storage_bytes(capacity).is_err()
        {
            return Err(ControllerError::Configuration);
        }
        sessions.transmit = Some(Transmit::new(capacity));
        sessions.orders = Some(super::orders::Orders::new(capacity, sessions.entries.len()));
        Ok(())
    }
}
