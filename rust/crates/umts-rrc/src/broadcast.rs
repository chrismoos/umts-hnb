// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_codec::{
    BCH_BYTES, BchMessage, BchPayload, CodecError, FIXED_SIB_BITS, FixedSegmentData, FixedSibData,
    SEGMENT_BITS, SEGMENT_BYTES, SHORT_SIB_BITS, SegmentPart, ShortSibData, SibType,
};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, ChipInstant, SYSTEM_FRAME_MODULUS},
};

const BCH_FRAMES: u64 = 2;
const MAX_SEGMENTS: usize = 16;
pub(crate) const MAX_REFERENCES: usize = 32;
const MAX_BLOCKS: usize = MAX_REFERENCES + 1;
const MIB_REPETITION: u16 = 8;
const MAX_SEGMENT_OFFSET: u16 = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    Timing,
    SegmentCount,
    BlockCount,
    BitLength,
    MissingMasterInformation,
    Collision { system_frame: u16 },
    WrongEpoch,
    UnalignedTime,
    Codec(CodecError),
}

impl From<CodecError> for ScheduleError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

#[derive(Clone, Debug)]
pub struct BroadcastTiming {
    repetition: u16,
    positions: Vec<u16>,
}

impl BroadcastTiming {
    // TS 25.331 8.1.1.1.5 and SchedulingInformation/SibOFF ASN.1 definitions.
    pub fn new(repetition: u16, positions: Vec<u16>) -> Result<Self, ScheduleError> {
        if !(4..=SYSTEM_FRAME_MODULUS as u16).contains(&repetition)
            || !repetition.is_power_of_two()
            || positions.is_empty()
            || positions.len() > MAX_SEGMENTS
            || positions
                .iter()
                .any(|&position| position >= repetition || !position.is_multiple_of(2))
            || positions.windows(2).any(|pair| {
                pair[1]
                    .checked_sub(pair[0])
                    .is_none_or(|offset| offset == 0 || offset > MAX_SEGMENT_OFFSET)
            })
        {
            return Err(ScheduleError::Timing);
        }
        Ok(Self {
            repetition,
            positions,
        })
    }

    pub fn repetition(&self) -> u16 {
        self.repetition
    }
    pub fn positions(&self) -> &[u16] {
        &self.positions
    }
}

pub struct BroadcastBlock {
    kind: SibType,
    timing: BroadcastTiming,
    segments: Vec<BchPayload>,
}

impl BroadcastBlock {
    pub fn new(
        kind: SibType,
        timing: BroadcastTiming,
        encoded: &[u8],
        bit_length: usize,
    ) -> Result<Self, ScheduleError> {
        if bit_length == 0
            || bit_length > MAX_SEGMENTS * SEGMENT_BITS
            || encoded.len() != bit_length.div_ceil(8)
        {
            return Err(ScheduleError::BitLength);
        }
        if kind.wire_value() == 0
            && (timing.repetition != MIB_REPETITION
                || timing
                    .positions
                    .iter()
                    .enumerate()
                    .any(|(i, &position)| usize::from(position) != i * BCH_FRAMES as usize))
        {
            return Err(ScheduleError::Timing);
        }
        let segments = if bit_length <= SHORT_SIB_BITS {
            vec![BchPayload::CompleteShort {
                kind,
                data: ShortSibData::new(encoded, bit_length)?,
            }]
        } else if bit_length <= FIXED_SIB_BITS {
            vec![BchPayload::Complete {
                kind,
                data: FixedSibData::padded(encoded, bit_length)?,
            }]
        } else {
            if !bit_length.is_multiple_of(8)
                && encoded[encoded.len() - 1] & ((1 << (8 - bit_length % 8)) - 1) != 0
            {
                return Err(CodecError::NonzeroUnusedBits.into());
            }
            let count = bit_length.div_ceil(SEGMENT_BITS);
            let mut segments = Vec::with_capacity(count);
            for index in 0..count {
                let start = index * SEGMENT_BITS;
                let length = SEGMENT_BITS.min(bit_length - start);
                let mut data = [0; SEGMENT_BYTES];
                for bit in 0..length {
                    let source = start + bit;
                    data[bit / 8] |=
                        ((encoded[source / 8] >> (7 - source % 8)) & 1) << (7 - bit % 8);
                }
                if index + 1 == count && length <= SHORT_SIB_BITS {
                    segments.push(BchPayload::LastShort {
                        kind,
                        index: index as u8,
                        data: ShortSibData::new(&data[..length.div_ceil(8)], length)?,
                    });
                    continue;
                }
                let part = if index == 0 {
                    SegmentPart::First { count: count as u8 }
                } else if index + 1 == count {
                    SegmentPart::Last { index: index as u8 }
                } else {
                    SegmentPart::Subsequent { index: index as u8 }
                };
                segments.push(BchPayload::Segment {
                    kind,
                    part,
                    data: FixedSegmentData::padded(&data[..length.div_ceil(8)], length)?,
                });
            }
            segments
        };
        if timing.positions.len() != segments.len() {
            return Err(ScheduleError::SegmentCount);
        }
        Ok(Self {
            kind,
            timing,
            segments,
        })
    }

