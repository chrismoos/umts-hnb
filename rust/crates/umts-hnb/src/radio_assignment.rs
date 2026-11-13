// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Result, bail};
use umts_rrc::common_channels::FddBand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FddAssignment {
    pub band: FddBand,
    pub downlink_hz: u64,
    pub uplink_hz: u64,
}

const CHANNEL_RASTER_HZ: i64 = 200_000;
const HZ_PER_MHZ: i64 = 1_000_000;

struct Band {
    band: FddBand,
    downlink: (u32, u32, i64),
    uplink: (u32, u32, i64),
    additional: Option<Additional>,
}

struct Additional {
    downlink_offset_hz: i64,
    uplink_offset_hz: i64,
    downlink_numbers: &'static [u32],
    uplink_numbers: &'static [u32],
}

// TS 25.101 v9.5.0 Tables 5.1, 5.1A and 5.2 define these paired FDD assignments.
const BANDS: &[Band] = &[
    Band {
        band: FddBand::I,
        downlink: (10562, 10838, 0),
        uplink: (9612, 9888, 0),
        additional: None,
    },
    Band {
        band: FddBand::II,
        downlink: (9662, 9938, 0),
        uplink: (9262, 9538, 0),
        additional: Some(Additional {
            downlink_offset_hz: 1_850_100_000,
            uplink_offset_hz: 1_850_100_000,
            downlink_numbers: &[412, 437, 462, 487, 512, 537, 562, 587, 612, 637, 662, 687],
            uplink_numbers: &[12, 37, 62, 87, 112, 137, 162, 187, 212, 237, 262, 287],
        }),
    },
    Band {
        band: FddBand::III,
        downlink: (1162, 1513, 1575),
        uplink: (937, 1288, 1525),
        additional: None,
    },
    Band {
        band: FddBand::IV,
        downlink: (1537, 1738, 1805),
        uplink: (1312, 1513, 1450),
        additional: Some(Additional {
            downlink_offset_hz: 1_735_100_000,
            uplink_offset_hz: 1_380_100_000,
            downlink_numbers: &[1887, 1912, 1937, 1962, 1987, 2012, 2037, 2062, 2087],
            uplink_numbers: &[1662, 1687, 1712, 1737, 1762, 1787, 1812, 1837, 1862],
        }),
    },
    Band {
        band: FddBand::V,
        downlink: (4357, 4458, 0),
        uplink: (4132, 4233, 0),
        additional: Some(Additional {
            downlink_offset_hz: 670_100_000,
            uplink_offset_hz: 670_100_000,
            downlink_numbers: &[1007, 1012, 1032, 1037, 1062, 1087],
            uplink_numbers: &[782, 787, 807, 812, 837, 862],
        }),
    },
    Band {
        band: FddBand::VI,
        downlink: (4387, 4413, 0),
        uplink: (4162, 4188, 0),
        additional: Some(Additional {
            downlink_offset_hz: 670_100_000,
            uplink_offset_hz: 670_100_000,
            downlink_numbers: &[1037, 1062],
            uplink_numbers: &[812, 837],
        }),
    },
    Band {
        band: FddBand::VII,
        downlink: (2237, 2563, 2175),
        uplink: (2012, 2338, 2100),
        additional: Some(Additional {
            downlink_offset_hz: 2_105_100_000,
            uplink_offset_hz: 2_030_100_000,
            downlink_numbers: &[
                2587, 2612, 2637, 2662, 2687, 2712, 2737, 2762, 2787, 2812, 2837, 2862, 2887, 2912,
            ],
            uplink_numbers: &[
                2362, 2387, 2412, 2437, 2462, 2487, 2512, 2537, 2562, 2587, 2612, 2637, 2662, 2687,
            ],
        }),
    },
    Band {
        band: FddBand::VIII,
        downlink: (2937, 3088, 340),
        uplink: (2712, 2863, 340),
        additional: None,
    },
    Band {
        band: FddBand::IX,
        downlink: (9237, 9387, 0),
        uplink: (8762, 8912, 0),
        additional: None,
    },
    Band {
        band: FddBand::X,
        downlink: (3112, 3388, 1490),
        uplink: (2887, 3163, 1135),
        additional: Some(Additional {
            downlink_offset_hz: 1_430_100_000,
            uplink_offset_hz: 1_075_100_000,
            downlink_numbers: &[
                3412, 3437, 3462, 3487, 3512, 3537, 3562, 3587, 3612, 3637, 3662, 3687,
            ],
            uplink_numbers: &[
                3187, 3212, 3237, 3262, 3287, 3312, 3337, 3362, 3387, 3412, 3437, 3462,
            ],
        }),
    },
    Band {
        band: FddBand::XI,
        downlink: (3712, 3787, 736),
        uplink: (3487, 3562, 733),
        additional: None,
    },
    Band {
        band: FddBand::XII,
        downlink: (3837, 3903, -37),
        uplink: (3612, 3678, -22),
        additional: Some(Additional {
            downlink_offset_hz: -54_900_000,
            uplink_offset_hz: -39_900_000,
            downlink_numbers: &[3927, 3932, 3957, 3962, 3987, 3992],
            uplink_numbers: &[3702, 3707, 3732, 3737, 3762, 3767],
        }),
    },
    Band {
        band: FddBand::XIII,
        downlink: (4017, 4043, -55),
        uplink: (3792, 3818, 21),
        additional: Some(Additional {
            downlink_offset_hz: -64_900_000,
            uplink_offset_hz: 11_100_000,
            downlink_numbers: &[4067, 4092],
            uplink_numbers: &[3842, 3867],
        }),
    },
    Band {
        band: FddBand::XIV,
        downlink: (4117, 4143, -63),
        uplink: (3892, 3918, 12),
        additional: Some(Additional {
            downlink_offset_hz: -72_900_000,
            uplink_offset_hz: 2_100_000,
            downlink_numbers: &[4167, 4192],
            uplink_numbers: &[3942, 3967],
        }),
    },
    Band {
        band: FddBand::XIX,
        downlink: (712, 763, 735),
        uplink: (312, 363, 770),
        additional: Some(Additional {
            downlink_offset_hz: 720_100_000,
            uplink_offset_hz: 755_100_000,
            downlink_numbers: &[787, 812, 837],
            uplink_numbers: &[387, 412, 437],
        }),
    },
    Band {
        band: FddBand::XX,
        downlink: (4512, 4638, -109),
        uplink: (4287, 4413, -23),
        additional: None,
    },
    Band {
        band: FddBand::XXI,
        downlink: (862, 912, 1326),
        uplink: (462, 512, 1358),
        additional: None,
    },
];

