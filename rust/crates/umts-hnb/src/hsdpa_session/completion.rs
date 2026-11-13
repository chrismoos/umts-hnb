// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{TransmitCommandOutcome, TransmitEvent},
    hsdpa_packet::{CommandResult, FlightError, Operation},
    hsdpa_tx::{RejectedTransmission, WaveformBuffer},
};

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub enum CommandProgress {
    Queued,
    Empty,
    Recycled {
        buffer: WaveformBuffer,
        rejection: Option<crate::hsdpa_tx::Error>,
        late_chips: Option<u64>,
    },
    Retirement(RetirementProgress),
    RetirementBackpressure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandFailure {
    Identity,
    Closed,
    Flight(FlightError),
    Retirement(RetirementError),
}

pub struct RejectedCommand {
    pub error: CommandFailure,
    pub result: CommandResult,
}

impl Session {
    #[cfg(test)]
    pub fn accept_command(
        &mut self,
        result: CommandResult,
    ) -> Result<CommandProgress, Box<RejectedCommand>> {
        self.accept_command_on(umts_types::hsdpa::CarrierId::Primary, result)
    }

    pub fn accept_command_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        result: CommandResult,
    ) -> Result<CommandProgress, Box<RejectedCommand>> {
        if !self.has_carrier(carrier) {
            return Err(Box::new(RejectedCommand {
                error: CommandFailure::Identity,
                result,
            }));
        }

        if !result.matches_request() {
            return Err(Box::new(RejectedCommand {
                error: CommandFailure::Identity,
                result,
            }));
        }
        if matches!(
            result.operation,
            Operation::BeginRetirement(_) | Operation::FinishRetirement(_)
        ) {
            return match self.retirement_result(&result) {
                Ok(progress) => Ok(CommandProgress::Retirement(progress)),
                Err(RetirementError::Transmit(crate::hsdpa_tx::Error::Capacity)) => {
                    Ok(CommandProgress::RetirementBackpressure)
                }
                Err(error) => Err(Box::new(RejectedCommand {
                    error: CommandFailure::Retirement(error),
                    result,
                })),
            };
        }
        if let Operation::Queue(key) = result.operation
            && self.links[carrier.index()]
                .queue_commands
                .get(usize::from(key.process().value()))
                .copied()
                .flatten()
                != Some((result.event.id, key))
        {
            return Err(Box::new(RejectedCommand {
                error: CommandFailure::Identity,
                result,
            }));
        }
        let key = match &result.event.outcome {
            TransmitCommandOutcome::HsdpaQueued { result: Ok(()), .. } => None,
            TransmitCommandOutcome::HsdpaQueued {
                key,
                result: Err(_),
            } => Some(*key),
            TransmitCommandOutcome::HsdpaCompletion(Some(completion)) => {
                Some(completion.transmission.transmission().key)
            }
            TransmitCommandOutcome::HsdpaCompletion(None) => None,
            TransmitCommandOutcome::Closed => {
                return Err(Box::new(RejectedCommand {
                    error: CommandFailure::Closed,
                    result,
                }));
            }
            _ => {
                return Err(Box::new(RejectedCommand {
                    error: CommandFailure::Identity,
                    result,
                }));
            }
        };
        if key.is_some_and(|key| !self.links[carrier.index()].packets.owns_transmission(key)) {
            return Err(Box::new(RejectedCommand {
                error: CommandFailure::Identity,
                result,
            }));
        }
        let CommandResult {
            operation,
            event: TransmitEvent { id, outcome },
        } = result;
        let rejected = |error, outcome| {
            Box::new(RejectedCommand {
                error: CommandFailure::Flight(error),
                result: CommandResult {
                    operation,
                    event: TransmitEvent { id, outcome },
                },
            })
        };
        let progress = match outcome {
            TransmitCommandOutcome::HsdpaQueued { result: Ok(()), .. } => {
                Ok(CommandProgress::Queued)
            }
            TransmitCommandOutcome::HsdpaQueued {
                key,
                result: Err(returned),
            } => {
                let RejectedTransmission {
                    error,
                    transmission,
                    late_chips,
                } = returned;
                self.links[carrier.index()]
                    .packets
                    .reject_transmit(transmission)
                    .map(|buffer| {
                        self.sdu_budget.release_untransmitted(carrier, key);
                        CommandProgress::Recycled {
                            buffer,
                            rejection: Some(error),
                            late_chips,
                        }
                    })
                    .map_err(|failure| {
                        rejected(
                            failure.error,
                            TransmitCommandOutcome::HsdpaQueued {
                                key,
                                result: Err(RejectedTransmission {
                                    error,
                                    transmission: failure.completion.transmission,
                                    late_chips,
                                }),
                            },
                        )
                    })
            }
            TransmitCommandOutcome::HsdpaCompletion(Some(completion)) => self
                .complete_transmit_on(carrier, completion)
                .map(|buffer| CommandProgress::Recycled {
                    buffer,
                    rejection: None,
                    late_chips: None,
                })
                .map_err(|failure| {
                    rejected(
                        failure.error,
                        TransmitCommandOutcome::HsdpaCompletion(Some(failure.completion)),
                    )
                }),
            TransmitCommandOutcome::HsdpaCompletion(None) => Ok(CommandProgress::Empty),
            _ => unreachable!("validated packet command outcome"),
        };
        if progress.is_ok()
            && let Operation::Queue(key) = operation
        {
            self.links[carrier.index()].queue_commands[usize::from(key.process().value())] = None;
        }
        progress
    }
}
