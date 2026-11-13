// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AssignmentResponse, Kind, PublishError, RabRequest, Session, domain_index};
use crate::{
    Domain, Error,
    ranap::rab::{Cause, Failed, Released, Setup},
    session::{ControlTime, Frame},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RabOutcome {
    Setup(Setup),
    Released(Released),
    SetupFailed(Failed),
    ReleaseFailed(Failed),
}
impl RabOutcome {
    pub fn id(&self) -> u8 {
        match self {
            Self::Setup(value) => value.id.get(),
            Self::Released(value) => value.id.get(),
            Self::SetupFailed(value) | Self::ReleaseFailed(value) => value.id.get(),
        }
    }
    fn kind(&self) -> Kind {
        match self {
            Self::Setup(_) | Self::SetupFailed(_) => Kind::Setup,
            Self::Released(_) | Self::ReleaseFailed(_) => Kind::Release,
        }
    }
    fn validate(&self, domain: Domain) -> Result<(), Error> {
        match self {
            Self::Setup(value) => {
                if let Some(association) = &value.association {
                    association.validate_domain(domain)?;
                }
            }
            Self::SetupFailed(value) | Self::ReleaseFailed(value) => {
                Cause::from_code(value.cause.code())?;
            }
            Self::Released(_) => (),
        }
        Ok(())
    }
    pub(super) fn matches_response(&self, response: &AssignmentResponse) -> bool {
        match self {
            Self::Setup(value) => response.setup.contains(value),
            Self::Released(value) => response.released.contains(value),
            Self::SetupFailed(value) => response.failed.contains(value),
            Self::ReleaseFailed(value) => response.release_failed.contains(value),
        }
    }
    fn append(&self, response: &mut AssignmentResponse) {
        match self {
            Self::Setup(value) => response.setup.push(value.clone()),
            Self::Released(value) => response.released.push(value.clone()),
            Self::SetupFailed(value) => response.failed.push(value.clone()),
            Self::ReleaseFailed(value) => response.release_failed.push(value.clone()),
        }
    }
}
impl Session {
    /// A success must describe completed radio admission and required user-plane initialization.
    pub fn record_rab_outcome(
        &mut self,
        request: RabRequest,
        outcome: RabOutcome,
    ) -> Result<(), Error> {
        self.record_rab_outcome_with(request, outcome, |_| Ok::<_, std::convert::Infallible>(()))
            .map_err(|error| match error {
                PublishError::Session(error) => error,
                PublishError::Downstream(never) => match never {},
            })
    }

    // The commit cannot mutate this Session, so validated authority remains current until recording.
    pub fn record_rab_outcome_with<T, E>(
        &mut self,
        request: RabRequest,
        outcome: RabOutcome,
        commit: impl FnOnce(super::RabAuthorization<'_>) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let pending = self.checked_rab(request).map_err(PublishError::Session)?;
        let item = pending
            .items
            .iter()
            .find(|item| item.id == outcome.id() && item.kind == outcome.kind())
            .ok_or(PublishError::Session(Error::Identity))?;
        if item.outcome.is_some() {
            return Err(PublishError::Session(Error::State));
        }
        outcome
            .validate(request.domain())
            .map_err(PublishError::Session)?;
        let operation = match outcome.kind() {
            Kind::Setup => super::RabOperation::SetupOrModify,
            Kind::Release => super::RabOperation::Release,
        };
        let authority = self
            .authorize_rab(request, outcome.id(), operation)
            .map_err(PublishError::Session)?;
        let committed = commit(authority).map_err(PublishError::Downstream)?;
        let pending = self
            .ues
            .get_mut(&request.key())
            .expect("validated UE")
            .rab_requests[domain_index(request.domain())]
        .iter_mut()
        .find(|entry| entry.request == request)
        .expect("validated request");
        let item = pending
            .items
            .iter_mut()
            .find(|item| item.id == outcome.id())
            .expect("validated RAB");
        item.outcome = Some(outcome);
        Ok(committed)
    }
    pub fn publish_ready_rab_with<T, E>(
        &mut self,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        self.ready(now).map_err(PublishError::Session)?;
        // TS 25.413 8.2.2 requires every requested RAB in the first response.
        let Some(pending) = self
            .ues
            .values()
            .flat_map(|ue| ue.rab_requests.iter().flatten())
            .filter(|pending| {
                !pending.items.is_empty() && pending.items.iter().all(|item| item.outcome.is_some())
            })
            .filter(|pending| self.check_rab_connection(pending.request).is_ok())
            .min_by_key(|pending| pending.request.serial())
        else {
            return Ok(None);
        };
        let request = pending.request;
        let mut response = AssignmentResponse::default();
        for item in &pending.items {
            item.outcome
                .as_ref()
                .expect("complete RAB outcomes")
                .append(&mut response);
        }
        self.publish_rab_with(request, &response, now, publish)
            .map(Some)
    }
}
