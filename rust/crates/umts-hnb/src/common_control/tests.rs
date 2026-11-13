// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count};
use umts_types::time::ChipInstant;

const REQUEST: [u8; 10] = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];

pub(crate) fn ingress(capacity: usize) -> ControlIngress {
    ControlIngress::new(
        at(0).epoch,
        TmLimits {
            maximum_pdus: count(4),
            maximum_pdu_bits: count(254),
            maximum_sdu_bits: count(1016),
        },
        count(capacity),
    )
    .unwrap()
}

fn transport(blocks: usize) -> Vec<u8> {
    let mut bytes = vec![0; blocks * 32];
    for block in bytes.chunks_exact_mut(32) {
        umts_mac::encode_common(
            FddCommonChannel::Rach,
            CommonHeader::CommonControl,
            Bits::new(&REQUEST, 78).unwrap(),
            block,
        )
        .unwrap();
    }
    bytes
}

fn batch(bytes: &[u8], blocks: usize, start: ChipInstant) -> RachMacBatch<'_> {
    RachMacBatch {
        transport: DecodedPrachMessage {
            start,
            code: umts_dsp::prach::PrachCode::new(7504).unwrap(),
            signature: umts_dsp::prach::PreambleSignature::new(13).unwrap(),
            tfci: umts_dsp::tfci::TfciDecision {
                tfci: umts_dsp::tfci::Tfci::new(0).unwrap(),
                normalized_score: 1.0,
                normalized_margin: None,
            },
            payload_bits: blocks * 256,
        },
        block_bits: 256,
        blocks,
        data: Bits::new(bytes, blocks * 256).unwrap(),
    }
}

pub(crate) fn enqueue_sdu(
    control: &mut ControlIngress,
    bytes: &[u8],
    bits: usize,
    start: ChipInstant,
) {
    let header =
        umts_mac::common_header_bits(FddCommonChannel::Rach, CommonHeader::CommonControl).unwrap();
    let length = header + bits;
    let mut wire = vec![0; length.div_ceil(BITS_PER_BYTE)];
    assert_eq!(
        umts_mac::encode_common(
            FddCommonChannel::Rach,
            CommonHeader::CommonControl,
            Bits::new(bytes, bits).unwrap(),
            &mut wire,
        )
        .unwrap(),
        length
    );
    let placeholder = [0; 32];
    let mut input = batch(&placeholder, 1, start);
    input.block_bits = length;
    input.transport.payload_bits = length;
    input.data = Bits::new(&wire, length).unwrap();
    control.enqueue(&input).unwrap();
}

#[test]
fn retry_preserves_original_bits_padding_metadata_and_one_cached_decode() {
    let mut control = ingress(2);
    let mut original = REQUEST.to_vec();
    original.extend([0xa5, 0x40]);
    enqueue_sdu(&mut control, &original, 90, at(1000));
    enqueue_sdu(&mut control, &REQUEST, 78, at(2000));
    for _ in 0..10 {
        assert_eq!(
            control.process_one_with(|input| {
                assert_eq!(input.transport().start, at(1000));
                assert_eq!(input.transport().signature.number(), 13);
                assert_eq!(input.transport().code.number(), 7504);
                assert_eq!(input.bytes(), original);
                assert_eq!(input.bit_length(), 90);
                assert_eq!(input.message().as_ref().unwrap().consumed_bits, 78);
                ControlDecision::Retry("full")
            }),
            Some(ControlDecision::Retry("full"))
        );
        assert_eq!(control.pending(), 2);
        assert_eq!(control.counters().decoded, 1);
    }
    assert_eq!(
        control.process_one_with(|_| ControlDecision::Consume("accepted")),
        Some(ControlDecision::Consume("accepted"))
    );
    assert_eq!(control.decode_one().unwrap().transport.start, at(2000));
    assert_eq!(control.counters().decoded, 2);
    assert_eq!(control.pending(), 0);
}

