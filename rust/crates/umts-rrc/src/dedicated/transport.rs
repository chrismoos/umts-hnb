// SPDX-License-Identifier: AGPL-3.0-or-later

use super::DedicatedError;
use crate::transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval};
use std::collections::BTreeSet;
use umts_rrc_schema::information_elements as ie;

const MAX_TRANSPORTS: usize = 32;
const MAX_FORMATS: usize = 32;
const MAX_COMBINATIONS: usize = 1024;
const MAX_RLC_BITS: u16 = 5000;
const MAX_BLOCKS: u16 = 512;
const MAX_ATTRIBUTE: u16 = 256;
const MAX_CTFC: u32 = (1 << 24) - 1;
const MAX_GAIN: u8 = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DchDirection {
    Uplink,
    Downlink,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DchFormat {
    pub rlc_bits: u16,
    pub blocks: u16,
}
impl DchFormat {
    // TS 25.331 8.6.5.1: zero-size transport includes parity only, not a MAC header.
    pub fn transport_bits(self, multiplexed: bool) -> usize {
        if self.rlc_bits == 0 {
            0
        } else {
            usize::from(self.rlc_bits) + if multiplexed { 4 } else { 0 }
        }
    }
    fn empty(self) -> bool {
        self.blocks == 0 || self.rlc_bits == 0
    }
}
#[derive(Clone, Debug)]
pub struct DchFormatGroup {
    pub rlc_bits: u16,
    pub blocks: Vec<u16>,
}
#[derive(Clone, Debug)]
pub struct DchTransport {
    pub id: u8,
    pub interval: TransportInterval,
    pub coding: ChannelCoding,
    pub crc: CrcSize,
    pub rate_matching_attribute: u16,
    pub groups: Vec<DchFormatGroup>,
}
impl DchTransport {
    pub fn formats(&self) -> impl Iterator<Item = DchFormat> + '_ {
        self.groups.iter().flat_map(|group| {
            group.blocks.iter().map(|&blocks| DchFormat {
                rlc_bits: group.rlc_bits,
                blocks,
            })
        })
    }
    pub fn frames(&self) -> usize {
        match self.interval {
            TransportInterval::Ms10 => 1,
            TransportInterval::Ms20 => 2,
            TransportInterval::Ms40 => 4,
            TransportInterval::Ms80 => 8,
        }
    }
    fn validate(&self) -> Result<(), DedicatedError> {
        if self.id == 0
            || usize::from(self.id) > MAX_TRANSPORTS
            || !(1..=MAX_ATTRIBUTE).contains(&self.rate_matching_attribute)
        {
            return Err(DedicatedError::Transport);
        }
        if self.groups.is_empty() || self.groups.len() > MAX_FORMATS {
            return Err(DedicatedError::Format);
        }
        let mut sizes = BTreeSet::new();
        let mut formats = 0;
        for group in &self.groups {
            if group.rlc_bits > MAX_RLC_BITS
                || !sizes.insert(group.rlc_bits)
                || group.blocks.is_empty()
                || group.blocks.len() > MAX_FORMATS
            {
                return Err(DedicatedError::Format);
            }
            let mut counts = BTreeSet::new();
            for &blocks in &group.blocks {
                if blocks > MAX_BLOCKS || !counts.insert(blocks) {
                    return Err(DedicatedError::Format);
                }
                formats += 1;
                if formats > MAX_FORMATS {
                    return Err(DedicatedError::Capacity);
                }
            }
        }
        Ok(())
    }
    fn wire_direction(
        &self,
        direction: DchDirection,
    ) -> Result<ie::TransportFormatSet, DedicatedError> {
        self.validate()?;
        let logical_channels = match direction {
            DchDirection::Uplink => ie::LogicalChannelList::configured(()),
            DchDirection::Downlink => ie::LogicalChannelList::allSizes(()),
        };
        let groups = ie::DedicatedDynamicTFInfoList(
            self.groups
                .iter()
                .map(|group| {
                    ie::DedicatedDynamicTFInfo::new(
                        ie::DedicatedDynamicTFInfoRlcSize::bitMode(bit_size(group.rlc_bits)),
                        group.blocks.iter().copied().map(blocks).collect(),
                        logical_channels.clone(),
                    )
                })
                .collect(),
        );
        let tti = match self.interval {
            TransportInterval::Ms10 => ie::DedicatedTransChTFSTti::tti10(groups),
            TransportInterval::Ms20 => ie::DedicatedTransChTFSTti::tti20(groups),
            TransportInterval::Ms40 => ie::DedicatedTransChTFSTti::tti40(groups),
            TransportInterval::Ms80 => ie::DedicatedTransChTFSTti::tti80(groups),
        };
        let coding = match self.coding {
            ChannelCoding::Convolutional(ConvolutionalRate::Half) => {
                ie::ChannelCodingType::convolutional(ie::CodingRate::half)
            }
            ChannelCoding::Convolutional(ConvolutionalRate::Third) => {
                ie::ChannelCodingType::convolutional(ie::CodingRate::third)
            }
            ChannelCoding::Turbo => ie::ChannelCodingType::turbo(()),
        };
        let crc = match self.crc {
            CrcSize::None => ie::CRCSize::crc0,
            CrcSize::Bits8 => ie::CRCSize::crc8,
            CrcSize::Bits12 => ie::CRCSize::crc12,
            CrcSize::Bits16 => ie::CRCSize::crc16,
            CrcSize::Bits24 => ie::CRCSize::crc24,
        };
        Ok(ie::TransportFormatSet::dedicatedTransChTFS(
            ie::DedicatedTransChTFS::new(
                tti,
                ie::SemistaticTFInformation::new(
                    coding,
                    ie::RateMatchingAttribute(self.rate_matching_attribute),
                    crc,
                ),
            ),
        ))
    }

    pub fn wire_uplink(&self) -> Result<ie::TransportFormatSet, DedicatedError> {
        self.wire_direction(DchDirection::Uplink)
    }

    pub fn wire_downlink(&self) -> Result<ie::TransportFormatSet, DedicatedError> {
        self.wire_direction(DchDirection::Downlink)
    }
}

