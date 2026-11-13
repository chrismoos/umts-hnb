// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::collections::BTreeSet;

fn assignment(first: u8, codes: u8, version: u8) -> ScchAssignment {
    ScchAssignment::new(
        first,
        codes,
        Modulation::Qam16,
        42,
        5,
        TransmissionVersion::new(version).unwrap(),
        true,
    )
    .unwrap()
}

fn integer(bits: &[Bit]) -> u16 {
    bits.iter().fold(0, |value, &bit| (value << 1) | bit as u16)
}

#[test]
fn dual_cell_limits_and_transport_tables_are_per_carrier() {
    use umts_types::hsdpa::DualCellCategory;
    for (value, maximum, table) in [
        (21, 23_370, TransportTable::BitAligned),
        (22, 27_952, TransportTable::BitAligned),
        (23, 35_280, TransportTable::OctetAligned),
        (24, 42_192, TransportTable::OctetAligned),
    ] {
        let category = DualCellCategory::new(value).unwrap().per_cell_category();
        let assignment = assignment(1, 15, 0).with_table(table).unwrap();
        assert!(assignment.validate_category(category, maximum).is_ok());
        assert_eq!(
            assignment.validate_category(category, maximum + 1),
            Err(DspError::InvalidCode)
        );
        let wrong = if table == TransportTable::BitAligned {
            TransportTable::OctetAligned
        } else {
            TransportTable::BitAligned
        };
        assert_eq!(
            assignment
                .with_table(wrong)
                .unwrap()
                .validate_category(category, 1000),
            Err(DspError::InvalidCode)
        );
    }
}

#[test]
fn code_set_mapping_is_unique_for_all_contiguous_assignments() {
    let mut fields = BTreeSet::new();
    for count in 1..=15 {
        for first in 1..=16 - count {
            let bits = assignment(first, count, 0).information();
            assert!(fields.insert(integer(&bits[..7])));
        }
    }
    assert_eq!(fields.len(), 120);
    let bits = assignment(1, 15, 6).information();
    assert_eq!(integer(&bits[..7]), 15);
    assert_eq!(integer(&bits[7..8]), 1);
    assert_eq!(integer(&bits[8..14]), 42);
    assert_eq!(integer(&bits[14..17]), 5);
    assert_eq!(integer(&bits[17..20]), 6);
    assert_eq!(integer(&bits[20..]), 1);
}

#[test]
fn version_tables_preserve_joint_redundancy_and_constellation() {
    let expected = [
        (true, 0, ConstellationVersion::Original),
        (false, 0, ConstellationVersion::Original),
        (true, 1, ConstellationVersion::Swap),
        (false, 1, ConstellationVersion::Swap),
        (true, 0, ConstellationVersion::Swap),
        (true, 0, ConstellationVersion::Invert),
        (true, 0, ConstellationVersion::SwapInvert),
        (true, 1, ConstellationVersion::Original),
    ];
    for (index, (systematic, r, b)) in expected.into_iter().enumerate() {
        let version = TransmissionVersion::new(index as u8).unwrap();
        let (redundancy, constellation) = version.parameters(Modulation::Qam16);
        assert_eq!(
            redundancy,
            Redundancy {
                prioritize_systematic: systematic,
                version: r
            }
        );
        assert_eq!(constellation, b);
        let (redundancy, constellation) = version.parameters(Modulation::Qpsk);
        assert_eq!(redundancy.version, index as u8 / 2);
        assert_eq!(redundancy.prioritize_systematic, index % 2 == 0);
        assert_eq!(constellation, ConstellationVersion::Original);
    }
    assert!(TransmissionVersion::new(8).is_err());
}

#[test]
fn puncturing_preserves_spec_position_order() {
    let input = std::array::from_fn::<_, CODED_PART2, _>(|index| Bit::from(index % 3 == 0));
    let mut output = [Bit::Zero; SCCH_BITS - FIRST_SLOT_BITS];
    puncture(&input, &PUNCTURE2, &mut output);
    let expected: Vec<_> = input
        .iter()
        .enumerate()
        .filter(|(index, _)| !PUNCTURE2.contains(&(index + 1)))
        .map(|(_, &value)| value)
        .collect();
    assert_eq!(output.as_slice(), expected);
}

#[test]
fn identity_changes_both_masked_parts_without_changing_assignment() {
    let allocation = assignment(2, 5, 2);
    let first = ScchEncoder::new(0x1234)
        .unwrap()
        .encode(allocation)
        .unwrap();
    let second = ScchEncoder::new(0xabcd)
        .unwrap()
        .encode(allocation)
        .unwrap();
    assert_ne!(first[..FIRST_SLOT_BITS], second[..FIRST_SLOT_BITS]);
    assert_ne!(first[FIRST_SLOT_BITS..], second[FIRST_SLOT_BITS..]);
    assert_eq!(
        first,
        ScchEncoder::new(0x1234)
            .unwrap()
            .encode(allocation)
            .unwrap()
    );
}

#[test]
fn secondary_orders_have_standard_reserved_fields() {
    let activate = SecondaryCellOrder::Activate.information();
    let deactivate = SecondaryCellOrder::Deactivate.information();
    let expected = [
        1, 1, 1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, 1, 0, 0, 1, 0, 0, 1, 0,
    ]
    .map(|bit| Bit::from(bit != 0));
    assert_eq!(activate, expected);
    for index in 0..activate.len() {
        assert_eq!(
            deactivate[index],
            if index == 19 {
                Bit::Zero
            } else {
                activate[index]
            }
        );
    }
    let encoder = ScchEncoder::new(0x1234).unwrap();
    assert_ne!(
        encoder.encode_order(SecondaryCellOrder::Activate).unwrap(),
        encoder
            .encode_order(SecondaryCellOrder::Deactivate)
            .unwrap()
    );
}
