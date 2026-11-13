// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    Error, HnbState, Kind, LinkState, PendingAssignment, RabRequest, Registration, Session,
    domain_index,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RabOperation {
    SetupOrModify,
    Release,
}

// The shared session borrow prevents cancellation or reuse while this authorization is consumed.
#[derive(Clone, Copy)]
pub struct RabAuthorization<'a> {
    _session: &'a Session,
    request: RabRequest,
    rab: u8,
    operation: RabOperation,
}
impl std::fmt::Debug for RabAuthorization<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RabAuthorization")
            .field("request", &self.request)
            .field("rab", &self.rab)
            .field("operation", &self.operation)
            .finish()
    }
}
impl RabAuthorization<'_> {
    pub fn request(self) -> RabRequest {
        self.request
    }
    pub fn rab(self) -> u8 {
        self.rab
    }
    pub fn operation(self) -> RabOperation {
        self.operation
    }
}
impl Session {
    pub fn same_rab_owner(&self, request: RabRequest) -> bool {
        request.owner == self.owner
    }
    pub fn has_pending_rab_request(&self, request: RabRequest) -> bool {
        self.checked_rab(request).is_ok()
    }
    pub fn owns_rab_request(&self, request: RabRequest) -> bool {
        request.owner == self.owner && request.association == self.association
    }
    pub fn check_rab_connection(&self, request: RabRequest) -> Result<(), Error> {
        if !self.owns_rab_request(request) {
            return Err(Error::Origin);
        }
        if !matches!(self.state, HnbState::Registered { .. }) {
            return Err(Error::State);
        }
        let ue = self.ues.get(&request.key).ok_or(Error::Origin)?;
        if !matches!(ue.registration,Registration::Active {context,cancelled:false} if context==request.context)
            || ue.domains[domain_index(request.domain)] != LinkState::Open
            || ue.connections[domain_index(request.domain)] != request.connection.get()
        {
            return Err(Error::State);
        }
        Ok(())
    }
    pub(super) fn checked_rab(&self, request: RabRequest) -> Result<&PendingAssignment, Error> {
        self.check_rab_connection(request)?;
        self.ues
            .get(&request.key)
            .expect("validated UE")
            .rab_requests[domain_index(request.domain)]
        .iter()
        .find(|pending| pending.request == request)
        .ok_or(Error::Origin)
    }
    pub fn authorize_rab(
        &self,
        request: RabRequest,
        rab: u8,
        operation: RabOperation,
    ) -> Result<RabAuthorization<'_>, Error> {
        let pending = self.checked_rab(request)?;
        let kind = match operation {
            RabOperation::SetupOrModify => Kind::Setup,
            RabOperation::Release => Kind::Release,
        };
        if !pending
            .items
            .iter()
            .any(|item| item.id == rab && item.kind == kind && item.outcome.is_none())
        {
            return Err(Error::Identity);
        }
        Ok(RabAuthorization {
            _session: self,
            request,
            rab,
            operation,
        })
    }
}
