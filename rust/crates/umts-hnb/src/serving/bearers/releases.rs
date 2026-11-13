// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rel99_admission::{RabAdmissionError, RabReleaseRequest, RejectedAdditionalAssignment};

pub(super) struct PendingReleases {
    slots: Vec<ReleaseSlot>,
    cursor: usize,
}

enum ReleaseSlot {
    Vacant,
    Scheduled(umts_iuh::session::RabRequest),
    Pending(RabReleaseRequest),
}

impl PendingReleases {
    pub(super) fn failed(&mut self, request: umts_iuh::session::RabRequest) {
        let index = self
            .slots
            .iter()
            .position(|slot| matches!(slot,ReleaseSlot::Scheduled(owner) if *owner==request))
            .expect("failed scheduled release transfers its exact reservation into cleanup");
        self.slots[index] = ReleaseSlot::Vacant;
    }
    pub(super) fn new(capacity: NonZeroUsize) -> Result<Self> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity.get())
            .context("reserve pending release owners")?;
        slots.resize_with(capacity.get(), || ReleaseSlot::Vacant);
        Ok(Self { slots, cursor: 0 })
    }

    pub(super) fn vacant(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| matches!(slot, ReleaseSlot::Vacant))
    }

    pub(super) fn reserve(&mut self, index: usize, request: umts_iuh::session::RabRequest) {
        assert!(matches!(self.slots[index], ReleaseSlot::Vacant));
        self.slots[index] = ReleaseSlot::Scheduled(request);
    }

    pub(super) fn complete(&mut self, owner: RabReleaseRequest) {
        let index = self
            .slots
            .iter()
            .position(|slot| matches!(slot, ReleaseSlot::Scheduled(request) if *request == owner.request()))
            .expect("scheduled release retains its reserved completion slot");
        self.slots[index] = if owner.has_setup() {
            ReleaseSlot::Pending(owner)
        } else {
            ReleaseSlot::Vacant
        };
    }

    pub(super) fn insert(&mut self, index: usize, owner: RabReleaseRequest) {
        assert!(
            matches!(self.slots[index], ReleaseSlot::Vacant),
            "release slot reserved before transfer"
        );
        self.slots[index] = ReleaseSlot::Pending(owner);
    }

    pub(super) fn pending(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !matches!(slot, ReleaseSlot::Vacant))
            .count()
    }
}

#[derive(Default)]
pub(super) struct ReleaseDispatch {
    pub accepted: usize,
    pub rejected: usize,
    pub failure: Option<BearerFailure>,
}

