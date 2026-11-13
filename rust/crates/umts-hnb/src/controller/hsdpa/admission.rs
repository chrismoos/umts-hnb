// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    hsdpa_session::{QueueBudget, RetainedDpchPolicy},
    radio_resources::HsdpaCodeSet,
    rel99_admission::{ActivationRequest, AdditionalLaunch},
};
use anyhow::{Context, Result, anyhow, ensure};
use umts_types::{CnDomain, time::CHIPS_PER_FRAME};

const RETRY_CHIPS: u64 = 10 * CHIPS_PER_FRAME;

#[derive(Clone, Copy)]
pub struct SecondaryHsdpaAdmission {
    pub primary: umts_dsp::scrambling::PrimaryScramblingCode,
    pub uarfcn_dl: u16,
    pub codes: HsdpaCodeSet,
    pub measurement_power_offset_db: f64,
    pub cpich_amplitude: f32,
    pub control_amplitude: f32,
    pub power_limit: Option<f32>,
    pub transmit_gamma_db: Option<f64>,
    pub downlink_origin: Option<ChipInstant>,
}

pub struct HsdpaAdmissionRequest<'a> {
    pub secondary: Option<SecondaryHsdpaAdmission>,
    pub scheduling: Option<&'a crate::hsdpa_session::SchedulingAdmission>,
    pub primary: umts_dsp::scrambling::PrimaryScramblingCode,
    pub policy: &'a RetainedDpchPolicy,
    pub codes: HsdpaCodeSet,
    pub launch: AdditionalLaunch,
    pub response_guard: NonZeroU64,
    pub queues: QueueBudget,
    pub downlink_origin: ChipInstant,
}

#[derive(Default, Debug)]
pub struct HsdpaAdmissionStep {
    pub scheduling: super::HsdpaSchedulingReport,
    pub visited: usize,
    pub admitted: usize,
    pub resource_backpressure: usize,
    pub feedback_wait: Option<(
        ReceiveIdentity,
        crate::rel99_admission::hsdpa::FeedbackAvailability,
    )>,
    pub failures: usize,
    pub first_failure: Option<(ReceiveIdentity, String)>,
    pub maximum_construction_time: std::time::Duration,
    pub maximum_catalog_time: std::time::Duration,
}

pub(super) struct Attempt {
    identity: ReceiveIdentity,
    retry_at: Option<ChipInstant>,
}

