// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use transactions::MessageType;
use umts_security::contexts::KeyOrigin;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityAction {
    CapabilitiesRequested {
        key: UeKey,
        domain: umts_iuh::Domain,
        transaction: TransactionId,
    },
    CapabilitiesPending {
        key: UeKey,
        domain: umts_iuh::Domain,
        transaction: TransactionId,
    },
    CommandPublished {
        key: UeKey,
        domain: umts_iuh::Domain,
        transaction: TransactionId,
        origin: KeyOrigin,
    },
    Rejected {
        key: UeKey,
        domain: umts_iuh::Domain,
        cause: umts_iuh::ranap::SecurityReject,
    },
    Deferred {
        key: UeKey,
        domain: umts_iuh::Domain,
    },
    RecoveryRequired {
        key: UeKey,
        origin: Option<KeyOrigin>,
    },
    Observed(SecurityWork),
}

impl CommonSignalling {
    pub fn actuate_security(
        &mut self,
        work: SecurityWork,
        radio_now: ChipInstant,
        now: SecurityTime,
    ) -> Result<SecurityAction, SignallingError> {
        if radio_now.epoch != self.epoch() {
            return Err(SignallingError::Origin);
        }
        match work {
            SecurityWork::NeedCapabilities { key, domain } => {
                tracing::trace!(
                    "Security mode waits for UE capabilities: key={key:?} domain={domain:?}"
                );
                let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
                if context.security.phase(domain) != connection_security::Phase::AwaitingPreparation
                {
                    return Err(SignallingError::Phase);
                }
                if let Some(transaction) = context.capabilities.pending_transaction() {
                    return Ok(SecurityAction::CapabilitiesPending {
                        key,
                        domain,
                        transaction,
                    });
                }
                let transaction = context.transactions.next(MessageType::CapabilityEnquiry);
                match self.request_capabilities(key, radio_now, transaction) {
                    Ok(()) => Ok(SecurityAction::CapabilitiesRequested {
                        key,
                        domain,
                        transaction,
                    }),
                    Err(error) if scheduling::backpressure(&error) => {
                        Ok(SecurityAction::Deferred { key, domain })
                    }
                    Err(error) => Err(error),
                }
            }
            SecurityWork::ReadyToPrepare { key, domain } => {
                let context = self.registry.get(key).ok_or(SignallingError::Origin)?;
                if !context.security.can_prepare(domain) {
                    tracing::trace!(
                        "Security mode preparation deferred: key={key:?} domain={domain:?} cs={:?} ps={:?} unconfirmed_completion={:?} delivery={} status_blocked={}",
                        context.security.phase(umts_iuh::Domain::Cs),
                        context.security.phase(umts_iuh::Domain::Ps),
                        context.security.unconfirmed_origin(),
                        context.protection.delivery_label(),
                        context.protection.blocks_status()
                    );
                    return Ok(SecurityAction::Deferred { key, domain });
                }
                let transaction = context.transactions.next(MessageType::SecurityModeCommand);
                match self.prepare_common_security(key, domain, now, transaction) {
                    Ok(connection_security::Preparation::Prepared(_)) => {
                        self.publish_prepared_security(key, domain, now)
                    }
                    Ok(connection_security::Preparation::Rejected(cause)) => {
                        tracing::warn!(
                            "Security mode rejected while preparing: key={key:?} domain={domain:?} cause={cause:?}"
                        );
                        Ok(SecurityAction::Rejected { key, domain, cause })
                    }
                    Err(error) if scheduling::backpressure(&error) => {
                        Ok(SecurityAction::Deferred { key, domain })
                    }
                    Err(error) => Err(error),
                }
            }
            SecurityWork::CommandReady { key, domain } => {
                self.publish_prepared_security(key, domain, now)
            }
            SecurityWork::AbortRequired {
                key,
                domain,
                origin,
            } => {
                let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
                if context
                    .security
                    .timeout_action(domain, now)
                    .map_err(security_error)?
                    != Some(origin)
                {
                    return Err(SignallingError::Phase);
                }
                if context.security.phase(domain) != connection_security::Phase::Prepared {
                    let first = !context.protection.blocks_status();
                    context.protection.require_recovery(None);
                    if first {
                        tracing::warn!(
                            "Security mode command unanswered, recovery required: key={key:?} domain={domain:?} phase={:?}",
                            context.security.phase(domain)
                        );
                    }
                    return Ok(SecurityAction::RecoveryRequired {
                        key,
                        origin: Some(origin),
                    });
                }
                tracing::warn!(
                    "Security mode command discarded before publication: key={key:?} domain={domain:?}"
                );
                context
                    .security
                    .discard_prepared(domain, origin)
                    .map_err(security_error)?;
                context
                    .security
                    .timeout_action(domain, now)
                    .map_err(security_error)?;
                Ok(SecurityAction::Rejected {
                    key,
                    domain,
                    cause: umts_iuh::ranap::SecurityReject::RadioProcedureFailure,
                })
            }
            SecurityWork::RecoveryRequired { key } => {
                Ok(SecurityAction::RecoveryRequired { key, origin: None })
            }
            other => Ok(SecurityAction::Observed(other)),
        }
    }

    fn publish_prepared_security(
        &mut self,
        key: UeKey,
        domain: umts_iuh::Domain,
        now: SecurityTime,
    ) -> Result<SecurityAction, SignallingError> {
        let security = &self
            .registry
            .get(key)
            .ok_or(SignallingError::Origin)?
            .security;
        let origin = security
            .pending_origin(domain)
            .ok_or(SignallingError::Phase)?;
        let transaction = security.transaction(domain).ok_or(SignallingError::Phase)?;
        match self.publish_security_command(key, domain, now) {
            Ok(()) => Ok(SecurityAction::CommandPublished {
                key,
                domain,
                transaction,
                origin,
            }),
            Err(error) if scheduling::backpressure(&error) => {
                // Nothing was admitted. Rebuild unsent activation points on retry without extending the core deadline.
                self.registry
                    .get_mut(key)
                    .expect("prepared owner")
                    .security
                    .discard_prepared(domain, origin)
                    .map_err(security_error)?;
                Ok(SecurityAction::Deferred { key, domain })
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests;
