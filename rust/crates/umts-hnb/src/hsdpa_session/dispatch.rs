// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_session::adaptation::FeedbackTraining;
use crate::{
    hsdpa_packet::{BlockBuffer, EncoderBank, PlanBank, PlanFormat, PrepareError, TransmitRequest},
    hsdpa_tx::{AllocationCalendar, ReservationId, WaveformBuffer, WaveformPreparer},
};

pub struct PacketPreparation<'a> {
    pub candidate: PacketCandidate,
    pub timing: FeedbackTiming,
    pub encoders: &'a mut EncoderBank,
    pub plans: &'a mut PlanBank,
    pub format: PlanFormat,
    pub preparer: &'a mut WaveformPreparer,
    pub control_code: u16,
    pub power: DownlinkPower,
    pub rate: Rate,
}

pub struct PacketBuffers {
    pub coded: Option<BlockBuffer>,
    pub waveform: WaveformBuffer,
}

#[must_use = "Return unused buffers to their admitted owner"]
pub enum PreparationOutcome {
    Prepared { unused_coded: Option<BlockBuffer> },
    NoPacket(PacketBuffers),
}

#[derive(Debug)]
pub enum DispatchError {
    Phase,
    Configuration,
    Timing(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_dsp::DspError,
    ),
    Power(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        Error,
    ),
    Selection(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        Error,
    ),
    Encoding(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::hsdpa_packet::Error,
    ),
    Waveform(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        PrepareError,
    ),
    Allocation(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::hsdpa_tx::Error,
    ),
}

pub struct RejectedDispatch {
    pub error: DispatchError,
    pub buffers: PacketBuffers,
}

