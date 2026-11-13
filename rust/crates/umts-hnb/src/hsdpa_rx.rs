// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex64;
use std::sync::Arc;
use umts_dsp::{
    DspError,
    dedicated::{DpcchStatistics, UplinkDpchCode, UplinkSlot},
    hsdpa::{Cqi, FeedbackChannel, FeedbackEvidence, HarqAck, decode_cqi, decode_harq_ack},
    pilots::PilotEstimate,
};
use umts_mac::hsdpa::{ScheduledTransmission, TransmissionKey};
use umts_types::{
    UeKey,
    time::{CHIPS_PER_SLOT, ChipInstant},
};

const SYMBOL_CHIPS: u64 = 256;
const SLOT_SYMBOLS: usize = 10;
const SUBFRAME_SYMBOLS: usize = 3 * SLOT_SYMBOLS;
const CQI_SYMBOLS: usize = 2 * SLOT_SYMBOLS;
const MAX_REPETITIONS: u8 = 4;
const HISTORY_SYMBOLS: usize = 160;
mod delivery;
mod mode;
mod policy;
pub use delivery::{Evidence, FeedbackPort, Measurement, Report, Request, RequestId};
pub use mode::{CarrierAcknowledgements, CarrierCqi, FeedbackMode, FeedbackModeSchedule};
pub use policy::{AckRejection, EvidenceLimits, FeedbackPolicy};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
struct Pilot {
    center: u64,
    channel: Complex64,
    coherent_fraction: f64,
    relative_disturbance: f64,
}

struct PendingSlot {
    start: ChipInstant,
    values: [Complex64; SLOT_SYMBOLS],
    pilot: Pilot,
    resolved_prefix: usize,
}