impl<T: Transport> CommonController<T> {
    pub fn admit_hsdpa_sessions(
        &mut self,
        request: HsdpaAdmissionRequest<'_>,
    ) -> Result<HsdpaAdmissionStep> {
        ensure!(
            request.launch.now.epoch == self.epoch()
                && request.downlink_origin.epoch == self.epoch(),
            "HSDPA admission time belongs to a different radio epoch"
        );
        let (Some(sessions), Some(admission), Some(receive)) =
            (&mut self.hsdpa, &mut self.admission, &mut self.dedicated)
        else {
            anyhow::bail!("HSDPA admission requires configured controller owners");
        };
        let control_origin = ChipInstant {
            index: request
                .downlink_origin
                .index
                .checked_sub(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                .context("HSDPA data origin precedes control lead")?,
            ..request.downlink_origin
        };
        sessions
            .transmit
            .as_mut()
            .context("HSDPA waveform owner is not configured")?
            .ensure_calendar(request.codes, control_origin, request.primary)
            .map_err(|error| anyhow!("HSDPA allocation calendar: {error:?}"))?;
        if request.secondary.is_some() {
            sessions
                .ensure_order_carrier(request.codes, request.primary)
                .map_err(anyhow::Error::msg)?;
        }
        let mut report = HsdpaAdmissionStep::default();
        if let Some(secondary) = request.secondary {
            let origin = secondary
                .downlink_origin
                .context("secondary HSDPA transmit origin is not configured")?;
            let control_origin = ChipInstant {
                index: origin
                    .index
                    .checked_sub(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                    .context("secondary HSDPA data origin precedes control lead")?,
                ..origin
            };
            sessions
                .transmit
                .as_mut()
                .context("HSDPA waveform owner is not configured")?
                .ensure_carrier_calendar(
                    umts_types::hsdpa::CarrierId::Secondary,
                    secondary.codes,
                    control_origin,
                    secondary.primary,
                )
                .map_err(|error| anyhow!("secondary HSDPA allocation calendar: {error:?}"))?;
        }
        sessions.preparation.poll().map_err(anyhow::Error::msg)?;
        if let Some(identity) = sessions.preparation.identity()
            && admission.committed_radio(identity).is_err()
            && !sessions
                .initial
                .as_ref()
                .is_some_and(|initial| initial.identity == identity)
        {
            // A stale result owns no radio state and releases its memory reservation.
            sessions.preparation.take();
        }
        for _ in 0..sessions.budget.get().min(sessions.entries.len()) {
            let slot = sessions.admission_cursor;
            sessions.admission_cursor = (slot + 1) % sessions.entries.len();
            if sessions.entries[slot].is_some() {
                continue;
            }
            let Some(identity) = admission.active_identity_at(slot) else {
                continue;
            };
            let unpublished = match receive.with_active_mapping(identity, |bank| {
                bank.hsdpa_route(identity).map(|route| route.is_none())
            }) {
                Ok(Ok(unpublished)) => unpublished,
                error => {
                    report.failures += 1;
                    if report.first_failure.is_none() {
                        report.first_failure = Some((
                            identity,
                            format!("HSDPA fallback route unavailable: {error:?}"),
                        ));
                    }
                    continue;
                }
            };
            let capability = self.radio.capabilities(identity.ue);
            let single_category =
                capability.and_then(|report| request.policy.selected_category(report));
            if unpublished
                && sessions.initial.is_none()
                && sessions.single_cell_fallback[slot] != Some(identity)
                && sessions.preparation.needs_single_fallback(identity)
                && single_category.is_some()
            {
                // No RRC has been published. Release the entire failed pair and rebuild
                // a single-cell configuration/catalog on the next bounded visit.
                drop(sessions.preparation.take());
                sessions.single_cell_fallback[slot] = Some(identity);
                tracing::info!(
                    ?identity,
                    "HSDPA selecting single-cell fallback: dual catalog build capacity"
                );
                continue;
            }
            let dual_category = request
                .secondary
                .filter(|_| sessions.single_cell_fallback[slot] != Some(identity))
                .and_then(|_| {
                    capability.and_then(|report| request.policy.selected_dual_cell(report))
                });
            let Some(category) = dual_category
                .map(|category| category.per_cell_category())
                .or(single_category)
            else {
                continue;
            };
            let Some(assigned) = self.radio.assigned(identity.ue) else {
                continue;
            };
            let cached_catalog = admission
                .prepared_hsdpa_configuration(
                    identity,
                    category,
                    assigned.crnti(),
                    dual_category.and(request.secondary),
                )
                .is_some();
            if sessions.attempts[slot].as_ref().is_some_and(|attempt| {
                attempt.identity == identity
                    && attempt
                        .retry_at
                        .is_none_or(|at| at.index > request.launch.now.index)
            }) {
                continue;
            }
            if !cached_catalog
                && request.scheduling.is_some_and(|scheduling| {
                    sessions.preparation.identity().is_none()
                        && scheduling.cell.available_bytes()
                            < scheduling
                                .budget
                                .for_carriers(1)
                                .map(|budget| budget.maximum_total_bytes.get())
                                .unwrap_or(usize::MAX)
                })
            {
                if unpublished && sessions.single_cell_fallback[slot] == Some(identity) {
                    sessions.single_cell_fallback[slot] = None;
                }
                report.resource_backpressure += 1;
                continue;
            }
            if request.scheduling.is_some()
                && !cached_catalog
                && sessions.preparation.identity().is_some()
                && !sessions.preparation.ready(identity)
            {
                continue;
            }
            let active = admission
                .active_radio(identity)
                .map_err(|error| anyhow!("HSDPA candidate radio: {error:?}"))?;
            if !active
                .plan()
                .configuration()
                .bearers()
                .iter()
                .any(|bearer| bearer.domain == Some(CnDomain::Ps))
            {
                continue;
            }
            report.visited += 1;
            let retry_at = ChipInstant {
                epoch: request.launch.now.epoch,
                index: request
                    .launch
                    .now
                    .index
                    .checked_add(RETRY_CHIPS)
                    .context("HSDPA admission retry time overflow")?,
            };
            let mut construction_rejected = false;
            let construction_started = std::time::Instant::now();
            let prepared: Result<bool> = (|| {
                let incarnation = sessions
                    .next_incarnation
                    .context("HSDPA session incarnation space exhausted")?;
                let procedure = admission.prepare_hsdpa_activation(
                    identity,
                    ActivationRequest {
                        policy: request.policy,
                        secondary: dual_category.and(request.secondary),
                        codes: request.codes,
                        launch: request.launch,
                        response_guard: request.response_guard,
                    },
                    &self.radio,
                    receive,
                )?;
                if let Some(scheduling) = request.scheduling
                    && !cached_catalog
                    && sessions.preparation.identity().is_none()
                {
                    let (preparation, secondary_preparation) =
                        match super::preparation::reserve_carriers(
                            procedure.construction().configuration().clone(),
                            request.codes,
                            dual_category.and(request.secondary),
                            scheduling,
                        ) {
                            Ok(prepared) => prepared,
                            Err(error)
                                if dual_category.is_some()
                                    && unpublished
                                    && single_category.is_some()
                                    && super::preparation::capacity_error(&error) =>
                            {
                                sessions.single_cell_fallback[slot] = Some(identity);
                                tracing::info!(
                                    ?identity,
                                    "HSDPA selecting single-cell fallback: dual catalog reservation capacity"
                                );
                                return Ok(false);
                            }
                            Err(error) => {
                                return Err(anyhow!("HSDPA catalog reservation: {error:?}"));
                            }
                        };
                    sessions
                        .preparation
                        .submit_carriers(identity, preparation, secondary_preparation)
                        .map_err(anyhow::Error::msg)?;
                    return Ok(false);
                }
                let mut cached = admission
                    .take_hsdpa_catalog(identity, procedure.construction().configuration());
                let secondary_incarnation = if dual_category.is_some() {
                    Some(
                        incarnation
                            .checked_add(1)
                            .context("secondary HSDPA incarnation space exhausted")?,
                    )
                } else {
                    None
                };
                let transferred = admission.transfer_hsdpa_feedback(identity, |feedback| {
                    let mut secondary_resources = None;
                    let resources = if let Some(scheduling) = request.scheduling {
                        if let Some(catalog) = cached.take() {
                            secondary_resources = catalog.secondary;
                            Some((catalog.resources, scheduling.feedback))
                        } else {
                            let Some(prepared) = sessions.preparation.take() else {
                                return Err((
                                    anyhow!("HSDPA catalog result is no longer available"),
                                    feedback,
                                ));
                            };
                            report.maximum_catalog_time =
                                report.maximum_catalog_time.max(prepared.elapsed);
                            if let Some(error) = prepared.non_capacity_error() {
                                construction_rejected = true;
                                return Err((
                                    anyhow!("HSDPA catalog construction: {error:?}"),
                                    feedback,
                                ));
                            }
                            secondary_resources = match prepared.secondary.transpose() {
                                Ok(resources) => resources,
                                Err(error) => {
                                    construction_rejected =
                                        !super::preparation::capacity_error(&error);
                                    return Err((
                                        anyhow!("secondary HSDPA catalog construction: {error:?}"),
                                        feedback,
                                    ));
                                }
                            };
                            match prepared.resources {
                                Ok(resources) => Some((resources, scheduling.feedback)),
                                Err(error) => {
                                    construction_rejected =
                                        !super::preparation::capacity_error(&error);
                                    return Err((
                                        anyhow!("HSDPA catalog construction: {error:?}"),
                                        feedback,
                                    ));
                                }
                            }
                        }
                    } else {
                        None
                    };
                    let settings = crate::hsdpa_session::SessionSettings {
                        incarnation,
                        downlink_origin: request.downlink_origin,
                        queues: QueueBudget {
                            pdus_per_queue: request.queues.pdus_per_queue,
                            total_bytes: request.queues.total_bytes,
                        },
                        codes: request.codes,
                        scheduling: None,
                    };
                    let built = if let Some(category) = dual_category {
                        let (
                            Some(secondary),
                            Some(incarnation),
                            Some(secondary_resources),
                            Some(scheduling),
                        ) = (
                            request.secondary,
                            secondary_incarnation,
                            secondary_resources,
                            request.scheduling,
                        )
                        else {
                            construction_rejected = true;
                            return Err((
                                anyhow!("dual-cell admission requires both reserved catalogs"),
                                feedback,
                            ));
                        };
                        Session::new_prepared_dual(
                            identity,
                            procedure,
                            feedback,
                            settings,
                            resources,
                            crate::hsdpa_session::DualCellSettings {
                                category,
                                incarnation,
                                codes: secondary.codes,
                                resources: secondary_resources,
                                feedback: scheduling.feedback,
                            },
                        )
                    } else {
                        Session::new_prepared(identity, procedure, feedback, settings, resources)
                    };
                    built.map_err(|rejected| {
                        construction_rejected = true;
                        (
                            anyhow!("HSDPA session construction: {:?}", rejected.error),
                            rejected.feedback,
                        )
                    })
                });
                if let Some(catalog) = cached {
                    admission.attach_hsdpa_catalog(identity, catalog);
                }
                let session = transferred?;
                if let Some(session) = session {
                    sessions.entries[slot] = Some(Box::new(session));
                    sessions.single_cell_fallback[slot] = None;
                    sessions.next_incarnation = incarnation
                        .get()
                        .checked_add(if dual_category.is_some() { 2 } else { 1 })
                        .and_then(NonZeroU64::new);
                    sessions.attempts[slot] = None;
                    Ok(true)
                } else {
                    report.feedback_wait =
                        Some((identity, admission.hsdpa_feedback_availability(identity)));
                    Ok(false)
                }
            })();
            report.maximum_construction_time = report
                .maximum_construction_time
                .max(construction_started.elapsed());
            match prepared {
                Ok(true) => report.admitted += 1,
                Ok(false) => (),
                Err(error) => {
                    if unpublished
                        && !cached_catalog
                        && sessions.single_cell_fallback[slot] == Some(identity)
                    {
                        sessions.single_cell_fallback[slot] = None;
                    }
                    sessions.attempts[slot] = Some(Attempt {
                        identity,
                        retry_at: Some(retry_at),
                    });
                    if construction_rejected {
                        // Rebuilding the same invalid catalog every cycle can starve radio work.
                        sessions.attempts[slot] = Some(Attempt {
                            identity,
                            retry_at: None,
                        });
                    }
                    report.failures += 1;
                    if report.first_failure.is_none() {
                        report.first_failure = Some((identity, format!("{error:#}")));
                    }
                }
            }
        }
        Ok(report)
    }
}