fn bit_size(bits: u16) -> ie::BitModeRLCSizeInfo {
    if bits < 128 {
        return ie::BitModeRLCSizeInfo::sizeType1(bits as u8);
    }
    let (base, step) = if bits < 256 {
        (128, 8)
    } else if bits < 1024 {
        (256, 16)
    } else {
        (1024, 64)
    };
    let part1 = ((bits - base) / step) as u8;
    let remainder = ((bits - base) % step) as u8;
    let part2 = (remainder != 0).then_some(remainder);
    match base {
        128 => {
            ie::BitModeRLCSizeInfo::sizeType2(ie::BitModeRLCSizeInfoSizeType2::new(part1, part2))
        }
        256 => {
            ie::BitModeRLCSizeInfo::sizeType3(ie::BitModeRLCSizeInfoSizeType3::new(part1, part2))
        }
        _ => ie::BitModeRLCSizeInfo::sizeType4(ie::BitModeRLCSizeInfoSizeType4::new(part1, part2)),
    }
}
fn blocks(value: u16) -> ie::NumberOfTransportBlocks {
    match value {
        0 => ie::NumberOfTransportBlocks::zero(()),
        1 => ie::NumberOfTransportBlocks::one(()),
        2..=17 => ie::NumberOfTransportBlocks::small(value as u8),
        _ => ie::NumberOfTransportBlocks::large(value),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GainFactors {
    pub control: u8,
    pub data: u8,
}
impl GainFactors {
    fn wire(self) -> Result<ie::PowerOffsetInformation, DedicatedError> {
        // TS 25.213 4.2.1 quantizes normalized beta factors with at least one factor equal to one.
        if self.control == 0
            || self.control > MAX_GAIN
            || self.data > MAX_GAIN
            || self.control.max(self.data) != MAX_GAIN
        {
            return Err(DedicatedError::Gains);
        }
        Ok(ie::PowerOffsetInformation::new(
            ie::GainFactorInformation::signalledGainFactors(ie::SignalledGainFactors::new(
                ie::SignalledGainFactorsModeSpecificInfo::fdd(
                    ie::SignalledGainFactorsModeSpecificInfoFdd::new(ie::GainFactor(self.control)),
                ),
                ie::GainFactor(self.data),
                None,
            )),
            None,
        ))
    }
}
#[derive(Clone, Debug)]
pub struct Combination {
    pub formats: Vec<usize>,
    pub gain: Option<GainFactors>,
}
pub struct DchTfcs {
    direction: DchDirection,
    transports: Vec<DchTransport>,
    combinations: Vec<Combination>,
    ctfc: Vec<u32>,
}
impl DchTfcs {
    pub fn new(
        direction: DchDirection,
        transports: Vec<DchTransport>,
        combinations: Vec<Combination>,
    ) -> Result<Self, DedicatedError> {
        if transports.is_empty()
            || transports.len() > MAX_TRANSPORTS
            || combinations.is_empty()
            || combinations.len() > MAX_COMBINATIONS
            || transports.windows(2).any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(DedicatedError::Capacity);
        }
        for transport in &transports {
            transport.validate()?;
        }
        let formats: Vec<Vec<_>> = transports
            .iter()
            .map(|transport| transport.formats().collect())
            .collect();
        let mut ctfc = Vec::with_capacity(combinations.len());
        let mut seen = BTreeSet::new();
        for combination in &combinations {
            if combination.formats.len() != transports.len() {
                return Err(DedicatedError::Combination);
            }
            match (direction, combination.gain) {
                (DchDirection::Uplink, Some(gain)) => {
                    gain.wire()?;
                }
                (DchDirection::Downlink, None) => {}
                _ => return Err(DedicatedError::Gains),
            }
            if combination.gain.is_some_and(|gain| gain.data == 0)
                && combination
                    .formats
                    .iter()
                    .zip(&formats)
                    .zip(&transports)
                    .any(|((&index, formats), transport)| {
                        formats.get(index).is_some_and(|format| {
                            format.blocks != 0
                                && (format.rlc_bits != 0 || !matches!(transport.crc, CrcSize::None))
                        })
                    })
            {
                return Err(DedicatedError::Gains);
            }
            let mut value = 0u32;
            for (&index, formats) in combination.formats.iter().zip(&formats).rev() {
                if index >= formats.len() {
                    return Err(DedicatedError::Combination);
                }
                value = value
                    .checked_mul(formats.len() as u32)
                    .and_then(|value| value.checked_add(index as u32))
                    .filter(|value| *value <= MAX_CTFC)
                    .ok_or(DedicatedError::Combination)?;
            }
            if !seen.insert(value) {
                return Err(DedicatedError::Combination);
            }
            ctfc.push(value);
        }
        if direction == DchDirection::Uplink {
            // TS 25.331 8.6.5.2 requires an empty TFC and shorter-TTI-empty projections.
            let empty = |combination: &Combination, index: usize| {
                formats[index][combination.formats[index]].empty()
            };
            if !combinations
                .iter()
                .any(|row| (0..transports.len()).all(|index| empty(row, index)))
            {
                return Err(DedicatedError::Combination);
            }
            let intervals: BTreeSet<_> = transports.iter().map(DchTransport::frames).collect();
            for row in &combinations {
                for &frames in &intervals {
                    if !combinations.iter().any(|candidate| {
                        transports.iter().enumerate().all(|(index, transport)| {
                            if transport.frames() < frames {
                                empty(candidate, index)
                            } else if transport.frames() == frames {
                                candidate.formats[index] == row.formats[index]
                            } else {
                                true
                            }
                        })
                    }) {
                        return Err(DedicatedError::Combination);
                    }
                }
            }
        }
        Ok(Self {
            direction,
            transports,
            combinations,
            ctfc,
        })
    }
    pub fn direction(&self) -> DchDirection {
        self.direction
    }
    pub fn transports(&self) -> &[DchTransport] {
        &self.transports
    }
    pub fn combinations(&self) -> &[Combination] {
        &self.combinations
    }
    pub fn ctfc_values(&self) -> &[u32] {
        &self.ctfc
    }
    pub fn transport(&self, id: u8) -> Option<&DchTransport> {
        self.transports
            .binary_search_by_key(&id, |transport| transport.id)
            .ok()
            .map(|index| &self.transports[index])
    }
    pub fn wire(&self) -> Result<ie::TFCS, DedicatedError> {
        let largest = self
            .ctfc
            .iter()
            .copied()
            .max()
            .ok_or(DedicatedError::Combination)?;
        let power: Vec<_> = self
            .combinations
            .iter()
            .map(|row| row.gain.map(GainFactors::wire).transpose())
            .collect::<Result<_, _>>()?;
        macro_rules! rows {
            ($variant:ident, $list:ident, $row:ident, $integer:ty) => {
                ie::TFCSReconfAddCtfcSize::$variant(ie::$list(
                    self.ctfc
                        .iter()
                        .zip(power)
                        .map(|(&ctfc, gain)| ie::$row::new(ctfc as $integer, gain))
                        .collect(),
                ))
            };
        }
        let size = match largest {
            0..=3 => rows!(
                ctfc2Bit,
                TFCSReconfAddCtfcSizeCtfc2Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc2Bit,
                u8
            ),
            4..=15 => rows!(
                ctfc4Bit,
                TFCSReconfAddCtfcSizeCtfc4Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc4Bit,
                u8
            ),
            16..=63 => rows!(
                ctfc6Bit,
                TFCSReconfAddCtfcSizeCtfc6Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc6Bit,
                u8
            ),
            64..=255 => rows!(
                ctfc8Bit,
                TFCSReconfAddCtfcSizeCtfc8Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc8Bit,
                u8
            ),
            256..=4095 => rows!(
                ctfc12Bit,
                TFCSReconfAddCtfcSizeCtfc12Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc12Bit,
                u16
            ),
            4096..=65535 => rows!(
                ctfc16Bit,
                TFCSReconfAddCtfcSizeCtfc16Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc16Bit,
                u16
            ),
            _ => rows!(
                ctfc24Bit,
                TFCSReconfAddCtfcSizeCtfc24Bit,
                AnonymousTFCSReconfAddCtfcSizeCtfc24Bit,
                u32
            ),
        };
        Ok(ie::TFCS::normalTFCI_Signalling(
            ie::ExplicitTFCSConfiguration::complete(ie::TFCSReconfAdd::new(size)),
        ))
    }
}
