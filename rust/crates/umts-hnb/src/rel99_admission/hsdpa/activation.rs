// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_controller::DedicatedController,
    dedicated_rx::ReceiveIdentity,
    hsdpa_session::RetainedDpchPolicy,
    radio_resources::HsdpaCodeSet,
    rel99_admission::{
        AdditionalLaunch, Rel99Admission,
        delivery::{
            MAX_SIZE_PASSES, RrcDeliveryTiming, activation_at, delivery_end, first_unprepared,
            joint_period, next_boundary,
        },
    },
    signalling::{CommonSignalling, HspaConfiguration},
};
use anyhow::{Context, Result, anyhow, ensure};
use std::{num::NonZeroU64, sync::Arc};
use umts_rrc::hsdpa::ActivationProcedure;
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};

const DELIVERY_TIMING: RrcDeliveryTiming = RrcDeliveryTiming {
    n1_frames: 10,
    guard_periods: 2,
};

pub struct ActivationRequest<'a> {
    pub secondary: Option<crate::controller::SecondaryHsdpaAdmission>,
    pub policy: &'a RetainedDpchPolicy,
    pub codes: HsdpaCodeSet,
    pub launch: AdditionalLaunch,
    pub response_guard: NonZeroU64,
}

impl Rel99Admission {
    pub(crate) fn prepare_hsdpa_activation(
        &self,
        identity: ReceiveIdentity,
        request: ActivationRequest<'_>,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<Box<ActivationProcedure>> {
        let launch = request.launch;
        ensure!(
            launch.now.epoch == identity.ue.epoch
                && launch.threshold.epoch == identity.ue.epoch
                && launch.threshold.index > launch.now.index,
            "HSDPA activation needs a future threshold in the UE radio epoch"
        );
        let active = self
            .active_radio(identity)
            .map_err(|error| anyhow!("HSDPA active radio: {error:?}"))?;
        let capability = radio
            .capabilities(identity.ue)
            .context("HSDPA capability information has not been received")?;
        let dual = request
            .policy
            .selected_dual_cell(capability)
            .zip(request.secondary);
        let category = dual
            .map(|(category, _)| category.per_cell_category())
            .or_else(|| request.policy.selected_category(capability))
            .context("UE has not reported a supported HSDPA category")?;
        let simultaneous = capability
            .simultaneous_dch()
            .context("UE has not reported simultaneous HSDPA/DCH reception limits")?;
        let limits = umts_rrc::capabilities::rel99::Rel99Limits::from_report(capability)
            .map_err(|error| anyhow!("HSDPA retained DCH capabilities: {error:?}"))?;
        simultaneous
            .apply(limits)
            .validate_dch(active.plan().configuration())
            .map_err(|error| {
                anyhow!("HSDPA retained DCH exceeds simultaneous reception limits: {error:?}")
            })?;
        let assigned = radio
            .assigned(identity.ue)
            .context("HSDPA UE has no assigned radio identity")?;
        self.prepare_hspa_activation(
            identity,
            launch,
            request.response_guard,
            radio,
            receive,
            |dedicated, activation| {
                if let Some(catalog) = &self
                    .entry(identity)
                    .and_then(|entry| entry.prepared_hsdpa.as_ref())
                {
                    ensure!(
                        Arc::ptr_eq(catalog.configuration.dedicated(), &dedicated)
                            && catalog.configuration.parameters().h_rnti == assigned.crnti()
                            && catalog.configuration.parameters().harq.category() == category,
                        "prepared HSDPA catalog no longer matches the admitted radio"
                    );
                    return umts_rrc::hsdpa::InitialActivation::schedule(
                        catalog.configuration.clone(),
                        activation,
                    )
                    .map(Arc::new)
                    .map_err(|error| anyhow!("prepared HSDPA activation: {error:?}"));
                }
                if let Some((category, secondary)) = dual {
                    let configuration = request.policy.construct_configuration_dual(
                        dedicated,
                        category,
                        request.codes,
                        assigned.crnti(),
                        secondary,
                    )?;
                    return umts_rrc::hsdpa::InitialActivation::schedule(configuration, activation)
                        .map(Arc::new)
                        .map_err(|error| anyhow!("dual-cell HSDPA activation: {error:?}"));
                }
                // C-RNTI remains cell-unique for this UE's entire connection.
                request.policy.construct(
                    dedicated,
                    category,
                    request.codes,
                    assigned.crnti(),
                    activation,
                )
            },
        )
    }

