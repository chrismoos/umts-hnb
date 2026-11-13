// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

mod cipher_positions;

#[test]
fn many_tiny_sdus_split_at_the_li_bound_without_losing_order() {
    let width = LiWidth::Fifteen;
    let mut bound = limits(width);
    bound.maximum_pdu_bytes = n(1024);
    bound.queued_sdus = n(MAX_QUEUED_SDUS);
    bound.maximum_buffer_bytes = 1024 * 1024;
    let mut tx = UmTransmitter::new(config(width), bound).unwrap();
    let mut rx = UmReceiver::new(config(width), bound).unwrap();
    for value in 0..MAX_QUEUED_SDUS {
        tx.enqueue(&[value as u8]).unwrap();
    }
    let mut received = Vec::new();
    for _ in 0..4 {
        let Some(bytes) = send(&mut tx, 1024) else {
            break;
        };
        assert!(parse(&bytes, config(width)).unwrap().indicators().len() <= MAX_LIS);
        received.extend(sdus(&mut rx, &bytes));
    }
    assert_eq!(
        received,
        (0..MAX_QUEUED_SDUS)
            .map(|value| vec![value as u8])
            .collect::<Vec<_>>()
    );
    assert!(!tx.has_pending());
}

#[test]
fn allocated_padding_uses_a_sequence_and_rolls_back_on_rejection() {
    let cfg = config(LiWidth::Fifteen);
    let mut tx = UmTransmitter::new(cfg, limits(LiWidth::Fifteen)).unwrap();
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Fifteen)).unwrap();
    assert_eq!(
        tx.transmit_or_padding_positioned_with(64, |_| Err::<(), _>("full")),
        Err(UmPublishError::Downstream("full"))
    );
    assert_eq!(tx.next_position(), 0);
    let bytes = tx
        .transmit_or_padding_positioned_with(64, |p| Ok::<_, ()>(p.bytes.to_vec()))
        .unwrap()
        .unwrap();
    assert!(sdus(&mut rx, &bytes).is_empty());
    assert_eq!(tx.next_position(), 1);
    tx.enqueue(b"after padding").unwrap();
    assert_eq!(
        sdus(&mut rx, &send(&mut tx, 64).unwrap()),
        [b"after padding".to_vec()]
    );
}

#[test]
fn parser_storage_exhaustion_is_not_a_malformed_wire_disposition() {
    let mut rx = UmReceiver::new(config(LiWidth::Fifteen), limits(LiWidth::Fifteen)).unwrap();
    let mut bytes = vec![1];
    for _ in 0..=MAX_LIS {
        bytes.extend_from_slice(&[0, 1]);
    }
    assert!(matches!(
        rx.receive_admitted(&bytes, |_, _| Ok::<_, ()>(())),
        Err(UmReceiveError::Rlc(UmError::Capacity))
    ));
    assert_eq!(rx.next_position(), Ok(0));
}

#[test]
fn malformed_wire_pdu_does_not_advance_receiver_or_admit_data() {
    let cfg = config(LiWidth::Seven);
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Seven)).unwrap();
    assert!(matches!(
        rx.receive_admitted(&[1], |_, _| -> Result<(), ()> {
            panic!("invalid PDU admitted")
        }),
        Err(UmReceiveError::MalformedPdu(UmError::Header))
    ));
    assert_eq!(rx.expected_sequence(), 0);
    assert_eq!(rx.next_position(), Ok(0));
    assert_eq!(rx.counters().rejected, 1);
    let mut tx = UmTransmitter::new(cfg, limits(LiWidth::Seven)).unwrap();
    tx.enqueue(b"valid").unwrap();
    let pdu = send(&mut tx, 20).unwrap();
    assert_eq!(sdus(&mut rx, &pdu), [b"valid".to_vec()]);
    assert_eq!(rx.expected_sequence(), 1);
}

#[test]
fn pending_receive_data_tracks_fragments_until_boundary_and_not_returned_completed_bytes() {
    let cfg = config(LiWidth::Seven);
    let mut tx = UmTransmitter::new(cfg, limits(LiWidth::Seven)).unwrap();
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Seven)).unwrap();
    assert!(!rx.pending_receive_data());
    tx.enqueue(b"abcdefghijklm").unwrap();
    let first = send(&mut tx, 6).unwrap();
    assert!(sdus(&mut rx, &first).is_empty());
    assert!(rx.pending_receive_data());
    let mut decoded = Vec::new();
    while let Some(pdu) = send(&mut tx, 6) {
        decoded.extend(sdus(&mut rx, &pdu));
    }
    assert_eq!(decoded, [b"abcdefghijklm".to_vec()]);
    assert!(!rx.pending_receive_data());
}

