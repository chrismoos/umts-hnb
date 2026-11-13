// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT};
use std::num::NonZeroUsize;

mod layout;
mod tables;
pub use layout::{CodeLayout, LayoutSet, PuncturingLimit};

const SUBFRAMES_PER_FRAME: u8 = 5;
const SLOTS_PER_SUBFRAME: u64 = 3;
const MAX_ETFCI: u8 = 127;
const MAX_RSN: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportBlockTable {
    Zero,
    One,
}

impl TransportBlockTable {
    // TS 25.321 Annex B reserves gaps even below the category's maximum size.
    pub fn bits(self, interval: Tti, format: Etfci) -> Option<NonZeroUsize> {
        let table = match (interval, self) {
            (Tti::TwoMilliseconds, Self::Zero) => &tables::TTI_2_TABLE_0,
            (Tti::TwoMilliseconds, Self::One) => &tables::TTI_2_TABLE_1,
            (Tti::TenMilliseconds, Self::Zero) => &tables::TTI_10_TABLE_0,
            (Tti::TenMilliseconds, Self::One) => &tables::TTI_10_TABLE_1,
        };
        NonZeroUsize::new(usize::from(table[usize::from(format.value())]))
    }

    pub fn formats(
        self,
        interval: Tti,
        category: Rel6Category,
    ) -> impl Iterator<Item = (Etfci, NonZeroUsize)> {
        (0..=MAX_ETFCI).filter_map(move |value| {
            let format = Etfci(value);
            let bits = self.bits(interval, format)?;
            (bits.get() <= category.maximum_transport_bits(interval)?).then_some((format, bits))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Category,
    Format,
    Sequence,
    Time,
    RateMatching,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tti {
    TwoMilliseconds,
    TenMilliseconds,
}

impl Tti {
    pub fn chips(self) -> u64 {
        match self {
            Self::TwoMilliseconds => SLOTS_PER_SUBFRAME * CHIPS_PER_SLOT,
            Self::TenMilliseconds => CHIPS_PER_FRAME,
        }
    }

    // TS 25.321 11.8.1.1.1: HARQ_RTT counts TTIs, not radio frames.
    pub fn processes(self) -> u8 {
        match self {
            Self::TwoMilliseconds => 8,
            Self::TenMilliseconds => 4,
        }
    }

    pub fn number(self, cfn: u8, subframe: u8) -> Result<u16, Error> {
        match self {
            Self::TwoMilliseconds if subframe < SUBFRAMES_PER_FRAME => {
                Ok(u16::from(cfn) * u16::from(SUBFRAMES_PER_FRAME) + u16::from(subframe))
            }
            Self::TenMilliseconds if subframe == 0 => Ok(u16::from(cfn)),
            _ => Err(Error::Time),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rel6Category(u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub maximum_codes: u8,
    pub minimum_spreading_factor: u16,
    pub maximum_ten_ms_bits: usize,
    pub maximum_two_ms_bits: Option<usize>,
}

impl Rel6Category {
    pub fn new(value: u8) -> Result<Self, Error> {
        if !(1..=6).contains(&value) {
            return Err(Error::Category);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    // TS 25.306 Table 5.1g. Four codes mean two SF2 and two SF4 channels.
    pub fn limits(self) -> Limits {
        let (codes, spreading, ten, two) = match self.0 {
            1 => (1, 4, 7110, None),
            2 => (2, 4, 14484, Some(2798)),
            3 => (2, 4, 14484, None),
            4 => (2, 2, 20000, Some(5772)),
            5 => (2, 2, 20000, None),
            _ => (4, 2, 20000, Some(11484)),
        };
        Limits {
            maximum_codes: codes,
            minimum_spreading_factor: spreading,
            maximum_ten_ms_bits: ten,
            maximum_two_ms_bits: two,
        }
    }

    pub fn maximum_transport_bits(self, tti: Tti) -> Option<usize> {
        let limits = self.limits();
        match tti {
            Tti::TwoMilliseconds => limits.maximum_two_ms_bits,
            Tti::TenMilliseconds => Some(limits.maximum_ten_ms_bits),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Etfci(u8);

impl Etfci {
    pub fn new(value: u8) -> Result<Self, Error> {
        if value > MAX_ETFCI {
            return Err(Error::Format);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsn(u8);

impl Rsn {
    pub fn new(value: u8) -> Result<Self, Error> {
        if value > MAX_RSN {
            return Err(Error::Sequence);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    // TS 25.212 Table 16, unless higher layers signal RV0-only operation.
    pub fn redundancy_version(
        self,
        systematic_bits: usize,
        physical_bits: usize,
        tti: Tti,
        tti_number: u16,
        rv_zero_only: bool,
    ) -> Result<u8, Error> {
        if systematic_bits == 0 || physical_bits == 0 {
            return Err(Error::RateMatching);
        }
        if rv_zero_only {
            return Ok(0);
        }
        let low_rate = systematic_bits <= (physical_bits - 1) / 2;
        let cycle = tti_number / u16::from(tti.processes());
        Ok(match (self.0, low_rate) {
            (0, _) => 0,
            (1, true) | (2, false) => 2,
            (1, false) => 3,
            (2, true) => 0,
            (_, true) => ((cycle % 2) * 2) as u8,
            (_, false) => (cycle % 4) as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_tables_preserve_reserved_indices_and_category_limits() {
        let bits = |table: TransportBlockTable, tti, index| {
            table
                .bits(tti, Etfci::new(index).unwrap())
                .map(NonZeroUsize::get)
        };
        assert_eq!(
            bits(TransportBlockTable::Zero, Tti::TwoMilliseconds, 120),
            None
        );
        assert_eq!(
            bits(TransportBlockTable::Zero, Tti::TwoMilliseconds, 127),
            Some(11484)
        );
        assert_eq!(
            bits(TransportBlockTable::One, Tti::TwoMilliseconds, 115),
            None
        );
        assert_eq!(
            bits(TransportBlockTable::One, Tti::TwoMilliseconds, 125),
            Some(11478)
        );
        assert_eq!(
            bits(TransportBlockTable::One, Tti::TwoMilliseconds, 126),
            None
        );
        assert_eq!(
            bits(TransportBlockTable::Zero, Tti::TenMilliseconds, 127),
            Some(20000)
        );
        assert_eq!(
            bits(TransportBlockTable::One, Tti::TenMilliseconds, 120),
            Some(19950)
        );
        assert_eq!(
            bits(TransportBlockTable::One, Tti::TenMilliseconds, 121),
            None
        );
        for table in [TransportBlockTable::Zero, TransportBlockTable::One] {
            for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
                assert_eq!(bits(table, interval, 0), Some(18));
                for value in 1..=6 {
                    let category = Rel6Category::new(value).unwrap();
                    let formats: Vec<_> = table.formats(interval, category).collect();
                    assert_eq!(
                        formats.is_empty(),
                        category.maximum_transport_bits(interval).is_none()
                    );
                    assert!(formats.windows(2).all(|pair| pair[0].1 < pair[1].1));
                    assert!(formats.iter().all(|(_, bits)| bits.get()
                        <= category.maximum_transport_bits(interval).unwrap()));
                }
            }
        }
    }

    #[test]
    fn categories_and_tti_limits_follow_the_original_edch_table() {
        let expected = [None, Some(2798), None, Some(5772), None, Some(11484)];
        for (index, bits) in expected.into_iter().enumerate() {
            let category = Rel6Category::new(index as u8 + 1).unwrap();
            assert_eq!(category.maximum_transport_bits(Tti::TwoMilliseconds), bits);
        }
        assert!(Rel6Category::new(0).is_err());
        assert!(Rel6Category::new(7).is_err());
        assert_eq!(Tti::TwoMilliseconds.number(255, 4), Ok(1279));
        assert_eq!(Tti::TenMilliseconds.number(255, 0), Ok(255));
        assert!(Tti::TwoMilliseconds.number(0, 5).is_err());
        assert!(Tti::TenMilliseconds.number(0, 1).is_err());
    }

    #[test]
    fn redundancy_uses_exact_half_rate_boundary_and_process_cycle() {
        for (systematic, expected) in [(49, [0, 2, 0]), (50, [0, 3, 2])] {
            for (rsn, rv) in expected.into_iter().enumerate() {
                assert_eq!(
                    Rsn::new(rsn as u8).unwrap().redundancy_version(
                        systematic,
                        100,
                        Tti::TwoMilliseconds,
                        0,
                        false
                    ),
                    Ok(rv)
                );
            }
        }
        let rsn = Rsn::new(3).unwrap();
        for cycle in 0..8 {
            assert_eq!(
                rsn.redundancy_version(49, 100, Tti::TwoMilliseconds, cycle * 8, false),
                Ok(((cycle % 2) * 2) as u8)
            );
            assert_eq!(
                rsn.redundancy_version(50, 100, Tti::TenMilliseconds, cycle * 4, false),
                Ok((cycle % 4) as u8)
            );
        }
        assert_eq!(
            rsn.redundancy_version(50, 100, Tti::TwoMilliseconds, 24, true),
            Ok(0)
        );
        assert!(
            rsn.redundancy_version(1, 0, Tti::TwoMilliseconds, 0, false)
                .is_err()
        );
    }
}
