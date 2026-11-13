// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::ControlAmServiceFloor,
    rel99_admission::delivery::{
        ADDITION_TIMING, MAX_SIZE_PASSES, activation_at, delivery_end, first_unprepared,
        joint_period, next_boundary,
    },
    rel99_plan::CompiledRel99,
    rel99_planner::PreparedExisting,
};
use std::num::NonZeroUsize;
use umts_rrc::dedicated::{ExistingDchSetup, ScheduledActivation};
use umts_rrc_codec::EncodedRrc;

#[derive(Clone, Copy, Debug)]
pub struct AdditionalLaunch {
    pub now: ChipInstant,
    pub threshold: ChipInstant,
    pub maximum_combinations: NonZeroUsize,
}

#[derive(Debug)]
pub struct AdditionalDelivery {
    pub(super) acknowledgement: super::super::delivery::AcknowledgedDelivery,
    pub activation: ScheduledActivation,
    pub publication_deadline: ChipInstant,
    pub wire_bits: usize,
    encoded: EncodedRrc,
    floor: ControlAmServiceFloor,
}

impl AdditionalDelivery {
    pub(super) fn matches(&self, encoded: &EncodedRrc) -> bool {
        self.encoded == *encoded
    }
}

impl Rel99Admission {
    pub(super) fn provisional_addition(
        &self,
        previous: ReceiveIdentity,
        target: &CompiledRel99,
        receive: &mut DedicatedController,
        launch: AdditionalLaunch,
    ) -> Result<ScheduledActivation, AdmissionError> {
        if launch.now.epoch != launch.threshold.epoch || launch.threshold.index <= launch.now.index
        {
            return Err(AdmissionError::Time);
        }
        let old = self.active_radio(previous)?;
        let clock = receive
            .with_active_mapping(previous, |bank| bank.downlink_clock(previous))
            .map_err(AdmissionError::Receive)?
            .map_err(|error| AdmissionError::Frame(FrameBuildError::Bearer(error)))?;
        activation_at(
            clock,
            launch.now,
            launch.threshold,
            joint_period(old.plan(), target)?,
        )
    }

    pub(super) fn plan_additional_delivery(
        &self,
        previous: ReceiveIdentity,
        prepared: &mut PreparedExisting,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
        launch: AdditionalLaunch,
    ) -> Result<AdditionalDelivery, AdmissionError> {
        let retained_hsdpa = receive
            .with_active_mapping(previous, |bank| bank.retained_hsdpa_configuration(previous))
            .map_err(AdmissionError::Receive)?
            .map_err(|error| AdmissionError::Frame(FrameBuildError::Bearer(error)))?;
        prepared.setup = ExistingDchSetup::new(
            prepared.setup.previous().clone(),
            prepared.setup.configuration().clone(),
            prepared.setup.rabs().to_vec(),
            prepared.setup.activation(),
        )
        .map_err(AdmissionError::Procedure)?
        .with_retained_hsdpa(retained_hsdpa);
        let entry = self.entry(previous).ok_or(AdmissionError::Identity)?;
        let old = entry.pool.as_ref().ok_or(AdmissionError::Phase)?.plan();
        let period = joint_period(old, prepared.change.plan())?;
        let (clock, floor) = self.delivery_clock(previous, receive, launch.maximum_combinations)?;
        let preparation_frames = self
            .limits
            .maximum_lead_frames
            .get()
            .checked_add(self.limits.prefill_frames.get())
            .ok_or(AdmissionError::Time)?;
        let preparation_chips = u64::try_from(preparation_frames)
            .ok()
            .and_then(|value| value.checked_mul(CHIPS_PER_FRAME))
            .ok_or(AdmissionError::Time)?;
        let next = first_unprepared(entry.next_frame, entry.frame.as_ref())?;
        let first = ChipInstant {
            epoch: previous.ue.epoch,
            index: launch
                .threshold
                .index
                .max(clock.next_frame.index)
                .max(next.index)
                .checked_add(preparation_chips)
                .ok_or(AdmissionError::Time)?,
        };
        let deadline = next_boundary(clock, first, floor.period_frames.get() as u64)?;
        let transaction = radio
            .next_bearer_setup_transaction(previous.ue)
            .map_err(AdmissionError::Signalling)?;
        for _ in 0..MAX_SIZE_PASSES {
            let encoded = prepared
                .setup
                .message(transaction)
                .map_err(AdmissionError::Procedure)?;
            let forecast = radio
                .additional_setup_forecast(previous, encoded.bit_length)
                .map_err(AdmissionError::Signalling)?;
            if forecast.pdu_bytes != floor.pdu_bytes {
                return Err(AdmissionError::Configuration);
            }
            let end = delivery_end(
                clock,
                deadline,
                floor,
                forecast.am.total_new_pdus,
                forecast.am.outstanding_pdus,
                ADDITION_TIMING,
            )?;
            let activation = activation_at(clock, launch.now, end, period)?;
            if activation == prepared.setup.activation() {
                return Ok(AdditionalDelivery {
                    acknowledgement: super::super::delivery::AcknowledgedDelivery::new(
                        activation.time(),
                        ADDITION_TIMING,
                    )?,
                    activation,
                    publication_deadline: deadline,
                    wire_bits: forecast.wire_bits,
                    encoded,
                    floor,
                });
            }
            prepared.setup = ExistingDchSetup::new(
                prepared.setup.previous().clone(),
                prepared.setup.configuration().clone(),
                prepared.setup.rabs().to_vec(),
                activation,
            )
            .map_err(AdmissionError::Procedure)?
            .with_retained_hsdpa(prepared.setup.retained_hsdpa().cloned());
        }
        Err(AdmissionError::Time)
    }
}

pub(super) fn validate_publication(
    identity: ReceiveIdentity,
    next_frame: ChipInstant,
    frame: Option<&FrameInput>,
    addition: &ScheduledAddition,
    now: ChipInstant,
    radio: &CommonSignalling,
    receive: &mut DedicatedController,
) -> Result<(), AdmissionError> {
    let Some(delivery) = addition.delivery.as_ref() else {
        return Ok(());
    };
    if !delivery.matches(addition.procedure.encoded())
        || now.index >= delivery.publication_deadline.index
    {
        return Err(AdmissionError::Time);
    }
    let clock = receive
        .with_active_mapping(identity, |bank| bank.downlink_clock(identity))
        .map_err(AdmissionError::Receive)?
        .map_err(|error| AdmissionError::Frame(FrameBuildError::Bearer(error)))?;
    let floor = delivery.floor;
    let forecast = radio
        .additional_setup_forecast(identity, addition.procedure.encoded().bit_length)
        .map_err(AdmissionError::Signalling)?;
    if forecast.pdu_bytes != floor.pdu_bytes || forecast.wire_bits != delivery.wire_bits {
        return Err(AdmissionError::Configuration);
    }
    let next = first_unprepared(next_frame, frame)?;
    let first = ChipInstant {
        epoch: now.epoch,
        index: clock.next_frame.index.max(next.index).max(now.index),
    };
    let end = delivery_end(
        clock,
        first,
        floor,
        forecast.am.total_new_pdus,
        forecast.am.outstanding_pdus,
        ADDITION_TIMING,
    )?;
    if end.index > delivery.activation.time().index {
        tracing::warn!(
            ?identity,
            ?now,
            ?first,
            ?end,
            activation = ?delivery.activation.time(),
            ?floor,
            forecast = ?forecast.am,
            "Additional RRC delivery no longer fits its activation budget"
        );
        return Err(AdmissionError::Time);
    }
    Ok(())
}
