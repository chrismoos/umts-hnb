// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc_codec::decode_uplink_common;

const REQUEST: [u8; 10] = [0x20, 0x90, 1, 1, 0, 0, 0, 0, 1, 0x60];
// UPER appends v4b0 present, idle-behavior absent, v590 absent and the four-bit release index.
const RELEASE_REQUEST: [u8; 11] = [0x28, 0x90, 1, 1, 0, 0, 0, 0, 1, 0x62, 0x20];

#[test]
fn absent_indicator_means_r99_even_with_an_earlier_extension_container() {
    assert_eq!(
        access_release(&decode_uplink_common(&REQUEST, 78).unwrap()),
        Ok(AccessRelease::R99)
    );
    let mut earlier = REQUEST;
    earlier[0] |= 0x08;
    assert_eq!(
        access_release(&decode_uplink_common(&earlier, 80).unwrap()),
        Ok(AccessRelease::R99)
    );
}

#[test]
fn independent_wire_indices_cover_release_four_through_sixteen_and_reject_spares() {
    for (index, expected) in [
        AccessRelease::R4,
        AccessRelease::R5,
        AccessRelease::R6,
        AccessRelease::R7,
        AccessRelease::R8,
        AccessRelease::R9,
        AccessRelease::R10,
        AccessRelease::R11,
        AccessRelease::R12,
        AccessRelease::R13,
        AccessRelease::R14,
        AccessRelease::R15,
        AccessRelease::R16,
    ]
    .into_iter()
    .enumerate()
    {
        let mut bytes = RELEASE_REQUEST;
        bytes[10] = (index as u8) << 3;
        let message = decode_uplink_common(&bytes, 85).unwrap();
        assert_eq!(message.consumed_bits, 85);
        assert_eq!(access_release(&message), Ok(expected));
    }
    for index in 13..16 {
        let mut bytes = RELEASE_REQUEST;
        bytes[10] = index << 3;
        assert_eq!(
            access_release(&decode_uplink_common(&bytes, 85).unwrap()),
            Err(Error::UnsupportedRelease)
        );
    }
}
