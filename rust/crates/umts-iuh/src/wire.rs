// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::Error;
use rasn::Decoder;
use rasn::{
    AsnType, Decode, Encode,
    types::{Any, BitStr, SequenceOf},
};

const MAX_PROTOCOL_IES: usize = 65535;
const FRAGMENT_UNIT: usize = 16384;
const MAX_FRAGMENT_UNITS: u8 = 4;
const LENGTH_EXTENDED_FLAG: u8 = 0x80;
const LENGTH_FRAGMENTED_FLAG: u8 = 0x40;
const LENGTH_VALUE_MASK: u8 = 0x3f;
const EXTENSIONS_PRESENT: u8 = 0x40;
const CRITICALITY_SHIFT: u32 = 6;
const BITS_PER_OCTET: usize = 8;
const PPID_HNBAP: u32 = 20;
const PPID_RUA: u32 = 19;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Hnbap,
    Rua,
    Ranap,
}
impl Protocol {
    pub fn ppid(self) -> Option<u32> {
        match self {
            Self::Hnbap => Some(PPID_HNBAP),
            Self::Rua => Some(PPID_RUA),
            Self::Ranap => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Initiating,
    Successful,
    Unsuccessful,
    Intermediate,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum Criticality {
    Reject,
    Ignore,
    Notify,
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub maximum_packet_bytes: usize,
    pub maximum_ies: usize,
    pub maximum_value_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            maximum_packet_bytes: 64 * 1024,
            maximum_ies: 64,
            maximum_value_bytes: 64 * 1024,
        }
    }
}
impl Limits {
    fn validate(self) -> Result<(), Error> {
        if self.maximum_packet_bytes == 0
            || self.maximum_ies == 0
            || self.maximum_ies > MAX_PROTOCOL_IES
            || self.maximum_value_bytes == 0
        {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}
#[derive(AsnType, Decode, Encode, Clone, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct InformationElement {
    pub id: u16,
    pub criticality: Criticality,
    pub value: Any,
}
impl std::fmt::Debug for InformationElement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InformationElement")
            .field("id", &self.id)
            .field("criticality", &self.criticality)
            .field("value_bytes", &self.value.as_bytes().len())
            .finish()
    }
}
impl InformationElement {
    pub fn new<T: Encode>(id: u16, criticality: Criticality, value: &T) -> Result<Self, Error> {
        Ok(Self {
            id,
            criticality,
            value: Any::new(encode_value(value)?),
        })
    }
    pub fn decode<T: Decode>(&self) -> Result<T, Error> {
        decode_value(self.value.as_bytes()).map_err(|_| Error::Value(self.id))
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("0..=65535"))]
struct IeList(SequenceOf<InformationElement>);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=65535"))]
pub(crate) struct ExtensionList(pub(crate) SequenceOf<InformationElement>);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct Contents {
    ies: IeList,
    extensions: Option<ExtensionList>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct Message {
    procedure: u8,
    criticality: Criticality,
    value: Any,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
enum ThreeWayPdu {
    Initiating(Message),
    Successful(Message),
    Unsuccessful(Message),
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
enum RanapPdu {
    Initiating(Message),
    Successful(Message),
    Unsuccessful(Message),
    Intermediate(Message),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub protocol: Protocol,
    pub outcome: Outcome,
    pub procedure: u8,
    pub criticality: Criticality,
    pub ies: Vec<InformationElement>,
    pub extensions: Vec<InformationElement>,
}
impl Packet {
    pub fn initiating(
        protocol: Protocol,
        procedure: u8,
        criticality: Criticality,
        ies: Vec<InformationElement>,
    ) -> Self {
        Self {
            protocol,
            outcome: Outcome::Initiating,
            procedure,
            criticality,
            ies,
            extensions: Vec::new(),
        }
    }
    pub fn successful(
        protocol: Protocol,
        procedure: u8,
        criticality: Criticality,
        ies: Vec<InformationElement>,
    ) -> Self {
        Self {
            protocol,
            outcome: Outcome::Successful,
            procedure,
            criticality,
            ies,
            extensions: Vec::new(),
        }
    }
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, Error> {
        limits.validate()?;
        validate_ies(&self.ies, &self.extensions, limits)?;
        let body = encode_value(&Contents {
            ies: IeList(self.ies.clone()),
            extensions: (!self.extensions.is_empty())
                .then(|| ExtensionList(self.extensions.clone())),
        })?;
        let message = Message {
            procedure: self.procedure,
            criticality: self.criticality,
            value: Any::new(body),
        };
        let bytes = match (self.protocol, self.outcome) {
            (Protocol::Ranap, outcome) => encode_value(&match outcome {
                Outcome::Initiating => RanapPdu::Initiating(message),
                Outcome::Successful => RanapPdu::Successful(message),
                Outcome::Unsuccessful => RanapPdu::Unsuccessful(message),
                Outcome::Intermediate => RanapPdu::Intermediate(message),
            })?,
            (_, Outcome::Initiating) => encode_value(&ThreeWayPdu::Initiating(message))?,
            (_, Outcome::Successful) => encode_value(&ThreeWayPdu::Successful(message))?,
            (_, Outcome::Unsuccessful) => encode_value(&ThreeWayPdu::Unsuccessful(message))?,
            (_, Outcome::Intermediate) => return Err(Error::Unsupported),
        };
        if bytes.len() > limits.maximum_packet_bytes {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }
    pub fn decode(protocol: Protocol, bytes: &[u8], limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        if bytes.is_empty() || bytes.len() > limits.maximum_packet_bytes {
            return Err(Error::Capacity);
        }
        let (outcome, message) = match protocol {
            Protocol::Ranap => match decode_value::<RanapPdu>(bytes)? {
                RanapPdu::Initiating(m) => (Outcome::Initiating, m),
                RanapPdu::Successful(m) => (Outcome::Successful, m),
                RanapPdu::Unsuccessful(m) => (Outcome::Unsuccessful, m),
                RanapPdu::Intermediate(m) => (Outcome::Intermediate, m),
            },
            _ => match decode_value::<ThreeWayPdu>(bytes)? {
                ThreeWayPdu::Initiating(m) => (Outcome::Initiating, m),
                ThreeWayPdu::Successful(m) => (Outcome::Successful, m),
                ThreeWayPdu::Unsuccessful(m) => (Outcome::Unsuccessful, m),
            },
        };
        preflight_contents(message.value.as_bytes(), limits)?;
        let contents = decode_value::<Contents>(message.value.as_bytes())?;
        let ies = contents.ies.0;
        let extensions = contents.extensions.map_or_else(Vec::new, |list| list.0);
        validate_ies(&ies, &extensions, limits)?;
        Ok(Self {
            protocol,
            outcome,
            procedure: message.procedure,
            criticality: message.criticality,
            ies,
            extensions,
        })
    }
    pub fn required(&self, id: u16) -> Result<&InformationElement, Error> {
        self.ies
            .iter()
            .find(|ie| ie.id == id)
            .ok_or(Error::MissingIe(id))
    }
    pub fn optional(&self, id: u16) -> Option<&InformationElement> {
        self.ies.iter().find(|ie| ie.id == id)
    }
    pub fn check(&self, known: &[u16], known_extensions: &[u16]) -> Result<Vec<u16>, Error> {
        let mut notify = Vec::new();
        for (ies, known) in [(&self.ies, known), (&self.extensions, known_extensions)] {
            let mut next = 0;
            for ie in ies {
                if let Some(position) = known.iter().position(|id| *id == ie.id) {
                    if position < next {
                        return Err(Error::IeOrder(ie.id));
                    }
                    next = position + 1;
                } else {
                    match ie.criticality {
                        Criticality::Reject => return Err(Error::UnknownRejectIe(ie.id)),
                        Criticality::Notify => notify.push(ie.id),
                        Criticality::Ignore => {}
                    }
                }
            }
        }
        Ok(notify)
    }
}
fn validate_ies(
    ies: &[InformationElement],
    extensions: &[InformationElement],
    limits: Limits,
) -> Result<(), Error> {
    if ies
        .len()
        .checked_add(extensions.len())
        .is_none_or(|count| count > limits.maximum_ies)
    {
        return Err(Error::Capacity);
    }
    let mut payload = 0usize;
    for list in [ies, extensions] {
        for (index, ie) in list.iter().enumerate() {
            if ie.value.as_bytes().len() > limits.maximum_value_bytes {
                return Err(Error::Capacity);
            }
            payload = payload
                .checked_add(ie.value.as_bytes().len())
                .ok_or(Error::Capacity)?;
            if payload > limits.maximum_packet_bytes {
                return Err(Error::Capacity);
            }
            if list[..index].iter().any(|previous| previous.id == ie.id) {
                return Err(Error::DuplicateIe(ie.id));
            }
        }
    }
    Ok(())
}

pub(crate) fn encode_value<T: Encode>(value: &T) -> Result<Vec<u8>, Error> {
    rasn::aper::encode(value).map_err(|_| Error::Encode)
}
pub(crate) fn decode_value<T: Decode>(bytes: &[u8]) -> Result<T, Error> {
    let mut decoder = rasn::aper::de::Decoder::<0, 0>::new(
        BitStr::from_slice(bytes),
        rasn::aper::de::DecoderOptions::aligned(),
    );
    let value = T::decode(&mut decoder).map_err(|_| Error::Decode)?;
    if decoder.input().len() >= BITS_PER_OCTET || decoder.input().any() {
        return Err(Error::Trailing);
    }
    Ok(value)
}

fn preflight_contents(bytes: &[u8], limits: Limits) -> Result<(), Error> {
    let flags = *bytes.first().ok_or(Error::Length)?;
    let mut cursor = Cursor { bytes, at: 1 };
    let count = usize::from(cursor.word()?);
    if count > limits.maximum_ies {
        return Err(Error::Capacity);
    }
    cursor.ies(count, limits.maximum_value_bytes)?;
    if flags & EXTENSIONS_PRESENT != 0 {
        let extensions = usize::from(cursor.word()?)
            .checked_add(1)
            .ok_or(Error::Length)?;
        if count
            .checked_add(extensions)
            .is_none_or(|total| total > limits.maximum_ies)
        {
            return Err(Error::Capacity);
        }
        cursor.ies(extensions, limits.maximum_value_bytes)?;
    }
    Ok(())
}

// TS 25.413 container lists contain containers, not a flat list of item values.
pub(crate) fn preflight_rab_list(bytes: &[u8], pairs: bool, limits: Limits) -> Result<(), Error> {
    limits.validate()?;
    if bytes.len() > limits.maximum_value_bytes {
        return Err(Error::Capacity);
    }
    let mut cursor = Cursor { bytes, at: 0 };
    let groups = usize::from(cursor.byte()?) + 1;
    let mut total = 0usize;
    for _ in 0..groups {
        let count = usize::from(cursor.word()?);
        total = total.checked_add(count).ok_or(Error::Capacity)?;
        if total > limits.maximum_ies {
            return Err(Error::Capacity);
        }
        for _ in 0..count {
            cursor.word()?;
            cursor.field(limits.maximum_value_bytes)?;
            if pairs {
                cursor.field(limits.maximum_value_bytes)?;
            }
        }
    }
    if cursor.at != bytes.len() {
        return Err(Error::Trailing);
    }
    Ok(())
}
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let end = self.at.checked_add(n).ok_or(Error::Length)?;
        let value = self.bytes.get(self.at..end).ok_or(Error::Length)?;
        self.at = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn word(&mut self) -> Result<u16, Error> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }
    fn ies(&mut self, count: usize, maximum: usize) -> Result<(), Error> {
        for _ in 0..count {
            self.word()?;
            self.field(maximum)?;
        }
        Ok(())
    }
    fn field(&mut self, maximum: usize) -> Result<(), Error> {
        if self.byte()? >> CRITICALITY_SHIFT > Criticality::Notify as u8 {
            return Err(Error::Decode);
        }
        let mut total = 0usize;
        loop {
            let first = self.byte()?;
            let (length, fragmented) = if first & LENGTH_EXTENDED_FLAG == 0 {
                (usize::from(first), false)
            } else if first & LENGTH_FRAGMENTED_FLAG == 0 {
                (
                    (usize::from(first & LENGTH_VALUE_MASK) << BITS_PER_OCTET)
                        | usize::from(self.byte()?),
                    false,
                )
            } else {
                let units = first & LENGTH_VALUE_MASK;
                if !(1..=MAX_FRAGMENT_UNITS).contains(&units) {
                    return Err(Error::Length);
                }
                (usize::from(units) * FRAGMENT_UNIT, true)
            };
            total = total.checked_add(length).ok_or(Error::Capacity)?;
            if total > maximum {
                return Err(Error::Capacity);
            }
            self.take(length)?;
            if !fragmented {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
