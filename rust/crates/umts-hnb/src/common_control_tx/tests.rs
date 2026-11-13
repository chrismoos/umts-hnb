// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, source};
use umts_dsp::bits::Bit;
use umts_types::time::CHIPS_PER_FRAME;

fn limits(source: &CommonTx) -> UmLimits {
    UmLimits {
        maximum_pdu_bytes: CommonControlDownlink::maximum_pdu_bytes(source, 1).unwrap(),
        maximum_sdu_bytes: count(256),
        queued_sdus: count(4),
        maximum_buffer_bytes: 4096,
    }
}

#[test]
fn queue_failure_preserves_um_sdu_sequence_and_retries_at_a_new_frame() {
    let mut source = source(false);
    let mut downlink = CommonControlDownlink::new(&source, 1, limits(&source)).unwrap();
    downlink
        .enqueue(Bits::new(&[0xa5; 80], 640).unwrap())
        .unwrap();
    let width = usize::from(source.information().common_layout().transport_block_bits());
    source
        .queue_transport(at(0), 1, &vec![Bit::Zero; width], &[])
        .unwrap();
    assert_eq!(
        downlink.publish_frame(&mut source, at(0)),
        Err(CommonControlTxError::Transmit(CommonTxError::Conflict))
    );
    assert_eq!(downlink.sequence(), 0);
    assert_eq!(downlink.pending_sdus(), 1);
    assert_eq!(
        downlink.publish_frame(&mut source, at(CHIPS_PER_FRAME)),
        Ok(true)
    );
    assert_eq!(downlink.sequence(), 1);
    assert_eq!(
        downlink.publish_frame(&mut source, at(CHIPS_PER_FRAME)),
        Err(CommonControlTxError::Transmit(CommonTxError::Conflict))
    );
    assert_eq!(downlink.sequence(), 1);
}

#[test]
fn physical_profile_binding_and_paging_mapping_are_not_silently_reused() {
    let source = source(false);
    assert_eq!(
        CommonControlDownlink::new(&source, 0, limits(&source)).err(),
        Some(CommonControlTxError::Profile)
    );
    let mut downlink = CommonControlDownlink::new(&source, 1, limits(&source)).unwrap();
    downlink.enqueue(Bits::new(&[0xa5], 8).unwrap()).unwrap();
    let mut other = crate::test_support::source(true);
    assert_eq!(
        downlink.publish_frame(&mut other, at(0)),
        Err(CommonControlTxError::Profile)
    );
    assert_eq!(downlink.sequence(), 0);
    assert_eq!(downlink.pending_sdus(), 1);
    assert_eq!(
        CommonControlDownlink::new(
            &source,
            1,
            UmLimits {
                maximum_pdu_bytes: count(3),
                ..limits(&source)
            }
        )
        .err(),
        Some(CommonControlTxError::Profile)
    );
}

#[test]
fn rrc_octet_padding_is_owned_by_rrc_boundary_and_sdu_queue_capacity_is_bounded() {
    let source = source(true);
    let mut downlink = CommonControlDownlink::new(&source, 1, limits(&source)).unwrap();
    assert_eq!(
        downlink.enqueue(Bits::empty()),
        Err(CommonControlTxError::Payload)
    );
    for _ in 0..4 {
        downlink
            .enqueue(Bits::new(&[0xab, 0xcf], 13).unwrap())
            .unwrap();
    }
    assert_eq!(&downlink.rrc[..2], &[0xab, 0xc8]);
    assert_eq!(
        downlink.enqueue(Bits::new(&[0], 1).unwrap()),
        Err(CommonControlTxError::Rlc(UmError::Capacity))
    );
    assert_eq!(downlink.pending_sdus(), 4);
}
