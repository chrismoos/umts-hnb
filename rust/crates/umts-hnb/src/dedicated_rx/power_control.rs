// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;

use umts_dsp::{dedicated::DpcchStatistics, pilots::PilotEstimate, pulse_shaping::SamplesPerChip};
use umts_types::time::{CHIPS_PER_SLOT, ChipInstant, SampleInstant};

use crate::{dedicated_rx::ReceiveIdentity, dedicated_tx::TpcEstimate};

#[cfg(test)]
const TPC_LANE_SLOTS: u64 = crate::dedicated_tx::ESTIMATE_SLOTS as u64;
const MINIMUM_PILOT_COUNT: usize = 3;
const MAXIMUM_PILOT_COUNT: usize = 8;
const PILOT_POWER_RELATIVE_TOLERANCE: f64 = 1e-9;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PilotSirTarget {
    db: f64,
    linear: f64,
}

impl PilotSirTarget {
    pub fn from_db(db: f64) -> Result<Self, PowerControlError> {
        if !db.is_finite() {
            return Err(PowerControlError::Configuration);
        }
        let linear = 10.0_f64.powf(db / 10.0);
        if !linear.is_finite() || linear <= 0.0 {
            return Err(PowerControlError::Configuration);
        }
        Ok(Self { db, linear })
    }

