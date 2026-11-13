// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{mem::size_of, num::NonZeroUsize};

use num_complex::Complex32;
use umts_dsp::{
    DspError,
    bits::Bit,
    hsdpa::{PdschSpreader, ScchSpreader, TransmissionPlan},
    scrambling::PrimaryScramblingCode,
};
use umts_mac::hsdpa::ScheduledTransmission;
use umts_types::UeKey;
use umts_types::time::{CHIPS_PER_SLOT, ChipInstant};

use crate::{
    common_tx::CommonTx,
    radio_resources::{HsdpaCodeLease, HsdpaCodeSet},
    tx_evidence::{TransportResolution, TransportSubmission},
};

const TTI_SLOTS: usize = 3;
// TS 25.211 7.8 starts HS-PDSCH two slots after its associated HS-SCCH.
const CONTROL_LEAD_SLOTS: usize = 2;
pub(crate) const CONTROL_LEAD_CHIPS: u64 = CONTROL_LEAD_SLOTS as u64 * CHIPS_PER_SLOT;
const BURST_SLOTS: usize = CONTROL_LEAD_SLOTS + TTI_SLOTS;
const BURST_CHIPS: usize = BURST_SLOTS * CHIPS_PER_SLOT as usize;
pub(crate) const MAX_PENDING: usize = 64;

pub(crate) const fn retained_transmission_bits() -> usize {
    (2 * BURST_CHIPS * size_of::<Complex32>()
        + size_of::<Storage>()
        + size_of::<PreparedTransmission>())
        * u8::BITS as usize
}

mod allocation;
mod orders;
pub(crate) use orders::retained_order_bits;
pub use orders::{
    CompletedOrder, OrderBuffer, OrderIdentity, OrderPreparation, OrderPreparationError,
    PreparedOrder, RejectedOrder,
};
mod preparation;
pub use allocation::{AllocationCalendar, ChannelUse, ReservationId};
#[cfg(test)]
mod tests;
pub use preparation::{Preparation, PreparationError, WaveformBuffer, WaveformPreparer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Dsp(DspError),
    Time,
    Identity,
    Codes,
    Conflict,
    Capacity,
    Unavailable,
}

#[derive(Clone, Copy)]
pub struct Amplitudes {
    pub control: f32,
    pub data: f32,
}

pub struct PreparedTransmission {
    storage: Box<Storage>,
}

struct Storage {
    metadata: Option<TransmissionMetadata>,
    control: Box<[Complex32]>,
    data: Box<[Complex32]>,
}

struct TransmissionMetadata {
    transmission: ScheduledTransmission,
    pool: HsdpaCodeSet,
    primary: PrimaryScramblingCode,
    control_code: u16,
    first_code: u8,
    code_count: u8,
    nominal_data_power: f64,
    start: ChipInstant,
    end: ChipInstant,
}

impl PreparedTransmission {
    fn metadata(&self) -> &TransmissionMetadata {
        self.storage
            .metadata
            .as_ref()
            .expect("prepared transmission metadata")
    }

    pub fn transmission(&self) -> ScheduledTransmission {
        self.metadata().transmission
    }

    pub(crate) fn primary_code(&self) -> PrimaryScramblingCode {
        self.metadata().primary
    }

    pub fn recycle(mut self) -> WaveformBuffer {
        self.storage.metadata = None;
        WaveformBuffer {
            storage: self.storage,
        }
    }

    pub(crate) fn retained_bits(&self) -> usize {
        retained_transmission_bits()
    }

    fn conflicts(&self, other: &Self) -> bool {
        self.metadata().conflicts(other.metadata())
    }
}

impl TransmissionMetadata {
    fn conflicts(&self, other: &Self) -> bool {
        let channels = |metadata: &Self| {
            ChannelUse::new(
                metadata.pool,
                metadata.transmission.downlink,
                metadata.control_code,
                metadata.first_code,
                metadata.code_count,
            )
            .expect("validated waveform allocation")
        };
        channels(self).conflicts(channels(other))
    }
}

fn overlaps(first: u64, end: u64, other_first: u64, other_end: u64) -> bool {
    first < other_end && other_first < end
}

