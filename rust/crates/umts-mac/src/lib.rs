// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_types::bits::{BITS_PER_BYTE, BitError, Bits, write_u32};

pub mod hsdpa;
pub mod hsupa;

const PREFIX_BITS: usize = 2;
const EXTENDED_BITS: usize = 8;
const MULTICAST_BITS: usize = 4;
const UE_TYPE_BITS: usize = 2;
const CELL_ID_BITS: usize = 16;
const UTRAN_ID_BITS: usize = 32;
const CHANNEL_BITS: usize = 4;
const CELL_ID_TYPE: u32 = 1;
const UTRAN_ID_TYPE: u32 = 0;
const MAX_LOGICAL_CHANNEL: u8 = 15;
const MAX_HEADER_FIELDS: usize = 4;
const RACH_CCCH: u32 = 0b00;
const RACH_DEDICATED: u32 = 0b01;
const FACH_BCCH: u32 = 0b00;
const FACH_EXTENDED: u32 = 0b01;
const FACH_CTCH_PREFIX: u32 = 0b10;
const FACH_DEDICATED: u32 = 0b11;
const FACH_CCCH: u32 = 0b01000000;
const FACH_CTCH: u32 = 0b10000000;
const FACH_MCCH: u32 = 0b01010000;
const FACH_MSCH: u32 = 0b01011111;
const FACH_MTCH: u32 = 0b0110;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacError {
    Bits(BitError),
    Reserved,
    Mapping,
    Identity,
    LogicalChannel,
    EmptyPayload,
    Capacity,
}

