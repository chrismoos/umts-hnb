// SPDX-License-Identifier: AGPL-3.0-or-later

use super::TxPump;
use umts_types::time::SampleInstant;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HalStopOutcome {
    NotAttempted,
    Succeeded,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransmitTail {
    end: SampleInstant,
    tick: u64,
}

impl TransmitTail {
    pub(super) fn new(end: SampleInstant, tick: u64) -> Self {
        Self { end, tick }
    }
    #[cfg(test)]
    pub fn end(self) -> SampleInstant {
        self.end
    }
    pub fn tick(self) -> u64 {
        self.tick
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalTransmitDrain {
    Unsubmitted,
    Waiting {
        tail: TransmitTail,
        observed_tick: u64,
    },
    Elapsed {
        tail: TransmitTail,
        observed_tick: u64,
    },
}

impl TerminalTransmitDrain {
    pub fn elapsed(self) -> bool {
        !matches!(self, Self::Waiting { .. })
    }
}

#[derive(Debug)]
pub enum TerminalDrainError {
    NotStopped,
    Driver(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        sdr_hal::error::Error,
    ),
    ClockReversed {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        previous: u64,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        observed: u64,
    },
}

impl TxPump {
    #[cfg(test)]
    pub fn terminal_tail(&self) -> Option<TransmitTail> {
        self.terminal_tail
    }

    // Only the joined epoch owner can use this observation to authorize resource retirement.
    pub(crate) fn observe_terminal_drain(
        &mut self,
    ) -> Result<TerminalTransmitDrain, TerminalDrainError> {
        if !self.terminal_stopped || !self.stop.is_stopping() {
            return Err(TerminalDrainError::NotStopped);
        }
        let Some(tail) = self.terminal_tail else {
            return Ok(TerminalTransmitDrain::Unsubmitted);
        };
        let observed = self
            .submission
            .hardware_time()
            .map_err(TerminalDrainError::Driver)?;
        if let Some(previous) = self.terminal_clock
            && observed < previous
        {
            return Err(TerminalDrainError::ClockReversed { previous, observed });
        }
        self.terminal_clock = Some(observed);
        Ok(if observed >= tail.tick {
            TerminalTransmitDrain::Elapsed {
                tail,
                observed_tick: observed,
            }
        } else {
            TerminalTransmitDrain::Waiting {
                tail,
                observed_tick: observed,
            }
        })
    }
}
