// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{PreparedServing, RetiredServing, fresh_epoch};
use anyhow::{Result, ensure};
use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};
use umts_config::Config;

#[derive(Debug)]
pub struct RecoveryBudget {
    maximum_restarts: u16,
    attempts: u16,
    backoff: Duration,
    maximum_backoff: Duration,
}

#[derive(Debug)]
#[must_use = "Retain the one-shot restart owner until its delay expires or recovery is canceled"]
pub struct PendingRestart {
    retired: RetiredServing,
    attempt: u16,
    ready_at: Instant,
}

#[derive(Debug)]
pub enum RestartError {
    TooEarly {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        ready_at: Instant,
    },
    Preparation(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        anyhow::Error,
    ),
}

#[derive(Debug)]
#[must_use = "Recover the pending restart owner before retrying preparation"]
pub struct RejectedRestart {
    pub error: RestartError,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pending: Box<PendingRestart>,
}

impl RejectedRestart {
    #[cfg(test)]
    pub fn into_pending(self) -> PendingRestart {
        *self.pending
    }
}

impl RecoveryBudget {
    pub fn new(config: &Config) -> Result<Self> {
        let recovery = &config.runtime.recovery;
        recovery.validate_schema()?;
        ensure!(
            recovery.backoff_ms <= recovery.maximum_backoff_ms,
            "runtime.recovery.backoff_ms must not exceed maximum_backoff_ms"
        );
        Ok(Self {
            maximum_restarts: recovery.maximum_restarts,
            attempts: 0,
            backoff: Duration::from_millis(u64::from(recovery.backoff_ms)),
            maximum_backoff: Duration::from_millis(u64::from(recovery.maximum_backoff_ms)),
        })
    }

    pub fn attempts(&self) -> u16 {
        self.attempts
    }

    /// The process-wide budget is never reset by successful registration or a short run.
    pub fn schedule(
        &mut self,
        retired: RetiredServing,
        now: Instant,
    ) -> Result<PendingRestart, RetiredServing> {
        if self.attempts >= self.maximum_restarts {
            return Err(retired);
        }
        let delay = retry_delay(self.backoff, self.maximum_backoff, self.attempts);
        let Some(ready_at) = now.checked_add(delay) else {
            return Err(retired);
        };
        self.attempts += 1;
        Ok(PendingRestart {
            retired,
            attempt: self.attempts,
            ready_at,
        })
    }
}

/// Consecutive failures to open the HNB-GW association at startup, before the radio opens.
#[derive(Debug)]
pub struct CoreRetry {
    limit: Option<NonZeroU32>,
    backoff: Duration,
    failures: u32,
}

impl CoreRetry {
    pub fn new(config: &Config) -> Self {
        Self {
            limit: NonZeroU32::new(config.core.iuh.retry_attempts),
            backoff: Duration::from_secs(config.core.iuh.retry_backoff_secs),
            failures: 0,
        }
    }

    /// Returns the delay before the next attempt, or None once the retry limit is spent.
    pub fn failed(&mut self) -> Option<Duration> {
        if self.limit.is_some_and(|limit| self.failures >= limit.get()) {
            return None;
        }
        self.failures = self.failures.saturating_add(1);
        Some(self.backoff)
    }

    pub fn connected(&mut self) {
        self.failures = 0;
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

fn retry_delay(base: Duration, maximum: Duration, attempt: u16) -> Duration {
    let multiplier = 1_u32.checked_shl(u32::from(attempt)).unwrap_or(u32::MAX);
    base.saturating_mul(multiplier).min(maximum)
}

impl PendingRestart {
    pub fn attempt(&self) -> u16 {
        self.attempt
    }
    pub fn ready_at(&self) -> Instant {
        self.ready_at
    }
    pub fn previous_epoch(&self) -> umts_types::RadioEpoch {
        self.retired.report().epoch
    }

    pub fn prepare(
        self,
        config: &Config,
        now: Instant,
    ) -> Result<PreparedServing, RejectedRestart> {
        if now < self.ready_at {
            return Err(RejectedRestart {
                error: RestartError::TooEarly {
                    ready_at: self.ready_at,
                },
                pending: Box::new(self),
            });
        }
        let plan = (|| {
            let epoch = fresh_epoch()?;
            ensure!(
                epoch != self.previous_epoch(),
                "new radio epoch repeats the retired epoch"
            );
            let prepared = PreparedServing::new(config, epoch)?;
            Ok(prepared)
        })();
        plan.map_err(|error| RejectedRestart {
            error: RestartError::Preparation(error),
            pending: Box::new(self),
        })
    }
}

#[cfg(test)]
mod tests;
