// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_dsp::DspError;
use umts_mac::hsdpa::{PackedPdu, ScheduledTransmission, TransmissionKey};
use umts_types::{
    hsdpa::CarrierId,
    time::{CHIPS_PER_SLOT, ChipInstant},
};

// TS 25.321 9.1.4: without the TSN extension, the aggregate limit is 26
// reordering SDUs across every MAC-ehs PDU received by this UE in a TTI.
// TS 25.331 8.5.60 allows the extension only with MIMO.
pub(super) const MAX_REORDERING_SDUS: usize = 26;
const TTI_CHIPS: u64 = 3 * CHIPS_PER_SLOT;
// Two cells x eight HARQ processes, with another sixteen slots for failed
// submissions retained until their TTI ends. Exhaustion backpressures scheduling.
const RESERVATIONS: usize = 32;

impl super::Session {
    /// Largest new transport block that leaves the other active carrier an equal share of
    /// one TTI's reordering SDUs. A block of n smallest-size SDUs can add one segment entry.
    pub(super) fn shared_tti_transport_bits(&self, queue: u8) -> Option<usize> {
        if !self.procedure.construction().configuration().uses_mac_ehs()
            || self.secondary_state() != Some(super::SecondaryState::Active)
        {
            return None;
        }
        let smallest = self
            .procedure
            .construction()
            .parameters()
            .queues
            .queues()
            .iter()
            .find(|configured| configured.id() == queue)?
            .sizes()
            .iter()
            .flatten()
            .map(|size| size.get())
            .min()?;
        Some((MAX_REORDERING_SDUS / 2 - 1) * smallest)
    }
}

#[derive(Clone, Copy)]
struct Reservation {
    carrier: CarrierId,
    transmission: ScheduledTransmission,
    sdus: usize,
}

pub(super) struct SduBudget {
    origin: ChipInstant,
    observed_now: ChipInstant,
    entries: [Option<Reservation>; RESERVATIONS],
}

impl SduBudget {
    pub(super) fn new(origin: ChipInstant) -> Self {
        Self {
            origin,
            observed_now: origin,
            entries: [None; RESERVATIONS],
        }
    }

    pub(super) fn observe_time(&mut self, now: ChipInstant) -> Result<(), DspError> {
        if now.epoch != self.origin.epoch {
            return Err(DspError::DiscontinuousTime);
        }
        if now.index <= self.observed_now.index {
            return Ok(());
        }
        self.observed_now = now;
        for slot in &mut self.entries {
            if slot.is_some_and(|entry| {
                entry
                    .transmission
                    .downlink
                    .index
                    .checked_add(TTI_CHIPS)
                    .is_some_and(|end| end <= now.index)
            }) {
                *slot = None;
            }
        }
        Ok(())
    }

    pub(super) fn earliest(
        &self,
        carrier: CarrierId,
        start: ChipInstant,
        sdus: usize,
    ) -> Result<ChipInstant, DspError> {
        if sdus == 0 || sdus > MAX_REORDERING_SDUS {
            return Err(DspError::Capacity);
        }
        if start.epoch != self.origin.epoch
            || start.index < self.origin.index
            || !(start.index - self.origin.index).is_multiple_of(TTI_CHIPS)
        {
            return Err(DspError::DiscontinuousTime);
        }
        if self.entries.iter().all(Option::is_some) {
            return Err(DspError::Capacity);
        }
        let floor = start.index.max(self.observed_now.index);
        let offset = (floor - self.origin.index)
            .div_ceil(TTI_CHIPS)
            .checked_mul(TTI_CHIPS)
            .ok_or(DspError::DiscontinuousTime)?;
        let mut candidate = ChipInstant {
            index: self
                .origin
                .index
                .checked_add(offset)
                .ok_or(DspError::DiscontinuousTime)?,
            ..start
        };
        // Each occupied candidate consumes at least one reservation, so the first
        // free TTI is found in at most RESERVATIONS + 1 probes without allocation.
        for _ in 0..=RESERVATIONS {
            let mut total = sdus;
            let mut same_carrier = false;
            for entry in self
                .entries
                .iter()
                .flatten()
                .filter(|entry| entry.transmission.downlink == candidate)
            {
                total += entry.sdus;
                same_carrier |= entry.carrier == carrier;
            }
            if !same_carrier && total <= MAX_REORDERING_SDUS {
                return Ok(candidate);
            }
            candidate.index = candidate
                .index
                .checked_add(TTI_CHIPS)
                .ok_or(DspError::DiscontinuousTime)?;
        }
        Err(DspError::Capacity)
    }