fn n(value: usize) -> std::num::NonZeroUsize {
    std::num::NonZeroUsize::new(value).unwrap()
}

#[test]
fn upper_batch_backpressure_preserves_sequence_reassembly_and_counters() {
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    let literal = [1, 0xf9, 5, 11, 0xfe, b'a', b'b', b'X', b'Y', b'Z', 0, 0];
    let before = rx.counters();
    assert!(matches!(
        rx.receive_admitted(&literal, |count, bytes| {
            assert_eq!((count, bytes), (2, 5));
            Err::<(), _>("full")
        }),
        Err(UmReceiveError::Admission("full"))
    ));
    assert_eq!(rx.expected_sequence(), 0);
    assert_eq!(rx.counters(), before);
    let received = rx
        .receive_admitted(&literal, |count, bytes| {
            assert_eq!((count, bytes), (2, 5));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        received.sdus().collect::<Vec<_>>(),
        [b"ab".as_slice(), b"XYZ".as_slice()]
    );
    assert_eq!(rx.expected_sequence(), 1);
    assert_eq!(rx.counters().delivered, 2);
}
fn config(width: LiWidth) -> UmConfig {
    UmConfig {
        li_width: width,
        interpretation: EInterpretation::Normal,
    }
}
fn limits(width: LiWidth) -> UmLimits {
    UmLimits {
        maximum_pdu_bytes: n(if width == LiWidth::Seven { 125 } else { 512 }),
        maximum_sdu_bytes: n(1024),
        queued_sdus: n(64),
        maximum_buffer_bytes: 128 * 1024,
    }
}
fn send(tx: &mut UmTransmitter, size: usize) -> Option<Vec<u8>> {
    tx.transmit_with(size, |bytes| Ok::<_, ()>(bytes.to_vec()))
        .unwrap()
}
fn sdus(rx: &mut UmReceiver, pdu: &[u8]) -> Vec<Vec<u8>> {
    rx.receive(pdu)
        .unwrap()
        .sdus()
        .map(<[u8]>::to_vec)
        .collect()
}

#[test]
fn literal_seven_bit_headers_use_cumulative_lengths_and_explicit_padding() {
    let bytes = [
        1, 0xf9, 5, 11, 0xfe, b'a', b'b', b'X', b'Y', b'Z', 0x91, 0xab,
    ];
    let parsed = parse(&bytes, config(LiWidth::Seven)).unwrap();
    assert_eq!(parsed.sequence, 0);
    assert_eq!(
        parsed.indicators(),
        &[
            LengthIndicator::Start,
            LengthIndicator::End(2),
            LengthIndicator::End(5),
            LengthIndicator::Padding
        ]
    );
    assert_eq!(parsed.data, b"abXYZ\x91\xab");
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    assert_eq!(sdus(&mut rx, &bytes), [b"ab".to_vec(), b"XYZ".to_vec()]);
    let mut tx = UmTransmitter::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    tx.enqueue(b"ab").unwrap();
    tx.enqueue(b"XYZ").unwrap();
    let mut expected = bytes;
    expected[10..].fill(0);
    assert_eq!(send(&mut tx, bytes.len()).unwrap(), expected);
}

#[test]
fn exact_end_marker_is_carried_to_the_next_pdu_including_a_padding_only_flush() {
    let cfg = config(LiWidth::Seven);
    let mut tx = UmTransmitter::new(cfg, limits(LiWidth::Seven)).unwrap();
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Seven)).unwrap();
    tx.enqueue(b"ABCD").unwrap();
    let first = send(&mut tx, 6).unwrap();
    assert_eq!(first, [1, 0xf8, b'A', b'B', b'C', b'D']);
    assert!(sdus(&mut rx, &first).is_empty());
    assert_eq!(tx.pending_sdus(), 0);
    assert!(tx.has_pending());
    let flush = send(&mut tx, 6).unwrap();
    assert_eq!(flush, [3, 1, 0xfe, 0, 0, 0]);
    assert_eq!(sdus(&mut rx, &flush), [b"ABCD".to_vec()]);
    assert!(!tx.has_pending());
    assert!(send(&mut tx, 6).is_none());
}

