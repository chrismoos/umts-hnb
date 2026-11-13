// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    cell::CellInformation,
    controller::{CommonController, ControllerTime},
    dedicated_rx::ReceiveIdentity,
    rab_handoff::{HandoffError, PendingRabAssignment, RabHandoff},
    radio_resources::RadioResources,
    rel99_admission::RejectedRabAssignment,
    rel99_planner::ConfiguredPolicy,
};
use anyhow::{Context, Result, anyhow, ensure};
use std::num::NonZeroUsize;
use umts_config::Config;
use umts_core::Transport;
use umts_types::{RadioEpoch, time::ChipInstant};

const UPLINK_SCRAMBLING_CODE_COUNT: u32 = 1 << 24;
const CN_DOMAINS: usize = 2;

#[cfg(test)]
mod dispatch_tests;
mod iu;
mod secondary;
use crate::controller::SecondaryHsdpaAdmission;
mod releases;
pub use iu::IuReleaseFailure;

#[derive(Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub enum DeferredCore {
    IuReleaseCapacity,
    Handoff(HandoffError),
}

#[derive(Debug)]
pub enum BearerFailure {
    HsupaAdmission {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        reason: String,
    },
    HsdpaAdmission {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: Option<ReceiveIdentity>,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        reason: String,
    },
    IuRelease {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: umts_iuh::session::IuReleaseRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: IuReleaseFailure,
    },
    ReleaseService(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::controller::ReleaseServiceError,
    ),
    ReleaseOutcome {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: umts_iuh::session::RabRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::rel99_admission::CoreReleaseError,
    },
    Cleanup(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        super::cleanup::CleanupFailure,
    ),
    Handoff(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        HandoffError,
    ),
    Capabilities {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: umts_iuh::session::RabRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::signalling::SignallingError,
    },
    LaunchTime {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        cursor: ChipInstant,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        now: ChipInstant,
    },
}

pub(super) struct BearerStep {
    pub media: crate::controller::MediaServiceStep,
    pub cleanup: super::cleanup::CleanupStep,
    pub accepted: usize,
    pub rejected: usize,
    pub failure: Option<BearerFailure>,
}

pub(super) struct BearerCoordinator {
    pub(super) assignments: RabHandoff,
    pub(super) inactivity: super::inactivity::Inactivity,
    pub(super) network_failures: u64,
    network_error: Option<crate::packet_rabs::Error>,
    policy: ConfiguredPolicy,
    resources: RadioResources,
    hsdpa: Option<super::hsdpa::SharedChannels>,
    secondary_hsdpa: Option<secondary::SecondaryChannels>,
    last_hsdpa_report: Option<ChipInstant>,
    hsupa: Option<super::hsupa::SharedChannels>,
    owners: super::owner_settings::OwnerPlan,
    circuits: super::circuit_settings::CircuitPlan,
    visits: NonZeroUsize,
    maximum_combinations: NonZeroUsize,
    cleanup: super::cleanup::Cleanup,
    releases: releases::PendingReleases,
    iu_releases: iu::IuReleases,
}

impl BearerCoordinator {
    pub(super) fn offer_core(
        &mut self,
        radio: &mut crate::signalling::CommonSignalling,
        event: &umts_iuh::session::Event,
    ) -> Result<(), DeferredCore> {
        // TS 25.413 8.26.2.1: every connection of the reset domain is released like a
        // peer disconnect, then RESET ACKNOWLEDGE is sent. Offers are idempotent, so a
        // retry after backpressure continues where it stopped.
        if let umts_iuh::session::Event::Reset { domain, releases } = event {
            for request in releases {
                self.offer_iu_release_request(request)
                    .map_err(|()| DeferredCore::IuReleaseCapacity)?;
            }
            radio.request_reset_acknowledge(*domain);
            return Ok(());
        }
        if self
            .offer_iu_release(event)
            .map_err(|()| DeferredCore::IuReleaseCapacity)?
        {
            return Ok(());
        }
        if self
            .assignments
            .offer(event)
            .map_err(DeferredCore::Handoff)?
        {
            Ok(())
        } else {
            tracing::warn!(kind = ?event.kind(), "Unsupported core event consumed");
            Ok(())
        }
    }

    pub(super) fn report_failures(&mut self) {
        if let Some(error) = self.network_error.take() {
            tracing::warn!("Packet user-plane service failed: {error:?}");
        }
    }

