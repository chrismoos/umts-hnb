// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::controller::{IuCompletionError, IuReleaseLaunch};
use crate::rel99_admission::RetainedReleaseBuild;
use umts_iuh::session::{Event, IuReleaseRequest};

enum State {
    Vacant,
    Received(IuReleaseRequest),
    Radio(IuReleaseRequest, ReceiveIdentity),
    Ready(IuReleaseRequest),
    Rollback {
        request: IuReleaseRequest,
        build: Box<RetainedReleaseBuild>,
    },
}

impl State {
    fn label(&self) -> &'static str {
        match self {
            State::Vacant => "vacant",
            State::Received(_) => "received",
            State::Radio(..) => "radio",
            State::Ready(_) => "ready",
            State::Rollback { .. } => "rollback",
        }
    }
}

// TS 25.413 maxNrOfRABs is 256, but one connection supports at most the NSAPIs a UE can hold.
const MAX_IU_VOLUME_REPORTS: usize = 16;

pub(super) struct IuReleases {
    slots: Vec<State>,
    cursor: usize,
    retry_after: Vec<u64>,
    received: u64,
    completed: u64,
    reports: Vec<umts_iuh::ranap::release::VolumeReport>,
}

impl IuReleases {
    pub(super) fn new(capacity: NonZeroUsize) -> Result<Self> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity.get())
            .context("reserve Iu release owners")?;
        slots.resize_with(capacity.get(), || State::Vacant);
        let mut retry_after = Vec::new();
        retry_after
            .try_reserve_exact(capacity.get())
            .context("reserve Iu release retry times")?;
        retry_after.resize(capacity.get(), 0);
        let mut reports = Vec::new();
        reports
            .try_reserve_exact(MAX_IU_VOLUME_REPORTS)
            .context("reserve Iu release volume reports")?;
        Ok(Self {
            slots,
            cursor: 0,
            retry_after,
            received: 0,
            completed: 0,
            reports,
        })
    }
    pub(super) fn offer(&mut self, event: &Event) -> Result<bool, ()> {
        let request = match event {
            Event::IuRelease { request, .. } | Event::Disconnected { request, .. } => request,
            _ => return Ok(false),
        };
        self.offer_request(request)
    }
    pub(super) fn offer_request(
        &mut self,
        request: &umts_iuh::session::IuReleaseRequest,
    ) -> Result<bool, ()> {
        if self.slots.iter().any(|slot| match slot {
            State::Received(owner)
            | State::Radio(owner, _)
            | State::Ready(owner)
            | State::Rollback { request: owner, .. } => owner == request,
            State::Vacant => false,
        }) {
            return Ok(true);
        }
        let vacant = self
            .slots
            .iter_mut()
            .find(|slot| matches!(slot, State::Vacant))
            .ok_or(())?;
        *vacant = State::Received(*request);
        self.received = self.received.saturating_add(1);
        Ok(true)
    }
    pub(super) fn pending(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !matches!(slot, State::Vacant))
            .count()
    }
}

#[derive(Debug)]
pub enum IuReleaseFailure {
    Build(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::rel99_admission::ReleaseAdmissionError,
    ),
    Rollback(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::rel99_admission::ReleaseRollbackError,
    ),
    Completion(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        IuCompletionError,
    ),
}

