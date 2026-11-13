// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::time::{CHIPS_PER_SLOT, ChipInstant};
use std::num::NonZeroUsize;

const SLOTS_PER_TTI: u64 = 3;
const MAXIMUM_QAM16_TRANSPORT_BITS: usize = 27952;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CarrierId {
    Primary,
    Secondary,
}

impl CarrierId {
    pub const fn index(self) -> usize {
        match self {
            Self::Primary => 0,
            Self::Secondary => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityError {
    Category,
    ProcessCount,
    SoftMemory,
    Time,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Category(u8, bool);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DualCellCategory(u8, bool);

impl DualCellCategory {
    pub fn new(value: u8) -> Result<Self, CapabilityError> {
        if !(21..=24).contains(&value) {
            return Err(CapabilityError::Category);
        }
        Ok(Self(value, value >= 23))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    pub fn with_qam64(self, enabled: bool) -> Result<Self, CapabilityError> {
        if enabled && self.0 < 23 {
            return Err(CapabilityError::Category);
        }
        Ok(Self(self.0, enabled))
    }

    pub fn per_cell_limits(self) -> Limits {
        self.per_cell_category().limits()
    }

    pub fn total_soft_bits(self) -> usize {
        self.per_cell_limits().total_soft_bits * 2
    }

    pub fn per_cell_category(self) -> Category {
        Category(self.0, self.1)
    }

    // Category 9 is conservative for category 21's 23370-bit per-cell limit.
    pub fn single_cell_fallback(self) -> Category {
        Category(
            match self.0 {
                21 => 9,
                22 => 10,
                23 => 13,
                _ => 14,
            },
            self.1,
        )
    }

    pub fn validate_process_memory(
        self,
        cells: [&[NonZeroUsize]; 2],
    ) -> Result<(), CapabilityError> {
        for memory in cells {
            if memory.is_empty() || memory.len() > 8 {
                return Err(CapabilityError::ProcessCount);
            }
            let total = memory
                .iter()
                .try_fold(0_usize, |total, value| total.checked_add(value.get()))
                .ok_or(CapabilityError::SoftMemory)?;
            if total > self.per_cell_limits().total_soft_bits {
                return Err(CapabilityError::SoftMemory);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub maximum_codes: u8,
    pub minimum_tti_interval: u8,
    pub maximum_transport_bits: usize,
    pub total_soft_bits: usize,
    pub qam16: bool,
    pub qam64: bool,
}

impl Category {
    pub fn new(value: u8) -> Result<Self, CapabilityError> {
        if !matches!(value, 1..=14 | 19..=20) {
            return Err(CapabilityError::Category);
        }
        Ok(Self(value, matches!(value, 13 | 14 | 19 | 20)))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    pub fn with_qam64(self, enabled: bool) -> Result<Self, CapabilityError> {
        if enabled && !matches!(self.0, 13 | 14 | 19 | 20 | 23 | 24) {
            return Err(CapabilityError::Category);
        }
        Ok(Self(self.0, enabled))
    }

    // TS 25.306 Table 5.1a, single-cell non-MIMO capabilities.
    pub fn limits(self) -> Limits {
        let (codes, interval, bits, soft) = match self.0 {
            1 => (5, 3, 7298, 19200),
            2 => (5, 3, 7298, 28800),
            3 => (5, 2, 7298, 28800),
            4 => (5, 2, 7298, 38400),
            5 => (5, 1, 7298, 57600),
            6 => (5, 1, 7298, 67200),
            7 => (10, 1, 14411, 115200),
            8 => (10, 1, 14411, 134400),
            9 => (15, 1, 20251, 172800),
            10 => (15, 1, 27952, 172800),
            11 => (5, 2, 3630, 14400),
            12 => (5, 1, 3630, 28800),
            13 => (15, 1, 35280, 259200),
            19 => (15, 1, 35280, 518400),
            20 => (15, 1, 42192, 518400),
            21 => (15, 1, 23370, 172800),
            22 => (15, 1, 27952, 172800),
            23 => (15, 1, 35280, 259200),
            24 => (15, 1, 42192, 259200),
            _ => (15, 1, 42192, 259200),
        };
        Limits {
            maximum_codes: codes,
            minimum_tti_interval: interval,
            maximum_transport_bits: if self.1 {
                bits
            } else {
                bits.min(MAXIMUM_QAM16_TRANSPORT_BITS)
            },
            total_soft_bits: soft,
            qam16: self.0 <= 10 || self.0 >= 13,
            qam64: self.1,
        }
    }

    pub fn validate_process_memory(self, memory: &[NonZeroUsize]) -> Result<(), CapabilityError> {
        if memory.is_empty() || memory.len() > 8 {
            return Err(CapabilityError::ProcessCount);
        }
        let total = memory
            .iter()
            .try_fold(0_usize, |total, value| total.checked_add(value.get()))
            .ok_or(CapabilityError::SoftMemory)?;
        if total > self.limits().total_soft_bits {
            return Err(CapabilityError::SoftMemory);
        }
        Ok(())
    }

    pub fn validate_spacing(
        self,
        previous: ChipInstant,
        next: ChipInstant,
    ) -> Result<(), CapabilityError> {
        let tti_chips = SLOTS_PER_TTI * CHIPS_PER_SLOT;
        let minimum = u64::from(self.limits().minimum_tti_interval) * tti_chips;
        if previous.epoch != next.epoch
            || next
                .index
                .checked_sub(previous.index)
                .is_none_or(|distance| distance < minimum || !distance.is_multiple_of(tti_chips))
        {
            return Err(CapabilityError::Time);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RadioEpoch;
    use std::num::NonZeroU64;

    #[test]
    fn dual_cell_categories_preserve_transport_limits_and_split_memory() {
        for (value, bits, total, fallback) in [
            (21, 23370, 345600, 9),
            (22, 27952, 345600, 10),
            (23, 35280, 518400, 13),
            (24, 42192, 518400, 14),
        ] {
            let category = DualCellCategory::new(value).unwrap();
            let limits = category.per_cell_limits();
            assert_eq!(category.value(), value);
            assert_eq!(limits.maximum_transport_bits, bits);
            assert_eq!(category.total_soft_bits(), total);
            assert_eq!(category.per_cell_category().limits(), limits);
            assert_eq!(limits.total_soft_bits * 2, total);
            assert_eq!(limits.qam64, value >= 23);
            assert_eq!(category.single_cell_fallback().value(), fallback);
            assert!(
                category
                    .single_cell_fallback()
                    .limits()
                    .maximum_transport_bits
                    <= bits
            );
            let memory = [NonZeroUsize::new(total / 16).unwrap(); 8];
            assert!(category.validate_process_memory([&memory, &memory]).is_ok());
            let excessive = [NonZeroUsize::new(total / 16 + 1).unwrap(); 8];
            let small = [NonZeroUsize::MIN];
            assert_eq!(
                category.validate_process_memory([&excessive, &small]),
                Err(CapabilityError::SoftMemory)
            );
            assert_eq!(
                category.validate_process_memory([&small, &excessive]),
                Err(CapabilityError::SoftMemory)
            );
        }
        for value in [0, 14, 20, 25, u8::MAX] {
            assert_eq!(DualCellCategory::new(value), Err(CapabilityError::Category));
        }
    }

    #[test]
    fn disabling_qam64_preserves_dual_category_and_soft_memory() {
        for value in [23, 24] {
            let reported = DualCellCategory::new(value).unwrap();
            let configured = reported.with_qam64(false).unwrap();
            assert_eq!(configured.value(), value);
            assert_eq!(configured.total_soft_bits(), reported.total_soft_bits());
            assert_eq!(configured.per_cell_limits().total_soft_bits, 259200);
            assert_eq!(configured.per_cell_limits().maximum_transport_bits, 27952);
            assert!(configured.per_cell_limits().qam16);
            assert!(!configured.per_cell_limits().qam64);
            assert_eq!(configured.with_qam64(true).unwrap(), reported);
            assert_eq!(configured.per_cell_category().value(), value);
        }
        assert!(DualCellCategory::new(21).unwrap().with_qam64(true).is_err());
        assert!(Category::new(10).unwrap().with_qam64(true).is_err());
    }

    #[test]
    fn original_categories_have_distinct_rate_and_memory_limits() {
        let six = Category::new(6).unwrap().limits();
        assert_eq!(
            (
                six.maximum_codes,
                six.maximum_transport_bits,
                six.total_soft_bits
            ),
            (5, 7298, 67200)
        );
        let ten = Category::new(10).unwrap().limits();
        assert_eq!(
            (
                ten.maximum_codes,
                ten.maximum_transport_bits,
                ten.total_soft_bits
            ),
            (15, 27952, 172800)
        );
        assert!(!Category::new(12).unwrap().limits().qam16);
        assert!(Category::new(15).is_err());
    }

    #[test]
    fn all_processes_share_the_capability_memory_budget() {
        let category = Category::new(1).unwrap();
        assert!(
            category
                .validate_process_memory(&[NonZeroUsize::new(3200).unwrap(); 6])
                .is_ok()
        );
        assert_eq!(
            category.validate_process_memory(&[NonZeroUsize::new(3201).unwrap(); 6]),
            Err(CapabilityError::SoftMemory)
        );
        assert!(category.validate_process_memory(&[]).is_err());
    }

    #[test]
    fn categories_nineteen_and_twenty_preserve_larger_single_cell_soft_buffers() {
        for (value, bits) in [(19, 35280), (20, 42192)] {
            let category = Category::new(value).unwrap();
            let limits = category.limits();
            assert_eq!(limits.maximum_transport_bits, bits);
            assert_eq!(limits.total_soft_bits, 518400);
            assert!(limits.qam64);
            assert!(
                category
                    .validate_process_memory(&[NonZeroUsize::new(86400).unwrap(); 6])
                    .is_ok()
            );
        }
        assert!(
            Category::new(14)
                .unwrap()
                .validate_process_memory(&[NonZeroUsize::new(86400).unwrap(); 6])
                .is_err()
        );
    }

    #[test]
    fn category_spacing_is_checked_on_the_same_epoch_and_tti_grid() {
        let epoch = RadioEpoch::new(NonZeroU64::new(1).unwrap());
        let at = |index| ChipInstant { epoch, index };
        let category = Category::new(1).unwrap();
        assert!(
            category
                .validate_spacing(at(512), at(512 + 3 * 7680))
                .is_ok()
        );
        assert!(
            category
                .validate_spacing(at(512), at(512 + 2 * 7680))
                .is_err()
        );
        assert!(
            category
                .validate_spacing(at(512), at(513 + 3 * 7680))
                .is_err()
        );
    }
}