    pub(super) fn reserve(
        &mut self,
        carrier: CarrierId,
        transmission: ScheduledTransmission,
        packet: PackedPdu,
    ) -> Result<(), DspError> {
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.carrier == carrier && entry.transmission.key == transmission.key)
            || self.earliest(carrier, transmission.downlink, packet.pdus)? != transmission.downlink
        {
            return Err(DspError::Capacity);
        }
        // PackedPdu::pdus counts all LCH-ID/L entries, including the final partial
        // SDU and a continuing first segment. Do not subtract end_fragment_octets.
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(DspError::Capacity)?;
        *slot = Some(Reservation {
            carrier,
            transmission,
            sdus: packet.pdus,
        });
        Ok(())
    }

    /// A failed aggregate completion may include a transmitted prefix, so only an unpublished
    /// cancellation or explicit queue rejection releases the charge.
    pub(super) fn release_untransmitted(&mut self, carrier: CarrierId, key: TransmissionKey) {
        if let Some(slot) = self.entries.iter_mut().find(|slot| {
            slot.is_some_and(|entry| entry.carrier == carrier && entry.transmission.key == key)
        }) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count};
    use std::num::NonZeroU64;
    use umts_mac::hsdpa::{HarqProcess, MacdPdu, PacketQueue, ProcessId, QueueFormat};
    use umts_types::{UeKey, bits::Bits};

    fn transmission(id: u64, tti: u64) -> ScheduledTransmission {
        let mut process = HarqProcess::new(
            UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            NonZeroU64::new(id).unwrap(),
            ProcessId::new(0).unwrap(),
        );
        process.replace(());
        process
            .schedule(at(tti * TTI_CHIPS), at((tti + 10) * TTI_CHIPS))
            .unwrap()
    }

    fn packed_geometry(sdus: usize, sdu_octets: usize, transport_octets: usize) -> PackedPdu {
        let mut sizes = [None; 8];
        sizes[0] = Some(count(sdu_octets * 8));
        let mut queue = PacketQueue::new(
            QueueFormat::ehs(0, 1, sizes).unwrap(),
            count(sdus),
            count(65536),
        )
        .unwrap();
        let payload = vec![0x5a; sdu_octets];
        for _ in 0..sdus {
            queue
                .enqueue(MacdPdu {
                    size_index: 0,
                    bits: Bits::new(&payload, sdu_octets * 8).unwrap(),
                })
                .unwrap();
        }
        let mut bytes = vec![0; transport_octets];
        let packet = queue
            .publish(transport_octets * 8, &mut bytes, |packet, _| {
                Ok::<_, ()>(packet)
            })
            .unwrap()
            .unwrap();
        assert_eq!(packet.pdus, sdus);
        packet
    }

    fn packed(sdus: usize, fragmented_last: bool) -> PackedPdu {
        let octets = 1 + sdus * (2 + 48) - if fragmented_last { 24 } else { 0 };
        let packet = packed_geometry(sdus, 48, octets);
        assert_eq!(packet.end_fragment_octets != 0, fragmented_last);
        packet
    }

    #[test]
    fn two_twenty_sdu_blocks_move_to_distinct_ttis() {
        let mut budget = SduBudget::new(at(0));
        let block = packed(20, false);
        budget
            .reserve(CarrierId::Primary, transmission(1, 0), block)
            .unwrap();
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(0), block.pdus),
            Ok(at(TTI_CHIPS))
        );
        assert!(
            budget
                .reserve(CarrierId::Secondary, transmission(2, 0), block)
                .is_err()
        );
        budget
            .reserve(CarrierId::Secondary, transmission(2, 1), block)
            .unwrap();
        assert_eq!(
            budget.earliest(CarrierId::Primary, at(TTI_CHIPS), block.pdus),
            Ok(at(2 * TTI_CHIPS))
        );
    }

    #[test]
    fn full_rate_eleven_plus_eleven_stays_simultaneous_in_consecutive_ttis() {
        let mut budget = SduBudget::new(at(0));
        let block = packed_geometry(11, 512, 42192 / 8);
        assert_eq!(block.transport_bits, 42192);
        assert_ne!(block.end_fragment_octets, 0);
        for tti in 0..4 {
            for (offset, carrier) in [CarrierId::Primary, CarrierId::Secondary]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    budget.earliest(carrier, at(tti * TTI_CHIPS), block.pdus),
                    Ok(at(tti * TTI_CHIPS))
                );
                budget
                    .reserve(
                        carrier,
                        transmission(tti * 2 + offset as u64 + 1, tti),
                        block,
                    )
                    .unwrap();
            }
        }
        assert_eq!(budget.entries.iter().flatten().count(), 8);
        budget.observe_time(at(TTI_CHIPS)).unwrap();
        assert_eq!(budget.entries.iter().flatten().count(), 6);
    }

    #[test]
    fn segmentation_counts_and_future_preparation_does_not_forget_earlier_ttis() {
        let mut budget = SduBudget::new(at(0));
        budget
            .reserve(CarrierId::Primary, transmission(1, 3), packed(20, false))
            .unwrap();
        budget
            .reserve(CarrierId::Primary, transmission(2, 0), packed(10, false))
            .unwrap();
        // Sixteen entries include a partial last SDU, which still consumes one slot.
        budget
            .reserve(CarrierId::Secondary, transmission(3, 0), packed(16, true))
            .unwrap();
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(3 * TTI_CHIPS), 7),
            Ok(at(4 * TTI_CHIPS))
        );
        let mut over = SduBudget::new(at(0));
        over.reserve(CarrierId::Primary, transmission(4, 0), packed(11, false))
            .unwrap();
        assert!(
            over.reserve(CarrierId::Secondary, transmission(5, 0), packed(16, true))
                .is_err()
        );
    }

    #[test]
    fn exact_cancellation_rolls_back_but_delayed_results_cannot_release_retry() {
        let mut budget = SduBudget::new(at(0));
        let block = packed(20, false);
        let old = transmission(1, 0);
        budget.reserve(CarrierId::Primary, old, block).unwrap();
        budget.release_untransmitted(CarrierId::Secondary, old.key);
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(0), 20),
            Ok(at(TTI_CHIPS))
        );
        budget.release_untransmitted(CarrierId::Primary, old.key);
        let retry = transmission(2, 0);
        budget.reserve(CarrierId::Primary, retry, block).unwrap();
        budget.release_untransmitted(CarrierId::Primary, old.key);
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(0), 20),
            Ok(at(TTI_CHIPS))
        );
        budget.observe_time(at(TTI_CHIPS)).unwrap();
        budget
            .reserve(CarrierId::Primary, transmission(3, 1), block)
            .unwrap();
        budget.release_untransmitted(CarrierId::Primary, retry.key);
        budget.observe_time(at(0)).unwrap();
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(TTI_CHIPS), 20),
            Ok(at(2 * TTI_CHIPS))
        );
    }

    #[test]
    fn equal_per_cell_keys_remain_separate_reservations() {
        let mut budget = SduBudget::new(at(0));
        let tx = transmission(1, 0);
        let block = packed(11, false);
        budget.reserve(CarrierId::Primary, tx, block).unwrap();
        budget.reserve(CarrierId::Secondary, tx, block).unwrap();
        budget.release_untransmitted(CarrierId::Primary, tx.key);
        assert_eq!(budget.entries.iter().flatten().count(), 1);
        assert_eq!(
            budget.earliest(CarrierId::Primary, at(0), 20),
            Ok(at(TTI_CHIPS))
        );
    }

    #[test]
    fn bounded_capacity_never_overwrites_future_reservations() {
        let mut budget = SduBudget::new(at(0));
        let block = packed(20, false);
        for index in 0..RESERVATIONS {
            budget
                .reserve(
                    CarrierId::Primary,
                    transmission(index as u64 + 1, index as u64),
                    block,
                )
                .unwrap();
        }
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(TTI_CHIPS), 1),
            Err(DspError::Capacity)
        );
        budget.observe_time(at(TTI_CHIPS)).unwrap();
        assert_eq!(
            budget.earliest(CarrierId::Secondary, at(TTI_CHIPS), 7),
            Ok(at(RESERVATIONS as u64 * TTI_CHIPS))
        );
    }
}
