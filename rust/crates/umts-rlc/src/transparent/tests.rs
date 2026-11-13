// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn receiver(segmentation: Segmentation, errors: ErrorDelivery) -> TmReceiver {
    TmReceiver::new(
        segmentation,
        errors,
        TmLimits {
            maximum_pdus: count(4),
            maximum_pdu_bits: count(64),
            maximum_sdu_bits: count(128),
        },
    )
}
fn pdu(bytes: &[u8], bits: usize, erroneous: bool) -> TmdPdu<'_> {
    TmdPdu {
        data: Bits::new(bytes, bits).unwrap(),
        erroneous,
    }
}

#[test]
fn segmented_non_octet_pdus_reassemble_in_one_tti_without_adding_headers_or_padding() {
    let pdus = [
        pdu(&[0xb0], 5, false),
        pdu(&[0x68], 5, false),
        pdu(&[0xe0], 5, false),
    ];
    let mut receiver = receiver(Segmentation::Enabled, ErrorDelivery::Discard);
    let mut batch = receiver.receive(&pdus).unwrap();
    let sdu = batch.next().unwrap();
    assert_eq!(sdu.data.len(), 15);
    assert_eq!(sdu.data.read_u32(0, 15), Ok(0b101100110111100));
    assert_eq!(sdu.quality, Quality::Correct);
    assert!(batch.next().is_none());
}

#[test]
fn disabled_segmentation_delivers_each_pdu_independently_in_order() {
    let pdus = [
        pdu(&[0xb0], 5, false),
        pdu(&[0x68], 5, true),
        pdu(&[0xe0], 5, false),
    ];
    for policy in [
        ErrorDelivery::Discard,
        ErrorDelivery::Indicate,
        ErrorDelivery::NoDetect,
    ] {
        let mut receiver = receiver(Segmentation::Disabled, policy);
        let batch = receiver
            .receive(&pdus)
            .unwrap()
            .map(|sdu| (sdu.data.read_u32(0, 5).unwrap(), sdu.quality))
            .collect::<Vec<_>>();
        assert_eq!(
            batch.iter().map(|(bits, _)| *bits).collect::<Vec<_>>(),
            if policy == ErrorDelivery::Discard {
                vec![22, 28]
            } else {
                vec![22, 13, 28]
            }
        );
        if policy == ErrorDelivery::NoDetect {
            assert!(
                batch
                    .iter()
                    .all(|(_, quality)| *quality == Quality::NotReported)
            );
        }
        if policy == ErrorDelivery::Indicate {
            assert_eq!(batch[1].1, Quality::Erroneous);
        }
    }
}

#[test]
fn one_erroneous_segment_marks_the_whole_sdu_and_no_data_is_retained_across_ttis() {
    let pdus = [pdu(&[0xb0], 5, false), pdu(&[0x68], 5, true)];
    for policy in [
        ErrorDelivery::Discard,
        ErrorDelivery::Indicate,
        ErrorDelivery::NoDetect,
    ] {
        let mut receiver = receiver(Segmentation::Enabled, policy);
        let mut batch = receiver.receive(&pdus).unwrap();
        if policy == ErrorDelivery::Discard {
            assert!(batch.next().is_none());
        } else {
            assert_eq!(
                batch.next().unwrap().quality,
                if policy == ErrorDelivery::Indicate {
                    Quality::Erroneous
                } else {
                    Quality::NotReported
                }
            );
        }
        let valid = [pdu(&[0xe0], 5, false)];
        let mut next = receiver.receive(&valid).unwrap();
        assert_eq!(next.next().unwrap().data.read_u32(0, 5), Ok(28));
        assert!(next.next().is_none());
    }
}

#[test]
fn length_capacity_and_equal_pdu_invariants_are_checked_before_reassembly() {
    let mut receiver = receiver(Segmentation::Enabled, ErrorDelivery::Discard);
    assert_eq!(
        receiver.receive(&[pdu(&[0; 9], 65, false)]).err(),
        Some(TmError::Capacity)
    );
    assert_eq!(
        receiver.receive(&[pdu(&[], 0, false)]).err(),
        Some(TmError::Length)
    );
    assert_eq!(
        receiver
            .receive(&[pdu(&[0], 3, false), pdu(&[0], 4, false)])
            .err(),
        Some(TmError::Length)
    );
    assert_eq!(
        receiver.receive(&[pdu(&[0; 8], 64, false); 3]).err(),
        Some(TmError::Capacity)
    );
    assert_eq!(
        receiver.receive(&[pdu(&[0], 1, false); 5]).err(),
        Some(TmError::Capacity)
    );
    assert!(receiver.receive(&[]).unwrap().next().is_none());
    let valid = [pdu(&[0xe0], 5, false)];
    assert_eq!(
        receiver
            .receive(&valid)
            .unwrap()
            .next()
            .unwrap()
            .data
            .read_u32(0, 5),
        Ok(28)
    );
}

#[test]
fn transmit_segmentation_preserves_non_octet_boundaries_and_requires_exact_sizes() {
    let input = Bits::new(&[0xb3, 0x78], 15).unwrap();
    let mut segments = segment(input, Segmentation::Enabled, count(5), count(3)).unwrap();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments.next().unwrap().read_u32(0, 5), Ok(22));
    assert_eq!(segments.len(), 2);
    assert_eq!(segments.next().unwrap().read_u32(0, 5), Ok(13));
    assert_eq!(segments.next().unwrap().read_u32(0, 5), Ok(28));
    assert!(segments.next().is_none());
    assert_eq!(
        segment(input, Segmentation::Disabled, count(5), count(3)).err(),
        Some(TmError::Length)
    );
    assert!(segment(input, Segmentation::Enabled, count(4), count(4)).is_err());
    assert!(segment(input, Segmentation::Enabled, count(5), count(2)).is_err());
    assert!(segment(Bits::empty(), Segmentation::Enabled, count(5), count(2)).is_err());
}
