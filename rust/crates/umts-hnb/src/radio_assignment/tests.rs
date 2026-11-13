// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn dual_downlink_keeps_primary_uplink_and_orders_offsets_by_carrier() {
    let above = dual_downlink_assignment(10637, 9687, 10662).unwrap();
    assert_eq!(above.center_hz, 2_129_900_000);
    assert_eq!(above.secondary_hz, 2_132_400_000);
    assert_eq!(above.primary.uplink_hz, 1_937_400_000);
    assert_eq!(above.offsets_hz, [-2_500_000, 2_500_000]);
    let below = dual_downlink_assignment(10637, 9687, 10612).unwrap();
    assert_eq!(below.center_hz, 2_124_900_000);
    assert_eq!(below.offsets_hz, [2_500_000, -2_500_000]);
    assert_eq!(below.primary, above.primary);
}

#[test]
fn dual_downlink_rejects_invalid_spacing_band_and_primary_pair() {
    for secondary in [10637, 10661, 10663, 10863, 4385] {
        assert!(dual_downlink_assignment(10637, 9687, secondary).is_err());
    }
    assert!(dual_downlink_assignment(10637, 9688, 10662).is_err());
}

#[test]
fn dual_downlink_resolves_additional_channel_raster() {
    let assignment = dual_downlink_assignment(437, 37, 462).unwrap();
    assert_eq!(assignment.primary.downlink_hz, 1_937_500_000);
    assert_eq!(assignment.secondary_hz, 1_942_500_000);
    assert_eq!(assignment.center_hz, 1_940_000_000);
}

fn paired_frequencies(downlink: u32, uplink: u32) -> Result<(u64, u64)> {
    let assignment = paired_assignment(downlink, uplink)?;
    Ok((assignment.downlink_hz, assignment.uplink_hz))
}

#[test]
fn paired_general_assignments_include_offsets_and_reversed_duplex() {
    for (dl, ul, downlink_hz, uplink_hz) in [
        (10637, 9687, 2_127_400_000, 1_937_400_000),
        (4385, 4160, 877_000_000, 832_000_000),
        (3012, 2787, 942_400_000, 897_400_000),
        (1638, 1413, 2_132_600_000, 1_732_600_000),
        (3388, 3163, 2_167_600_000, 1_767_600_000),
        (3837, 3612, 730_400_000, 700_400_000),
        (712, 312, 877_400_000, 832_400_000),
        (4017, 3792, 748_400_000, 779_400_000),
        (4512, 4287, 793_400_000, 834_400_000),
    ] {
        assert_eq!(
            paired_frequencies(dl, ul).unwrap(),
            (downlink_hz, uplink_hz)
        );
    }
}

#[test]
fn every_general_band_endpoint_and_additional_pair_is_valid() {
    for band in BANDS {
        for (dl, ul) in [
            (band.downlink.0, band.uplink.0),
            (band.downlink.1, band.uplink.1),
        ] {
            assert_eq!(
                paired_frequencies(dl, ul).unwrap(),
                (
                    general_frequency(dl, band.downlink).unwrap() as u64,
                    general_frequency(ul, band.uplink).unwrap() as u64,
                )
            );
        }
        if let Some(additional) = &band.additional {
            assert_eq!(
                additional.downlink_numbers.len(),
                additional.uplink_numbers.len()
            );
            for (&dl, &ul) in additional
                .downlink_numbers
                .iter()
                .zip(additional.uplink_numbers)
            {
                assert_eq!(
                    paired_frequencies(dl, ul).unwrap(),
                    (
                        (i64::from(dl) * CHANNEL_RASTER_HZ + additional.downlink_offset_hz) as u64,
                        (i64::from(ul) * CHANNEL_RASTER_HZ + additional.uplink_offset_hz) as u64,
                    )
                );
            }
        }
    }
    assert_eq!(
        paired_frequencies(1037, 812).unwrap(),
        (877_500_000, 832_500_000)
    );
    assert_eq!(
        paired_frequencies(787, 387).unwrap(),
        (877_500_000, 832_500_000)
    );
}

#[test]
fn unsupported_channels_and_nonpaired_assignments_are_rejected() {
    for (dl, ul) in [
        (99, 99),
        (0, 0),
        (u32::MAX, u32::MAX),
        (4385, 9687),
        (4385, 4161),
        (1008, 783),
        (10561, 9611),
    ] {
        assert!(paired_frequencies(dl, ul).is_err(), "DL={dl} UL={ul}");
    }
}

#[test]
fn resolved_band_matches_shared_carrier_and_rejects_invalid_pairing() {
    for (downlink, uplink, band) in [
        (4400, 4175, 5),
        (10637, 9687, 1),
        (3012, 2787, 8),
        (712, 312, 19),
        (4512, 4287, 20),
        (862, 462, 21),
    ] {
        assert_eq!(
            paired_assignment(downlink, uplink).unwrap().band.number(),
            band
        );
    }
    assert!(paired_assignment(4400, 4176).is_err());
}
