// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{core_link::CoreLink, rab_handoff::PendingRabAssignment, rel99_plan::CompiledRel99};
use std::num::NonZeroUsize;
use umts_core::{Gateway, Transport};
use umts_iuh::{
    Domain,
    ranap::rab::{Cause, Failed, Released},
    session::{ControlTime, RabOperation, RabOutcome, RabRequest},
};
use umts_rrc::dedicated::NewRab;
use umts_types::CnDomain;

const INVALID_RAB_ID: u16 = 30;
const MAX_RBS: usize = 32;

#[cfg(test)]
mod tests;

pub(crate) trait ReleaseRabRecord {
    const AUTHORITATIVE: bool;

    fn identity(&self) -> u8;
    fn domain(&self) -> CnDomain;
    fn bearers(&self) -> &[u8];
    fn request(&self) -> Option<RabRequest>;
}

impl ReleaseRabRecord for NewRab {
    const AUTHORITATIVE: bool = false;

    fn identity(&self) -> u8 {
        self.identity
    }

    fn domain(&self) -> CnDomain {
        self.domain
    }

    fn bearers(&self) -> &[u8] {
        &self.bearers
    }

    fn request(&self) -> Option<RabRequest> {
        None
    }
}

impl ReleaseRabRecord for crate::rel99_admission::ActiveRab {
    const AUTHORITATIVE: bool = true;

    fn identity(&self) -> u8 {
        self.plan().rab
    }

    fn domain(&self) -> CnDomain {
        self.plan().domain
    }

    fn bearers(&self) -> &[u8] {
        &self.plan().bearers
    }

    fn request(&self) -> Option<RabRequest> {
        Some(self.request())
    }
}

#[derive(Debug)]
pub enum CoreReleaseError {
    Identity,
    Mapping,
    Capacity,
    NoRadioChange,
    Authority(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        umts_core::Error,
    ),
    Admission(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        AdmissionError,
    ),
}
pub struct RejectedRabRelease {
    pub error: CoreReleaseError,
    pub pending: Box<PendingRabAssignment>,
}
impl std::fmt::Debug for RejectedRabRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedRabRelease")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
pub struct RejectedCoreRelease {
    pub error: CoreReleaseError,
    pub request: Box<ScheduledReleaseRequest>,
    pub authority: Box<RabReleaseRequest>,
}
impl std::fmt::Debug for RejectedCoreRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedCoreRelease")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
enum Outcome {
    AwaitingRadio,
    Ready(RabOutcome),
    Recorded,
    Superseded,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(test)]
pub struct ReleaseOutcomeProgress {
    pub items: usize,
    pub recorded: usize,
    pub superseded: usize,
}
enum RadioDisposition {
    NotRequired,
    Pending(RadioTarget),
    Committed(RadioTarget),
}
struct RadioTarget {
    previous: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
}