#[test]
fn fifteen_bit_one_short_and_implicit_single_padding_octet_match_literal_vectors() {
    let cfg = config(LiWidth::Fifteen);
    let mut tx = UmTransmitter::new(cfg, limits(LiWidth::Fifteen)).unwrap();
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Fifteen)).unwrap();
    tx.enqueue(b"ABCD").unwrap();
    let first = send(&mut tx, 8).unwrap();
    assert_eq!(first, [1, 0xff, 0xf8, b'A', b'B', b'C', b'D', 0]);
    assert!(sdus(&mut rx, &first).is_empty());
    let flush = send(&mut tx, 8).unwrap();
    assert_eq!(flush, [3, 0xff, 0xf7, 0xff, 0xfe, 0, 0, 0]);
    assert_eq!(sdus(&mut rx, &flush), [b"ABCD".to_vec()]);
    tx.enqueue(b"1234").unwrap();
    let implicit = send(&mut tx, 10).unwrap();
    assert_eq!(implicit, [5, 0xff, 0xf9, 0, 8, b'1', b'2', b'3', b'4', 0]);
    assert_eq!(sdus(&mut rx, &implicit), [b"1234".to_vec()]);
}

#[test]
fn failed_publication_never_consumes_sdu_or_sequence_or_pending_boundary() {
    let mut tx = UmTransmitter::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    tx.enqueue(b"ABCD").unwrap();
    let mut rejected = Vec::new();
    assert_eq!(
        tx.transmit_with(6, |bytes| {
            rejected.extend_from_slice(bytes);
            Err::<(), _>("full")
        }),
        Err(UmPublishError::Downstream("full"))
    );
    assert_eq!(tx.sequence(), 0);
    assert_eq!(tx.pending_sdus(), 1);
    assert_eq!(send(&mut tx, 6).unwrap(), rejected);
    let mut flush = Vec::new();
    assert!(
        tx.transmit_with(6, |bytes| {
            flush.extend_from_slice(bytes);
            Err::<(), _>("late")
        })
        .is_err()
    );
    assert_eq!(tx.sequence(), 1);
    assert!(tx.has_pending());
    assert_eq!(send(&mut tx, 6).unwrap(), flush);
    assert_eq!(tx.counters().publication_failures, 2);
}

#[test]
fn sequence_gap_discards_only_incomplete_prefix_and_recovers_later_sdu_boundaries() {
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    assert!(sdus(&mut rx, &[0, b'A', b'B']).is_empty());
    assert_eq!(
        sdus(&mut rx, &[5, 5, 10, b'C', b'D', b'X', b'Y', b'Z']),
        [b"XYZ".to_vec()]
    );
    assert_eq!(rx.expected_sequence(), 3);
    assert_eq!(rx.counters().gaps, 1);
    assert_eq!(sdus(&mut rx, &[7, 0xf9, 2, b'Q']), [b"Q".to_vec()]);
}

#[test]
fn alternative_interpretation_and_all_complete_middle_and_start_indicators_are_received() {
    let cfg = UmConfig {
        li_width: LiWidth::Seven,
        interpretation: EInterpretation::Alternative,
    };
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Seven)).unwrap();
    assert_eq!(sdus(&mut rx, &[0, b'A', b'B']), [b"AB".to_vec()]);
    assert!(sdus(&mut rx, &[3, 0xf8, b'C']).is_empty());
    assert!(sdus(&mut rx, &[5, 0xfc, b'D']).is_empty());
    assert_eq!(sdus(&mut rx, &[7, 2, b'E']), [b"CDE".to_vec()]);
    assert_eq!(sdus(&mut rx, &[9, 0xfa, b'F', b'G']), [b"FG".to_vec()]);
    let cfg = UmConfig {
        li_width: LiWidth::Fifteen,
        interpretation: EInterpretation::Alternative,
    };
    let mut rx = UmReceiver::new(cfg, limits(LiWidth::Fifteen)).unwrap();
    assert_eq!(sdus(&mut rx, &[1, 0xff, 0xf4, b'Z', 0xa5]), [b"Z".to_vec()]);
}