    pub(crate) fn prepare_hspa_activation<C: HspaConfiguration>(
        &self,
        identity: ReceiveIdentity,
        launch: AdditionalLaunch,
        response_guard: NonZeroU64,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
        mut construct: impl FnMut(
            Arc<umts_rrc::dedicated::Rel99Configuration>,
            umts_rrc::dedicated::ScheduledActivation,
        ) -> Result<Arc<C>>,
    ) -> Result<Box<umts_rrc::dedicated::ActivationProcedure<C>>> {
        ensure!(
            launch.now.epoch == identity.ue.epoch
                && launch.threshold.epoch == identity.ue.epoch
                && launch.threshold.index > launch.now.index,
            "HSPA activation needs a future threshold in the UE radio epoch"
        );
        let active = self
            .active_radio(identity)
            .map_err(|error| anyhow!("HSPA active radio: {error:?}"))?;
        let entry = self.entry(identity).context("HSPA admission is absent")?;
        let (clock, floor) = self
            .delivery_clock(identity, receive, launch.maximum_combinations)
            .map_err(|error| anyhow!("HSPA control delivery capacity: {error:?}"))?;
        let period = joint_period(active.plan(), active.plan())
            .map_err(|error| anyhow!("HSPA retained transport timing: {error:?}"))?;
        let preparation_chips = self
            .limits
            .maximum_lead_frames
            .get()
            .checked_add(self.limits.prefill_frames.get())
            .and_then(|frames| u64::try_from(frames).ok())
            .and_then(|frames| frames.checked_mul(CHIPS_PER_FRAME))
            .context("HSPA preparation lead overflow")?;
        let next = first_unprepared(entry.next_frame, entry.frame.as_ref())
            .map_err(|error| anyhow!("HSPA preparation cursor: {error:?}"))?;
        let first = ChipInstant {
            epoch: identity.ue.epoch,
            index: launch
                .threshold
                .index
                .max(clock.next_frame.index)
                .max(next.index)
                .checked_add(preparation_chips)
                .context("HSPA publication threshold overflow")?,
        };
        let deadline = next_boundary(clock, first, floor.period_frames.get() as u64)
            .map_err(|error| anyhow!("HSPA publication boundary: {error:?}"))?;
        let transaction = radio
            .next_hspa_activation_transaction(identity.ue)
            .map_err(|error| anyhow!("HSPA RRC transaction: {error:?}"))?;
        let mut activation = activation_at(clock, launch.now, first, period)
            .map_err(|error| anyhow!("HSPA initial activation time: {error:?}"))?;
        for _ in 0..MAX_SIZE_PASSES {
            let construction = construct(active.plan().configuration().clone(), activation)?;
            ensure!(
                Arc::ptr_eq(construction.dedicated(), active.plan().configuration())
                    && construction.activation() == activation,
                "HSPA construction must preserve the forecast configuration and activation"
            );
            let encoded = construction
                .message(transaction)
                .map_err(|error| anyhow!("HSPA activation encoding: {error:?}"))?;
            let forecast = radio
                .control_am_forecast(identity, encoded.bit_length)
                .map_err(|error| anyhow!("HSPA protected control forecast: {error:?}"))?;
            ensure!(
                forecast.pdu_bytes == floor.pdu_bytes,
                "HSPA forecast disagrees with the active control bearer PDU size"
            );
            let end = delivery_end(
                clock,
                deadline,
                floor,
                forecast.am.total_new_pdus,
                forecast.am.outstanding_pdus,
                DELIVERY_TIMING,
            )
            .map_err(|error| anyhow!("HSPA control delivery budget: {error:?}"))?;
            let planned = activation_at(clock, launch.now, end, period)
                .map_err(|error| anyhow!("HSPA delivery activation boundary: {error:?}"))?;
            if planned == activation {
                return umts_rrc::dedicated::ActivationProcedure::new(
                    identity.ue,
                    transaction,
                    construction,
                    launch.now,
                    deadline,
                    response_guard,
                )
                .map(Box::new)
                .map_err(|error| anyhow!("HSPA activation procedure: {error:?}"));
            }
            activation = planned;
        }
        anyhow::bail!("HSPA encoded-size delivery budget did not converge")
    }
}