#[must_use = "Retain the original request until its remaining setup outcomes are settled"]
pub struct SetupAfterRelease {
    pending: PendingRabAssignment,
}
impl SetupAfterRelease {
    pub fn request(&self) -> RabRequest {
        self.pending.request()
    }
    pub fn pending(&self) -> &PendingRabAssignment {
        &self.pending
    }
    pub(crate) fn into_pending(self) -> PendingRabAssignment {
        self.pending
    }
}
impl std::fmt::Debug for SetupAfterRelease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetupAfterRelease")
            .field("request", &self.request())
            .finish_non_exhaustive()
    }
}
struct Item {
    rab: u8,
    outcome: Outcome,
}
#[must_use]
pub struct RabReleaseRequest {
    pending: Box<PendingRabAssignment>,
    bearers: Vec<crate::dedicated_bearers::BearerId>,
    items: Vec<Item>,
    cursor: usize,
    radio: RadioDisposition,
}
impl std::fmt::Debug for RabReleaseRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RabReleaseRequest")
            .field("request", &self.request())
            .field(
                "previous",
                &self.radio_target().map(|target| target.previous),
            )
            .field("bearers", &self.bearers)
            .finish_non_exhaustive()
    }
}
impl RabReleaseRequest {
    fn radio_target(&self) -> Option<&RadioTarget> {
        match &self.radio {
            RadioDisposition::NotRequired => None,
            RadioDisposition::Pending(target) | RadioDisposition::Committed(target) => Some(target),
        }
    }
    pub fn request(&self) -> RabRequest {
        self.pending.request()
    }
    pub fn radio_bearers(&self) -> &[crate::dedicated_bearers::BearerId] {
        &self.bearers
    }
    pub fn needs_radio(&self) -> bool {
        !self.bearers.is_empty()
    }
    pub fn outcomes_recorded(&self) -> bool {
        self.items
            .iter()
            .all(|item| matches!(item.outcome, Outcome::Recorded | Outcome::Superseded))
    }
    #[cfg(test)]
    pub fn progress(&self) -> ReleaseOutcomeProgress {
        ReleaseOutcomeProgress {
            items: self.items.len(),
            recorded: self
                .items
                .iter()
                .filter(|item| matches!(item.outcome, Outcome::Recorded))
                .count(),
            superseded: self
                .items
                .iter()
                .filter(|item| matches!(item.outcome, Outcome::Superseded))
                .count(),
        }
    }
    pub fn has_setup(&self) -> bool {
        !self.pending.assignment().setup.is_empty()
    }
    pub(crate) fn into_pending(self) -> PendingRabAssignment {
        *self.pending
    }
    pub fn into_setup_after_release<C: Transport>(
        self,
        gateway: &Gateway<C>,
    ) -> Result<SetupAfterRelease, Self> {
        let request = self.request();
        if !self.has_setup()
            || !self.outcomes_recorded()
            || matches!(self.radio, RadioDisposition::Pending(_))
            || !self.pending.is_committed()
            || !gateway.same_rab_owner(request)
            || !gateway.rab_connection_current(request)
            || self.pending.assignment().setup.iter().any(|item| {
                gateway
                    .authorize_rab(request, item.first.id.get(), RabOperation::SetupOrModify)
                    .is_err()
            })
        {
            return Err(self);
        }
        Ok(SetupAfterRelease {
            pending: *self.pending,
        })
    }
    #[cfg(test)]
    pub(crate) fn prepare<C: Transport, R: ReleaseRabRecord>(
        pending: PendingRabAssignment,
        source: RabRequest,
        previous: ReceiveIdentity,
        plan: Arc<CompiledRel99>,
        inventory: &[R],
        gateway: &Gateway<C>,
    ) -> Result<Self, RejectedRabRelease> {
        Self::prepare_checked(pending, Some(source), previous, plan, inventory, gateway)
    }
    fn prepare_checked<C: Transport, R: ReleaseRabRecord>(
        mut pending: PendingRabAssignment,
        source: Option<RabRequest>,
        previous: ReceiveIdentity,
        plan: Arc<CompiledRel99>,
        inventory: &[R],
        gateway: &Gateway<C>,
    ) -> Result<Self, RejectedRabRelease> {
        let checked = (|| {
            let request = pending.request();
            if request.key() != previous.ue
                || source == Some(request)
                || pending.assignment().release.is_empty()
                || !gateway.rab_connection_current(request)
            {
                return Err(CoreReleaseError::Identity);
            }
            if !gateway.same_rab_owner(request) || !pending.confirm(gateway) {
                return Err(CoreReleaseError::Authority(umts_core::Error::Session(
                    umts_iuh::Error::Origin,
                )));
            }
            let domain = match request.domain() {
                Domain::Cs => CnDomain::Cs,
                Domain::Ps => CnDomain::Ps,
            };
            let mut membership = [false; MAX_RBS];
            for rab in inventory.iter().filter(|rab| rab.domain() == domain) {
                let active = rab
                    .bearers()
                    .iter()
                    .filter(|id| plan.configuration().bearer(**id).is_some())
                    .count();
                if active == 0 {
                    continue;
                }
                if active != rab.bearers().len() {
                    return Err(CoreReleaseError::Mapping);
                }
                for id in rab.bearers() {
                    let index = usize::from(*id);
                    if index >= MAX_RBS
                        || membership[index]
                        || plan
                            .configuration()
                            .bearer(*id)
                            .is_none_or(|rb| rb.domain != Some(domain))
                    {
                        return Err(CoreReleaseError::Mapping);
                    }
                    membership[index] = true;
                }
            }
            if plan
                .configuration()
                .bearers()
                .iter()
                .filter(|rb| rb.domain == Some(domain))
                .any(|rb| !membership.get(usize::from(rb.id)).copied().unwrap_or(false))
            {
                return Err(CoreReleaseError::Mapping);
            }
            let mut items = Vec::new();
            items
                .try_reserve_exact(pending.assignment().release.len())
                .map_err(|_| CoreReleaseError::Capacity)?;
            let mut bearers = Vec::new();
            bearers
                .try_reserve_exact(MAX_RBS)
                .map_err(|_| CoreReleaseError::Capacity)?;
            let mut selected = [false; MAX_RBS];
            for release in &pending.assignment().release {
                let rab = release.id.get();
                gateway
                    .authorize_rab(request, rab, RabOperation::Release)
                    .map_err(CoreReleaseError::Authority)?;
                let metadata = inventory
                    .iter()
                    .find(|item| item.identity() == rab && item.domain() == domain);
                let active = metadata.map_or(0, |metadata| {
                    metadata
                        .bearers()
                        .iter()
                        .filter(|id| plan.configuration().bearer(**id).is_some())
                        .count()
                });
                let outcome = if active == 0 {
                    // TS 25.413 8.2.2 reports an unknown release ID, without discarding valid sibling results.
                    Outcome::Ready(RabOutcome::ReleaseFailed(Failed::new(
                        rab,
                        Cause::from_code(INVALID_RAB_ID).expect("standard RANAP cause"),
                    )))
                } else {
                    let metadata = metadata.expect("active RAB metadata");
                    let owner = if R::AUTHORITATIVE {
                        metadata.request()
                    } else {
                        source
                    };
                    if owner.is_none_or(|owner| owner == request || !owner.same_connection(request))
                    {
                        return Err(CoreReleaseError::Identity);
                    }
                    if active != metadata.bearers().len() {
                        return Err(CoreReleaseError::Mapping);
                    }
                    for id in metadata.bearers() {
                        let rb = plan
                            .configuration()
                            .bearer(*id)
                            .ok_or(CoreReleaseError::Mapping)?;
                        let index = usize::from(*id);
                        if rb.domain != Some(domain) || index >= MAX_RBS || selected[index] {
                            return Err(CoreReleaseError::Mapping);
                        }
                        selected[index] = true;
                        bearers.push(
                            crate::dedicated_bearers::BearerId::new(*id)
                                .map_err(|_| CoreReleaseError::Mapping)?,
                        );
                    }
                    Outcome::AwaitingRadio
                };
                items.push(Item { rab, outcome });
            }
            Ok((items, bearers))
        })();
        match checked {
            Ok((items, bearers)) => Ok(Self {
                radio: if bearers.is_empty() {
                    RadioDisposition::NotRequired
                } else {
                    RadioDisposition::Pending(RadioTarget { previous, plan })
                },
                pending: Box::new(pending),
                bearers,
                items,
                cursor: 0,
            }),
            Err(error) => Err(RejectedRabRelease {
                error,
                pending: Box::new(pending),
            }),
        }
    }
    pub(super) fn authorized<C: Transport>(&self, gateway: &Gateway<C>) -> bool {
        self.items.iter().all(|item| {
            matches!(item.outcome, Outcome::Recorded)
                || gateway
                    .authorize_rab(self.request(), item.rab, RabOperation::Release)
                    .is_ok()
        })
    }
    fn validate_target<C: Transport>(
        &self,
        request: &ScheduledReleaseRequest,
        gateway: &Gateway<C>,
    ) -> Result<(), CoreReleaseError> {
        if !self.needs_radio() {
            return Err(CoreReleaseError::NoRadioChange);
        }
        let target = self.radio_target().ok_or(CoreReleaseError::NoRadioChange)?;
        if !self.authorized(gateway) {
            return Err(CoreReleaseError::Identity);
        }
        if request.pool.identity().ue != target.previous.ue
            || !Arc::ptr_eq(request.procedure.previous(), target.plan.configuration())
            || request.procedure.released().len() != self.bearers.len()
            || self
                .bearers
                .iter()
                .any(|id| !request.procedure.released().contains(&id.get()))
        {
            return Err(CoreReleaseError::Mapping);
        }
        Ok(())
    }
    pub(super) fn committed(&mut self) {
        self.radio = match std::mem::replace(&mut self.radio, RadioDisposition::NotRequired) {
            RadioDisposition::Pending(target) | RadioDisposition::Committed(target) => {
                RadioDisposition::Committed(target)
            }
            RadioDisposition::NotRequired => RadioDisposition::NotRequired,
        };
        for item in &mut self.items {
            if matches!(item.outcome, Outcome::AwaitingRadio) {
                item.outcome = Outcome::Ready(RabOutcome::Released(Released::new(item.rab)));
            }
        }
    }
    /// TS 25.413 8.2.2: each released PS RAB that requested data volume reporting includes
    /// the DL Data Volumes IE. `volume` returns that RAB's undelivered octets.
    pub fn attach_volumes(&mut self, mut volume: impl FnMut(u8) -> Option<u32>) {
        if self.request().domain() != umts_iuh::Domain::Ps {
            return;
        }
        for item in &mut self.items {
            if let Outcome::Ready(RabOutcome::Released(released)) = &mut item.outcome
                && released.volumes.is_none()
                && let Some(octets) = volume(item.rab)
            {
                released.volumes =
                    umts_iuh::ranap::rab::Volumes::new(vec![umts_iuh::ranap::rab::Volume::new(
                        octets, None,
                    )])
                    .ok();
            }
        }
    }
    pub fn record_ready<C: Transport>(
        &mut self,
        core: &mut CoreLink<C>,
        now: ControlTime,
        budget: NonZeroUsize,
    ) -> Result<usize, CoreReleaseError> {
        let mut recorded = 0;
        let request = self.request();
        if !core.gateway().same_rab_owner(request) {
            return Err(CoreReleaseError::Authority(umts_core::Error::Session(
                umts_iuh::Error::Origin,
            )));
        }
        for _ in 0..budget.get().min(self.items.len()) {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.items.len();
            let item = &mut self.items[index];
            if matches!(item.outcome, Outcome::Recorded | Outcome::Superseded) {
                continue;
            }
            let gateway = core.gateway();
            let current =
                if !gateway.owns_rab_request(request) || !gateway.rab_connection_current(request) {
                    false
                } else {
                    match gateway.authorize_rab(request, item.rab, RabOperation::Release) {
                        Ok(_) => true,
                        Err(umts_core::Error::Session(
                            umts_iuh::Error::Identity
                            | umts_iuh::Error::Origin
                            | umts_iuh::Error::State,
                        )) => false,
                        Err(error) => return Err(CoreReleaseError::Authority(error)),
                    }
                };
            if !current {
                item.outcome = Outcome::Superseded;
                continue;
            }
            let Outcome::Ready(outcome) = &item.outcome else {
                continue;
            };
            core.gateway_mut()
                .record_rab_outcome(now, request, outcome.clone())
                .map_err(CoreReleaseError::Authority)?;
            self.items[index].outcome = Outcome::Recorded;
            recorded += 1;
        }
        Ok(recorded)
    }
}
impl Rel99Admission {
    pub fn prepare_unmapped_release<C: Transport>(
        &self,
        mut pending: PendingRabAssignment,
        gateway: &Gateway<C>,
    ) -> Result<RabReleaseRequest, RejectedRabRelease> {
        let checked = (|| {
            let request = pending.request();
            if request.key().epoch != self.epoch
                || pending.assignment().release.is_empty()
                || !gateway.rab_connection_current(request)
            {
                return Err(CoreReleaseError::Identity);
            }
            if !gateway.same_rab_owner(request) || !pending.confirm(gateway) {
                return Err(CoreReleaseError::Authority(umts_core::Error::Session(
                    umts_iuh::Error::Origin,
                )));
            }
            let domain = match request.domain() {
                Domain::Cs => CnDomain::Cs,
                Domain::Ps => CnDomain::Ps,
            };
            for entry in self
                .entries
                .iter()
                .flatten()
                .filter(|entry| entry.identity.ue == request.key())
            {
                if entry.lifecycle.phase() != AdmissionPhase::Active
                    || entry.failure.is_some()
                    || entry.pending.is_some()
                    || !entry.lifecycle.is_idle()
                    || entry
                        .inventory
                        .as_ref()
                        .is_some_and(|inventory| inventory.initial_pending())
                {
                    return Err(CoreReleaseError::Admission(AdmissionError::Phase));
                }
                let configuration = entry
                    .pool
                    .as_ref()
                    .ok_or(CoreReleaseError::Mapping)?
                    .plan()
                    .configuration();
                if configuration
                    .bearers()
                    .iter()
                    .any(|bearer| bearer.domain == Some(domain))
                    || entry.inventory.as_ref().is_some_and(|inventory| {
                        inventory
                            .active()
                            .iter()
                            .any(|rab| rab.plan().domain == domain)
                    })
                {
                    return Err(CoreReleaseError::Mapping);
                }
            }
            let mut items = Vec::new();
            items
                .try_reserve_exact(pending.assignment().release.len())
                .map_err(|_| CoreReleaseError::Capacity)?;
            for release in &pending.assignment().release {
                gateway
                    .authorize_rab(request, release.id.get(), RabOperation::Release)
                    .map_err(CoreReleaseError::Authority)?;
                items.push(Item {
                    rab: release.id.get(),
                    outcome: Outcome::Ready(RabOutcome::ReleaseFailed(Failed::new(
                        release.id.get(),
                        Cause::from_code(INVALID_RAB_ID).expect("standard RANAP cause"),
                    ))),
                });
            }
            Ok(items)
        })();
        match checked {
            Ok(items) => Ok(RabReleaseRequest {
                pending: Box::new(pending),
                bearers: Vec::new(),
                items,
                cursor: 0,
                radio: RadioDisposition::NotRequired,
            }),
            Err(error) => Err(RejectedRabRelease {
                error,
                pending: Box::new(pending),
            }),
        }
    }
    pub fn prepare_core_release<C: Transport>(
        &self,
        previous: ReceiveIdentity,
        pending: PendingRabAssignment,
        gateway: &Gateway<C>,
    ) -> Result<RabReleaseRequest, RejectedRabRelease> {
        let Some(entry) = self.entry(previous) else {
            return Err(RejectedRabRelease {
                error: CoreReleaseError::Identity,
                pending: Box::new(pending),
            });
        };
        if entry.lifecycle.phase() != AdmissionPhase::Active || !entry.lifecycle.is_idle() {
            return Err(RejectedRabRelease {
                error: CoreReleaseError::Identity,
                pending: Box::new(pending),
            });
        }
        let Some(plan) = entry.pool.as_ref().map(|pool| pool.plan().clone()) else {
            return Err(RejectedRabRelease {
                error: CoreReleaseError::Identity,
                pending: Box::new(pending),
            });
        };
        if let Some(inventory) = entry.inventory.as_ref() {
            if inventory.initial_pending() {
                return Err(RejectedRabRelease {
                    error: CoreReleaseError::Identity,
                    pending: Box::new(pending),
                });
            }
            let domain = pending.request().domain();
            let source = inventory.connection_anchor(domain);
            return RabReleaseRequest::prepare_checked(
                pending,
                source,
                previous,
                plan,
                inventory.active(),
                gateway,
            );
        }
        let source = match pending.request().domain() {
            Domain::Cs => entry
                .circuit_assignment
                .as_ref()
                .map(CircuitAssignment::request),
            Domain::Ps => entry
                .packet_assignment
                .as_ref()
                .map(PacketAssignment::request),
        }
        .or_else(|| {
            entry
                .rab_assignment
                .as_ref()
                .map(|source| source.pending().request())
                .filter(|request| request.domain() == pending.request().domain())
        });
        RabReleaseRequest::prepare_checked(
            pending,
            source,
            previous,
            plan,
            entry.procedure.rabs(),
            gateway,
        )
    }
    pub fn begin_core_release<C: Transport>(
        &mut self,
        request: Box<ScheduledReleaseRequest>,
        authority: RabReleaseRequest,
        gateway: &Gateway<C>,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedCoreRelease> {
        let Some(target) = authority.radio_target() else {
            return Err(RejectedCoreRelease {
                error: CoreReleaseError::NoRadioChange,
                request,
                authority: Box::new(authority),
            });
        };
        let previous = target.previous;
        let check = authority.validate_target(&request, gateway).and_then(|()| {
            let entry = self.entry(previous).ok_or(CoreReleaseError::Identity)?;
            if !entry
                .pool
                .as_ref()
                .is_some_and(|pool| Arc::ptr_eq(pool.plan(), &target.plan))
            {
                return Err(CoreReleaseError::Mapping);
            }
            Ok(())
        });
        if let Err(error) = check {
            return Err(RejectedCoreRelease {
                error,
                request,
                authority: Box::new(authority),
            });
        }
        let authority = ReleaseOwner::Rab(authority);
        match self.begin_release_inner(previous, request, radio, receive, Some(&authority)) {
            Ok(identity) => {
                self.entry_mut(previous)
                    .expect("accepted core release")
                    .lifecycle
                    .release_mut()
                    .expect("scheduled owner")
                    .core = Some(authority);
                Ok(identity)
            }
            Err(rejected) => Err(RejectedCoreRelease {
                error: CoreReleaseError::Admission(rejected.error),
                request: rejected.request,
                authority: Box::new(authority.into_rab().expect("RAB release authority")),
            }),
        }
    }
    pub fn attach_release_volumes(
        &mut self,
        identity: ReceiveIdentity,
        volume: impl FnMut(u8) -> Option<u32>,
    ) {
        if let Some(core) = self
            .entry_mut(identity)
            .and_then(|entry| entry.lifecycle.release_mut())
            .and_then(|release| release.core.as_mut())
        {
            core.attach_volumes(volume);
        }
    }
    pub fn record_release_outcomes<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &mut CoreLink<C>,
        now: ControlTime,
        budget: NonZeroUsize,
    ) -> Result<usize, CoreReleaseError> {
        self.entry_mut(identity)
            .and_then(|entry| entry.lifecycle.release_mut())
            .and_then(|release| release.core.as_mut())
            .ok_or(CoreReleaseError::Identity)?
            .record_ready(core, now, budget)
    }
}