    pub(super) fn new(
        config: &Config,
        epoch: RadioEpoch,
        cell: &CellInformation,
        power: Option<&super::downlink_power::Plan>,
    ) -> Result<Self> {
        let capacity = NonZeroUsize::new(usize::try_from(config.runtime.maximum_ues)?)
            .context("serving UE capacity must be nonzero")?;
        let first = config.rel99.physical.uplink_scrambling_code_base;
        let remaining = UPLINK_SCRAMBLING_CODE_COUNT
            .checked_sub(first)
            .context("uplink scrambling-code base exceeds the FDD code space")?;
        ensure!(
            usize::try_from(remaining)? >= capacity.get(),
            "uplink scrambling-code range cannot reserve one code per configured UE"
        );
        let mut resources = RadioResources::new(
            epoch,
            cell,
            capacity,
            first..=UPLINK_SCRAMBLING_CODE_COUNT - 1,
        )
        .map_err(|error| anyhow!("prepare cell radio resources: {error:?}"))?;
        let hsdpa = super::hsdpa::SharedChannels::reserve(config, &mut resources, power)?;
        let hsupa = super::hsupa::SharedChannels::reserve(config, &mut resources, power)?;
        let secondary_hsdpa = secondary::SecondaryChannels::reserve(config, epoch, cell, power)?;
        let visits = NonZeroUsize::new(usize::from(config.receiver.dedicated.work_per_cycle))
            .context("media visit budget must be nonzero")?;
        let cleanup_capacity = capacity
            .get()
            .checked_mul(2)
            .and_then(NonZeroUsize::new)
            .context("bearer cleanup capacity overflow")?;
        let assignment_capacity = capacity
            .get()
            .checked_mul(CN_DOMAINS)
            .and_then(NonZeroUsize::new)
            .context("RAB handoff capacity overflow")?;
        Ok(Self {
            assignments: RabHandoff::with_capacity(
                umts_iuh::wire::Limits::default(),
                assignment_capacity,
            )
            .map_err(|error| anyhow!("prepare RAB handoff: {error:?}"))?,
            inactivity: super::inactivity::Inactivity::new(config)?,
            network_failures: 0,
            network_error: None,
            policy: ConfiguredPolicy::new(config)?,
            resources,
            hsdpa,
            secondary_hsdpa,
            last_hsdpa_report: None,
            hsupa,
            owners: super::owner_settings::OwnerPlan::new(config, power)?,
            circuits: super::circuit_settings::CircuitPlan::new(config)?,
            visits,
            maximum_combinations: NonZeroUsize::new(usize::from(
                config.rel99.physical.planner.budgets.maximum_combinations,
            ))
            .context("scheduler combination budget must be nonzero")?,
            cleanup: super::cleanup::Cleanup::new(cleanup_capacity, visits)?,
            releases: releases::PendingReleases::new(cleanup_capacity)?,
            iu_releases: iu::IuReleases::new(cleanup_capacity)?,
        })
    }

    pub(super) fn begin<T: Transport>(
        &mut self,
        controller: &mut CommonController<T>,
        pending: PendingRabAssignment,
        time: ControllerTime,
        threshold: ChipInstant,
        feedback_lead_chips: u64,
    ) -> Result<ReceiveIdentity, Box<RejectedRabAssignment>> {
        let owners = &self.owners;
        let prepared = controller.prepare_rab_dch(pending, &self.policy, &mut self.resources)?;
        let prepared = match &self.hsdpa {
            Some(shared) => {
                match shared.prepare_initial(controller, prepared, &mut self.resources)? {
                    crate::controller::InitialCatalogAdmission::Deferred(identity) => {
                        return Ok(identity);
                    }
                    crate::controller::InitialCatalogAdmission::Ready(prepared) => prepared,
                }
            }
            None => prepared,
        };
        controller.begin_prepared_rab_dch(prepared, &self.policy, &mut self.resources, |prepared| {
            owners.settings(prepared, time.radio, threshold, feedback_lead_chips)
        })
    }

    pub(super) fn outstanding(&self) -> usize {
        self.resources.outstanding()
    }

    pub(super) fn hsdpa_codes(&self) -> Option<crate::radio_resources::HsdpaCodeSet> {
        self.hsdpa.as_ref().map(super::hsdpa::SharedChannels::codes)
    }

