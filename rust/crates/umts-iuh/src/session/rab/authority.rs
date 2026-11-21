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
    /// Whether an Iu release has started for the connection of this request. TS 25.413 8.1
    /// gives the release precedence, so no response is sent for the request.
    pub fn rab_superseded(&self, request: RabRequest) -> bool {
        self.ues
            .get(&request.key)
            .is_some_and(|ue| ue.domains[domain_index(request.domain)] == LinkState::Releasing)
    }
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
        let slot = domain_index(request.domain);
        // A request that survived the start of a release is one the UE is already completing.
        let open = match ue.domains[slot] {
            LinkState::Open => true,
            LinkState::Releasing => ue.rab_requests[slot]
                .iter()
                .any(|pending| pending.request == request),
            LinkState::Closed => false,
        };
        if !matches!(ue.registration,Registration::Active {context,cancelled:false} if context==request.context)
            || !open
            || ue.connections[slot] != request.connection.get()
        {
            return Err(Error::State);
        }
        Ok(())
    }
    /// Puts back a request the core already had answered, so the radio side can set the same
    /// bearers up again after the UE was parked. TS 25.413 has no message for this: the RABs
    /// never left the core, so no RAB ASSIGNMENT RESPONSE is sent for the request.
    pub fn reinstate_rab(
        &mut self,
        request: RabRequest,
        assignment: &crate::ranap::rab::Assignment,
    ) -> Result<(), Error> {
        self.check_rab_connection(request)?;
        let mut pending = super::PendingAssignment::new(request, assignment)?;
        pending.local = true;
        let requests = &mut self
            .ues
            .get_mut(&request.key)
            .expect("validated UE")
            .rab_requests[domain_index(request.domain)];
        if requests.iter().any(|entry| entry.request == request) {
            return Err(Error::State);
        }
        requests.push(pending);
        Ok(())
    }
    /// Records that the UE has been sent the reconfiguration for this request. TS 25.331
    /// has no way to withdraw it, so the request stays valid through a later Iu release
    /// until the radio procedure ends. No response is sent for it once the release starts.
    pub fn mark_rab_in_flight(&mut self, request: RabRequest) -> bool {
        if self.checked_rab(request).is_err() {
            return false;
        }
        let Some(pending) = self.ues.get_mut(&request.key).and_then(|ue| {
            ue.rab_requests[domain_index(request.domain)]
                .iter_mut()
                .find(|pending| pending.request == request)
        }) else {
            return false;
        };
        pending.in_flight = true;
        true
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