impl From<BitError> for MacError {
    fn from(value: BitError) -> Self {
        Self::Bits(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogicalChannelId(u8);
impl LogicalChannelId {
    pub fn new(value: u8) -> Result<Self, MacError> {
        if !(1..=MAX_LOGICAL_CHANNEL).contains(&value) {
            return Err(MacError::LogicalChannel);
        }
        Ok(Self(value))
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommonIdentity {
    Cell(u16),
    // TS 25.321 9.2.1 permits U-RNTI only for downlink SRB1 using RLC UM. Bearer mapping must enforce this.
    Utran(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FddCommonChannel {
    Rach,
    Fach,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommonHeader {
    Broadcast,
    CommonControl,
    CommonTraffic,
    Dedicated {
        identity: CommonIdentity,
        channel: LogicalChannelId,
    },
    MulticastControl,
    MulticastSchedule,
    MulticastTraffic {
        channel: LogicalChannelId,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct CommonPdu<'a> {
    pub header: CommonHeader,
    pub payload: Bits<'a>,
}

struct Reader<'a> {
    input: Bits<'a>,
    position: usize,
}
impl<'a> Reader<'a> {
    fn read(&mut self, width: usize) -> Result<u32, MacError> {
        let value = self.input.read_u32(self.position, width)?;
        self.position += width;
        Ok(value)
    }
    fn payload(&self) -> Result<Bits<'a>, MacError> {
        let bits = self.input.subspan(self.position..self.input.len())?;
        if bits.is_empty() {
            return Err(MacError::EmptyPayload);
        }
        Ok(bits)
    }
    fn logical_channel(&mut self) -> Result<LogicalChannelId, MacError> {
        LogicalChannelId::new(self.read(CHANNEL_BITS)? as u8 + 1).map_err(|_| MacError::Reserved)
    }
}

// TS 25.321 9.2.1.1, Tables 9.2.1.6/7 forbid a U-RNTI in an uplink common-channel MAC header.
fn dedicated(
    reader: &mut Reader<'_>,
    transport: FddCommonChannel,
) -> Result<CommonHeader, MacError> {
    let identity = match reader.read(UE_TYPE_BITS)? {
        CELL_ID_TYPE => CommonIdentity::Cell(reader.read(CELL_ID_BITS)? as u16),
        UTRAN_ID_TYPE if transport == FddCommonChannel::Fach => {
            CommonIdentity::Utran(reader.read(UTRAN_ID_BITS)?)
        }
        UTRAN_ID_TYPE => return Err(MacError::Identity),
        _ => return Err(MacError::Reserved),
    };
    Ok(CommonHeader::Dedicated {
        identity,
        channel: reader.logical_channel()?,
    })
}

// TS 25.321 Tables 9.2.1.2/4 use direction-specific, variable-width FDD TCTFs.
pub fn decode_common(
    transport: FddCommonChannel,
    input: Bits<'_>,
) -> Result<CommonPdu<'_>, MacError> {
    let mut reader = Reader { input, position: 0 };
    let prefix = reader.read(PREFIX_BITS)?;
    let header = match (transport, prefix) {
        (FddCommonChannel::Rach, RACH_CCCH) => CommonHeader::CommonControl,
        (FddCommonChannel::Rach, RACH_DEDICATED) | (FddCommonChannel::Fach, FACH_DEDICATED) => {
            dedicated(&mut reader, transport)?
        }
        (FddCommonChannel::Fach, FACH_BCCH) => CommonHeader::Broadcast,
        (FddCommonChannel::Fach, FACH_EXTENDED) => {
            let next = reader.read(MULTICAST_BITS - PREFIX_BITS)?;
            let short = (prefix << (MULTICAST_BITS - PREFIX_BITS)) | next;
            if short == FACH_MTCH {
                CommonHeader::MulticastTraffic {
                    channel: reader.logical_channel()?,
                }
            } else if short == FACH_CCCH >> (EXTENDED_BITS - MULTICAST_BITS)
                || short == FACH_MCCH >> (EXTENDED_BITS - MULTICAST_BITS)
            {
                let suffix = reader.read(EXTENDED_BITS - MULTICAST_BITS)?;
                match (short << (EXTENDED_BITS - MULTICAST_BITS)) | suffix {
                    FACH_CCCH => CommonHeader::CommonControl,
                    FACH_MCCH => CommonHeader::MulticastControl,
                    FACH_MSCH => CommonHeader::MulticastSchedule,
                    _ => return Err(MacError::Reserved),
                }
            } else {
                return Err(MacError::Reserved);
            }
        }
        (FddCommonChannel::Fach, FACH_CTCH_PREFIX) => {
            if reader.read(EXTENDED_BITS - PREFIX_BITS)? != 0 {
                return Err(MacError::Reserved);
            }
            CommonHeader::CommonTraffic
        }
        _ => return Err(MacError::Reserved),
    };
    Ok(CommonPdu {
        header,
        payload: reader.payload()?,
    })
}

fn header_fields(
    transport: FddCommonChannel,
    header: CommonHeader,
) -> Result<([(u32, usize); MAX_HEADER_FIELDS], usize), MacError> {
    let mut fields = [(0, 0); MAX_HEADER_FIELDS];
    fields[0] = match (transport, header) {
        (FddCommonChannel::Rach, CommonHeader::CommonControl) => (RACH_CCCH, PREFIX_BITS),
        (
            FddCommonChannel::Rach,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(_),
                ..
            },
        ) => (RACH_DEDICATED, PREFIX_BITS),
        (FddCommonChannel::Fach, CommonHeader::Broadcast) => (FACH_BCCH, PREFIX_BITS),
        (FddCommonChannel::Fach, CommonHeader::CommonControl) => (FACH_CCCH, EXTENDED_BITS),
        (FddCommonChannel::Fach, CommonHeader::CommonTraffic) => (FACH_CTCH, EXTENDED_BITS),
        (FddCommonChannel::Fach, CommonHeader::Dedicated { .. }) => (FACH_DEDICATED, PREFIX_BITS),
        (FddCommonChannel::Fach, CommonHeader::MulticastControl) => (FACH_MCCH, EXTENDED_BITS),
        (FddCommonChannel::Fach, CommonHeader::MulticastSchedule) => (FACH_MSCH, EXTENDED_BITS),
        (FddCommonChannel::Fach, CommonHeader::MulticastTraffic { .. }) => {
            (FACH_MTCH, MULTICAST_BITS)
        }
        _ => return Err(MacError::Mapping),
    };
    let count = match header {
        CommonHeader::Dedicated { identity, channel } => {
            fields[1] = match identity {
                CommonIdentity::Cell(_) => (CELL_ID_TYPE, UE_TYPE_BITS),
                CommonIdentity::Utran(_) => (UTRAN_ID_TYPE, UE_TYPE_BITS),
            };
            fields[2] = match identity {
                CommonIdentity::Cell(id) => (u32::from(id), CELL_ID_BITS),
                CommonIdentity::Utran(id) => (id, UTRAN_ID_BITS),
            };
            fields[3] = (u32::from(channel.get() - 1), CHANNEL_BITS);
            4
        }
        CommonHeader::MulticastTraffic { channel } => {
            fields[1] = (u32::from(channel.get() - 1), CHANNEL_BITS);
            2
        }
        _ => 1,
    };
    Ok((fields, count))
}

pub fn encode_common(
    transport: FddCommonChannel,
    header: CommonHeader,
    payload: Bits<'_>,
    output: &mut [u8],
) -> Result<usize, MacError> {
    if payload.is_empty() {
        return Err(MacError::EmptyPayload);
    }
    let (fields, count) = header_fields(transport, header)?;
    let header_bits: usize = fields[..count].iter().map(|(_, width)| width).sum();
    let length = header_bits
        .checked_add(payload.len())
        .ok_or(MacError::Capacity)?;
    let output = output
        .get_mut(..length.div_ceil(BITS_PER_BYTE))
        .ok_or(MacError::Capacity)?;
    output.fill(0);
    let mut position = 0;
    for (value, width) in &fields[..count] {
        write_u32(output, position, *width, *value)?;
        position += width;
    }
    payload.copy_into(output, position)?;
    Ok(length)
}

pub fn common_header_bits(
    transport: FddCommonChannel,
    header: CommonHeader,
) -> Result<usize, MacError> {
    let (fields, count) = header_fields(transport, header)?;
    Ok(fields[..count].iter().map(|(_, width)| width).sum())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DedicatedMapping {
    Single,
    Multiplexed,
}
#[derive(Clone, Copy, Debug)]
pub struct DedicatedPdu<'a> {
    pub channel: Option<LogicalChannelId>,
    pub payload: Bits<'a>,
}

// TS 25.321 9.2.1.1 includes C/T on DCH only when MAC multiplexing is configured.
pub fn decode_dedicated(
    mapping: DedicatedMapping,
    input: Bits<'_>,
) -> Result<DedicatedPdu<'_>, MacError> {
    let mut reader = Reader { input, position: 0 };
    let channel = match mapping {
        DedicatedMapping::Single => None,
        DedicatedMapping::Multiplexed => Some(reader.logical_channel()?),
    };
    Ok(DedicatedPdu {
        channel,
        payload: reader.payload()?,
    })
}

pub fn encode_dedicated(
    mapping: DedicatedMapping,
    channel: Option<LogicalChannelId>,
    payload: Bits<'_>,
    output: &mut [u8],
) -> Result<usize, MacError> {
    let header_bits = match (mapping, channel) {
        (DedicatedMapping::Single, None) => 0,
        (DedicatedMapping::Multiplexed, Some(_)) => CHANNEL_BITS,
        _ => return Err(MacError::Mapping),
    };
    if payload.is_empty() {
        return Err(MacError::EmptyPayload);
    }
    let length = header_bits
        .checked_add(payload.len())
        .ok_or(MacError::Capacity)?;
    let output = output
        .get_mut(..length.div_ceil(BITS_PER_BYTE))
        .ok_or(MacError::Capacity)?;
    output.fill(0);
    if let Some(channel) = channel {
        write_u32(output, 0, header_bits, u32::from(channel.get() - 1))?;
    }
    payload.copy_into(output, header_bits)?;
    Ok(length)
}

#[cfg(test)]
mod tests;
