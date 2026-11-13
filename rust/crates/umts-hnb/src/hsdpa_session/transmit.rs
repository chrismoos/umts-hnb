// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::TransmitCommandId,
    hsdpa_packet::{ControlPort, FlightError, PrepareError, RejectedPreparation, TransmitRequest},
    hsdpa_tx::{
        AllocationCalendar, PreparedTransmission, ReservationId, WaveformBuffer, WaveformPreparer,
    },
};
use umts_mac::hsdpa::TransmissionKey;

pub(super) struct PendingPacket {
    transmission: PreparedTransmission,
    feedback_requested: bool,
    reservation: ReservationId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketPublication {
    Idle,
    Backpressure,
    Queued {
        command: TransmitCommandId,
        key: TransmissionKey,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketPublicationError {
    Phase,
    Closed,
    Feedback(FeedbackError),
    Allocation(crate::hsdpa_tx::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketCancellationError {
    Allocation(crate::hsdpa_tx::Error),
    Flight(FlightError),
}

impl Session {
    pub(super) fn matches_transmit_configuration_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
        process: umts_mac::hsdpa::ProcessId,
        plan: &umts_dsp::hsdpa::TransmissionPlan,
        control_code: u16,
    ) -> bool {
        let Some(link) = self.links.get(carrier.index()) else {
            return false;
        };
        let codes = link.rates.codes();
        plan.h_rnti() == self.procedure.construction().parameters().h_rnti
            && (plan.assignment().table() != umts_dsp::hsdpa::TransportTable::OctetAligned
                || codes
                    .control_codes()
                    .nth(usize::from(plan.assignment().control_list_number() - 1))
                    .is_some_and(|code| code.index() == control_code))
            && link
                .packets
                .soft_bits(process)
                .is_ok_and(|bits| bits == plan.soft_bits())
            && codes
                .control_codes()
                .any(|code| code.index() == control_code)
            && plan
                .assignment()
                .validate_category(link.packets.category(), plan.transport_bits().get())
                .is_ok()
    }

    pub(super) fn prepare_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        preparer: &mut WaveformPreparer,
        request: TransmitRequest<'_>,
        buffer: WaveformBuffer,
        reservation: ReservationId,
    ) -> Result<(), RejectedPreparation> {
        if self.phase() != Phase::Active
            || self.links[carrier.index()].feedback_policy.is_none()
            || self.links[carrier.index()].pending.is_some()
            || self.links[carrier.index()]
                .queue_commands
                .get(usize::from(request.process.value()))
                .is_none_or(Option::is_some)
        {
            return Err(RejectedPreparation {
                error: PrepareError::Phase,
                buffer,
            });
        }
        if !self.matches_transmit_configuration_on(
            carrier,
            request.process,
            request.plan,
            request.control_code,
        ) {
            return Err(RejectedPreparation {
                error: PrepareError::Waveform(crate::hsdpa_tx::Error::Identity),
                buffer,
            });
        }
        let ehs_packet = if self.procedure.construction().configuration().uses_mac_ehs() {
            let packet = self.links[carrier.index()]
                .packets
                .payload(request.process)
                .ok()
                .flatten()
                .map(|block| block.packet());
            let Some(packet) = packet else {
                return Err(RejectedPreparation {
                    error: PrepareError::Owner(OwnerError::Process),
                    buffer,
                });
            };
            if self
                .sdu_budget
                .earliest(carrier, request.downlink, packet.pdus)
                != Ok(request.downlink)
            {
                return Err(RejectedPreparation {
                    error: PrepareError::Waveform(crate::hsdpa_tx::Error::Capacity),
                    buffer,
                });
            }
            Some(packet)
        } else {
            None
        };
        let transmission = self.links[carrier.index()]
            .packets
            .prepare_waveform(preparer, request, buffer)?;
        if let Some(packet) = ehs_packet {
            self.sdu_budget
                .reserve(carrier, transmission.transmission(), packet)
                .expect("exclusive preflighted aggregate SDU reservation");
        }
        self.links[carrier.index()].pending = Some(PendingPacket {
            transmission,
            feedback_requested: false,
            reservation,
        });
        Ok(())
    }

    pub fn pending_transmission_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
    ) -> Option<umts_mac::hsdpa::ScheduledTransmission> {
        if !self.has_carrier(carrier) {
            return None;
        }

        self.links[carrier.index()]
            .pending
            .as_ref()
            .map(|pending| pending.transmission.transmission())
    }

    pub fn publish_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        control: &mut ControlPort<'_>,
        calendar: &mut AllocationCalendar,
    ) -> Result<PacketPublication, PacketPublicationError> {
        if !self.has_carrier(carrier) {
            return Err(PacketPublicationError::Phase);
        }

        if self.phase() != Phase::Active {
            return Err(PacketPublicationError::Phase);
        }
        if !control.available() {
            return Err(PacketPublicationError::Closed);
        }
        let link = &mut self.links[carrier.index()];
        let Some(pending) = &mut link.pending else {
            return Ok(PacketPublication::Idle);
        };
        calendar
            .validate_unpublished(pending.reservation)
            .map_err(PacketPublicationError::Allocation)?;
        let key = pending.transmission.transmission().key;
        if !pending.feedback_requested {
            let requested = if let Some(schedule) = self.feedback_mode {
                link.packets
                    .request_feedback_cells(key.process(), &mut self.feedback, schedule)
            } else {
                link.packets
                    .request_feedback(key.process(), &mut self.feedback)
            };
            match requested {
                Ok(_) => pending.feedback_requested = true,
                Err(FeedbackError::Capacity) if self.feedback.available() => {
                    return Ok(PacketPublication::Backpressure);
                }
                Err(error) => return Err(PacketPublicationError::Feedback(error)),
            }
        }
        let pending = link.pending.take().expect("pending packet");
        match control.queue(pending.transmission) {
            Ok(command) => {
                calendar
                    .publish(pending.reservation)
                    .expect("exclusive validated reservation");
                link.queue_commands[usize::from(key.process().value())] = Some((command, key));
                Ok(PacketPublication::Queued { command, key })
            }
            Err(transmission) => {
                link.pending = Some(PendingPacket {
                    transmission,
                    feedback_requested: pending.feedback_requested,
                    reservation: pending.reservation,
                });
                if control.available() {
                    Ok(PacketPublication::Backpressure)
                } else {
                    Err(PacketPublicationError::Closed)
                }
            }
        }
    }

    pub fn cancel_pending_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        calendar: &mut AllocationCalendar,
    ) -> Result<Option<WaveformBuffer>, PacketCancellationError> {
        if !self.has_carrier(carrier) {
            return Ok(None);
        }

        if let Some(pending) = &self.links[carrier.index()].pending {
            calendar
                .validate_unpublished(pending.reservation)
                .map_err(PacketCancellationError::Allocation)?;
        }
        let Some(pending) = self.links[carrier.index()].pending.take() else {
            return Ok(None);
        };
        let key = pending.transmission.transmission().key;
        match self.links[carrier.index()]
            .packets
            .reject_transmit(pending.transmission)
        {
            Ok(buffer) => {
                self.sdu_budget.release_untransmitted(carrier, key);
                calendar
                    .cancel(pending.reservation)
                    .expect("exclusive validated reservation");
                Ok(Some(buffer))
            }
            Err(rejected) => {
                self.links[carrier.index()].pending = Some(PendingPacket {
                    transmission: rejected.completion.transmission,
                    feedback_requested: pending.feedback_requested,
                    reservation: pending.reservation,
                });
                Err(PacketCancellationError::Flight(rejected.error))
            }
        }
    }
}