    pub fn kind(&self) -> SibType {
        self.kind
    }
    pub fn timing(&self) -> &BroadcastTiming {
        &self.timing
    }
}

pub struct BroadcastSchedule {
    epoch: RadioEpoch,
    messages: Vec<[u8; BCH_BYTES]>,
}

impl BroadcastSchedule {
    pub fn compile(epoch: RadioEpoch, blocks: &[BroadcastBlock]) -> Result<Self, ScheduleError> {
        if blocks.is_empty() || blocks.len() > MAX_BLOCKS {
            return Err(ScheduleError::BlockCount);
        }
        if blocks
            .iter()
            .filter(|block| block.kind.wire_value() == 0)
            .count()
            != 1
        {
            return Err(ScheduleError::MissingMasterInformation);
        }
        let mut messages = Vec::with_capacity((SYSTEM_FRAME_MODULUS / BCH_FRAMES) as usize);
        for frame in (0..SYSTEM_FRAME_MODULUS as u16).step_by(BCH_FRAMES as usize) {
            let mut payload = None;
            for block in blocks {
                if let Some(segment) = block
                    .timing
                    .positions
                    .iter()
                    .position(|&position| frame % block.timing.repetition == position)
                {
                    if payload.is_some() {
                        return Err(ScheduleError::Collision {
                            system_frame: frame,
                        });
                    }
                    payload = Some(block.segments[segment].clone());
                }
            }
            messages.push(
                BchMessage {
                    system_frame: frame,
                    payload: payload.unwrap_or(BchPayload::NoSegment),
                }
                .encode()?,
            );
        }
        Ok(Self { epoch, messages })
    }

