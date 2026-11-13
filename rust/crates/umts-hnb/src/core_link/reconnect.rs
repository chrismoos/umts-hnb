// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU32;
use umts_core::{Gateway, LossReason, Phase, Transport};
use umts_iuh::session::ControlTime;

const MILLISECONDS_PER_SECOND: u64 = 1_000;

/// Opens a fresh HNB-GW association and session for the given control time and generation.
pub type Connector<T> = Box<dyn FnMut(ControlTime, u64) -> Result<Gateway<T>, String>>;

/// Replaces a closed HNB-GW association after a fixed backoff without touching the radio.
pub struct Reconnect<T: Transport> {
    connect: Connector<T>,
    limit: Option<NonZeroU32>,
    backoff_ms: u64,
    failures: u32,
    generation: u64,
    retry_at: Option<ControlTime>,
    exhausted: bool,
}

impl<T: Transport> Reconnect<T> {
    /// A `limit` of `None` retries indefinitely.
    pub fn new(limit: Option<NonZeroU32>, backoff_ms: u64, connect: Connector<T>) -> Self {
        Self {
            connect,
            limit,
            backoff_ms,
            failures: 0,
            generation: 0,
            retry_at: None,
            exhausted: false,
        }
    }

    pub fn exhausted(&self) -> bool {
        self.exhausted
    }

    #[cfg(test)]
    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub(super) fn service(
        &mut self,
        phase: Phase,
        loss: Option<LossReason>,
        now: ControlTime,
    ) -> Option<Gateway<T>> {
        if phase == Phase::Serving {
            if self.failures != 0 {
                tracing::info!(
                    "HNB-GW registration succeeded after {} reconnect attempts",
                    self.failures
                );
                self.failures = 0;
            }
            return None;
        }
        if phase != Phase::Closed || self.exhausted {
            return None;
        }
        let retry_at = match self.retry_at {
            Some(retry_at) => retry_at,
            None => {
                if self.limit.is_some_and(|limit| self.failures >= limit.get()) {
                    self.exhausted = true;
                    tracing::warn!(
                        "Giving up on the HNB-GW after {} reconnect attempts",
                        self.failures
                    );
                    return None;
                }
                self.failures = self.failures.saturating_add(1);
                let retry_at = ControlTime(now.0.saturating_add(self.backoff_ms));
                tracing::warn!(
                    "HNB-GW association closed ({loss:?}), reconnect attempt {} in {}s",
                    self.failures,
                    self.backoff_ms / MILLISECONDS_PER_SECOND
                );
                self.retry_at = Some(retry_at);
                retry_at
            }
        };
        if now < retry_at {
            return None;
        }
        self.retry_at = None;
        self.generation = self.generation.wrapping_add(1);
        match (self.connect)(now, self.generation) {
            Ok(gateway) => {
                tracing::info!("Reconnecting to the HNB-GW, attempt {}", self.failures);
                Some(gateway)
            }
            Err(error) => {
                tracing::warn!("HNB-GW reconnect attempt {} failed: {error}", self.failures);
                None
            }
        }
    }
}
