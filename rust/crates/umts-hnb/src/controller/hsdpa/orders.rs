// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitError},
    hsdpa_packet::{CommandResult, ControlPort, Operation},
    hsdpa_session::{OrderProgress, OrderTransmission},
    hsdpa_tx::{OrderBuffer, OrderIdentity, OrderPreparation},
    radio_resources::HsdpaCodeSet,
};
use umts_types::time::CHIPS_PER_SLOT;

const SUBFRAME_CHIPS: u64 = 3 * CHIPS_PER_SLOT;

struct Flight {
    key: OrderIdentity,
    transmission: OrderTransmission,
    command: TransmitCommandId,
    queued: bool,
    feedback_requested: bool,
    completed: bool,
}

pub(super) struct Orders {
    codes: Option<(HsdpaCodeSet, umts_dsp::scrambling::PrimaryScramblingCode)>,
    buffers: Vec<OrderBuffer>,
    capacity: usize,
    flights: Box<[Option<Flight>]>,
    cursor: usize,
    feedback_cursor: usize,
    retained: Option<CommandResult>,
}

impl Orders {
    pub(super) fn has_flight(&self, identity: crate::dedicated_rx::ReceiveIdentity) -> bool {
        self.flights
            .get(identity.ue.slot as usize)
            .and_then(Option::as_ref)
            .is_some_and(|flight| flight.key.identity == identity)
    }
    pub(super) fn new(capacity: NonZeroUsize, ues: usize) -> Self {
        Self {
            codes: None,
            buffers: Vec::new(),
            capacity: capacity.get(),
            flights: (0..ues).map(|_| None).collect(),
            cursor: 0,
            feedback_cursor: 0,
            retained: None,
        }
    }

    fn session(entries: &mut [Option<Box<Session>>], key: OrderIdentity) -> Option<&mut Session> {
        entries
            .get_mut(key.identity.ue.slot as usize)?
            .as_deref_mut()
            .filter(|session| session.identity() == key.identity)
    }

