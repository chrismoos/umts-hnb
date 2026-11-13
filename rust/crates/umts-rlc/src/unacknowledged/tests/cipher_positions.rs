// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn nonzero_initial_hfn_preserves_first_count_and_wire_wraps() {
    let base = 0x1234_5000;
    let mut tx =
        UmTransmitter::with_initial_position(config(LiWidth::Seven), limits(LiWidth::Seven), base)
            .unwrap();
    let mut rx =
        UmReceiver::with_initial_position(config(LiWidth::Seven), limits(LiWidth::Seven), base)
            .unwrap();
    assert_eq!(rx.next_position(), Ok(base));
    assert_eq!(rx.receive_position(0), Ok(base));
    for offset in 0..260 {
        tx.enqueue(&[offset as u8]).unwrap();
        let pdu = tx
            .transmit_positioned_with(6, |publication| {
                assert_eq!(publication.position, base + offset);
                assert_eq!(publication.bytes[0] >> 1, (offset % 128) as u8);
                Ok::<_, ()>(publication.bytes.to_vec())
            })
            .unwrap()
            .unwrap();
        if offset == 0 {
            assert!(
                rx.receive_admitted(&pdu, |_, _| Err::<(), _>("full"))
                    .is_err()
            );
            assert_eq!(rx.next_position(), Ok(base));
        }
        assert_eq!(rx.receive(&pdu).unwrap().position, base + offset);
    }
    assert_eq!(rx.next_position(), Ok(base + 260));
}

#[test]
fn initial_um_position_requires_zero_sn_without_accepting_a_fake_previous_pdu() {
    for position in [1, 127, 129, u64::MAX] {
        assert!(
            UmTransmitter::with_initial_position(
                config(LiWidth::Seven),
                limits(LiWidth::Seven),
                position
            )
            .is_err()
        );
        assert!(
            UmReceiver::with_initial_position(
                config(LiWidth::Seven),
                limits(LiWidth::Seven),
                position
            )
            .is_err()
        );
    }
    let base = 4096;
    let rx =
        UmReceiver::with_initial_position(config(LiWidth::Seven), limits(LiWidth::Seven), base)
            .unwrap();
    assert_eq!(rx.expected_sequence(), 0);
    assert_eq!(rx.receive_position(127), Ok(base + 127));
    assert_eq!(rx.counters(), UmRxCounters::default());
}

#[test]
fn extended_positions_cross_multiple_wire_wraps_and_failed_publication_does_not_advance() {
    let mut tx = UmTransmitter::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    for position in 0..300 {
        tx.enqueue(&[position as u8]).unwrap();
        let bytes = tx
            .transmit_positioned_with(6, |publication| {
                assert_eq!(publication.position, position);
                assert_eq!(publication.bytes[0] >> 1, (position % 128) as u8);
                Ok::<_, ()>(publication.bytes.to_vec())
            })
            .unwrap()
            .unwrap();
        let received = rx.receive(&bytes).unwrap();
        assert_eq!(received.position, position);
        assert_eq!(received.sdus().collect::<Vec<_>>(), [&[position as u8][..]]);
        assert_eq!(tx.next_position(), position + 1);
        assert_eq!(rx.next_position(), Ok(position + 1));
    }
    tx.enqueue(b"x").unwrap();
    assert_eq!(
        tx.transmit_positioned_with(6, |publication| {
            assert_eq!(publication.position, 300);
            Err::<(), _>(7)
        }),
        Err(UmPublishError::Downstream(7))
    );
    assert_eq!(tx.next_position(), 300);
    tx.transmit_positioned_with(6, |publication| {
        assert_eq!(publication.position, 300);
        Ok::<_, ()>(())
    })
    .unwrap()
    .unwrap();
    assert_eq!(tx.next_position(), 301);
}

#[test]
fn deciphering_precedes_li_parsing_and_upper_admission_preserves_the_cipher_position() {
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    let literal = [1, 0xf9, 5, 11, 0xfe, b'a', b'b', b'X', b'Y', b'Z', 0, 0];
    let mut encrypted = literal;
    for byte in &mut encrypted[1..] {
        *byte ^= 0xa5;
    }
    let before = rx.counters();
    assert!(matches!(
        rx.receive_prepared_admitted(&encrypted, |_| Err::<&[u8], _>(7), |_, _| Ok::<_, ()>(())),
        Err(UmPreparedReceiveError::Preparation(7))
    ));
    assert_eq!(rx.next_position(), Ok(0));
    let mut plain = literal;
    plain[0] ^= 2;
    assert!(matches!(
        rx.receive_prepared_admitted(&encrypted, |_| Ok::<_, ()>(&plain), |_, _| Ok::<_, ()>(())),
        Err(UmPreparedReceiveError::Rlc(UmError::Header))
    ));
    assert_eq!(rx.next_position(), Ok(0));
    plain.copy_from_slice(&literal);
    assert!(matches!(
        rx.receive_prepared_admitted(
            &encrypted,
            |position| {
                assert_eq!(position, 0);
                Ok::<_, ()>(&plain)
            },
            |count, bytes| {
                assert_eq!((count, bytes), (2, 5));
                Err::<(), _>(9)
            }
        ),
        Err(UmPreparedReceiveError::Admission(9))
    ));
    assert_eq!(rx.next_position(), Ok(0));
    assert_eq!(rx.counters(), before);
    let received = rx
        .receive_prepared_admitted(
            &encrypted,
            |position| {
                assert_eq!(position, 0);
                Ok::<_, ()>(&plain)
            },
            |_, _| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(received.position, 0);
    assert_eq!(
        received.sdus().collect::<Vec<_>>(),
        [b"ab".as_slice(), b"XYZ".as_slice()]
    );
    assert_eq!(rx.next_position(), Ok(1));
}

#[test]
fn basic_in_order_um_wraps_on_a_lower_sequence_but_not_on_a_duplicate() {
    let mut rx = UmReceiver::new(config(LiWidth::Seven), limits(LiWidth::Seven)).unwrap();
    for (sequence, position) in [(126, 126), (127, 127), (0, 128), (0, 128), (2, 130)] {
        let bytes = [(sequence << 1) | 1, 0xf9, 2, b'A'];
        assert_eq!(rx.receive_position(sequence), Ok(position));
        assert_eq!(rx.receive(&bytes).unwrap().position, position);
    }
    assert_eq!(rx.next_position(), Ok(131));
    assert_eq!(rx.expected_sequence(), 3);
    assert_eq!(rx.receive_position(128), Err(UmError::Header));
    let bytes = [(3 << 1) | 1, 0xf9, 2, b'B'];
    assert!(matches!(
        rx.receive_admitted(&bytes, |_, _| Err::<(), _>(7)),
        Err(UmReceiveError::Admission(7))
    ));
    assert_eq!(rx.next_position(), Ok(131));
    assert_eq!(rx.receive(&bytes).unwrap().position, 131);
}