#[test]
fn malformed_lengths_ordering_and_pdu_size_leave_receive_state_unchanged() {
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    for bytes in [
        &[1][..],
        &[1, 0x0a, b'A'],
        &[1, 5, 4, b'A', b'B'],
        &[1, 0xff, 2, b'A'],
        &[1, 3, 0xf8, b'A'],
        &[0; 126],
    ] {
        assert!(
            rx.receive(bytes).is_err(),
            "accepted malformed PDU: {bytes:?}"
        );
        assert_eq!(rx.expected_sequence(), 0);
    }
    assert_eq!(sdus(&mut rx, &[1, 0xf9, 2, b'A']), [b"A".to_vec()]);
}

#[test]
fn receive_buffer_admission_is_atomic_and_one_short_slack_preserves_maximum_sdu() {
    let mut small = limits(LiWidth::Fifteen);
    small.maximum_sdu_bytes = n(4);
    let mut rx = UmReceiver::new(config(LiWidth::Fifteen), small).unwrap();
    assert!(sdus(&mut rx, &[1, 0xff, 0xf8, b'A', b'B', b'C', b'D', 0]).is_empty());
    assert_eq!(
        sdus(&mut rx, &[3, 0xff, 0xf7, 0xff, 0xfe]),
        [b"ABCD".to_vec()]
    );
    let mut small = limits(LiWidth::Seven);
    small.maximum_sdu_bytes = n(4);
    let mut rx = UmReceiver::new(config(LiWidth::Seven), small).unwrap();
    assert!(sdus(&mut rx, &[0, b'A', b'B', b'C']).is_empty());
    assert_eq!(
        rx.receive(&[3, 4, b'D', b'E']).err(),
        Some(UmError::Capacity)
    );
    assert_eq!(rx.expected_sequence(), 1);
    assert_eq!(sdus(&mut rx, &[5, 0xf9, 2, b'Q']), [b"Q".to_vec()]);
}

#[test]
fn full_queue_recovery_variable_pdu_sizes_and_sequence_wrap_preserve_all_sdu_bytes() {
    for width in [LiWidth::Seven, LiWidth::Fifteen] {
        let cfg = config(width);
        let mut tx = UmTransmitter::new(cfg, limits(width)).unwrap();
        let mut rx = UmReceiver::new(cfg, limits(width)).unwrap();
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        for cycle in 0..4 {
            for index in 0..64 {
                let payload: Vec<_> = (0..1 + (index * 17 + cycle) % 80)
                    .map(|byte| (byte * 31 + index + cycle) as u8)
                    .collect();
                tx.enqueue(&payload).unwrap();
                expected.push(payload);
            }
            assert_eq!(tx.enqueue(b"full"), Err(UmError::Capacity));
            let mut calls = 0;
            while tx.has_pending() {
                let size = [7, 11, 31, 44, 125][calls % 5];
                let pdu = send(&mut tx, size).unwrap();
                actual.extend(sdus(&mut rx, &pdu));
                calls += 1;
                assert!(calls < 4096);
            }
        }
        assert_eq!(actual, expected);
        assert!(tx.counters().submitted > 128);
        assert_eq!(tx.sequence(), (tx.counters().submitted % 128) as u8);
        assert_eq!(rx.counters().gaps, 0);
    }
}

#[test]
fn unsupported_transmit_interpretation_and_invalid_budgets_fail_at_setup() {
    let cfg = config(LiWidth::Seven);
    assert_eq!(
        UmTransmitter::new(
            UmConfig {
                interpretation: EInterpretation::Alternative,
                ..cfg
            },
            limits(LiWidth::Seven)
        )
        .err(),
        Some(UmError::Unsupported)
    );
    assert_eq!(
        UmTransmitter::new(
            cfg,
            UmLimits {
                maximum_buffer_bytes: 1,
                ..limits(LiWidth::Seven)
            }
        )
        .err(),
        Some(UmError::Capacity)
    );
    assert_eq!(
        UmReceiver::new(
            cfg,
            UmLimits {
                maximum_buffer_bytes: 1,
                ..limits(LiWidth::Seven)
            }
        )
        .err(),
        Some(UmError::Capacity)
    );
    assert_eq!(
        UmTransmitter::new(
            cfg,
            UmLimits {
                maximum_pdu_bytes: n(126),
                ..limits(LiWidth::Seven)
            }
        )
        .err(),
        Some(UmError::Capacity)
    );
}
