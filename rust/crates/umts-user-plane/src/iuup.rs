// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::WireError;

pub mod initialization;
pub mod negotiation;

const DATA_CRC: u8 = 0;
const DATA_NO_CRC: u8 = 1;
const CONTROL: u8 = 14;
const TYPE_SHIFT: u8 = 4;
const FRAME_MASK: u8 = 0x0f;
const QUALITY_SHIFT: u8 = 6;
const RFCI_MASK: u8 = 0x3f;
const HEADER_CRC_SHIFT: u8 = 2;
const PAYLOAD_CRC_HIGH: u8 = 3;
const CHECKED_HEADER_BYTES: usize = 2;
const DATA_HEADER_BYTES: usize = 3;
const CHECKED_DATA_HEADER_BYTES: usize = 4;
const CRC6_WIDTH: u8 = 6;
const CRC6_POLY: u16 = 0x2f;
const CRC10_WIDTH: u8 = 10;
const CRC10_POLY: u16 = 0x233;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameQuality {
    Good,
    Bad,
    BadRadio,
    Spare,
}
impl FrameQuality {
    fn wire(self) -> u8 {
        match self {
            Self::Good => 0,
            Self::Bad => 1,
            Self::BadRadio => 2,
            Self::Spare => 3,
        }
    }
    fn decode(value: u8) -> Self {
        match value {
            0 => Self::Good,
            1 => Self::Bad,
            2 => Self::BadRadio,
            _ => Self::Spare,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadCheck {
    Absent,
    Valid,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Acknowledgment {
    Procedure,
    Positive,
    Negative,
}
impl Acknowledgment {
    fn wire(self) -> u8 {
        match self {
            Self::Procedure => 0,
            Self::Positive => 1,
            Self::Negative => 2,
        }
    }
    fn decode(value: u8) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Procedure),
            1 => Ok(Self::Positive),
            2 => Ok(Self::Negative),
            _ => Err(WireError::Header),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Data {
        number: u8,
        quality: FrameQuality,
        rfci: u8,
        payload_crc: PayloadCheck,
        payload: &'a [u8],
    },
    Control {
        number: u8,
        acknowledgment: Acknowledgment,
        mode: u8,
        procedure: u8,
        payload_crc: PayloadCheck,
        payload: &'a [u8],
    },
}

// TS 25.415 6.7.7 uses zero-initialized, MSB-first systematic CRC without a final complement.
fn crc(input: &[u8], width: u8, polynomial: u16) -> u16 {
    let high = 1 << (width - 1);
    let mask = (1 << width) - 1;
    let mut state = 0;
    for byte in input {
        for bit in (0..u8::BITS).rev() {
            let feedback = (state & high != 0) ^ (byte & (1 << bit) != 0);
            state = (state << 1) & mask;
            if feedback {
                state ^= polynomial;
            }
        }
    }
    state
}
pub fn decode(input: &[u8]) -> Result<Frame<'_>, WireError> {
    if input.len() < DATA_HEADER_BYTES {
        return Err(WireError::Length);
    }
    let kind = input[0] >> TYPE_SHIFT;
    if !matches!(kind, DATA_CRC | DATA_NO_CRC | CONTROL) {
        return Err(WireError::Unsupported(kind));
    }
    if input[2] >> HEADER_CRC_SHIFT
        != crc(&input[..CHECKED_HEADER_BYTES], CRC6_WIDTH, CRC6_POLY) as u8
    {
        return Err(WireError::HeaderCrc);
    }
    let header = if kind == DATA_NO_CRC {
        DATA_HEADER_BYTES
    } else {
        CHECKED_DATA_HEADER_BYTES
    };
    if input.len() < header {
        return Err(WireError::Length);
    }
    let payload = &input[header..];
    let check = if kind == DATA_NO_CRC
        || (kind == CONTROL && (input[0] >> HEADER_CRC_SHIFT) & PAYLOAD_CRC_HIGH != 0)
    {
        PayloadCheck::Absent
    } else {
        let expected = u16::from(input[2] & PAYLOAD_CRC_HIGH) << u8::BITS | u16::from(input[3]);
        if crc(payload, CRC10_WIDTH, CRC10_POLY) == expected {
            PayloadCheck::Valid
        } else {
            PayloadCheck::Failed
        }
    };
    if kind == CONTROL {
        Ok(Frame::Control {
            number: input[0] & PAYLOAD_CRC_HIGH,
            acknowledgment: Acknowledgment::decode(
                (input[0] >> HEADER_CRC_SHIFT) & PAYLOAD_CRC_HIGH,
            )?,
            mode: input[1] >> TYPE_SHIFT,
            procedure: input[1] & FRAME_MASK,
            payload_crc: check,
            payload,
        })
    } else {
        Ok(Frame::Data {
            number: input[0] & FRAME_MASK,
            quality: FrameQuality::decode(input[1] >> QUALITY_SHIFT),
            rfci: input[1] & RFCI_MASK,
            payload_crc: check,
            payload,
        })
    }
}
pub fn encode(frame: Frame<'_>, output: &mut [u8]) -> Result<usize, WireError> {
    let (first, second, payload, checked) = match frame {
        Frame::Data {
            number,
            quality,
            rfci,
            payload_crc,
            payload,
        } => {
            if number > FRAME_MASK || rfci > RFCI_MASK || payload_crc == PayloadCheck::Failed {
                return Err(WireError::Header);
            }
            let checked = payload_crc == PayloadCheck::Valid;
            (
                ((if checked { DATA_CRC } else { DATA_NO_CRC }) << TYPE_SHIFT) | number,
                (quality.wire() << QUALITY_SHIFT) | rfci,
                payload,
                checked,
            )
        }
        Frame::Control {
            number,
            acknowledgment,
            mode,
            procedure,
            payload_crc,
            payload,
        } => {
            if number > PAYLOAD_CRC_HIGH
                || mode > FRAME_MASK
                || procedure > FRAME_MASK
                || payload_crc == PayloadCheck::Failed
            {
                return Err(WireError::Header);
            }
            let checked = acknowledgment == Acknowledgment::Procedure;
            if checked != (payload_crc == PayloadCheck::Valid) {
                return Err(WireError::Header);
            }
            (
                (CONTROL << TYPE_SHIFT) | (acknowledgment.wire() << HEADER_CRC_SHIFT) | number,
                (mode << TYPE_SHIFT) | procedure,
                payload,
                checked,
            )
        }
    };
    let header = if first >> TYPE_SHIFT == DATA_NO_CRC {
        DATA_HEADER_BYTES
    } else {
        CHECKED_DATA_HEADER_BYTES
    };
    let size = header
        .checked_add(payload.len())
        .ok_or(WireError::Capacity)?;
    if size > output.len() {
        return Err(WireError::Capacity);
    }
    output[..header].fill(0);
    output[0] = first;
    output[1] = second;
    output[2] =
        (crc(&output[..CHECKED_HEADER_BYTES], CRC6_WIDTH, CRC6_POLY) as u8) << HEADER_CRC_SHIFT;
    if checked {
        let payload_crc = crc(payload, CRC10_WIDTH, CRC10_POLY);
        output[2] |= (payload_crc >> u8::BITS) as u8;
        output[3] = payload_crc as u8;
    }
    output[header..size].copy_from_slice(payload);
    Ok(size)
}

#[cfg(test)]
mod tests;
