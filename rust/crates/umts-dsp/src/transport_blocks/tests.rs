// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::convolutional;
fn count(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}
fn payload(bits: usize) -> Vec<Bit> {
    (0..bits).map(|index| Bit::from(index % 7 < 3)).collect()
}
fn soft(bits: &[Bit]) -> Vec<f32> {
    bits.iter()
        .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
        .collect()
}

#[test]
fn convolutional_sparse_evidence_must_not_publish_an_ambiguous_zero_word() {
    for crc in [
        TransportCrc::None,
        TransportCrc::Crc8,
        TransportCrc::Crc12,
        TransportCrc::Crc16,
        TransportCrc::Crc24,
    ] {
        for (block_bits, blocks) in [(40, 1), (40, 2), (505, 1)] {
            for rate in [CodeRate::Half, CodeRate::Third] {
                let plan = BlockPlan::new(
                    BlockFormat {
                        block_bits,
                        blocks,
                        crc,
                        rate,
                    },
                    count(10000),
                )
                .unwrap();
                let mut evidence = vec![0.0; plan.coded_bits()];
                evidence[0] = 4.0;
                // Two different CRC-valid payloads agree on the only observed coded bit.
                // Both therefore have exactly the same minimum path cost, zero.
                for one_at in [None, Some(plan.payload_bits() - 1)] {
                    let mut payload = vec![Bit::Zero; plan.payload_bits()];
                    if let Some(index) = one_at {
                        payload[index] = Bit::One;
                    }
                    let mut coded = vec![Bit::Zero; plan.coded_bits()];
                    BlockEncoder::new(plan)
                        .encode(&payload, &mut coded)
                        .unwrap();
                    assert_eq!(coded[0], Bit::Zero);
                    let mut observed = vec![Bit::One; payload.len()];
                    assert_eq!(
                        BlockDecoder::new(plan).decode(&soft(&coded), &mut observed),
                        Ok(BlockOutcome::Decoded)
                    );
                    assert_eq!(observed, payload);
                }
                let mut output = vec![Bit::One; plan.payload_bits()];
                let mut decoder = BlockDecoder::new(plan);
                assert_eq!(
                    decoder.decode(&evidence, &mut output),
                    Ok(BlockOutcome::Erased)
                );
                assert_eq!(decoder.crc_counts(), CrcCounts::default());
                assert!(output.iter().all(|bit| *bit == Bit::One));
            }
        }
    }
}

#[test]
fn crc_counts_include_blocks_after_first_failure_without_publishing_payload() {
    let layout = CrcBlockLayout::new(40, 3, TransportCrc::Crc16).unwrap();
    let input = payload(layout.payload);
    let mut joined = vec![Bit::Zero; layout.joined];
    layout.attach(&input, &mut joined).unwrap();
    let width = 40 + TransportCrc::Crc16.width();
    joined[width - 1] = Bit::from(joined[width - 1] == Bit::Zero);
    joined[3 * width - 1] = Bit::from(joined[3 * width - 1] == Bit::Zero);
    let mut output = vec![Bit::One; layout.payload];
    let mut counts = CrcCounts::default();
    assert_eq!(
        layout.publish(&joined, &mut output, &mut counts),
        Ok(BlockOutcome::InvalidCrc { block: 0 })
    );
    assert_eq!(
        counts,
        CrcCounts {
            passed: 1,
            failed: 2
        }
    );
    assert!(output.iter().all(|bit| *bit == Bit::One));
    layout.attach(&input, &mut joined).unwrap();
    assert_eq!(
        layout.publish(&joined, &mut output, &mut counts),
        Ok(BlockOutcome::Decoded)
    );
    assert_eq!(
        counts,
        CrcCounts {
            passed: 3,
            failed: 0
        }
    );
    assert_eq!(output, input);
}

