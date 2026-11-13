// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn row(rfci: u8, lengths: &[u16], ipti: Option<u8>) -> Combination {
    Combination::new(Rfci::new(rfci).unwrap(), lengths, ipti).unwrap()
}

fn initialization(rows: &[Combination]) -> Initialization {
    Initialization::new(rows, Versions::new(3).unwrap(), DataPduType::Crc, false).unwrap()
}

#[test]
fn mode_versions_cover_all_wire_values_and_reject_empty_intersections() {
    assert_eq!(Versions::new(0), Err(WireError::Header));
    for version in 1..=16 {
        let value = ModeVersion::new(version).unwrap();
        assert_eq!(value.wire(), version - 1);
        assert_eq!(value.number(), version);
        assert_eq!(ModeVersion::from_wire(version - 1), Ok(value));
        let versions = Versions::new(1 << (version - 1)).unwrap();
        assert!(versions.supports(value));
        assert_eq!(versions.highest(), value);
    }
    for invalid in [0, 17, 255] {
        assert_eq!(ModeVersion::new(invalid), Err(WireError::Header));
    }
    for invalid in [16, 254, 255] {
        assert_eq!(ModeVersion::from_wire(invalid), Err(WireError::Header));
    }
    assert_eq!(
        Versions::new(1)
            .unwrap()
            .intersection(Versions::new(2).unwrap()),
        None
    );
    assert_eq!(
        Versions::new(7)
            .unwrap()
            .intersection(Versions::new(6).unwrap())
            .unwrap()
            .mask(),
        6
    );
}

#[test]
fn initialization_matches_spec_field_octets_not_only_roundtrips() {
    // TS 25.415 figure 24 places IPTI nibbles after the complete RFCI list.
    let expected = [0x16, 0x00, 81, 103, 60, 0x81, 39, 0, 0, 0x23, 0, 3, 0];
    let frame = initialization(&[
        row(0, &[81, 103, 60], Some(2)),
        row(1, &[39, 0, 0], Some(3)),
    ]);
    assert_eq!(frame.encoded_bytes(), expected.len());
    let mut output = [0xff; 64];
    let bytes = frame.encode(&mut output).unwrap();
    assert_eq!(&output[..bytes], expected);
    assert!(output[bytes..].iter().all(|byte| *byte == 0xff));
    assert_eq!(Initialization::decode(&expected), Ok(frame));

    let expected = [0x03, 0xc5, 0x01, 0x00, 0x80, 0x01, 0x10];
    let frame = Initialization::new(
        &[row(5, &[256], None)],
        Versions::new(0x8001).unwrap(),
        DataPduType::NoCrc,
        true,
    )
    .unwrap();
    let bytes = frame.encode(&mut output).unwrap();
    assert_eq!(&output[..bytes], expected);
    assert_eq!(Initialization::decode(&expected), Ok(frame));
}

#[test]
fn ignored_spares_and_odd_ipti_padding_do_not_change_the_table() {
    let frame = initialization(&[row(2, &[0], Some(15))]);
    let mut output = [0; 64];
    let bytes = frame.encode(&mut output).unwrap();
    output[0] |= 0xe0;
    output[3] |= 0x0f;
    output[bytes - 1] |= 0x0f;
    for extension in 0..=CONTROL_SPARE_EXTENSION {
        assert_eq!(
            Initialization::decode(&output[..bytes + extension]),
            Ok(frame.clone())
        );
    }
    assert_eq!(
        Initialization::decode(&output[..bytes + CONTROL_SPARE_EXTENSION + 1]),
        Err(WireError::Length)
    );
}

#[test]
fn every_initialization_truncation_and_small_output_is_rejected() {
    let frame = initialization(&[row(62, &[65535; MAX_SUBFLOWS], Some(15))]);
    let mut output = [0; 64];
    let bytes = frame.encode(&mut output).unwrap();
    for size in 0..bytes {
        assert!(
            Initialization::decode(&output[..size]).is_err(),
            "accepted {size}"
        );
        assert_eq!(frame.encode(&mut vec![0; size]), Err(WireError::Capacity));
    }
    assert_eq!(Initialization::decode(&output[..bytes]), Ok(frame));
}

