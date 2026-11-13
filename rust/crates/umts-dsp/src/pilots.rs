// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex64;

// TS 25.211 Tables 3, 4 and 8, with the first transmitted bit in the most significant used position.
const UL_FOUR: [u8; 15] = [15, 9, 11, 9, 13, 15, 15, 13, 11, 15, 11, 13, 13, 9, 9];
const UL_SIX: [u8; 15] = [62, 38, 45, 36, 53, 62, 60, 52, 46, 63, 45, 55, 52, 39, 39];
const UL_EIGHT: [u8; 15] = [
    0xfe, 0xae, 0xbb, 0xaa, 0xeb, 0xfe, 0xfa, 0xea, 0xbe, 0xff, 0xbb, 0xef, 0xea, 0xaf, 0xaf,
];
// TS 25.211 Table 12, antenna one without transmit diversity.
const DL_TWO: [u16; 15] = [3, 0, 1, 0, 2, 3, 3, 2, 1, 3, 1, 2, 2, 0, 0];
const DL_EIGHT: [u16; 15] = [
    0xfe, 0xce, 0xdd, 0xcc, 0xed, 0xfe, 0xfc, 0xec, 0xde, 0xff, 0xdd, 0xef, 0xec, 0xcf, 0xcf,
];
const DL_SIXTEEN: [u16; 15] = [
    0xfefe, 0xcefc, 0xddec, 0xccde, 0xedff, 0xfedd, 0xfcef, 0xecec, 0xdecf, 0xffcf, 0xddfe, 0xefce,
    0xecdd, 0xcfcc, 0xcfed,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UplinkPilots {
    Three,
    Four,
    Five,
    Six,
    Seven,
    Eight,
}
impl UplinkPilots {
    pub fn bits(self) -> usize {
        match self {
            Self::Three => 3,
            Self::Four => 4,
            Self::Five => 5,
            Self::Six => 6,
            Self::Seven => 7,
            Self::Eight => 8,
        }
    }
    pub fn word(self, slot: u8) -> Option<u8> {
        let slot = usize::from(slot);
        if slot >= UL_FOUR.len() {
            return None;
        }
        Some(match self {
            Self::Three => UL_FOUR[slot] & 0b111,
            Self::Four => UL_FOUR[slot],
            Self::Five => UL_SIX[slot] & 0b11111,
            Self::Six => UL_SIX[slot],
            Self::Seven => (UL_SIX[slot] << 1) | 1,
            Self::Eight => UL_EIGHT[slot],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownlinkPilots {
    Two,
    Four,
    Eight,
    Sixteen,
}
impl DownlinkPilots {
    pub fn bits(self) -> usize {
        match self {
            Self::Two => 2,
            Self::Four => 4,
            Self::Eight => 8,
            Self::Sixteen => 16,
        }
    }
    pub fn word(self, slot: u8) -> Option<u16> {
        let slot = usize::from(slot);
        if slot >= DL_TWO.len() {
            return None;
        }
        Some(match self {
            Self::Two => DL_TWO[slot],
            Self::Four => 0b1100 | DL_TWO[slot],
            Self::Eight => DL_EIGHT[slot],
            Self::Sixteen => DL_SIXTEEN[slot],
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PilotEstimate {
    // Effective complex channel includes the UE's control-channel gain beta_c.
    pub channel: Complex64,
    pub observed_power: f64,
    // Mean squared despread-pilot residual, not a calibrated ISCP or SIR estimate.
    pub residual_power: f64,
}

#[cfg(test)]
mod tests;