#[test]
fn malformed_head_can_be_retried_then_discarded_without_redecoding_or_poisoning_the_fifo() {
    let mut control = ingress(2);
    enqueue_sdu(&mut control, &[0xff; 4], 32, at(1000));
    enqueue_sdu(&mut control, &REQUEST, 78, at(2000));
    assert_eq!(
        control.process_one_with(|input| {
            assert!(input.message().is_err());
            ControlDecision::Retry(())
        }),
        Some(ControlDecision::Retry(()))
    );
    assert_eq!(control.counters().malformed, 1);
    assert!(control.decode_one().unwrap().message.is_err());
    assert_eq!(control.counters().malformed, 1);
    assert!(control.decode_one().unwrap().message.is_ok());
    assert_eq!(control.pending(), 0);
}

#[test]
fn one_tti_queues_independent_tm_sdus_and_decodes_exactly_one_per_service_call() {
    let mut control = ingress(2);
    let bytes = transport(2);
    control.enqueue(&batch(&bytes, 2, at(1000))).unwrap();
    assert_eq!(control.pending(), 2);
    for left in [1, 0] {
        let event = control.decode_one().unwrap();
        assert_eq!(event.transport.start, at(1000));
        assert_eq!(event.transport.signature.number(), 13);
        assert_eq!(event.transport.code.number(), 7504);
        assert_eq!(event.message.unwrap().consumed_bits, 78);
        assert_eq!(control.pending(), left);
    }
    assert!(control.decode_one().is_none());
    assert_eq!(control.counters().decoded, 2);
}

#[test]
fn malformed_last_mac_block_and_queue_exhaustion_publish_no_partial_tti() {
    let mut control = ingress(2);
    let mut bytes = transport(2);
    bytes[32] |= 0x80;
    assert!(matches!(
        control.enqueue(&batch(&bytes, 2, at(1000))),
        Err(ControlError::Mac(_))
    ));
    assert_eq!(control.pending(), 0);
    bytes[32] &= !0x80;
    let pdu = batch(&bytes, 2, at(1000));
    control.enqueue(&pdu).unwrap();
    assert_eq!(control.enqueue(&pdu), Err(ControlError::Capacity));
    assert_eq!(control.pending(), 2);
    assert!(control.decode_one().unwrap().message.is_ok());
    assert_eq!(control.enqueue(&pdu), Err(ControlError::Capacity));
    assert_eq!(control.pending(), 1);
    assert!(control.decode_one().unwrap().message.is_ok());
    control.enqueue(&pdu).unwrap();
    assert_eq!(control.counters().queued, 4);
}

#[test]
fn dedicated_rach_payload_is_not_coerced_to_common_control_and_wrong_epoch_is_rejected() {
    let mut control = ingress(2);
    let mut bytes = transport(1);
    umts_mac::encode_common(
        FddCommonChannel::Rach,
        CommonHeader::Dedicated {
            identity: umts_mac::CommonIdentity::Cell(7),
            channel: umts_mac::LogicalChannelId::new(2).unwrap(),
        },
        Bits::new(&[0xa5], 8).unwrap(),
        &mut bytes,
    )
    .unwrap();
    let pdu = batch(&bytes, 1, at(1000));
    assert!(matches!(
        pdu.pdus().next().unwrap().unwrap().header,
        CommonHeader::Dedicated { .. }
    ));
    assert_eq!(control.enqueue(&pdu), Err(ControlError::Mapping));
    assert_eq!(control.pending(), 0);
    let mut wrong = at(1000);
    wrong.epoch = RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
    assert_eq!(
        control.enqueue(&batch(&bytes, 1, wrong)),
        Err(ControlError::Epoch)
    );
}

#[test]
fn malformed_rrc_is_an_explicit_event_and_later_control_sdus_still_decode() {
    let mut control = ingress(2);
    let mut bytes = transport(2);
    bytes[..32].fill(0xff);
    bytes[0] = 0x18;
    control.enqueue(&batch(&bytes, 2, at(1000))).unwrap();
    assert!(control.decode_one().unwrap().message.is_err());
    assert!(control.decode_one().unwrap().message.is_ok());
    assert_eq!(control.counters().malformed, 1);
    assert_eq!(control.counters().decoded, 1);
}