pub struct RejectedTransmission {
    pub error: Error,
    pub transmission: PreparedTransmission,
    // Chips by which a Time rejection missed the rendering cursor.
    pub late_chips: Option<u64>,
}

struct Pending {
    transmission: PreparedTransmission,
    submission: TransportSubmission,
    resolution: TransportResolution,
    cancelled: bool,
}

pub struct CompletedTransmission {
    pub transmission: PreparedTransmission,
    pub submitted: bool,
}

pub struct TransmitBank {
    isolated: bool,
    power_limit: Option<f64>,
    lease: HsdpaCodeLease,
    primary: PrimaryScramblingCode,
    next: ChipInstant,
    progress: crate::tx_evidence::Reader,
    rate: usize,
    group_delay: usize,
    pending: Box<[Option<Pending>]>,
    orders: Box<[Option<orders::PendingOrder>]>,
    retiring: Box<[Option<UeKey>]>,
}

impl TransmitBank {
    pub fn required_storage_bytes(capacity: NonZeroUsize) -> Result<usize, Error> {
        if capacity.get() > MAX_PENDING {
            return Err(Error::Capacity);
        }
        capacity
            .get()
            .checked_mul(
                2 * BURST_CHIPS * size_of::<Complex32>()
                    + size_of::<Storage>()
                    + size_of::<Option<Pending>>()
                    + retained_order_bits() / u8::BITS as usize
                    + size_of::<Option<orders::PendingOrder>>(),
            )
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .and_then(|bytes| bytes.checked_add(MAX_PENDING * size_of::<Option<UeKey>>()))
            .ok_or(Error::Capacity)
    }

    pub fn new(
        source: &CommonTx,
        lease: HsdpaCodeLease,
        capacity: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
    ) -> Result<Self, (Error, HsdpaCodeLease)> {
        Self::for_carrier(
            source,
            source.information().primary_code(),
            lease,
            capacity,
            maximum_bytes,
        )
    }

    pub(crate) fn for_carrier(
        source: &CommonTx,
        primary: PrimaryScramblingCode,
        lease: HsdpaCodeLease,
        capacity: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
    ) -> Result<Self, (Error, HsdpaCodeLease)> {
        if !source.has_carrier(primary) || lease.primary_code() != primary.index() {
            return Err((Error::Identity, lease));
        }
        match Self::required_storage_bytes(capacity) {
            Ok(bytes) if bytes <= maximum_bytes.get() => (),
            _ => return Err((Error::Capacity, lease)),
        };
        Ok(Self {
            isolated: false,
            power_limit: source.hsdpa_power_limit().map(f64::from),
            lease,
            primary,
            next: source.next_chip(),
            progress: source.submission_progress(),
            rate: source.samples_per_slot() / CHIPS_PER_SLOT as usize,
            group_delay: source.group_delay_samples(),
            pending: (0..capacity.get()).map(|_| None).collect(),
            orders: (0..capacity.get()).map(|_| None).collect(),
            retiring: vec![None; MAX_PENDING].into_boxed_slice(),
        })
    }

    pub fn codes(&self) -> HsdpaCodeSet {
        *self.lease.codes()
    }

    pub(crate) fn primary_code(&self) -> PrimaryScramblingCode {
        self.primary
    }

    pub fn next_chip(&self) -> ChipInstant {
        self.next
    }

