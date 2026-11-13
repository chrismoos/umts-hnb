// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::WireError;
use std::num::NonZeroU32;

pub const HEADER_BYTES: usize = 8;
pub const OPTIONAL_BYTES: usize = 4;
const VERSION_PROTOCOL: u8 = 0x30;
const VERSION_PROTOCOL_MASK: u8 = 0xf0;
const EXTENSION: u8 = 4;
const SEQUENCE: u8 = 2;
const N_PDU: u8 = 1;
const OPTIONAL_FLAGS: u8 = EXTENSION | SEQUENCE | N_PDU;
const MAX_EXTENSIONS: usize = 16;
const REQUIRED_COMPREHENSION: u8 = 0x80;
const ECHO_REQUEST: u8 = 1;
const ECHO_RESPONSE: u8 = 2;
const G_PDU: u8 = 255;
const VERSION_NOT_SUPPORTED: u8 = 3;
const ERROR_INDICATION: u8 = 26;
const EXTENSION_HEADER_TYPE_LIST_IE: u8 = 141;
const SUPPORTED_EXTENSIONS: u8 = 31;
const RECOVERY_IE: u8 = 14;
const TEID_DATA_IE: u8 = 16;
const GSN_ADDRESS_IE: u8 = 133;
const TEID_DATA_IE_BYTES: usize = 5;
const GSN_ADDRESS_HEADER_BYTES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Teid(NonZeroU32);
impl Teid {
    pub fn new(value: u32) -> Result<Self, WireError> {
        NonZeroU32::new(value).map(Self).ok_or(WireError::Header)
    }
    pub fn get(self) -> u32 {
        self.0.get()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerTeid(u32);
impl PeerTeid {
    // TS 29.281 5.1 forbids local zero allocation but accepts a signaled peer zero.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
    pub const fn get(self) -> u32 {
        self.0
    }
}
impl From<Teid> for PeerTeid {
    fn from(value: Teid) -> Self {
        Self(value.get())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageType {
    Data,
    EchoRequest,
    EchoResponse,
    ErrorIndication,
    SupportedExtensions,
    VersionNotSupported,
    Unknown(u8),
}
impl MessageType {
    fn from_wire(value: u8) -> Self {
        match value {
            G_PDU => Self::Data,
            ECHO_REQUEST => Self::EchoRequest,
            ECHO_RESPONSE => Self::EchoResponse,
            ERROR_INDICATION => Self::ErrorIndication,
            SUPPORTED_EXTENSIONS => Self::SupportedExtensions,
            VERSION_NOT_SUPPORTED => Self::VersionNotSupported,
            value => Self::Unknown(value),
        }
    }
    fn wire(self) -> u8 {
        match self {
            Self::Data => G_PDU,
            Self::EchoRequest => ECHO_REQUEST,
            Self::EchoResponse => ECHO_RESPONSE,
            Self::ErrorIndication => ERROR_INDICATION,
            Self::SupportedExtensions => SUPPORTED_EXTENSIONS,
            Self::VersionNotSupported => VERSION_NOT_SUPPORTED,
            Self::Unknown(value) => value,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    pub kind: MessageType,
    pub teid: Option<PeerTeid>,
    pub sequence: Option<u16>,
    pub n_pdu: Option<u8>,
    pub payload: &'a [u8],
}

pub fn decode(input: &[u8]) -> Result<Packet<'_>, WireError> {
    if input.len() < HEADER_BYTES {
        return Err(WireError::Length);
    }
    // TS 29.060 6 requires receivers to ignore the spare bit and unused optional fields.
    // TS 29.060 6: version 1 in bits 8-6, protocol type 1 (GTP) in bit 5.
    if input[0] >> 5 != VERSION_PROTOCOL >> 5 {
        return Err(WireError::Version(input[0] >> 5));
    }
    if input[0] & VERSION_PROTOCOL_MASK != VERSION_PROTOCOL {
        return Err(WireError::Header);
    }
    let length = usize::from(u16::from_be_bytes([input[2], input[3]]));
    if length != input.len() - HEADER_BYTES {
        return Err(WireError::Length);
    }
    let kind = MessageType::from_wire(input[1]);
    let raw_teid = u32::from_be_bytes(input[4..HEADER_BYTES].try_into().expect("fixed TEID"));
    let teid = if kind == MessageType::Data || raw_teid != 0 {
        Some(PeerTeid::new(raw_teid))
    } else {
        None
    };
    let mut cursor = HEADER_BYTES;
    let mut sequence = None;
    let mut n_pdu = None;
    if input[0] & OPTIONAL_FLAGS != 0 {
        let optional = input
            .get(cursor..cursor + OPTIONAL_BYTES)
            .ok_or(WireError::Length)?;
        cursor += OPTIONAL_BYTES;
        if input[0] & SEQUENCE != 0 {
            sequence = Some(u16::from_be_bytes([optional[0], optional[1]]));
        }
        if input[0] & N_PDU != 0 {
            n_pdu = Some(optional[2]);
        }
        if input[0] & EXTENSION != 0 {
            let mut next = optional[3];
            let mut extensions = 0;
            while next != 0 {
                if extensions == MAX_EXTENSIONS {
                    return Err(WireError::Capacity);
                }
                extensions += 1;
                let size =
                    usize::from(*input.get(cursor).ok_or(WireError::Length)?) * OPTIONAL_BYTES;
                if size == 0 {
                    return Err(WireError::Length);
                }
                let header = input
                    .get(cursor..cursor.checked_add(size).ok_or(WireError::Length)?)
                    .ok_or(WireError::Length)?;
                if next & REQUIRED_COMPREHENSION != 0 {
                    return Err(WireError::RequiredExtension(next));
                }
                next = header[size - 1];
                cursor += size;
            }
        }
    }
    if matches!(kind, MessageType::Data) {
        if teid.is_none() {
            return Err(WireError::Header);
        }
    } else if matches!(
        kind,
        MessageType::EchoRequest
            | MessageType::EchoResponse
            | MessageType::ErrorIndication
            | MessageType::SupportedExtensions
    ) && (teid.is_some() || sequence.is_none())
    {
        return Err(WireError::Header);
    }
    Ok(Packet {
        kind,
        teid,
        sequence,
        n_pdu,
        payload: &input[cursor..],
    })
}

pub fn encode(packet: Packet<'_>, output: &mut [u8]) -> Result<usize, WireError> {
    let optional = packet.sequence.is_some() || packet.n_pdu.is_some();
    let header = HEADER_BYTES + if optional { OPTIONAL_BYTES } else { 0 };
    let size = header
        .checked_add(packet.payload.len())
        .ok_or(WireError::Capacity)?;
    let length = u16::try_from(size - HEADER_BYTES).map_err(|_| WireError::Length)?;
    if output.len() < size {
        return Err(WireError::Capacity);
    }
    if packet.kind == MessageType::Data && packet.teid.is_none() {
        return Err(WireError::Header);
    }
    if matches!(
        packet.kind,
        MessageType::EchoRequest
            | MessageType::EchoResponse
            | MessageType::ErrorIndication
            | MessageType::SupportedExtensions
            | MessageType::VersionNotSupported
    ) && (packet.teid.is_some() || packet.sequence.is_none())
    {
        return Err(WireError::Header);
    }
    output[..header].fill(0);
    output[0] = VERSION_PROTOCOL
        | if packet.sequence.is_some() {
            SEQUENCE
        } else {
            0
        }
        | if packet.n_pdu.is_some() { N_PDU } else { 0 };
    output[1] = packet.kind.wire();
    output[2..4].copy_from_slice(&length.to_be_bytes());
    output[4..HEADER_BYTES].copy_from_slice(&packet.teid.map_or(0, PeerTeid::get).to_be_bytes());
    if optional {
        output[HEADER_BYTES..HEADER_BYTES + 2]
            .copy_from_slice(&packet.sequence.unwrap_or(0).to_be_bytes());
        output[HEADER_BYTES + 2] = packet.n_pdu.unwrap_or(0);
    }
    output[header..size].copy_from_slice(packet.payload);
    Ok(size)
}

pub fn echo_response(sequence: u16, output: &mut [u8]) -> Result<usize, WireError> {
    // TS 29.060 7.2.2 requires the GTP-U recovery counter to be transmitted as zero.
    encode(
        Packet {
            kind: MessageType::EchoResponse,
            teid: None,
            sequence: Some(sequence),
            n_pdu: None,
            payload: &[RECOVERY_IE, 0],
        },
        output,
    )
}

/// TS 29.060 7.3.7: Error Indication for a G-PDU that matched no tunnel. `teid` is the TEID
/// of that G-PDU and `local` the address it was sent to. The header TEID is zero (9.3.1).
pub fn error_indication(
    teid: u32,
    local: std::net::IpAddr,
    sequence: u16,
    output: &mut [u8],
) -> Result<usize, WireError> {
    let mut payload = [0_u8; TEID_DATA_IE_BYTES + GSN_ADDRESS_HEADER_BYTES + 16];
    payload[0] = TEID_DATA_IE;
    payload[1..5].copy_from_slice(&teid.to_be_bytes());
    payload[5] = GSN_ADDRESS_IE;
    let address_len = match local {
        std::net::IpAddr::V4(address) => {
            payload[8..12].copy_from_slice(&address.octets());
            4
        }
        std::net::IpAddr::V6(address) => {
            payload[8..24].copy_from_slice(&address.octets());
            16
        }
    };
    payload[6..8].copy_from_slice(&(address_len as u16).to_be_bytes());
    encode(
        Packet {
            kind: MessageType::ErrorIndication,
            teid: None,
            sequence: Some(sequence),
            n_pdu: None,
            payload: &payload[..TEID_DATA_IE_BYTES + GSN_ADDRESS_HEADER_BYTES + address_len],
        },
        output,
    )
}

/// TS 29.060 7.2.3 and 11.1.1: Version Not Supported includes only the header, whose version
/// field names the latest version this end supports (GTP version 1).
pub fn version_not_supported(sequence: u16, output: &mut [u8]) -> Result<usize, WireError> {
    encode(
        Packet {
            kind: MessageType::VersionNotSupported,
            teid: None,
            sequence: Some(sequence),
            n_pdu: None,
            payload: &[],
        },
        output,
    )
}

/// TS 29.060 7.2.4 and 6: sent when a comprehension-required extension header is not
/// understood. `supported` lists the extension header types this end interprets.
pub fn supported_extension_headers(
    supported: &[u8],
    sequence: u16,
    output: &mut [u8],
) -> Result<usize, WireError> {
    let mut payload = [0_u8; 2 + 255];
    let count = supported.len().min(255);
    payload[0] = EXTENSION_HEADER_TYPE_LIST_IE;
    payload[1] = count as u8;
    payload[2..2 + count].copy_from_slice(&supported[..count]);
    encode(
        Packet {
            kind: MessageType::SupportedExtensions,
            teid: None,
            sequence: Some(sequence),
            n_pdu: None,
            payload: &payload[..2 + count],
        },
        output,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorIndication {
    pub teid: u32,
    pub address: std::net::IpAddr,
}

/// The mandatory IEs of a received Error Indication (TS 29.060 7.3.7, 7.7.13, 7.7.32).
pub fn parse_error_indication(payload: &[u8]) -> Result<ErrorIndication, WireError> {
    let mut teid = None;
    let mut address = None;
    let mut cursor = 0;
    while cursor < payload.len() {
        match payload[cursor] {
            TEID_DATA_IE => {
                let bytes = payload
                    .get(cursor + 1..cursor + TEID_DATA_IE_BYTES)
                    .ok_or(WireError::Length)?;
                teid = Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
                cursor += TEID_DATA_IE_BYTES;
            }
            kind if kind & 0x80 != 0 => {
                let header = payload
                    .get(cursor + 1..cursor + GSN_ADDRESS_HEADER_BYTES)
                    .ok_or(WireError::Length)?;
                let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
                let value = payload
                    .get(
                        cursor + GSN_ADDRESS_HEADER_BYTES
                            ..cursor + GSN_ADDRESS_HEADER_BYTES + length,
                    )
                    .ok_or(WireError::Length)?;
                if kind == GSN_ADDRESS_IE {
                    address = Some(match value {
                        [a, b, c, d] => std::net::IpAddr::from([*a, *b, *c, *d]),
                        v if v.len() == 16 => {
                            let mut octets = [0_u8; 16];
                            octets.copy_from_slice(v);
                            std::net::IpAddr::from(octets)
                        }
                        _ => return Err(WireError::Length),
                    });
                }
                cursor += GSN_ADDRESS_HEADER_BYTES + length;
            }
            // TS 29.060 7.7.1: a TV information element has a fixed, type-specific length, so
            // an unknown TV type cannot be skipped.
            kind => return Err(WireError::Unsupported(kind)),
        }
    }
    Ok(ErrorIndication {
        teid: teid.ok_or(WireError::Length)?,
        address: address.ok_or(WireError::Length)?,
    })
}

#[cfg(test)]
mod tests;
