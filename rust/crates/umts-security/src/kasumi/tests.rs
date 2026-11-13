// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn ts35203_independent_blocks_match_and_key_schedules_remain_private() {
    for (key, input, expected) in [
        (
            [
                0x2b, 0xd6, 0x45, 0x9f, 0x82, 0xc5, 0xb3, 0, 0x95, 0x2c, 0x49, 0x10, 0x48, 0x81,
                0xff, 0x48,
            ],
            0xea024714ad5c4d84,
            0xdf1f9b251c0bf45f,
        ),
        (
            [
                0x8c, 0xe3, 0x3e, 0x2c, 0xc3, 0xc0, 0xb5, 0xfc, 0x1f, 0x3d, 0xe8, 0xa6, 0xdc, 0x66,
                0xb1, 0xf3,
            ],
            0xd3c5d592327fb11c,
            0xde551988ceb2f9b7,
        ),
    ] {
        let cipher = Kasumi::new(&Key::new(key), 0);
        assert_eq!(cipher.encrypt(input), expected);
        let other = Kasumi::new(&Key::new([0; 16]), 0);
        assert_ne!(other.encrypt(input), expected);
        assert_eq!(cipher.encrypt(input), expected);
    }
}

#[test]
fn ts35203_iterated_vector_exercises_every_s_box_entry() {
    let key = Key::new([
        0x3a, 0x3b, 0x39, 0xb5, 0xc3, 0xf2, 0x37, 0x6d, 0x69, 0xf7, 0xd5, 0x46, 0xe5, 0xf8, 0x5d,
        0x43,
    ]);
    let cipher = Kasumi::new(&key, 0);
    let mut block = 0xca49c1c75771ab0b;
    for _ in 0..50 {
        block = cipher.encrypt(block);
    }
    assert_eq!(block, 0x738bad4c4a690802);
    assert_eq!(S7[38], 58);
    assert_eq!(S9[138], 339);
}
