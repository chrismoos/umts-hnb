// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_dsp::{
    bits::{Bit, unpack_msb},
    hsupa::{ControlDecision, ControlInformation},
};
use umts_types::{
    RadioEpoch, UeKey,
    bits::write_u32,
    hsupa::{Etfci, Rsn},
    time::ChipInstant,
};

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn report(bits: usize) -> Report {
    let epoch = RadioEpoch::new(NonZeroU64::MIN);
    Report {
        timing: None,
        identity: ReceiveIdentity {
            ue: UeKey {
                epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        },
        start: ChipInstant { epoch, index: 0 },
        absolute_tti: 0,
        control: ControlDecision {
            information: ControlInformation {
                format: Etfci::new(1).unwrap(),
                sequence: Rsn::new(0).unwrap(),
                happy: true,
            },
            normalized_score: 1.0,
            normalized_margin: Some(0.5),
        },
        feedback: Reception::Ack { deliver: true },
        outcome: BlockOutcome::Decoded,
        combined: false,
        reset_reason: None,
        payload_bits: bits,
    }
}
fn packet(fields: &[(u32, usize)]) -> Vec<Bit> {
    let bits = fields.iter().map(|(_, width)| width).sum::<usize>();
    let mut packed = vec![0; bits.div_ceil(8)];
    let mut offset = 0;
    for &(value, width) in fields {
        write_u32(&mut packed, offset, width, value).unwrap();
        offset += width;
    }
    let mut output = vec![Bit::Zero; bits];
    unpack_msb(&packed, &mut output).unwrap();
    output
}
fn delivery(capacity: usize) -> PacketDelivery {
    PacketDelivery::new(
        report(0).identity,
        &[
            (
                1,
                Mapping {
                    logical_channel: 1,
                    flow: 0,
                    pdu_bits: nz(8),
                },
            ),
            (
                2,
                Mapping {
                    logical_channel: 2,
                    flow: 1,
                    pdu_bits: nz(8),
                },
            ),
        ],
        nz(256),
        nz(capacity),
        NonZeroU64::new(10).unwrap(),
        0,
        nz(1_000_000),
    )
    .unwrap()
}
fn admit(
    owner: &mut PacketDelivery,
    bits: &[Bit],
    now: u64,
) -> Result<PacketAdmission, PacketError> {
    owner.admit(
        ReportRef {
            report: report(bits.len()),
            payload: bits,
        },
        now,
    )
}

#[test]
fn full_forward_window_admits_missing_head_with_pending_delivery() {
    let entries = [(
        1,
        Mapping {
            logical_channel: 1,
            flow: 0,
            pdu_bits: nz(8),
        },
    )];
    let capacity = PacketDelivery::required_capacity(&entries).unwrap();
    assert_eq!(capacity, 33);
    let mut owner = PacketDelivery::new(
        report(0).identity,
        &entries,
        nz(256),
        nz(capacity),
        NonZeroU64::new(100).unwrap(),
        0,
        nz(1_000_000),
    )
    .unwrap();
    let group = |sequence| packet(&[(1, 6), (1, 6), (sequence, 6), (sequence, 8)]);
    admit(&mut owner, &group(0), 1).unwrap();
    assert!(owner.peek(1).unwrap().is_some());
    for sequence in 2..=32 {
        assert_eq!(admit(&mut owner, &group(sequence), 2).unwrap().stored, 1);
    }
    assert_eq!(admit(&mut owner, &group(1), 3).unwrap().stored, 1);
    for sequence in 0..=32 {
        assert_eq!(
            owner.peek(3).unwrap().unwrap().bits.read_u32(0, 8).unwrap(),
            sequence
        );
        assert!(owner.release());
    }
    assert!(owner.is_empty());
    let mut aliases = entries.to_vec();
    aliases.push((2, entries[0].1));
    assert_eq!(
        PacketDelivery::required_capacity(&aliases).unwrap(),
        capacity
    );
    aliases.push((
        3,
        Mapping {
            logical_channel: 2,
            ..entries[0].1
        },
    ));
    assert_eq!(PacketDelivery::required_capacity(&aliases).unwrap(), 65);
}

#[test]
fn reordered_groups_preserve_pdu_boundaries_and_consumer_backpressure() {
    let mut owner = delivery(2);
    let later = packet(&[(1, 6), (2, 6), (1, 6), (0xaabb, 16)]);
    let earlier = packet(&[(1, 6), (1, 6), (0, 6), (0x11, 8)]);
    assert_eq!(admit(&mut owner, &later, 1).unwrap().stored, 1);
    assert!(owner.peek(2).unwrap().is_none());
    assert_eq!(admit(&mut owner, &later, 2).unwrap().duplicates, 1);
    assert_eq!(admit(&mut owner, &earlier, 3).unwrap().stored, 1);
    for expected in [0x11, 0xaa, 0xbb] {
        for _ in 0..2 {
            let pdu = owner.peek(3).unwrap().unwrap();
            assert_eq!(pdu.logical_channel, 1);
            assert_eq!(pdu.bits.read_u32(0, 8).unwrap(), expected);
        }
        assert!(owner.release());
    }
    assert!(owner.is_empty());
    assert!(!owner.release());
    assert_eq!(admit(&mut owner, &earlier, 4).unwrap().old, 1);
}

#[test]
fn whole_transport_admission_retries_without_partial_logical_channel_changes() {
    let mut owner = delivery(2);
    let occupied = packet(&[(1, 6), (1, 6), (0, 6), (0x10, 8)]);
    let combined = packet(&[
        (1, 6),
        (1, 6),
        (2, 6),
        (1, 6),
        (1, 6),
        (0x20, 8),
        (0, 6),
        (0x30, 8),
    ]);
    admit(&mut owner, &occupied, 1).unwrap();
    assert_eq!(
        admit(&mut owner, &combined, 2),
        Err(PacketError::Backpressure)
    );
    assert_eq!(
        owner.peek(2).unwrap().unwrap().bits.read_u32(0, 8).unwrap(),
        0x10
    );
    owner.release();
    assert_eq!(admit(&mut owner, &combined, 2).unwrap().stored, 2);
    for (channel, expected) in [(2, 0x30), (1, 0x20)] {
        let pdu = owner.peek(2).unwrap().unwrap();
        assert_eq!(pdu.logical_channel, channel);
        assert_eq!(pdu.bits.read_u32(0, 8).unwrap(), expected);
        owner.release();
    }
    assert!(owner.is_empty());
}

#[test]
fn restricted_delivery_preserves_other_channels_until_admission_expands() {
    let mut owner = delivery(2);
    let first = packet(&[(1, 6), (1, 6), (0, 6), (0x20, 8)]);
    let second = packet(&[(2, 6), (1, 6), (0, 6), (0x30, 8)]);
    admit(&mut owner, &first, 1).unwrap();
    admit(&mut owner, &second, 1).unwrap();
    for _ in 0..2 {
        let pdu = owner
            .peek_matching(2, |logical| logical == 2)
            .unwrap()
            .unwrap();
        assert_eq!(pdu.logical_channel, 2);
        assert_eq!(pdu.bits.read_u32(0, 8).unwrap(), 0x30);
    }
    assert!(owner.release());
    assert!(
        owner
            .peek_matching(2, |logical| logical == 2)
            .unwrap()
            .is_none()
    );
    assert!(!owner.is_empty());
    let pdu = owner.peek(3).unwrap().unwrap();
    assert_eq!(pdu.logical_channel, 1);
    assert_eq!(pdu.bits.read_u32(0, 8).unwrap(), 0x20);
    assert!(owner.release());
    assert!(owner.is_empty());
}

#[test]
fn hold_expiry_si_and_wrong_generation_do_not_create_duplicate_data() {
    let mut owner = delivery(2);
    let later = packet(&[(1, 6), (1, 6), (3, 6), (0x55, 8)]);
    admit(&mut owner, &later, 1).unwrap();
    assert!(owner.peek(10).unwrap().is_none());
    assert!(owner.peek(11).unwrap().is_some());
    owner.release();
    let si = packet(&[(0x23456, 18)]);
    let mut metadata = report(si.len());
    metadata.control.information.format = Etfci::new(0).unwrap();
    assert_eq!(
        owner
            .admit(
                ReportRef {
                    report: metadata,
                    payload: &si
                },
                12
            )
            .unwrap()
            .scheduling_information,
        Some(0x23456)
    );
    assert!(owner.is_empty());
    metadata.identity.configuration = NonZeroU64::new(2).unwrap();
    assert_eq!(
        owner.admit(
            ReportRef {
                report: metadata,
                payload: &si
            },
            12
        ),
        Err(PacketError::Identity)
    );
    assert_eq!(admit(&mut owner, &later, 11), Err(PacketError::Time));
    assert_eq!(
        admit(&mut owner, &later[..10], 12),
        Err(PacketError::Payload)
    );
    assert!(owner.is_empty());
}

#[test]
fn feedback_association_uses_uplink_frame_not_late_decode_time() {
    use crate::{
        dedicated_rx::FrameAnchor,
        hsupa_rx::{FeedbackAssociation, FeedbackError},
    };
    use umts_dsp::{dedicated::DpchTiming, hsupa::Indicator};
    use umts_types::hsupa::Tti;
    for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
        let mut metadata = report(0);
        let common = metadata.start;
        let uplink = FrameAnchor {
            time: ChipInstant {
                index: 1024,
                ..common
            },
            cfn: 255,
        };
        let association = FeedbackAssociation::new(
            metadata.identity,
            uplink,
            common,
            interval,
            DpchTiming::new(0).unwrap(),
        )
        .unwrap();
        for tti in 0..10 {
            metadata.start.index = uplink.time.index + tti * interval.chips();
            metadata.absolute_tti = u64::from(interval.number(255, 0).unwrap()) + tti;
            let first = match interval {
                Tti::TwoMilliseconds => 35_840,
                Tti::TenMilliseconds => 97_280,
            };
            let expected = first + tti * interval.chips();
            let request = association
                .request(
                    metadata,
                    ChipInstant {
                        index: expected,
                        ..common
                    },
                )
                .unwrap();
            assert_eq!(request.timing.start.index, expected);
            assert_eq!(request.indicator, Indicator::Positive);
            assert!(matches!(
                association.request(
                    metadata,
                    ChipInstant {
                        index: expected + 1,
                        ..common
                    }
                ),
                Err(FeedbackError::Late { .. })
            ));
        }
        metadata.absolute_tti += 1;
        assert!(matches!(
            association.request(metadata, common),
            Err(FeedbackError::Time)
        ));
        metadata.absolute_tti -= 1;
        metadata.feedback = Reception::Nack;
        assert!(matches!(
            association.request(metadata, common),
            Err(FeedbackError::Result)
        ));
        metadata.outcome = BlockOutcome::InvalidCrc { block: 0 };
        assert_eq!(
            association.request(metadata, common).unwrap().indicator,
            Indicator::Negative
        );
    }
}
