// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{EncodedInformation, bits::BitWriter};
use umts_rrc_codec::{CodecError, RrcValueKind, validate_rrc_value};

const MAX_FORMATS: usize = 32;
const MAX_BLOCKS: u16 = 512;
const MAX_RATE_MATCHING_ATTRIBUTE: u16 = 256;
const FORMAT_COUNT_BITS: usize = 5;
const TTI_CHOICE_BITS: usize = 3;
const RLC_CHOICE_BITS: usize = 2;
const BLOCK_COUNT_CHOICE_BITS: usize = 2;
const SMALL_BLOCK_COUNT_BITS: usize = 4;
const LARGE_BLOCK_COUNT_BITS: usize = 9;
const LOGICAL_CHANNEL_CHOICE_BITS: usize = 2;
const CODING_CHOICE_BITS: usize = 2;
const RATE_MATCHING_BITS: usize = 8;
const CRC_CHOICE_BITS: usize = 3;
const MAX_CHANNELS: usize = 32;
const MAX_COMBINATIONS: usize = 1024;
const MAX_CTFC: u32 = (1 << 24) - 1;
const EXPLICIT_TFCS_CHOICE_BITS: usize = 2;
const CTFC_SIZE_CHOICE_BITS: usize = 3;
const COMBINATION_COUNT_BITS: usize = 10;
const CTFC_WIDTHS: [usize; 7] = [2, 4, 6, 8, 12, 16, 24];
const COMPLETE_TFCS_CHOICE: u64 = 0;
const GAIN_BITS: usize = 4;
const MAX_GAIN: u8 = 15;
const MESSAGE_OFFSET_MIN_DB: i8 = -5;
const MESSAGE_OFFSET_MAX_DB: i8 = 10;
const MESSAGE_OFFSET_BITS: usize = 4;
const CONVOLUTIONAL_CHOICE: u64 = 1;
const TURBO_CHOICE: u64 = 2;
const ZERO_BLOCKS_CHOICE: u64 = 0;
const ONE_BLOCK_CHOICE: u64 = 1;
const SMALL_BLOCKS_CHOICE: u64 = 2;
const LARGE_BLOCKS_CHOICE: u64 = 3;
const OCTET_RLC_SMALL_BASE_BITS: u16 = 16;
const OCTET_RLC_MEDIUM_BASE_BITS: u16 = 272;
const OCTET_RLC_LARGE_BASE_BITS: u16 = 1040;
const OCTET_RLC_MAXIMUM_BITS: u16 = 5000;
const OCTET_RLC_MEDIUM_STEP_BITS: u16 = 32;
const OCTET_RLC_LARGE_STEP_BITS: u16 = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TransportInterval {
    Ms10,
    Ms20,
    Ms40,
    Ms80,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ConvolutionalRate {
    Half,
    Third,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelCoding {
    Convolutional(ConvolutionalRate),
    Turbo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CrcSize {
    None,
    Bits8,
    Bits12,
    Bits16,
    Bits24,
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum LogicalChannels {
    AllSizes,
    Configured,
}

pub struct CommonFormatGroup {
    pub transport_block_bits: u16,
    pub block_counts: Vec<u16>,
    pub logical_channels: LogicalChannels,
}

pub struct CommonTransportFormatSet {
    pub interval: TransportInterval,
    pub coding: ChannelCoding,
    pub rate_matching_attribute: u16,
    pub crc: CrcSize,
    pub groups: Vec<CommonFormatGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    FormatCount,
    BlockCount,
    BlockSize,
    RateMatching,
    DuplicateFormat,
    Combination,
    PowerOffset,
    Codec(CodecError),
}

// TS 25.331 OctetModeRLC-SizeInfoType1 represents size in bits, not octets.
pub(crate) fn octet_rlc_size(
    bits: u16,
) -> Result<umts_rrc_schema::information_elements::OctetModeRLCSizeInfoType1, TransportError> {
    use umts_rrc_schema::information_elements as ie;
    if !bits.is_multiple_of(8)
        || !(OCTET_RLC_SMALL_BASE_BITS..=OCTET_RLC_MAXIMUM_BITS).contains(&bits)
    {
        return Err(TransportError::BlockSize);
    }
    Ok(if bits < OCTET_RLC_MEDIUM_BASE_BITS {
        ie::OctetModeRLCSizeInfoType1::sizeType1(((bits - OCTET_RLC_SMALL_BASE_BITS) / 8) as u8)
    } else if bits < OCTET_RLC_LARGE_BASE_BITS {
        let remainder =
            ((bits - OCTET_RLC_MEDIUM_BASE_BITS) % OCTET_RLC_MEDIUM_STEP_BITS / 8) as u8;
        ie::OctetModeRLCSizeInfoType1::sizeType2(ie::OctetModeRLCSizeInfoType1SizeType2::new(
            ((bits - OCTET_RLC_MEDIUM_BASE_BITS) / OCTET_RLC_MEDIUM_STEP_BITS) as u8,
            (remainder != 0).then_some(remainder),
        ))
    } else {
        let remainder = ((bits - OCTET_RLC_LARGE_BASE_BITS) % OCTET_RLC_LARGE_STEP_BITS / 8) as u8;
        ie::OctetModeRLCSizeInfoType1::sizeType3(ie::OctetModeRLCSizeInfoType1SizeType3::new(
            ((bits - OCTET_RLC_LARGE_BASE_BITS) / OCTET_RLC_LARGE_STEP_BITS) as u8,
            (remainder != 0).then_some(remainder),
        ))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrachMessagePower {
    control: u8,
    data: u8,
    message_offset_db: i8,
}

impl PrachMessagePower {
    pub fn new(control: u8, data: u8, message_offset_db: i8) -> Result<Self, TransportError> {
        if control == 0
            || control.max(data) != MAX_GAIN
            || !(MESSAGE_OFFSET_MIN_DB..=MESSAGE_OFFSET_MAX_DB).contains(&message_offset_db)
        {
            return Err(TransportError::PowerOffset);
        }
        Ok(Self {
            control,
            data,
            message_offset_db,
        })
    }

    fn write(self, output: &mut BitWriter) {
        // TS 25.331 10.3.5.8: explicit FDD beta factors and preamble-to-message offset.
        output.flag(true);
        output.flag(false);
        output.flag(false);
        output.flag(false);
        output.write(u64::from(self.control), GAIN_BITS);
        output.write(u64::from(self.data), GAIN_BITS);
        output.write(
            (self.message_offset_db - MESSAGE_OFFSET_MIN_DB) as u64,
            MESSAGE_OFFSET_BITS,
        );
    }
}

pub struct TransportCombinations {
    ctfc: Vec<u32>,
}

impl TransportCombinations {
    // TS 25.331 14.10 orders TrCH1 first in the mixed-radix CTFC representation.
    // Channel order is ascending identity, except FACH/PCH use their signaled list order.
    pub fn new(
        channels: &[&CommonTransportFormatSet],
        combinations: &[Vec<usize>],
    ) -> Result<Self, TransportError> {
        if channels.is_empty()
            || channels.len() > MAX_CHANNELS
            || combinations.is_empty()
            || combinations.len() > MAX_COMBINATIONS
        {
            return Err(TransportError::Combination);
        }
        for channel in channels {
            channel.validate()?;
        }
        let counts: Vec<_> = channels
            .iter()
            .map(|channel| channel.formats().count())
            .collect();
        let mut ctfc = Vec::with_capacity(combinations.len());
        let mut unique = std::collections::BTreeSet::new();
        for combination in combinations {
            if combination.len() != channels.len() {
                return Err(TransportError::Combination);
            }
            let mut value = 0_u32;
            for (&index, &count) in combination.iter().zip(&counts).rev() {
                if index >= count {
                    return Err(TransportError::Combination);
                }
                value = value
                    .checked_mul(count as u32)
                    .and_then(|value| value.checked_add(index as u32))
                    .filter(|&value| value <= MAX_CTFC)
                    .ok_or(TransportError::Combination)?;
            }
            if !unique.insert(value) {
                return Err(TransportError::DuplicateFormat);
            }
            ctfc.push(value);
        }
        Ok(Self { ctfc })
    }

    pub fn ctfc_values(&self) -> &[u32] {
        &self.ctfc
    }

    pub fn encode(&self) -> Result<EncodedInformation, TransportError> {
        let mut output = BitWriter::default();
        self.write(&mut output);
        validate_rrc_value(
            RrcValueKind::TransportFormatCombinations,
            &output.bytes,
            output.length,
        )
        .map_err(TransportError::Codec)?;
        Ok(EncodedInformation {
            bytes: output.bytes,
            bit_length: output.length,
        })
    }

    pub(crate) fn write(&self, output: &mut BitWriter) {
        self.write_inner(output, None);
    }

    pub(crate) fn write_prach(
        &self,
        output: &mut BitWriter,
        powers: &[PrachMessagePower],
    ) -> Result<(), TransportError> {
        if powers.len() != self.ctfc.len() {
            return Err(TransportError::PowerOffset);
        }
        self.write_inner(output, Some(powers));
        Ok(())
    }

    fn write_inner(&self, output: &mut BitWriter, powers: Option<&[PrachMessagePower]>) {
        let maximum = self.ctfc.iter().copied().max().unwrap_or(0);
        let choice = CTFC_WIDTHS
            .iter()
            .position(|&width| maximum < 1 << width)
            .expect("validated CTFC width");
        output.flag(false);
        output.write(COMPLETE_TFCS_CHOICE, EXPLICIT_TFCS_CHOICE_BITS);
        output.write(choice as u64, CTFC_SIZE_CHOICE_BITS);
        output.write((self.ctfc.len() - 1) as u64, COMBINATION_COUNT_BITS);
        for (index, &value) in self.ctfc.iter().enumerate() {
            output.flag(powers.is_some());
            output.write(u64::from(value), CTFC_WIDTHS[choice]);
            if let Some(powers) = powers {
                powers[index].write(output);
            }
        }
    }
}

fn common_rlc_size(bits: u16) -> Result<(u64, u64, usize), TransportError> {
    // TS 25.331 OctetModeRLC-SizeInfoType2 uses three discontinuous size ranges.
    const RANGES: [(u16, u16, u16, usize); 3] =
        [(48, 296, 8, 5), (312, 1320, 16, 6), (1384, 4968, 64, 6)];
    for (choice, &(minimum, maximum, step, width)) in RANGES.iter().enumerate() {
        if (minimum..=maximum).contains(&bits) && (bits - minimum).is_multiple_of(step) {
            return Ok((choice as u64, u64::from((bits - minimum) / step), width));
        }
    }
    Err(TransportError::BlockSize)
}

impl CommonTransportFormatSet {
    pub fn validate(&self) -> Result<(), TransportError> {
        if self.groups.is_empty() || self.groups.len() > MAX_FORMATS {
            return Err(TransportError::FormatCount);
        }
        if !(1..=MAX_RATE_MATCHING_ATTRIBUTE).contains(&self.rate_matching_attribute) {
            return Err(TransportError::RateMatching);
        }
        let mut formats = Vec::new();
        for group in &self.groups {
            common_rlc_size(group.transport_block_bits)?;
            if group.block_counts.is_empty() || group.block_counts.len() > MAX_FORMATS {
                return Err(TransportError::FormatCount);
            }
            for &count in &group.block_counts {
                if count > MAX_BLOCKS {
                    return Err(TransportError::BlockCount);
                }
                if formats.contains(&(group.transport_block_bits, count)) {
                    return Err(TransportError::DuplicateFormat);
                }
                formats.push((group.transport_block_bits, count));
                if formats.len() > MAX_FORMATS {
                    return Err(TransportError::FormatCount);
                }
            }
        }
        Ok(())
    }

    // TS 25.331 TransportFormatSet/CommonTransChTFS, fixed TTI and FDD RLC size.
    pub fn encode(&self) -> Result<EncodedInformation, TransportError> {
        let mut output = BitWriter::default();
        self.write(&mut output)?;
        validate_rrc_value(
            RrcValueKind::TransportFormatSet,
            &output.bytes,
            output.length,
        )
        .map_err(TransportError::Codec)?;
        Ok(EncodedInformation {
            bytes: output.bytes,
            bit_length: output.length,
        })
    }

    pub(crate) fn write(&self, output: &mut BitWriter) -> Result<(), TransportError> {
        self.validate()?;
        output.flag(true);
        output.write(self.interval as u64, TTI_CHOICE_BITS);
        output.write((self.groups.len() - 1) as u64, FORMAT_COUNT_BITS);
        for group in &self.groups {
            output.flag(false);
            let (choice, size, width) = common_rlc_size(group.transport_block_bits)?;
            output.write(choice, RLC_CHOICE_BITS);
            output.write(size, width);
            output.write((group.block_counts.len() - 1) as u64, FORMAT_COUNT_BITS);
            for &count in &group.block_counts {
                write_block_count(output, count);
            }
            output.write(group.logical_channels as u64, LOGICAL_CHANNEL_CHOICE_BITS);
        }
        match self.coding {
            ChannelCoding::Convolutional(rate) => {
                output.write(CONVOLUTIONAL_CHOICE, CODING_CHOICE_BITS);
                output.flag(matches!(rate, ConvolutionalRate::Third));
            }
            ChannelCoding::Turbo => output.write(TURBO_CHOICE, CODING_CHOICE_BITS),
        }
        output.write(
            u64::from(self.rate_matching_attribute - 1),
            RATE_MATCHING_BITS,
        );
        output.write(self.crc as u64, CRC_CHOICE_BITS);
        Ok(())
    }

    pub fn formats(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.groups.iter().flat_map(|group| {
            group
                .block_counts
                .iter()
                .map(|&count| (group.transport_block_bits, count))
        })
    }
}

fn write_block_count(output: &mut BitWriter, count: u16) {
    match count {
        0 => output.write(ZERO_BLOCKS_CHOICE, BLOCK_COUNT_CHOICE_BITS),
        1 => output.write(ONE_BLOCK_CHOICE, BLOCK_COUNT_CHOICE_BITS),
        2..=17 => {
            output.write(SMALL_BLOCKS_CHOICE, BLOCK_COUNT_CHOICE_BITS);
            output.write(u64::from(count - 2), SMALL_BLOCK_COUNT_BITS);
        }
        _ => {
            output.write(LARGE_BLOCKS_CHOICE, BLOCK_COUNT_CHOICE_BITS);
            output.write(u64::from(count - 18), LARGE_BLOCK_COUNT_BITS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prach_power_is_explicit_per_combination_and_matches_independent_per_vector() {
        let channel = baseline(168, vec![0, 1]);
        let combinations = TransportCombinations::new(&[&channel], &[vec![0], vec![1]]).unwrap();
        let powers = [
            PrachMessagePower::new(15, 8, -5).unwrap(),
            PrachMessagePower::new(7, 15, 10).unwrap(),
        ];
        let mut output = BitWriter::default();
        combinations.write_prach(&mut output, &powers).unwrap();
        assert_eq!(output.length, 54);
        assert_eq!(output.bytes, [0x00, 0x01, 0x91, 0xf0, 0x16, 0x1f, 0xfc]);
        validate_rrc_value(
            RrcValueKind::TransportFormatCombinations,
            &output.bytes,
            output.length,
        )
        .unwrap();
        let mut empty = BitWriter::default();
        assert_eq!(
            combinations.write_prach(&mut empty, &powers[..1]),
            Err(TransportError::PowerOffset)
        );
        assert_eq!(empty.length, 0);
    }

    #[test]
    fn prach_power_rejects_invalid_normalization_and_offset_ranges() {
        for (control, data, offset) in [
            (0, 15, 2),
            (14, 14, 2),
            (16, 15, 2),
            (15, 16, 2),
            (15, 15, -6),
            (15, 15, 11),
        ] {
            assert_eq!(
                PrachMessagePower::new(control, data, offset),
                Err(TransportError::PowerOffset)
            );
        }
        assert!(PrachMessagePower::new(15, 0, 2).is_ok());
    }

    fn baseline(size: u16, counts: Vec<u16>) -> CommonTransportFormatSet {
        CommonTransportFormatSet {
            interval: TransportInterval::Ms20,
            coding: ChannelCoding::Convolutional(ConvolutionalRate::Half),
            rate_matching_attribute: 256,
            crc: CrcSize::Bits16,
            groups: vec![CommonFormatGroup {
                transport_block_bits: size,
                block_counts: counts,
                logical_channels: LogicalChannels::Configured,
            }],
        }
    }

    #[test]
    fn all_size_ranges_and_block_count_branches_match_generated_codec() {
        for size in [48, 168, 296, 312, 1320, 1384, 4968] {
            let encoded = baseline(size, vec![0, 1, 2, 17, 18, 512]).encode().unwrap();
            assert!(encoded.bit_length > 40);
        }
    }

    #[test]
    fn tfi_order_is_retained_across_multiple_size_groups() {
        let mut formats = baseline(168, vec![0, 1]);
        formats.groups.push(CommonFormatGroup {
            transport_block_bits: 336,
            block_counts: vec![1, 2],
            logical_channels: LogicalChannels::AllSizes,
        });
        assert!(matches!(formats.encode(), Err(TransportError::BlockSize)));
        formats.groups[1].transport_block_bits = 328;
        formats.encode().unwrap();
        assert_eq!(
            formats.formats().collect::<Vec<_>>(),
            [(168, 0), (168, 1), (328, 1), (328, 2)]
        );
    }

    #[test]
    fn nonrepresentable_sizes_and_duplicate_formats_are_rejected() {
        for size in [0, 47, 49, 304, 1328, 4969] {
            assert!(matches!(
                baseline(size, vec![1]).encode(),
                Err(TransportError::BlockSize)
            ));
        }
        assert!(matches!(
            baseline(168, vec![1, 1]).encode(),
            Err(TransportError::DuplicateFormat)
        ));
        assert!(matches!(
            baseline(168, vec![513]).encode(),
            Err(TransportError::BlockCount)
        ));
    }

    #[test]
    fn literal_common_tfs_vector_preserves_size_and_tfi_order() {
        let encoded = baseline(168, vec![0, 1]).encode().unwrap();
        assert_eq!(encoded.bit_length, 42);
        assert_eq!(encoded.bytes, [0x90, 0x07, 0x84, 0x55, 0xfe, 0xc0]);
    }

    #[test]
    fn ctfc_matches_independent_two_channel_mixed_radix_values() {
        let first = baseline(168, vec![0, 1]);
        let second = baseline(328, vec![0, 1, 2]);
        let combinations = TransportCombinations::new(
            &[&first, &second],
            &[vec![0, 0], vec![1, 0], vec![0, 1], vec![1, 2]],
        )
        .unwrap();
        assert_eq!(combinations.ctfc_values(), [0, 1, 2, 5]);
        combinations.encode().unwrap();
        let simple = TransportCombinations::new(&[&first], &[vec![0], vec![1]])
            .unwrap()
            .encode()
            .unwrap();
        assert_eq!(simple.bit_length, 22);
        assert_eq!(simple.bytes, [0x00, 0x01, 0x04]);
    }

    #[test]
    fn combinations_reject_bad_indices_duplicates_and_empty_lists() {
        let channel = baseline(168, vec![0, 1]);
        for indices in [
            vec![],
            vec![vec![2]],
            vec![vec![0, 1]],
            vec![vec![0], vec![0]],
        ] {
            assert!(TransportCombinations::new(&[&channel], &indices).is_err());
        }
    }

    #[test]
    fn every_ctfc_width_and_the_24_bit_limit_are_checked() {
        let channel = baseline(168, (0..32).collect());
        let channels = [&channel; 5];
        for width in CTFC_WIDTHS {
            let value = (1_u32 << width) - 1;
            let indices = (0..channels.len())
                .map(|index| ((value >> (5 * index)) & 31) as usize)
                .collect();
            let combinations = TransportCombinations::new(&channels, &[indices]).unwrap();
            assert_eq!(combinations.ctfc_values(), [value]);
            combinations.encode().unwrap();
        }
        assert!(matches!(
            TransportCombinations::new(&channels, &[vec![0, 0, 0, 0, 16]]),
            Err(TransportError::Combination)
        ));
    }
}
