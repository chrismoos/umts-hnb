// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::{
    Decode, Encode,
    types::{BitStr, FixedBitString},
};
use umts_rrc_schema::{
    class_definitions::{
        BCCHBCHMessage, DLCCCHMessage, DLCCCHMessageType, DLDCCHMessage, DLDCCHMessageType,
        PCCHMessage, PCCHMessageType, ULCCCHMessage, ULCCCHMessageType, ULCCCHMessageTypeR11,
        ULDCCHMessage, ULDCCHMessageType, ULDCCHMessageTypeExt,
    },
    information_elements as ie, pdu_definitions as pdu,
};

pub const BCH_BITS: usize = 246;
pub const BCH_BYTES: usize = BCH_BITS.div_ceil(8);
pub const FIXED_SIB_BITS: usize = 226;
pub const FIXED_SIB_BYTES: usize = FIXED_SIB_BITS.div_ceil(8);
pub const SHORT_SIB_BITS: usize = 214;
pub const SHORT_SIB_BYTES: usize = SHORT_SIB_BITS.div_ceil(8);
pub const SEGMENT_BITS: usize = 222;
pub const SEGMENT_BYTES: usize = SEGMENT_BITS.div_ceil(8);
const BYTE_BITS: usize = 8;
const SYSTEM_FRAME_MODULUS: u16 = 4096;
pub const MAX_UPLINK_COMMON_BITS: usize = 8192;
pub const MAX_DEDICATED_BITS: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedRrc {
    pub bytes: Vec<u8>,
    pub bit_length: usize,
}

pub fn encode_paging(value: &PCCHMessage) -> Result<EncodedRrc, CodecError> {
    if !matches!(value.message, PCCHMessageType::pagingType1(_)) {
        return Err(CodecError::UnsupportedPayload);
    }
    let bytes = rasn::uper::encode(value).map_err(|_| CodecError::Encode)?;
    let (decoded, bit_length) = decode::<PCCHMessage>(&bytes, bytes.len() * BYTE_BITS)?;
    if decoded != *value {
        return Err(CodecError::Constraints);
    }
    Ok(EncodedRrc { bytes, bit_length })
}

#[derive(Debug)]
pub struct DecodedDownlinkCommon {
    pub value: Box<DLCCCHMessage>,
    pub consumed_bits: usize,
}

#[derive(Debug)]
pub struct DecodedUplinkDedicated {
    pub value: Box<ULDCCHMessage>,
    pub consumed_bits: usize,
}

#[derive(Debug)]
pub struct DecodedDownlinkDedicated {
    pub value: Box<DLDCCHMessage>,
    pub consumed_bits: usize,
}

pub fn decode_downlink_dedicated(
    bytes: &[u8],
    bit_length: usize,
) -> Result<DecodedDownlinkDedicated, CodecError> {
    bounded_input(bytes, bit_length, MAX_DEDICATED_BITS)?;
    let (value, consumed_bits) = decode::<DLDCCHMessage>(bytes, bit_length)?;
    if matches!(value.message, DLDCCHMessageType::spare1(_)) {
        return Err(CodecError::UnsupportedPayload);
    }
    Ok(DecodedDownlinkDedicated {
        value: Box::new(value),
        consumed_bits,
    })
}

pub fn encode_downlink_dedicated(value: &DLDCCHMessage) -> Result<EncodedRrc, CodecError> {
    let bytes = rasn::uper::encode(value).map_err(|_| CodecError::Encode)?;
    let decoded = decode_downlink_dedicated(&bytes, bytes.len() * BYTE_BITS)?;
    if decoded.value.as_ref() != value {
        return Err(CodecError::Constraints);
    }
    Ok(EncodedRrc {
        bytes,
        bit_length: decoded.consumed_bits,
    })
}

fn bounded_input(bytes: &[u8], length: usize, maximum: usize) -> Result<(), CodecError> {
    if length == 0 || length > maximum || bytes.len() != length.div_ceil(BYTE_BITS) {
        return Err(CodecError::InvalidBitLength);
    }
    Ok(())
}

pub fn decode_downlink_common(
    bytes: &[u8],
    bit_length: usize,
) -> Result<DecodedDownlinkCommon, CodecError> {
    bounded_input(bytes, bit_length, MAX_UPLINK_COMMON_BITS)?;
    let (value, consumed_bits) = decode::<DLCCCHMessage>(bytes, bit_length)?;
    match &value.message {
        DLCCCHMessageType::rrcConnectionSetup(_)
        | DLCCCHMessageType::rrcConnectionReject(_)
        | DLCCCHMessageType::rrcConnectionRelease(_)
            if value.integrity_check_info.is_some() =>
        {
            return Err(CodecError::InvalidRrcValue);
        }
        DLCCCHMessageType::spare1(_) | DLCCCHMessageType::spare2(_) => {
            return Err(CodecError::UnsupportedPayload);
        }
        _ => {}
    }
    Ok(DecodedDownlinkCommon {
        value: Box::new(value),
        consumed_bits,
    })
}

