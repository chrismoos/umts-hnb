// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    PilotFeedback, PilotObservation, PilotTpcController, PowerControlError, ReceiveError,
    TpcProvenance,
};
use crate::{
    dedicated_tx::{DedicatedTpcPort, TpcEstimate, TransmitCommandId, TransmitError},
    radio_io::RadioClock,
};
use umts_types::time::{CHIPS_PER_SLOT, ChipInstant};

const INFLIGHT_ESTIMATES: usize = 8;

pub struct DedicatedFeedback {
    pub(super) port: DedicatedTpcPort,
    pub(super) clock: RadioClock,
    pub(super) failed: bool,
    minimum_lead: u64,
}

impl DedicatedFeedback {
    #[cfg(test)]
    pub(crate) fn new(port: DedicatedTpcPort, clock: RadioClock) -> Self {
        Self::from_preparation(port, clock, 0)
    }
    pub(crate) fn from_preparation(
        port: DedicatedTpcPort,
        clock: RadioClock,
        minimum_lead: u64,
    ) -> Self {
        Self {
            port,
            clock,
            failed: false,
            minimum_lead,
        }
    }
    pub fn origin(&self) -> crate::radio_io::AirClockOrigin {
        self.clock.origin()
    }
    pub fn minimum_lead_chips(&self) -> u64 {
        self.minimum_lead
    }
    pub(super) fn now(&mut self) -> Result<Option<ChipInstant>, ReceiveError> {
        if self.failed {
            return Err(ReceiveError::FeedbackClosed);
        }
        let result = self
            .clock
            .read_upper_bound()
            .map_err(|_| ReceiveError::FeedbackClosed)
            .and_then(|time| {
                time.chip
                    .map(|chip| {
                        chip.index
                            .checked_add(1)
                            .map(|index| ChipInstant { index, ..chip })
                            .ok_or(ReceiveError::FeedbackClosed)
                    })
                    .transpose()
            });
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PilotTpcCounters {
    pub produced: u64,
    pub submitted: u64,
    pub accepted: u64,
    pub backpressure: u64,
    pub rejected: u64,
    pub retired: u64,
    pub stale_observations: u64,
    pub unusable_observations: u64,
    pub publication_clock_waits: u64,
    pub received_downlink_up: u64,
    pub received_downlink_down: u64,
    pub received_downlink_undecided: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct RejectedPilotObservation {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub reason: PowerControlError,
    #[expect(
        dead_code,
        reason = "Owns the rejected operation until it is retried or dropped."
    )]
    pub uplink_slot: ChipInstant,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub measurement_end: umts_types::time::SampleInstant,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub ready_at: ChipInstant,
    #[expect(
        dead_code,
        reason = "Owns the rejected operation until it is retried or dropped."
    )]
    pub pilot: umts_dsp::pilots::PilotEstimate,
}

#[derive(Clone, Debug)]
pub struct PilotTpcStatus {
    pub counters: PilotTpcCounters,
    pub last_accepted: Option<TpcProvenance>,
    pub last_command: Option<umts_dsp::bits::Bit>,
    pub last_sir_linear: Option<f64>,
    pub last_observed_control: Option<(ChipInstant, umts_dsp::dedicated::UplinkControl)>,
    pub failure: Option<ReceiveError>,
}

pub(super) struct PilotPowerOwner {
    pub controller: PilotTpcController,
    pub pending: Option<PilotFeedback>,
    pub queued: bool,
    inflight: [Option<(TransmitCommandId, PilotFeedback)>; INFLIGHT_ESTIMATES],
    inflight_head: usize,
    inflight_count: usize,
    minimum_lead: u64,
    status: PilotTpcStatus,
    pub(super) last_discard: Option<RejectedPilotObservation>,
    pub(super) failed_observation: Option<RejectedPilotObservation>,
    pub(super) maximum_observation_age_chips: u64,
    pub(super) maximum_publication_age_chips: u64,
    stop_at: Option<ChipInstant>,
    retiring: bool,
}

impl PilotPowerOwner {
    pub fn new(controller: PilotTpcController, minimum_lead: u64) -> Self {
        Self {
            controller,
            pending: None,
            queued: false,
            inflight: [None; INFLIGHT_ESTIMATES],
            inflight_head: 0,
            inflight_count: 0,
            minimum_lead,
            status: PilotTpcStatus {
                counters: PilotTpcCounters::default(),
                last_accepted: None,
                last_command: None,
                last_sir_linear: None,
                last_observed_control: None,
                failure: None,
            },
            stop_at: None,
            last_discard: None,
            failed_observation: None,
            maximum_observation_age_chips: 0,
            maximum_publication_age_chips: 0,
            retiring: false,
        }
    }