    pub fn db(self) -> f64 {
        self.db
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PilotTpcSettings {
    pub target: PilotSirTarget,
    pub minimum_coherent_fraction: f64,
    pub uplink_anchor: ChipInstant,
    pub downlink_anchor: ChipInstant,
    pub rate: SamplesPerChip,
    pub lead_chips: NonZeroU64,
    pub maximum_observation_age_chips: NonZeroU64,
}

#[derive(Clone, Copy, Debug)]
pub struct PilotObservation {
    pub identity: ReceiveIdentity,
    pub uplink_slot: ChipInstant,
    pub measurement_end: SampleInstant,
    pub ready_at: ChipInstant,
    pub sample_generation: u64,
    pub continuity: u64,
    pub pilot: PilotEstimate,
    pub pilot_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerControlError {
    Configuration,
    Identity,
    Epoch,
    Time,
    PublicationNotReady,
    Discontinuity,
    Stale,
    InvalidPilot,
    Coherence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TpcProvenance {
    pub identity: ReceiveIdentity,
    pub uplink_slot: ChipInstant,
    pub measurement_end: SampleInstant,
    pub ready_at: ChipInstant,
    // This bounds lifecycle publication, not when a measurement may drive an unrendered slot.
    pub preparation_frontier: ChipInstant,
    pub sample_generation: u64,
    pub continuity: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PilotFeedback {
    pub estimate: TpcEstimate,
    pub provenance: TpcProvenance,
}

pub struct PilotTpcController {
    identity: ReceiveIdentity,
    settings: PilotTpcSettings,
    previous: Option<PilotObservationKey>,
    noise: Option<f64>,
    actual_sir: Option<f64>,
    frame_disturbance: f64,
    frame_slots: u32,
    settled_frames: u32,
    margin: f64,
    quality_signal: f64,
    quality_disturbance: f64,
    quality_slots: u32,
    quality_rejected: u32,
    diagnostic: FrameDiagnostic,
}

const NOISE_FALL: f64 = 1.0 / 8.0;
const NOISE_RISE_PER_SLOT: f64 = 1.000_002_56;
const ACTUAL_SIR_SMOOTHING: f64 = 1.0 / 16.0;
const MINIMUM_ACTUAL_SIR: f64 = 1.0;
const SWAMPED_PILOT_LIMIT: f64 = 10.0;
const INTERFERED_PILOT_LIMIT: f64 = 2.5;
static EDCH_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(not(test))]
static CELL_FLOOR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static CELL_FLOOR: std::sync::atomic::AtomicU64 = const { std::sync::atomic::AtomicU64::new(0) };
}

fn with_cell_floor<R>(action: impl FnOnce(&std::sync::atomic::AtomicU64) -> R) -> R {
    #[cfg(not(test))]
    return action(&CELL_FLOOR);
    #[cfg(test)]
    return CELL_FLOOR.with(action);
}

fn cell_floor(disturbance: f64) -> f64 {
    use std::sync::atomic::Ordering;
    let stored = f64::from_bits(with_cell_floor(|floor| floor.load(Ordering::Relaxed)));
    let floor = if !(stored.is_finite() && stored > 0.0) {
        disturbance
    } else if disturbance < stored {
        stored + (disturbance - stored) * NOISE_FALL
    } else {
        stored * NOISE_RISE_PER_SLOT
    };
    with_cell_floor(|cell| cell.store(floor.to_bits(), Ordering::Relaxed));
    floor
}

#[cfg(not(test))]
static PILOT_NOISE_RATIO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static PILOT_NOISE_RATIO: std::sync::atomic::AtomicU64 = const { std::sync::atomic::AtomicU64::new(0) };
}

fn with_ratio<R>(action: impl FnOnce(&std::sync::atomic::AtomicU64) -> R) -> R {
    #[cfg(not(test))]
    return action(&PILOT_NOISE_RATIO);
    #[cfg(test)]
    return PILOT_NOISE_RATIO.with(action);
}
const RATIO_SETTLING_FRAMES: u32 = 100;
const RATIO_FRAME_SLOTS: u32 = 15;
const QUIET_RATIO_FALL: f64 = 1.0 / 4.0;
const QUIET_RATIO_RISE_PER_FRAME: f64 = 1.000_001;
const MEASURED_FLOOR_OFFSET: f64 = 5.0;
const MEASURED_INTERFERED_PILOT_LIMIT: f64 = 1.9;
const MAXIMUM_INTERFERED_PILOT_LIMIT: f64 = 6.3;
const MARGIN_RAISE_SIR: f64 = 1.6;
const MARGIN_LOWER_SIR: f64 = 2.5;
const MARGIN_RAISE_REJECTED_SLOTS: u32 = 3;
const MARGIN_RAISE_PER_FRAME: f64 = 1.023;
const MARGIN_LOWER_PER_FRAME: f64 = 1.0023;

fn adapt_margin(margin: f64, frame_sir: f64, rejected: u32) -> f64 {
    let next = if rejected >= MARGIN_RAISE_REJECTED_SLOTS || frame_sir < MARGIN_RAISE_SIR {
        margin * MARGIN_RAISE_PER_FRAME
    } else if frame_sir > MARGIN_LOWER_SIR {
        margin / MARGIN_LOWER_PER_FRAME
    } else {
        margin
    };
    next.clamp(
        MEASURED_INTERFERED_PILOT_LIMIT,
        MAXIMUM_INTERFERED_PILOT_LIMIT,
    )
}

fn pilot_noise_ratio() -> Option<f64> {
    let value = f64::from_bits(with_ratio(|ratio| {
        ratio.load(std::sync::atomic::Ordering::Relaxed)
    }));
    (value.is_finite() && value > 0.0).then_some(value)
}

fn track_quiet_ratio(current: Option<f64>, ratio: f64) -> f64 {
    match current {
        _ if !(ratio.is_finite() && ratio > 0.0) => current.unwrap_or(0.0),
        Some(level) if ratio < level => level + (ratio - level) * QUIET_RATIO_FALL,
        Some(level) => level * QUIET_RATIO_RISE_PER_FRAME,
        None => ratio,
    }
}

pub(crate) fn set_edch_slots(slots: u64) {
    EDCH_SLOTS.store(slots, std::sync::atomic::Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameDiagnostic {
    signal: f64,
    disturbance: f64,
    accepted: u32,
    rejected: u32,
    up: u32,
    edch: bool,
}

impl FrameDiagnostic {
    fn emit(&mut self, identity: ReceiveIdentity, noise: Option<f64>, target: f64, margin: f64) {
        const SLOTS: u32 = 15;
        if self.accepted + self.rejected < SLOTS {
            return;
        }
        if tracing::enabled!(tracing::Level::DEBUG) {
            let decibels = |value: f64| 10.0 * value.max(1e-30).log10();
            let count = f64::from(self.accepted.max(1));
            tracing::debug!(
                "Uplink pilot frame: slot={} configuration={} signal_db={:.1} disturbance_db={:.1} noise_db={:.1} target_db={:.1} accepted={} rejected={} up={} edch={} strip_db={:.1} received_db={:.1} rot_db={:.1} margin_db={:.1}",
                identity.ue.slot,
                identity.configuration,
                decibels(self.signal / count),
                decibels(self.disturbance / count),
                decibels(noise.unwrap_or(0.0)),
                decibels(target),
                self.accepted,
                self.rejected,
                self.up,
                self.edch,
                decibels(crate::uplink_noise::strip_noise().unwrap_or(0.0)),
                decibels(crate::uplink_noise::received_power().unwrap_or(0.0)),
                decibels(crate::uplink_noise::rise_over_thermal().unwrap_or(1.0)),
                decibels(margin),
            );
        }
        *self = Self::default();
    }
}

#[derive(Clone, Copy, Debug)]
struct PilotObservationKey {
    uplink_slot: ChipInstant,
    measurement_end: SampleInstant,
    ready_at: ChipInstant,
    sample_generation: u64,
    continuity: u64,
}

impl PilotTpcController {
    pub(super) fn set_target(&mut self, target: PilotSirTarget) {
        self.settings.target = target;
    }
    pub(super) fn downlink_anchor(&self) -> ChipInstant {
        self.settings.downlink_anchor
    }
    pub fn new(
        identity: ReceiveIdentity,
        settings: PilotTpcSettings,
    ) -> Result<Self, PowerControlError> {
        validate_settings(identity, settings)?;
        #[cfg(test)]
        {
            with_cell_floor(|floor| floor.store(0, std::sync::atomic::Ordering::Relaxed));
            with_ratio(|ratio| ratio.store(0, std::sync::atomic::Ordering::Relaxed));
        }
        Ok(Self {
            identity,
            settings,
            previous: None,
            noise: None,
            actual_sir: None,
            frame_disturbance: 0.0,
            frame_slots: 0,
            settled_frames: 0,
            margin: MEASURED_INTERFERED_PILOT_LIMIT,
            quality_signal: 0.0,
            quality_disturbance: 0.0,
            quality_slots: 0,
            quality_rejected: 0,
            diagnostic: FrameDiagnostic::default(),
        })
    }

    fn adapt_frame_margin(&mut self) {
        if self.quality_slots + self.quality_rejected < RATIO_FRAME_SLOTS {
            return;
        }
        let sir = if self.quality_slots == 0 {
            0.0
        } else {
            self.quality_signal / self.quality_disturbance
        };
        self.margin = adapt_margin(self.margin, sir, self.quality_rejected);
        self.quality_signal = 0.0;
        self.quality_disturbance = 0.0;
        self.quality_slots = 0;
        self.quality_rejected = 0;
    }

    fn measured_floor(&mut self, disturbance: f64) -> Option<f64> {
        let strip = crate::uplink_noise::strip_noise()?;
        self.frame_disturbance += disturbance;
        self.frame_slots += 1;
        if self.frame_slots == RATIO_FRAME_SLOTS {
            let ratio = self.frame_disturbance / f64::from(RATIO_FRAME_SLOTS) / strip;
            self.frame_disturbance = 0.0;
            self.frame_slots = 0;
            if self.settled_frames < RATIO_SETTLING_FRAMES {
                self.settled_frames += 1;
            } else {
                let next = track_quiet_ratio(pilot_noise_ratio(), ratio);
                with_ratio(|cell| cell.store(next.to_bits(), std::sync::atomic::Ordering::Relaxed));
            }
        }
        Some(strip * pilot_noise_ratio()? / MEASURED_FLOOR_OFFSET)
    }

    pub fn observe(
        &mut self,
        observation: PilotObservation,
    ) -> Result<PilotFeedback, PowerControlError> {
        validate_observation(self.identity, self.settings, observation)?;
        let key = PilotObservationKey {
            uplink_slot: observation.uplink_slot,
            measurement_end: observation.measurement_end,
            ready_at: observation.ready_at,
            sample_generation: observation.sample_generation,
            continuity: observation.continuity,
        };
        if let Some(previous) = self.previous {
            if observation.sample_generation < previous.sample_generation
                || observation.continuity < previous.continuity
            {
                return Err(PowerControlError::Discontinuity);
            }
            if observation.uplink_slot.index <= previous.uplink_slot.index
                || observation.measurement_end.index <= previous.measurement_end.index
                || observation.ready_at.index < previous.ready_at.index
            {
                return Err(PowerControlError::Time);
            }
        }
        let (signal, disturbance) = match pilot_powers(
            observation.pilot,
            observation.pilot_count,
            self.settings.minimum_coherent_fraction,
        ) {
            Ok(powers) => powers,
            Err(error) => {
                self.quality_rejected += 1;
                self.adapt_frame_margin();
                self.diagnostic.rejected += 1;
                self.diagnostic.emit(
                    self.identity,
                    self.noise,
                    self.settings.target.linear,
                    self.margin,
                );
                return Err(error);
            }
        };
        let slot = self.identity.ue.slot;
        let has_edch =
            slot < 64 && EDCH_SLOTS.load(std::sync::atomic::Ordering::Relaxed) & (1 << slot) != 0;
        let noise = match self.noise {
            Some(level) if disturbance < level => level + (disturbance - level) * NOISE_FALL,
            Some(level) => level * NOISE_RISE_PER_SLOT,
            None => disturbance,
        };
        self.noise = Some(noise);
        let shared = cell_floor(disturbance);
        let measured = self.measured_floor(disturbance);
        let noise = measured.unwrap_or(if has_edch { noise } else { shared });
        self.quality_signal += signal;
        self.quality_disturbance += disturbance;
        self.quality_slots += 1;
        self.adapt_frame_margin();
        let interfered_limit = if measured.is_some() {
            self.margin
        } else {
            INTERFERED_PILOT_LIMIT
        };
        let actual = signal / disturbance;
        let actual = self.actual_sir.map_or(actual, |smoothed| {
            smoothed + (actual - smoothed) * ACTUAL_SIR_SMOOTHING
        });
        self.actual_sir = Some(actual);
        let against_noise = signal / noise;
        let target = self.settings.target.linear;
        // A pilot below its own disturbance that still reads far above target is another
        // handset's burst, so the reading cannot order this handset down.
        if signal < disturbance && against_noise >= target * SWAMPED_PILOT_LIMIT {
            self.diagnostic.rejected += 1;
            self.diagnostic
                .emit(self.identity, Some(noise), target, self.margin);
            return Err(PowerControlError::Coherence);
        }
        let sir_linear = if !has_edch {
            (signal / disturbance).max(against_noise / interfered_limit)
        } else if actual < MINIMUM_ACTUAL_SIR
            && against_noise >= target
            && against_noise < target * SWAMPED_PILOT_LIMIT
        {
            0.5 * target
        } else {
            against_noise
        };
        if !sir_linear.is_finite() || sir_linear <= 0.0 {
            return Err(PowerControlError::InvalidPilot);
        }
        self.diagnostic.signal += signal;
        self.diagnostic.disturbance += disturbance;
        self.diagnostic.accepted += 1;
        self.diagnostic.up += u32::from(sir_linear < target);
        self.diagnostic.edch = has_edch;
        self.diagnostic
            .emit(self.identity, Some(noise), target, self.margin);
        let observed_at = ChipInstant {
            epoch: observation.measurement_end.epoch,
            index: observation
                .measurement_end
                .index
                .div_ceil(self.settings.rate.get() as u64),
        };
        let valid_until = observed_at
            .index
            .checked_add(self.settings.maximum_observation_age_chips.get())
            .and_then(|value| value.checked_add(self.settings.lead_chips.get()))
            .and_then(|value| value.checked_add(CHIPS_PER_SLOT))
            .ok_or(PowerControlError::Time)?;
        let estimate = TpcEstimate::new(
            observation.identity,
            ChipInstant {
                epoch: observation.ready_at.epoch,
                index: valid_until,
            },
            observed_at,
            sir_linear,
            self.settings.target.linear,
        )
        .map_err(|_| PowerControlError::Time)?;
        let frontier = preparation_frontier(self.settings, observation.ready_at)?;
        self.previous = Some(key);
        Ok(PilotFeedback {
            estimate,
            provenance: TpcProvenance {
                identity: observation.identity,
                uplink_slot: observation.uplink_slot,
                measurement_end: observation.measurement_end,
                ready_at: observation.ready_at,
                preparation_frontier: frontier,
                sample_generation: observation.sample_generation,
                continuity: observation.continuity,
            },
        })
    }

    pub(super) fn for_publication(
        &self,
        mut feedback: PilotFeedback,
        now: ChipInstant,
    ) -> Result<PilotFeedback, PowerControlError> {
        if feedback.estimate.identity() != self.identity
            || now.epoch != self.settings.downlink_anchor.epoch
        {
            return Err(PowerControlError::Identity);
        }
        if now.index < feedback.estimate.observed_at().index {
            return Err(PowerControlError::PublicationNotReady);
        }
        if now.index >= feedback.estimate.valid_until().index {
            return Err(PowerControlError::Stale);
        }
        feedback.provenance.preparation_frontier = preparation_frontier(self.settings, now)?;
        Ok(feedback)
    }
}

fn validate_settings(
    identity: ReceiveIdentity,
    settings: PilotTpcSettings,
) -> Result<(), PowerControlError> {
    if identity.ue.epoch != settings.uplink_anchor.epoch
        || identity.ue.epoch != settings.downlink_anchor.epoch
        || !settings.target.db.is_finite()
        || !settings.target.linear.is_finite()
        || settings.target.linear <= 0.0
        || !settings.minimum_coherent_fraction.is_finite()
        || !(0.0..=1.0).contains(&settings.minimum_coherent_fraction)
        || settings.minimum_coherent_fraction == 0.0
    {
        return Err(PowerControlError::Configuration);
    }
    Ok(())
}

fn validate_observation(
    identity: ReceiveIdentity,
    settings: PilotTpcSettings,
    observation: PilotObservation,
) -> Result<(), PowerControlError> {
    if observation.identity != identity {
        return Err(PowerControlError::Identity);
    }
    if observation.uplink_slot.epoch != settings.uplink_anchor.epoch
        || observation.ready_at.epoch != settings.downlink_anchor.epoch
        || observation.measurement_end.epoch != observation.ready_at.epoch
    {
        return Err(PowerControlError::Epoch);
    }
    let uplink_delta = observation
        .uplink_slot
        .index
        .checked_sub(settings.uplink_anchor.index)
        .ok_or(PowerControlError::Time)?;
    if !uplink_delta.is_multiple_of(CHIPS_PER_SLOT)
        || observation.sample_generation == 0
        || observation.continuity == 0
    {
        return Err(PowerControlError::Time);
    }
    let rate = settings.rate.get() as u64;
    let measurement_chip = observation.measurement_end.index.div_ceil(rate);
    let ready_sample = observation
        .ready_at
        .index
        .checked_mul(rate)
        .ok_or(PowerControlError::Time)?;
    if observation.measurement_end.index > ready_sample {
        return Err(PowerControlError::Time);
    }
    let age = observation
        .ready_at
        .index
        .checked_sub(measurement_chip)
        .ok_or(PowerControlError::Time)?;
    if age > settings.maximum_observation_age_chips.get() {
        return Err(PowerControlError::Stale);
    }
    if !observation.pilot.channel.re.is_finite()
        || !observation.pilot.channel.im.is_finite()
        || !observation.pilot.observed_power.is_finite()
        || !observation.pilot.residual_power.is_finite()
        || observation.pilot.observed_power <= 0.0
        || observation.pilot.residual_power <= 0.0
    {
        return Err(PowerControlError::InvalidPilot);
    }
    Ok(())
}

fn pilot_powers(
    pilot: PilotEstimate,
    pilot_count: usize,
    minimum_coherent_fraction: f64,
) -> Result<(f64, f64), PowerControlError> {
    if !(MINIMUM_PILOT_COUNT..=MAXIMUM_PILOT_COUNT).contains(&pilot_count) {
        return Err(PowerControlError::InvalidPilot);
    }
    let channel_power = pilot.channel.norm_sqr();
    if !channel_power.is_finite() {
        return Err(PowerControlError::InvalidPilot);
    }
    let expected_observed = channel_power + pilot.residual_power;
    let difference = (expected_observed - pilot.observed_power).abs();
    let scale = expected_observed.abs().max(pilot.observed_power.abs());
    if !expected_observed.is_finite() || difference > PILOT_POWER_RELATIVE_TOLERANCE * scale {
        return Err(PowerControlError::InvalidPilot);
    }
    let coherence = channel_power / pilot.observed_power;
    if !coherence.is_finite() || coherence < minimum_coherent_fraction {
        return Err(PowerControlError::Coherence);
    }
    let statistics =
        DpcchStatistics::new(pilot, pilot_count).map_err(|_| PowerControlError::InvalidPilot)?;
    let sir = statistics
        .sir_linear()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or(PowerControlError::InvalidPilot)?;
    let disturbance = statistics.disturbance_symbol_power;
    if !disturbance.is_finite() || disturbance <= 0.0 {
        return Err(PowerControlError::InvalidPilot);
    }
    Ok((sir * disturbance, disturbance))
}

fn preparation_frontier(
    settings: PilotTpcSettings,
    ready_at: ChipInstant,
) -> Result<ChipInstant, PowerControlError> {
    let threshold = ready_at
        .index
        .checked_add(settings.lead_chips.get())
        .ok_or(PowerControlError::Time)?;
    let base = threshold.max(settings.downlink_anchor.index);
    let delta = base
        .checked_sub(settings.downlink_anchor.index)
        .ok_or(PowerControlError::Time)?;
    let slots = delta.div_ceil(CHIPS_PER_SLOT);
    let index = settings
        .downlink_anchor
        .index
        .checked_add(
            slots
                .checked_mul(CHIPS_PER_SLOT)
                .ok_or(PowerControlError::Time)?,
        )
        .ok_or(PowerControlError::Time)?;
    Ok(ChipInstant {
        epoch: settings.downlink_anchor.epoch,
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_dsp::bits::Bit;
    use umts_types::{RadioEpoch, UeKey};

    fn epoch() -> RadioEpoch {
        RadioEpoch::new(NonZeroU64::new(1).unwrap())
    }

    fn identity() -> ReceiveIdentity {
        ReceiveIdentity {
            ue: UeKey {
                epoch: epoch(),
                slot: 0,
                generation: NonZeroU64::new(1).unwrap(),
            },
            configuration: NonZeroU64::new(1).unwrap(),
        }
    }

    fn at(index: u64) -> ChipInstant {
        ChipInstant {
            epoch: epoch(),
            index,
        }
    }

    fn at_epoch(epoch_number: u64, index: u64) -> ChipInstant {
        ChipInstant {
            epoch: RadioEpoch::new(NonZeroU64::new(epoch_number).unwrap()),
            index,
        }
    }

    fn settings(target_db: f64) -> PilotTpcSettings {
        PilotTpcSettings {
            target: PilotSirTarget::from_db(target_db).unwrap(),
            minimum_coherent_fraction: 0.5,
            uplink_anchor: at(0),
            downlink_anchor: at(0),
            rate: SamplesPerChip::Two,
            lead_chips: NonZeroU64::new(1).unwrap(),
            maximum_observation_age_chips: NonZeroU64::new(10_000).unwrap(),
        }
    }

    fn observation(index: u64, observed: f64, residual: f64) -> PilotObservation {
        PilotObservation {
            identity: identity(),
            uplink_slot: at(index),
            measurement_end: SampleInstant {
                epoch: epoch(),
                index: index * 2,
            },
            ready_at: at(index + 1),
            sample_generation: 1,
            continuity: 1,
            pilot: PilotEstimate {
                channel: num_complex::Complex64::new((observed - residual).sqrt(), 0.0),
                observed_power: observed,
                residual_power: residual,
            },
            pilot_count: 6,
        }
    }

    #[test]
    fn tpc_direction_follows_ts_25_214_rule() {
        let mut decrease = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        assert_eq!(
            decrease
                .observe(observation(0, 2.0, 0.1))
                .unwrap()
                .estimate
                .command(),
            Bit::Zero
        );
        let mut increase = PilotTpcController::new(identity(), settings(30.0)).unwrap();
        assert_eq!(
            increase
                .observe(observation(0, 2.0, 0.1))
                .unwrap()
                .estimate
                .command(),
            Bit::One
        );
    }

    #[test]
    fn swamped_pilot_far_above_target_is_not_a_down_command() {
        crate::uplink_noise::set_strip_noise_for_test(None);
        let mut settings = settings(0.0);
        settings.minimum_coherent_fraction = 0.01;
        let mut controller = PilotTpcController::new(identity(), settings).unwrap();
        assert_eq!(
            controller
                .observe(observation(0, 1.0, 0.001))
                .unwrap()
                .estimate
                .command(),
            Bit::Zero
        );
        assert_eq!(
            controller.observe(observation(2560, 100.0, 60.0)).err(),
            Some(PowerControlError::Coherence)
        );
        assert_eq!(
            controller
                .observe(observation(5120, 100.0, 0.1))
                .unwrap()
                .estimate
                .command(),
            Bit::Zero
        );
    }

    #[test]
    fn measured_floor_follows_strip_noise_and_not_traffic() {
        let mut controller = PilotTpcController::new(identity(), settings(13.0)).unwrap();
        crate::uplink_noise::set_strip_noise_for_test(None);
        assert_eq!(controller.measured_floor(0.01), None);
        crate::uplink_noise::set_strip_noise_for_test(Some(1.0));
        let frames = |controller: &mut PilotTpcController, count: u32, disturbance: f64| {
            let mut floor = None;
            for _ in 0..count * RATIO_FRAME_SLOTS {
                floor = controller.measured_floor(disturbance);
            }
            floor
        };
        assert_eq!(frames(&mut controller, RATIO_SETTLING_FRAMES, 0.1), None);
        let quiet = frames(&mut controller, 1, 0.01).unwrap();
        assert!((quiet / (0.01 / MEASURED_FLOOR_OFFSET) - 1.0).abs() < 1e-9);
        let loaded = frames(&mut controller, 5_000, 0.1).unwrap();
        assert!((loaded / quiet - 1.0).abs() < 0.01);
        crate::uplink_noise::set_strip_noise_for_test(Some(4.0));
        let raised = controller.measured_floor(0.4).unwrap();
        assert!((raised / (4.0 * loaded) - 1.0).abs() < 1e-6);
        crate::uplink_noise::set_strip_noise_for_test(None);
    }

    #[test]
    fn margin_rises_fast_on_poor_frames_and_returns_slowly_within_bounds() {
        let base = MEASURED_INTERFERED_PILOT_LIMIT;
        assert_eq!(adapt_margin(base, 10.0, 0), base);
        assert_eq!(adapt_margin(base, 2.0, 0), base);
        let mut margin = base;
        for _ in 0..100 {
            margin = adapt_margin(margin, 1.0, 0);
        }
        assert_eq!(margin, MAXIMUM_INTERFERED_PILOT_LIMIT);
        assert!(adapt_margin(base, 10.0, MARGIN_RAISE_REJECTED_SLOTS) > base);
        assert_eq!(adapt_margin(margin, 2.0, 0), margin);
        let mut frames = 0;
        while margin > base {
            margin = adapt_margin(margin, 10.0, 0);
            frames += 1;
        }
        assert!((300..1000).contains(&frames), "{frames}");
    }

    #[test]
    fn quiet_ratio_keeps_the_lowest_frames_and_ignores_load() {
        let mut ratio = track_quiet_ratio(None, 4.0);
        assert_eq!(ratio, 4.0);
        for _ in 0..10_000 {
            ratio = track_quiet_ratio(Some(ratio), 40.0);
        }
        assert!(ratio < 4.0 * 1.02);
        for _ in 0..20 {
            ratio = track_quiet_ratio(Some(ratio), 1.0);
        }
        assert!((ratio - 1.0).abs() < 0.02);
        assert_eq!(track_quiet_ratio(Some(ratio), f64::NAN), ratio);
    }

    #[test]
    fn pilot_sir_is_common_gain_invariant() {
        let mut first = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        let a = first.observe(observation(0, 2.0, 0.1)).unwrap();
        let mut second = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        let b = second.observe(observation(0, 200.0, 10.0)).unwrap();
        assert!((a.estimate.sir_linear() - b.estimate.sir_linear()).abs() < 1e-12);
        assert_eq!(a.estimate.command(), b.estimate.command());
    }

    #[test]
    fn timing_ceil_and_downlink_anchor_are_explicit() {
        let mut configured = settings(0.0);
        configured.downlink_anchor = at(512);
        let mut controller = PilotTpcController::new(identity(), configured).unwrap();
        let mut sample = observation(0, 2.0, 0.1);
        sample.measurement_end.index = 2_049;
        sample.ready_at = at(1_026);
        let feedback = controller.observe(sample).unwrap();
        assert_eq!(feedback.estimate.observed_at(), at(1_025));
        assert_eq!(
            feedback.provenance.preparation_frontier,
            at(512 + CHIPS_PER_SLOT)
        );
    }

    #[test]
    fn duplicate_is_rejected_but_forward_gap_preserves_measurement_provenance() {
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        controller.observe(observation(0, 2.0, 0.1)).unwrap();
        assert_eq!(
            controller.observe(observation(0, 2.0, 0.1)),
            Err(PowerControlError::Time)
        );
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        controller.observe(observation(0, 2.0, 0.1)).unwrap();
        let next = controller
            .observe(observation(CHIPS_PER_SLOT * 2, 2.0, 0.1))
            .unwrap();
        assert_eq!(next.provenance.uplink_slot.index, CHIPS_PER_SLOT * 2);
        assert_eq!(next.estimate.observed_at().index, CHIPS_PER_SLOT * 2);
    }

    #[test]
    fn backlog_keeps_measurement_order_without_reserving_a_future_slot_per_observation() {
        let mut configured = settings(0.0);
        configured.maximum_observation_age_chips = NonZeroU64::new(100_000).unwrap();
        let mut controller = PilotTpcController::new(identity(), configured).unwrap();
        let mut first = observation(0, 2.0, 0.1);
        first.ready_at = at(50_000);
        let first_feedback = controller.observe(first).unwrap();
        let mut second = observation(CHIPS_PER_SLOT, 2.0, 0.1);
        second.ready_at = at(50_000);
        let second_feedback = controller.observe(second).unwrap();
        assert_eq!(
            second_feedback.provenance.preparation_frontier,
            first_feedback.provenance.preparation_frontier
        );

        let mut late = observation(CHIPS_PER_SLOT * 2, 2.0, 0.1);
        late.ready_at = at(100_000);
        let updated = controller.observe(late).unwrap();
        assert!(
            updated.provenance.preparation_frontier.index
                > second_feedback.provenance.preparation_frontier.index + CHIPS_PER_SLOT
        );
        assert!(
            updated.provenance.preparation_frontier.index
                >= late.ready_at.index + configured.lead_chips.get()
        );
        assert_eq!(updated.estimate.observed_at().index, CHIPS_PER_SLOT * 2);
        assert_eq!(updated.provenance.ready_at, late.ready_at);
        assert_eq!(
            updated.estimate.valid_until().index,
            CHIPS_PER_SLOT * 3 + 100_001
        );
        let mut retry = observation(CHIPS_PER_SLOT * 2, 2.0, 0.1);
        retry.ready_at = at(50_000);
        assert_eq!(controller.observe(retry), Err(PowerControlError::Time));
    }

    #[test]
    fn advancing_sample_generation_is_accepted_but_old_generation_cannot_reenter() {
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        controller.observe(observation(0, 2.0, 0.1)).unwrap();
        let mut next = observation(CHIPS_PER_SLOT, 2.0, 0.1);
        next.sample_generation = 2;
        next.continuity = 2;
        let accepted = controller.observe(next).unwrap();
        assert_eq!(accepted.provenance.sample_generation, 2);
        assert_eq!(accepted.provenance.continuity, 2);
        assert_eq!(
            controller.observe(observation(CHIPS_PER_SLOT * 2, 2.0, 0.1)),
            Err(PowerControlError::Discontinuity)
        );
    }

    #[test]
    fn delayed_publication_preserves_measurement_and_distinguishes_expired_from_future_updates() {
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        let original = controller.observe(observation(0, 2.0, 0.1)).unwrap();
        let retimed = controller.for_publication(original, at(3000)).unwrap();
        assert_eq!(retimed.estimate, original.estimate);
        assert_eq!(retimed.provenance.preparation_frontier, at(5120));
        assert_eq!(
            retimed.estimate.observed_at(),
            original.estimate.observed_at()
        );
        assert_eq!(
            retimed.estimate.valid_until(),
            original.estimate.valid_until()
        );
        assert_eq!(
            retimed.provenance.measurement_end,
            original.provenance.measurement_end
        );
        assert_eq!(retimed.provenance.ready_at, original.provenance.ready_at);
        assert_eq!(
            retimed.estimate.sir_linear(),
            original.estimate.sir_linear()
        );
        assert_eq!(
            controller.for_publication(retimed, at(13000)),
            Err(PowerControlError::Stale)
        );
        let future = controller
            .observe(observation(CHIPS_PER_SLOT, 2.0, 0.1))
            .unwrap();
        assert_eq!(
            controller.for_publication(future, at(future.estimate.observed_at().index - 1)),
            Err(PowerControlError::PublicationNotReady)
        );
    }

    #[test]
    fn invalid_pilot_and_horizon_are_rejected() {
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        let mut invalid = observation(0, 1.0, 1.0);
        invalid.pilot.channel = num_complex::Complex64::default();
        assert_eq!(
            controller.observe(invalid),
            Err(PowerControlError::Coherence)
        );
        let mut configured = settings(0.0);
        configured.lead_chips = NonZeroU64::new(TPC_LANE_SLOTS * CHIPS_PER_SLOT).unwrap();
        let mut controller = PilotTpcController::new(identity(), configured).unwrap();
        assert_eq!(
            controller.observe(observation(0, 2.0, 0.1)),
            Err(PowerControlError::Time)
        );
    }

    #[test]
    fn malformed_pilot_power_noise_and_count_are_rejected() {
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        let mut inconsistent = observation(0, 2.0, 0.1);
        inconsistent.pilot.channel = num_complex::Complex64::new(1.0, 0.0);
        assert_eq!(
            controller.observe(inconsistent),
            Err(PowerControlError::InvalidPilot)
        );

        let mut zero_noise = observation(0, 1.0, 0.0);
        zero_noise.pilot.channel = num_complex::Complex64::new(1.0, 0.0);
        assert_eq!(
            controller.observe(zero_noise),
            Err(PowerControlError::InvalidPilot)
        );

        let mut bad_count = observation(0, 2.0, 0.1);
        bad_count.pilot_count = 2;
        assert_eq!(
            controller.observe(bad_count),
            Err(PowerControlError::InvalidPilot)
        );

        let mut nonfinite = observation(0, 2.0, 0.1);
        nonfinite.pilot.observed_power = f64::NAN;
        assert_eq!(
            controller.observe(nonfinite),
            Err(PowerControlError::InvalidPilot)
        );

        let mut nonfinite_channel = observation(0, 2.0, 0.1);
        nonfinite_channel.pilot.channel.re = f64::NAN;
        assert_eq!(
            controller.observe(nonfinite_channel),
            Err(PowerControlError::InvalidPilot)
        );

        let mut nonfinite_residual = observation(0, 2.0, 0.1);
        nonfinite_residual.pilot.residual_power = f64::INFINITY;
        assert_eq!(
            controller.observe(nonfinite_residual),
            Err(PowerControlError::InvalidPilot)
        );

        let mut negative_power = observation(0, 2.0, 0.1);
        negative_power.pilot.residual_power = -0.1;
        assert_eq!(
            controller.observe(negative_power),
            Err(PowerControlError::InvalidPilot)
        );

        let mut too_many = observation(0, 2.0, 0.1);
        too_many.pilot_count = 9;
        assert_eq!(
            controller.observe(too_many),
            Err(PowerControlError::InvalidPilot)
        );

        // A relative check must not become an absolute 1e-9 power floor.
        let mut tiny_mismatch = observation(0, 1e-12, 1e-13);
        tiny_mismatch.pilot.channel =
            num_complex::Complex64::new((1e-12_f64 - 1e-13_f64 + 1e-15).sqrt(), 0.0);
        assert_eq!(
            controller.observe(tiny_mismatch),
            Err(PowerControlError::InvalidPilot)
        );
    }

    #[test]
    fn equality_uses_deterministic_zero_command() {
        let mut controller = PilotTpcController::new(identity(), settings(0.0)).unwrap();
        let feedback = controller.observe(observation(0, 2.4, 1.0)).unwrap();
        assert_eq!(feedback.estimate.sir_linear(), 1.0);
        assert_eq!(feedback.estimate.command(), Bit::Zero);
    }

    #[test]
    fn stale_and_epoch_mismatch_are_rejected() {
        let mut configured = settings(0.0);
        configured.maximum_observation_age_chips = NonZeroU64::new(1).unwrap();
        let mut controller = PilotTpcController::new(identity(), configured).unwrap();
        let mut stale = observation(0, 2.0, 0.1);
        stale.ready_at = at(3);
        assert_eq!(controller.observe(stale), Err(PowerControlError::Stale));
        let mut wrong = observation(0, 2.0, 0.1);
        wrong.identity = ReceiveIdentity {
            ue: UeKey {
                epoch: epoch(),
                slot: 1,
                generation: NonZeroU64::new(1).unwrap(),
            },
            configuration: NonZeroU64::new(1).unwrap(),
        };
        assert_eq!(controller.observe(wrong), Err(PowerControlError::Identity));

        let mut wrong_epoch = observation(0, 2.0, 0.1);
        wrong_epoch.measurement_end.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
        assert_eq!(
            controller.observe(wrong_epoch),
            Err(PowerControlError::Epoch)
        );

        let mut wrong_uplink_epoch = observation(0, 2.0, 0.1);
        wrong_uplink_epoch.uplink_slot = at_epoch(2, 0);
        assert_eq!(
            controller.observe(wrong_uplink_epoch),
            Err(PowerControlError::Epoch)
        );

        let mut wrong_ready_epoch = observation(0, 2.0, 0.1);
        wrong_ready_epoch.ready_at = at_epoch(2, 1);
        assert_eq!(
            controller.observe(wrong_ready_epoch),
            Err(PowerControlError::Epoch)
        );
    }

    #[test]
    fn configuration_and_time_overflow_are_rejected() {
        assert_eq!(
            PilotSirTarget::from_db(f64::INFINITY),
            Err(PowerControlError::Configuration)
        );
        let mut invalid = settings(0.0);
        invalid.minimum_coherent_fraction = 0.0;
        assert!(matches!(
            PilotTpcController::new(identity(), invalid),
            Err(PowerControlError::Configuration)
        ));

        let mut nonfinite_fraction = settings(0.0);
        nonfinite_fraction.minimum_coherent_fraction = f64::NAN;
        assert!(matches!(
            PilotTpcController::new(identity(), nonfinite_fraction),
            Err(PowerControlError::Configuration)
        ));

        let mut wrong_anchor_epoch = settings(0.0);
        wrong_anchor_epoch.downlink_anchor = at_epoch(2, 0);
        assert!(matches!(
            PilotTpcController::new(identity(), wrong_anchor_epoch),
            Err(PowerControlError::Configuration)
        ));

        let mut overflow_settings = settings(0.0);
        overflow_settings.maximum_observation_age_chips = NonZeroU64::new(u64::MAX).unwrap();
        overflow_settings.lead_chips = NonZeroU64::new(2).unwrap();
        let mut controller = PilotTpcController::new(identity(), overflow_settings).unwrap();
        let mut overflow = observation(0, 2.0, 0.1);
        overflow.ready_at = at(u64::MAX);
        assert_eq!(controller.observe(overflow), Err(PowerControlError::Time));
    }
}
