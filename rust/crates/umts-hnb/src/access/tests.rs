// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{access_settings, at, count};

fn observation(start: u64, signature: u8) -> AccessObservation {
    AccessObservation {
        code: PrachCode::new(7504).unwrap(),
        signature: PreambleSignature::new(signature).unwrap(),
        nominal_start: at(start),
        received_start: at(start + 54),
    }
}

#[test]
fn automatic_occasion_selection_matches_all_subchannels_and_both_timings_across_sfn_wrap() {
    let wrap = umts_types::time::SYSTEM_FRAME_MODULUS * umts_types::time::CHIPS_PER_FRAME
        / ACCESS_SLOT_CHIPS as u64;
    for timing in [AichTiming::ThreeSlots, AichTiming::FiveSlots] {
        for subchannel in 0..12 {
            let calendar = AccessCalendar::new(
                at(0).epoch,
                PrachCode::new(7504).unwrap(),
                &access_settings(timing, &[3, 13], &[subchannel]),
                count(1),
                100,
            )
            .unwrap();
            for cycle in [0, 1, 3, wrap - 1, wrap, wrap + 1] {
                let first = at(cycle * ACCESS_SLOT_CHIPS as u64 + 17);
                let selected = calendar.profile().next_preamble(first).unwrap();
                assert!(selected.index >= first.index);
                let indicator = selected.index + u64::from(timing.preamble_to_indicator_chips());
                assert!(indicator.is_multiple_of(ACCESS_SLOT_CHIPS as u64));
                assert_eq!(
                    (indicator / ACCESS_SLOT_CHIPS as u64) % 12,
                    u64::from(subchannel)
                );
                assert!(selected.index - first.index < 12 * ACCESS_SLOT_CHIPS as u64);
                assert_eq!(calendar.profile().next_preamble(selected), Ok(selected));
                assert_eq!(
                    calendar
                        .profile()
                        .next_preamble(at(selected.index + 1))
                        .unwrap()
                        .index,
                    selected.index + 12 * ACCESS_SLOT_CHIPS as u64
                );
            }
            assert_eq!(
                calendar.profile().next_preamble(at(u64::MAX)),
                Err(AccessScheduleError::Occasion)
            );
        }
    }
}

#[test]
fn cancelling_one_signature_preserves_its_peer_and_reclaims_empty_access_slots() {
    let mut calendar = AccessCalendar::new(
        at(0).epoch,
        PrachCode::new(7504).unwrap(),
        &access_settings(AichTiming::FiveSlots, &[3, 13], &[7]),
        count(1),
        100,
    )
    .unwrap();
    for signature in [3, 13] {
        calendar
            .schedule(
                observation(23040, signature),
                Acquisition::Positive,
                at(28000),
                at(0),
                0,
            )
            .unwrap();
    }
    assert_eq!(calendar.pending(), 1);
    assert!(
        calendar
            .cancel(at(35840), PreambleSignature::new(3).unwrap())
            .unwrap()
    );
    let mut expected = AcquisitionIndicators::new((1 << 3) | (1 << 13)).unwrap();
    expected.set(13, Acquisition::Positive).unwrap();
    assert_eq!(
        calendar.take(at(35840)).unwrap().unwrap().signals(),
        expected.signals()
    );
    assert_eq!(
        calendar.cancel(at(35840), PreambleSignature::new(13).unwrap()),
        Err(AccessScheduleError::Late)
    );
    let next = 23040 + 61440;
    calendar
        .schedule(
            observation(next, 3),
            Acquisition::Positive,
            at(next + 4200),
            at(40960),
            0,
        )
        .unwrap();
    assert!(
        calendar
            .cancel(at(next + 12800), PreambleSignature::new(3).unwrap())
            .unwrap()
    );
    assert_eq!(calendar.pending(), 0);
    assert!(
        !calendar
            .cancel(at(next + 12800), PreambleSignature::new(3).unwrap())
            .unwrap()
    );
}

#[test]
fn nominal_grid_not_arrival_peak_selects_response_and_message_times() {
    for (timing, offset, message) in [
        (AichTiming::ThreeSlots, 7680, 15360),
        (AichTiming::FiveSlots, 12800, 20480),
    ] {
        let settings = access_settings(timing, &[3, 13], &[7]);
        let nominal = 35840 - offset;
        let mut calendar = AccessCalendar::new(
            at(0).epoch,
            PrachCode::new(7504).unwrap(),
            &settings,
            count(2),
            100,
        )
        .unwrap();
        let result = calendar
            .schedule(
                observation(nominal, 13),
                Acquisition::Positive,
                at(nominal + 4150),
                at(0),
                0,
            )
            .unwrap();
        assert_eq!(result.indicator_start, at(35840));
        assert_eq!(result.message_start, at(nominal + message));
        assert!(!result.duplicate);
        assert!(
            calendar
                .schedule(
                    observation(nominal, 13),
                    Acquisition::Positive,
                    at(nominal + 4150),
                    at(0),
                    0
                )
                .unwrap()
                .duplicate
        );
        calendar
            .schedule(
                observation(nominal, 3),
                Acquisition::Negative,
                at(nominal + 4150),
                at(0),
                0,
            )
            .unwrap();
        assert_eq!(
            calendar.schedule(
                observation(nominal, 13),
                Acquisition::Negative,
                at(nominal + 4150),
                at(0),
                0
            ),
            Err(AccessScheduleError::Conflict)
        );
        let values = calendar.take(at(35840)).unwrap().unwrap().signals();
        for (symbol, pair) in values.chunks_exact(2).enumerate() {
            let expected = (if (symbol & 13).count_ones().is_multiple_of(2) {
                1
            } else {
                -1
            }) - (if (symbol & 3).count_ones().is_multiple_of(2) {
                1
            } else {
                -1
            });
            assert_eq!(pair, &[expected, expected]);
        }
        assert_eq!(
            calendar.schedule(
                observation(nominal, 13),
                Acquisition::Positive,
                at(nominal + 4150),
                at(0),
                0
            ),
            Err(AccessScheduleError::Late)
        );
    }
}

