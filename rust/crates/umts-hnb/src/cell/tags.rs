// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Result, ensure};

const MAX_MIB: u8 = 8;
const MAX_PLMN: u16 = 256;
const MAX_CELL: u8 = 4;
pub(super) const REFERENCE_MIB_VALUE_TAG: u8 = 3;
const REFERENCE_SIB1_VALUE_TAG: u16 = 1;
const REFERENCE_SIB3_VALUE_TAG: u8 = 1;
const REFERENCE_SIB5_VALUE_TAG: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BroadcastTags {
    mib: u8,
    sib1: u16,
    sib3: u8,
    sib5: u8,
}

impl BroadcastTags {
    pub(crate) fn reference(mib: u8) -> Result<Self> {
        Self::new(
            mib,
            REFERENCE_SIB1_VALUE_TAG,
            REFERENCE_SIB3_VALUE_TAG,
            REFERENCE_SIB5_VALUE_TAG,
        )
    }

    pub(crate) fn new(mib: u8, sib1: u16, sib3: u8, sib5: u8) -> Result<Self> {
        ensure!((1..=MAX_MIB).contains(&mib), "MIB value tag must be 1..8");
        ensure!(
            (1..=MAX_PLMN).contains(&sib1),
            "SIB1 value tag must be 1..256"
        );
        ensure!(
            (1..=MAX_CELL).contains(&sib3) && (1..=MAX_CELL).contains(&sib5),
            "cell value tags must be 1..4"
        );
        Ok(Self {
            mib,
            sib1,
            sib3,
            sib5,
        })
    }

    pub(crate) fn initial() -> Self {
        Self::reference(REFERENCE_MIB_VALUE_TAG)
            .expect("valid offline reference tags")
            .next()
    }

    // TS 25.331 8.1.1.6 scopes cached content to its value tag, with six-hour validity.
    pub(crate) fn next(self) -> Self {
        Self {
            mib: self.mib % MAX_MIB + 1,
            sib1: self.sib1 % MAX_PLMN + 1,
            sib3: self.sib3 % MAX_CELL + 1,
            sib5: self.sib5 % MAX_CELL + 1,
        }
    }

    pub(crate) fn mib(self) -> u8 {
        self.mib
    }
    pub(crate) fn sib1(self) -> u16 {
        self.sib1
    }
    pub(crate) fn sib3(self) -> u16 {
        u16::from(self.sib3)
    }
    pub(crate) fn sib5(self) -> u16 {
        u16::from(self.sib5)
    }

    pub(crate) fn sib11(self) -> u16 {
        // The persisted full broadcast transcript advances every tag together, including SIB11.
        u16::from(self.sib3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_wrap_in_their_own_wire_ranges() {
        let mut tags = BroadcastTags::new(MAX_MIB, MAX_PLMN, MAX_CELL, MAX_CELL)
            .unwrap()
            .next();
        assert_eq!(tags, BroadcastTags::new(1, 1, 1, 1).unwrap());
        for _ in 0..MAX_PLMN {
            assert_eq!(tags.sib11(), tags.sib3());
            tags = tags.next();
        }
        assert_eq!(tags, BroadcastTags::new(1, 1, 1, 1).unwrap());
        for (mib, sib1, sib3, sib5) in [
            (0, 1, 1, 1),
            (9, 1, 1, 1),
            (1, 0, 1, 1),
            (1, 257, 1, 1),
            (1, 1, 0, 1),
            (1, 1, 1, 5),
        ] {
            assert!(BroadcastTags::new(mib, sib1, sib3, sib5).is_err());
        }
    }
}