impl BearerCoordinator {
    pub(in crate::serving) fn iu_release_counts(&self) -> (u64, u64) {
        (self.iu_releases.received, self.iu_releases.completed)
    }
    pub(super) fn service_iu_releases<T: Transport, D: umts_user_plane::packet::BoundDatagrams>(
        &mut self,
        controller: &mut CommonController<T>,
        traffic: &mut crate::rel99_traffic::Rel99Traffic<D>,
        time: ControllerTime,
        threshold: ChipInstant,
        feedback_lead_chips: u64,
    ) -> Option<BearerFailure> {
        let mut first = None;
        for _ in 0..self.visits.get().min(self.iu_releases.slots.len()) {
            let index = self.iu_releases.cursor;
            self.iu_releases.cursor = (index + 1) % self.iu_releases.slots.len();
            if time.radio.index < self.iu_releases.retry_after[index] {
                continue;
            }
            let state = std::mem::replace(&mut self.iu_releases.slots[index], State::Vacant);
            let before = state.label();
            self.iu_releases.slots[index] = match state {
                State::Vacant => State::Vacant,
                State::Received(request) => {
                    if !controller.core().gateway().owns_iu_release(request) {
                        controller
                            .radio_mut()
                            .finish_core_connection_release(request);
                        State::Vacant
                    } else {
                        let owners = &self.owners;
                        match controller.begin_iu_radio_release(
                            request,
                            &self.policy,
                            &mut self.resources,
                            crate::rel99_admission::AdditionalLaunch {
                                now: time.radio,
                                threshold,
                                maximum_combinations: self.maximum_combinations,
                            },
                            |prepared, delivery| {
                                owners.release_settings(
                                    prepared,
                                    delivery,
                                    time.radio,
                                    feedback_lead_chips,
                                )
                            },
                        ) {
                            Ok(IuReleaseLaunch::Pending) => State::Received(request),
                            Ok(IuReleaseLaunch::NoRadio) => State::Ready(request),
                            Ok(IuReleaseLaunch::Scheduled(identity)) => {
                                State::Radio(request, identity)
                            }
                            Err(build) => {
                                self.iu_releases.retry_after[index] = time
                                    .radio
                                    .index
                                    .saturating_add(umts_types::time::CHIPS_PER_FRAME);
                                State::Rollback { request, build }
                            }
                        }
                    }
                }
                State::Radio(request, identity) => {
                    if controller
                        .admission()
                        .is_some_and(|admission| admission.completed_iu_radio_release(request))
                    {
                        State::Ready(request)
                    } else if !controller.core().gateway().owns_iu_release(request)
                        && controller
                            .admission()
                            .is_none_or(|admission| !admission.has_ue(request.key()))
                    {
                        controller
                            .radio_mut()
                            .finish_core_connection_release(request);
                        State::Vacant
                    } else if controller
                        .radio()
                        .dedicated_route(request.key())
                        .is_some_and(|active| {
                            controller.release_rab_ready(active)
                                && controller.admission().is_some_and(|admission| {
                                    admission.release_phase(active).is_none()
                                })
                        })
                    {
                        State::Received(request)
                    } else {
                        State::Radio(request, identity)
                    }
                }
                State::Ready(request) => {
                    let connection = umts_types::ConnectionKey {
                        ue: request.key(),
                        domain: request.domain().into(),
                    };
                    let (voices, packets) = traffic.services_mut();
                    if voices.has_connection(connection) || packets.has_connection(connection) {
                        State::Ready(request)
                    } else if !controller.core().gateway().owns_iu_release(request) {
                        controller.take_iu_radio_completion(request);
                        controller
                            .radio_mut()
                            .finish_core_connection_release(request);
                        State::Vacant
                    } else {
                        // TS 25.413 8.5.2: the completion includes the data volume reports
                        // collected when the PS transports were released.
                        self.iu_releases.reports.clear();
                        packets.take_iu_volume_reports(request, &mut self.iu_releases.reports);
                        match controller.complete_iu_connection_release(
                            request,
                            time.security.control,
                            &self.iu_releases.reports,
                        ) {
                            Ok(crate::signalling::ConnectionReleaseProgress::Complete) => {
                                self.iu_releases.completed =
                                    self.iu_releases.completed.saturating_add(1);
                                controller.take_iu_radio_completion(request);
                                State::Vacant
                            }
                            Ok(
                                crate::signalling::ConnectionReleaseProgress::PendingSecurity
                                | crate::signalling::ConnectionReleaseProgress::PendingNasDelivery,
                            ) => {
                                self.iu_releases.retry_after[index] = time
                                    .radio
                                    .index
                                    .saturating_add(umts_types::time::CHIPS_PER_FRAME);
                                State::Ready(request)
                            }
                            Err(error) => {
                                self.iu_releases.retry_after[index] = time
                                    .radio
                                    .index
                                    .saturating_add(umts_types::time::CHIPS_PER_FRAME);
                                if first.is_none() {
                                    first = Some(BearerFailure::IuRelease {
                                        request,
                                        error: IuReleaseFailure::Completion(error),
                                    });
                                }
                                State::Ready(request)
                            }
                        }
                    }
                }
                State::Rollback { request, mut build } => {
                    match build.rollback(controller.radio_mut(), &mut self.resources) {
                        Ok(()) => {
                            if first.is_none() {
                                first = Some(BearerFailure::IuRelease {
                                    request,
                                    error: IuReleaseFailure::Build(build.error),
                                });
                            }
                            State::Received(request)
                        }
                        Err(error) => {
                            if first.is_none() {
                                first = Some(BearerFailure::IuRelease {
                                    request,
                                    error: IuReleaseFailure::Rollback(error),
                                });
                            }
                            State::Rollback { request, build }
                        }
                    }
                }
            };
            let after = self.iu_releases.slots[index].label();
            if before != after {
                tracing::debug!("Iu release slot {index}: {before} -> {after}");
            }
        }
        first
    }
}