    pub(super) fn prepare_hsdpa_transmit(
        &mut self,
        source: &crate::common_tx::CommonTx,
    ) -> Result<Option<crate::hsdpa_tx::TransmitBank>> {
        self.hsdpa
            .as_mut()
            .map(|shared| shared.prepare_transmit(source))
            .transpose()
    }

    pub(super) fn secondary_hsdpa(&self) -> Option<SecondaryHsdpaAdmission> {
        self.secondary_hsdpa
            .as_ref()
            .map(secondary::SecondaryChannels::admission)
    }

    pub(super) fn prepare_secondary_hsdpa_transmit(
        &mut self,
        source: &crate::common_tx::CommonTx,
    ) -> Result<Option<crate::hsdpa_tx::TransmitBank>> {
        let bank = self
            .secondary_hsdpa
            .as_mut()
            .map(|shared| shared.prepare_transmit(source))
            .transpose()?;
        if let Some(secondary) = self.secondary_hsdpa() {
            self.hsdpa
                .as_mut()
                .context("secondary HSDPA requires primary shared channels")?
                .configure_secondary(secondary);
        }
        Ok(bank)
    }

    pub(super) fn prepare_hsupa_transmit(
        &mut self,
        source: &crate::common_tx::CommonTx,
    ) -> Result<Option<Box<crate::hsupa_tx::TransmitBank>>> {
        self.hsupa
            .as_mut()
            .map(|shared| shared.prepare_transmit(source))
            .transpose()
    }

    pub(super) fn pending_cleanups(&self) -> usize {
        self.cleanup.pending() + self.releases.pending() + self.iu_releases.pending()
    }
    pub(super) fn offer_iu_release(
        &mut self,
        event: &umts_iuh::session::Event,
    ) -> Result<bool, ()> {
        self.iu_releases.offer(event)
    }
    pub(super) fn offer_iu_release_request(
        &mut self,
        request: &umts_iuh::session::IuReleaseRequest,
    ) -> Result<bool, ()> {
        self.iu_releases.offer_request(request)
    }

    pub(super) fn recovery_required(&self) -> Option<ReceiveIdentity> {
        self.cleanup.recovery_required()
    }

    pub(super) fn recovery_error(&self) -> Option<&crate::rel99_admission::AdmissionError> {
        self.cleanup.recovery_error()
    }

