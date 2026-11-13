// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn uplink_table_three_and_four_literals_preserve_left_to_right_bit_order() {
    let six = [
        "111110", "100110", "101101", "100100", "110101", "111110", "111100", "110100", "101110",
        "111111", "101101", "110111", "110100", "100111", "100111",
    ];
    let four = [
        "1111", "1001", "1011", "1001", "1101", "1111", "1111", "1101", "1011", "1111", "1011",
        "1101", "1101", "1001", "1001",
    ];
    let eight = [
        "11111110", "10101110", "10111011", "10101010", "11101011", "11111110", "11111010",
        "11101010", "10111110", "11111111", "10111011", "11101111", "11101010", "10101111",
        "10101111",
    ];
    for slot in 0..15 {
        for (kind, expected) in [
            (UplinkPilots::Three, &four[slot][1..]),
            (UplinkPilots::Four, four[slot]),
            (UplinkPilots::Five, &six[slot][1..]),
            (UplinkPilots::Six, six[slot]),
            (UplinkPilots::Eight, eight[slot]),
        ] {
            assert_eq!(
                format!(
                    "{:0width$b}",
                    kind.word(slot as u8).unwrap(),
                    width = kind.bits()
                ),
                expected
            );
        }
        assert_eq!(
            format!("{:07b}", UplinkPilots::Seven.word(slot as u8).unwrap()),
            format!("{}1", six[slot])
        );
    }
    assert!(UplinkPilots::Six.word(15).is_none());
}

#[test]
fn downlink_table_twelve_literals_are_not_the_uplink_pilots() {
    let pairs = [
        "11", "00", "01", "00", "10", "11", "11", "10", "01", "11", "01", "10", "10", "00", "00",
    ];
    let eight = [
        "11111110", "11001110", "11011101", "11001100", "11101101", "11111110", "11111100",
        "11101100", "11011110", "11111111", "11011101", "11101111", "11101100", "11001111",
        "11001111",
    ];
    let sixteen = [
        "1111111011111110",
        "1100111011111100",
        "1101110111101100",
        "1100110011011110",
        "1110110111111111",
        "1111111011011101",
        "1111110011101111",
        "1110110011101100",
        "1101111011001111",
        "1111111111001111",
        "1101110111111110",
        "1110111111001110",
        "1110110011011101",
        "1100111111001100",
        "1100111111101101",
    ];
    for slot in 0..15 {
        assert_eq!(
            format!("{:02b}", DownlinkPilots::Two.word(slot as u8).unwrap()),
            pairs[slot]
        );
        assert_eq!(
            format!("{:04b}", DownlinkPilots::Four.word(slot as u8).unwrap()),
            format!("11{}", pairs[slot])
        );
        assert_eq!(
            format!("{:08b}", DownlinkPilots::Eight.word(slot as u8).unwrap()),
            eight[slot]
        );
        assert_eq!(
            format!("{:016b}", DownlinkPilots::Sixteen.word(slot as u8).unwrap()),
            sixteen[slot]
        );
    }
    assert_ne!(
        DownlinkPilots::Eight.word(1).unwrap(),
        u16::from(UplinkPilots::Eight.word(1).unwrap())
    );
    assert!(DownlinkPilots::Sixteen.word(15).is_none());
}
