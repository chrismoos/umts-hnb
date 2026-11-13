// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::{CodecError, EncodedRrc, MAX_DEDICATED_BITS, encode_downlink_dedicated};
use umts_rrc_schema::class_definitions::DLDCCHMessage;
#[cfg(test)]
use umts_security::algorithms::Uia1Key;
use umts_security::contexts::IntegrityMac;
use umts_types::Direction;

mod streams;
pub use streams::{AcceptedUplink, DownlinkIntegrity, StreamError, UplinkIntegrity};
mod epochs;
pub use epochs::{EpochError, IntegrityEpochs, IntegritySeed};
mod schedule;
pub(crate) use schedule::IntegritySetup;
pub use schedule::{
    CompletionAdmission, IntegritySchedule, RecoveryPlan, ScheduleError, ScheduleFailure,
};

const PRESENCE_BITS: usize = 1;
const MAC_BITS: usize = 32;
const SEQUENCE_BITS: usize = 4;
const ADDED_BITS: usize = MAC_BITS + SEQUENCE_BITS;
const MAC_OFFSET: usize = PRESENCE_BITS;
const SEQUENCE_OFFSET: usize = MAC_OFFSET + MAC_BITS;
const PROTECTED_HEADER_BITS: usize = PRESENCE_BITS + ADDED_BITS;
const MAX_SRB: u8 = 4;
const SEQUENCE_MASK: u32 = (1 << SEQUENCE_BITS) - 1;
const OCTET_BITS: usize = u8::BITS as usize;

pub struct Parameters<'a> {
    pub key: &'a dyn IntegrityMac,
    pub count: u32,
    pub fresh: u32,
    pub bearer: u8,
    pub direction: Direction,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Header,
    Length,
    Bearer,
    Direction,
    Sequence,
    Authentication,
    Codec(CodecError),
    Algorithm(umts_security::Error),
}

pub struct VerifiedUplink<'a> {
    bytes: &'a [u8],
    bit_length: usize,
    count: u32,
}
pub(crate) fn uplink_sequence(bytes: &[u8]) -> Result<u8, Error> {
    if bytes.len() < PROTECTED_HEADER_BITS.div_ceil(OCTET_BITS) {
        return Err(Error::Length);
    }
    if get(bytes, 0, PRESENCE_BITS) == 0 {
        return Err(Error::Header);
    }
    Ok(get(bytes, SEQUENCE_OFFSET, SEQUENCE_BITS) as u8)
}
impl<'a> VerifiedUplink<'a> {
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub fn count(&self) -> u32 {
        self.count
    }
    pub fn bit_length(&self) -> usize {
        self.bit_length
    }
}

pub fn protect_downlink(
    parameters: Parameters<'_>,
    message: &DLDCCHMessage,
) -> Result<EncodedRrc, Error> {
    validate(&parameters, Direction::Downlink)?;
    if message.integrity_check_info.is_some() {
        return Err(Error::Header);
    }
    let unprotected = encode_downlink_dedicated(message).map_err(Error::Codec)?;
    let length = protected_bit_length(unprotected.bit_length)?;
    let mut bytes = vec![0; length.div_ceil(OCTET_BITS)];
    put(&mut bytes, 0, 1, 1);
    for index in PRESENCE_BITS..unprotected.bit_length {
        put(
            &mut bytes,
            index + ADDED_BITS,
            1,
            get(&unprotected.bytes, index, 1),
        );
    }
    // TS 25.331 8.5.10.3 embeds RB identity in the 32-bit MAC field and zeroes RRC SN before hashing.
    put(
        &mut bytes,
        MAC_OFFSET,
        MAC_BITS,
        u32::from(parameters.bearer),
    );
    let mac = parameters
        .key
        .authenticate(
            parameters.count,
            parameters.fresh,
            Direction::Downlink,
            &bytes,
            bytes.len() * OCTET_BITS,
        )
        .map_err(Error::Algorithm)?;
    put(&mut bytes, MAC_OFFSET, MAC_BITS, u32::from_be_bytes(mac));
    put(
        &mut bytes,
        SEQUENCE_OFFSET,
        SEQUENCE_BITS,
        parameters.count & SEQUENCE_MASK,
    );
    Ok(EncodedRrc {
        bytes,
        bit_length: length,
    })
}