impl Session {
    /// After encoding succeeds, HARQ retains the packet even if waveform preparation fails.
    pub fn prepare_scheduled_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        calendar: &mut AllocationCalendar,
        request: PacketPreparation<'_>,
        buffers: PacketBuffers,
    ) -> Result<PreparationOutcome, RejectedDispatch> {
        if !self.has_carrier(carrier) {
            return Err(RejectedDispatch {
                error: DispatchError::Phase,
                buffers,
            });
        }

        let reservation = match calendar.reserve(
            self.identity.ue,
            request.candidate.downlink,
            request.control_code,
            request.format.first_code,
            request.format.codes,
        ) {
            Ok(id) => id,
            Err(error) => {
                return Err(RejectedDispatch {
                    error: DispatchError::Allocation(error),
                    buffers,
                });
            }
        };
        let result = self.prepare_reserved_packet_on(carrier, request, buffers, reservation);
        if !matches!(result, Ok(PreparationOutcome::Prepared { .. })) {
            calendar
                .cancel(reservation)
                .expect("exclusive unpublished reservation");
        }
        result
    }

    fn prepare_reserved_packet_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        request: PacketPreparation<'_>,
        mut buffers: PacketBuffers,
        reservation: ReservationId,
    ) -> Result<PreparationOutcome, RejectedDispatch> {
        let reject = |error, buffers| RejectedDispatch { error, buffers };
        if self.phase() != Phase::Active
            || self.links[carrier.index()].feedback_policy.is_none()
            || self.links[carrier.index()].pending.is_some()
        {
            return Err(reject(DispatchError::Phase, buffers));
        }
        let candidate = request.candidate;
        if candidate.carrier != carrier {
            return Err(reject(DispatchError::Configuration, buffers));
        }
        let amplitudes = match self.packet_amplitudes_on(
            carrier,
            SelectedPacket {
                candidate,
                format: request.format,
                control_code: request.control_code,
                rate: request.rate,
            },
            request.power,
        ) {
            Ok(amplitudes) => amplitudes,
            Err(error) => return Err(reject(DispatchError::Power(error), buffers)),
        };
        let Some(plan) = request.plans.get_mut(request.format) else {
            return Err(reject(DispatchError::Configuration, buffers));
        };
        if !self.matches_transmit_configuration_on(
            carrier,
            candidate.process,
            plan,
            request.control_code,
        ) {
            return Err(reject(DispatchError::Configuration, buffers));
        }
        let expected_ack = match request.timing.acknowledgement(candidate.downlink) {
            Ok(start) => start,
            Err(error) => return Err(reject(DispatchError::Timing(error), buffers)),
        };
        match candidate.work {
            PacketWork::New { queue } if self.queues.staged_packet(queue).is_some() => {
                if buffers.coded.is_none() {
                    return Ok(PreparationOutcome::NoPacket(buffers));
                }
                if self
                    .queues
                    .staged_packet(queue)
                    .is_none_or(|packet| packet.transport_bits != plan.transport_bits().get())
                {
                    return Err(reject(DispatchError::Configuration, buffers));
                }
                match self.links[carrier.index()].packets.bind_staged(
                    &mut self.queues,
                    queue,
                    candidate.process,
                ) {
                    Ok(true) => {
                        let training = self.links[carrier.index()]
                            .staged_feedback_training
                            .get(usize::from(queue))
                            .copied()
                            .unwrap_or(FeedbackTraining::None);
                        self.links[carrier.index()]
                            .adaptation
                            .bind_packet(candidate.process, training);
                    }
                    Ok(false) => return Ok(PreparationOutcome::NoPacket(buffers)),
                    Err(error) => {
                        return Err(reject(
                            DispatchError::Selection(Error::Packet(error)),
                            buffers,
                        ));
                    }
                }
            }
            PacketWork::New { .. } => {
                let Some(encoder) = request.encoders.get_mut(plan.transport_bits()) else {
                    return Err(reject(DispatchError::Configuration, buffers));
                };
                let Some(coded) = buffers.coded.take() else {
                    return Ok(PreparationOutcome::NoPacket(buffers));
                };
                match self.encode_new_packet_on(carrier, candidate, encoder, coded) {
                    Ok(None) => self.links[carrier.index()]
                        .adaptation
                        .bind_packet(candidate.process, FeedbackTraining::None),
                    Ok(Some(coded)) => {
                        buffers.coded = Some(coded);
                        return Ok(PreparationOutcome::NoPacket(buffers));
                    }
                    Err(rejected) => {
                        buffers.coded = Some(rejected.buffer);
                        return Err(reject(DispatchError::Encoding(rejected.error), buffers));
                    }
                }
            }
            PacketWork::Retained { transport_bits } => {
                if transport_bits != plan.transport_bits()
                    || self.links[carrier.index()]
                        .packets
                        .process_state(candidate.process)
                        != Ok(umts_mac::hsdpa::ProcessState::Ready)
                    || self.links[carrier.index()]
                        .packets
                        .payload(candidate.process)
                        .ok()
                        .flatten()
                        .is_none_or(|block| block.transport_bits() != plan.transport_bits())
                {
                    return Err(reject(DispatchError::Configuration, buffers));
                }
            }
        }
        match self.prepare_packet_on(
            carrier,
            request.preparer,
            TransmitRequest {
                process: candidate.process,
                downlink: candidate.downlink,
                expected_ack,
                plan,
                control_code: request.control_code,
                amplitudes,
            },
            buffers.waveform,
            reservation,
        ) {
            Ok(()) => {
                let transmission = self
                    .pending_transmission_on(carrier)
                    .expect("prepared packet retained");
                self.links[carrier.index()]
                    .feedback_snapshot
                    .prepared_format(
                        transmission,
                        request.format,
                        matches!(candidate.work, PacketWork::Retained { .. }),
                    );
                if let Ok(Some(packet)) = self.links[carrier.index()]
                    .packets
                    .payload(candidate.process)
                    .map(|block| block.map(|block| block.packet()))
                {
                    self.links[carrier.index()]
                        .feedback_snapshot
                        .formats
                        .associate_packet(transmission, packet);
                }
                Ok(PreparationOutcome::Prepared {
                    unused_coded: buffers.coded,
                })
            }
            Err(rejected) => Err(reject(
                DispatchError::Waveform(rejected.error),
                PacketBuffers {
                    coded: buffers.coded,
                    waveform: rejected.buffer,
                },
            )),
        }
    }
}