#[derive(Clone, Copy)]
struct Symbol {
    start: u64,
    soft: f32,
    coherent_fraction: f64,
    relative_disturbance: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Observation<T> {
    pub evidence: FeedbackEvidence<T>,
    pub minimum_pilot_fraction: f64,
    pub mean_relative_pilot_disturbance: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum Readout<T> {
    Pending,
    Missing,
    Observed(Observation<T>),
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct TransmissionReadout {
    pub key: TransmissionKey,
    pub expected_ack: ChipInstant,
    pub acknowledgement: Readout<HarqAck>,
}

pub struct FeedbackReceiver {
    ue: UeKey,
    channel: FeedbackChannel,
    channel_activation: ChipInstant,
    origin: ChipInstant,
    pending: Option<PendingSlot>,
    previous_pilot: Option<Pilot>,
    symbols: [Option<Symbol>; HISTORY_SYMBOLS],
    processed_until: Option<ChipInstant>,
    delivery: Option<delivery::Delivery>,
}

impl FeedbackReceiver {
    pub fn new(
        ue: UeKey,
        code: Arc<UplinkDpchCode>,
        maximum_dpdch: u8,
        origin: ChipInstant,
    ) -> Result<Self, DspError> {
        if ue.epoch != origin.epoch {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(Self {
            ue,
            channel: FeedbackChannel::new(code, maximum_dpdch, origin)?,
            channel_activation: origin,
            origin,
            pending: None,
            previous_pilot: None,
            symbols: [None; HISTORY_SYMBOLS],
            processed_until: None,
            delivery: None,
        })
    }

    pub fn processed_until(&self) -> Option<ChipInstant> {
        self.processed_until
    }

    // Only irreversibly discarded RX intervals may advance this watermark without symbols.
    pub(crate) fn discard_before(&mut self, boundary: ChipInstant) -> Result<(), DspError> {
        if boundary.epoch != self.origin.epoch
            || boundary
                .index
                .checked_sub(self.origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(CHIPS_PER_SLOT))
            || self
                .processed_until
                .is_some_and(|previous| boundary.index < previous.index)
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| boundary.index <= pending.start.index)
        {
            return Err(DspError::DiscontinuousTime);
        }
        self.pending = None;
        self.previous_pilot = None;
        self.processed_until = Some(boundary);
        self.service_requests();
        Ok(())
    }

    pub fn replace_channel_at(
        &mut self,
        replacement: &mut FeedbackChannel,
        activation: ChipInstant,
    ) -> Result<(), DspError> {
        if !self.channel.shares_timeline(replacement)
            || activation.epoch != self.origin.epoch
            || activation.index < self.channel_activation.index
            || activation
                .index
                .checked_sub(self.origin.index)
                .is_none_or(|delta| !delta.is_multiple_of(umts_types::time::CHIPS_PER_FRAME))
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.start.index >= activation.index)
        {
            return Err(DspError::DiscontinuousTime);
        }
        // Pending symbols are already despread with their original channelization code.
        std::mem::swap(&mut self.channel, replacement);
        self.channel_activation = activation;
        Ok(())
    }

    pub fn connect(
        &mut self,
        capacity: std::num::NonZeroUsize,
        budget: std::num::NonZeroUsize,
    ) -> Result<FeedbackPort, DspError> {
        if self.delivery.is_some() || self.pending.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        let (delivery, port) = delivery::Delivery::new(self.ue, capacity, budget)?;
        self.delivery = Some(delivery);
        Ok(port)
    }

    pub fn delivery_bytes(capacity: std::num::NonZeroUsize) -> Result<usize, DspError> {
        delivery::Delivery::retained_bytes(capacity)
    }

    pub fn service_requests(&mut self) {
        if let Some(mut delivery) = self.delivery.take() {
            delivery.service(self);
            self.delivery = Some(delivery);
        }
    }

    pub fn observe(
        &self,
        transmission: ScheduledTransmission,
        repetitions: u8,
    ) -> Result<TransmissionReadout, DspError> {
        if transmission.key.ue() != self.ue {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(TransmissionReadout {
            key: transmission.key,
            expected_ack: transmission.expected_ack,
            acknowledgement: self.ack(transmission.expected_ack, repetitions)?,
        })
    }

    pub fn feed(&mut self, slot: &UplinkSlot<'_>) -> Result<(), DspError> {
        let start = slot.start();
        if start.epoch != self.channel_activation.epoch
            || start.index < self.channel_activation.index
            || self
                .processed_until
                .is_some_and(|through| start.index < through.index)
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| start.index <= pending.start.index)
        {
            return Err(DspError::DiscontinuousTime);
        }
        let values = self.channel.despread_tracked_slot(slot)?;
        let pilot = Self::pilot(start, slot.control().pilot, slot.pilot_count())?;
        let contiguous = self.pending.as_ref().is_some_and(|pending| {
            pending.start.index.checked_add(CHIPS_PER_SLOT) == Some(start.index)
        });
        if contiguous {
            let pending = self.pending.take().expect("contiguous slot");
            for (index, value) in pending
                .values
                .into_iter()
                .enumerate()
                .skip(pending.resolved_prefix)
            {
                let time = pending.start.index + index as u64 * SYMBOL_CHIPS;
                let center = time + SYMBOL_CHIPS / 2;
                let (first, last) = if center < pending.pilot.center {
                    (self.previous_pilot.unwrap_or(pending.pilot), pending.pilot)
                } else {
                    (pending.pilot, pilot)
                };
                self.store_symbol(time, value, first, last);
            }
            self.previous_pilot = Some(pending.pilot);
        } else {
            self.symbols.fill(None);
            self.previous_pilot = None;
        }
        // Symbols before this pilot's center already have the same two bracketing pilots
        // that delayed interpolation would use. Equality belongs to the following pair.
        let resolved_prefix = slot.pilot_count() / 2;
        for (index, &value) in values.iter().take(resolved_prefix).enumerate() {
            self.store_symbol(
                start.index + index as u64 * SYMBOL_CHIPS,
                value,
                self.previous_pilot.unwrap_or(pilot),
                pilot,
            );
        }
        self.processed_until = Some(ChipInstant {
            index: start
                .index
                .checked_add(resolved_prefix as u64 * SYMBOL_CHIPS)
                .ok_or(DspError::DiscontinuousTime)?,
            ..start
        });
        self.pending = Some(PendingSlot {
            start,
            values,
            pilot,
            resolved_prefix,
        });
        self.service_requests();
        Ok(())
    }

    fn store_symbol(&mut self, time: u64, value: Complex64, first: Pilot, last: Pilot) {
        let (channel, interpolation_fraction) = interpolate(first, last, time + SYMBOL_CHIPS / 2);
        let power = channel.norm_sqr();
        let projected = if power > 0.0 {
            (value * channel.conj()).re / power
        } else {
            0.0
        };
        // Unusable interpolation contributes no evidence, not an ACK inferred from its sign.
        let soft = if projected.is_finite() && projected.abs() <= f64::from(f32::MAX) {
            projected as f32
        } else {
            0.0
        };
        let coherent_fraction = if power > 0.0 {
            first
                .coherent_fraction
                .min(last.coherent_fraction)
                .min(interpolation_fraction)
        } else {
            0.0
        };
        let relative_disturbance = first.relative_disturbance.max(last.relative_disturbance);
        let ring = (((time - self.origin.index) / SYMBOL_CHIPS) % HISTORY_SYMBOLS as u64) as usize;
        self.symbols[ring] = Some(Symbol {
            start: time,
            soft,
            coherent_fraction,
            relative_disturbance,
        });
    }

    pub fn ack(&self, start: ChipInstant, repetitions: u8) -> Result<Readout<HarqAck>, DspError> {
        self.read(start, repetitions, decode_harq_ack)
    }

    /// Start is the HS-DPCCH subframe origin. CQI occupies its final two slots.
    pub fn cqi(&self, start: ChipInstant, repetitions: u8) -> Result<Readout<Cqi>, DspError> {
        let start = ChipInstant {
            index: start
                .index
                .checked_add(CHIPS_PER_SLOT)
                .ok_or(DspError::DiscontinuousTime)?,
            ..start
        };
        self.read::<CQI_SYMBOLS, _>(start, repetitions, decode_cqi)
    }

    fn read<const N: usize, T>(
        &self,
        start: ChipInstant,
        repetitions: u8,
        decode: impl FnOnce(&[f32; N]) -> Result<FeedbackEvidence<T>, DspError>,
    ) -> Result<Readout<T>, DspError> {
        if !(1..=MAX_REPETITIONS).contains(&repetitions) {
            return Err(DspError::InvalidLength);
        }
        if start.epoch != self.origin.epoch
            || start
                .index
                .checked_sub(self.origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(SYMBOL_CHIPS))
        {
            return Err(DspError::DiscontinuousTime);
        }
        let span = ((usize::from(repetitions) - 1) * SUBFRAME_SYMBOLS + N) as u64 * SYMBOL_CHIPS;
        let end = start
            .index
            .checked_add(span)
            .ok_or(DspError::DiscontinuousTime)?;
        if self
            .processed_until
            .is_none_or(|watermark| watermark.index < end)
        {
            return Ok(Readout::Pending);
        }
        let mut soft = [0.0; N];
        let mut coherent_fraction = 1.0_f64;
        let mut relative_disturbance = 0.0;
        for repetition in 0..usize::from(repetitions) {
            for (index, output) in soft.iter_mut().enumerate() {
                let time =
                    start.index + (repetition * SUBFRAME_SYMBOLS + index) as u64 * SYMBOL_CHIPS;
                let ring =
                    (((time - self.origin.index) / SYMBOL_CHIPS) % HISTORY_SYMBOLS as u64) as usize;
                let Some(symbol) = self.symbols[ring].filter(|symbol| symbol.start == time) else {
                    return Ok(Readout::Missing);
                };
                *output += symbol.soft / f32::from(repetitions);
                coherent_fraction = coherent_fraction.min(symbol.coherent_fraction);
                relative_disturbance +=
                    symbol.relative_disturbance / (N * usize::from(repetitions)) as f64;
            }
        }
        Ok(Readout::Observed(Observation {
            evidence: decode(&soft)?,
            minimum_pilot_fraction: coherent_fraction,
            mean_relative_pilot_disturbance: relative_disturbance,
        }))
    }

    fn pilot(start: ChipInstant, estimate: PilotEstimate, count: usize) -> Result<Pilot, DspError> {
        let statistics = DpcchStatistics::new(estimate, count)?;
        let power = estimate.channel.norm_sqr();
        if !power.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        Ok(Pilot {
            center: start
                .index
                .checked_add(count as u64 * SYMBOL_CHIPS / 2)
                .ok_or(DspError::DiscontinuousTime)?,
            channel: estimate.channel,
            coherent_fraction: if estimate.observed_power > 0.0 {
                (power / estimate.observed_power).clamp(0.0, 1.0)
            } else {
                0.0
            },
            relative_disturbance: if statistics.signal_code_power > 0.0 {
                statistics.disturbance_symbol_power / statistics.signal_code_power
            } else {
                f64::INFINITY
            },
        })
    }
}

fn interpolate(first: Pilot, last: Pilot, center: u64) -> (Complex64, f64) {
    if first.center == last.center {
        return (last.channel, 1.0);
    }
    let fraction = center.saturating_sub(first.center) as f64 / (last.center - first.center) as f64;
    let channel = first.channel * (1.0 - fraction) + last.channel * fraction;
    // Normalize phase cancellation against the same amplitude interpolation, not a neighboring gain.
    let amplitude = first.channel.norm() * (1.0 - fraction) + last.channel.norm() * fraction;
    let coherence = if amplitude > 0.0 {
        (channel.norm_sqr() / amplitude.powi(2)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (channel, coherence)
}
