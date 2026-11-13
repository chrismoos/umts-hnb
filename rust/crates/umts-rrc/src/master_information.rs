// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::{CodecError, SibType, validate_master_information};

use crate::bits::BitWriter as Bits;
use crate::broadcast::{
    BroadcastBlock, BroadcastSchedule, BroadcastTiming, MAX_REFERENCES, ScheduleError,
};
use umts_types::RadioEpoch;

const CELL_TAG_MAX: u16 = 4;
const PLMN_TAG_MAX: u16 = 256;
const MIB_TAG_MAX: u8 = 8;
const IDENTITY_MAX: u8 = 15;
const MIB_TAG_BITS: usize = 3;
const PLMN_TYPE_BITS: usize = 2;
const DIGIT_BITS: usize = 4;
const REFERENCE_COUNT_BITS: usize = 5;
const REFERENCE_CHOICE_BITS: usize = 5;
const PLMN_TAG_BITS: usize = 8;
const CELL_TAG_BITS: usize = 2;
const IDENTITY_BITS: usize = 4;
const SEGMENT_COUNT_BITS: usize = 4;
const SCHEDULING_CHOICE_BITS: usize = 4;
const SEGMENT_OFFSET_BITS: usize = 4;
const MIB_REPETITION_FRAMES: u16 = 8;
const BCH_FRAMES: usize = 2;

#[repr(u8)]
enum MibChoice {
    Type1 = 0,
    Type2 = 1,
    Type3 = 2,
    Type4 = 3,
    Type5 = 4,
    Type6 = 5,
    Type7 = 6,
    Type11 = 10,
    Type12 = 11,
    Type13 = 12,
    Type13_1 = 13,
    Type13_2 = 14,
    Type13_3 = 15,
    Type13_4 = 16,
    Type14 = 17,
    Type15 = 18,
    Type16 = 19,
    Type17 = 20,
    SchedulingBlock1 = 21,
    SchedulingBlock2 = 22,
    Type15_1 = 23,
    Type15_2 = 24,
    Type15_3 = 25,
    Type15_4 = 26,
    Type18 = 27,
    Type15_5 = 28,
    Type5bis = 29,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MasterInformationError {
    Plmn,
    ValueTag,
    References,
    UnsupportedType,
    Codec(CodecError),
    Schedule(ScheduleError),
}

#[derive(Clone, Copy, Debug)]
pub enum ReferenceTag {
    Value(u16),
    None,
    Predefined { identity: u8, value: u8 },
    Occurrence { identity: u8, value: u8 },
}

pub struct MibReference {
    kind: SibType,
    tag: ReferenceTag,
    timing: BroadcastTiming,
}

impl MibReference {
    pub fn for_block(
        block: &BroadcastBlock,
        tag: ReferenceTag,
    ) -> Result<Self, MasterInformationError> {
        let reference = Self {
            kind: block.kind(),
            tag,
            timing: block.timing().clone(),
        };
        reference.type_and_tag(&mut Bits::default())?;
        Ok(reference)
    }

