// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, plan};
use std::num::NonZeroU64;

#[test]
fn preencoded_cycle_matches_direct_encoder_across_payloads_and_sfn_wrap() {
    for compact in [false, true] {
        let (_, schedule) = plan(compact).into_parts();
        let cycle = BroadcastCycle::new(&schedule, at(0)).unwrap();
        assert_eq!(cycle.blocks.len(), CYCLE_BLOCKS);
        assert_eq!(
            cycle.blocks.len() * std::mem::size_of::<[Bit; TTI_BITS]>(),
            1_105_920,
        );
        let encoder = BroadcastEncoder::new().unwrap();
        for frame in [0, 2, 4, 6, 8, 10, 12, 4094, 4096, 4098] {
            let time = at(frame * CHIPS_PER_FRAME);
            let mut payload = [Bit::Zero; PAYLOAD_BITS];
            unpack_msb(schedule.message_at(time).unwrap(), &mut payload).unwrap();
            let mut expected = [Bit::Zero; TTI_BITS];
            encoder.encode(&payload, &mut expected).unwrap();
            assert_eq!(cycle.frame_at(time).unwrap(), &expected[..FRAME_BITS]);
            assert_eq!(
                cycle.frame_at(at((frame + 1) * CHIPS_PER_FRAME)).unwrap(),
                &expected[FRAME_BITS..],
            );
        }
        assert_eq!(
            cycle.frame_at(at(0)).unwrap(),
            cycle
                .frame_at(at(SYSTEM_FRAME_MODULUS * CHIPS_PER_FRAME))
                .unwrap(),
        );
    }
}

#[test]
fn cycle_preserves_schedule_epoch_and_origin_alignment_guards() {
    let (_, schedule) = plan(false).into_parts();
    let wrong_epoch = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 0,
    };
    assert!(matches!(
        BroadcastCycle::new(&schedule, wrong_epoch),
        Err(CommonTxError::Schedule(ScheduleError::WrongEpoch)),
    ));
    for index in [1, CHIPS_PER_FRAME, CHIPS_PER_FRAME + 1] {
        assert!(matches!(
            BroadcastCycle::new(&schedule, at(index)),
            Err(CommonTxError::Schedule(ScheduleError::UnalignedTime)),
        ));
    }
    let origin = at((SYSTEM_FRAME_MODULUS + BCH_FRAMES) * CHIPS_PER_FRAME);
    let cycle = BroadcastCycle::new(&schedule, origin).unwrap();
    assert_eq!(
        cycle.frame_at(origin).unwrap(),
        cycle.frame_at(at(2 * CHIPS_PER_FRAME)).unwrap()
    );
    assert!(matches!(
        cycle.frame_at(wrong_epoch),
        Err(CommonTxError::Schedule(ScheduleError::WrongEpoch)),
    ));
    assert!(matches!(
        cycle.frame_at(at(1)),
        Err(CommonTxError::Schedule(ScheduleError::UnalignedTime)),
    ));
    assert!(cycle.frame_at(at(CHIPS_PER_FRAME)).is_ok());
}
