// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn division(input: &[u8], width: usize, polynomial: u16) -> u16 {
    let mut bits: Vec<_> = input
        .iter()
        .flat_map(|byte| (0..u8::BITS).rev().map(move |bit| byte & (1 << bit) != 0))
        .collect();
    let data = bits.len();
    bits.extend(std::iter::repeat_n(false, width));
    for index in 0..data {
        if bits[index] {
            for term in 0..=width {
                if polynomial & (1 << (width - term)) != 0 {
                    bits[index + term] = !bits[index + term];
                }
            }
        }
    }
    bits[data..]
        .iter()
        .fold(0, |value, bit| (value << 1) | u16::from(*bit))
}

#[test]
fn crc_matches_systematic_polynomial_division_across_all_control_octets() {
    for word in 0..=u16::MAX {
        let bytes = word.to_be_bytes();
        assert_eq!(
            crc(&bytes, CRC6_WIDTH, CRC6_POLY),
            division(&bytes, 6, 0x6f)
        );
    }
    for length in 0..=64 {
        let bytes: Vec<_> = (0..length)
            .map(|index| (index * 37 + length) as u8)
            .collect();
        assert_eq!(
            crc(&bytes, CRC10_WIDTH, CRC10_POLY),
            division(&bytes, 10, 0x633)
        );
    }
}

#[test]
fn corrupt_header_is_discarded_but_payload_corruption_is_reported_separately() {
    let frame = Frame::Data {
        number: 15,
        quality: FrameQuality::BadRadio,
        rfci: 63,
        payload_crc: PayloadCheck::Valid,
        payload: &[0x12, 0x34, 0x56],
    };
    let mut output = [0; 32];
    let size = encode(frame, &mut output).unwrap();
    assert_eq!(decode(&output[..size]).unwrap(), frame);
    for bit in 0..16 {
        let mut bad = output;
        bad[bit / 8] ^= 1 << (7 - bit % 8);
        assert!(matches!(
            decode(&bad[..size]),
            Err(WireError::HeaderCrc | WireError::Unsupported(_))
        ));
    }
    output[size - 1] ^= 1;
    let Frame::Data {
        payload_crc,
        quality,
        ..
    } = decode(&output[..size]).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(payload_crc, PayloadCheck::Failed);
    assert_eq!(quality, FrameQuality::BadRadio);
}

#[test]
fn control_acknowledgments_do_not_have_a_payload_crc_even_for_negative_responses() {
    for acknowledgment in [
        Acknowledgment::Procedure,
        Acknowledgment::Positive,
        Acknowledgment::Negative,
    ] {
        let frame = Frame::Control {
            number: 3,
            acknowledgment,
            mode: 1,
            procedure: 0,
            payload_crc: if acknowledgment == Acknowledgment::Procedure {
                PayloadCheck::Valid
            } else {
                PayloadCheck::Absent
            },
            payload: &[0x40, 0],
        };
        let mut output = [0; 32];
        let size = encode(frame, &mut output).unwrap();
        assert_eq!(decode(&output[..size]).unwrap(), frame);
        if acknowledgment != Acknowledgment::Procedure {
            assert_eq!(output[2] & PAYLOAD_CRC_HIGH, 0);
            assert_eq!(output[3], 0);
        }
    }
    assert_eq!(decode(&[0x70, 0, 0]), Err(WireError::Unsupported(7)));
}

#[test]
fn unprotected_data_has_only_the_three_octet_header() {
    let frame = Frame::Data {
        number: 0,
        quality: FrameQuality::Good,
        rfci: 0,
        payload_crc: PayloadCheck::Absent,
        payload: &[],
    };
    let mut output = [0; 3];
    assert_eq!(encode(frame, &mut output).unwrap(), 3);
    assert_eq!(decode(&output).unwrap(), frame);
}
