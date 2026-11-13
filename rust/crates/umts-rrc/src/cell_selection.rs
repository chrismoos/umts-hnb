// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    EncodedInformation,
    bits::BitWriter,
    system_information::{SystemInformationError, checked},
};
use umts_rrc_codec::SibType;

const CELL_ID_BITS: usize = 28;
const QUALITY_BITS: usize = 5;
const RX_LEVEL_BITS: usize = 6;
const HYSTERESIS_BITS: usize = 5;
const RESELECTION_BITS: usize = 5;
const POWER_BITS: usize = 7;
const SEARCH_QUALITY_BITS: usize = 5;
const SEARCH_LEVEL_BITS: usize = 7;
const BARRED_TIMER_BITS: usize = 3;
const MIN_QUALITY_DB: i16 = -24;
const MIN_RX_LEVEL_DBM: i16 = -115;
const MAX_RX_LEVEL_DBM: i16 = -25;
const MAX_HYSTERESIS_DB: u8 = 40;
const MIN_POWER_DBM: i16 = -50;
const MAX_POWER_DBM: i16 = 33;
const MAX_RESELECTION_SECONDS: u8 = 31;
const MIN_SEARCH_QUALITY_DB: i16 = -32;
const MAX_SEARCH_QUALITY_DB: i16 = 20;
const MIN_SEARCH_LEVEL_DB: i16 = -105;
const MAX_SEARCH_LEVEL_DB: i16 = 91;
pub const ACCESS_CLASS_COUNT: usize = 16;

