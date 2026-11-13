// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::ranap::rab::{Assignment, AssignmentResponse};
mod authority;
mod outcomes;
pub use authority::{RabAuthorization, RabOperation};
pub use outcomes::RabOutcome;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RabRequest {
    owner: NonZeroU64,
    association: AssociationId,
    key: UeKey,
    domain: Domain,
    context: ContextId,
    serial: NonZeroU64,
    connection: NonZeroU64,
}
impl RabRequest {
    pub(super) fn owned_connection(self, owner: NonZeroU64, connection: NonZeroU64) -> bool {
        self.owner == owner && self.connection == connection
    }
    pub(super) fn new(
        owner: NonZeroU64,
        association: AssociationId,
        key: UeKey,
        domain: Domain,
        context: ContextId,
        serial: NonZeroU64,
        connection: NonZeroU64,
    ) -> Self {
        Self {
            owner,
            association,
            key,
            domain,
            context,
            serial,
            connection,
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
    pub fn same_ue_session(self, other: Self) -> bool {
        self.owner == other.owner
            && self.association == other.association
            && self.key == other.key
            && self.context == other.context
    }
    pub fn same_connection(self, other: Self) -> bool {
        self.same_ue_session(other)
            && self.domain == other.domain
            && self.connection == other.connection
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Setup,
    Release,
}
struct Item {
    id: u8,
    kind: Kind,
    outcome: Option<RabOutcome>,
}
pub(super) struct PendingAssignment {
    request: RabRequest,
    items: Vec<Item>,
    reservation: Reservation,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reservation {
    Setup,
    Release,
}
impl PendingAssignment {
    pub(super) fn new(request: RabRequest, assignment: &Assignment) -> Result<Self, Error> {
        assignment.validate()?;
        for item in &assignment.setup {
            for (ie, error) in item
                .first
                .parameters
                .iter()
                .flat_map(|parameters| parameters.ignored_rate_extensions())
            {
                tracing::warn!(
                    "Ignoring erroneous RAB rate extension with criticality ignore, using root rates: rab={} ie={ie} error={error:?}",
                    item.first.id.get()
                );
            }
        }
        let items = assignment
            .setup
            .iter()
            .map(|item| Item {
                id: item.first.id.get(),
                kind: Kind::Setup,
                outcome: None,
            })
            .chain(assignment.release.iter().map(|item| Item {
                id: item.id.get(),
                kind: Kind::Release,
                outcome: None,
            }))
            .collect();
        let reservation = if assignment.release.is_empty() {
            Reservation::Setup
        } else {
            Reservation::Release
        };
        Ok(Self {
            request,
            items,
            reservation,
        })
    }
    fn cancels(&self, previous: &Item) -> bool {
        self.items
            .iter()
            .any(|item| item.kind == Kind::Release && item.id == previous.id)
    }
    pub(super) fn admit(
        &self,
        pending: &[Self],
        maximum: usize,
        release_reserve: usize,
    ) -> Result<(), Error> {
        let occupied = pending
            .iter()
            .filter(|request| {
                request.reservation == self.reservation
                    && request.items.iter().any(|item| !self.cancels(item))
            })
            .count();
        if occupied
            >= if self.reservation == Reservation::Release {
                release_reserve
            } else {
                maximum
            }
        {
            return Err(Error::Capacity);
        }
        Ok(())
    }
    pub(super) fn supersede(&self, pending: &mut Vec<Self>) {
        // TS 25.413 8.2 discards preceding nonqueued work for only the released RAB.
        for previous in pending.iter_mut() {
            previous.items.retain(|item| !self.cancels(item));
        }
        pending.retain(|request| !request.items.is_empty());
    }
    fn validate_response(&self, response: &AssignmentResponse) -> Result<(), Error> {
        response.validate()?;
        // Queuing needs a TQUEUING owner and subsequent outcomes, not a final response token.
        if !response.queued.is_empty() {
            return Err(Error::Unsupported);
        }
        let outcomes = response.setup.len()
            + response.released.len()
            + response.failed.len()
            + response.release_failed.len();
        if outcomes != self.items.len() {
            return Err(Error::Identity);
        }
        for item in &self.items {
            if item
                .outcome
                .as_ref()
                .is_some_and(|outcome| !outcome.matches_response(response))
            {
                return Err(Error::Identity);
            }
            let failure = match item.kind {
                Kind::Setup => response
                    .failed
                    .iter()
                    .find(|result| result.id.get() == item.id),
                Kind::Release => response
                    .release_failed
                    .iter()
                    .find(|result| result.id.get() == item.id),
            };
            if failure.is_none() {
                let success = match item.kind {
                    Kind::Setup => response
                        .setup
                        .iter()
                        .any(|result| result.id.get() == item.id),
                    Kind::Release => response
                        .released
                        .iter()
                        .any(|result| result.id.get() == item.id),
                };
                if !success {
                    return Err(Error::Identity);
                }
            }
        }
        Ok(())
    }
}

impl Session {
    /// The caller must complete radio admission and required Iu-UP initialization before reporting setup success.
    pub fn publish_rab_with<T, E>(
        &mut self,
        request: RabRequest,
        response: &AssignmentResponse,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let fail = PublishError::Session;
        self.ready(now).map_err(fail)?;
        self.checked_rab(request)
            .map_err(fail)?
            .validate_response(response)
            .map_err(fail)?;
        for setup in &response.setup {
            if let Some(association) = &setup.association {
                association.validate_domain(request.domain).map_err(fail)?
            }
        }
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
                ranap: response.encode(self.limits.codec).map_err(fail)?,
            }
            .encode(self.limits.codec)
            .map_err(fail)?,
        };
        let result = publish(frame).map_err(PublishError::Downstream)?;
        let pending = &mut self
            .ues
            .get_mut(&request.key)
            .expect("validated UE")
            .rab_requests[domain_index(request.domain)];
        let index = pending
            .iter()
            .position(|entry| entry.request == request)
            .expect("validated pending request");
        pending.remove(index);
        self.changed_at = now;
        Ok(result)
    }
}