    pub fn enqueue(
        &mut self,
        transmission: PreparedTransmission,
    ) -> Result<(), RejectedTransmission> {
        let metadata = transmission.metadata();
        let error = if self.isolated
            || self
                .retiring
                .contains(&Some(metadata.transmission.key.ue()))
        {
            Some(Error::Unavailable)
        } else if metadata.pool != self.codes() || metadata.primary != self.primary {
            Some(Error::Codes)
        } else if !self.progress.available()
            || metadata.start.epoch != self.next.epoch
            || metadata.start.index < self.next.index
        {
            Some(Error::Time)
        } else if self.pending.iter().flatten().any(|pending| {
            pending.transmission.transmission().key == metadata.transmission.key
                || pending.transmission.conflicts(&transmission)
        }) || self.order_conflicts(metadata)
        {
            Some(Error::Conflict)
        } else if !self.admits_power(metadata) {
            Some(Error::Dsp(DspError::InvalidAmplitude))
        } else if self.pending.iter().all(Option::is_some) {
            Some(Error::Capacity)
        } else {
            None
        };
        if let Some(error) = error {
            let late_chips = (error == Error::Time)
                .then(|| self.next.index.saturating_sub(metadata.start.index))
                .filter(|late| *late != 0);
            return Err(RejectedTransmission {
                error,
                transmission,
                late_chips,
            });
        }
        let submission = self.progress.interval_receipt(
            metadata.start,
            metadata.end,
            self.rate,
            self.group_delay,
        );
        let resolution = self.progress.interval_resolution(
            metadata.start,
            metadata.end,
            self.rate,
            self.group_delay,
        );
        let (Some(submission), Some(resolution)) = (submission, resolution) else {
            return Err(RejectedTransmission {
                error: Error::Time,
                transmission,
                late_chips: None,
            });
        };
        *self
            .pending
            .iter_mut()
            .find(|pending| pending.is_none())
            .expect("checked capacity") = Some(Pending {
            transmission,
            submission,
            resolution,
            cancelled: false,
        });
        Ok(())
    }

    fn admits_power(&self, proposed: &TransmissionMetadata) -> bool {
        if !proposed.nominal_data_power.is_finite() || proposed.nominal_data_power < 0.0 {
            return false;
        }
        let Some(limit) = self.power_limit else {
            return true;
        };
        let active = |metadata: &TransmissionMetadata, chip: u64| {
            metadata.transmission.downlink.index <= chip && chip < metadata.end.index
        };
        let samples = std::iter::once(proposed.transmission.downlink.index).chain(
            self.pending
                .iter()
                .flatten()
                .filter(|pending| !pending.cancelled)
                .map(|pending| pending.transmission.metadata().transmission.downlink.index)
                .filter(|&chip| active(proposed, chip)),
        );
        samples.into_iter().all(|chip| {
            let total = self
                .pending
                .iter()
                .flatten()
                .filter(|pending| !pending.cancelled)
                .map(|pending| pending.transmission.metadata())
                .filter(|metadata| active(metadata, chip))
                .fold(proposed.nominal_data_power, |sum, metadata| {
                    sum + metadata.nominal_data_power
                });
            total.is_finite() && total <= limit * (1.0 + 8.0 * f64::from(f32::EPSILON))
        })
    }

    pub(crate) fn can_begin_retirement(&self, ue: UeKey) -> Result<(), Error> {
        if ue.epoch != self.next.epoch {
            return Err(Error::Identity);
        }
        if !self.retiring.contains(&Some(ue)) && !self.retiring.iter().any(Option::is_none) {
            return Err(Error::Capacity);
        }
        Ok(())
    }

    pub fn begin_retirement(&mut self, ue: UeKey) -> Result<(), Error> {
        self.can_begin_retirement(ue)?;
        if !self.retiring.contains(&Some(ue)) {
            let slot = self
                .retiring
                .iter_mut()
                .find(|slot| slot.is_none())
                .ok_or(Error::Capacity)?;
            *slot = Some(ue);
        }
        self.retire_orders(ue);
        for pending in self.pending.iter_mut().flatten() {
            if pending.transmission.transmission().key.ue() == ue
                && pending.transmission.metadata().start.index >= self.next.index
            {
                pending.cancelled = true;
            }
        }
        Ok(())
    }

    /// The controller must stop this UE's producers before requesting retirement.
    pub(crate) fn retirement_ready(&self, ue: UeKey) -> Result<bool, Error> {
        if !self.retiring.contains(&Some(ue)) {
            return Err(Error::Identity);
        }
        Ok(!self.orders_pending(ue)
            && !self
                .pending
                .iter()
                .flatten()
                .any(|pending| pending.transmission.transmission().key.ue() == ue))
    }

    pub fn finish_retirement(&mut self, ue: UeKey) -> Result<bool, Error> {
        let slot = self
            .retiring
            .iter()
            .position(|slot| *slot == Some(ue))
            .ok_or(Error::Identity)?;
        if self.orders_pending(ue)
            || self
                .pending
                .iter()
                .flatten()
                .any(|pending| pending.transmission.transmission().key.ue() == ue)
        {
            return Ok(false);
        }
        self.retiring[slot] = None;
        Ok(true)
    }