#[derive(Clone, Copy, Debug)]
pub enum SelectionMeasure {
    Rscp,
    EcNo { hysteresis2_db: Option<u8> },
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum BarredDuration {
    Seconds10,
    Seconds20,
    Seconds40,
    Seconds80,
    Seconds160,
    Seconds320,
    Seconds640,
    Seconds1280,
}

#[derive(Clone, Copy, Debug)]
pub enum CellBarred {
    NotBarred,
    Barred {
        intra_frequency_reselection_allowed: bool,
        duration: BarredDuration,
    },
}

pub struct CellAccess {
    pub barred: CellBarred,
    pub reserved_for_operator: bool,
    pub reservation_extension: bool,
    pub barred_classes: Option<[bool; ACCESS_CLASS_COUNT]>,
}

pub struct CellSelection {
    pub measure: SelectionMeasure,
    pub q_qual_min_db: i16,
    pub q_rxlev_min_dbm: i16,
    pub hysteresis1_db: u8,
    pub reselection_seconds: u8,
    pub max_uplink_power_dbm: i16,
    pub intra_search_db: Option<i16>,
    pub inter_search_db: Option<i16>,
    pub hcs_search_db: Option<i16>,
}

pub struct Sib3 {
    pub cell_identity: u32,
    pub sib4_available: bool,
    pub selection: CellSelection,
    pub access: CellAccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellSelectionError {
    CellIdentity,
    Quality,
    RxLevel,
    Hysteresis,
    Reselection,
    Power,
    Search,
    Encoding(SystemInformationError),
}

fn hysteresis(value: u8) -> Result<u64, CellSelectionError> {
    if value > MAX_HYSTERESIS_DB || !value.is_multiple_of(2) {
        return Err(CellSelectionError::Hysteresis);
    }
    Ok(u64::from(value / 2))
}

impl Sib3 {
    // TS 25.331 SysInfoType3 and CellSelectReselectInfoSIB-3-4, base FDD fields.
    pub fn encode(&self) -> Result<EncodedInformation, CellSelectionError> {
        if self.cell_identity >= 1 << CELL_ID_BITS {
            return Err(CellSelectionError::CellIdentity);
        }
        let selection = &self.selection;
        if !(MIN_QUALITY_DB..=0).contains(&selection.q_qual_min_db) {
            return Err(CellSelectionError::Quality);
        }
        // Q-RxlevMin physical value is 2*IE+1. Reject non-representable even dBm.
        if !(MIN_RX_LEVEL_DBM..=MAX_RX_LEVEL_DBM).contains(&selection.q_rxlev_min_dbm)
            || (selection.q_rxlev_min_dbm - MIN_RX_LEVEL_DBM) % 2 != 0
        {
            return Err(CellSelectionError::RxLevel);
        }
        let hyst1 = hysteresis(selection.hysteresis1_db)?;
        let hyst2 = match selection.measure {
            SelectionMeasure::EcNo {
                hysteresis2_db: Some(value),
            } => Some(hysteresis(value)?),
            _ => None,
        };
        if selection.reselection_seconds > MAX_RESELECTION_SECONDS {
            return Err(CellSelectionError::Reselection);
        }
        if !(MIN_POWER_DBM..=MAX_POWER_DBM).contains(&selection.max_uplink_power_dbm) {
            return Err(CellSelectionError::Power);
        }
        for threshold in [selection.intra_search_db, selection.inter_search_db]
            .into_iter()
            .flatten()
        {
            if !(MIN_SEARCH_QUALITY_DB..=MAX_SEARCH_QUALITY_DB).contains(&threshold)
                || (threshold - MIN_SEARCH_QUALITY_DB) % 2 != 0
            {
                return Err(CellSelectionError::Search);
            }
        }
        if selection.hcs_search_db.is_some_and(|value| {
            !(MIN_SEARCH_LEVEL_DB..=MAX_SEARCH_LEVEL_DB).contains(&value)
                || (value - MIN_SEARCH_LEVEL_DB) % 2 != 0
        }) {
            return Err(CellSelectionError::Search);
        }
        let mut output = BitWriter::default();
        output.flag(false);
        output.flag(self.sib4_available);
        output.write(u64::from(self.cell_identity), CELL_ID_BITS);
        output.flag(false);
        output.flag(false);
        match selection.measure {
            SelectionMeasure::Rscp => output.flag(true),
            SelectionMeasure::EcNo { .. } => {
                output.flag(false);
                output.flag(hyst2.is_some());
                if let Some(value) = hyst2 {
                    output.write(value, HYSTERESIS_BITS);
                }
            }
        }
        output.flag(false);
        for present in [
            selection.intra_search_db.is_some(),
            selection.inter_search_db.is_some(),
            selection.hcs_search_db.is_some(),
            false,
        ] {
            output.flag(present);
        }
        for threshold in [selection.intra_search_db, selection.inter_search_db]
            .into_iter()
            .flatten()
        {
            output.write(
                ((threshold - MIN_SEARCH_QUALITY_DB) / 2) as u64,
                SEARCH_QUALITY_BITS,
            );
        }
        if let Some(value) = selection.hcs_search_db {
            output.write(
                ((value - MIN_SEARCH_LEVEL_DB) / 2) as u64,
                SEARCH_LEVEL_BITS,
            );
        }
        output.write(
            (selection.q_qual_min_db - MIN_QUALITY_DB) as u64,
            QUALITY_BITS,
        );
        output.write(
            ((selection.q_rxlev_min_dbm - MIN_RX_LEVEL_DBM) / 2) as u64,
            RX_LEVEL_BITS,
        );
        output.write(hyst1, HYSTERESIS_BITS);
        output.write(u64::from(selection.reselection_seconds), RESELECTION_BITS);
        output.write(
            (selection.max_uplink_power_dbm - MIN_POWER_DBM) as u64,
            POWER_BITS,
        );
        output.flag(self.access.barred_classes.is_some());
        match self.access.barred {
            CellBarred::NotBarred => output.flag(true),
            CellBarred::Barred {
                intra_frequency_reselection_allowed,
                duration,
            } => {
                output.flag(false);
                output.flag(!intra_frequency_reselection_allowed);
                output.write(duration as u64, BARRED_TIMER_BITS);
            }
        }
        output.flag(!self.access.reserved_for_operator);
        output.flag(!self.access.reservation_extension);
        if let Some(classes) = &self.access.barred_classes {
            for &barred in classes {
                output.flag(!barred);
            }
        }
        checked(SibType::Type3, output).map_err(CellSelectionError::Encoding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline() -> Sib3 {
        Sib3 {
            cell_identity: 0x1234567,
            sib4_available: false,
            selection: CellSelection {
                measure: SelectionMeasure::EcNo {
                    hysteresis2_db: Some(2),
                },
                q_qual_min_db: -24,
                q_rxlev_min_dbm: -115,
                hysteresis1_db: 2,
                reselection_seconds: 5,
                max_uplink_power_dbm: 24,
                intra_search_db: Some(-32),
                inter_search_db: Some(16),
                hcs_search_db: None,
            },
            access: CellAccess {
                barred: CellBarred::NotBarred,
                reserved_for_operator: false,
                reservation_extension: false,
                barred_classes: Some([false; ACCESS_CLASS_COUNT]),
            },
        }
    }

    #[test]
    fn generated_codec_validates_quality_measures_and_access_variants() {
        for measure in [
            SelectionMeasure::Rscp,
            SelectionMeasure::EcNo {
                hysteresis2_db: None,
            },
            SelectionMeasure::EcNo {
                hysteresis2_db: Some(40),
            },
        ] {
            for barred in [
                CellBarred::NotBarred,
                CellBarred::Barred {
                    intra_frequency_reselection_allowed: false,
                    duration: BarredDuration::Seconds1280,
                },
            ] {
                let mut sib = baseline();
                sib.selection.measure = measure;
                sib.selection.hcs_search_db = Some(91);
                sib.access.barred = barred;
                sib.encode().unwrap();
            }
        }
    }

    #[test]
    fn physical_units_are_checked_instead_of_silently_rounded() {
        let mut sib = baseline();
        sib.selection.q_rxlev_min_dbm = -114;
        assert!(matches!(sib.encode(), Err(CellSelectionError::RxLevel)));
        sib.selection.q_rxlev_min_dbm = -25;
        sib.selection.hysteresis1_db = 3;
        assert!(matches!(sib.encode(), Err(CellSelectionError::Hysteresis)));
        sib.selection.hysteresis1_db = 40;
        sib.selection.intra_search_db = Some(-31);
        assert!(matches!(sib.encode(), Err(CellSelectionError::Search)));
        sib.selection.intra_search_db = None;
        sib.encode().unwrap();
    }

    #[test]
    fn literal_sib3_vector_preserves_cell_identity_and_access_bit_order() {
        let encoded = baseline().encode().unwrap();
        assert_eq!(encoded.bit_length, 102);
        assert_eq!(
            encoded.bytes,
            [
                0x04, 0x8d, 0x15, 0x9c, 0x42, 0xc0, 0x60, 0x00, 0x04, 0xb2, 0xbf, 0xff, 0xfc
            ]
        );
    }
}