#[test]
fn invalid_tables_cannot_be_constructed_or_received() {
    let good = row(0, &[1], None);
    assert_eq!(
        Initialization::new(&[], Versions::new(1).unwrap(), DataPduType::Crc, false),
        Err(WireError::Length)
    );
    for bad in [
        vec![good, good],
        vec![good, row(1, &[1, 2], None)],
        vec![good, row(1, &[1], Some(0))],
    ] {
        assert_eq!(
            Initialization::new(&bad, Versions::new(1).unwrap(), DataPduType::Crc, false),
            Err(WireError::Header)
        );
    }
    assert_eq!(Rfci::new(63), Err(WireError::Header));
    assert_eq!(
        Combination::new(Rfci::new(0).unwrap(), &[], None),
        Err(WireError::Header)
    );
    assert_eq!(
        Combination::new(Rfci::new(0).unwrap(), &[0; MAX_SUBFLOWS + 1], None),
        Err(WireError::Header)
    );
    assert_eq!(
        Combination::new(Rfci::new(0).unwrap(), &[1], Some(16)),
        Err(WireError::Header)
    );
    for bad in [
        &[0, 0x80, 1, 0, 1, 0][..],
        &[2, 0xbf, 1, 0, 1, 0],
        &[2, 0, 1, 0x80, 2, 0, 1, 0],
        &[2, 0x80, 1, 0, 0, 0],
        &[2, 0x80, 1, 0, 1, 0x20],
    ] {
        assert!(Initialization::decode(bad).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn chain_append_is_atomic_bounded_and_does_not_require_ipti_in_every_frame() {
    let frame = initialization(&[row(0, &[1], Some(2))]);
    let formats = Formats::from_initialization(&frame);
    let next = initialization(&[row(1, &[2], None)]);
    let joined = formats.append(&next).unwrap();
    assert_eq!(
        joined.rows().copied().collect::<Vec<_>>(),
        vec![row(0, &[1], Some(2)), row(1, &[2], None)]
    );
    assert_eq!(formats.rows().count(), 1);
    assert_eq!(formats.append(&frame), Err(WireError::Header));
    assert_eq!(
        formats.append(&initialization(&[row(2, &[1, 1], None)])),
        Err(WireError::Header)
    );
    let rows: Vec<_> = (0..MAX_RFCIS as u8)
        .map(|rfci| row(rfci, &[1], None))
        .collect();
    let full = initialization(&rows);
    let mut output = [0; 256];
    let bytes = full.encode(&mut output).unwrap();
    assert_eq!(Initialization::decode(&output[..bytes]), Ok(full.clone()));
    assert_eq!(
        Formats::from_initialization(&full).append(&next),
        Err(WireError::Capacity)
    );
    let mut too_many = rows;
    too_many.push(good_row());
    assert_eq!(
        Initialization::new(
            &too_many,
            Versions::new(1).unwrap(),
            DataPduType::Crc,
            false
        ),
        Err(WireError::Capacity)
    );
}

fn good_row() -> Combination {
    row(0, &[1], None)
}

fn data<'a>(payload: &'a [u8]) -> Frame<'a> {
    Frame::Data {
        number: 7,
        quality: FrameQuality::Good,
        rfci: 0,
        payload_crc: PayloadCheck::Valid,
        payload,
    }
}

#[test]
fn media_subflows_are_bit_contiguous_and_do_not_copy_on_receive() {
    let formats = Formats::from_initialization(&initialization(&[row(0, &[3, 0, 6], None)]));
    let subflows = [
        Bits::new(&[0xa0], 3).unwrap(),
        Bits::empty(),
        Bits::new(&[0xcc], 6).unwrap(),
    ];
    let mut output = [0xff; 3];
    assert_eq!(
        formats.pack(Rfci::new(0).unwrap(), &subflows, &mut output),
        Ok(2)
    );
    assert_eq!(output, [0xb9, 0x80, 0xff]);
    let media = formats.downlink(data(&output[..2])).unwrap();
    assert_eq!(media.number, 7);
    assert_eq!(media.subflows().len(), 3);
    assert_eq!(media.subflows()[0].read_u32(0, 3), Ok(5));
    assert!(media.subflows()[1].is_empty());
    assert_eq!(media.subflows()[2].read_u32(0, 6), Ok(51));
    assert_eq!(formats.maximum_payload_bytes(), 2);
    assert_eq!(formats.first().rfci(), Rfci::new(0).unwrap());
    assert_eq!(
        formats.pack(Rfci::new(0).unwrap(), &subflows, &mut [0]),
        Err(WireError::Capacity)
    );
    assert_eq!(
        formats.pack(Rfci::new(0).unwrap(), &subflows[..2], &mut output),
        Err(WireError::Length)
    );
    assert_eq!(
        formats.pack(Rfci::new(1).unwrap(), &subflows, &mut output),
        Err(WireError::Header)
    );
}

#[test]
fn downlink_drops_bad_crc_quality_unknown_rfci_and_wrong_pdu_geometry() {
    let formats = Formats::from_initialization(&initialization(&[row(0, &[9], None)]));
    for quality in [
        FrameQuality::Bad,
        FrameQuality::BadRadio,
        FrameQuality::Spare,
    ] {
        let Frame::Data {
            number,
            rfci,
            payload_crc,
            payload,
            ..
        } = data(&[0, 0])
        else {
            unreachable!()
        };
        assert!(matches!(
            formats.downlink(Frame::Data {
                number,
                rfci,
                payload_crc,
                payload,
                quality
            }),
            Err(MediaError::Quality)
        ));
    }
    for (payload_crc, expected) in [
        (PayloadCheck::Failed, MediaError::PayloadCrc),
        (PayloadCheck::Absent, MediaError::Pdu),
    ] {
        let Frame::Data {
            number,
            rfci,
            quality,
            payload,
            ..
        } = data(&[0, 0])
        else {
            unreachable!()
        };
        assert!(
            matches!(formats.downlink(Frame::Data {number, rfci, quality, payload, payload_crc}), Err(actual) if actual == expected)
        );
    }
    let Frame::Data {
        number,
        quality,
        payload_crc,
        payload,
        ..
    } = data(&[0, 0])
    else {
        unreachable!()
    };
    for rfci in [1, 63] {
        assert!(matches!(
            formats.downlink(Frame::Data {
                number,
                quality,
                payload_crc,
                payload,
                rfci
            }),
            Err(MediaError::Rfci)
        ));
    }
    assert!(matches!(
        formats.downlink(data(&[0])),
        Err(MediaError::Length)
    ));
    for bytes in 2..=2 + DATA_SPARE_EXTENSION {
        assert!(formats.downlink(data(&[0xff; 6][..bytes])).is_ok());
    }
    assert!(matches!(
        formats.downlink(data(&[0; 7])),
        Err(MediaError::Length)
    ));
    let unchecked = Formats::from_initialization(
        &Initialization::new(
            &[row(0, &[0], None)],
            Versions::new(1).unwrap(),
            DataPduType::NoCrc,
            false,
        )
        .unwrap(),
    );
    assert!(
        unchecked
            .downlink(Frame::Data {
                number: 0,
                quality: FrameQuality::Good,
                rfci: 0,
                payload_crc: PayloadCheck::Absent,
                payload: &[]
            })
            .is_ok()
    );
    assert!(matches!(
        unchecked.downlink(data(&[])),
        Err(MediaError::Pdu)
    ));
}