    // TS 25.331 SIBSb-TypeAndTag and SIB-Type have different choice/enumeration order.
    fn type_and_tag(&self, output: &mut Bits) -> Result<(), MasterInformationError> {
        let (choice, tag_class) = match self.kind {
            SibType::Type1 => (MibChoice::Type1, TagClass::Plmn),
            SibType::Type2 => (MibChoice::Type2, TagClass::Cell),
            SibType::Type3 => (MibChoice::Type3, TagClass::Cell),
            SibType::Type4 => (MibChoice::Type4, TagClass::Cell),
            SibType::Type5 => (MibChoice::Type5, TagClass::Cell),
            SibType::Type6 => (MibChoice::Type6, TagClass::Cell),
            SibType::Type7 => (MibChoice::Type7, TagClass::None),
            SibType::Type11 => (MibChoice::Type11, TagClass::Cell),
            SibType::Type12 => (MibChoice::Type12, TagClass::Cell),
            SibType::Type13 => (MibChoice::Type13, TagClass::Cell),
            SibType::Type13_1 => (MibChoice::Type13_1, TagClass::Cell),
            SibType::Type13_2 => (MibChoice::Type13_2, TagClass::Cell),
            SibType::Type13_3 => (MibChoice::Type13_3, TagClass::Cell),
            SibType::Type13_4 => (MibChoice::Type13_4, TagClass::Cell),
            SibType::Type14 => (MibChoice::Type14, TagClass::None),
            SibType::Type15 => (MibChoice::Type15, TagClass::Cell),
            SibType::Type15_1 => (MibChoice::Type15_1, TagClass::Cell),
            SibType::Type15_2 => (MibChoice::Type15_2, TagClass::Occurrence),
            SibType::Type15_3 => (MibChoice::Type15_3, TagClass::Occurrence),
            SibType::Type16 => (MibChoice::Type16, TagClass::Predefined),
            SibType::Type17 => (MibChoice::Type17, TagClass::None),
            SibType::Type15_4 => (MibChoice::Type15_4, TagClass::Cell),
            SibType::Type18 => (MibChoice::Type18, TagClass::Cell),
            SibType::SchedulingBlock1 => (MibChoice::SchedulingBlock1, TagClass::Cell),
            SibType::SchedulingBlock2 => (MibChoice::SchedulingBlock2, TagClass::Cell),
            SibType::Type15_5 => (MibChoice::Type15_5, TagClass::Cell),
            SibType::Type5bis => (MibChoice::Type5bis, TagClass::Cell),
            _ => return Err(MasterInformationError::UnsupportedType),
        };
        output.write(choice as u64, REFERENCE_CHOICE_BITS);
        match (tag_class, self.tag) {
            (TagClass::Plmn, ReferenceTag::Value(value)) if (1..=PLMN_TAG_MAX).contains(&value) => {
                output.write(u64::from(value - 1), PLMN_TAG_BITS)
            }
            (TagClass::Cell, ReferenceTag::Value(value)) if (1..=CELL_TAG_MAX).contains(&value) => {
                output.write(u64::from(value - 1), CELL_TAG_BITS)
            }
            (TagClass::None, ReferenceTag::None) => {}
            (TagClass::Predefined, ReferenceTag::Predefined { identity, value })
            | (TagClass::Occurrence, ReferenceTag::Occurrence { identity, value })
                if identity <= IDENTITY_MAX && value <= IDENTITY_MAX =>
            {
                output.write(u64::from(identity), IDENTITY_BITS);
                output.write(u64::from(value), IDENTITY_BITS);
            }
            _ => return Err(MasterInformationError::ValueTag),
        }
        Ok(())
    }
}

enum TagClass {
    Plmn,
    Cell,
    None,
    Predefined,
    Occurrence,
}

pub struct MasterInformationBlock {
    pub value_tag: u8,
    pub mcc: [u8; 3],
    pub mnc: Vec<u8>,
    pub references: Vec<MibReference>,
}

pub type EncodedMasterInformation = crate::EncodedInformation;

impl MasterInformationBlock {
    // TS 25.331 MasterInformationBlock, PLMN-Type and SchedulingInformation UPER.
    pub fn encode(&self) -> Result<EncodedMasterInformation, MasterInformationError> {
        if !(1..=MIB_TAG_MAX).contains(&self.value_tag) {
            return Err(MasterInformationError::ValueTag);
        }
        if !(2..=3).contains(&self.mnc.len())
            || self.mcc.iter().chain(&self.mnc).any(|&digit| digit > 9)
        {
            return Err(MasterInformationError::Plmn);
        }
        if self.references.is_empty() || self.references.len() > MAX_REFERENCES {
            return Err(MasterInformationError::References);
        }
        let mut output = Bits::default();
        output.flag(false);
        output.write(u64::from(self.value_tag - 1), MIB_TAG_BITS);
        output.write(0, PLMN_TYPE_BITS);
        for &digit in &self.mcc {
            output.write(u64::from(digit), DIGIT_BITS);
        }
        output.flag(self.mnc.len() == 3);
        for &digit in &self.mnc {
            output.write(u64::from(digit), DIGIT_BITS);
        }
        output.write((self.references.len() - 1) as u64, REFERENCE_COUNT_BITS);
        for reference in &self.references {
            reference.type_and_tag(&mut output)?;
            let timing = &reference.timing;
            let segmented = timing.positions().len() > 1;
            output.flag(segmented);
            output.flag(segmented);
            if segmented {
                output.write((timing.positions().len() - 1) as u64, SEGMENT_COUNT_BITS);
            }
            let exponent = timing.repetition().ilog2();
            output.write(u64::from(exponent - 2), SCHEDULING_CHOICE_BITS);
            output.write(
                u64::from(timing.positions()[0] / BCH_FRAMES as u16),
                exponent as usize - 1,
            );
            if segmented {
                output.write((timing.positions().len() - 2) as u64, SEGMENT_COUNT_BITS);
                for pair in timing.positions().windows(2) {
                    output.write(
                        u64::from((pair[1] - pair[0]) / BCH_FRAMES as u16 - 1),
                        SEGMENT_OFFSET_BITS,
                    );
                }
            }
        }
        validate_master_information(&output.bytes, output.length)
            .map_err(MasterInformationError::Codec)?;
        Ok(EncodedMasterInformation {
            bytes: output.bytes,
            bit_length: output.length,
        })
    }
}

pub fn compile_broadcast(
    epoch: RadioEpoch,
    value_tag: u8,
    mcc: [u8; 3],
    mnc: Vec<u8>,
    mut blocks: Vec<(BroadcastBlock, ReferenceTag)>,
) -> Result<BroadcastSchedule, MasterInformationError> {
    if blocks.is_empty() || blocks.len() > MAX_REFERENCES {
        return Err(MasterInformationError::References);
    }
    let references = blocks
        .iter()
        .map(|(block, tag)| MibReference::for_block(block, *tag))
        .collect::<Result<_, _>>()?;
    let encoded = MasterInformationBlock {
        value_tag,
        mcc,
        mnc,
        references,
    }
    .encode()?;
    let count = if encoded.bit_length <= umts_rrc_codec::FIXED_SIB_BITS {
        1
    } else {
        encoded.bit_length.div_ceil(umts_rrc_codec::SEGMENT_BITS)
    };
    let timing = BroadcastTiming::new(
        MIB_REPETITION_FRAMES,
        (0..count)
            .map(|index| (BCH_FRAMES * index) as u16)
            .collect(),
    )
    .map_err(MasterInformationError::Schedule)?;
    let mib = BroadcastBlock::new(
        SibType::MasterInformationBlock,
        timing,
        &encoded.bytes,
        encoded.bit_length,
    )
    .map_err(MasterInformationError::Schedule)?;
    let mut schedule = vec![mib];
    schedule.extend(blocks.drain(..).map(|(block, _)| block));
    BroadcastSchedule::compile(epoch, &schedule).map_err(MasterInformationError::Schedule)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(
        kind: u8,
        tag: ReferenceTag,
        repetition: u16,
        positions: Vec<u16>,
    ) -> MibReference {
        let length: usize = if positions.len() == 1 {
            8
        } else {
            positions.len() * 222
        };
        let block = BroadcastBlock::new(
            SibType::new(kind).unwrap(),
            BroadcastTiming::new(repetition, positions).unwrap(),
            &vec![0; length.div_ceil(8)],
            length,
        )
        .unwrap();
        MibReference::for_block(&block, tag).unwrap()
    }

    #[test]
    fn generated_codec_validates_mib_plmn_and_all_legal_repetition_periods() {
        for repetition in [4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096] {
            for mnc in [vec![0, 1], vec![0, 0, 1]] {
                let mib = MasterInformationBlock {
                    value_tag: 8,
                    mcc: [0, 0, 1],
                    mnc,
                    references: vec![
                        reference(1, ReferenceTag::Value(256), repetition, vec![2]),
                        reference(7, ReferenceTag::None, 8, vec![4]),
                    ],
                };
                let encoded = mib.encode().unwrap();
                assert!(encoded.bit_length > 30 && encoded.bit_length < 226);
            }
        }
    }

    #[test]
    fn generated_codec_validates_distinct_tag_classes_and_segment_offsets() {
        let mib = MasterInformationBlock {
            value_tag: 1,
            mcc: [9, 9, 9],
            mnc: vec![9, 9, 9],
            references: vec![
                reference(3, ReferenceTag::Value(4), 64, vec![2, 4, 10]),
                reference(
                    23,
                    ReferenceTag::Predefined {
                        identity: 15,
                        value: 15,
                    },
                    16,
                    vec![6],
                ),
                reference(
                    21,
                    ReferenceTag::Occurrence {
                        identity: 7,
                        value: 8,
                    },
                    32,
                    vec![10],
                ),
                reference(28, ReferenceTag::Value(3), 64, vec![12]),
            ],
        };
        mib.encode().unwrap();
    }

    #[test]
    fn invalid_plmn_and_tag_combinations_are_not_encoded() {
        let mut mib = MasterInformationBlock {
            value_tag: 0,
            mcc: [0, 0, 1],
            mnc: vec![0, 1],
            references: vec![reference(7, ReferenceTag::None, 8, vec![2])],
        };
        assert!(matches!(
            mib.encode(),
            Err(MasterInformationError::ValueTag)
        ));
        mib.value_tag = 1;
        mib.mnc = vec![1];
        assert!(matches!(mib.encode(), Err(MasterInformationError::Plmn)));
        let block = BroadcastBlock::new(
            SibType::new(7).unwrap(),
            BroadcastTiming::new(8, vec![2]).unwrap(),
            &[0],
            8,
        )
        .unwrap();
        assert!(matches!(
            MibReference::for_block(&block, ReferenceTag::Value(1)),
            Err(MasterInformationError::ValueTag)
        ));
    }

    #[test]
    fn minimal_mib_matches_literal_uper_vector() {
        let encoded = MasterInformationBlock {
            value_tag: 1,
            mcc: [0, 0, 1],
            mnc: vec![0, 1],
            references: vec![reference(7, ReferenceTag::None, 8, vec![2])],
        }
        .encode()
        .unwrap();
        assert_eq!(encoded.bit_length, 45);
        assert_eq!(encoded.bytes, [0x00, 0x00, 0x40, 0x20, 0x30, 0x28]);
    }

    #[test]
    fn compiled_plan_broadcasts_actual_mib_for_the_scheduled_blocks() {
        use std::num::NonZeroU64;
        use umts_rrc_codec::{BchMessage, BchPayload};
        use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};
        let epoch = RadioEpoch::new(NonZeroU64::new(1).unwrap());
        let block = BroadcastBlock::new(
            SibType::new(7).unwrap(),
            BroadcastTiming::new(8, vec![2]).unwrap(),
            &[0x80],
            1,
        )
        .unwrap();
        let schedule = compile_broadcast(
            epoch,
            1,
            [0, 0, 1],
            vec![0, 1],
            vec![(block, ReferenceTag::None)],
        )
        .unwrap();
        let BchPayload::CompleteShort { kind, data } = BchMessage::decode(
            schedule
                .message_at(ChipInstant { epoch, index: 0 })
                .unwrap(),
        )
        .unwrap()
        .payload
        else {
            panic!("missing MIB");
        };
        assert_eq!(kind.wire_value(), 0);
        assert_eq!(data.bit_length(), 45);
        assert_eq!(&data.bytes()[..6], &[0x00, 0x00, 0x40, 0x20, 0x30, 0x28]);
        assert!(
            matches!(BchMessage::decode(schedule.message_at(ChipInstant { epoch, index: 2 * CHIPS_PER_FRAME }).unwrap()).unwrap().payload, BchPayload::CompleteShort { kind, data } if kind.wire_value() == 7 && data.bit_length() == 1)
        );
    }
}