    pub(super) fn service<T: Transport, D: umts_user_plane::packet::BoundDatagrams>(
        &mut self,
        controller: &mut CommonController<T>,
        traffic: &mut crate::rel99_traffic::Rel99Traffic<D>,
        time: ControllerTime,
        control: std::time::Instant,
        feedback_lead_chips: u64,
    ) -> Result<BearerStep, crate::controller::ControllerError> {
        self.assignments.confirm(controller.core().gateway());
        let receive = controller
            .dedicated_mut()
            .expect("serving attaches dedicated controller");
        if let Err(error) = traffic.network_step(receive) {
            self.network_failures = self.network_failures.saturating_add(1);
            if self.network_error.is_none() {
                self.network_error = Some(error);
            }
        }
        traffic.packets_mut().take_peer_failures(|route| {
            match controller.request_gtp_peer_failure_release(time.security.control, route) {
                Ok(requested) => {
                    if requested {
                        tracing::warn!(
                            "GTP-U Error Indication: requested PS Iu release ue={:?} rab={}",
                            route.connection.ue,
                            route.rab
                        );
                    }
                    true
                }
                Err(error) => {
                    self.network_failures = self.network_failures.saturating_add(1);
                    if self.network_failures.is_power_of_two() {
                        tracing::warn!(
                            "GTP-U Error Indication: PS Iu release request failed ue={:?} rab={} error={error:?}",
                            route.connection.ue,
                            route.rab
                        );
                    }
                    false
                }
            }
        });
        if let Err(error) =
            self.inactivity
                .service(controller, traffic, &self.assignments, time.radio)
        {
            self.inactivity.counters.failed = self.inactivity.counters.failed.saturating_add(1);
            if self.inactivity.counters.failed.is_power_of_two() {
                tracing::warn!(
                    "Idle resource service: {error:?} failures={}",
                    self.inactivity.counters.failed
                );
            }
        }
        let cleanup = self.cleanup.step(
            controller,
            &mut self.resources,
            traffic,
            time.radio,
            time.security.control,
        );
        if let Some(request) = cleanup.release_transferred {
            self.releases.failed(request)
        }
        if self.cleanup.recovery_required().is_some() {
            return Ok(BearerStep {
                media: crate::controller::MediaServiceStep::default(),
                cleanup,
                accepted: 0,
                rejected: 0,
                failure: None,
            });
        }
        let mut accepted = 0;
        let mut rejected = 0;
        let mut failure = None;
        let (cursor, guard) = controller.dedicated_launch_cursor();
        match super::launch::threshold(cursor, time.radio, guard) {
            Err(_) => {
                failure = Some(BearerFailure::LaunchTime {
                    cursor,
                    now: time.radio,
                })
            }
            Ok(threshold) => {
                if let Some(error) = self.service_iu_releases(
                    controller,
                    traffic,
                    time,
                    threshold,
                    feedback_lead_chips,
                ) {
                    failure = Some(error);
                }
                let dispatch =
                    self.advance_releases(controller, time, threshold, feedback_lead_chips);
                accepted += dispatch.accepted;
                rejected += dispatch.rejected;
                if failure.is_none() {
                    failure = dispatch.failure;
                }
                if let Some(slot) = self.cleanup.vacant() {
                    let owners = &self.owners;
                    match controller.service_initial_hsdpa(
                        &self.policy,
                        &mut self.resources,
                        |prepared| {
                            owners.settings(prepared, time.radio, threshold, feedback_lead_chips)
                        },
                    ) {
                        Ok(Some(Ok(_))) => (),
                        Ok(Some(Err(rejection))) => {
                            rejected += 1;
                            self.cleanup.reject(slot, rejection);
                        }
                        Ok(None) => (),
                        Err(reason) => {
                            if failure.is_none() {
                                failure = Some(BearerFailure::HsdpaAdmission {
                                    identity: None,
                                    reason,
                                });
                            }
                        }
                    }
                }
                for _ in 0..self.visits.get() {
                    let Some(slot) = self.cleanup.vacant() else {
                        break;
                    };
                    let pending =
                        self.assignments
                            .take_committed_matching_rabs(|request, kind, rabs| {
                                if !controller.core().gateway().rab_connection_current(request) {
                                    return true;
                                }
                                let key = request.key();
                                if let Some(identity) = controller.radio().dedicated_route(key) {
                                    if kind == crate::rab_handoff::AssignmentKind::SetupOnly
                                        && controller.packet_modification_matches(request, rabs)
                                    {
                                        return controller.packet_modification_ready(identity);
                                    }
                                    if !matches!(kind, crate::rab_handoff::AssignmentKind::Empty) {
                                        match controller.ensure_hsupa_dch(
                                            identity,
                                            crate::rel99_admission::AdditionalLaunch {
                                                now: time.radio,
                                                threshold,
                                                maximum_combinations: self.maximum_combinations,
                                            },
                                            &mut self.resources,
                                        ) {
                                            Ok(true) => (),
                                            Ok(false) => return false,
                                            Err(error) => {
                                                if failure.is_none() {
                                                    failure = Some(BearerFailure::HsupaAdmission {
                                                        reason: format!(
                                                            "DCH restoration: {error:#}"
                                                        ),
                                                    });
                                                }
                                                return false;
                                            }
                                        }
                                    }
                                    return match kind {
                                        crate::rab_handoff::AssignmentKind::SetupOnly => {
                                            controller.additional_rab_ready(identity)
                                        }
                                        crate::rab_handoff::AssignmentKind::ReleaseOnly
                                        | crate::rab_handoff::AssignmentKind::Mixed => {
                                            self.releases.vacant().is_some()
                                                && controller.release_rab_ready(identity)
                                        }
                                        crate::rab_handoff::AssignmentKind::Empty => true,
                                    };
                                }
                                if matches!(
                                    kind,
                                    crate::rab_handoff::AssignmentKind::ReleaseOnly
                                        | crate::rab_handoff::AssignmentKind::Mixed
                                ) {
                                    return self.releases.vacant().is_some();
                                }
                                if self.resources.slot_reserved(key.slot as usize) {
                                    return false;
                                }
                                match controller.radio_mut().ensure_capabilities(key, time.radio) {
                                    Ok(crate::signalling::CapabilityReadiness::Ready) => {
                                        request.domain() != umts_iuh::Domain::Ps
                                            || self.hsdpa.as_ref().is_none_or(|shared| {
                                                shared.initial_preparation_available(controller)
                                            })
                                    }
                                    Ok(_) => false,
                                    Err(error) => {
                                        let terminal = matches!(
                                            error,
                                            crate::signalling::SignallingError::Origin
                                                | crate::signalling::SignallingError::Phase
                                        );
                                        if failure.is_none() {
                                            failure = Some(BearerFailure::Capabilities {
                                                request,
                                                error,
                                            });
                                        }
                                        terminal
                                    }
                                }
                            });
                    match pending {
                        Ok(Some(pending))
                            if pending.kind() == crate::rab_handoff::AssignmentKind::SetupOnly
                                && controller.packet_modification_matches(
                                    pending.request(),
                                    &pending
                                        .assignment()
                                        .setup
                                        .iter()
                                        .map(|item| item.first.id.get())
                                        .collect::<Vec<_>>(),
                                ) =>
                        {
                            match controller.modify_packet_rabs(
                                &pending,
                                traffic.packets_mut(),
                                time.security.control,
                            ) {
                                Ok(report) => {
                                    accepted += usize::from(report.modified != 0);
                                    rejected += usize::from(report.rejected != 0);
                                }
                                Err(error) => {
                                    rejected += 1;
                                    self.cleanup.reject_additional(
                                        slot,
                                        Box::new(
                                            crate::rel99_admission::RejectedAdditionalAssignment {
                                                error,
                                                pending,
                                                change: None,
                                                rollback_error: None,
                                            },
                                        ),
                                    );
                                }
                            }
                        }
                        Ok(Some(pending))
                            if matches!(
                                pending.kind(),
                                crate::rab_handoff::AssignmentKind::ReleaseOnly
                                    | crate::rab_handoff::AssignmentKind::Mixed
                            ) =>
                        {
                            let request = pending.request();
                            if !controller.core().gateway().rab_connection_current(request) {
                                self.cleanup.reject_release(
                                    slot,
                                    Box::new(crate::rel99_admission::RejectedReleaseAssignment {
                                        error: crate::rel99_admission::ReleaseAdmissionError::Core(
                                            crate::rel99_admission::CoreReleaseError::Identity,
                                        ),
                                        pending,
                                        pool: None,
                                        change: None,
                                        rollback_error: None,
                                    }),
                                );
                                rejected += 1;
                                continue;
                            }
                            let release_slot = self
                                .releases
                                .vacant()
                                .expect("release capacity checked before selection");
                            let owners = &self.owners;
                            let result = if controller
                                .radio()
                                .dedicated_route(pending.request().key())
                                .is_some()
                            {
                                controller.begin_release_rab_with_delivery(
                                    pending,
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
                                )
                            } else {
                                controller.prepare_unmapped_release(pending)
                                    .map(crate::rel99_admission::ReleaseLaunchResult::OutcomeOnly)
                                    .map_err(|rejected| Box::new(crate::rel99_admission::RejectedReleaseAssignment {
                                        error: crate::rel99_admission::ReleaseAdmissionError::Core(rejected.error),
                                        pending: *rejected.pending, pool: None, change: None, rollback_error: None,
                                    }))
                            };
                            match result {
                                Ok(crate::rel99_admission::ReleaseLaunchResult::Scheduled(_)) => {
                                    self.releases.reserve(release_slot, request);
                                    accepted += 1;
                                }
                                Ok(crate::rel99_admission::ReleaseLaunchResult::OutcomeOnly(
                                    owner,
                                )) => {
                                    self.releases.insert(release_slot, owner);
                                    accepted += 1;
                                }
                                Err(rejected_release) => {
                                    rejected += 1;
                                    self.cleanup.reject_release(slot, rejected_release);
                                }
                            }
                        }
                        Ok(Some(pending))
                            if controller
                                .radio()
                                .dedicated_route(pending.request().key())
                                .is_some() =>
                        {
                            let owners = &self.owners;
                            match controller.begin_additional_rab_with_delivery(
                                pending,
                                &self.policy,
                                &mut self.resources,
                                crate::rel99_admission::AdditionalLaunch {
                                    now: time.radio,
                                    threshold,
                                    maximum_combinations: self.maximum_combinations,
                                },
                                |prepared, delivery| {
                                    owners.existing_settings(
                                        prepared,
                                        delivery,
                                        time.radio,
                                        feedback_lead_chips,
                                    )
                                },
                            ) {
                                Ok(_) => accepted += 1,
                                Err(rejection) => {
                                    rejected += 1;
                                    self.cleanup.reject_additional(slot, rejection);
                                }
                            }
                        }
                        Ok(Some(pending)) => match self.begin(
                            controller,
                            pending,
                            time,
                            threshold,
                            feedback_lead_chips,
                        ) {
                            Ok(_) => accepted += 1,
                            Err(rejection) => {
                                rejected += 1;
                                self.cleanup.reject(slot, rejection);
                            }
                        },
                        Ok(None) => break,
                        Err(error) => {
                            if failure.is_none() {
                                failure = Some(BearerFailure::Handoff(error));
                            }
                            break;
                        }
                    }
                }
            }
        }
        let playout = self.circuits.playout();
        let (voices, packets) = traffic.services_mut();
        let mut media = controller.service_initial_media(
            packets,
            voices,
            time.security.control,
            self.visits,
            |_| {
                Ok(crate::rel99_admission::CircuitPlayoutTime {
                    control,
                    radio: time.radio,
                    settings: playout,
                })
            },
            |metadata, item| self.circuits.settings(metadata, item),
        )?;
        let additional = controller.service_additional_media(
            &mut self.resources,
            packets,
            voices,
            crate::controller::AdditionalServiceSettings {
                control: time.security.control,
                playout: crate::rel99_admission::CircuitPlayoutTime {
                    control,
                    radio: time.radio,
                    settings: playout,
                },
                visits: self.visits,
            },
            |metadata, item| self.circuits.settings(metadata, item),
        )?;
        media.visited += additional.visited;
        media.failures += additional.failures;
        if media.first_failure.is_none() {
            media.first_failure = additional.first_failure;
        }
        let release_failure = self.collect_releases(controller, traffic, time);
        if failure.is_none() {
            failure = release_failure;
        }
        if let Some(hsdpa) = &self.hsdpa
            && failure.is_none()
        {
            let (cursor, guard) = controller.dedicated_launch_cursor();
            match super::launch::threshold(cursor, time.radio, guard) {
                Ok(threshold) => match hsdpa.admit(
                    controller,
                    crate::rel99_admission::AdditionalLaunch {
                        now: time.radio,
                        threshold,
                        maximum_combinations: self.maximum_combinations,
                    },
                ) {
                    Ok(report) => {
                        if report.visited != 0
                            && (report.admitted != 0
                                || report.failures != 0
                                || self.last_hsdpa_report.is_none_or(|last| {
                                    last.epoch != time.radio.epoch
                                        || time.radio.index.saturating_sub(last.index)
                                            >= umts_types::time::CHIPS_PER_SECOND
                                }))
                        {
                            self.last_hsdpa_report = Some(time.radio);
                            if report.failures != 0 {
                                tracing::warn!(
                                    "HSDPA admission: visited={} admitted={} failures={} maximum_construction_us={} background_catalog_us={} feedback_wait={:?}",
                                    report.visited,
                                    report.admitted,
                                    report.failures,
                                    report.maximum_construction_time.as_micros(),
                                    report.maximum_catalog_time.as_micros(),
                                    report.feedback_wait,
                                );
                            } else {
                                tracing::debug!(
                                    "HSDPA admission: visited={} admitted={} maximum_construction_us={} background_catalog_us={} feedback_wait={:?}",
                                    report.visited,
                                    report.admitted,
                                    report.maximum_construction_time.as_micros(),
                                    report.maximum_catalog_time.as_micros(),
                                    report.feedback_wait,
                                );
                            }
                        }
                        if let Some((identity, reason)) = report.first_failure {
                            failure = Some(BearerFailure::HsdpaAdmission {
                                identity: Some(identity),
                                reason,
                            });
                        }
                    }
                    Err(error) => {
                        failure = Some(BearerFailure::HsdpaAdmission {
                            identity: None,
                            reason: format!("{error:#}"),
                        });
                    }
                },
                Err(_) => {
                    failure = Some(BearerFailure::LaunchTime {
                        cursor,
                        now: time.radio,
                    });
                }
            }
        }
        if let Some(hsupa) = &mut self.hsupa {
            let (cursor, guard) = controller.dedicated_launch_cursor();
            let result = super::launch::threshold(cursor, time.radio, guard)
                .map_err(|error| anyhow!("HSUPA launch threshold: {error:?}"))
                .and_then(|threshold| {
                    hsupa.admit(
                        controller,
                        &mut self.resources,
                        crate::rel99_admission::AdditionalLaunch {
                            now: time.radio,
                            threshold,
                            maximum_combinations: self.maximum_combinations,
                        },
                    )
                });
            if let Err(error) = result
                && failure.is_none()
            {
                failure = Some(BearerFailure::HsupaAdmission {
                    reason: format!("{error:#}"),
                });
            }
        }
        Ok(BearerStep {
            media,
            cleanup,
            accepted,
            rejected,
            failure,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cell::{CellPlan, ReferencePowers},
        test_support::at,
    };

    fn config() -> Config {
        serde_yaml::from_str(include_str!(
            "../../../../../rust/test-support/fixtures/config/rel99.yaml"
        ))
        .unwrap()
    }

    fn cell(config: &Config) -> CellPlan {
        CellPlan::compile(
            config,
            at(0).epoch,
            1,
            ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -90,
            },
        )
        .unwrap()
    }

    #[test]
    fn peer_disconnect_is_owned_without_blocking_another_ues_direct_transfer() {
        use crate::core_link::tests::{ready, register};
        use umts_iuh::{
            Domain, ranap, rua,
            session::{ControlTime, Event},
            wire::Limits,
        };

        let source = crate::test_support::source(false);
        let mut radio = crate::signalling::tests::controller(&source);
        let (mut link, peer) = ready(&mut radio);
        let (first, context, _) = register(&mut link, &peer, &mut radio, &source, 0, 3);
        let (second, second_context, _) = register(&mut link, &peer, &mut radio, &source, 1, 6);
        let other = radio.connection(second, umts_types::CnDomain::Cs).unwrap();
        let config = config();
        let information = cell(&config);
        let mut plan =
            BearerCoordinator::new(&config, at(0).epoch, information.information(), None).unwrap();
        peer.rua(rua::Message::Disconnect {
            context,
            domain: Domain::Cs,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        });
        let mut request = None;
        let step = link
            .drive(ControlTime(10), &mut radio, |_, event| {
                assert!(plan.offer_iu_release(event)?);
                assert!(plan.offer_iu_release(event)?);
                let Event::Disconnected { request: owner, .. } = event else {
                    panic!("disconnect owner required")
                };
                request = Some(*owner);
                Ok::<_, ()>(())
            })
            .unwrap();
        assert!(step.gateway.delivered);
        assert_eq!(plan.pending_cleanups(), 1);
        let request = request.unwrap();
        assert!(link.gateway().owns_iu_release(request));
        peer.rua(rua::Message::Direct {
            context: second_context,
            domain: Domain::Cs,
            ranap: ranap::downlink_direct(&[5, 2], None, Limits::default()).unwrap(),
        });
        assert!(
            link.drive(ControlTime(11), &mut radio, |_, _| Err::<(), _>(()))
                .unwrap()
                .gateway
                .delivered
        );
        assert_eq!(
            radio.connection(second, umts_types::CnDomain::Cs),
            Some(other)
        );
        assert_eq!(
            radio
                .notify_core_connection_release(first, umts_types::CnDomain::Cs)
                .unwrap(),
            crate::signalling::ConnectionReleaseProgress::Complete
        );
        link.gateway_mut()
            .complete_iu_release(ControlTime(12), request, &[])
            .unwrap();
        assert!(!link.gateway().owns_iu_release(request));
        assert_eq!(radio.connection(first, umts_types::CnDomain::Cs), None);
        assert_eq!(
            radio.connection(second, umts_types::CnDomain::Cs),
            Some(other)
        );
    }

    #[test]
    fn cell_resource_range_is_validated_without_reserving_fixture_codes() {
        let mut config = config();
        let information = cell(&config);
        let plan =
            BearerCoordinator::new(&config, at(0).epoch, information.information(), None).unwrap();
        assert_eq!(plan.outstanding(), 0);
        config.rel99.physical.uplink_scrambling_code_base = UPLINK_SCRAMBLING_CODE_COUNT - 1;
        assert!(
            BearerCoordinator::new(&config, at(0).epoch, information.information(), None).is_err()
        );
        config.runtime.maximum_ues = 1;
        assert!(
            BearerCoordinator::new(&config, at(0).epoch, information.information(), None).is_ok()
        );
        config.rel99.physical.uplink_scrambling_code_base = UPLINK_SCRAMBLING_CODE_COUNT;
        assert!(
            BearerCoordinator::new(&config, at(0).epoch, information.information(), None).is_err()
        );
    }
}
