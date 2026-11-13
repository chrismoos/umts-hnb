// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn iu_transport_uses_the_16khz_clock_and_ignores_received_payload_type_and_marker() {
    assert_eq!(IU_CLOCK_HZ / 50, 320);
    let packet = Packet {
        marker: true,
        payload_type: 96,
        sequence: 0x1234,
        timestamp: 320,
        ssrc: 0x10203040,
        payload: &[0xe0, 0, 0, 0],
    };
    let mut output = [0; 32];
    let size = encode(packet, &mut output).unwrap();
    assert_eq!(
        &output[..HEADER_BYTES],
        &[
            0x80, 0xe0, 0x12, 0x34, 0, 0, 1, 0x40, 0x10, 0x20, 0x30, 0x40
        ]
    );
    assert_eq!(decode(&output[..size]).unwrap(), packet);
    output[1] = 7;
    assert_eq!(decode(&output[..size]).unwrap().payload, packet.payload);
    for flags in [0x81, 0x90, 0xa0, 0x40] {
        output[0] = flags;
        assert_eq!(decode(&output[..size]), Err(WireError::Header));
    }
}