impl BearerCoordinator {
    pub(super) fn advance_releases<T: Transport>(
        &mut self,
        controller: &mut CommonController<T>,
        time: ControllerTime,
        threshold: ChipInstant,
        feedback_lead_chips: u64,
    ) -> ReleaseDispatch {
        let mut report = ReleaseDispatch::default();
        for _ in 0..self.visits.get().min(self.releases.slots.len()) {
            let index = self.releases.cursor;
            self.releases.cursor = (index + 1) % self.releases.slots.len();
            let mut owner =
                match std::mem::replace(&mut self.releases.slots[index], ReleaseSlot::Vacant) {
                    ReleaseSlot::Pending(owner) => owner,
                    slot => {
                        self.releases.slots[index] = slot;
                        continue;
                    }
                };
            if !owner.outcomes_recorded()
                && let Err(error) = controller.record_core_release_ready(
                    &mut owner,
                    time.security.control,
                    NonZeroUsize::MIN,
                )
            {
                if report.failure.is_none() {
                    report.failure = Some(BearerFailure::ReleaseOutcome {
                        request: owner.request(),
                        error,
                    });
                }
                self.releases.insert(index, owner);
                continue;
            }
            if !owner.outcomes_recorded() {
                self.releases.insert(index, owner);
                continue;
            }
            if !owner.has_setup() {
                continue;
            }
            let Some(slot) = self.cleanup.vacant() else {
                self.releases.insert(index, owner);
                continue;
            };
            let identity = controller.radio().dedicated_route(owner.request().key());
            if controller
                .core()
                .gateway()
                .rab_connection_current(owner.request())
                && identity.is_some_and(|identity| !controller.additional_rab_ready(identity))
            {
                self.releases.insert(index, owner);
                continue;
            }
            if identity.is_none()
                && controller
                    .core()
                    .gateway()
                    .rab_connection_current(owner.request())
            {
                let key = owner.request().key();
                if self.resources.slot_reserved(key.slot as usize) {
                    self.releases.insert(index, owner);
                    continue;
                }
                match controller.radio_mut().ensure_capabilities(key, time.radio) {
                    Ok(crate::signalling::CapabilityReadiness::Ready) => (),
                    Ok(_) => {
                        self.releases.insert(index, owner);
                        continue;
                    }
                    Err(error) => {
                        let terminal = matches!(
                            error,
                            crate::signalling::SignallingError::Origin
                                | crate::signalling::SignallingError::Phase
                        );
                        if report.failure.is_none() {
                            report.failure = Some(BearerFailure::Capabilities {
                                request: owner.request(),
                                error: error.clone(),
                            });
                        }
                        if terminal {
                            self.cleanup.reject_additional(
                                slot,
                                Box::new(RejectedAdditionalAssignment {
                                    error: RabAdmissionError::Admission(
                                        crate::rel99_admission::AdmissionError::Signalling(error),
                                    ),
                                    pending: owner.into_pending(),
                                    change: None,
                                    rollback_error: None,
                                }),
                            );
                            report.rejected += 1;
                        } else {
                            self.releases.insert(index, owner);
                        }
                        continue;
                    }
                }
            }
            let continuation = match owner.into_setup_after_release(controller.core().gateway()) {
                Ok(continuation) => continuation,
                Err(owner) => {
                    self.cleanup.reject_additional(
                        slot,
                        Box::new(RejectedAdditionalAssignment {
                            error: RabAdmissionError::Construction,
                            pending: owner.into_pending(),
                            change: None,
                            rollback_error: None,
                        }),
                    );
                    report.rejected += 1;
                    continue;
                }
            };
            let owners = &self.owners;
            if identity.is_none() {
                match controller.begin_initial_after_release_rab_with_settings(
                    continuation,
                    &self.policy,
                    &mut self.resources,
                    |prepared| {
                        owners.settings(prepared, time.radio, threshold, feedback_lead_chips)
                    },
                ) {
                    Ok(_) => report.accepted += 1,
                    Err(rejected) => {
                        self.cleanup.reject(slot, rejected);
                        report.rejected += 1;
                    }
                }
                continue;
            }
            match controller.begin_after_release_rab_with_delivery(
                continuation,
                &self.policy,
                &mut self.resources,
                crate::rel99_admission::AdditionalLaunch {
                    now: time.radio,
                    threshold,
                    maximum_combinations: self.maximum_combinations,
                },
                |prepared, delivery| {
                    owners.existing_settings(prepared, delivery, time.radio, feedback_lead_chips)
                },
            ) {
                Ok(_) => report.accepted += 1,
                Err(rejected) => {
                    self.cleanup.reject_additional(slot, rejected);
                    report.rejected += 1;
                }
            }
        }
        report
    }

    pub(super) fn collect_releases<T: Transport, D: umts_user_plane::packet::BoundDatagrams>(
        &mut self,
        controller: &mut CommonController<T>,
        traffic: &mut crate::rel99_traffic::Rel99Traffic<D>,
        time: ControllerTime,
    ) -> Option<BearerFailure> {
        let mut first_failure = None;
        let (voices, packets) = traffic.services_mut();
        for _ in 0..self.visits.get() {
            match controller.service_one_release(
                &mut self.resources,
                packets,
                voices,
                time.radio,
                time.security.control,
            ) {
                Ok(Some(owner)) => self.releases.complete(owner),
                Ok(_) => (),
                Err(error) => {
                    if first_failure.is_none() {
                        first_failure = Some(BearerFailure::ReleaseService(error));
                    }
                }
            }
        }
        first_failure
    }
}
