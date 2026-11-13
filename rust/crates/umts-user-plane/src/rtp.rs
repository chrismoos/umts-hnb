// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::WireError;

pub const HEADER_BYTES: usize = 12;
pub const IU_CLOCK_HZ: u32 = 16_000;
const VERSION: u8 = 0x80;
const MARKER: u8 = 0x80;
const PAYLOAD_TYPE_MASK: u8 = 0x7f;
const DYNAMIC_PAYLOAD_START: u8 = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: &'a [u8],
}
pub fn decode(input: &[u8]) -> Result<Packet<'_>, WireError> {
    if input.len() < HEADER_BYTES {
        return Err(WireError::Length);
    }
    // TS 25.414 5.1.3.3.1 disallows padding, extensions and CSRCs on the Iu-CS RTP profile.
    if input[0] != VERSION {
        return Err(WireError::Header);
    }
    Ok(Packet {
        marker: input[1] & MARKER != 0,
        payload_type: input[1] & PAYLOAD_TYPE_MASK,
        sequence: u16::from_be_bytes(input[2..4].try_into().expect("sequence")),
        timestamp: u32::from_be_bytes(input[4..8].try_into().expect("timestamp")),
        ssrc: u32::from_be_bytes(input[8..HEADER_BYTES].try_into().expect("SSRC")),
        payload: &input[HEADER_BYTES..],
    })
}
pub fn encode(packet: Packet<'_>, output: &mut [u8]) -> Result<usize, WireError> {
    if !(DYNAMIC_PAYLOAD_START..=PAYLOAD_TYPE_MASK).contains(&packet.payload_type) {
        return Err(WireError::Header);
    }
    let size = HEADER_BYTES
        .checked_add(packet.payload.len())
        .ok_or(WireError::Capacity)?;
    if size > output.len() {
        return Err(WireError::Capacity);
    }
    output[0] = VERSION;
    output[1] = packet.payload_type | if packet.marker { MARKER } else { 0 };
    output[2..4].copy_from_slice(&packet.sequence.to_be_bytes());
    output[4..8].copy_from_slice(&packet.timestamp.to_be_bytes());
    output[8..HEADER_BYTES].copy_from_slice(&packet.ssrc.to_be_bytes());
    output[HEADER_BYTES..size].copy_from_slice(packet.payload);
    Ok(size)
}

#[cfg(test)]
mod tests;