pub fn protected_bit_length(unprotected: usize) -> Result<usize, Error> {
    unprotected
        .checked_add(ADDED_BITS)
        .filter(|bits| *bits <= MAX_DEDICATED_BITS)
        .ok_or(Error::Length)
}

// Authentication covers the original RLC SDU including padding and unknown RRC extension bits.
pub fn verify_uplink<'a>(
    parameters: Parameters<'_>,
    bytes: &'a [u8],
    scratch: &mut [u8],
) -> Result<VerifiedUplink<'a>, Error> {
    verify_uplink_bits(parameters, bytes, bytes.len() * OCTET_BITS, scratch)
}

/// TS 25.331 8.5.10.3 and 12.1.3 include RRC padding but do not octet-align ordinary uplink TM.
pub fn verify_uplink_bits<'a>(
    parameters: Parameters<'_>,
    bytes: &'a [u8],
    bit_length: usize,
    scratch: &mut [u8],
) -> Result<VerifiedUplink<'a>, Error> {
    validate(&parameters, Direction::Uplink)?;
    if !(PROTECTED_HEADER_BITS..=MAX_DEDICATED_BITS).contains(&bit_length)
        || bytes.len() != bit_length.div_ceil(OCTET_BITS)
        || scratch.len() < bytes.len()
    {
        return Err(Error::Length);
    }
    if get(bytes, 0, 1) != 1 {
        return Err(Error::Header);
    }
    if get(bytes, SEQUENCE_OFFSET, SEQUENCE_BITS) != parameters.count & SEQUENCE_MASK {
        return Err(Error::Sequence);
    }
    let received = get(bytes, MAC_OFFSET, MAC_BITS).to_be_bytes();
    let scratch = &mut scratch[..bytes.len()];
    scratch.copy_from_slice(bytes);
    put(scratch, MAC_OFFSET, MAC_BITS, u32::from(parameters.bearer));
    put(scratch, SEQUENCE_OFFSET, SEQUENCE_BITS, 0);
    let expected = parameters
        .key
        .authenticate(
            parameters.count,
            parameters.fresh,
            Direction::Uplink,
            scratch,
            bit_length,
        )
        .map_err(Error::Algorithm)?;
    let difference = received
        .into_iter()
        .zip(expected)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    if difference != 0 {
        return Err(Error::Authentication);
    }
    Ok(VerifiedUplink {
        bytes,
        bit_length,
        count: parameters.count,
    })
}

fn validate(parameters: &Parameters<'_>, direction: Direction) -> Result<(), Error> {
    if parameters.bearer > MAX_SRB {
        return Err(Error::Bearer);
    }
    if parameters.direction != direction {
        return Err(Error::Direction);
    }
    Ok(())
}
fn get(bytes: &[u8], offset: usize, bits: usize) -> u32 {
    (offset..offset + bits).fold(0, |value, index| {
        (value << 1)
            | u32::from((bytes[index / OCTET_BITS] >> (OCTET_BITS - 1 - index % OCTET_BITS)) & 1)
    })
}
fn put(bytes: &mut [u8], offset: usize, bits: usize, value: u32) {
    for index in 0..bits {
        let mask = 1 << (OCTET_BITS - 1 - (offset + index) % OCTET_BITS);
        let byte = &mut bytes[(offset + index) / OCTET_BITS];
        if value & (1 << (bits - index - 1)) != 0 {
            *byte |= mask
        } else {
            *byte &= !mask
        }
    }
}

#[cfg(test)]
mod tests;