pub fn encode_downlink_common(value: &DLCCCHMessage) -> Result<EncodedRrc, CodecError> {
    let bytes = rasn::uper::encode(value).map_err(|_| CodecError::Encode)?;
    let decoded = decode_downlink_common(&bytes, bytes.len() * BYTE_BITS)?;
    if decoded.value.as_ref() != value {
        return Err(CodecError::Constraints);
    }
    let bit_length = decoded.consumed_bits;
    Ok(EncodedRrc { bytes, bit_length })
}

pub fn decode_uplink_dedicated(
    bytes: &[u8],
    bit_length: usize,
) -> Result<DecodedUplinkDedicated, CodecError> {
    bounded_input(bytes, bit_length, MAX_DEDICATED_BITS)?;
    let (value, consumed_bits) = decode::<ULDCCHMessage>(bytes, bit_length)?;
    match &value.message {
        // TS 25.331 RRC CONNECTION SETUP COMPLETE forbids integrity protection on this message.
        ULDCCHMessageType::rrcConnectionSetupComplete(_)
            if value.integrity_check_info.is_some() =>
        {
            return Err(CodecError::InvalidRrcValue);
        }
        ULDCCHMessageType::ul_DCCH_MessageType_ext(extension)
            if !matches!(extension, ULDCCHMessageTypeExt::ueInformationResponse(_)) =>
        {
            return Err(CodecError::UnsupportedPayload);
        }
        _ => {}
    }
    Ok(DecodedUplinkDedicated {
        value: Box::new(value),
        consumed_bits,
    })
}

#[derive(Debug)]
pub struct DecodedUplinkCommon {
    pub value: Box<ULCCCHMessage>,
    pub consumed_bits: usize,
}

pub fn decode_uplink_common(
    bytes: &[u8],
    bit_length: usize,
) -> Result<DecodedUplinkCommon, CodecError> {
    if bit_length == 0
        || bit_length > MAX_UPLINK_COMMON_BITS
        || bytes.len() != bit_length.div_ceil(BYTE_BITS)
    {
        return Err(CodecError::InvalidBitLength);
    }
    let (value, consumed_bits) = decode::<ULCCCHMessage>(bytes, bit_length)?;
    match &value.message {
        // TS 25.331 RRC CONNECTION REQUEST forbids integrity protection on this message.
        ULCCCHMessageType::rrcConnectionRequest(_) if value.integrity_check_info.is_some() => {
            return Err(CodecError::InvalidRrcValue);
        }
        ULCCCHMessageType::uL_CCCH_MessageType_r11(extension)
            if !matches!(extension, ULCCCHMessageTypeR11::cellUpdate(_)) =>
        {
            return Err(CodecError::UnsupportedPayload);
        }
        _ => {}
    }
    // TS 25.331 12.1.3 requires receivers to accept any extension/padding bit string after the basic production.
    Ok(DecodedUplinkCommon {
        value: Box::new(value),
        consumed_bits,
    })
}

#[cfg(test)]
mod common_control_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    InvalidSystemFrame,
    InvalidSibType,
    InvalidBitLength,
    NonzeroUnusedBits,
    Constraints,
    Encode,
    Decode,
    UnsupportedPayload,
    InvalidSegment,
    InvalidMasterInformation,
    InvalidSystemInformation,
    InvalidRrcValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RrcValueKind {
    SystemInformation(SibType),
    TransportFormatSet,
    TransportFormatCombinations,
    PrachSystemInformation,
}

pub fn validate_master_information(bytes: &[u8], bit_length: usize) -> Result<(), CodecError> {
    validate_system_information(SibType::MasterInformationBlock, bytes, bit_length)
}

pub fn validate_system_information(
    kind: SibType,
    bytes: &[u8],
    bit_length: usize,
) -> Result<(), CodecError> {
    validate_rrc_value(RrcValueKind::SystemInformation(kind), bytes, bit_length)
}

pub fn validate_rrc_value(
    kind: RrcValueKind,
    bytes: &[u8],
    bit_length: usize,
) -> Result<(), CodecError> {
    const MAX_VALUE_BITS: usize = 64 * 1024;
    let max_bits = match kind {
        RrcValueKind::SystemInformation(_) => 16 * SEGMENT_BITS,
        _ => MAX_VALUE_BITS,
    };
    if bit_length == 0 || bit_length > max_bits || bytes.len() != bit_length.div_ceil(BYTE_BITS) {
        return Err(CodecError::InvalidBitLength);
    }
    let valid = match kind {
        RrcValueKind::TransportFormatSet => canonical::<ie::TransportFormatSet>(bytes, bit_length),
        RrcValueKind::TransportFormatCombinations => canonical::<ie::TFCS>(bytes, bit_length),
        RrcValueKind::PrachSystemInformation => {
            canonical::<ie::PRACHSystemInformation>(bytes, bit_length)
        }
        RrcValueKind::SystemInformation(sib) => match sib {
            SibType::MasterInformationBlock => {
                canonical::<ie::MasterInformationBlock>(bytes, bit_length)
            }
            SibType::Type1 => canonical::<ie::SysInfoType1>(bytes, bit_length),
            SibType::Type3 => canonical::<ie::SysInfoType3>(bytes, bit_length),
            SibType::Type5 => canonical::<ie::SysInfoType5>(bytes, bit_length),
            SibType::Type7 => canonical::<ie::SysInfoType7>(bytes, bit_length),
            SibType::Type11 => canonical::<ie::SysInfoType11>(bytes, bit_length),
            _ => return Err(CodecError::UnsupportedPayload),
        },
    };
    if valid {
        return Ok(());
    }
    Err(match kind {
        RrcValueKind::SystemInformation(SibType::MasterInformationBlock) => {
            CodecError::InvalidMasterInformation
        }
        RrcValueKind::SystemInformation(_) => CodecError::InvalidSystemInformation,
        _ => CodecError::InvalidRrcValue,
    })
}

