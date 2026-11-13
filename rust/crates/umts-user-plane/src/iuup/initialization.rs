// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU16;

use super::{Frame, FrameQuality, PayloadCheck};
use crate::WireError;
use umts_types::bits::{BITS_PER_BYTE, Bits};

pub const MAX_SUBFLOWS: usize = 7;
pub const MAX_RFCIS: usize = 63;
pub const MAX_CHAIN_FRAMES: usize = 4;
const TIMING_FLAG: u8 = 0x10;
const FLOW_SHIFT: u8 = 1;
const FLOW_MASK: u8 = 7;
const CHAIN_FLAG: u8 = 1;
const LAST_FLAG: u8 = 0x80;
const LONG_LENGTH_FLAG: u8 = 0x40;
const RFCI_MASK: u8 = 0x3f;
const NIBBLE_SHIFT: u8 = 4;
const NIBBLE_MASK: u8 = 0x0f;
const INITIALIZATION_TRAILER: usize = 3;
const CONTROL_SPARE_EXTENSION: usize = 32;
const DATA_SPARE_EXTENSION: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rfci(u8);

impl Rfci {
    pub fn new(value: u8) -> Result<Self, WireError> {
        if usize::from(value) < MAX_RFCIS {
            Ok(Self(value))
        } else {
            Err(WireError::Header)
        }
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeVersion(u8);

impl ModeVersion {
    // TS 25.415 6.6.3.26 encodes version 1 as zero.
    pub fn new(version: u8) -> Result<Self, WireError> {
        if (1..=u16::BITS as u8).contains(&version) {
            Ok(Self(version - 1))
        } else {
            Err(WireError::Header)
        }
    }

    pub fn from_wire(value: u8) -> Result<Self, WireError> {
        Self::new(value.checked_add(1).ok_or(WireError::Header)?)
    }

    pub fn wire(self) -> u8 {
        self.0
    }
    pub fn number(self) -> u8 {
        self.0 + 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Versions(NonZeroU16);

impl Versions {
    pub fn new(mask: u16) -> Result<Self, WireError> {
        NonZeroU16::new(mask).map(Self).ok_or(WireError::Header)
    }

    pub fn mask(self) -> u16 {
        self.0.get()
    }
    pub fn supports(self, version: ModeVersion) -> bool {
        self.mask() & (1 << version.wire()) != 0
    }
    pub fn intersection(self, peer: Self) -> Option<Self> {
        Self::new(self.mask() & peer.mask()).ok()
    }
    pub fn highest(self) -> ModeVersion {
        ModeVersion((u16::BITS - 1 - self.0.leading_zeros()) as u8)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataPduType {
    Crc,
    NoCrc,
}

impl DataPduType {
    fn wire(self) -> u8 {
        match self {
            Self::Crc => 0,
            Self::NoCrc => 1,
        }
    }
    fn decode(value: u8) -> Result<Self, WireError> {
        match value {
            0 => Ok(Self::Crc),
            1 => Ok(Self::NoCrc),
            other => Err(WireError::Unsupported(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Combination {
    rfci: Rfci,
    lengths: [u16; MAX_SUBFLOWS],
    count: u8,
    ipti: Option<u8>,
}

impl Combination {
    pub fn new(rfci: Rfci, lengths: &[u16], ipti: Option<u8>) -> Result<Self, WireError> {
        if lengths.is_empty()
            || lengths.len() > MAX_SUBFLOWS
            || ipti.is_some_and(|value| value > NIBBLE_MASK)
        {
            return Err(WireError::Header);
        }
        let mut result = Self {
            rfci,
            lengths: [0; MAX_SUBFLOWS],
            count: lengths.len() as u8,
            ipti,
        };
        result.lengths[..lengths.len()].copy_from_slice(lengths);
        Ok(result)
    }

    pub fn rfci(self) -> Rfci {
        self.rfci
    }
    pub fn lengths(&self) -> &[u16] {
        &self.lengths[..usize::from(self.count)]
    }
    pub fn ipti(self) -> Option<u8> {
        self.ipti
    }
    pub fn payload_bits(&self) -> usize {
        self.lengths().iter().map(|value| usize::from(*value)).sum()
    }

    fn long_lengths(&self) -> bool {
        self.lengths()
            .iter()
            .any(|length| *length > u16::from(u8::MAX))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Initialization {
    formats: Formats,
    versions: Versions,
    chained: bool,
}

impl Initialization {
    pub fn new(
        rows: &[Combination],
        versions: Versions,
        data: DataPduType,
        chained: bool,
    ) -> Result<Self, WireError> {
        if let Some(first) = rows.first()
            && rows
                .iter()
                .any(|row| row.ipti.is_some() != first.ipti.is_some())
        {
            return Err(WireError::Header);
        }
        Ok(Self {
            formats: Formats::new(rows, data)?,
            versions,
            chained,
        })
    }

    pub fn rows(&self) -> impl Iterator<Item = &Combination> {
        self.formats.rows()
    }
    pub fn versions(&self) -> Versions {
        self.versions
    }
    pub fn data_type(&self) -> DataPduType {
        self.formats.data_type()
    }
    pub fn chained(&self) -> bool {
        self.chained
    }
    pub fn subflows(&self) -> usize {
        self.formats.subflows()
    }
    fn has_timing(&self) -> bool {
        self.formats.first.ipti.is_some()
    }
    fn row_count(&self) -> usize {
        1 + self.formats.rest.len()
    }

    pub fn encoded_bytes(&self) -> usize {
        1 + self
            .rows()
            .map(|row| 1 + row.lengths().len() * if row.long_lengths() { 2 } else { 1 })
            .sum::<usize>()
            + if self.has_timing() {
                self.row_count().div_ceil(2)
            } else {
                0
            }
            + INITIALIZATION_TRAILER
    }

    pub fn encode(&self, output: &mut [u8]) -> Result<usize, WireError> {
        let size = self.encoded_bytes();
        let output = output.get_mut(..size).ok_or(WireError::Capacity)?;
        output.fill(0);
        output[0] = (self.formats.first.count << FLOW_SHIFT)
            | if self.has_timing() { TIMING_FLAG } else { 0 }
            | if self.chained { CHAIN_FLAG } else { 0 };
        let mut cursor = 1;
        for (index, row) in self.rows().enumerate() {
            let long = row.long_lengths();
            output[cursor] = row.rfci.get()
                | if index + 1 == self.row_count() {
                    LAST_FLAG
                } else {
                    0
                }
                | if long { LONG_LENGTH_FLAG } else { 0 };
            cursor += 1;
            for length in row.lengths() {
                if long {
                    output[cursor..cursor + 2].copy_from_slice(&length.to_be_bytes());
                    cursor += 2;
                } else {
                    output[cursor] = *length as u8;
                    cursor += 1;
                }
            }
        }
        if self.has_timing() {
            for (index, row) in self.rows().enumerate() {
                if let Some(ipti) = row.ipti {
                    output[cursor + index / 2] |= ipti
                        << if index.is_multiple_of(2) {
                            NIBBLE_SHIFT
                        } else {
                            0
                        };
                }
            }
            cursor += self.row_count().div_ceil(2);
        }
        output[cursor..cursor + 2].copy_from_slice(&self.versions.mask().to_be_bytes());
        output[cursor + 2] = self.data_type().wire() << NIBBLE_SHIFT;
        Ok(size)
    }

    pub fn decode(input: &[u8]) -> Result<Self, WireError> {
        let flags = *input.first().ok_or(WireError::Length)?;
        let flows = usize::from((flags >> FLOW_SHIFT) & FLOW_MASK);
        if flows == 0 {
            return Err(WireError::Header);
        }
        let timing = flags & TIMING_FLAG != 0;
        let mut cursor = 1;
        let mut rows = Vec::with_capacity(MAX_RFCIS);
        loop {
            if rows.len() == MAX_RFCIS {
                return Err(WireError::Capacity);
            }
            let header = *input.get(cursor).ok_or(WireError::Length)?;
            cursor += 1;
            let mut lengths = [0; MAX_SUBFLOWS];
            for length in &mut lengths[..flows] {
                *length = if header & LONG_LENGTH_FLAG != 0 {
                    let value = input.get(cursor..cursor + 2).ok_or(WireError::Length)?;
                    cursor += 2;
                    u16::from_be_bytes([value[0], value[1]])
                } else {
                    let value = *input.get(cursor).ok_or(WireError::Length)?;
                    cursor += 1;
                    u16::from(value)
                };
            }
            rows.push(Combination::new(
                Rfci::new(header & RFCI_MASK)?,
                &lengths[..flows],
                timing.then_some(0),
            )?);
            if header & LAST_FLAG != 0 {
                break;
            }
        }
        if timing {
            let values = input
                .get(cursor..cursor + rows.len().div_ceil(2))
                .ok_or(WireError::Length)?;
            for (index, row) in rows.iter_mut().enumerate() {
                row.ipti = Some(
                    (values[index / 2]
                        >> if index.is_multiple_of(2) {
                            NIBBLE_SHIFT
                        } else {
                            0
                        })
                        & NIBBLE_MASK,
                );
            }
            cursor += rows.len().div_ceil(2);
        }
        let trailer = input
            .get(cursor..cursor + INITIALIZATION_TRAILER)
            .ok_or(WireError::Length)?;
        if input.len() - cursor - INITIALIZATION_TRAILER > CONTROL_SPARE_EXTENSION {
            return Err(WireError::Length);
        }
        Self::new(
            &rows,
            Versions::new(u16::from_be_bytes([trailer[0], trailer[1]]))?,
            DataPduType::decode(trailer[2] >> NIBBLE_SHIFT)?,
            flags & CHAIN_FLAG != 0,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formats {
    first: Combination,
    rest: Box<[Combination]>,
    data: DataPduType,
}

impl Formats {
    fn new(rows: &[Combination], data: DataPduType) -> Result<Self, WireError> {
        let (first, rest) = rows.split_first().ok_or(WireError::Length)?;
        if rows.len() > MAX_RFCIS {
            return Err(WireError::Capacity);
        }
        let mut seen = [false; MAX_RFCIS];
        for row in rows {
            if row.count != first.count
                || std::mem::replace(&mut seen[usize::from(row.rfci.get())], true)
            {
                return Err(WireError::Header);
            }
        }
        Ok(Self {
            first: *first,
            rest: rest.into(),
            data,
        })
    }

    pub fn from_initialization(frame: &Initialization) -> Self {
        frame.formats.clone()
    }

    pub fn append(&self, frame: &Initialization) -> Result<Self, WireError> {
        if frame.subflows() != self.subflows() || frame.data_type() != self.data {
            return Err(WireError::Header);
        }
        if 1 + self.rest.len() + frame.row_count() > MAX_RFCIS {
            return Err(WireError::Capacity);
        }
        let rows: Vec<_> = self.rows().chain(frame.rows()).copied().collect();
        Self::new(&rows, self.data)
    }

    pub fn rows(&self) -> impl Iterator<Item = &Combination> {
        std::iter::once(&self.first).chain(self.rest.iter())
    }
    pub fn first(&self) -> &Combination {
        &self.first
    }
    pub fn lookup(&self, rfci: Rfci) -> Option<&Combination> {
        self.rows().find(|row| row.rfci == rfci)
    }
    pub fn data_type(&self) -> DataPduType {
        self.data
    }
    pub fn subflows(&self) -> usize {
        self.first.lengths().len()
    }
    pub fn maximum_payload_bytes(&self) -> usize {
        self.rest
            .iter()
            .fold(self.first.payload_bits(), |maximum, row| {
                maximum.max(row.payload_bits())
            })
            .div_ceil(BITS_PER_BYTE)
    }

    pub fn maximum_frame_bytes(&self) -> usize {
        self.maximum_payload_bytes()
            + match self.data {
                DataPduType::Crc => super::CHECKED_DATA_HEADER_BYTES,
                DataPduType::NoCrc => super::DATA_HEADER_BYTES,
            }
    }

    pub fn pack(
        &self,
        rfci: Rfci,
        subflows: &[Bits<'_>],
        output: &mut [u8],
    ) -> Result<usize, WireError> {
        let row = self.lookup(rfci).ok_or(WireError::Header)?;
        if subflows.len() != row.lengths().len()
            || subflows
                .iter()
                .zip(row.lengths())
                .any(|(bits, length)| bits.len() != usize::from(*length))
        {
            return Err(WireError::Length);
        }
        let size = row.payload_bits().div_ceil(BITS_PER_BYTE);
        let output = output.get_mut(..size).ok_or(WireError::Capacity)?;
        output.fill(0);
        let mut offset = 0;
        for bits in subflows {
            bits.copy_into(output, offset)
                .map_err(|_| WireError::Length)?;
            offset += bits.len();
        }
        Ok(size)
    }

    pub fn downlink<'a>(&self, frame: Frame<'a>) -> Result<Media<'a>, MediaError> {
        let Frame::Data {
            rfci,
            number,
            quality,
            payload_crc,
            payload,
        } = frame
        else {
            return Err(MediaError::Pdu);
        };
        if payload_crc == PayloadCheck::Failed {
            return Err(MediaError::PayloadCrc);
        }
        if quality != FrameQuality::Good {
            return Err(MediaError::Quality);
        }
        if (self.data == DataPduType::Crc) != (payload_crc == PayloadCheck::Valid) {
            return Err(MediaError::Pdu);
        }
        let row = self
            .lookup(Rfci::new(rfci).map_err(|_| MediaError::Rfci)?)
            .ok_or(MediaError::Rfci)?;
        let bytes = row.payload_bits().div_ceil(BITS_PER_BYTE);
        if payload.len() < bytes || payload.len() - bytes > DATA_SPARE_EXTENSION {
            return Err(MediaError::Length);
        }
        let data = Bits::new(payload, row.payload_bits()).map_err(|_| MediaError::Length)?;
        let mut subflows = [Bits::empty(); MAX_SUBFLOWS];
        let mut offset = 0;
        for (slot, length) in subflows.iter_mut().zip(row.lengths()) {
            let end = offset + usize::from(*length);
            *slot = data.subspan(offset..end).map_err(|_| MediaError::Length)?;
            offset = end;
        }
        Ok(Media {
            rfci: row.rfci,
            number,
            subflows,
            count: self.subflows(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaError {
    Pdu,
    PayloadCrc,
    Quality,
    Rfci,
    Length,
}

pub struct Media<'a> {
    pub rfci: Rfci,
    pub number: u8,
    subflows: [Bits<'a>; MAX_SUBFLOWS],
    count: usize,
}

impl<'a> Media<'a> {
    pub fn subflows(&self) -> &[Bits<'a>] {
        &self.subflows[..self.count]
    }
}

#[cfg(test)]
mod tests;
