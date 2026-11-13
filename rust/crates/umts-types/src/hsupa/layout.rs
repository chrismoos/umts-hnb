// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Error, Rel6Category, Tti};
use std::num::NonZeroUsize;

const LAYOUTS: [&[u16]; 10] = [
    &[256],
    &[128],
    &[64],
    &[32],
    &[16],
    &[8],
    &[4],
    &[4, 4],
    &[2, 2],
    &[2, 2, 4, 4],
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeLayout(u8);

impl CodeLayout {
    pub fn new(spreading_factors: &[u16]) -> Result<Self, Error> {
        LAYOUTS
            .iter()
            .position(|sfs| *sfs == spreading_factors)
            .map(|index| Self(index as u8))
            .ok_or(Error::Format)
    }

    pub fn spreading_factors(self) -> &'static [u16] {
        LAYOUTS[usize::from(self.0)]
    }

    pub fn up_to(self) -> impl Iterator<Item = Self> {
        (0..=self.0).map(Self)
    }

    pub fn physical_bits(self, interval: Tti) -> usize {
        self.spreading_factors()
            .iter()
            .map(|sf| (interval.chips() / u64::from(*sf)) as usize)
            .sum()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PuncturingLimit {
    numerator: u16,
    denominator: u16,
}

impl PuncturingLimit {
    pub fn percent(self) -> u8 {
        self.numerator as u8
    }

    // TS 25.331 E-DCH MAC-d flow configuration: 0.44..1.0 in steps of 0.04.
    pub fn new(percent: u8) -> Result<Self, Error> {
        if !(44..=100).contains(&percent) || !(percent - 44).is_multiple_of(4) {
            return Err(Error::RateMatching);
        }
        Ok(Self {
            numerator: u16::from(percent),
            denominator: 100,
        })
    }

    fn permits(self, physical: usize, coded: usize) -> bool {
        (physical as u128) * u128::from(self.denominator)
            >= (coded as u128) * u128::from(self.numerator)
    }
}

pub struct LayoutSet {
    mask: u16,
    interval: Tti,
    non_maximum: PuncturingLimit,
}

impl LayoutSet {
    pub fn interval(&self) -> Tti {
        self.interval
    }

    pub fn new(
        interval: Tti,
        category: Rel6Category,
        allowed: &[CodeLayout],
        non_maximum: PuncturingLimit,
    ) -> Result<Self, Error> {
        if category.maximum_transport_bits(interval).is_none() {
            return Err(Error::Category);
        }
        let limits = category.limits();
        let mut mask = 0;
        for layout in allowed {
            let sfs = layout.spreading_factors();
            if sfs.len() > usize::from(limits.maximum_codes)
                || sfs.iter().any(|sf| *sf < limits.minimum_spreading_factor)
            {
                return Err(Error::Category);
            }
            mask |= 1 << layout.0;
        }
        if mask == 0 {
            return Err(Error::Format);
        }
        Ok(Self {
            mask,
            interval,
            non_maximum,
        })
    }

    fn layouts(&self) -> impl DoubleEndedIterator<Item = CodeLayout> + '_ {
        (0..LAYOUTS.len() as u8)
            .filter(|index| self.mask & (1 << index) != 0)
            .map(CodeLayout)
    }

    // TS 25.212 4.8.4.1, BPSK and uncompressed TTIs only.
    pub fn select(&self, coded_bits: NonZeroUsize) -> Result<CodeLayout, Error> {
        let coded = coded_bits.get();
        if let Some(first) = self
            .layouts()
            .find(|layout| layout.physical_bits(self.interval) >= coded)
            && first.spreading_factors().len() == 1
        {
            return Ok(first);
        }
        let mut eligible = self.layouts().filter(|layout| {
            self.non_maximum
                .permits(layout.physical_bits(self.interval), coded)
        });
        if let Some(mut selected) = eligible.next() {
            for follower in eligible {
                if follower.spreading_factors().len() != 1 {
                    break;
                }
                selected = follower;
            }
            return Ok(selected);
        }
        let maximum = self.layouts().next_back().ok_or(Error::Format)?;
        let numerator = if maximum.spreading_factors().len() == 4 {
            33
        } else {
            44
        };
        let limit = PuncturingLimit {
            numerator,
            denominator: 100,
        };
        if limit.permits(maximum.physical_bits(self.interval), coded) {
            Ok(maximum)
        } else {
            Err(Error::RateMatching)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_obeys_single_code_preference_and_maximum_puncturing() {
        let allowed: Vec<_> = LAYOUTS
            .iter()
            .map(|sfs| CodeLayout::new(sfs).unwrap())
            .collect();
        let set = LayoutSet::new(
            Tti::TwoMilliseconds,
            Rel6Category::new(6).unwrap(),
            &allowed,
            PuncturingLimit::new(52).unwrap(),
        )
        .unwrap();
        let select = |bits| {
            set.select(NonZeroUsize::new(bits).unwrap())
                .map(CodeLayout::spreading_factors)
        };
        assert_eq!(select(30), Ok(&[256][..]));
        assert_eq!(select(31), Ok(&[128][..]));
        assert_eq!(select(2000), Ok(&[4][..]));
        assert_eq!(select(8000), Ok(&[2, 2][..]));
        assert_eq!(select(30000), Ok(&[2, 2, 4, 4][..]));
        assert_eq!(select(40000), Err(Error::RateMatching));
        assert!(
            LayoutSet::new(
                Tti::TwoMilliseconds,
                Rel6Category::new(1).unwrap(),
                &allowed,
                PuncturingLimit::new(52).unwrap()
            )
            .is_err()
        );
    }
}