    pub fn status(&self) -> PilotTpcStatus {
        self.status.clone()
    }
    pub fn minimum_lead(&self) -> u64 {
        self.minimum_lead
    }
    pub fn failed(&self) -> bool {
        self.status.failure.is_some()
    }
    pub fn downlink_anchor(&self) -> ChipInstant {
        self.controller.downlink_anchor()
    }

    pub fn stop_at(&mut self, boundary: ChipInstant) -> Result<(), ReceiveError> {
        let anchor = self.downlink_anchor();
        if boundary.epoch != anchor.epoch
            || boundary.index < anchor.index
            || !(boundary.index - anchor.index).is_multiple_of(CHIPS_PER_SLOT)
        {
            return Err(ReceiveError::Time);
        }
        self.stop_at = Some(boundary);
        if self.pending.is_some_and(|feedback| {
            feedback.provenance.preparation_frontier.index >= boundary.index
        }) {
            self.pending = None;
            self.retiring = true;
            self.status.counters.retired = self.status.counters.retired.saturating_add(1);
        }
        if self
            .inflight
            .iter()
            .flatten()
            .any(|(_, feedback)| feedback.provenance.preparation_frontier.index >= boundary.index)
        {
            self.retiring = true;
        }
        Ok(())
    }

    pub fn observing(&mut self, now: ChipInstant) -> Result<bool, ReceiveError> {
        if self.retiring {
            return Ok(false);
        }
        if let Some(boundary) = self.stop_at {
            let anchor = self.downlink_anchor();
            let threshold = now
                .index
                .checked_add(self.minimum_lead)
                .ok_or(ReceiveError::Time)?
                .max(anchor.index);
            let slots = (threshold - anchor.index).div_ceil(CHIPS_PER_SLOT);
            let first = slots
                .checked_mul(CHIPS_PER_SLOT)
                .and_then(|delta| anchor.index.checked_add(delta))
                .ok_or(ReceiveError::Time)?;
            if first >= boundary.index {
                self.retiring = true;
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn needs_queue(&self) -> bool {
        self.pending.is_some() && !self.queued && self.status.failure.is_none()
    }

    pub fn ready(&mut self) -> Result<bool, ReceiveError> {
        if let Some(error) = &self.status.failure {
            return Err(error.clone());
        }
        if !self.retiring && (self.pending.is_some() || self.inflight_count == INFLIGHT_ESTIMATES) {
            self.status.counters.backpressure = self.status.counters.backpressure.saturating_add(1);
        }
        Ok(self.retiring || self.pending.is_none() && self.inflight_count < INFLIGHT_ESTIMATES)
    }

    pub fn produced(&mut self, feedback: PilotFeedback) {
        self.maximum_observation_age_chips = self.maximum_observation_age_chips.max(
            feedback
                .provenance
                .ready_at
                .index
                .saturating_sub(feedback.estimate.observed_at().index),
        );
        if self.stop_at.is_some_and(|boundary| {
            feedback.provenance.preparation_frontier.index >= boundary.index
        }) {
            self.retiring = true;
            self.status.counters.retired = self.status.counters.retired.saturating_add(1);
            return;
        }
        self.pending = Some(feedback);
        self.status.counters.produced = self.status.counters.produced.saturating_add(1);
    }

    pub fn observed_control(
        &mut self,
        at: ChipInstant,
        control: umts_dsp::dedicated::UplinkControl,
    ) {
        self.status.last_observed_control = Some((at, control));
        let count = if control.tpc.is_finite() && control.tpc < 0.0 {
            &mut self.status.counters.received_downlink_up
        } else if control.tpc.is_finite() && control.tpc > 0.0 {
            &mut self.status.counters.received_downlink_down
        } else {
            &mut self.status.counters.received_downlink_undecided
        };
        *count = count.saturating_add(1);
    }

    pub fn discard_observation(&mut self, error: PowerControlError, observation: PilotObservation) {
        self.last_discard = Some(RejectedPilotObservation {
            reason: error,
            uplink_slot: observation.uplink_slot,
            measurement_end: observation.measurement_end,
            ready_at: observation.ready_at,
            pilot: observation.pilot,
        });
        if error == PowerControlError::Stale {
            self.status.counters.stale_observations =
                self.status.counters.stale_observations.saturating_add(1);
        } else {
            self.status.counters.unusable_observations =
                self.status.counters.unusable_observations.saturating_add(1);
        }
    }
    pub fn fail_observation(&mut self, error: PowerControlError, observation: PilotObservation) {
        self.failed_observation
            .get_or_insert(RejectedPilotObservation {
                reason: error,
                uplink_slot: observation.uplink_slot,
                measurement_end: observation.measurement_end,
                ready_at: observation.ready_at,
                pilot: observation.pilot,
            });
        self.fail(ReceiveError::PowerControl(error));
    }
    pub fn retire_pending(&mut self) {
        if self.pending.take().is_some() {
            self.status.counters.retired = self.status.counters.retired.saturating_add(1);
        }
    }

    pub fn fail(&mut self, error: ReceiveError) {
        if self.status.failure.is_none() {
            self.status.failure = Some(error);
            self.status.counters.rejected = self.status.counters.rejected.saturating_add(1);
        }
    }

    pub fn publish(
        &mut self,
        port: &mut DedicatedTpcPort,
        now: ChipInstant,
    ) -> Result<bool, ReceiveError> {
        if let Some(error) = &self.status.failure {
            return Err(error.clone());
        }
        if !self.observing(now)? {
            if self.pending.take().is_some() {
                self.status.counters.retired = self.status.counters.retired.saturating_add(1);
            }
            return Ok(false);
        }
        let Some(feedback) = self.pending else {
            return Ok(false);
        };
        if self.inflight_count == INFLIGHT_ESTIMATES {
            self.status.counters.backpressure = self.status.counters.backpressure.saturating_add(1);
            return Ok(false);
        }
        let feedback = match self.controller.for_publication(feedback, now) {
            Ok(updated) => {
                self.pending = Some(updated);
                updated
            }
            Err(PowerControlError::PublicationNotReady) => {
                self.status.counters.publication_clock_waits = self
                    .status
                    .counters
                    .publication_clock_waits
                    .saturating_add(1);
                return Ok(false);
            }
            Err(PowerControlError::Stale) => {
                self.pending = None;
                self.status.counters.retired = self.status.counters.retired.saturating_add(1);
                return Ok(false);
            }
            Err(error) => return Err(ReceiveError::PowerControl(error)),
        };
        self.maximum_publication_age_chips = self.maximum_publication_age_chips.max(
            now.index
                .saturating_sub(feedback.estimate.observed_at().index),
        );
        match port.publish_estimate(feedback.estimate) {
            Ok(id) => {
                self.pending = None;
                let tail = (self.inflight_head + self.inflight_count) % INFLIGHT_ESTIMATES;
                self.inflight[tail] = Some((id, feedback));
                self.inflight_count += 1;
                self.status.counters.submitted = self.status.counters.submitted.saturating_add(1);
                Ok(true)
            }
            Err(estimate) => {
                if estimate != feedback.estimate || !port.available() {
                    self.fail(ReceiveError::FeedbackClosed);
                    return Err(ReceiveError::FeedbackClosed);
                }
                self.status.counters.backpressure =
                    self.status.counters.backpressure.saturating_add(1);
                Ok(false)
            }
        }
    }

    pub fn reconcile(
        &mut self,
        id: TransmitCommandId,
        estimate: TpcEstimate,
        result: Result<(), TransmitError>,
    ) -> Result<bool, ReceiveError> {
        let Some((expected, feedback)) = self.inflight[self.inflight_head] else {
            self.fail(ReceiveError::Identity);
            return Err(ReceiveError::Identity);
        };
        if id != expected || feedback.estimate != estimate {
            self.fail(ReceiveError::Identity);
            return Err(ReceiveError::Identity);
        }
        self.inflight[self.inflight_head] = None;
        self.inflight_head = (self.inflight_head + 1) % INFLIGHT_ESTIMATES;
        self.inflight_count -= 1;
        if self.stop_at.is_some_and(|boundary| {
            feedback.provenance.preparation_frontier.index >= boundary.index
        }) {
            self.status.counters.retired = self.status.counters.retired.saturating_add(1);
            return Ok(false);
        }
        match result {
            Ok(()) => {
                self.status.last_accepted = Some(feedback.provenance);
                self.status.last_command = Some(feedback.estimate.command());
                self.status.last_sir_linear = Some(feedback.estimate.sir_linear());
                self.status.counters.accepted = self.status.counters.accepted.saturating_add(1);
                Ok(true)
            }
            Err(TransmitError::FeedbackLate) => {
                self.status.counters.retired = self.status.counters.retired.saturating_add(1);
                Ok(false)
            }
            Err(error) => {
                let error = ReceiveError::Feedback(error);
                self.fail(error.clone());
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedicated_rx::{
        PilotObservation, PilotSirTarget, PilotTpcSettings, ReceiveIdentity,
    };
    use crate::test_support::at;
    use std::num::NonZeroU64;
    use umts_dsp::{pilots::PilotEstimate, pulse_shaping::SamplesPerChip};
    use umts_types::{UeKey, time::SampleInstant};

    fn owner() -> PilotPowerOwner {
        let identity = ReceiveIdentity {
            ue: UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        };
        let mut controller = PilotTpcController::new(
            identity,
            PilotTpcSettings {
                target: PilotSirTarget::from_db(10.0).unwrap(),
                minimum_coherent_fraction: 0.1,
                uplink_anchor: at(1024),
                downlink_anchor: at(512),
                rate: SamplesPerChip::Two,
                lead_chips: NonZeroU64::MIN,
                maximum_observation_age_chips: NonZeroU64::new(CHIPS_PER_SLOT * 2).unwrap(),
            },
        )
        .unwrap();
        let feedback = controller
            .observe(PilotObservation {
                identity,
                uplink_slot: at(1024),
                measurement_end: SampleInstant {
                    epoch: at(0).epoch,
                    index: 5120,
                },
                ready_at: at(2560),
                sample_generation: 1,
                continuity: 1,
                pilot: PilotEstimate {
                    channel: num_complex::Complex64::new(0.9_f64.sqrt(), 0.0),
                    observed_power: 1.0,
                    residual_power: 0.1,
                },
                pilot_count: 6,
            })
            .unwrap();
        let mut owner = PilotPowerOwner::new(controller, 1);
        owner.produced(feedback);
        owner
    }

    #[test]
    fn pending_feedback_deferrals_and_queue_membership_are_explicit() {
        let mut owner = owner();
        assert!(owner.needs_queue());
        owner.queued = true;
        assert!(!owner.needs_queue());
        assert!(!owner.ready().unwrap());
        assert!(!owner.ready().unwrap());
        assert_eq!(owner.status().counters.backpressure, 2);
        owner.stop_at(at(3072)).unwrap();
        assert!(!owner.needs_queue());
        assert!(owner.ready().unwrap());
        assert_eq!(owner.status().counters.backpressure, 2);
        owner.queued = false;
        assert!(!owner.needs_queue());
    }

    #[test]
    fn replacement_cancels_only_feedback_at_the_physical_downlink_boundary() {
        let mut owner = owner();
        assert_eq!(
            owner.pending.unwrap().provenance.preparation_frontier,
            at(3072)
        );
        assert!(!owner.ready().unwrap());
        owner.stop_at(at(3072)).unwrap();
        assert!(owner.ready().unwrap());
        assert!(owner.pending.is_none());
        assert!(!owner.observing(at(2560)).unwrap());
        assert_eq!(owner.status().counters.retired, 1);
        assert_eq!(owner.status().failure, None);
    }

    #[test]
    fn replacement_cutoff_retains_prior_pending_feedback_and_rejects_wrong_coordinates() {
        let mut owner = owner();
        let decision = owner.pending.unwrap();
        assert_eq!(owner.stop_at(at(3073)), Err(ReceiveError::Time));
        assert_eq!(owner.pending, Some(decision));
        owner.stop_at(at(5632)).unwrap();
        assert_eq!(owner.pending, Some(decision));
        assert!(owner.observing(at(2560)).unwrap());
        assert!(!owner.observing(at(5632)).unwrap());
        assert!(owner.ready().unwrap());
        assert_eq!(owner.status().failure, None);
    }

    #[test]
    fn fatal_observation_retains_first_coordinates_without_counting_a_stale_discard() {
        let mut owner = owner();
        let observation = PilotObservation {
            identity: owner.pending.unwrap().estimate.identity(),
            uplink_slot: at(3584),
            measurement_end: SampleInstant {
                epoch: at(0).epoch,
                index: 10241,
            },
            ready_at: at(5120),
            sample_generation: 1,
            continuity: 1,
            pilot: PilotEstimate {
                channel: num_complex::Complex64::new(1.0, 0.0),
                observed_power: 1.1,
                residual_power: 0.1,
            },
            pilot_count: 6,
        };
        owner.fail_observation(PowerControlError::Time, observation);
        owner.fail_observation(
            PowerControlError::Epoch,
            PilotObservation {
                ready_at: at(9000),
                ..observation
            },
        );
        let retained = owner.failed_observation.unwrap();
        assert_eq!(retained.reason, PowerControlError::Time);
        assert_eq!(retained.measurement_end, observation.measurement_end);
        assert_eq!(retained.ready_at, observation.ready_at);
        assert_eq!(
            owner.status().failure,
            Some(ReceiveError::PowerControl(PowerControlError::Time))
        );
        assert_eq!(owner.status().counters.stale_observations, 0);
        assert_eq!(owner.status().counters.unusable_observations, 0);
        assert_eq!(owner.status().counters.rejected, 1);
    }
}
