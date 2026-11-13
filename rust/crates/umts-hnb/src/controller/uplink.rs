// SPDX-License-Identifier: AGPL-3.0-or-later

use super::UplinkFailure;
use crate::{dedicated_bearers::SduDelivery, rel99_admission::DuplexFramePolicy};

#[cfg(test)]
mod tests;

pub(super) struct UplinkOutcomes<E> {
    pub first: Option<UplinkFailure<E>>,
    pub failures: usize,
}

impl<E> Default for UplinkOutcomes<E> {
    fn default() -> Self {
        Self {
            first: None,
            failures: 0,
        }
    }
}

impl<E> UplinkOutcomes<E> {
    pub(super) fn deliver<P: DuplexFramePolicy<UplinkError = E>>(
        &mut self,
        policy: &mut P,
        delivery: SduDelivery<'_>,
    ) -> bool {
        let metadata = delivery.metadata;
        match policy.uplink(delivery) {
            Ok(accepted) => accepted,
            Err(error) => {
                self.failures = self.failures.saturating_add(1);
                if self.first.is_none() {
                    self.first = Some(UplinkFailure { metadata, error });
                }
                false
            }
        }
    }
}
