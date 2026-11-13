// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::epoch::EpochRetirementError;

#[derive(Debug)]
#[must_use = "Only a retired serving owner can authorize an automatic restart"]
pub struct RetiredServing {
    report: Box<EpochReport>,
}

impl RetiredServing {
    pub fn report(&self) -> &EpochReport {
        &self.report
    }
}

#[must_use = "Retain this runtime until its actual worker owners and scheduled TX tail can be retired"]
pub struct RetainedShutdown<T: Transport, D: BoundDatagrams> {
    pub error: EpochRetirementError,
    runtime: Box<ServingRuntime<T, D>>,
}

impl<T: Transport, D: BoundDatagrams> std::fmt::Debug for RetainedShutdown<T, D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedShutdown")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<T: Transport, D: BoundDatagrams> RetainedShutdown<T, D> {
    pub fn into_runtime(self) -> Box<ServingRuntime<T, D>> {
        self.runtime
    }
}

impl<T: Transport, D: BoundDatagrams> ServingRuntime<T, D> {
    /// Failure returns every original media, radio and quarantine owner for another attempt.
    pub fn try_finish_shutdown(mut self) -> Result<RetiredServing, RetainedShutdown<T, D>> {
        match self.epoch.finish_retirement() {
            Ok(report) => Ok(RetiredServing {
                report: Box::new(report),
            }),
            Err(error) => Err(RetainedShutdown {
                error,
                runtime: Box::new(self),
            }),
        }
    }
}
