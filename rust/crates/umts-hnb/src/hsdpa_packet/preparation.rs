// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_tx::{
    self, Amplitudes, Preparation, PreparedTransmission, WaveformBuffer, WaveformPreparer,
};
use umts_dsp::hsdpa::TransmissionPlan;
use umts_types::time::ChipInstant;

pub struct TransmitRequest<'a> {
    pub process: ProcessId,
    pub downlink: ChipInstant,
    pub expected_ack: ChipInstant,
    pub plan: &'a mut TransmissionPlan,
    pub control_code: u16,
    pub amplitudes: Amplitudes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareError {
    Phase,
    Owner(OwnerError),
    Waveform(hsdpa_tx::Error),
    Rollback {
        waveform: hsdpa_tx::Error,
        owner: OwnerError,
    },
}

pub struct RejectedPreparation {
    pub error: PrepareError,
    pub buffer: WaveformBuffer,
}

impl PacketOwner {
    pub fn prepare_waveform(
        &mut self,
        preparer: &mut WaveformPreparer,
        request: TransmitRequest<'_>,
        buffer: WaveformBuffer,
    ) -> Result<PreparedTransmission, RejectedPreparation> {
        match self.payload(request.process) {
            Ok(Some(payload)) if payload.transport_bits() == request.plan.transport_bits() => (),
            Ok(Some(_)) => {
                return Err(RejectedPreparation {
                    error: PrepareError::Waveform(hsdpa_tx::Error::Dsp(DspError::InvalidLength)),
                    buffer,
                });
            }
            Ok(None) => {
                return Err(RejectedPreparation {
                    error: PrepareError::Owner(OwnerError::Process),
                    buffer,
                });
            }
            Err(error) => {
                return Err(RejectedPreparation {
                    error: PrepareError::Owner(error),
                    buffer,
                });
            }
        }
        let transmission =
            match self.reserve(request.process, request.downlink, request.expected_ack) {
                Ok(transmission) => transmission,
                Err(error) => {
                    return Err(RejectedPreparation {
                        error: PrepareError::Owner(error),
                        buffer,
                    });
                }
            };
        let coded = self
            .payload(request.process)
            .expect("reserved process")
            .expect("reservation requires payload")
            .coded();
        let prepared = match request
            .plan
            .select_process(transmission.key.process().value(), transmission.new_data)
        {
            Ok(()) => preparer.prepare(
                Preparation {
                    transmission,
                    plan: request.plan,
                    coded,
                    control_code: request.control_code,
                    amplitudes: request.amplitudes,
                },
                buffer,
            ),
            Err(error) => Err(hsdpa_tx::PreparationError {
                error: hsdpa_tx::Error::Dsp(error),
                buffer,
            }),
        };
        match prepared {
            Ok(prepared) => Ok(prepared),
            Err(rejected) => {
                let error = match self.cancel_preparation(transmission.key) {
                    Ok(()) => PrepareError::Waveform(rejected.error),
                    Err(owner) => PrepareError::Rollback {
                        waveform: rejected.error,
                        owner,
                    },
                };
                Err(RejectedPreparation {
                    error,
                    buffer: rejected.buffer,
                })
            }
        }
    }
}
