// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::{AsnType, Decode, Decoder, Encode};

macro_rules! sized_list {
    ($name:ident, $size:literal) => {
        #[derive(AsnType, Decode, Encode, Debug, PartialEq, Eq)]
        #[rasn(delegate, size($size))]
        struct $name(Vec<u8>);
    };
}
sized_list!(ByteCount, "0..=255");
sized_list!(WordCount, "0..=256");
sized_list!(FullWordCount, "0..=65535");
sized_list!(OffsetCount, "1..=65535");

#[derive(AsnType, Decode, Encode, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct Frame<T> {
    values: T,
    tail: Option<bool>,
}

#[test]
fn aper_length_follows_constrained_whole_number_alignment_after_root_flags() {
    // X.691 11.9.3.3 invokes 11.5 for a length with upper bound below 64K.
    assert_eq!(
        rasn::aper::encode(&Frame {
            values: ByteCount(vec![7]),
            tail: None
        })
        .unwrap(),
        [0, 1, 7]
    );
    assert_eq!(
        rasn::aper::encode(&Frame {
            values: WordCount(vec![7]),
            tail: None
        })
        .unwrap(),
        [0, 0, 1, 7]
    );
    assert_eq!(
        rasn::aper::encode(&Frame {
            values: FullWordCount(vec![7]),
            tail: None
        })
        .unwrap(),
        [0, 0, 1, 7]
    );
    assert_eq!(
        rasn::aper::encode(&Frame {
            values: OffsetCount(vec![7]),
            tail: None
        })
        .unwrap(),
        [0, 0, 0, 7]
    );
    assert_eq!(
        rasn::aper::encode(&Frame {
            values: FullWordCount(vec![]),
            tail: None
        })
        .unwrap(),
        [0, 0, 0]
    );
    let present = Frame {
        values: FullWordCount(vec![7]),
        tail: Some(true),
    };
    assert_eq!(rasn::aper::encode(&present).unwrap(), [0x40, 0, 1, 7, 0x80]);
    assert_eq!(
        rasn::aper::decode::<Frame<FullWordCount>>(&[0x40, 0, 1, 7, 0x80]).unwrap(),
        present
    );
    assert_eq!(
        rasn::aper::decode::<Frame<ByteCount>>(&[0, 1, 7]).unwrap(),
        Frame {
            values: ByteCount(vec![7]),
            tail: None
        }
    );
    assert_eq!(
        rasn::aper::decode::<Frame<WordCount>>(&[0, 0, 1, 7]).unwrap(),
        Frame {
            values: WordCount(vec![7]),
            tail: None
        }
    );
}

#[test]
fn uper_length_remains_unaligned_after_root_flags() {
    assert_eq!(
        rasn::uper::encode(&Frame {
            values: ByteCount(vec![7]),
            tail: None
        })
        .unwrap(),
        [0, 0x41, 0xc0]
    );
    assert_eq!(
        rasn::uper::encode(&Frame {
            values: FullWordCount(vec![7]),
            tail: None
        })
        .unwrap(),
        [0, 0, 0x41, 0xc0]
    );
}

#[derive(AsnType, Decode, Encode, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=15"))]
struct Nibble(u8);
#[derive(AsnType, Decode, Encode, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=16"))]
struct Nibbles(Vec<Nibble>);
#[derive(AsnType, Decode, Encode, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct SmallList {
    flag: bool,
    values: Nibbles,
}

#[test]
fn constrained_sequence_elements_do_not_gain_octet_padding() {
    // X.691 20.3 and 20.6 append component fields without container padding.
    let value = SmallList {
        flag: false,
        values: Nibbles(vec![Nibble(1), Nibble(0)]),
    };
    let bytes = [0x08, 0x80];
    assert_eq!(rasn::aper::encode(&value).unwrap(), bytes);
    assert_eq!(rasn::aper::decode::<SmallList>(&bytes).unwrap(), value);
    assert_eq!(rasn::uper::encode(&value).unwrap(), bytes);
    assert_eq!(rasn::uper::decode::<SmallList>(&bytes).unwrap(), value);
}

#[derive(AsnType, Decode, Encode, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct PilotBits {
    flag: bool,
    value: rasn::types::FixedBitString<32>,
}

#[test]
fn fixed_bit_string_over_sixteen_bits_aligns_only_in_aper() {
    // X.691 16.10 aligns a fixed bit string longer than sixteen bits.
    let mut bits = rasn::types::FixedBitString::<32>::default();
    bits.set(0, true);
    bits.set(31, true);
    let value = PilotBits {
        flag: true,
        value: bits,
    };
    let aper = [0x80, 0x80, 0, 0, 1];
    let uper = [0xc0, 0, 0, 0, 0x80];
    assert_eq!(rasn::aper::encode(&value).unwrap(), aper);
    assert_eq!(rasn::aper::decode::<PilotBits>(&aper).unwrap(), value);
    assert_eq!(rasn::uper::encode(&value).unwrap(), uper);
    assert_eq!(rasn::uper::decode::<PilotBits>(&uper).unwrap(), value);
}
