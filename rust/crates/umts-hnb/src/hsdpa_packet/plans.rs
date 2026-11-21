// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use umts_dsp::hsdpa::{
    Modulation, ScchAssignment, ScchEncoder, TransmissionPlan, TransmissionVersion,
};
use umts_types::hsdpa::Category;

type CodingKey = (
    (NonZeroUsize, NonZeroUsize, u8, usize),
    u8,
    TransmissionVersion,
    bool,
);
// The H-RNTI of the cell-wide prototypes. Each UE's bank encodes its own HS-SCCH control.
const PROTOTYPE_H_RNTI: u16 = 0;

/// A source table depends only on the coding, not on the UE, its H-RNTI or its first code, so
/// one copy serves every UE and carrier. Tables live as long as the cell and are charged to the
/// cell scheduling budget once.
#[derive(Clone)]
pub struct SharedPlanTables {
    inner: Arc<Mutex<SharedTables>>,
    remaining: Arc<AtomicUsize>,
}

struct SharedTables {
    plans: BTreeMap<CodingKey, TransmissionPlan>,
    bytes: usize,
}

impl SharedPlanTables {
    pub(crate) fn new(remaining: Arc<AtomicUsize>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SharedTables {
                plans: BTreeMap::new(),
                bytes: 0,
            })),
            remaining,
        }
    }

    pub fn retained_bytes(&self) -> usize {
        self.lock().bytes
    }

    pub fn tables(&self) -> usize {
        self.lock().plans.len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SharedTables> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanFormat {
    pub transport_bits: NonZeroUsize,
    pub soft_bits: NonZeroUsize,
    pub first_code: u8,
    pub codes: u8,
    pub modulation: Modulation,
    pub size_index: u8,
    pub version: TransmissionVersion,
    pub table: umts_dsp::hsdpa::TransportTable,
    pub control_list_number: u8,
}

impl PlanFormat {
    fn geometry(self) -> (NonZeroUsize, NonZeroUsize, u8, usize) {
        (
            self.transport_bits,
            self.soft_bits,
            self.codes,
            self.modulation.bits_per_symbol(),
        )
    }

    fn same_coding(self, other: Self) -> bool {
        self.geometry() == other.geometry()
            && self.size_index == other.size_index
            && self.version == other.version
            && self.table == other.table
    }

    fn shared_key(self) -> CodingKey {
        (
            self.geometry(),
            self.size_index,
            self.version,
            self.table == umts_dsp::hsdpa::TransportTable::OctetAligned,
        )
    }

    #[cfg(test)]
    fn coding_key(self) -> impl Ord {
        (
            self.geometry(),
            self.size_index,
            self.version,
            self.table == umts_dsp::hsdpa::TransportTable::OctetAligned,
        )
    }

    pub fn new(
        assignment: ScchAssignment,
        transport_bits: NonZeroUsize,
        soft_bits: NonZeroUsize,
    ) -> Self {
        Self {
            transport_bits,
            soft_bits,
            first_code: assignment.first_code(),
            codes: assignment.code_count(),
            modulation: assignment.modulation(),
            size_index: assignment.transport_size_index(),
            version: assignment.version(),
            table: assignment.table(),
            control_list_number: assignment.control_list_number(),
        }
    }

    fn assignment(self) -> Result<ScchAssignment, DspError> {
        ScchAssignment::new(
            self.first_code,
            self.codes,
            self.modulation,
            self.size_index,
            0,
            self.version,
            false,
        )?
        .with_table(self.table)?
        .on_control(self.control_list_number)
    }
}

pub struct PlanBank {
    h_rnti: u16,
    entries: Box<[(PlanFormat, TransmissionPlan)]>,
    retained_bytes: usize,
}

impl PlanBank {
    pub fn h_rnti(&self) -> u16 {
        self.h_rnti
    }
    pub fn formats(&self) -> impl Iterator<Item = PlanFormat> + '_ {
        self.entries.iter().map(|(format, _)| *format)
    }

    pub fn matching_formats(
        &self,
        transport_bits: NonZeroUsize,
        soft_bits: NonZeroUsize,
        codes: u8,
        modulation: Modulation,
    ) -> impl Iterator<Item = PlanFormat> + '_ {
        let geometry = (
            transport_bits,
            soft_bits,
            codes,
            modulation.bits_per_symbol(),
        );
        let start = self
            .entries
            .partition_point(|(key, _)| key.geometry() < geometry);
        let end = self
            .entries
            .partition_point(|(key, _)| key.geometry() <= geometry);
        self.entries[start..end].iter().map(|(format, _)| *format)
    }

    #[cfg(test)]
    pub fn new(
        formats: &[PlanFormat],
        category: Category,
        h_rnti: u16,
        maximum_formats: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if formats.is_empty() || formats.len() > maximum_formats.get() {
            return Err(DspError::Capacity);
        }
        let mut formats: Vec<_> = formats.iter().copied().enumerate().collect();
        for (_, format) in &formats {
            format.assignment()?;
        }
        formats.sort_by_key(|(_, format)| format.coding_key());
        formats.dedup_by(|(_, left), (_, right)| left.same_coding(*right));
        formats.sort_by_key(|(index, _)| *index);
        let mut retained_bytes = std::mem::size_of::<Self>();
        for (_, format) in &formats {
            let bytes = TransmissionPlan::required_storage_bytes(format.assignment()?)?;
            retained_bytes = retained_bytes
                .checked_add(bytes)
                .and_then(|value| {
                    value.checked_add(
                        std::mem::size_of::<(PlanFormat, TransmissionPlan)>()
                            - std::mem::size_of::<TransmissionPlan>(),
                    )
                })
                .ok_or(DspError::Capacity)?;
            if retained_bytes > maximum_bytes.get() {
                return Err(DspError::Capacity);
            }
        }
        let control = ScchEncoder::new(h_rnti)?;
        let mut entries = Vec::with_capacity(formats.len());
        for (_, format) in formats {
            let plan = TransmissionPlan::retransmission(
                format.assignment()?,
                category,
                &control,
                format.transport_bits,
                format.soft_bits,
                maximum_coded_bits,
            )?;
            entries.push((format, plan));
        }
        entries.sort_by_key(|(format, _)| format.geometry());
        Ok(Self {
            h_rnti,
            entries: entries.into_boxed_slice(),
            retained_bytes,
        })
    }

    #[cfg(test)]
    fn plan(&self, format: PlanFormat) -> Option<&TransmissionPlan> {
        self.entries
            .iter()
            .find(|(candidate, _)| *candidate == format)
            .map(|(_, plan)| plan)
    }

    pub fn get_mut(&mut self, format: PlanFormat) -> Option<&mut TransmissionPlan> {
        let start = self
            .entries
            .partition_point(|(key, _)| key.geometry() < format.geometry());
        let end = self
            .entries
            .partition_point(|(key, _)| key.geometry() <= format.geometry());
        let (_, plan) = self.entries[start..end]
            .iter_mut()
            .find(|(key, _)| key.same_coding(format))?;
        // OVSF placement changes HS-SCCH, not the retained rate-matching map.
        plan.select_first_code_on(format.first_code, format.control_list_number)
            .ok()?;
        Some(plan)
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Missing tables are built into `shared` and charged to the cell budget. Only per-UE plan
    /// state and HS-SCCH control words count against `maximum_bytes`.
    pub fn with_shared(
        formats: &[PlanFormat],
        category: Category,
        h_rnti: u16,
        maximum_formats: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
        shared: &SharedPlanTables,
    ) -> Result<Self, DspError> {
        if formats.is_empty() || formats.len() > maximum_formats.get() {
            return Err(DspError::Capacity);
        }
        let entry_bytes = std::mem::size_of::<(PlanFormat, TransmissionPlan)>();
        let retained_bytes = formats
            .len()
            .checked_mul(entry_bytes)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or(DspError::Capacity)?;
        if retained_bytes > maximum_bytes.get() {
            return Err(DspError::Capacity);
        }
        for format in formats {
            let assignment = format.assignment()?;
            assignment.validate_category(category, format.transport_bits.get())?;
            if format.soft_bits.get() > category.limits().total_soft_bits {
                return Err(DspError::Capacity);
            }
        }
        let control = ScchEncoder::new(h_rnti)?;
        let mut tables = shared.lock();
        let missing: Vec<PlanFormat> = {
            let mut seen = std::collections::BTreeSet::new();
            formats
                .iter()
                .filter(|format| {
                    !tables.plans.contains_key(&format.shared_key())
                        && seen.insert(format.shared_key())
                })
                .copied()
                .collect()
        };
        if !missing.is_empty() {
            let mut bytes = 0usize;
            for format in &missing {
                bytes = bytes
                    .checked_add(TransmissionPlan::shared_storage_bytes(
                        format.assignment()?,
                    )?)
                    .ok_or(DspError::Capacity)?;
            }
            shared
                .remaining
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(bytes)
                })
                .map_err(|_| DspError::Capacity)?;
            let prototype = ScchEncoder::new(PROTOTYPE_H_RNTI)?;
            let mut built = Vec::with_capacity(missing.len());
            for format in &missing {
                match TransmissionPlan::retransmission(
                    format.assignment()?,
                    category,
                    &prototype,
                    format.transport_bits,
                    format.soft_bits,
                    maximum_coded_bits,
                ) {
                    Ok(plan) => built.push((format.shared_key(), plan)),
                    Err(error) => {
                        shared.remaining.fetch_add(bytes, Ordering::Relaxed);
                        return Err(error);
                    }
                }
            }
            tables.bytes += bytes;
            for (key, plan) in built {
                tables.plans.insert(key, plan);
            }
        }
        let mut entries = Vec::with_capacity(formats.len());
        for format in formats {
            let prototype = tables
                .plans
                .get(&format.shared_key())
                .ok_or(DspError::Capacity)?;
            entries.push((
                *format,
                prototype.for_assignment(format.assignment()?, &control)?,
            ));
        }
        drop(tables);
        entries.sort_by_key(|(format, _)| format.geometry());
        Ok(Self {
            h_rnti,
            entries: entries.into_boxed_slice(),
            retained_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::count;
    use umts_dsp::hsdpa::Cqi;

    fn formats(category: Category) -> Vec<PlanFormat> {
        (1..=30)
            .step_by(3)
            .map(|cqi| {
                let reference = Cqi::new(cqi).unwrap().reference(category).unwrap();
                PlanFormat::new(
                    reference.assignment(1, 0, false).unwrap(),
                    reference.transport_bits,
                    count(28_800),
                )
            })
            .collect()
    }

    fn output(plan: &mut TransmissionPlan) -> Vec<umts_dsp::bits::Bit> {
        let coded: Vec<_> = (0..plan.coded_bits())
            .map(|index| umts_dsp::bits::Bit::from((index * 7 + index / 3) % 5 < 2))
            .collect();
        let mut output = vec![umts_dsp::bits::Bit::Zero; plan.physical_bits()];
        plan.prepare(&coded, &mut output).unwrap();
        output
    }

    #[test]
    fn shared_banks_match_independent_banks_and_build_each_table_once() {
        let category = Category::new(10).unwrap();
        let formats = formats(category);
        let remaining = Arc::new(AtomicUsize::new(100_000_000));
        let shared = SharedPlanTables::new(Arc::clone(&remaining));
        let mut first = PlanBank::with_shared(
            &formats,
            category,
            7,
            count(64),
            count(100_000),
            count(1_000_000),
            &shared,
        )
        .unwrap();
        let built = shared.retained_bytes();
        let tables = shared.tables();
        assert!(built > 0 && tables == formats.len());
        assert_eq!(remaining.load(Ordering::Relaxed), 100_000_000 - built);
        assert!(first.retained_bytes() < built / 10);
        let mut second = PlanBank::with_shared(
            &formats,
            category,
            9,
            count(64),
            count(100_000),
            count(1_000_000),
            &shared,
        )
        .unwrap();
        assert_eq!(shared.retained_bytes(), built);
        assert_eq!(shared.tables(), tables);
        assert_eq!(remaining.load(Ordering::Relaxed), 100_000_000 - built);
        let mut independent = PlanBank::new(
            &formats,
            category,
            7,
            count(64),
            count(100_000),
            count(400_000_000),
        )
        .unwrap();
        for format in &formats {
            let reference = independent.get_mut(*format).unwrap();
            let expected = output(reference);
            let expected_control = *reference.control_bits();
            let one = first.get_mut(*format).unwrap();
            assert_eq!(output(one), expected);
            assert_eq!(*one.control_bits(), expected_control);
            assert_eq!(one.h_rnti(), 7);
            let two = second.get_mut(*format).unwrap();
            assert_eq!(output(two), expected);
            assert_eq!(two.h_rnti(), 9);
            assert_ne!(*two.control_bits(), expected_control);
            assert!(
                first
                    .plan(*format)
                    .unwrap()
                    .shares_sources(second.plan(*format).unwrap())
            );
        }
    }

    #[test]
    fn shared_tables_that_do_not_fit_the_cell_budget_change_nothing() {
        let category = Category::new(10).unwrap();
        let formats = formats(category);
        let remaining = Arc::new(AtomicUsize::new(1_000));
        let shared = SharedPlanTables::new(Arc::clone(&remaining));
        assert_eq!(
            PlanBank::with_shared(
                &formats,
                category,
                7,
                count(64),
                count(100_000),
                count(1_000_000),
                &shared,
            )
            .err(),
            Some(DspError::Capacity)
        );
        assert_eq!(remaining.load(Ordering::Relaxed), 1_000);
        assert_eq!((shared.tables(), shared.retained_bytes()), (0, 0));
    }

    #[test]
    fn a_category_that_cannot_use_a_shared_format_is_refused() {
        let formats = formats(Category::new(10).unwrap());
        let shared = SharedPlanTables::new(Arc::new(AtomicUsize::new(100_000_000)));
        assert!(
            PlanBank::with_shared(
                &formats,
                Category::new(6).unwrap(),
                7,
                count(64),
                count(100_000),
                count(1_000_000),
                &shared,
            )
            .is_err()
        );
        assert_eq!(shared.tables(), 0);
    }

    #[test]
    fn retained_payload_uses_unmapped_index_only_for_changed_geometry() {
        let category = Category::new(10).unwrap();
        let reference = Cqi::new(1).unwrap().reference(category).unwrap();
        let original = PlanFormat::new(
            reference.assignment(1, 0, false).unwrap(),
            reference.transport_bits,
            count(28_800),
        );
        let changed = PlanFormat::new(
            ScchAssignment::new(
                1,
                5,
                Modulation::Qam16,
                63,
                0,
                TransmissionVersion::new(0).unwrap(),
                false,
            )
            .unwrap(),
            reference.transport_bits,
            count(28_800),
        );
        let mut bank = PlanBank::new(
            &[changed, original],
            category,
            7,
            count(2),
            count(100_000),
            count(1_000_000),
        )
        .unwrap();
        let matching: Vec<_> = bank
            .matching_formats(
                reference.transport_bits,
                count(28_800),
                5,
                Modulation::Qam16,
            )
            .collect();
        assert_eq!(matching, [changed]);
        assert_eq!(
            bank.get_mut(changed).unwrap().transport_bits(),
            reference.transport_bits
        );
        assert_eq!(
            bank.get_mut(changed).unwrap().physical_bits(),
            5 * Modulation::Qam16.bits_per_code()
        );
        assert_eq!(
            bank.get_mut(original)
                .unwrap()
                .assignment()
                .transport_size_index(),
            original.size_index
        );
        let invalid = PlanFormat {
            size_index: 63,
            ..original
        };
        assert!(
            PlanBank::new(
                &[invalid],
                category,
                7,
                count(1),
                count(100_000),
                count(1_000_000)
            )
            .is_err()
        );
    }

    #[test]
    fn code_placement_reuses_coding_storage_and_refreshes_control() {
        let category = Category::new(10).unwrap();
        let reference = Cqi::new(6).unwrap().reference(category).unwrap();
        let first = PlanFormat::new(
            reference.assignment(1, 0, false).unwrap(),
            reference.transport_bits,
            count(28_800),
        );
        let moved = PlanFormat {
            first_code: 2,
            ..first
        };
        let mut bank = PlanBank::new(
            &[first, moved],
            category,
            7,
            count(2),
            count(100_000),
            count(1_000_000),
        )
        .unwrap();
        assert_eq!(bank.entries.len(), 1);
        assert_eq!(
            bank.matching_formats(
                first.transport_bits,
                first.soft_bits,
                first.codes,
                first.modulation
            )
            .count(),
            1
        );
        let original = *bank.get_mut(first).unwrap().control_bits();
        let plan = bank.get_mut(moved).unwrap();
        assert_eq!(plan.assignment().first_code(), 2);
        assert_ne!(*plan.control_bits(), original);
        assert_eq!(
            *plan.control_bits(),
            ScchEncoder::new(7)
                .unwrap()
                .encode(moved.assignment().unwrap())
                .unwrap()
        );
        assert_eq!(*bank.get_mut(first).unwrap().control_bits(), original);
        assert!(
            bank.get_mut(PlanFormat {
                first_code: u8::MAX,
                ..first
            })
            .is_none()
        );
        assert_eq!(*bank.get_mut(first).unwrap().control_bits(), original);
    }

    #[test]
    fn deduplication_preserves_first_placement_and_selection_order() {
        let category = Category::new(10).unwrap();
        let reference = Cqi::new(6).unwrap().reference(category).unwrap();
        let base = PlanFormat::new(
            reference.assignment(1, 0, false).unwrap(),
            reference.transport_bits,
            count(28_800),
        );
        let formats = [5, 0, 1].map(|version| PlanFormat {
            version: TransmissionVersion::new(version).unwrap(),
            ..base
        });
        let moved = |format: PlanFormat| PlanFormat {
            first_code: 2,
            ..format
        };
        let bank = PlanBank::new(
            &[
                formats[0],
                moved(formats[0]),
                formats[1],
                formats[2],
                moved(formats[1]),
            ],
            category,
            7,
            count(5),
            count(100_000),
            count(1_000_000),
        )
        .unwrap();
        assert_eq!(bank.formats().collect::<Vec<_>>(), formats);
        let invalid = PlanFormat {
            first_code: u8::MAX,
            ..base
        };
        assert!(
            PlanBank::new(
                &[base, invalid],
                category,
                7,
                count(2),
                count(100_000),
                count(1_000_000),
            )
            .is_err()
        );
    }

    #[test]
    fn process_selection_does_not_change_cached_format_identity() {
        let category = Category::new(10).unwrap();
        let reference = Cqi::new(6).unwrap().reference(category).unwrap();
        let format = PlanFormat::new(
            reference.assignment(1, 0, false).unwrap(),
            reference.transport_bits,
            count(28_800),
        );
        let mut bank = PlanBank::new(
            &[format, format],
            category,
            1,
            count(2),
            count(100_000),
            count(1_000_000),
        )
        .unwrap();
        assert_eq!(bank.entries.len(), 1);
        bank.get_mut(format)
            .unwrap()
            .select_process(3, true)
            .unwrap();
        let plan = bank.get_mut(format).unwrap();
        assert_eq!(plan.assignment().harq_process(), 3);
        assert!(plan.assignment().new_data());
        assert_eq!(plan.transport_bits(), reference.transport_bits);
        let other = PlanFormat {
            soft_bits: count(14_400),
            ..format
        };
        assert!(bank.get_mut(other).is_none());
        assert!(
            PlanBank::new(
                &[format, other],
                category,
                1,
                count(1),
                count(100_000),
                count(1_000_000),
            )
            .is_err()
        );
        assert!(
            PlanBank::new(
                &[format],
                category,
                1,
                count(1),
                count(100_000),
                count(bank.retained_bytes() - 1),
            )
            .is_err()
        );
    }
}
