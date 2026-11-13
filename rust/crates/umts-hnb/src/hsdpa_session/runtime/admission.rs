// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_packet::PlanFormat;
use umts_dsp::hsdpa::{CrcMethod, ScchAssignment, TransmissionVersion};

const REDUNDANCY_VERSIONS: u8 = 8;
const TURBO_STREAMS: usize = 3;
const TRANSPORT_SIZE_INDICES: u8 = 64;
const UNMAPPED_TRANSPORT_SIZE: u8 = 63;

#[derive(Clone, Copy)]
pub struct SchedulingBudget {
    pub maximum_formats: NonZeroUsize,
    pub maximum_coded_bits: NonZeroUsize,
    pub maximum_plan_bytes: NonZeroUsize,
    pub maximum_buffer_bytes: NonZeroUsize,
    pub maximum_encoder_bytes: NonZeroUsize,
    pub maximum_total_bytes: NonZeroUsize,
}

impl SchedulingBudget {
    /// Derive one carrier's budget from the original aggregate UE budget.
    /// Pass the original budget to `SchedulingPreparation::new`, which applies the carrier split
    /// itself.
    pub fn for_carriers(self, carriers: usize) -> Result<Self, Error> {
        if !matches!(carriers, 1 | 2) {
            return Err(Error::Capacity);
        }
        let components = self
            .maximum_plan_bytes
            .get()
            .checked_add(self.maximum_buffer_bytes.get())
            .and_then(|bytes| bytes.checked_add(self.maximum_encoder_bytes.get()))
            .and_then(|bytes| bytes.checked_add(scheduling_resource_overhead()))
            .ok_or(Error::Capacity)?;
        let maximum_total_bytes =
            NonZeroUsize::new((self.maximum_total_bytes.get() / carriers).min(components))
                .ok_or(Error::Capacity)?;
        Ok(Self {
            maximum_total_bytes,
            ..self
        })
    }
}

// Exactly the fixed owner overhead charged by SchedulingResources::new.
// encoder/plan storage and coded buffers are covered by their component caps.
fn scheduling_resource_overhead() -> usize {
    std::mem::size_of::<SchedulingResources>()
        - std::mem::size_of::<EncoderBank>()
        - std::mem::size_of::<PlanBank>()
}

pub struct SchedulingAdmission {
    pub settings: SchedulingSettings,
    pub budget: SchedulingBudget,
    pub feedback: crate::hsdpa_rx::FeedbackPolicy,
    pub cell: CellSchedulingBudget,
}

pub(crate) struct SchedulingPreparation {
    construction: Arc<umts_rrc::hsdpa::InitialConfiguration>,
    rates: Rates,
    settings: SchedulingSettings,
    budget: SchedulingBudget,
    reservation: budget::Reservation,
}

impl SchedulingPreparation {
    pub(crate) fn new(
        construction: Arc<umts_rrc::hsdpa::InitialConfiguration>,
        codes: crate::radio_resources::HsdpaCodeSet,
        settings: SchedulingSettings,
        budget: SchedulingBudget,
        cell: &CellSchedulingBudget,
    ) -> Result<Self, Error> {
        let rates =
            Rates::new(construction.parameters().harq.category(), codes).map_err(Error::Rate)?;
        let budget =
            budget.for_carriers(1 + usize::from(construction.secondary_cell().is_some()))?;
        let reservation = cell
            .reserve(budget.maximum_total_bytes)
            .ok_or(Error::Capacity)?;
        Ok(Self {
            construction,
            rates,
            settings,
            budget,
            reservation,
        })
    }
}

impl Session {
    pub fn prepare_scheduling_resources(
        &self,
        settings: SchedulingSettings,
        budget: SchedulingBudget,
        cell: &CellSchedulingBudget,
    ) -> Result<Box<SchedulingResources>, Error> {
        self.prepare_scheduling_resources_on(
            umts_types::hsdpa::CarrierId::Primary,
            settings,
            budget,
            cell,
        )
    }

