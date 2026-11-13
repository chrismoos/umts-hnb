// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::coding::{DecodeProgress, DecodeWork};
use std::sync::Arc;

#[test]
fn next_format_assembly_cannot_overwrite_pending_decode_or_its_metadata() {
    for cancel in [false, true] {
        let formats = [40, 81].map(|block_bits| BlockFormat {
            block_bits,
            blocks: 1,
            crc: TransportCrc::Crc16,
            rate: CodeRate::Half,
        });
        let plans: Vec<_> = formats
            .iter()
            .map(|format| {
                let width = BlockPlan::new(*format, limits().maximum_coded_bits)
                    .unwrap()
                    .coded_bits();
                Arc::new(
                    UplinkTransportPlan::new(id(1), *format, Rel99Tti::Ms10, &[width], limits())
                        .unwrap(),
                )
            })
            .collect();
        let mut receiver = DedicatedUplink::for_plans(plans).unwrap();
        let payloads = [payload(40), payload(81)];
        for index in 0..2 {
            receiver.select(index).unwrap();
            receiver
                .begin(at(index as u64 * CHIPS_PER_FRAME), index as u8)
                .unwrap();
            let coded = coded_frames(formats[index], Rel99Tti::Ms10, &payloads[index]);
            let input: Vec<_> = coded
                .iter()
                .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
                .collect();
            receiver
                .receive(
                    at(index as u64 * CHIPS_PER_FRAME),
                    MatchedTransport {
                        id: id(1),
                        input_bits: input.len(),
                        output_bits: input.len(),
                    },
                    &input,
                )
                .unwrap();
            if index == 0 {
                assert_eq!(receiver.begin_decode(), Ok(true));
                let mut work = DecodeWork::new(count(257));
                assert_eq!(receiver.decode_work(&mut work), Ok(DecodeProgress::Pending));
                assert!(receiver.finish_decode().is_err());
            }
        }
        assert_eq!(receiver.ready_start(), Some(at(CHIPS_PER_FRAME)));
        assert_eq!(receiver.begin_decode(), Ok(false));
        assert!(receiver.begin(at(2 * CHIPS_PER_FRAME), 2).is_err());
        if cancel {
            assert!(receiver.cancel_decode());
        } else {
            let mut work = DecodeWork::new(NonZeroUsize::MAX);
            assert_eq!(
                receiver.decode_work(&mut work),
                Ok(DecodeProgress::Complete(BlockOutcome::Decoded))
            );
            let old = receiver.completed_decode().unwrap();
            assert_eq!(old.start, at(0));
            assert_eq!(old.selection, 0);
            assert_eq!(old.payload, payloads[0]);
            receiver.finish_decode().unwrap();
        }
        assert_eq!(receiver.begin_decode(), Ok(true));
        let mut work = DecodeWork::new(NonZeroUsize::MAX);
        assert_eq!(
            receiver.decode_work(&mut work),
            Ok(DecodeProgress::Complete(BlockOutcome::Decoded))
        );
        let next = receiver.completed_decode().unwrap();
        assert_eq!(next.start, at(CHIPS_PER_FRAME));
        assert_eq!(next.selection, 1);
        assert_eq!(next.payload, payloads[1]);
        receiver.finish_decode().unwrap();
        assert_eq!(receiver.pending_decode_count(), 0);
    }
}