#[test]
fn table_seven_subchannel_cycle_is_checked_for_both_signalled_timings() {
    for timing in [AichTiming::ThreeSlots, AichTiming::FiveSlots] {
        let settings = access_settings(timing, &[13], &[7]);
        for dl_access_slot in 12..72 {
            let target = dl_access_slot * ACCESS_SLOT_CHIPS as u64;
            let nominal = target - u64::from(timing.preamble_to_indicator_chips());
            let mut calendar = AccessCalendar::new(
                at(0).epoch,
                PrachCode::new(7504).unwrap(),
                &settings,
                count(1),
                100,
            )
            .unwrap();
            let result = calendar.schedule(
                observation(nominal, 13),
                Acquisition::Positive,
                at(nominal + 4150),
                at(0),
                0,
            );
            if dl_access_slot % 12 == 7 {
                assert_eq!(result.unwrap().indicator_start, at(target));
            } else {
                assert_eq!(result, Err(AccessScheduleError::Occasion));
            }
        }
    }
}

#[test]
fn invalid_observations_deadlines_and_epochs_do_not_publish_indicators() {
    let settings = access_settings(AichTiming::FiveSlots, &[13], &[7]);
    let mut calendar = AccessCalendar::new(
        at(0).epoch,
        PrachCode::new(7504).unwrap(),
        &settings,
        count(1),
        100,
    )
    .unwrap();
    let valid = observation(23040, 13);
    assert_eq!(
        calendar.schedule(valid, Acquisition::Positive, at(27189), at(0), 0),
        Err(AccessScheduleError::IncompletePreamble)
    );
    assert_eq!(
        calendar.schedule(valid, Acquisition::Positive, at(27190), at(35841), 0),
        Err(AccessScheduleError::Late)
    );
    assert_eq!(
        calendar.schedule(valid, Acquisition::Positive, at(27190), at(0), 8651),
        Err(AccessScheduleError::Late)
    );
    assert_eq!(
        calendar.schedule(valid, Acquisition::None, at(27190), at(0), 0),
        Err(AccessScheduleError::NoIndicator)
    );
    let mut bad = valid;
    bad.received_start.index = 23039;
    assert_eq!(
        calendar.schedule(bad, Acquisition::Positive, at(27190), at(0), 0),
        Err(AccessScheduleError::Arrival)
    );
    bad.received_start.index = 23141;
    assert_eq!(
        calendar.schedule(bad, Acquisition::Positive, at(27190), at(0), 0),
        Err(AccessScheduleError::Arrival)
    );
    bad = valid;
    bad.code = PrachCode::new(1).unwrap();
    assert_eq!(
        calendar.schedule(bad, Acquisition::Positive, at(27190), at(0), 0),
        Err(AccessScheduleError::Code)
    );
    bad = valid;
    bad.signature = PreambleSignature::new(3).unwrap();
    assert_eq!(
        calendar.schedule(bad, Acquisition::Positive, at(27190), at(0), 0),
        Err(AccessScheduleError::Signature)
    );
    bad = valid;
    bad.nominal_start.epoch = umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
    assert_eq!(
        calendar.schedule(bad, Acquisition::Positive, at(27190), at(0), 0),
        Err(AccessScheduleError::Epoch)
    );
    assert!(calendar.take(at(35840)).unwrap().is_none());
}

#[test]
fn pending_capacity_expiry_and_monotonic_consumption_are_bounded() {
    let subchannels: Vec<_> = (0..12).collect();
    let settings = access_settings(AichTiming::FiveSlots, &[13], &subchannels);
    let mut calendar = AccessCalendar::new(
        at(0).epoch,
        PrachCode::new(7504).unwrap(),
        &settings,
        count(1),
        100,
    )
    .unwrap();
    calendar
        .schedule(
            observation(23040, 13),
            Acquisition::Positive,
            at(27190),
            at(0),
            0,
        )
        .unwrap();
    assert_eq!(
        calendar.schedule(
            observation(28160, 13),
            Acquisition::Positive,
            at(32310),
            at(0),
            0
        ),
        Err(AccessScheduleError::Capacity)
    );
    assert!(calendar.take(at(40960)).unwrap().is_none());
    assert_eq!(calendar.expired(), 1);
    assert_eq!(
        calendar.take(at(35840)).err(),
        Some(AccessScheduleError::Late)
    );
    assert!(
        AccessCalendar::new(
            at(0).epoch,
            PrachCode::new(7504).unwrap(),
            &settings,
            count(MAX_ACCESS_RESPONSES + 1),
            100
        )
        .is_err()
    );
    assert!(
        AccessCalendar::new(
            at(0).epoch,
            PrachCode::new(7504).unwrap(),
            &settings,
            count(1),
            ACCESS_SLOT_CHIPS as u64
        )
        .is_err()
    );
}
