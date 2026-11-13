// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{ControlAmServiceFloor, DownlinkClock};
use std::num::NonZeroUsize;

pub(super) const MAX_ACTIVATION_FRAMES: u8 = u8::MAX;
pub(super) const MAX_SIZE_PASSES: usize = 4;
const CONTROL_SERVICE_SLOTS_PER_DATA: usize = 2;
const MILLISECONDS_PER_FRAME: u64 = 10;

fn acknowledgement_wait_frames() -> u64 {
    let profile = umts_rrc::establishment::SignallingProfile;
    // TS 25.322 9.5(a,f) permits a poll retry followed by a prohibited STATUS report.
    (u64::from(profile.poll_timeout_ms()) + u64::from(profile.peer_status_prohibit_ms()))
        .div_ceil(MILLISECONDS_PER_FRAME)
}

#[derive(Debug)]
pub(super) struct AcknowledgedDelivery {
    activation: ChipInstant,
    acknowledgement_deadline: ChipInstant,
    confirmed: bool,
    aborting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryDecision {
    Pending,
    Commit,
    Release,
    Failed,
}

impl AcknowledgedDelivery {
    pub fn new(activation: ChipInstant, timing: RrcDeliveryTiming) -> Result<Self, AdmissionError> {
        let processing = timing
            .n1_frames
            .checked_mul(CHIPS_PER_FRAME)
            .ok_or(AdmissionError::Time)?;
        let index = activation
            .index
            .checked_sub(processing)
            .ok_or(AdmissionError::Time)?;
        Ok(Self {
            activation,
            acknowledgement_deadline: ChipInstant {
                epoch: activation.epoch,
                index,
            },
            confirmed: false,
            aborting: false,
        })
    }

