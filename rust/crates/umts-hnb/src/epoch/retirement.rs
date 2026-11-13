// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_io::{HalStopOutcome, TerminalDrainError, TerminalTransmitDrain};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochOwner {
    Preparation,
    Submission,
    Receive,
}

#[derive(Debug)]
pub enum EpochRetirementError {
    NotJoined,
    Finished,
    MissingOwner(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        EpochOwner,
    ),
    TransmitStop(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        String,
    ),
    ReceiveStop(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        String,
    ),
    Drain(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        TerminalDrainError,
    ),
    TailPending(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        TerminalTransmitDrain,
    ),
}

#[derive(Default)]
pub(super) struct Retirement {
    preparation: Option<TxPreparation>,
    submission: Option<TxPump>,
    receiver: Option<CommonRx>,
    finished: bool,
}

impl<T: Transport> CommonEpoch<T> {
    pub(super) fn collect_retired_owners(&mut self) -> Result<(), EpochJoinError> {
        if self.retirement.finished {
            return Ok(());
        }
        if self.retirement.preparation.is_none() {
            self.retirement.preparation = self.tx.take_retired_preparation();
        }
        if self.retirement.submission.is_none() {
            self.retirement.submission = self.tx.take_retired_submission();
        }
        if self.retirement.receiver.is_none() {
            self.retirement.receiver = self.rx.take_retired_runtime();
        }
        for (owner, present) in [
            (
                EpochOwner::Preparation,
                self.retirement.preparation.is_some(),
            ),
            (EpochOwner::Submission, self.retirement.submission.is_some()),
            (EpochOwner::Receive, self.retirement.receiver.is_some()),
        ] {
            if !present {
                return Err(EpochJoinError::MissingOwner(owner));
            }
        }
        Ok(())
    }

    /// Requires actual joined owners. Reports and channel closure cannot substitute for them.
    pub fn poll_retirement(&mut self) -> Result<TerminalTransmitDrain, EpochRetirementError> {
        if self.retirement.finished {
            return Err(EpochRetirementError::Finished);
        }
        if self.phase != EpochPhase::Stopped || self.tx_report.is_none() || self.rx_report.is_none()
        {
            return Err(EpochRetirementError::NotJoined);
        }
        let rx = self
            .retirement
            .receiver
            .as_mut()
            .ok_or(EpochRetirementError::MissingOwner(EpochOwner::Receive))?;
        let rx_report = self.rx_report.as_mut().expect("joined receive report");
        if rx_report.shutdown != HalStopOutcome::Succeeded {
            match rx.stop_for_retirement() {
                Ok(()) => rx_report.shutdown = HalStopOutcome::Succeeded,
                Err(error) => {
                    let error = format!("{error:?}");
                    rx_report.shutdown = HalStopOutcome::Failed(error.clone());
                    return Err(EpochRetirementError::ReceiveStop(error));
                }
            }
        }
        let tx = self
            .retirement
            .submission
            .as_mut()
            .ok_or(EpochRetirementError::MissingOwner(EpochOwner::Submission))?;
        let tx_report = self.tx_report.as_mut().expect("joined transmit report");
        if tx_report.shutdown != HalStopOutcome::Succeeded {
            match tx.stop() {
                Ok(()) => tx_report.shutdown = HalStopOutcome::Succeeded,
                Err(error) => {
                    let error = error.to_string();
                    tx_report.shutdown = HalStopOutcome::Failed(error.clone());
                    return Err(EpochRetirementError::TransmitStop(error));
                }
            }
        }
        tx.observe_terminal_drain()
            .map_err(EpochRetirementError::Drain)
    }

    pub fn finish_retirement(&mut self) -> Result<EpochReport, EpochRetirementError> {
        if !self.retirement.finished {
            let progress = self.poll_retirement()?;
            if !progress.elapsed() {
                return Err(EpochRetirementError::TailPending(progress));
            }
            self.controller.take();
            self.retirement.preparation.take();
            self.retirement.receiver.take();
            self.retirement.submission.take();
            self.retirement.finished = true;
        }
        Ok(EpochReport {
            epoch: self.epoch(),
            failure: self.failure.clone(),
            tx: self
                .tx_report
                .as_ref()
                .ok_or(EpochRetirementError::NotJoined)?
                .clone(),
            rx: self
                .rx_report
                .as_ref()
                .ok_or(EpochRetirementError::NotJoined)?
                .clone(),
        })
    }
}