fn decode<T: Decode>(bytes: &[u8], bit_length: usize) -> Result<(T, usize), CodecError> {
    let input = BitStr::from_slice(bytes);
    let input = input
        .get(..bit_length)
        .ok_or(CodecError::InvalidBitLength)?;
    let mut decoder =
        rasn::uper::de::Decoder::<0, 0>::new(input, rasn::uper::de::DecoderOptions::unaligned());
    let value = T::decode(&mut decoder).map_err(|_| CodecError::Decode)?;
    Ok((value, bit_length - decoder.input().len()))
}

fn canonical<T: Decode + Encode>(bytes: &[u8], bit_length: usize) -> bool {
    decode::<T>(bytes, bit_length).is_ok_and(|(value, consumed)| {
        consumed == bit_length && rasn::uper::encode(&value).is_ok_and(|encoded| encoded == bytes)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SibType {
    MasterInformationBlock = 0,
    Type1 = 1,
    Type2 = 2,
    Type3 = 3,
    Type4 = 4,
    Type5 = 5,
    Type6 = 6,
    Type7 = 7,
    Dummy = 8,
    Dummy2 = 9,
    Dummy3 = 10,
    Type11 = 11,
    Type12 = 12,
    Type13 = 13,
    Type13_1 = 14,
    Type13_2 = 15,
    Type13_3 = 16,
    Type13_4 = 17,
    Type14 = 18,
    Type15 = 19,
    Type15_1 = 20,
    Type15_2 = 21,
    Type15_3 = 22,
    Type16 = 23,
    Type17 = 24,
    Type15_4 = 25,
    Type18 = 26,
    SchedulingBlock1 = 27,
    SchedulingBlock2 = 28,
    Type15_5 = 29,
    Type5bis = 30,
    Extension = 31,
}

impl SibType {
    pub fn new(value: u8) -> Result<Self, CodecError> {
        Ok(match value {
            0 => Self::MasterInformationBlock,
            1 => Self::Type1,
            2 => Self::Type2,
            3 => Self::Type3,
            4 => Self::Type4,
            5 => Self::Type5,
            6 => Self::Type6,
            7 => Self::Type7,
            8 => Self::Dummy,
            9 => Self::Dummy2,
            10 => Self::Dummy3,
            11 => Self::Type11,
            12 => Self::Type12,
            13 => Self::Type13,
            14 => Self::Type13_1,
            15 => Self::Type13_2,
            16 => Self::Type13_3,
            17 => Self::Type13_4,
            18 => Self::Type14,
            19 => Self::Type15,
            20 => Self::Type15_1,
            21 => Self::Type15_2,
            22 => Self::Type15_3,
            23 => Self::Type16,
            24 => Self::Type17,
            25 => Self::Type15_4,
            26 => Self::Type18,
            27 => Self::SchedulingBlock1,
            28 => Self::SchedulingBlock2,
            29 => Self::Type15_5,
            30 => Self::Type5bis,
            31 => Self::Extension,
            _ => return Err(CodecError::InvalidSibType),
        })
    }

    pub fn wire_value(self) -> u8 {
        self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixedSibData([u8; FIXED_SIB_BYTES]);

impl FixedSibData {
    // TS 25.331 CompleteSIB and 12.1.3 require zero padding up to 226 bits.
    pub fn padded(data: &[u8], bit_length: usize) -> Result<Self, CodecError> {
        if bit_length <= SHORT_SIB_BITS {
            return Err(CodecError::InvalidBitLength);
        }
        Ok(Self(padded_bits(data, bit_length, FIXED_SIB_BITS)?))
    }

    pub fn bytes(&self) -> &[u8; FIXED_SIB_BYTES] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortSibData {
    bytes: [u8; SHORT_SIB_BYTES],
    bit_length: usize,
}

impl ShortSibData {
    pub fn new(data: &[u8], bit_length: usize) -> Result<Self, CodecError> {
        if !(1..=SHORT_SIB_BITS).contains(&bit_length) {
            return Err(CodecError::InvalidBitLength);
        }
        Ok(Self {
            bytes: padded_bits(data, bit_length, SHORT_SIB_BITS)?,
            bit_length,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.bit_length.div_ceil(BYTE_BITS)]
    }

    pub fn bit_length(&self) -> usize {
        self.bit_length
    }

    fn from_bits(bits: &BitStr) -> Result<Self, CodecError> {
        if !(1..=SHORT_SIB_BITS).contains(&bits.len()) {
            return Err(CodecError::InvalidBitLength);
        }
        let mut bytes = [0; SHORT_SIB_BYTES];
        for (index, bit) in bits.iter().by_vals().enumerate() {
            bytes[index / BYTE_BITS] |= u8::from(bit) << (BYTE_BITS - 1 - index % BYTE_BITS);
        }
        Self::new(&bytes[..bits.len().div_ceil(BYTE_BITS)], bits.len())
    }
}

fn padded_bits<const N: usize>(
    data: &[u8],
    bit_length: usize,
    capacity_bits: usize,
) -> Result<[u8; N], CodecError> {
    if bit_length > capacity_bits
        || data.len() != bit_length.div_ceil(BYTE_BITS)
        || capacity_bits.div_ceil(BYTE_BITS) != N
    {
        return Err(CodecError::InvalidBitLength);
    }
    if !bit_length.is_multiple_of(BYTE_BITS)
        && data
            .last()
            .is_some_and(|byte| byte & ((1 << (BYTE_BITS - bit_length % BYTE_BITS)) - 1) != 0)
    {
        return Err(CodecError::NonzeroUnusedBits);
    }
    let mut output = [0; N];
    output[..data.len()].copy_from_slice(data);
    Ok(output)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixedSegmentData([u8; SEGMENT_BYTES]);

impl FixedSegmentData {
    pub fn padded(data: &[u8], bit_length: usize) -> Result<Self, CodecError> {
        Ok(Self(padded_bits(data, bit_length, SEGMENT_BITS)?))
    }

    pub fn bytes(&self) -> &[u8; SEGMENT_BYTES] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentPart {
    First { count: u8 },
    Subsequent { index: u8 },
    Last { index: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BchPayload {
    NoSegment,
    Complete {
        kind: SibType,
        data: FixedSibData,
    },
    CompleteShort {
        kind: SibType,
        data: ShortSibData,
    },
    LastShort {
        kind: SibType,
        index: u8,
        data: ShortSibData,
    },
    Segment {
        kind: SibType,
        part: SegmentPart,
        data: FixedSegmentData,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BchMessage {
    pub system_frame: u16,
    pub payload: BchPayload,
}

impl BchMessage {
    pub fn encode(&self) -> Result<[u8; BCH_BYTES], CodecError> {
        if self.system_frame >= SYSTEM_FRAME_MODULUS || !self.system_frame.is_multiple_of(2) {
            return Err(CodecError::InvalidSystemFrame);
        }
        let payload = match &self.payload {
            BchPayload::NoSegment => pdu::SystemInformationBCHPayload::noSegment(()),
            BchPayload::Complete { kind, data } => {
                pdu::SystemInformationBCHPayload::completeSIB(pdu::CompleteSIB {
                    sib_type: generated_sib_type(*kind),
                    sib_data_fixed: BitStr::from_slice(data.bytes())[..FIXED_SIB_BITS].to_bitvec(),
                })
            }
            BchPayload::CompleteShort { kind, data } => {
                pdu::SystemInformationBCHPayload::completeSIB_List(pdu::CompleteSIBList(vec![
                    pdu::CompleteSIBshort {
                        sib_type: generated_sib_type(*kind),
                        sib_data_variable: ie::SIBDataVariable(
                            BitStr::from_slice(data.bytes())[..data.bit_length()].to_bitvec(),
                        ),
                    },
                ]))
            }
            BchPayload::LastShort { kind, index, data } => {
                if !(1..=15).contains(index) {
                    return Err(CodecError::InvalidSegment);
                }
                pdu::SystemInformationBCHPayload::lastSegmentShort(pdu::LastSegmentShort {
                    sib_type: generated_sib_type(*kind),
                    segment_index: ie::SegmentIndex(*index),
                    sib_data_variable: ie::SIBDataVariable(
                        BitStr::from_slice(data.bytes())[..data.bit_length()].to_bitvec(),
                    ),
                })
            }
            BchPayload::Segment { kind, part, data } => {
                // rasn FixedBitString<N> stores N bytes but encodes only the leading N bits.
                let mut bits = FixedBitString::<SEGMENT_BITS>::ZERO;
                bits[..SEGMENT_BITS]
                    .copy_from_bitslice(&BitStr::from_slice(data.bytes())[..SEGMENT_BITS]);
                let kind = generated_sib_type(*kind);
                let bits = ie::SIBDataFixed(bits);
                match *part {
                    SegmentPart::First { count } if (1..=16).contains(&count) => {
                        pdu::SystemInformationBCHPayload::firstSegment(pdu::FirstSegment {
                            sib_type: kind,
                            seg_count: ie::SegCount(count),
                            sib_data_fixed: bits,
                        })
                    }
                    SegmentPart::Subsequent { index } if (1..=15).contains(&index) => {
                        pdu::SystemInformationBCHPayload::subsequentSegment(
                            pdu::SubsequentSegment {
                                sib_type: kind,
                                segment_index: ie::SegmentIndex(index),
                                sib_data_fixed: bits,
                            },
                        )
                    }
                    SegmentPart::Last { index } if (1..=15).contains(&index) => {
                        pdu::SystemInformationBCHPayload::lastSegment(pdu::LastSegment {
                            sib_type: kind,
                            segment_index: ie::SegmentIndex(index),
                            sib_data_fixed: bits,
                        })
                    }
                    _ => return Err(CodecError::InvalidSegment),
                }
            }
        };
        let value = BCCHBCHMessage {
            message: pdu::SystemInformationBCH {
                sfn_prime: ie::SFNPrime(self.system_frame / 2),
                payload,
            },
        };
        let encoded = rasn::uper::encode(&value).map_err(|_| CodecError::Encode)?;
        if encoded.len() > BCH_BYTES {
            return Err(CodecError::Encode);
        }
        let mut output = [0; BCH_BYTES];
        output[..encoded.len()].copy_from_slice(&encoded);
        Ok(output)
    }

    pub fn decode(input: &[u8; BCH_BYTES]) -> Result<Self, CodecError> {
        // TS 25.331 12.1.3 permits arbitrary padding after the decoded BCH value.
        let (value, _) = decode::<BCCHBCHMessage>(input, BCH_BITS)?;
        let system_frame = value
            .message
            .sfn_prime
            .0
            .checked_mul(2)
            .filter(|&frame| frame < SYSTEM_FRAME_MODULUS)
            .ok_or(CodecError::InvalidSystemFrame)?;
        let payload = match value.message.payload {
            pdu::SystemInformationBCHPayload::noSegment(()) => BchPayload::NoSegment,
            pdu::SystemInformationBCHPayload::completeSIB(sib) => {
                let data = pack_bits::<FIXED_SIB_BYTES>(&sib.sib_data_fixed, FIXED_SIB_BITS)?;
                BchPayload::Complete {
                    kind: SibType::new(sib.sib_type as u8)?,
                    data: FixedSibData(data),
                }
            }
            pdu::SystemInformationBCHPayload::completeSIB_List(mut list) => {
                if list.0.len() != 1 {
                    return Err(CodecError::UnsupportedPayload);
                }
                let sib = list.0.pop().expect("one complete short SIB");
                BchPayload::CompleteShort {
                    kind: SibType::new(sib.sib_type as u8)?,
                    data: ShortSibData::from_bits(&sib.sib_data_variable.0)?,
                }
            }
            pdu::SystemInformationBCHPayload::lastSegmentShort(segment) => BchPayload::LastShort {
                kind: SibType::new(segment.sib_type as u8)?,
                index: segment.segment_index.0,
                data: ShortSibData::from_bits(&segment.sib_data_variable.0)?,
            },
            pdu::SystemInformationBCHPayload::firstSegment(segment) => BchPayload::Segment {
                kind: SibType::new(segment.sib_type as u8)?,
                part: SegmentPart::First {
                    count: segment.seg_count.0,
                },
                data: FixedSegmentData(pack_bits::<SEGMENT_BYTES>(
                    &segment.sib_data_fixed.0[..SEGMENT_BITS],
                    SEGMENT_BITS,
                )?),
            },
            pdu::SystemInformationBCHPayload::subsequentSegment(segment) => BchPayload::Segment {
                kind: SibType::new(segment.sib_type as u8)?,
                part: SegmentPart::Subsequent {
                    index: segment.segment_index.0,
                },
                data: FixedSegmentData(pack_bits::<SEGMENT_BYTES>(
                    &segment.sib_data_fixed.0[..SEGMENT_BITS],
                    SEGMENT_BITS,
                )?),
            },
            pdu::SystemInformationBCHPayload::lastSegment(segment) => BchPayload::Segment {
                kind: SibType::new(segment.sib_type as u8)?,
                part: SegmentPart::Last {
                    index: segment.segment_index.0,
                },
                data: FixedSegmentData(pack_bits::<SEGMENT_BYTES>(
                    &segment.sib_data_fixed.0[..SEGMENT_BITS],
                    SEGMENT_BITS,
                )?),
            },
            _ => return Err(CodecError::UnsupportedPayload),
        };
        Ok(Self {
            system_frame,
            payload,
        })
    }
}

fn pack_bits<const N: usize>(bits: &BitStr, expected: usize) -> Result<[u8; N], CodecError> {
    if bits.len() != expected || expected.div_ceil(BYTE_BITS) != N {
        return Err(CodecError::Decode);
    }
    let mut output = [0; N];
    for (index, bit) in bits.iter().by_vals().enumerate() {
        output[index / BYTE_BITS] |= u8::from(bit) << (BYTE_BITS - 1 - index % BYTE_BITS);
    }
    Ok(output)
}

fn generated_sib_type(kind: SibType) -> ie::SIBType {
    use ie::SIBType::*;
    const TYPES: [ie::SIBType; 32] = [
        masterInformationBlock,
        systemInformationBlockType1,
        systemInformationBlockType2,
        systemInformationBlockType3,
        systemInformationBlockType4,
        systemInformationBlockType5,
        systemInformationBlockType6,
        systemInformationBlockType7,
        dummy,
        dummy2,
        dummy3,
        systemInformationBlockType11,
        systemInformationBlockType12,
        systemInformationBlockType13,
        systemInformationBlockType13_1,
        systemInformationBlockType13_2,
        systemInformationBlockType13_3,
        systemInformationBlockType13_4,
        systemInformationBlockType14,
        systemInformationBlockType15,
        systemInformationBlockType15_1,
        systemInformationBlockType15_2,
        systemInformationBlockType15_3,
        systemInformationBlockType16,
        systemInformationBlockType17,
        systemInformationBlockType15_4,
        systemInformationBlockType18,
        schedulingBlock1,
        schedulingBlock2,
        systemInformationBlockType15_5,
        systemInformationBlockType5bis,
        extensionType,
    ];
    TYPES[usize::from(kind.wire_value())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_information_validation_uses_exact_bits_not_octet_padding() {
        let mib = [0x00, 0x00, 0x40, 0x20, 0x30, 0x28];
        validate_master_information(&mib, 45).unwrap();
        for bits in [44, 46, 47, 48] {
            assert_eq!(
                validate_master_information(&mib, bits),
                Err(CodecError::InvalidMasterInformation)
            );
        }
        let mut nonzero_unused = mib;
        nonzero_unused[5] |= 1;
        assert_eq!(
            validate_master_information(&nonzero_unused, 45),
            Err(CodecError::InvalidMasterInformation)
        );
    }

    #[test]
    fn every_sib_identifier_has_the_same_generated_wire_value() {
        for wire in 0..32 {
            let kind = SibType::new(wire).unwrap();
            assert_eq!(generated_sib_type(kind) as u8, wire);
            let message = BchMessage {
                system_frame: 4094,
                payload: BchPayload::Complete {
                    kind,
                    data: FixedSibData::padded(&[0; SHORT_SIB_BYTES], SHORT_SIB_BITS + 1).unwrap(),
                },
            };
            assert_eq!(
                BchMessage::decode(&message.encode().unwrap()).unwrap(),
                message
            );
        }
    }

    #[test]
    fn invalid_constrained_segment_index_is_rejected_on_decode() {
        let message = BchMessage {
            system_frame: 0,
            payload: BchPayload::Segment {
                kind: SibType::Type1,
                part: SegmentPart::Subsequent { index: 15 },
                data: FixedSegmentData::padded(&[], 0).unwrap(),
            },
        };
        let mut input = message.encode().unwrap();
        // The constrained index occupies bits 20..23 and the code 15 would mean invalid index 16.
        input[2] |= 0x0f;
        assert_eq!(BchMessage::decode(&input), Err(CodecError::Decode));
    }

    #[test]
    fn bch_decoder_reads_246_bits_and_never_the_octet_tail() {
        let message = BchMessage {
            system_frame: 0,
            payload: BchPayload::Complete {
                kind: SibType::Type1,
                data: FixedSibData::padded(&[0; SHORT_SIB_BYTES], SHORT_SIB_BITS + 1).unwrap(),
            },
        };
        let mut input = message.encode().unwrap();
        assert!(decode::<BCCHBCHMessage>(&input, BCH_BITS - 1).is_err());
        input[BCH_BYTES - 1] |= 0x03;
        assert_eq!(BchMessage::decode(&input).unwrap(), message);
    }

    #[test]
    fn master_information_boundary_rejects_truncated_and_unbounded_inputs() {
        assert_eq!(
            validate_master_information(&[], 0),
            Err(CodecError::InvalidBitLength)
        );
        assert_eq!(
            validate_master_information(&[0], 1),
            Err(CodecError::InvalidMasterInformation)
        );
        assert_eq!(
            validate_master_information(&[0; 500], 4000),
            Err(CodecError::InvalidBitLength)
        );
    }

    #[test]
    fn no_segment_matches_independent_eleven_bit_sfn_and_four_bit_choice() {
        for frame in [0, 2, 14, 2048, 4094] {
            let message = BchMessage {
                system_frame: frame,
                payload: BchPayload::NoSegment,
            };
            let encoded = message.encode().unwrap();
            let mut expected = [0; BCH_BYTES];
            let header = (frame / 2) << 5;
            expected[..2].copy_from_slice(&header.to_be_bytes());
            assert_eq!(encoded, expected);
            assert_eq!(BchMessage::decode(&expected).unwrap(), message);
        }
    }

    #[test]
    fn complete_sib_matches_independent_unaligned_bit_positions() {
        for kind in [0, 1, 5, 27, 31] {
            let mut data = [0xa5; FIXED_SIB_BYTES];
            data[FIXED_SIB_BYTES - 1] &= 0xc0;
            let message = BchMessage {
                system_frame: 42,
                payload: BchPayload::Complete {
                    kind: SibType::new(kind).unwrap(),
                    data: FixedSibData::padded(&data, FIXED_SIB_BITS).unwrap(),
                },
            };
            let actual = message.encode().unwrap();
            let mut bits = Vec::new();
            for (value, width) in [(21_u32, 11), (9, 4), (u32::from(kind), 5)] {
                bits.extend((0..width).rev().map(|i| ((value >> i) & 1) as u8));
            }
            bits.extend((0..FIXED_SIB_BITS).map(|i| (data[i / 8] >> (7 - i % 8)) & 1));
            let mut expected = [0; BCH_BYTES];
            for (i, bit) in bits.iter().enumerate() {
                expected[i / 8] |= bit << (7 - i % 8);
            }
            assert_eq!(actual, expected);
            assert_eq!(BchMessage::decode(&expected).unwrap(), message);
        }
    }

    #[test]
    fn short_sib_choices_match_independent_length_and_payload_bits() {
        for length in [1_usize, SHORT_SIB_BITS] {
            let mut bytes = vec![0xa5; length.div_ceil(BYTE_BITS)];
            if !length.is_multiple_of(BYTE_BITS) {
                *bytes.last_mut().unwrap() &= 0xff << (BYTE_BITS - length % BYTE_BITS);
            }
            for last in [false, true] {
                let data = ShortSibData::new(&bytes, length).unwrap();
                assert_eq!(data.bytes(), bytes);
                assert_eq!(data.bit_length(), length);
                let message = BchMessage {
                    system_frame: 42,
                    payload: if last {
                        BchPayload::LastShort {
                            kind: SibType::Type5,
                            index: 15,
                            data,
                        }
                    } else {
                        BchPayload::CompleteShort {
                            kind: SibType::Type5,
                            data,
                        }
                    },
                };
                // TS 25.331 ASN.1 uses choice 7 plus a four-bit list count, or choice 3 plus segment index.
                let fields = if last {
                    vec![(21_usize, 11), (3, 4), (5, 5), (14, 4), (length - 1, 8)]
                } else {
                    vec![(21_usize, 11), (7, 4), (0, 4), (5, 5), (length - 1, 8)]
                };
                let mut bits = Vec::new();
                for (value, width) in fields {
                    bits.extend((0..width).rev().map(|index| ((value >> index) & 1) as u8));
                }
                assert_eq!(bits.len(), 32);
                bits.extend(
                    (0..length)
                        .map(|index| (bytes[index / BYTE_BITS] >> (7 - index % BYTE_BITS)) & 1),
                );
                let mut expected = [0; BCH_BYTES];
                for (index, bit) in bits.into_iter().enumerate() {
                    expected[index / BYTE_BITS] |= bit << (7 - index % BYTE_BITS);
                }
                assert_eq!(message.encode().unwrap(), expected);
                assert_eq!(BchMessage::decode(&expected).unwrap(), message);
                let mut invalid_length = expected;
                invalid_length[3] = SHORT_SIB_BITS as u8;
                assert_eq!(BchMessage::decode(&invalid_length), Err(CodecError::Decode));
                if last {
                    let mut invalid_index = expected;
                    invalid_index[2] |= 0x0f;
                    assert_eq!(BchMessage::decode(&invalid_index), Err(CodecError::Decode));
                }
            }
        }
    }

    #[test]
    fn complete_and_segment_size_boundaries_are_not_interchangeable() {
        for length in [1_usize, 214, 215, 222, 226, 227] {
            let bytes = vec![0; length.div_ceil(BYTE_BITS)];
            assert_eq!(
                ShortSibData::new(&bytes, length).is_ok(),
                length <= SHORT_SIB_BITS
            );
            assert_eq!(
                FixedSibData::padded(&bytes, length).is_ok(),
                (215..=FIXED_SIB_BITS).contains(&length)
            );
            assert_eq!(
                FixedSegmentData::padded(&bytes, length).is_ok(),
                length <= SEGMENT_BITS
            );
        }
        assert_eq!(ShortSibData::new(&[], 0), Err(CodecError::InvalidBitLength));
        assert_eq!(ShortSibData::new(&[], 1), Err(CodecError::InvalidBitLength));
        assert_eq!(
            ShortSibData::new(&[0; 2], 1),
            Err(CodecError::InvalidBitLength)
        );
        assert_eq!(
            ShortSibData::new(&[0x81], 1),
            Err(CodecError::NonzeroUnusedBits)
        );
        let mut unused = [0; SHORT_SIB_BYTES];
        unused[SHORT_SIB_BYTES - 1] = 1;
        assert_eq!(
            ShortSibData::new(&unused, SHORT_SIB_BITS),
            Err(CodecError::NonzeroUnusedBits)
        );
        for index in [0, 16] {
            assert_eq!(
                BchMessage {
                    system_frame: 0,
                    payload: BchPayload::LastShort {
                        kind: SibType::Type1,
                        index,
                        data: ShortSibData::new(&[0x80], 1).unwrap()
                    },
                }
                .encode(),
                Err(CodecError::InvalidSegment)
            );
        }
    }

    #[test]
    fn short_sib_decode_preserves_length_and_accepts_arbitrary_outer_padding() {
        let message = BchMessage {
            system_frame: 24,
            payload: BchPayload::CompleteShort {
                kind: SibType::Type1,
                data: ShortSibData::new(&[0x80], 1).unwrap(),
            },
        };
        let mut encoded = message.encode().unwrap();
        encoded[4] |= 0x7f;
        encoded[5..].fill(0xff);
        assert_eq!(BchMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn invalid_values_and_unsupported_payloads_are_explicit() {
        for frame in [1, 4095, 4096] {
            assert_eq!(
                BchMessage {
                    system_frame: frame,
                    payload: BchPayload::NoSegment
                }
                .encode(),
                Err(CodecError::InvalidSystemFrame)
            );
        }
        assert_eq!(SibType::new(32), Err(CodecError::InvalidSibType));
        assert_eq!(
            ShortSibData::new(&[1], 1),
            Err(CodecError::NonzeroUnusedBits)
        );
        assert_eq!(
            FixedSibData::padded(&[], 1),
            Err(CodecError::InvalidBitLength)
        );
        let mut input = [0; BCH_BYTES];
        input[1] = 0x16;
        assert_eq!(
            BchMessage::decode(&input),
            Err(CodecError::UnsupportedPayload)
        );
    }

    #[test]
    fn arbitrary_bch_payloads_are_bounded_and_reject_invalid_encodings() {
        let mut seed = 0x12345678_u32;
        let mut failures = 0;
        for _ in 0..512 {
            let input = std::array::from_fn(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            });
            if BchMessage::decode(&input) == Err(CodecError::Decode) {
                failures += 1;
            }
        }
        assert!(failures > 0);
    }

    #[test]
    fn decoder_accepts_arbitrary_padding_as_required_by_rrc() {
        let message = BchMessage {
            system_frame: 24,
            payload: BchPayload::NoSegment,
        };
        let mut encoded = message.encode().unwrap();
        encoded[2..].fill(0xff);
        assert_eq!(BchMessage::decode(&encoded).unwrap(), message);
    }

    #[test]
    fn concurrent_codec_calls_are_independent() {
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                std::thread::spawn(move || {
                    for frame in (worker * 100..worker * 100 + 100).step_by(2) {
                        let message = BchMessage {
                            system_frame: frame,
                            payload: BchPayload::NoSegment,
                        };
                        assert_eq!(
                            BchMessage::decode(&message.encode().unwrap()).unwrap(),
                            message
                        );
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn fixed_segments_match_independent_choice_and_constrained_integer_bits() {
        for (part, choice, field) in [
            (SegmentPart::First { count: 16 }, 1_u32, 15_u32),
            (SegmentPart::Subsequent { index: 7 }, 2, 6),
            (SegmentPart::Last { index: 15 }, 10, 14),
        ] {
            let mut bytes = [0xa6; SEGMENT_BYTES];
            bytes[SEGMENT_BYTES - 1] &= 0xfc;
            let message = BchMessage {
                system_frame: 42,
                payload: BchPayload::Segment {
                    kind: SibType::new(5).unwrap(),
                    part,
                    data: FixedSegmentData::padded(&bytes, SEGMENT_BITS).unwrap(),
                },
            };
            let mut bits = Vec::new();
            for (value, width) in [(21_u32, 11), (choice, 4), (5, 5), (field, 4)] {
                bits.extend((0..width).rev().map(|i| ((value >> i) & 1) as u8));
            }
            bits.extend((0..SEGMENT_BITS).map(|i| (bytes[i / 8] >> (7 - i % 8)) & 1));
            let mut expected = [0; BCH_BYTES];
            for (i, bit) in bits.iter().enumerate() {
                expected[i / 8] |= bit << (7 - i % 8);
            }
            assert_eq!(message.encode().unwrap(), expected);
            assert_eq!(BchMessage::decode(&expected).unwrap(), message);
        }
    }

    #[test]
    fn segment_indices_and_counts_are_checked_before_encoding() {
        for part in [
            SegmentPart::First { count: 0 },
            SegmentPart::First { count: 17 },
            SegmentPart::Subsequent { index: 0 },
            SegmentPart::Last { index: 16 },
        ] {
            let message = BchMessage {
                system_frame: 0,
                payload: BchPayload::Segment {
                    kind: SibType::new(1).unwrap(),
                    part,
                    data: FixedSegmentData::padded(&[], 0).unwrap(),
                },
            };
            assert_eq!(message.encode(), Err(CodecError::InvalidSegment));
        }
    }
}
