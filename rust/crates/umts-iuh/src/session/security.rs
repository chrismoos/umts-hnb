// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    AssociationId, ContextId, ControlTime, Domain, Error, Frame, FrameScope, LinkState, Protocol,
    PublishError, Registration, Session, domain_index, ranap, rua,
};
use std::num::NonZeroU64;
use umts_types::UeKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecurityRequest {
    association: AssociationId,
    key: UeKey,
    domain: Domain,
    context: ContextId,
    serial: NonZeroU64,
}

impl SecurityRequest {
    pub(super) fn new(
        association: AssociationId,
        key: UeKey,
        domain: Domain,
        context: ContextId,
        serial: NonZeroU64,
    ) -> Self {
        Self {
            association,
            key,
            domain,
            context,
            serial,
        }
    }
    pub fn association(self) -> AssociationId {
        self.association
    }
    pub fn key(self) -> UeKey {
        self.key
    }
    pub fn domain(self) -> Domain {
        self.domain
    }
    pub fn context(self) -> ContextId {
        self.context
    }
    pub fn serial(self) -> NonZeroU64 {
        self.serial
    }
}

#[derive(Clone, Copy)]
pub(super) struct PendingSecurity {
    request: SecurityRequest,
    integrity: u16,
    encryption: Option<u16>,
    keys: ranap::KeyStatus,
}

impl PendingSecurity {
    pub(super) fn new(
        request: SecurityRequest,
        command: &ranap::SecurityCommand,
    ) -> Result<Self, Error> {
        Ok(Self {
            request,
            integrity: algorithm_mask(&command.integrity.algorithms)?,
            encryption: command
                .encryption
                .as_ref()
                .map(|info| algorithm_mask(&info.algorithms))
                .transpose()?,
            keys: command.key_status,
        })
    }
}

fn algorithm_mask(algorithms: &[ranap::Algorithm]) -> Result<u16, Error> {
    const MAX_ALGORITHMS: usize = u16::BITS as usize;
    if algorithms.is_empty() || algorithms.len() > MAX_ALGORITHMS {
        return Err(Error::Unsupported);
    }
    algorithms.iter().try_fold(0u16, |mask, algorithm| {
        if usize::from(algorithm.0) >= MAX_ALGORITHMS {
            Err(Error::Unsupported)
        } else {
            Ok(mask | 1 << algorithm.0)
        }
    })
}

impl Session {
    pub fn security_response_cancelled_by_iu_release(&self, request: SecurityRequest) -> bool {
        request.association == self.association && self.ues.get(&request.key).is_some_and(|ue| {
            let slot = domain_index(request.domain);
            matches!(ue.registration, Registration::Active { context, cancelled: false } if context == request.context)
                && ue.domains[slot] == LinkState::Releasing
                && ue.release_requests[slot].is_some()
                && ue.security_requests[slot].is_some_and(|pending| pending.request == request)
        })
    }
    pub fn publish_security_with<T, E>(
        &mut self,
        request: SecurityRequest,
        response: ranap::SecurityResponse,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let fail = PublishError::Session;
        self.ready(now).map_err(fail)?;
        if request.association != self.association {
            return Err(fail(Error::Origin));
        }
        let ue = self.ues.get_mut(&request.key).ok_or(fail(Error::Origin))?;
        if !matches!(ue.registration, Registration::Active { context, cancelled: false } if context == request.context)
        {
            return Err(fail(Error::State));
        }
        let slot = domain_index(request.domain);
        if ue.domains[slot] != LinkState::Open {
            return Err(fail(Error::State));
        }
        let pending = ue.security_requests[slot]
            .filter(|pending| pending.request == request)
            .ok_or(fail(Error::Origin))?;
        let selected = match response {
            ranap::SecurityResponse::Complete {
                integrity,
                encryption,
            } => {
                if integrity.0 >= u16::BITS as u8 || pending.integrity & (1 << integrity.0) == 0 {
                    return Err(fail(Error::Unsupported));
                }
                match (pending.encryption, encryption) {
                    (None, None) => (),
                    (Some(mask), Some(algorithm))
                        if algorithm.0 < u16::BITS as u8 && mask & (1 << algorithm.0) != 0 => {}
                    _ => return Err(fail(Error::Unsupported)),
                }
                if ue.security_algorithms[slot].is_some() && pending.keys != ranap::KeyStatus::New {
                    return Err(fail(Error::State));
                }
                let alternative = (integrity, encryption.unwrap_or(ranap::Algorithm(0)));
                for other in ue.security_algorithms.iter().flatten() {
                    if (other.0, other.1.unwrap_or(ranap::Algorithm(0))) != alternative {
                        return Err(fail(Error::State));
                    }
                }
                Some((integrity, encryption))
            }
            ranap::SecurityResponse::Reject(_) => None,
        };
        let bytes = response.encode(self.limits.codec).map_err(fail)?;
        let frame = Frame {
            association: self.association,
            scope: FrameScope::Ue {
                key: request.key,
                domain: Some(request.domain),
            },
            protocol: Protocol::Rua,
            data: rua::Message::Direct {
                domain: request.domain,
                context: request.context,
                ranap: bytes,
            }
            .encode(self.limits.codec)
            .map_err(fail)?,
        };
        let result = publish(frame).map_err(PublishError::Downstream)?;
        ue.security_requests[slot] = None;
        if let Some(selected) = selected {
            ue.security_algorithms[slot] = Some(selected)
        }
        self.changed_at = now;
        Ok(result)
    }
}
