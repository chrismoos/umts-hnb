// SPDX-License-Identifier: AGPL-3.0-or-later

use super::super::delivery::{
    MAX_SIZE_PASSES, RELEASE_TIMING, activation_at, delivery_end, first_unprepared, joint_period,
    next_boundary,
};
use super::*;
use crate::{dedicated_bearers::ControlAmServiceFloor, rel99_planner::PreparedRelease};
use umts_rrc::dedicated::ScheduledActivation;
use umts_rrc_codec::EncodedRrc;

#[derive(Debug)]
pub struct ReleaseDelivery {
    pub(super) acknowledgement: super::super::delivery::AcknowledgedDelivery,
    pub activation: ScheduledActivation,
    pub publication_deadline: ChipInstant,
    pub wire_bits: usize,
    encoded: EncodedRrc,
    floor: ControlAmServiceFloor,
}
impl ReleaseDelivery {
    pub(super) fn matches(&self, encoded: &EncodedRrc) -> bool {
        self.encoded == *encoded
    }
}

impl Rel99Admission {
    pub(super) fn provisional_release(
        &self,
        previous: ReceiveIdentity,
        target: &crate::rel99_plan::CompiledRel99,
        receive: &mut DedicatedController,
        launch: crate::rel99_admission::AdditionalLaunch,
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

    pub(super) fn plan_release_delivery(
        &self,
        previous: ReceiveIdentity,
        prepared: &mut PreparedRelease,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
        launch: crate::rel99_admission::AdditionalLaunch,
    ) -> Result<ReleaseDelivery, AdmissionError> {
        let retained_hsdpa = receive
            .with_active_mapping(previous, |bank| bank.retained_hsdpa_configuration(previous))
            .map_err(AdmissionError::Receive)?
            .map_err(|error| AdmissionError::Frame(FrameBuildError::Bearer(error)))?;
        prepared.release = umts_rrc::dedicated::ExistingDchRelease::new(
            prepared.release.previous().clone(),
            prepared.release.configuration().clone(),
            prepared.release.activation(),
        )
        .map_err(AdmissionError::Procedure)?
        .with_retained_hsdpa(retained_hsdpa);
        let entry = self.entry(previous).ok_or(AdmissionError::Identity)?;
        let period = joint_period(&prepared.previous, prepared.plan())?;
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
            .next_bearer_release_transaction(previous.ue)
            .map_err(AdmissionError::Signalling)?;
        for _ in 0..MAX_SIZE_PASSES {
            let encoded = prepared
                .release
                .message(transaction)
                .map_err(AdmissionError::Procedure)?;
            let forecast = radio
                .control_am_forecast(previous, encoded.bit_length)
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
                RELEASE_TIMING,
            )?;
            let activation = activation_at(clock, launch.now, end, period)?;
            if activation == prepared.release.activation() {
                tracing::debug!(
                    "Release delivery plan: previous={previous:?} now={:?} publication_deadline={deadline:?} activation={activation:?} bits={} bytes={:02x?}",
                    launch.now,
                    encoded.bit_length,
                    encoded.bytes
                );
                for (side, plan) in [
                    ("previous", prepared.previous.as_ref()),
                    ("successor", prepared.plan()),
                ] {
                    let configuration = plan.configuration();
                    for channel in configuration.downlink().transports() {
                        let multiplexed = configuration
                            .multiplexed(umts_rrc::dedicated::DchDirection::Downlink, channel.id)
                            .map_err(|_| AdmissionError::Configuration)?;
                        tracing::debug!(
                            "Release downlink transport: previous={previous:?} side={side} id={} interval={:?} coding={:?} crc={:?} attribute={} multiplexed={} groups={:?}",
                            channel.id,
                            channel.interval,
                            channel.coding,
                            channel.crc,
                            channel.rate_matching_attribute,
                            multiplexed,
                            channel.groups
                        );
                        let id = umts_dsp::dedicated_transport::TransportId::new(channel.id)
                            .map_err(|_| AdmissionError::Configuration)?;
                        for (index, format) in channel.formats().enumerate() {
                            let matched = match plan.downlink() {
                                crate::rel99_plan::DownlinkPlans::Fixed(plan) => {
                                    plan.matched_tti_bits(id, index)
                                }
                                crate::rel99_plan::DownlinkPlans::Flexible(plan) => {
                                    plan.matched_tti_bits(id, index)
                                }
                            };
                            tracing::debug!(
                                "Release downlink allocation: previous={previous:?} side={side} id={} format={index} transport_bits={} blocks={} matched_tti_bits={matched:?}",
                                channel.id,
                                format.transport_bits(multiplexed),
                                format.blocks
                            );
                        }
                    }
                }
                return Ok(ReleaseDelivery {
                    acknowledgement: super::super::delivery::AcknowledgedDelivery::new(
                        activation.time(),
                        RELEASE_TIMING,
                    )?,
                    activation,
                    publication_deadline: deadline,
                    wire_bits: forecast.wire_bits,
                    encoded,
                    floor,
                });
            }
            prepared
                .set_activation(activation)
                .map_err(AdmissionError::Procedure)?;
        }
        Err(AdmissionError::Time)
    }
}

pub(super) fn validate_release_publication(
    identity: ReceiveIdentity,
    next_frame: ChipInstant,
    frame: Option<&FrameInput>,
    release: &ScheduledRelease,
    now: ChipInstant,
    radio: &CommonSignalling,
    receive: &mut DedicatedController,
) -> Result<(), AdmissionError> {
    let Some(delivery) = release.delivery.as_ref() else {
        return Ok(());
    };
    let procedure = &release.procedure;
    if now.index >= delivery.publication_deadline.index
        || procedure.activation() != delivery.activation
        || procedure.encoded() != &delivery.encoded
    {
        return Err(AdmissionError::Time);
    }
    let clock = receive
        .with_active_mapping(identity, |bank| bank.downlink_clock(identity))
        .map_err(AdmissionError::Receive)?
        .map_err(|error| AdmissionError::Frame(FrameBuildError::Bearer(error)))?;
    let forecast = radio
        .control_am_forecast(identity, procedure.encoded().bit_length)
        .map_err(AdmissionError::Signalling)?;
    if forecast.pdu_bytes != delivery.floor.pdu_bytes || forecast.wire_bits != delivery.wire_bits {
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
        delivery.floor,
        forecast.am.total_new_pdus,
        forecast.am.outstanding_pdus,
        RELEASE_TIMING,
    )?;
    if end.index > delivery.activation.time().index {
        return Err(AdmissionError::Time);
    }
    Ok(())
}
