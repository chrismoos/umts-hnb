// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_core::{Gateway, Transport};
use umts_iuh::session::IuReleaseRequest;

pub(crate) enum ReleaseOwner {
    Rab(RabReleaseRequest),
    Iu(IuRadioRelease),
}

pub struct IuRadioRelease {
    request: IuReleaseRequest,
    previous: ReceiveIdentity,
    plan: Arc<crate::rel99_plan::CompiledRel99>,
    bearers: Vec<crate::dedicated_bearers::BearerId>,
}

impl IuRadioRelease {
    pub fn radio_bearers(&self) -> &[crate::dedicated_bearers::BearerId] {
        &self.bearers
    }
}

impl ReleaseOwner {
    pub(super) fn rab(&self) -> Option<&RabReleaseRequest> {
        match self {
            Self::Rab(owner) => Some(owner),
            Self::Iu(_) => None,
        }
    }
    pub(super) fn into_rab(self) -> Option<RabReleaseRequest> {
        match self {
            Self::Rab(owner) => Some(owner),
            Self::Iu(_) => None,
        }
    }
    pub(super) fn iu(&self) -> Option<IuReleaseRequest> {
        match self {
            Self::Iu(owner) => Some(owner.request),
            Self::Rab(_) => None,
        }
    }
    pub(super) fn authorized<C: Transport>(&self, gateway: &Gateway<C>) -> bool {
        match self {
            Self::Rab(owner) => owner.authorized(gateway),
            Self::Iu(owner) => gateway.owns_iu_release(owner.request),
        }
    }
    pub(super) fn outcomes_recorded(&self) -> bool {
        self.rab().is_none_or(RabReleaseRequest::outcomes_recorded)
    }
    pub(super) fn committed(&mut self) {
        if let Self::Rab(owner) = self {
            owner.committed();
        }
    }
    pub(super) fn attach_volumes(&mut self, volume: impl FnMut(u8) -> Option<u32>) {
        if let Self::Rab(owner) = self {
            owner.attach_volumes(volume);
        }
    }
    pub(super) fn record_ready<C: Transport>(
        &mut self,
        core: &mut crate::core_link::CoreLink<C>,
        now: umts_iuh::session::ControlTime,
        budget: std::num::NonZeroUsize,
    ) -> Result<usize, CoreReleaseError> {
        match self {
            Self::Rab(owner) => owner.record_ready(core, now, budget),
            Self::Iu(_) => Ok(0),
        }
    }
}

impl Rel99Admission {
    pub(crate) fn attach_iu_release_delivery(
        &mut self,
        previous: ReceiveIdentity,
        delivery: ReleaseDelivery,
    ) {
        self.entry_mut(previous)
            .expect("admitted Iu owner")
            .lifecycle
            .release_mut()
            .expect("scheduled release")
            .delivery = Some(delivery);
    }
    pub fn has_ue(&self, key: umts_types::UeKey) -> bool {
        self.entries
            .get(key.slot as usize)
            .and_then(Option::as_ref)
            .is_some_and(|entry| entry.identity.ue == key)
    }
    pub fn transition_in_flight(&self, previous: ReceiveIdentity) -> bool {
        self.entry(previous).is_some_and(|entry| {
            entry.lifecycle.phase() == AdmissionPhase::Active
                && entry.failure.is_none()
                && !entry.lifecycle.is_idle()
        })
    }

    pub fn prepare_iu_radio_release<C: Transport>(
        &self,
        previous: ReceiveIdentity,
        request: IuReleaseRequest,
        gateway: &Gateway<C>,
    ) -> Result<IuRadioRelease, CoreReleaseError> {
        if request.key() != previous.ue || !gateway.owns_iu_release(request) {
            return Err(CoreReleaseError::Identity);
        }
        let entry = self.entry(previous).ok_or(CoreReleaseError::Identity)?;
        if entry.lifecycle.phase() != AdmissionPhase::Active
            || entry.failure.is_some()
            || !entry.lifecycle.is_idle()
            || entry
                .release_history
                .as_ref()
                .is_some_and(|history| history.iu.is_some())
        {
            return Err(CoreReleaseError::Admission(AdmissionError::Phase));
        }
        let plan = entry
            .pool
            .as_ref()
            .ok_or(CoreReleaseError::Mapping)?
            .plan()
            .clone();
        let inventory = entry.inventory.as_ref().ok_or(CoreReleaseError::Mapping)?;
        let mut bearers = Vec::new();
        bearers
            .try_reserve_exact(plan.configuration().bearers().len())
            .map_err(|_| CoreReleaseError::Capacity)?;
        for active in inventory.active() {
            if active.plan().domain != request.domain().into() {
                continue;
            }
            if !request.same_connection(active.request()) {
                return Err(CoreReleaseError::Identity);
            }
            for &id in &active.plan().bearers {
                let bearer = crate::dedicated_bearers::BearerId::new(id)
                    .map_err(|_| CoreReleaseError::Mapping)?;
                if bearers.contains(&bearer)
                    || plan
                        .configuration()
                        .bearer(id)
                        .is_none_or(|rb| rb.domain != Some(request.domain().into()))
                {
                    return Err(CoreReleaseError::Mapping);
                }
                bearers.push(bearer);
            }
        }
        if plan.configuration().bearers().iter().any(|rb| {
            rb.domain == Some(request.domain().into())
                && !bearers.iter().any(|id| id.get() == rb.id)
        }) {
            return Err(CoreReleaseError::Mapping);
        }
        Ok(IuRadioRelease {
            request,
            previous,
            plan,
            bearers,
        })
    }

    pub fn completed_iu_radio_release(&self, request: IuReleaseRequest) -> bool {
        self.entries
            .get(request.key().slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.identity.ue == request.key())
            .and_then(|entry| entry.release_history.as_ref())
            .is_some_and(|history| history.iu == Some(request))
    }

    pub fn take_completed_iu_radio_release(&mut self, request: IuReleaseRequest) -> bool {
        let Some(history) = self
            .entries
            .get_mut(request.key().slot as usize)
            .and_then(Option::as_mut)
            .filter(|entry| entry.identity.ue == request.key())
            .and_then(|entry| entry.release_history.as_mut())
        else {
            return false;
        };
        if history.iu != Some(request) {
            return false;
        }
        history.iu = None;
        true
    }

    pub fn begin_iu_radio_release<C: Transport>(
        &mut self,
        request: Box<ScheduledReleaseRequest>,
        authority: IuRadioRelease,
        gateway: &Gateway<C>,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedScheduledRelease> {
        let previous = authority.previous;
        if !gateway.owns_iu_release(authority.request)
            || !Arc::ptr_eq(request.procedure.previous(), authority.plan.configuration())
            || request.procedure.released().len() != authority.bearers.len()
            || authority
                .bearers
                .iter()
                .any(|rb| !request.procedure.released().contains(&rb.get()))
        {
            return Err(RejectedScheduledRelease {
                error: AdmissionError::Identity,
                request,
            });
        }
        let authority = ReleaseOwner::Iu(authority);
        let identity =
            self.begin_release_inner(previous, request, radio, receive, Some(&authority))?;
        self.entry_mut(previous)
            .expect("admitted Iu release")
            .lifecycle
            .release_mut()
            .expect("scheduled release")
            .core = Some(authority);
        Ok(identity)
    }
}