    pub(crate) fn paired_with(&self, source: &CommonTx) -> bool {
        self.next == source.next_chip() && self.progress.paired_with(&source.submission_progress())
    }

    pub(crate) fn matches_progress(&self, progress: &crate::tx_evidence::Reader) -> bool {
        self.progress.paired_with(progress)
    }

    /// Quarantine this carrier while retaining every owner for an explicit failed completion.
    pub(crate) fn isolate(&mut self) {
        if !self.isolated {
            tracing::warn!(
                primary_code = self.primary.index(),
                "Isolating HSDPA transmit bank"
            );
        }
        self.isolated = true;
        for pending in self.pending.iter_mut().flatten() {
            pending.cancelled = true;
        }
        self.isolate_orders();
    }

    /// Validate the entire slot before modifying common-channel samples or advancing clocks.
    pub(crate) fn preflight_slot(&mut self, start: ChipInstant, limit: Option<f64>) {
        let power = self
            .pending
            .iter()
            .flatten()
            .filter(|pending| !pending.cancelled)
            .map(|pending| pending.transmission.metadata())
            .filter(|metadata| {
                start.index >= metadata.transmission.downlink.index
                    && start.index < metadata.end.index
            })
            .map(|metadata| metadata.nominal_data_power)
            .sum::<f64>();
        if start != self.next
            || !power.is_finite()
            || power < 0.0
            || limit
                .or(self.power_limit)
                .is_some_and(|limit| power > limit * (1.0 + 8.0 * f64::from(f32::EPSILON)))
        {
            self.isolate();
        }
    }

    pub(crate) fn mix_slot(
        &mut self,
        start: ChipInstant,
        output: &mut [Complex32],
    ) -> Result<f64, Error> {
        if output.len() != CHIPS_PER_SLOT as usize || (!self.isolated && start != self.next) {
            return Err(Error::Time);
        }
        self.preflight_slot(start, None);
        let next = start.index.checked_add(CHIPS_PER_SLOT).ok_or(Error::Time)?;
        if self.isolated {
            self.next = ChipInstant {
                index: next,
                ..start
            };
            return Ok(0.0);
        }
        let mut reserved_power = 0.0;
        for pending in self.pending.iter().flatten() {
            if pending.cancelled {
                continue;
            }
            let burst = &pending.transmission;
            let metadata = burst.metadata();
            if start.index >= metadata.start.index && start.index < metadata.end.index {
                if start.index >= metadata.transmission.downlink.index {
                    reserved_power += metadata.nominal_data_power;
                }
                let offset = (start.index - metadata.start.index) as usize;
                for ((chip, control), data) in output
                    .iter_mut()
                    .zip(&burst.storage.control[offset..offset + CHIPS_PER_SLOT as usize])
                    .zip(&burst.storage.data[offset..offset + CHIPS_PER_SLOT as usize])
                {
                    *chip += *control + *data;
                }
            }
        }
        self.mix_orders(start, output);
        self.next.index = next;
        Ok(reserved_power)
    }

    pub fn take_completed(&mut self) -> Option<CompletedTransmission> {
        let slot = self.pending.iter_mut().find(|pending| {
            pending.as_ref().is_some_and(|pending| {
                pending.cancelled
                    || (self.next.index >= pending.transmission.metadata().end.index
                        && (pending.resolution.resolved() || !pending.resolution.viable()))
            })
        })?;
        let pending = slot.take().expect("completed transmission");
        Some(CompletedTransmission {
            submitted: !pending.cancelled && pending.submission.submitted(),
            transmission: pending.transmission,
        })
    }

    /// Retire all queued samples before releasing this lease through RadioResources.
    #[cfg(test)]
    pub fn into_retained(self) -> (HsdpaCodeLease, impl Iterator<Item = PreparedTransmission>) {
        (
            self.lease,
            self.pending
                .into_vec()
                .into_iter()
                .flatten()
                .map(|pending| pending.transmission),
        )
    }
}