    pub fn message_at(&self, time: ChipInstant) -> Result<&[u8; BCH_BYTES], ScheduleError> {
        if time.epoch != self.epoch {
            return Err(ScheduleError::WrongEpoch);
        }
        if !time.index.is_multiple_of(BCH_FRAMES * CHIPS_PER_FRAME) {
            return Err(ScheduleError::UnalignedTime);
        }
        Ok(&self.messages[usize::from(time.system_frame()) / BCH_FRAMES as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    fn epoch() -> RadioEpoch {
        RadioEpoch::new(NonZeroU64::new(1).unwrap())
    }
    fn instant(frame: u64) -> ChipInstant {
        ChipInstant {
            epoch: epoch(),
            index: frame * CHIPS_PER_FRAME,
        }
    }
    fn mib() -> BroadcastBlock {
        BroadcastBlock::new(
            SibType::new(0).unwrap(),
            BroadcastTiming::new(8, vec![0]).unwrap(),
            &[0x80],
            1,
        )
        .unwrap()
    }

    fn payload_bits(length: usize) -> Vec<u8> {
        let mut bytes = vec![0xa5; length.div_ceil(8)];
        if !length.is_multiple_of(8) {
            *bytes.last_mut().unwrap() &= u8::MAX << (8 - length % 8);
        }
        bytes
    }

    fn payload_choice(message: &[u8; BCH_BYTES]) -> u8 {
        // TS 25.331 SystemInformation-BCH puts its four-bit choice after eleven SFN bits.
        const CHOICE_SHIFT: u32 = 1;
        const CHOICE_MASK: u8 = 0x0f;
        (message[1] >> CHOICE_SHIFT) & CHOICE_MASK
    }

    #[test]
    fn complete_payload_choice_preserves_true_length_at_short_fixed_boundaries() {
        const SHORT_COMPLETE_CHOICE: u8 = 7;
        const FIXED_COMPLETE_CHOICE: u8 = 9;
        for length in [1, 214, 215, 222, 226] {
            let input = payload_bits(length);
            let block = BroadcastBlock::new(
                SibType::Type3,
                BroadcastTiming::new(32, vec![2]).unwrap(),
                &input,
                length,
            )
            .unwrap();
            let schedule = BroadcastSchedule::compile(epoch(), &[mib(), block]).unwrap();
            let encoded = schedule.message_at(instant(2)).unwrap();
            let decoded = BchMessage::decode(encoded).unwrap();
            assert_eq!(decoded.system_frame, 2);
            if length <= SHORT_SIB_BITS {
                assert_eq!(payload_choice(encoded), SHORT_COMPLETE_CHOICE);
                let BchPayload::CompleteShort { kind, data } = decoded.payload else {
                    panic!("short complete required at {length} bits");
                };
                assert_eq!(kind, SibType::Type3);
                assert_eq!(data.bit_length(), length);
                assert_eq!(data.bytes(), input.as_slice());
            } else {
                assert_eq!(payload_choice(encoded), FIXED_COMPLETE_CHOICE);
                let BchPayload::Complete { kind, data } = decoded.payload else {
                    panic!("fixed complete required at {length} bits");
                };
                assert_eq!(kind, SibType::Type3);
                assert_eq!(&data.bytes()[..input.len()], input.as_slice());
                for bit in length..FIXED_SIB_BITS {
                    assert_eq!((data.bytes()[bit / 8] >> (7 - bit % 8)) & 1, 0);
                }
            }
        }
    }

    #[test]
    fn segmented_last_choice_uses_actual_remainder_including_exact_multiples() {
        const FIRST_CHOICE: u8 = 1;
        const SUBSEQUENT_CHOICE: u8 = 2;
        const SHORT_LAST_CHOICE: u8 = 3;
        const FIXED_LAST_CHOICE: u8 = 10;
        for length in [227_usize, 436, 437, 444, 445, 666] {
            let input = payload_bits(length);
            let count = length.div_ceil(SEGMENT_BITS);
            let positions: Vec<_> = (1..=count).map(|index| (index * 2) as u16).collect();
            let block = BroadcastBlock::new(
                SibType::Type5,
                BroadcastTiming::new(32, positions.clone()).unwrap(),
                &input,
                length,
            )
            .unwrap();
            let schedule = BroadcastSchedule::compile(epoch(), &[mib(), block]).unwrap();
            let mut restored = Vec::new();
            for (index, frame) in positions.into_iter().enumerate() {
                let encoded = schedule.message_at(instant(u64::from(frame))).unwrap();
                let last = index + 1 == count;
                let remaining = length - index * SEGMENT_BITS;
                let decoded = BchMessage::decode(encoded).unwrap();
                assert_eq!(decoded.system_frame, frame);
                if last && remaining <= SHORT_SIB_BITS {
                    assert_eq!(payload_choice(encoded), SHORT_LAST_CHOICE);
                    let BchPayload::LastShort {
                        kind,
                        index: segment,
                        data,
                    } = decoded.payload
                    else {
                        panic!("short last required for {length} bits");
                    };
                    assert_eq!(kind, SibType::Type5);
                    assert_eq!(segment as usize, index);
                    assert_eq!(data.bit_length(), remaining);
                    restored.extend(
                        (0..remaining).map(|bit| (data.bytes()[bit / 8] >> (7 - bit % 8)) & 1),
                    );
                } else {
                    let expected_part = if index == 0 {
                        assert_eq!(payload_choice(encoded), FIRST_CHOICE);
                        SegmentPart::First { count: count as u8 }
                    } else if last {
                        assert_eq!(payload_choice(encoded), FIXED_LAST_CHOICE);
                        SegmentPart::Last { index: index as u8 }
                    } else {
                        assert_eq!(payload_choice(encoded), SUBSEQUENT_CHOICE);
                        SegmentPart::Subsequent { index: index as u8 }
                    };
                    let BchPayload::Segment { kind, part, data } = decoded.payload else {
                        panic!("fixed segment required for {length} bits");
                    };
                    assert_eq!(kind, SibType::Type5);
                    assert_eq!(part, expected_part);
                    let carried = remaining.min(SEGMENT_BITS);
                    restored.extend(
                        (0..carried).map(|bit| (data.bytes()[bit / 8] >> (7 - bit % 8)) & 1),
                    );
                    for bit in carried..SEGMENT_BITS {
                        assert_eq!((data.bytes()[bit / 8] >> (7 - bit % 8)) & 1, 0);
                    }
                }
            }
            assert_eq!(restored.len(), length);
            for (bit, value) in restored.into_iter().enumerate() {
                assert_eq!(value, (input[bit / 8] >> (7 - bit % 8)) & 1);
            }
        }
    }

    #[test]
    fn schedules_follow_sfn_modulo_equation_across_wrap() {
        let sib = BroadcastBlock::new(
            SibType::new(1).unwrap(),
            BroadcastTiming::new(16, vec![2]).unwrap(),
            &[0x90],
            4,
        )
        .unwrap();
        let schedule = BroadcastSchedule::compile(epoch(), &[mib(), sib]).unwrap();
        for frame in (0..2 * SYSTEM_FRAME_MODULUS).step_by(2) {
            let decoded = BchMessage::decode(schedule.message_at(instant(frame)).unwrap()).unwrap();
            assert_eq!(decoded.system_frame, (frame % SYSTEM_FRAME_MODULUS) as u16);
            match decoded.payload {
                BchPayload::CompleteShort { kind, .. } if frame % 8 == 0 => {
                    assert_eq!(kind.wire_value(), 0)
                }
                BchPayload::CompleteShort { kind, .. } if frame % 16 == 2 => {
                    assert_eq!(kind.wire_value(), 1)
                }
                BchPayload::NoSegment => assert!(frame % 8 != 0 && frame % 16 != 2),
                _ => panic!("unexpected scheduled payload"),
            }
        }
    }

    #[test]
    fn non_byte_aligned_segments_reassemble_without_lost_or_duplicated_bits() {
        let length: usize = 1003;
        let mut input: Vec<u8> = (0..length.div_ceil(8)).map(|i| (i * 37) as u8).collect();
        *input.last_mut().unwrap() &= 0xe0;
        let item = BroadcastBlock::new(
            SibType::new(5).unwrap(),
            BroadcastTiming::new(32, vec![2, 4, 6, 10, 12]).unwrap(),
            &input,
            length,
        )
        .unwrap();
        let schedule = BroadcastSchedule::compile(epoch(), &[mib(), item]).unwrap();
        let mut restored = Vec::new();
        for (index, frame) in [2, 4, 6, 10, 12].into_iter().enumerate() {
            let message = BchMessage::decode(schedule.message_at(instant(frame)).unwrap()).unwrap();
            match message.payload {
                BchPayload::Segment { kind, part, data } => {
                    assert_eq!(kind.wire_value(), 5);
                    assert!(index < 4);
                    assert_eq!(
                        part,
                        if index == 0 {
                            SegmentPart::First { count: 5 }
                        } else {
                            SegmentPart::Subsequent { index: index as u8 }
                        }
                    );
                    restored.extend(
                        (0..SEGMENT_BITS).map(|bit| (data.bytes()[bit / 8] >> (7 - bit % 8)) & 1),
                    );
                }
                BchPayload::LastShort {
                    kind,
                    index: segment,
                    data,
                } => {
                    assert_eq!(kind.wire_value(), 5);
                    assert_eq!(index, 4);
                    assert_eq!(segment, 4);
                    assert_eq!(data.bit_length(), 115);
                    restored.extend(
                        (0..data.bit_length())
                            .map(|bit| (data.bytes()[bit / 8] >> (7 - bit % 8)) & 1),
                    );
                }
                _ => panic!("missing segment"),
            }
        }
        assert_eq!(restored.len(), length);
        for (bit, &value) in restored.iter().enumerate() {
            assert_eq!(
                value,
                if bit < length {
                    (input[bit / 8] >> (7 - bit % 8)) & 1
                } else {
                    0
                }
            );
        }
    }

    #[test]
    fn collisions_and_invalid_schedules_are_not_silently_resolved() {
        for (period, positions) in [
            (3, vec![0]),
            (8192, vec![0]),
            (8, vec![1]),
            (8, vec![0, 0]),
            (8, vec![6, 2]),
            (64, vec![0, 34]),
        ] {
            assert!(matches!(
                BroadcastTiming::new(period, positions),
                Err(ScheduleError::Timing)
            ));
        }
        let colliding = BroadcastBlock::new(
            SibType::new(1).unwrap(),
            BroadcastTiming::new(16, vec![0]).unwrap(),
            &[0],
            8,
        )
        .unwrap();
        assert!(matches!(
            BroadcastSchedule::compile(epoch(), &[mib(), colliding]),
            Err(ScheduleError::Collision { system_frame: 0 })
        ));
        assert!(matches!(
            BroadcastBlock::new(
                SibType::new(0).unwrap(),
                BroadcastTiming::new(16, vec![0]).unwrap(),
                &[0],
                8
            ),
            Err(ScheduleError::Timing)
        ));
        assert!(matches!(
            BroadcastBlock::new(
                SibType::new(1).unwrap(),
                BroadcastTiming::new(16, vec![0, 2]).unwrap(),
                &[0],
                8
            ),
            Err(ScheduleError::SegmentCount)
        ));
    }

    #[test]
    fn epoch_and_tti_alignment_are_checked_before_lookup() {
        let schedule = BroadcastSchedule::compile(epoch(), &[mib()]).unwrap();
        assert_eq!(
            schedule.message_at(instant(1)),
            Err(ScheduleError::UnalignedTime)
        );
        let mut time = instant(0);
        time.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
        assert_eq!(schedule.message_at(time), Err(ScheduleError::WrongEpoch));
    }
}
