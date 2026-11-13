// SPDX-License-Identifier: AGPL-3.0-or-later

use super::Error;
use crate::{algorithms::MAX_F8_BITS, contexts::KeyOrigin, counters::CipherMode};

const OCTET_BITS: usize = u8::BITS as usize;
const AM_HEADER_OCTETS: usize = 2;
const UM_HEADER_OCTETS: usize = 1;
const AM_DATA_FLAG: u16 = 0x8000;
const AM_SN_SHIFT: u32 = 3;
const AM_SN_MASK: u16 = 0x0fff;
const UM_SN_SHIFT: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header {
    Control,
    Data {
        sequence: Option<u16>,
        clear_octets: usize,
    },
}

pub fn header(mode: CipherMode, bytes: &[u8], bits: usize) -> Result<Header, Error> {
    if bits == 0 || bytes.len() != bits.div_ceil(OCTET_BITS) {
        return Err(Error::Length);
    }
    if mode == CipherMode::Transparent {
        if bits > MAX_F8_BITS {
            return Err(Error::Length);
        }
        return Ok(Header::Data {
            sequence: None,
            clear_octets: 0,
        });
    }
    if !bits.is_multiple_of(OCTET_BITS) {
        return Err(Error::Length);
    }
    let (sequence, clear_octets) = match mode {
        CipherMode::Acknowledged => {
            if bytes[0] & (AM_DATA_FLAG >> OCTET_BITS) as u8 == 0 {
                return Ok(Header::Control);
            }
            if bytes.len() <= AM_HEADER_OCTETS {
                return Err(Error::Length);
            }
            let word = u16::from_be_bytes([bytes[0], bytes[1]]);
            ((word >> AM_SN_SHIFT) & AM_SN_MASK, AM_HEADER_OCTETS)
        }
        CipherMode::Unacknowledged => {
            if bytes.len() <= UM_HEADER_OCTETS {
                return Err(Error::Length);
            }
            (u16::from(bytes[0] >> UM_SN_SHIFT), UM_HEADER_OCTETS)
        }
        CipherMode::Transparent => return Err(Error::Mode),
    };
    if bits - clear_octets * OCTET_BITS > MAX_F8_BITS {
        return Err(Error::Length);
    }
    // TS 25.322 9.7.8 excludes only the first AM two octets or UM one octet, not the LI chain.
    Ok(Header::Data {
        sequence: Some(sequence),
        clear_octets,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CipherUse {
    Control,
    Data {
        origin: Option<KeyOrigin>,
        count: u32,
    },
}
