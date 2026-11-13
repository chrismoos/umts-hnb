// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    EInterpretation, LiWidth, MAX_LIS, SEQUENCE_HEADER_BYTES, SEVEN_BIT_MAX_PDU, UmConfig, UmError,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LengthIndicator {
    End(u16),
    PreviousExact,
    PreviousOneShort,
    Start,
    Complete,
    CompleteOneShort,
    Middle,
    Padding,
}

impl LengthIndicator {
    pub(super) fn value(self, width: LiWidth) -> u16 {
        let mask = width.mask();
        match self {
            Self::End(end) => end,
            Self::PreviousExact => 0,
            Self::PreviousOneShort => mask - 4,
            Self::Start => mask - 3,
            Self::Complete => mask - 2,
            Self::CompleteOneShort => mask - 5,
            Self::Middle => mask - 1,
            Self::Padding => mask,
        }
    }
    fn decode(value: u16, width: LiWidth) -> Self {
        let mask = width.mask();
        match value {
            0 => Self::PreviousExact,
            value if value == mask => Self::Padding,
            value if value == mask - 1 => Self::Middle,
            value if value == mask - 2 => Self::Complete,
            value if value == mask - 3 => Self::Start,
            value if width == LiWidth::Fifteen && value == mask - 4 => Self::PreviousOneShort,
            value if width == LiWidth::Fifteen && value == mask - 5 => Self::CompleteOneShort,
            value => Self::End(value),
        }
    }
}

pub struct UmPdu<'a> {
    pub sequence: u8,
    pub data: &'a [u8],
    pub complete_without_li: bool,
    indicators: [LengthIndicator; MAX_LIS],
    count: usize,
}

impl UmPdu<'_> {
    pub fn indicators(&self) -> &[LengthIndicator] {
        &self.indicators[..self.count]
    }
}

// TS 25.322 9.2.1.3 and 9.2.2.5/8 place E at the LSB of each header octet/word.
pub fn parse(input: &[u8], config: UmConfig) -> Result<UmPdu<'_>, UmError> {
    if config.li_width == LiWidth::Seven && input.len() > SEVEN_BIT_MAX_PDU {
        return Err(UmError::Length);
    }
    let first = *input.first().ok_or(UmError::Length)?;
    let mut next = first & 1 != 0;
    let complete_without_li = !next && config.interpretation == EInterpretation::Alternative;
    let mut cursor = SEQUENCE_HEADER_BYTES;
    let mut indicators = [LengthIndicator::Padding; MAX_LIS];
    let mut count = 0;
    while next {
        if count == MAX_LIS {
            return Err(UmError::Capacity);
        }
        let bytes = input
            .get(cursor..cursor + config.li_width.octets())
            .ok_or(UmError::Header)?;
        let word = if bytes.len() == 1 {
            u16::from(bytes[0])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        };
        next = word & 1 != 0;
        indicators[count] = LengthIndicator::decode(word >> 1, config.li_width);
        count += 1;
        cursor += bytes.len();
    }
    let data = &input[cursor..];
    let mut previous = 0;
    for (index, indicator) in indicators[..count].iter().enumerate() {
        match indicator {
            LengthIndicator::End(end) => {
                let end = usize::from(*end);
                if end <= previous || end > data.len() {
                    return Err(UmError::Header);
                }
                previous = end;
            }
            LengthIndicator::Padding if index + 1 != count => return Err(UmError::Header),
            LengthIndicator::Start
            | LengthIndicator::PreviousExact
            | LengthIndicator::PreviousOneShort
                if index != 0 =>
            {
                return Err(UmError::Header);
            }
            LengthIndicator::Complete
            | LengthIndicator::CompleteOneShort
            | LengthIndicator::Middle
                if count != 1 =>
            {
                return Err(UmError::Header);
            }
            _ => {}
        }
    }
    if data.is_empty() && !indicators[..count].contains(&LengthIndicator::Padding) {
        return Err(UmError::Length);
    }
    if indicators.first() == Some(&LengthIndicator::CompleteOneShort) && data.len() < 2 {
        return Err(UmError::Length);
    }
    Ok(UmPdu {
        sequence: first >> 1,
        data,
        complete_without_li,
        indicators,
        count,
    })
}