fn general_frequency(number: u32, range: (u32, u32, i64)) -> Option<i64> {
    (range.0..=range.1)
        .contains(&number)
        .then(|| i64::from(number) * CHANNEL_RASTER_HZ + range.2 * HZ_PER_MHZ)
}

pub(crate) fn paired_assignment(downlink: u32, uplink: u32) -> Result<FddAssignment> {
    for band in BANDS {
        let additional = band.additional.as_ref();
        let dl = general_frequency(downlink, band.downlink).or_else(|| {
            additional
                .filter(|a| a.downlink_numbers.contains(&downlink))
                .map(|a| i64::from(downlink) * CHANNEL_RASTER_HZ + a.downlink_offset_hz)
        });
        let ul = general_frequency(uplink, band.uplink).or_else(|| {
            additional
                .filter(|a| a.uplink_numbers.contains(&uplink))
                .map(|a| i64::from(uplink) * CHANNEL_RASTER_HZ + a.uplink_offset_hz)
        });
        if let (Some(dl), Some(ul)) = (dl, ul) {
            let separation = general_frequency(band.downlink.0, band.downlink).unwrap()
                - general_frequency(band.uplink.0, band.uplink).unwrap();
            if dl - ul == separation {
                return Ok(FddAssignment {
                    band: band.band,
                    downlink_hz: u64::try_from(dl)?,
                    uplink_hz: u64::try_from(ul)?,
                });
            }
        }
    }
    bail!("UARFCN DL {downlink}/UL {uplink} is not a supported fixed-duplex paired FDD assignment")
}

const ADJACENT_CARRIER_SPACING_HZ: u64 = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DualDownlinkAssignment {
    pub primary: FddAssignment,
    pub secondary_hz: u64,
    pub center_hz: u64,
    pub offsets_hz: [i64; 2],
}

pub(crate) fn dual_downlink_assignment(
    downlink: u32,
    uplink: u32,
    secondary: u32,
) -> Result<DualDownlinkAssignment> {
    let primary = paired_assignment(downlink, uplink)?;
    let secondary_hz = secondary_downlink_frequency(primary, secondary)?;
    let center_hz = primary.downlink_hz.min(secondary_hz) + ADJACENT_CARRIER_SPACING_HZ / 2;
    let center = i64::try_from(center_hz)?;
    Ok(DualDownlinkAssignment {
        primary,
        secondary_hz,
        center_hz,
        offsets_hz: [
            i64::try_from(primary.downlink_hz)? - center,
            i64::try_from(secondary_hz)? - center,
        ],
    })
}

pub(crate) fn secondary_downlink_frequency(
    primary: FddAssignment,
    secondary_uarfcn: u32,
) -> Result<u64> {
    for band in BANDS.iter().filter(|band| band.band == primary.band) {
        let frequency = general_frequency(secondary_uarfcn, band.downlink).or_else(|| {
            band.additional
                .as_ref()
                .filter(|additional| additional.downlink_numbers.contains(&secondary_uarfcn))
                .map(|additional| {
                    i64::from(secondary_uarfcn) * CHANNEL_RASTER_HZ + additional.downlink_offset_hz
                })
        });
        if let Some(frequency) = frequency {
            let frequency = u64::try_from(frequency)?;
            if primary.downlink_hz.abs_diff(frequency) == ADJACENT_CARRIER_SPACING_HZ {
                return Ok(frequency);
            }
        }
    }
    bail!(
        "secondary downlink UARFCN {secondary_uarfcn} must be in the primary FDD band and exactly 5 MHz from the primary downlink"
    )
}

#[cfg(test)]
mod tests;