    pub fn poll(
        &mut self,
        identity: ReceiveIdentity,
        receipt: umts_rlc::acknowledged::SduReceipt,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<bool, AdmissionError> {
        if self.confirmed {
            return self
                .observe(now, true)
                .map(|decision| decision == DeliveryDecision::Commit);
        }
        let transmission = radio.bearer_setup_transmission(identity.ue, receipt);
        let previously_aborting = self.aborting;
        if transmission.is_err() || matches!(transmission, Ok(SduTransmission::Discarded)) {
            self.aborting = true;
        }
        let decision = self.observe(
            now,
            matches!(transmission, Ok(SduTransmission::Acknowledged)),
        )?;
        match decision {
            DeliveryDecision::Commit => {
                tracing::debug!(
                    "Transition delivery acknowledged: identity={identity:?} now={now:?} deadline={:?} activation={:?}",
                    self.acknowledgement_deadline,
                    self.activation
                );
                return Ok(true);
            }
            DeliveryDecision::Pending => return Ok(false),
            DeliveryDecision::Failed => return Err(AdmissionError::Time),
            DeliveryDecision::Release => (),
        }
        if !previously_aborting {
            tracing::warn!(
                "Transition delivery deadline missed: identity={identity:?} now={now:?} deadline={:?} activation={:?} transmission={transmission:?} action=protected_um_release",
                self.acknowledgement_deadline,
                self.activation
            );
        }
        match radio.abort_transition_delivery(identity) {
            Ok(()) => (),
            Err(ref error) if crate::signalling::scheduling::backpressure(error) => (),
            Err(error) => return Err(AdmissionError::Signalling(error)),
        }
        if radio.transition_release_complete(identity.ue) {
            return Err(AdmissionError::Time);
        }
        Ok(false)
    }

    fn observe(
        &mut self,
        now: ChipInstant,
        acknowledged: bool,
    ) -> Result<DeliveryDecision, AdmissionError> {
        if now.epoch != self.activation.epoch {
            return Err(AdmissionError::Time);
        }
        if self.confirmed {
            return Ok(DeliveryDecision::Commit);
        }
        if !self.aborting && acknowledged && now.index < self.acknowledgement_deadline.index {
            self.confirmed = true;
            return Ok(DeliveryDecision::Commit);
        }
        if self.aborting || now.index >= self.acknowledgement_deadline.index {
            self.aborting = true;
            return Ok(if now.index >= self.activation.index {
                DeliveryDecision::Failed
            } else {
                DeliveryDecision::Release
            });
        }
        Ok(DeliveryDecision::Pending)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RrcDeliveryTiming {
    pub n1_frames: u64,
    pub guard_periods: usize,
}

pub(super) const ADDITION_TIMING: RrcDeliveryTiming = RrcDeliveryTiming {
    n1_frames: 10,
    guard_periods: 2,
};

pub(super) const RELEASE_TIMING: RrcDeliveryTiming = RrcDeliveryTiming {
    n1_frames: 10,
    guard_periods: 2,
};

pub(super) fn joint_period(
    previous: &crate::rel99_plan::CompiledRel99,
    target: &crate::rel99_plan::CompiledRel99,
) -> Result<u64, AdmissionError> {
    let mut period = 1u64;
    for tfcs in [
        previous.uplink_configured(),
        previous.downlink_configured(),
        target.uplink_configured(),
        target.downlink_configured(),
    ] {
        for id in tfcs.transport_ids() {
            let frames = tfcs
                .definition(id)
                .ok_or(AdmissionError::Configuration)?
                .interval
                .frames() as u64;
            if !frames.is_power_of_two() {
                return Err(AdmissionError::Configuration);
            }
            period = period.max(frames);
        }
    }
    Ok(period)
}

pub(super) fn first_unprepared(
    next: ChipInstant,
    frame: Option<&FrameInput>,
) -> Result<ChipInstant, AdmissionError> {
    let Some(frame) = frame else { return Ok(next) };
    if frame.start.epoch != next.epoch {
        return Err(AdmissionError::Identity);
    }
    Ok(ChipInstant {
        epoch: next.epoch,
        index: next.index.max(
            frame
                .start
                .index
                .checked_add(CHIPS_PER_FRAME)
                .ok_or(AdmissionError::Time)?,
        ),
    })
}

pub(super) fn next_boundary(
    clock: DownlinkClock,
    threshold: ChipInstant,
    period: u64,
) -> Result<ChipInstant, AdmissionError> {
    if clock.anchor.time.epoch != threshold.epoch || threshold.index < clock.anchor.time.index {
        return Err(AdmissionError::Time);
    }
    let delta = (threshold.index - clock.anchor.time.index).div_ceil(CHIPS_PER_FRAME);
    let logical = clock
        .first_frame
        .checked_add(delta)
        .ok_or(AdmissionError::Time)?;
    let aligned = logical
        .div_ceil(period)
        .checked_mul(period)
        .ok_or(AdmissionError::Time)?;
    let frames = aligned
        .checked_sub(clock.first_frame)
        .ok_or(AdmissionError::Time)?;
    Ok(ChipInstant {
        epoch: threshold.epoch,
        index: clock
            .anchor
            .time
            .index
            .checked_add(
                frames
                    .checked_mul(CHIPS_PER_FRAME)
                    .ok_or(AdmissionError::Time)?,
            )
            .ok_or(AdmissionError::Time)?,
    })
}

pub(super) fn activation_at(
    clock: DownlinkClock,
    now: ChipInstant,
    threshold: ChipInstant,
    period: u64,
) -> Result<umts_rrc::dedicated::ScheduledActivation, AdmissionError> {
    let target = next_boundary(clock, threshold, period)?;
    if now.epoch != target.epoch || now.index < clock.anchor.time.index {
        return Err(AdmissionError::Time);
    }
    let elapsed = (now.index - clock.anchor.time.index) / CHIPS_PER_FRAME;
    let current = ChipInstant {
        epoch: now.epoch,
        index: clock
            .anchor
            .time
            .index
            .checked_add(
                elapsed
                    .checked_mul(CHIPS_PER_FRAME)
                    .ok_or(AdmissionError::Time)?,
            )
            .ok_or(AdmissionError::Time)?,
    };
    let ahead = target
        .index
        .checked_sub(current.index)
        .ok_or(AdmissionError::Time)?
        / CHIPS_PER_FRAME;
    let ahead = u8::try_from(ahead)
        .ok()
        .and_then(std::num::NonZeroU8::new)
        .ok_or_else(|| {
            tracing::warn!(
                ahead_frames = ahead,
                maximum_frames = MAX_ACTIVATION_FRAMES,
                ?now,
                ?target,
                "RRC activation outside the bounded CFN window"
            );
            AdmissionError::Time
        })?;
    umts_rrc::dedicated::ScheduledActivation::new(
        current,
        clock.anchor.cfn.wrapping_add(elapsed as u8),
        ahead,
    )
    .map_err(AdmissionError::Procedure)
}

pub(super) fn delivery_end(
    clock: DownlinkClock,
    first: ChipInstant,
    floor: ControlAmServiceFloor,
    new_pdus: usize,
    outstanding: usize,
    timing: RrcDeliveryTiming,
) -> Result<ChipInstant, AdmissionError> {
    let first = next_boundary(clock, first, floor.period_frames.get() as u64)?;
    let pdus = new_pdus
        .checked_add(outstanding)
        .ok_or(AdmissionError::Time)?;
    let reserved_slots = pdus
        .checked_mul(CONTROL_SERVICE_SLOTS_PER_DATA)
        .ok_or(AdmissionError::Time)?;
    let periods = reserved_slots
        .div_ceil(floor.pdus_per_period.get())
        .checked_add(timing.guard_periods)
        .ok_or(AdmissionError::Time)?;
    let frames = periods
        .checked_mul(floor.period_frames.get())
        .and_then(|value| u64::try_from(value).ok())
        .and_then(|value| value.checked_add(acknowledgement_wait_frames()))
        .and_then(|value| value.checked_add(timing.n1_frames))
        .ok_or(AdmissionError::Time)?;
    Ok(ChipInstant {
        epoch: first.epoch,
        index: first
            .index
            .checked_add(
                frames
                    .checked_mul(CHIPS_PER_FRAME)
                    .ok_or(AdmissionError::Time)?,
            )
            .ok_or(AdmissionError::Time)?,
    })
}

impl Rel99Admission {
    pub(super) fn delivery_clock(
        &self,
        previous: ReceiveIdentity,
        receive: &mut DedicatedController,
        maximum: NonZeroUsize,
    ) -> Result<(DownlinkClock, ControlAmServiceFloor), AdmissionError> {
        receive
            .with_active_mapping(previous, |bank| {
                Ok((
                    bank.downlink_clock(previous)?,
                    bank.control_am_service_floor(previous, maximum)?,
                ))
            })
            .map_err(AdmissionError::Receive)?
            .map_err(|error| AdmissionError::Frame(FrameBuildError::Bearer(error)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        dedicated_rx::FrameAnchor,
        test_support::{at, count},
    };

    fn clock() -> DownlinkClock {
        DownlinkClock {
            anchor: FrameAnchor {
                time: at(257),
                cfn: 255,
            },
            first_frame: 255,
            next_frame: at(257),
        }
    }

    #[test]
    fn complete_rlc_ack_is_required_before_processing_deadline_and_abort_is_terminal() {
        let activation = at(100 * CHIPS_PER_FRAME);
        let mut gate = AcknowledgedDelivery::new(activation, ADDITION_TIMING).unwrap();
        let before = at(gate.acknowledgement_deadline.index - 1);
        assert_eq!(
            gate.observe(before, false).unwrap(),
            DeliveryDecision::Pending
        );
        assert_eq!(
            gate.observe(before, true).unwrap(),
            DeliveryDecision::Commit
        );
        assert_eq!(
            gate.observe(activation, false).unwrap(),
            DeliveryDecision::Commit
        );
        let mut gate = AcknowledgedDelivery::new(activation, ADDITION_TIMING).unwrap();
        assert_eq!(
            gate.observe(gate.acknowledgement_deadline, true).unwrap(),
            DeliveryDecision::Release
        );
        assert_eq!(
            gate.observe(at(gate.acknowledgement_deadline.index + 1), true)
                .unwrap(),
            DeliveryDecision::Release
        );
        assert_eq!(
            gate.observe(activation, true).unwrap(),
            DeliveryDecision::Failed
        );
    }

    #[test]
    fn joint_boundary_uses_logical_cfn_and_preserves_physical_offset() {
        let clock = clock();
        let threshold = at(clock.anchor.time.index + 1);
        let boundary = next_boundary(clock, threshold, 4).unwrap();
        assert_eq!(boundary, at(clock.anchor.time.index + CHIPS_PER_FRAME));
        let activation = activation_at(clock, clock.anchor.time, threshold, 4).unwrap();
        assert_eq!(activation.cfn(), 0);
        assert_eq!(activation.time(), boundary);
    }

    #[test]
    fn activation_rejects_stale_time_and_unrepresentable_distance() {
        let clock = clock();
        assert!(activation_at(clock, clock.anchor.time, at(clock.anchor.time.index), 1).is_err());
        let far =
            at(clock.anchor.time.index + (u64::from(MAX_ACTIVATION_FRAMES) + 1) * CHIPS_PER_FRAME);
        assert!(activation_at(clock, clock.anchor.time, far, 1).is_err());
        let last = at(clock.anchor.time.index + u64::from(MAX_ACTIVATION_FRAMES) * CHIPS_PER_FRAME);
        assert_eq!(
            activation_at(clock, clock.anchor.time, last, 1)
                .unwrap()
                .time(),
            last
        );
        assert!(next_boundary(clock, at(0), 1).is_err());
    }

    #[test]
    fn delivery_budget_includes_joint_alignment_old_data_and_ue_processing() {
        let clock = clock();
        let floor = ControlAmServiceFloor {
            period_frames: count(4),
            pdus_per_period: count(2),
            pdu_bytes: count(16),
        };
        let end = delivery_end(
            clock,
            at(clock.anchor.time.index + 1),
            floor,
            5,
            3,
            ADDITION_TIMING,
        )
        .unwrap();
        let first = next_boundary(clock, at(clock.anchor.time.index + 1), 4).unwrap();
        let frames = (8 + ADDITION_TIMING.guard_periods) as u64 * 4
            + acknowledgement_wait_frames()
            + ADDITION_TIMING.n1_frames;
        assert_eq!(end, at(first.index + frames * CHIPS_PER_FRAME));
        let without_old = delivery_end(
            clock,
            at(clock.anchor.time.index + 1),
            floor,
            5,
            0,
            ADDITION_TIMING,
        )
        .unwrap();
        assert_eq!(end.index - without_old.index, 12 * CHIPS_PER_FRAME);
    }

    #[test]
    fn release_timing_preserves_n1_and_guards() {
        assert_eq!(RELEASE_TIMING.n1_frames, 10);
        assert_eq!(RELEASE_TIMING.guard_periods, 2);
        assert_eq!(acknowledgement_wait_frames(), 40);
    }

    #[test]
    fn release_budget_recovers_boundary_fragment_loss_with_peer_status_delay() {
        use std::collections::VecDeque;
        use umts_rlc::acknowledged::{AmEntity, AmLimits, PublicationKind};
        use umts_rrc::establishment::SignallingProfile;

        const DOWNLINK_BYTES: usize = 42;
        const UPLINK_BYTES: usize = 29;
        const RELEASE_BYTES: usize = 150;
        const TTI_FRAMES: u64 = 4;
        const MAX_SIMULATION_FRAMES: u64 = 128;
        let period = TTI_FRAMES * CHIPS_PER_FRAME;
        let clock = DownlinkClock {
            anchor: FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            first_frame: 0,
            next_frame: at(0),
        };
        let configuration = SignallingProfile.network_am_configuration(
            count(DOWNLINK_BYTES),
            count(UPLINK_BYTES),
            0,
            0,
        );
        let limits = AmLimits {
            maximum_sdu_bytes: count(1024),
            queued_sdus: count(8),
            maximum_buffer_bytes: 1024 * 1024,
        };
        for (feedback_frames, lost_sequence) in [2, 4, 8]
            .into_iter()
            .flat_map(|frames| [0, 3].map(|sequence| (frames, sequence)))
        {
            let mut sender = AmEntity::new(configuration, limits, at(0)).unwrap();
            let mut peer = AmEntity::new(
                umts_rlc::acknowledged::AmConfig {
                    transmit_pdu_bytes: configuration.receive_pdu_bytes,
                    receive_pdu_bytes: configuration.transmit_pdu_bytes,
                    status_prohibit_chips: NonZeroU64::new(
                        u64::from(SignallingProfile.peer_status_prohibit_ms()) * CHIPS_PER_FRAME
                            / MILLISECONDS_PER_FRAME,
                    ),
                    ..configuration
                },
                limits,
                at(0),
            )
            .unwrap();
            let payload = [0x5a; RELEASE_BYTES];
            let forecast = sender.forecast_delivery(payload.len()).unwrap();
            assert_eq!(forecast.total_new_pdus, 4);
            let receipt = sender.enqueue_tracked(&payload).unwrap();
            let end = delivery_end(
                clock,
                at(0),
                ControlAmServiceFloor {
                    period_frames: count(TTI_FRAMES as usize),
                    pdus_per_period: count(1),
                    pdu_bytes: count(DOWNLINK_BYTES),
                },
                forecast.total_new_pdus,
                0,
                RELEASE_TIMING,
            )
            .unwrap();
            let activation = next_boundary(clock, end, TTI_FRAMES).unwrap();
            let mut downlink = VecDeque::new();
            let mut uplink = VecDeque::new();
            let mut dropped = false;
            let mut delivered = None;
            let mut acknowledged = None;
            for frame in 0..MAX_SIMULATION_FRAMES {
                let now = frame * CHIPS_PER_FRAME;
                while downlink.front().is_some_and(|(due, _)| *due <= now) {
                    let (_, bytes): (u64, Vec<u8>) = downlink.pop_front().unwrap();
                    peer.receive(at(now), &bytes).unwrap();
                }
                while uplink.front().is_some_and(|(due, _)| *due <= now) {
                    let (_, bytes): (u64, Vec<u8>) = uplink.pop_front().unwrap();
                    sender.receive(at(now), &bytes).unwrap();
                }
                for _ in 0..forecast.total_new_pdus {
                    if peer
                        .deliver_with(|delivery| {
                            for sdu in delivery.sdus() {
                                assert_eq!(sdu, payload);
                                assert!(delivered.replace(now).is_none());
                            }
                            Ok::<_, ()>(())
                        })
                        .unwrap()
                        .is_none()
                    {
                        break;
                    }
                }
                if matches!(
                    sender.transmission(receipt).unwrap(),
                    SduTransmission::Acknowledged
                ) {
                    acknowledged = Some(now);
                    break;
                }
                if now.is_multiple_of(period) {
                    sender
                        .publish_next(at(now), |publication| {
                            if matches!(publication.kind, PublicationKind::Data { sequence, .. } if sequence == lost_sequence)
                                && !dropped
                            {
                                dropped = true;
                            } else {
                                downlink.push_back((now + period, publication.bytes.to_vec()));
                            }
                            Ok::<_, ()>(())
                        })
                        .unwrap();
                    peer.publish_next(at(now), |publication| {
                        uplink.push_back((
                            now + feedback_frames * CHIPS_PER_FRAME,
                            publication.bytes.to_vec(),
                        ));
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                }
            }
            let received = delivered.expect("bounded missing-fragment recovery");
            assert!(dropped);
            assert!(sender.counters().retransmitted > 0);
            assert!(received + RELEASE_TIMING.n1_frames * CHIPS_PER_FRAME <= activation.index);
            assert_eq!(sender.counters().resets, 0);
            let acknowledged = acknowledged.expect("bounded complete RLC acknowledgement");
            assert!(acknowledged + RELEASE_TIMING.n1_frames * CHIPS_PER_FRAME < activation.index);
        }
    }
}
