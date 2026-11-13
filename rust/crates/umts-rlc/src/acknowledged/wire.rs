// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    AMD_HEADER_BYTES, AmError, DATA_FLAG, HFN_BITS, LiWidth, MAX_HFN, MAX_LIS, MAX_PDU_BYTES,
    POLL_FLAG, SN_MASK,
};
use umts_types::bits::{Bits, write_u32};

const HE_DATA: u16 = 0;
pub(super) const HE_LI: u16 = 1;
const HE_COMPLETE: u16 = 2;
pub(super) const HE_MASK: u16 = 3;
const SN_SHIFT: usize = 3;
const CONTROL_SHIFT: usize = 4;
const RESET_TYPE: u8 = 1;
const RESET_ACK_TYPE: u8 = 2;
const RSN_FLAG: u8 = 8;
const RESET_RESERVED: u8 = 7;
const RESET_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Indicator {
    End(u16),
    PreviousExact,
    PreviousOneShort,
    Padding,
    Status,
}
impl Indicator {
    pub(super) fn word(self, width: LiWidth) -> u16 {
        match self {
            Self::End(end) => end,
            Self::PreviousExact => 0,
            Self::PreviousOneShort => width.mask() - 4,
            Self::Padding => width.mask(),
            Self::Status => width.mask() - 1,
        }
    }
}

pub struct AmdPdu<'a> {
    pub sequence: u16,
    pub poll: bool,
    pub complete: bool,
    pub one_byte_padding: bool,
    pub data: &'a [u8],
    pub piggybacked_status: Option<&'a [u8]>,
    indicators: [Indicator; MAX_LIS],
    count: usize,
}
impl AmdPdu<'_> {
    pub fn indicators(&self) -> &[Indicator] {
        &self.indicators[..self.count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AmdHeader {
    pub sequence: u16,
    pub poll: bool,
    pub complete: bool,
    pub indicators: bool,
}

// TS 25.322 9.2.1.4 packs D/C, 12-bit SN, P and two HE bits into the first 16 bits.
pub fn parse_amd_header(input: &[u8], accept_special_he: bool) -> Result<AmdHeader, AmError> {
    if input.len() <= AMD_HEADER_BYTES || input.len() > MAX_PDU_BYTES {
        return Err(AmError::Length);
    }
    let header = u16::from_be_bytes([input[0], input[1]]);
    if header & DATA_FLAG == 0 {
        return Err(AmError::Header);
    }
    let he = header & HE_MASK;
    if he == HE_MASK || (he == HE_COMPLETE && !accept_special_he) {
        return Err(AmError::Header);
    }
    Ok(AmdHeader {
        sequence: (header >> SN_SHIFT) & SN_MASK,
        poll: header & POLL_FLAG != 0,
        complete: he == HE_COMPLETE,
        indicators: he == HE_LI,
    })
}

pub fn parse_amd(
    input: &[u8],
    width: LiWidth,
    accept_special_he: bool,
) -> Result<AmdPdu<'_>, AmError> {
    let header = parse_amd_header(input, accept_special_he)?;
    let mut cursor = AMD_HEADER_BYTES;
    let mut next = header.indicators;
    let mut indicators = [Indicator::Padding; MAX_LIS];
    let mut count = 0;
    while next {
        if count == MAX_LIS {
            return Err(AmError::Capacity);
        }
        let bytes = input
            .get(cursor..cursor + width.octets())
            .ok_or(AmError::Header)?;
        let word = if width == LiWidth::Seven {
            u16::from(bytes[0])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        };
        next = word & 1 != 0;
        let value = word >> 1;
        indicators[count] = match value {
            0 => Indicator::PreviousExact,
            value if value == width.mask() => Indicator::Padding,
            value if value == width.mask() - 1 => Indicator::Status,
            value if width == LiWidth::Fifteen && value == width.mask() - 4 => {
                Indicator::PreviousOneShort
            }
            value if value >= width.mask() - if width == LiWidth::Seven { 3 } else { 5 } => {
                return Err(AmError::Header);
            }
            value => Indicator::End(value),
        };
        cursor += bytes.len();
        count += 1;
    }
    let mut end = 0;
    let mut data_end = input.len() - cursor;
    let mut status = None;
    for (position, &indicator) in indicators[..count].iter().enumerate() {
        match indicator {
            Indicator::End(value) => {
                if usize::from(value) <= end || usize::from(value) > data_end {
                    return Err(AmError::Header);
                }
                end = usize::from(value);
            }
            Indicator::PreviousExact | Indicator::PreviousOneShort if position != 0 => {
                return Err(AmError::Header);
            }
            Indicator::Padding | Indicator::Status => {
                if position + 1 != count {
                    return Err(AmError::Header);
                }
                if indicator == Indicator::Status {
                    let bytes = &input[cursor + end..];
                    super::parse_status(bytes)?;
                    status = Some(bytes);
                }
                data_end = end;
            }
            _ => {}
        }
    }
    let one_byte_padding = width == LiWidth::Fifteen
        && end + 1 == data_end
        && matches!(indicators[..count].last(), Some(Indicator::End(_)));
    if one_byte_padding {
        data_end = end;
    }
    Ok(AmdPdu {
        sequence: header.sequence,
        poll: header.poll,
        complete: header.complete,
        one_byte_padding,
        data: &input[cursor..cursor + data_end],
        piggybacked_status: status,
        indicators,
        count,
    })
}

pub(super) fn data_header(output: &mut [u8], sequence: u16, poll: bool, indicators: bool) {
    let header = DATA_FLAG
        | (sequence << SN_SHIFT)
        | if poll { POLL_FLAG } else { 0 }
        | if indicators { HE_LI } else { HE_DATA };
    output[..AMD_HEADER_BYTES].copy_from_slice(&header.to_be_bytes());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reset {
    pub acknowledge: bool,
    pub sequence: bool,
    pub hfn: u32,
}
pub fn parse_reset(input: &[u8]) -> Result<Reset, AmError> {
    if input.len() < RESET_BYTES || input.len() > MAX_PDU_BYTES {
        return Err(AmError::Length);
    }
    if input[0] & RESET_RESERVED != 0 {
        return Err(AmError::Header);
    }
    let kind = input[0] >> CONTROL_SHIFT;
    if kind != RESET_TYPE && kind != RESET_ACK_TYPE {
        return Err(AmError::Header);
    }
    let bits = Bits::new(input, input.len() * 8).map_err(|_| AmError::Length)?;
    Ok(Reset {
        acknowledge: kind == RESET_ACK_TYPE,
        sequence: input[0] & RSN_FLAG != 0,
        hfn: bits.read_u32(8, HFN_BITS).map_err(|_| AmError::Header)?,
    })
}
pub(super) fn write_reset(output: &mut [u8], reset: Reset) -> Result<(), AmError> {
    if output.len() < RESET_BYTES || reset.hfn > MAX_HFN {
        return Err(AmError::Length);
    }
    output.fill(0);
    output[0] = (if reset.acknowledge {
        RESET_ACK_TYPE
    } else {
        RESET_TYPE
    } << CONTROL_SHIFT)
        | if reset.sequence { RSN_FLAG } else { 0 };
    write_u32(output, 8, HFN_BITS, reset.hfn).map_err(|_| AmError::Length)
}
