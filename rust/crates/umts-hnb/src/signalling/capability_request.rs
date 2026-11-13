// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CommonSignalling, EstablishmentPhase, SignallingError, TransactionId, transactions};
use umts_types::{UeKey, time::ChipInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityReadiness {
    Ready,
    Pending(TransactionId),
    Requested(TransactionId),
}

impl CommonSignalling {
    pub fn ensure_capabilities(
        &mut self,
        key: UeKey,
        now: ChipInstant,
    ) -> Result<CapabilityReadiness, SignallingError> {
        let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
        if now.epoch != key.epoch {
            return Err(SignallingError::Origin);
        }
        if context.setup.phase() != EstablishmentPhase::Established {
            return Err(SignallingError::Phase);
        }
        if context.capabilities.report().is_some() {
            return Ok(CapabilityReadiness::Ready);
        }
        if let Some(transaction) = context.capabilities.pending_transaction() {
            return Ok(CapabilityReadiness::Pending(transaction));
        }
        let transaction = context
            .transactions
            .next(transactions::MessageType::CapabilityEnquiry);
        self.request_capabilities(key, now, transaction)?;
        Ok(CapabilityReadiness::Requested(transaction))
    }
}

#[cfg(test)]
mod tests;
