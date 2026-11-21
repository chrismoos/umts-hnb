// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{collections::VecDeque, num::NonZeroUsize};
use umts_core::{Gateway, Transport};
mod rejection;
pub use rejection::{FailedRabAssignment, FailureDispositionError, RejectionStep};
use umts_iuh::{
    ranap::rab::Assignment,
    session::{ControlTime, Event, RabAuthorization, RabOperation, RabRequest},
    wire::{Limits, Protocol},
};

const NOTIFY_ID_BYTES: usize = size_of::<u16>();
const MAX_PROTOCOL_IES: usize = u16::MAX as usize;

#[derive(Debug)]
pub enum HandoffError {
    Busy,
    Capacity,
    Protocol(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_iuh::Error,
    ),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssignmentKind {
    SetupOnly,
    ReleaseOnly,
    Mixed,
    Empty,
}

impl AssignmentKind {
    fn from_assignment(assignment: &Assignment) -> Self {
        match (assignment.setup.is_empty(), assignment.release.is_empty()) {
            (false, false) => Self::Mixed,
            (false, true) => Self::SetupOnly,
            (true, false) => Self::ReleaseOnly,
            (true, true) => Self::Empty,
        }
    }
}

struct EncodedAssignment {
    committed: bool,
    request: RabRequest,
    received_at: ControlTime,
    kind: AssignmentKind,
    setup_rabs: Vec<u8>,
    wire: Vec<u8>,
    notify_ies: Vec<u16>,
    outer_notifications: Vec<ProtocolNotifications>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ProtocolNotifications {
    protocol: Protocol,
    ids: Vec<u16>,
}

impl ProtocolNotifications {
    #[cfg(test)]
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    #[cfg(test)]
    pub fn ids(&self) -> &[u16] {
        &self.ids
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct PendingRabAssignment {
    committed: bool,
    request: RabRequest,
    received_at: ControlTime,
    kind: AssignmentKind,
    assignment: Assignment,
    notify_ies: Vec<u16>,
    outer_notifications: Vec<ProtocolNotifications>,
}

#[derive(Clone, Copy)]
pub struct SetupAssignment<'a> {
    pending: &'a PendingRabAssignment,
}
impl<'a> SetupAssignment<'a> {
    pub fn after_release(release: &'a crate::rel99_admission::SetupAfterRelease) -> Self {
        Self {
            pending: release.pending(),
        }
    }
    pub fn pending(&self) -> &'a PendingRabAssignment {
        self.pending
    }
    pub fn authorize_items<'g, T: Transport>(
        &self,
        gateway: &'g Gateway<T>,
    ) -> Result<Vec<RabAuthorization<'g>>, umts_core::Error> {
        let mut proofs = Vec::new();
        proofs
            .try_reserve_exact(self.pending.assignment.setup.len())
            .map_err(|_| umts_core::Error::Capacity)?;
        for item in &self.pending.assignment.setup {
            proofs.push(gateway.authorize_rab(
                self.pending.request,
                item.first.id.get(),
                RabOperation::SetupOrModify,
            )?);
        }
        Ok(proofs)
    }
}

impl std::fmt::Debug for PendingRabAssignment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingRabAssignment")
            .field("request", &self.request)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl PendingRabAssignment {
    pub fn setup_selection(&self) -> Option<SetupAssignment<'_>> {
        (!self.assignment.setup.is_empty() && self.assignment.release.is_empty())
            .then_some(SetupAssignment { pending: self })
    }
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    // This witness proves past Session commitment, not continued authority for any RAB item.
    pub fn confirm<T: Transport>(&mut self, gateway: &Gateway<T>) -> bool {
        if gateway.has_pending_rab_request(self.request) {
            self.committed = true;
        }
        self.committed
    }

    pub fn request(&self) -> RabRequest {
        self.request
    }

    pub fn kind(&self) -> AssignmentKind {
        self.kind
    }

    pub fn assignment(&self) -> &Assignment {
        &self.assignment
    }

    #[cfg(test)]
    pub fn notify_ies(&self) -> &[u16] {
        &self.notify_ies
    }

    #[cfg(test)]
    pub fn outer_notifications(&self) -> &[ProtocolNotifications] {
        &self.outer_notifications
    }

    // Session publication commits only after the CoreLink event callback accepts the event.
    #[cfg(test)]
    pub fn authorize_items<'a, T: Transport>(
        &self,
        gateway: &'a Gateway<T>,
    ) -> Result<Vec<RabAuthorization<'a>>, umts_core::Error> {
        let count = self
            .assignment
            .setup
            .len()
            .checked_add(self.assignment.release.len())
            .ok_or(umts_core::Error::Capacity)?;
        let mut proofs = Vec::new();
        proofs
            .try_reserve_exact(count)
            .map_err(|_| umts_core::Error::Capacity)?;
        for item in &self.assignment.setup {
            proofs.push(gateway.authorize_rab(
                self.request,
                item.first.id.get(),
                RabOperation::SetupOrModify,
            )?);
        }
        for item in &self.assignment.release {
            proofs.push(gateway.authorize_rab(
                self.request,
                item.id.get(),
                RabOperation::Release,
            )?);
        }
        Ok(proofs)
    }
}