#[test]
fn every_crc_and_convolutional_rate_preserves_multiblock_transport_order() {
    for crc in [
        TransportCrc::None,
        TransportCrc::Crc8,
        TransportCrc::Crc12,
        TransportCrc::Crc16,
        TransportCrc::Crc24,
    ] {
        for rate in [CodeRate::Half, CodeRate::Third] {
            let plan = BlockPlan::new(
                BlockFormat {
                    block_bits: 253,
                    blocks: 3,
                    crc,
                    rate,
                },
                count(10000),
            )
            .unwrap();
            let input = payload(plan.payload_bits());
            let mut coded = vec![Bit::Zero; plan.coded_bits()];
            BlockEncoder::new(plan).encode(&input, &mut coded).unwrap();
            let mut output = vec![Bit::One; plan.payload_bits()];
            assert_eq!(
                BlockDecoder::new(plan).decode(&soft(&coded), &mut output),
                Ok(BlockOutcome::Decoded)
            );
            assert_eq!(output, input);
        }
    }
}

#[test]
fn invalid_final_crc_never_publishes_earlier_valid_blocks() {
    let format = BlockFormat {
        block_bits: 40,
        blocks: 2,
        crc: TransportCrc::Crc16,
        rate: CodeRate::Half,
    };
    let plan = BlockPlan::new(format, count(10000)).unwrap();
    let mut joined = Vec::new();
    for bits in payload(plan.payload_bits()).chunks_exact(40) {
        joined.extend_from_slice(bits);
        let mut crc = vec![Bit::Zero; 16];
        format.crc.write(bits, &mut crc).unwrap();
        joined.extend(crc);
    }
    let last = joined.last_mut().unwrap();
    *last = Bit::from(*last == Bit::Zero);
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    plan.segmentation
        .encode(
            &joined,
            &mut coded,
            &mut vec![Bit::Zero; plan.segmentation.block_bits()],
        )
        .unwrap();
    let mut output = vec![Bit::One; plan.payload_bits()];
    assert_eq!(
        BlockDecoder::new(plan).decode(&soft(&coded), &mut output),
        Ok(BlockOutcome::InvalidCrc { block: 1 })
    );
    assert!(output.iter().all(|bit| *bit == Bit::One));
}

#[test]
fn filler_erasure_absence_and_resource_failures_remain_distinct() {
    let plan = BlockPlan::new(
        BlockFormat {
            block_bits: 505,
            blocks: 1,
            crc: TransportCrc::Crc8,
            rate: CodeRate::Half,
        },
        count(10000),
    )
    .unwrap();
    assert_eq!(plan.segmentation.filler_bits(), 1);
    let width = plan.segmentation.block_bits();
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    for (index, output) in coded.chunks_exact_mut((width + 8) * 2).enumerate() {
        let mut input = vec![Bit::Zero; width];
        if index == 0 {
            input[0] = Bit::One;
        }
        convolutional::encode(CodeRate::Half, &input, output).unwrap();
    }
    let mut decoder = BlockDecoder::new(plan);
    let mut output = vec![Bit::One; 505];
    assert_eq!(
        decoder.decode(&soft(&coded), &mut output),
        Ok(BlockOutcome::InvalidFiller)
    );
    assert_eq!(
        decoder.decode(&vec![0.0; plan.coded_bits()], &mut output),
        Ok(BlockOutcome::Erased)
    );
    assert!(output.iter().all(|bit| *bit == Bit::One));
    let absent = BlockPlan::new(
        BlockFormat {
            block_bits: 40,
            blocks: 0,
            crc: TransportCrc::Crc16,
            rate: CodeRate::Half,
        },
        count(1),
    )
    .unwrap();
    assert_eq!(
        BlockDecoder::new(absent).decode(&[], &mut []),
        Ok(BlockOutcome::Absent)
    );
    assert!(
        BlockPlan::new(
            BlockFormat {
                block_bits: usize::MAX,
                blocks: 512,
                crc: TransportCrc::Crc16,
                rate: CodeRate::Third
            },
            count(1)
        )
        .is_err()
    );
    assert!(
        BlockPlan::new(
            BlockFormat {
                block_bits: 505,
                blocks: 1,
                crc: TransportCrc::Crc8,
                rate: CodeRate::Half,
            },
            count(1)
        )
        .is_err()
    );
    assert_eq!(
        decoder.decode(&vec![f32::NAN; plan.coded_bits()], &mut output),
        Err(DspError::NonFiniteEvidence)
    );
}