    fn collect(
        &mut self,
        entries: &mut [Option<Box<Session>>],
        control: &mut ControlPort<'_>,
        budget: usize,
    ) -> Result<(), String> {
        if self.codes.is_none() {
            return Ok(());
        }
        if self.retained.is_some() {
            // An unknown command outcome cannot establish waveform custody or the peer's codebook,
            // so affected sessions stop resuming data until epoch teardown.
            for flight in self.flights.iter().flatten() {
                if let Some(session) = Self::session(entries, flight.key) {
                    session.secondary_order_not_transmitted(flight.transmission);
                }
            }
            return Err("HS-SCCH order result identity requires recovery".into());
        }
        let mut failure = None;
        for _ in 0..budget {
            let Some(result) = control.take_order_result() else {
                break;
            };
            if !result.matches_request() {
                self.retained = Some(result);
                return Err("HS-SCCH order result mismatch".into());
            }
            if matches!(&result.event.outcome, TransmitCommandOutcome::Closed) {
                self.retained = Some(result);
                return Err("HS-SCCH order transmitter closed".into());
            }
            let key = match result.operation {
                Operation::Order(key) => key,
                Operation::OrderCompletion => match &result.event.outcome {
                    TransmitCommandOutcome::HsdpaOrderCompletion(Some(completion)) => {
                        completion.order.identity()
                    }
                    TransmitCommandOutcome::HsdpaOrderCompletion(None) => continue,
                    _ => {
                        self.retained = Some(result);
                        return Err("HS-SCCH order completion unavailable".into());
                    }
                },
                _ => {
                    self.retained = Some(result);
                    return Err("HS-SCCH order operation mismatch".into());
                }
            };
            let Some(slot) = self.flights.get_mut(key.identity.ue.slot as usize) else {
                self.retained = Some(result);
                return Err("HS-SCCH order UE slot mismatch".into());
            };
            let Some(flight) = slot.as_mut().filter(|flight| flight.key == key) else {
                self.retained = Some(result);
                return Err("HS-SCCH order flight mismatch".into());
            };
            if matches!(result.operation, Operation::Order(_)) && result.event.id != flight.command
            {
                self.retained = Some(result);
                return Err("HS-SCCH order command mismatch".into());
            }
            if matches!(
                &result.event.outcome,
                TransmitCommandOutcome::HsdpaOrderQueued { result: Err(_), .. }
                    | TransmitCommandOutcome::HsdpaOrderCompletion(Some(_))
            ) && self.buffers.len() == self.capacity
            {
                self.retained = Some(result);
                return Err("HS-SCCH order buffer ownership mismatch".into());
            }
            match result.event.outcome {
                TransmitCommandOutcome::HsdpaOrderQueued { result: Ok(()), .. } => {
                    flight.queued = true;
                    if let Some(session) = Self::session(entries, key)
                        && let Err(error) = session.accept_secondary_order(flight.transmission)
                    {
                        session.secondary_order_not_transmitted(flight.transmission);
                        failure.get_or_insert_with(|| {
                            format!("HS-SCCH accepted order state: {error:?}")
                        });
                    }
                }
                TransmitCommandOutcome::HsdpaOrderQueued {
                    result: Err(rejected),
                    ..
                } => {
                    self.buffers.push(rejected.order.recycle());
                    *slot = None;
                }
                TransmitCommandOutcome::HsdpaOrderCompletion(Some(completed)) => {
                    if !completed.submitted
                        && let Some(session) = Self::session(entries, key)
                    {
                        session.secondary_order_not_transmitted(flight.transmission);
                    }
                    self.buffers.push(completed.order.recycle());
                    flight.completed = true;
                }
                _ => {
                    // Closed has no waveform custody. Outstanding bank ownership is resolved by epoch teardown.
                    failure.get_or_insert_with(|| "HS-SCCH order transmitter closed".into());
                }
            }
        }
        for _ in 0..budget.min(self.flights.len()) {
            let index = self.feedback_cursor;
            self.feedback_cursor = (index + 1) % self.flights.len();
            let slot = &mut self.flights[index];
            let Some(flight) = slot else {
                continue;
            };
            if flight.queued && !flight.feedback_requested {
                if let Some(session) = Self::session(entries, flight.key) {
                    if matches!(session.phase(), Phase::Active | Phase::ChangingRadio) {
                        match session.request_secondary_order_feedback(flight.transmission) {
                            Ok(_) => flight.feedback_requested = true,
                            Err(Error::Capacity) => (),
                            Err(error) => {
                                flight.feedback_requested = true;
                                session.secondary_order_not_transmitted(flight.transmission);
                                failure.get_or_insert_with(|| {
                                    format!("HS-SCCH order feedback request: {error:?}")
                                });
                            }
                        }
                    } else {
                        flight.feedback_requested = true;
                    }
                } else {
                    flight.feedback_requested = true;
                }
            }
            if flight.queued && flight.feedback_requested && flight.completed {
                *slot = None;
            }
        }
        if self.buffers.len() < self.capacity {
            match control.request_order_completion() {
                Ok(_) | Err(TransmitError::Capacity | TransmitError::Phase) => (),
                Err(error) => {
                    failure.get_or_insert_with(|| {
                        format!("HS-SCCH order completion request: {error:?}")
                    });
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Sessions {
    pub(super) fn ensure_order_carrier(
        &mut self,
        codes: HsdpaCodeSet,
        primary: umts_dsp::scrambling::PrimaryScramblingCode,
    ) -> Result<(), String> {
        let orders = self
            .orders
            .as_mut()
            .ok_or("HS-SCCH order owner is unavailable")?;
        if orders
            .codes
            .is_some_and(|existing| existing != (codes, primary))
        {
            return Err("HS-SCCH order carrier changed".into());
        }
        if orders.codes.is_none() {
            orders.buffers = (0..orders.capacity)
                .map(|_| OrderBuffer::default())
                .collect();
        }
        orders.codes = Some((codes, primary));
        Ok(())
    }

    pub(super) fn service_order_results(
        &mut self,
        admission: &mut crate::rel99_admission::Rel99Admission,
    ) -> Result<(), String> {
        if let (Some(orders), Some(mut control)) = (&mut self.orders, admission.hsdpa_control()) {
            orders.collect(&mut self.entries, &mut control, self.budget.get())
        } else {
            Ok(())
        }
    }
}

impl<T: Transport> CommonController<T> {
    pub fn service_hsdpa_orders(
        &mut self,
        now: ChipInstant,
        not_before: ChipInstant,
    ) -> Result<(), String> {
        if now.epoch != not_before.epoch || not_before.index < now.index {
            return Err("HS-SCCH order scheduling time is invalid".into());
        }
        let (Some(sessions), Some(receive), Some(admission)) =
            (&mut self.hsdpa, &mut self.dedicated, &mut self.admission)
        else {
            return Ok(());
        };
        sessions.service_order_results(admission)?;
        let (Some(orders), Some(transmit), Some(mut control)) = (
            &mut sessions.orders,
            &mut sessions.transmit,
            admission.hsdpa_control(),
        ) else {
            return Ok(());
        };
        let Some((codes, _)) = orders.codes else {
            return Ok(());
        };
        let Some(control_code) = codes.control_codes().next().map(|code| code.index()) else {
            return Err("HS-SCCH order control code missing".into());
        };
        for _ in 0..sessions.budget.get().min(sessions.entries.len()) {
            let index = orders.cursor;
            orders.cursor = (index + 1) % sessions.entries.len();
            let Some(session) = sessions.entries[index].as_deref_mut() else {
                continue;
            };
            if session.phase() != Phase::Active
                || session.secondary_state().is_none()
                || orders.flights[index].is_some()
            {
                continue;
            }
            session
                .advance_secondary_order(now)
                .map_err(|error| format!("HS-SCCH order timer: {error:?}"))?;
            if let Some(target) = session.next_order_target(now) {
                session
                    .request_secondary_order(target)
                    .map_err(|error| format!("HS-SCCH order target: {error:?}"))?;
            }
            // Let the previous request leave the receive queue before assigning a retry its own ACK
            // identity.
            // A delayed previous report must not acknowledge the retry.
            if session.order_progress() != OrderProgress::Ready
                || session.secondary_order_feedback_pending()
            {
                continue;
            }
            let timing = receive
                .with_active_mapping(session.identity(), |bank| session.feedback_timing(bank))
                .map_err(|error| format!("HS-SCCH order receiver: {error:?}"))?
                .map_err(|error| format!("HS-SCCH order timing: {error:?}"))?;
            let scch_start = ChipInstant {
                index: not_before
                    .index
                    .div_ceil(SUBFRAME_CHIPS)
                    .checked_mul(SUBFRAME_CHIPS)
                    .ok_or("HS-SCCH order time overflow")?,
                ..not_before
            };
            let ack = session
                .order_feedback_time(scch_start, timing)
                .map_err(|error| format!("HS-SCCH order ACK timing: {error:?}"))?;
            let tx = session
                .prepare_secondary_order(scch_start, ack)
                .map_err(|error| format!("HS-SCCH order prepare: {error:?}"))?;
            let amplitude = session
                .order_control_amplitude()
                .ok_or("HS-SCCH order power unavailable")?;
            let Some(buffer) = orders.buffers.pop() else {
                break;
            };
            let key = OrderIdentity {
                identity: session.identity(),
                generation: tx.generation,
                ordinal: tx.ordinal,
                scch_start: tx.scch_start,
                expected_ack: tx.feedback_start,
            };
            let request = OrderPreparation {
                key,
                h_rnti: session.h_rnti(),
                order: tx.order,
                control_code,
                amplitude,
            };
            let order = match transmit.prepare_order(request, buffer) {
                Ok(order) => order,
                Err(rejected) => {
                    orders.buffers.push(rejected.buffer);
                    return Err(format!("HS-SCCH order waveform: {:?}", rejected.error));
                }
            };
            match control.queue_order(order) {
                Ok(command) => {
                    orders.flights[index] = Some(Flight {
                        key,
                        transmission: tx,
                        command,
                        queued: false,
                        feedback_requested: false,
                        completed: false,
                    })
                }
                Err(order) => {
                    orders.buffers.push(order.recycle());
                    break;
                }
            }
        }
        Ok(())
    }
}