pub struct RabHandoff {
    limits: Limits,
    pending: VecDeque<EncodedAssignment>,
    capacity: NonZeroUsize,
}

impl RabHandoff {
    #[cfg(test)]
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            pending: VecDeque::new(),
            capacity: NonZeroUsize::MIN,
        }
    }

    pub fn with_capacity(limits: Limits, capacity: NonZeroUsize) -> Result<Self, HandoffError> {
        capacity
            .get()
            .checked_mul(limits.maximum_packet_bytes)
            .ok_or(HandoffError::Capacity)?;
        let mut pending = VecDeque::new();
        pending
            .try_reserve_exact(capacity.get())
            .map_err(|_| HandoffError::Capacity)?;
        Ok(Self {
            limits,
            pending,
            capacity,
        })
    }

    pub fn is_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub(crate) fn has_ue(&self, key: umts_types::UeKey) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.request.key() == key)
    }

    pub fn confirm<T: Transport>(&mut self, gateway: &Gateway<T>) -> usize {
        let mut confirmed = 0;
        for pending in &mut self.pending {
            if !pending.committed && gateway.has_pending_rab_request(pending.request) {
                pending.committed = true;
                confirmed += 1;
            }
        }
        confirmed
    }

    pub fn offer(&mut self, event: &Event) -> Result<bool, HandoffError> {
        let original = event;
        let mut base = event;
        let mut envelope_count = 0usize;
        let mut outer_ids = 0usize;
        while let Event::Notified {
            notify_ies, event, ..
        } = base
        {
            envelope_count = envelope_count
                .checked_add(1)
                .ok_or(HandoffError::Capacity)?;
            outer_ids = outer_ids
                .checked_add(notify_ies.len())
                .ok_or(HandoffError::Capacity)?;
            if envelope_count > self.limits.maximum_ies || outer_ids > self.limits.maximum_ies {
                return Err(HandoffError::Capacity);
            }
            base = event;
        }
        let Event::RabAssignment {
            request,
            received_at,
            assignment,
            notify_ies,
        } = base
        else {
            return Ok(false);
        };
        if self.pending.len() == self.capacity.get() {
            return Err(HandoffError::Busy);
        }
        let count = assignment
            .setup
            .len()
            .checked_add(assignment.release.len())
            .ok_or(HandoffError::Capacity)?;
        let kind = AssignmentKind::from_assignment(assignment);
        if self.limits.maximum_packet_bytes == 0
            || self.limits.maximum_value_bytes == 0
            || self.limits.maximum_ies == 0
            || self.limits.maximum_ies > MAX_PROTOCOL_IES
            || count > self.limits.maximum_ies
            || notify_ies
                .len()
                .checked_add(outer_ids)
                .is_none_or(|count| count > self.limits.maximum_ies)
        {
            return Err(HandoffError::Capacity);
        }
        let wire = assignment
            .encode(self.limits)
            .map_err(HandoffError::Protocol)?;
        let retained_bytes = notify_ies
            .len()
            .checked_add(outer_ids)
            .ok_or(HandoffError::Capacity)?
            .checked_mul(NOTIFY_ID_BYTES)
            .and_then(|bytes| bytes.checked_add(wire.len()))
            .and_then(|bytes| bytes.checked_add(assignment.setup.len()))
            .and_then(|bytes| {
                envelope_count
                    .checked_mul(size_of::<ProtocolNotifications>())
                    .and_then(|overhead| bytes.checked_add(overhead))
            })
            .ok_or(HandoffError::Capacity)?;
        if retained_bytes > self.limits.maximum_packet_bytes {
            return Err(HandoffError::Capacity);
        }
        let mut notify = Vec::new();
        notify
            .try_reserve_exact(notify_ies.len())
            .map_err(|_| HandoffError::Capacity)?;
        notify.extend_from_slice(notify_ies);
        let mut outer_notifications = Vec::new();
        outer_notifications
            .try_reserve_exact(envelope_count)
            .map_err(|_| HandoffError::Capacity)?;
        let mut envelope = original;
        while let Event::Notified {
            protocol,
            notify_ies,
            event,
        } = envelope
        {
            let mut ids = Vec::new();
            ids.try_reserve_exact(notify_ies.len())
                .map_err(|_| HandoffError::Capacity)?;
            ids.extend_from_slice(notify_ies);
            outer_notifications.push(ProtocolNotifications {
                protocol: *protocol,
                ids,
            });
            envelope = event;
        }
        let mut setup_rabs = Vec::new();
        setup_rabs
            .try_reserve_exact(assignment.setup.len())
            .map_err(|_| HandoffError::Capacity)?;
        setup_rabs.extend(assignment.setup.iter().map(|item| item.first.id.get()));
        self.pending.push_back(EncodedAssignment {
            committed: false,
            request: *request,
            received_at: *received_at,
            kind,
            setup_rabs,
            wire,
            notify_ies: notify,
            outer_notifications,
        });
        Ok(true)
    }

    pub(crate) fn reoffer(&mut self, pending: &PendingRabAssignment) -> Result<(), HandoffError> {
        if self.pending.len() == self.capacity.get() {
            return Err(HandoffError::Capacity);
        }
        let wire = pending
            .assignment
            .encode(self.limits)
            .map_err(HandoffError::Protocol)?;
        let mut setup_rabs = Vec::new();
        setup_rabs
            .try_reserve_exact(pending.assignment.setup.len())
            .map_err(|_| HandoffError::Capacity)?;
        setup_rabs.extend(
            pending
                .assignment
                .setup
                .iter()
                .map(|item| item.first.id.get()),
        );
        self.pending.push_back(EncodedAssignment {
            committed: true,
            request: pending.request,
            received_at: pending.received_at,
            kind: pending.kind,
            setup_rabs,
            wire,
            notify_ies: Vec::new(),
            outer_notifications: Vec::new(),
        });
        Ok(())
    }

    #[cfg(test)]
    pub fn take(&mut self) -> Result<Option<PendingRabAssignment>, HandoffError> {
        self.take_ready(|_| true)
    }

    /// At most one full bounded queue is examined per call.
    #[cfg(test)]
    pub fn take_ready(
        &mut self,
        mut ready: impl FnMut(RabRequest) -> bool,
    ) -> Result<Option<PendingRabAssignment>, HandoffError> {
        self.take_selected(false, |request, _, _| ready(request))
    }

    #[cfg(test)]
    pub fn take_committed_ready(
        &mut self,
        mut ready: impl FnMut(RabRequest) -> bool,
    ) -> Result<Option<PendingRabAssignment>, HandoffError> {
        self.take_selected(true, |request, _, _| ready(request))
    }

    #[cfg(test)]
    pub fn take_committed_matching(
        &mut self,
        mut ready: impl FnMut(RabRequest, AssignmentKind) -> bool,
    ) -> Result<Option<PendingRabAssignment>, HandoffError> {
        self.take_selected(true, |request, kind, _| ready(request, kind))
    }

    pub(crate) fn take_committed_matching_rabs(
        &mut self,
        ready: impl FnMut(RabRequest, AssignmentKind, &[u8]) -> bool,
    ) -> Result<Option<PendingRabAssignment>, HandoffError> {
        self.take_selected(true, ready)
    }

    fn take_selected(
        &mut self,
        require_committed: bool,
        mut ready: impl FnMut(RabRequest, AssignmentKind, &[u8]) -> bool,
    ) -> Result<Option<PendingRabAssignment>, HandoffError> {
        let Some(index) = self.pending.iter().position(|pending| {
            (!require_committed || pending.committed)
                && ready(pending.request, pending.kind, &pending.setup_rabs)
        }) else {
            return Ok(None);
        };
        let pending = &self.pending[index];
        let decoded =
            Assignment::decode(&pending.wire, self.limits).map_err(HandoffError::Protocol)?;
        let pending = self
            .pending
            .remove(index)
            .expect("decoded pending assignment");
        Ok(Some(PendingRabAssignment {
            committed: pending.committed,
            request: pending.request,
            received_at: pending.received_at,
            kind: pending.kind,
            assignment: decoded.assignment,
            notify_ies: pending.notify_ies,
            outer_notifications: pending.outer_notifications,
        }))
    }
}

#[cfg(test)]
mod tests;
