// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    coding::{ChannelCoding, CodedBlockEncoder, CodedBlockFormat},
    convolutional::CodeRate,
    crc::TransportCrc,
};
use std::num::{NonZeroU8, NonZeroUsize};

mod incremental;

#[test]
fn private_workspace_rebinds_absent_small_and_segmented_formats_in_both_directions() {
    for coding in [
        ChannelCoding::Convolutional(CodeRate::Half),
        ChannelCoding::Convolutional(CodeRate::Third),
        ChannelCoding::Turbo,
    ] {
        let formats: Vec<_> = [0, 40, 481, 5098, 6000, 488]
            .into_iter()
            .map(|bits| {
                CodedBlockPlan::new(
                    CodedBlockFormat {
                        block_bits: bits.max(1),
                        blocks: usize::from(bits != 0),
                        crc: TransportCrc::Crc16,
                        coding,
                    },
                    NonZeroUsize::new(100000).unwrap(),
                )
                .unwrap()
            })
            .collect();
        let plan = Arc::new(DecoderBankPlan::new(formats.clone()).unwrap());
        let budget = DecodeBudget::new(NonZeroU8::new(6).unwrap()).unwrap();
        let mut first = DecoderBank::new(Arc::clone(&plan), budget).unwrap();
        let mut second = DecoderBank::new(plan, budget).unwrap();
        for index in [4, 0, 2, 1, 3, 5, 4, 3, 0, 1, 2] {
            let plan = formats[index];
            for (worker, decoder) in [&mut first, &mut second].into_iter().enumerate() {
                decoder.select(index).unwrap();
                assert!(decoder.select(usize::MAX).is_err());
                let input: Vec<_> = (0..plan.payload_bits())
                    .map(|bit| Bit::from((bit + worker) % 7 < 3))
                    .collect();
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
                    decoder.decode(&soft, &mut output),
                    Ok(if index == 0 {
                        BlockOutcome::Absent
                    } else {
                        BlockOutcome::Decoded
                    })
                );
                assert_eq!(output, input);
            }
        }
    }
}
