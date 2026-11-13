// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::coding::{DecodeProgress, DecodeWork};

#[test]
fn segmented_work_preserves_atomic_payload_and_crc_at_every_budget_and_gain() {
    for coding in [
        ChannelCoding::Convolutional(CodeRate::Half),
        ChannelCoding::Convolutional(CodeRate::Third),
        ChannelCoding::Turbo,
    ] {
        for blocks in [0, 1, 19] {
            let plan = CodedBlockPlan::new(
                CodedBlockFormat {
                    block_bits: 320,
                    blocks,
                    crc: TransportCrc::Crc16,
                    coding,
                },
                NonZeroUsize::new(100_000).unwrap(),
            )
            .unwrap();
            let bank = Arc::new(DecoderBankPlan::new(vec![plan]).unwrap());
            for gain in [0.001, 1.0, 1000.0] {
                for limit in [257, 80 * 1024] {
                    let input: Vec<_> = (0..plan.payload_bits())
                        .map(|index| Bit::from(index % 7 < 3))
                        .collect();
                    let mut coded = vec![Bit::Zero; plan.coded_bits()];
                    CodedBlockEncoder::new(plan)
                        .unwrap()
                        .encode(&input, &mut coded)
                        .unwrap();
                    let soft: Vec<_> = coded
                        .iter()
                        .map(|bit| if *bit == Bit::Zero { gain } else { -gain })
                        .collect();
                    let mut decoder = DecoderBank::new(
                        Arc::clone(&bank),
                        DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
                    )
                    .unwrap();
                    let mut output = vec![Bit::One; input.len()];
                    let mut progress = decoder.begin(&soft, &mut output).unwrap();
                    let mut visits = 0;
                    while progress == DecodeProgress::Pending {
                        assert!(decoder.select(0).is_err());
                        assert!(output.iter().all(|bit| *bit == Bit::One));
                        let mut work = DecodeWork::new(NonZeroUsize::new(limit).unwrap());
                        progress = decoder.step(&soft, &mut output, &mut work).unwrap();
                        assert!(work.remaining() <= limit);
                        visits += 1;
                        assert!(visits < 100_000);
                    }
                    assert_eq!(
                        progress,
                        DecodeProgress::Complete(if blocks == 0 {
                            BlockOutcome::Absent
                        } else {
                            BlockOutcome::Decoded
                        })
                    );
                    assert_eq!(output, input);
                    if blocks == 19 {
                        assert!(visits > 1);
                    }
                    decoder.select(0).unwrap();
                }
            }
        }
    }
}

#[test]
fn cancelling_partial_codeword_allows_a_different_format_without_old_results() {
    for coding in [
        ChannelCoding::Convolutional(CodeRate::Third),
        ChannelCoding::Turbo,
    ] {
        let formats: Vec<_> = [6000, 40]
            .into_iter()
            .map(|bits| {
                CodedBlockPlan::new(
                    CodedBlockFormat {
                        block_bits: bits,
                        blocks: 1,
                        crc: TransportCrc::Crc16,
                        coding,
                    },
                    NonZeroUsize::new(100_000).unwrap(),
                )
                .unwrap()
            })
            .collect();
        let mut decoder = DecoderBank::new(
            Arc::new(DecoderBankPlan::new(formats.clone()).unwrap()),
            DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
        )
        .unwrap();
        for (index, plan) in formats.into_iter().enumerate() {
            decoder.select(index).unwrap();
            let input = vec![Bit::from(index != 0); plan.payload_bits()];
            let mut coded = vec![Bit::Zero; plan.coded_bits()];
            CodedBlockEncoder::new(plan)
                .unwrap()
                .encode(&input, &mut coded)
                .unwrap();
            let soft: Vec<_> = coded
                .iter()
                .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
                .collect();
            let mut output = vec![Bit::Zero; input.len()];
            assert_eq!(
                decoder.begin(&soft, &mut output),
                Ok(DecodeProgress::Pending)
            );
            if index == 0 {
                let mut work = DecodeWork::new(NonZeroUsize::new(257).unwrap());
                assert_eq!(
                    decoder.step(&soft, &mut output, &mut work),
                    Ok(DecodeProgress::Pending)
                );
                decoder.cancel();
            } else {
                let mut work = DecodeWork::new(NonZeroUsize::MAX);
                assert_eq!(
                    decoder.step(&soft, &mut output, &mut work),
                    Ok(DecodeProgress::Complete(BlockOutcome::Decoded))
                );
                assert_eq!(output, input);
            }
        }
    }
}