    pub fn prepare_scheduling_resources_on(
        &self,
        carrier: umts_types::hsdpa::CarrierId,
        settings: SchedulingSettings,
        budget: SchedulingBudget,
        cell: &CellSchedulingBudget,
    ) -> Result<Box<SchedulingResources>, Error> {
        if !self.has_carrier(carrier) {
            return Err(Error::Identity);
        }

        if self.links[carrier.index()].scheduling.is_some() || self.retirement.started() {
            return Err(Error::Phase);
        }
        SchedulingPreparation::new(
            self.procedure.construction().configuration().clone(),
            self.links[carrier.index()].rates.codes(),
            settings,
            budget,
            cell,
        )?
        .build()
    }
}

impl SchedulingPreparation {
    pub(crate) fn build(self) -> Result<Box<SchedulingResources>, Error> {
        let settings = self.settings;
        let budget = self.budget;
        let reservation = self.reservation;
        let parameters = self.construction.parameters();
        let mut rates: Vec<_> = self.rates.all_candidates().collect();
        let mut sizes: Vec<_> = rates
            .iter()
            .map(|rate| rate.reference.transport_bits)
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        rates.sort_by_key(|rate| {
            (
                rate.reference.codes,
                rate.reference.modulation.bits_per_symbol(),
            )
        });
        rates.dedup_by_key(|rate| {
            (
                rate.reference.codes,
                rate.reference.modulation.bits_per_symbol(),
            )
        });
        let mut memories = parameters.harq.process_memory().to_vec();
        memories.sort_unstable();
        memories.dedup();
        let mut formats = Vec::new();
        for transport_bits in sizes {
            let coding = umts_dsp::turbo::TurboBlockPlan::new(
                umts_dsp::turbo::TurboBlockFormat {
                    block_bits: transport_bits.get(),
                    blocks: 1,
                    crc: umts_dsp::crc::TransportCrc::Crc24,
                },
                budget.maximum_coded_bits,
            )
            .map_err(Error::Rate)?;
            for &soft_bits in &memories {
                // TS 25.212 4.5.4 retains the systematic stream in virtual IR storage.
                if soft_bits.get() < coding.coded_bits() / TURBO_STREAMS {
                    continue;
                }
                for rate in &rates {
                    // TS 25.321 9.2.3.1 preserves the original TB across changed allocations.
                    let size_index = (0..TRANSPORT_SIZE_INDICES)
                        .find(|&index| {
                            rate.reference.table.transport_bits(
                                rate.reference.modulation,
                                rate.reference.codes,
                                index,
                            ) == Ok(transport_bits)
                        })
                        .unwrap_or(UNMAPPED_TRANSPORT_SIZE);
                    for version in 0..REDUNDANCY_VERSIONS {
                        let assignment = ScchAssignment::new(
                            rate.assignment.first_code(),
                            rate.reference.codes,
                            rate.reference.modulation,
                            size_index,
                            0,
                            TransmissionVersion::new(version).expect("bounded redundancy version"),
                            false,
                        )
                        .and_then(|assignment| assignment.with_table(rate.reference.table))
                        .and_then(|assignment| {
                            assignment.on_control(rate.assignment.control_list_number())
                        })
                        .map_err(Error::Rate)?;
                        let format = PlanFormat::new(assignment, transport_bits, soft_bits);
                        if formats.len() == budget.maximum_formats.get() {
                            return Err(Error::Capacity);
                        }
                        formats.push(format);
                    }
                }
            }
        }
        let encoders = self
            .rates
            .encoders(
                budget.maximum_coded_bits,
                CrcMethod::Method1,
                budget.maximum_encoder_bytes.min(budget.maximum_total_bytes),
            )
            .map_err(Error::Rate)?;
        let bytes = encoders
            .maximum_coded_bits()
            .get()
            .checked_mul(std::mem::size_of::<umts_dsp::bits::Bit>())
            .and_then(|value| value.checked_add(std::mem::size_of::<Option<BlockBuffer>>()))
            .and_then(|value| {
                value.checked_mul(parameters.harq.process_memory().len() + MAX_STAGED_QUEUES)
            })
            .and_then(|bytes| {
                bytes.checked_add(crate::hsdpa_packet::PacketOwner::staging_metadata_bytes())
            })
            .ok_or(Error::Capacity)?;
        if bytes > budget.maximum_buffer_bytes.get() {
            return Err(Error::Capacity);
        }
        let resource_overhead = scheduling_resource_overhead();
        let remaining = budget
            .maximum_total_bytes
            .get()
            .checked_sub(encoders.retained_bytes())
            .and_then(|value| value.checked_sub(bytes))
            .and_then(|value| value.checked_sub(resource_overhead))
            .and_then(NonZeroUsize::new)
            .ok_or(Error::Capacity)?;
        let plans = PlanBank::new(
            &formats,
            parameters.harq.category(),
            parameters.h_rnti,
            budget.maximum_formats,
            budget.maximum_coded_bits,
            budget.maximum_plan_bytes.min(remaining),
        )
        .map_err(Error::Rate)?;
        let processes = u8::try_from(parameters.harq.process_memory().len())
            .ok()
            .and_then(std::num::NonZeroU8::new)
            .ok_or(Error::Capacity)?;
        let resources = SchedulingResources::new(
            encoders,
            plans,
            processes,
            settings,
            budget.maximum_total_bytes,
            reservation,
        )
        .map_err(Error::Rate)?;
        tracing::info!(
            category = parameters.harq.category().value(),
            formats = formats.len(),
            plan_bytes = resources.plans.retained_bytes(),
            encoder_bytes = resources.encoders.retained_bytes(),
            retained_bytes = resources.retained_bytes,
            "HSDPA scheduling resources prepared"
        );
        Ok(Box::new(resources))
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::test_support::count;

    fn budget(total: usize) -> SchedulingBudget {
        SchedulingBudget {
            maximum_formats: count(65536),
            maximum_coded_bits: count(150000),
            maximum_plan_bytes: count(300_000_000),
            maximum_buffer_bytes: count(4_000_000),
            maximum_encoder_bytes: count(16_000_000),
            maximum_total_bytes: count(total),
        }
    }

    #[test]
    fn ue_budget_splits_once_and_single_carrier_reserves_only_usable_capacity() {
        assert_eq!(
            budget(320_000_000)
                .for_carriers(1)
                .unwrap()
                .maximum_total_bytes
                .get(),
            320_000_000
        );
        let aggregate = budget(640_000_000);
        let dual = aggregate.for_carriers(2).unwrap();
        assert_eq!(dual.maximum_total_bytes.get(), 320_000_000);
        let single = aggregate.for_carriers(1).unwrap();
        assert_eq!(
            single.maximum_total_bytes.get(),
            320_000_000 + scheduling_resource_overhead()
        );
        assert_eq!(dual.maximum_plan_bytes, aggregate.maximum_plan_bytes);
        assert_eq!(dual.maximum_buffer_bytes, aggregate.maximum_buffer_bytes);
        assert_eq!(dual.maximum_encoder_bytes, aggregate.maximum_encoder_bytes);
        assert_eq!(dual.maximum_formats, aggregate.maximum_formats);
        assert_eq!(dual.maximum_coded_bits, aggregate.maximum_coded_bits);
    }

    #[test]
    fn invalid_counts_zero_share_and_overflow_fail_without_rounding_up() {
        for count in [0, 3, usize::MAX] {
            assert!(budget(100).for_carriers(count).is_err());
        }
        assert!(budget(1).for_carriers(2).is_err());
        assert_eq!(
            budget(101)
                .for_carriers(2)
                .unwrap()
                .maximum_total_bytes
                .get(),
            50
        );
        let mut overflow = budget(100);
        overflow.maximum_plan_bytes = count(usize::MAX);
        assert!(overflow.for_carriers(1).is_err());
    }

    #[test]
    fn dual_peak_and_retained_reservations_stay_within_aggregate_and_roll_back() {
        let share = budget(101).for_carriers(2).unwrap().maximum_total_bytes;
        let cell = CellSchedulingBudget::new(count(101));
        let mut primary = cell.reserve(share).unwrap();
        let secondary = cell.reserve(share).unwrap();
        assert_eq!(cell.available_bytes(), 1);
        assert!(cell.reserve(count(2)).is_none());
        assert!(!primary.retain(share.get() + 1));
        assert!(primary.retain(30));
        assert_eq!(cell.available_bytes(), 21);
        drop(secondary);
        drop(primary);
        assert_eq!(cell.available_bytes(), 101);
        let too_small = CellSchedulingBudget::new(count(99));
        let primary = too_small.reserve(share).unwrap();
        assert!(too_small.reserve(share).is_none());
        drop(primary);
        assert_eq!(too_small.available_bytes(), 99);
    }
}
