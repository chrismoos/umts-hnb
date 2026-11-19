// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{DedicatedTransmitRuntime, TransmitRuntimeLimits},
    test_support::{at, count, source},
};
use num_complex::Complex32;
use umts_types::time::CHIPS_PER_SLOT;

#[test]
fn grant_owner_waits_for_actual_tx_and_retries_loss_without_duplicate_commands() {
    for (lost, quiesce) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut source = source(true);
        let mut writer = source.submission_writer().unwrap();
        let (_resources, bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
        let (mut runtime, mut ports) = DedicatedTransmitRuntime::new(
            &source,
            count(2),
            count(8),
            count(1_000_000),
            TransmitRuntimeLimits {
                command_capacity: count(4),
                commands_per_step: count(2),
                feedback_capacity: count(4),
                feedback_per_step: count(2),
                maximum_inflight_retained_bits: count(8_000_000),
                maximum_frame_payload_bits: count(100_000),
            },
        )
        .unwrap();
        assert!(runtime.attach_hsupa(bank).is_ok());
        let mut grants = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 10).unwrap();
        assert_eq!(
            grants.service(at(0), at(0), &mut ports.control),
            Ok(GrantProgress::Queued)
        );
        let Some(Pending::Command { end, .. }) = grants.pending else {
            panic!("command");
        };
        assert_eq!(
            end.index,
            GRANT_FRAME_OFFSET_CHIPS + Tti::TwoMilliseconds.chips()
        );
        if quiesce {
            assert_eq!(
                grants.quiesce_before(ChipInstant {
                    index: end.index - 1,
                    ..end
                }),
                Err(Error::Time)
            );
            assert!(grants.quiescing.is_none());
            grants.quiesce_before(end).unwrap();
            grants.quiesce_before(end).unwrap();
            assert_eq!(grants.request(11), Err(Error::Unavailable));
        }
        assert_eq!(
            grants.service(at(0), at(0), &mut ports.control),
            Ok(GrantProgress::Pending)
        );
        assert_eq!(ports.control.outstanding(), 1);
        runtime.service().unwrap();
        assert!(
            grants
                .route_command(ports.control.try_event().unwrap())
                .is_none()
        );
        assert_eq!(grants.counters().admitted, 1);
        assert_eq!(grants.counters().submitted, 0);
        let Some(Pending::Air { id, end, .. }) = grants.pending else {
            panic!("air receipt");
        };
        assert!(
            grants
                .accept_completion(Completion {
                    id,
                    identity: identities[1],
                    feedback_tti: None,
                    end,
                    submitted: true,
                })
                .is_err()
        );
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        for _ in 0..6 {
            let window = if lost {
                runtime
                    .generate_lost_slot(&mut source, &mut samples)
                    .unwrap()
            } else {
                runtime.generate_slot(&mut source, &mut samples).unwrap()
            };
            assert!(if lost {
                writer.lost(window)
            } else {
                writer.accepted(window)
            });
        }
        assert!(ports.control.queue_hsupa(Command::TakeCompletion).is_ok());
        runtime.service().unwrap();
        let TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(Ok(Some(completion)))) =
            ports.control.try_event().unwrap().outcome
        else {
            panic!("resolved completion");
        };
        assert_eq!(completion.submitted, !lost);
        grants.accept_completion(completion).unwrap();
        assert_eq!(grants.last_submitted_end, (!lost).then_some(end));
        assert!(grants.accept_completion(completion).is_err());
        assert_eq!(grants.counters().submitted, u64::from(!lost));
        assert_eq!(grants.grant_transmissions, u8::from(!lost));
        assert_eq!(grants.counters().lost, u64::from(lost));
        if quiesce {
            assert_eq!(
                grants.service(end, end, &mut ports.control),
                Ok(GrantProgress::Quiesced)
            );
            assert!(!grants.pending());
            assert_eq!(grants.counters().queued, 1);
            continue;
        }
        let next = at(6 * CHIPS_PER_SLOT);
        assert_eq!(
            grants.service(next, next, &mut ports.control),
            Ok(if lost {
                GrantProgress::Queued
            } else {
                GrantProgress::Submitted
            })
        );
        if !lost {
            grants.request(11).unwrap();
            assert_eq!(
                grants.service(next, next, &mut ports.control),
                Ok(GrantProgress::Queued)
            );
        }
        let Some(Pending::Command { grant, end, .. }) = grants.pending else {
            panic!("next grant");
        };
        assert_eq!(grant.value(), if lost { 10 } else { 11 });
        assert_eq!(
            (end.index - GRANT_FRAME_OFFSET_CHIPS) % Tti::TwoMilliseconds.chips(),
            0
        );
    }
}

#[test]
fn changed_grant_repetition_has_a_receipt_bound_and_a_finite_budget() {
    let source = source(true);
    let (_resources, _bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let mut owner = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 29).unwrap();
    assert!(!owner.repeat_due(at(GRANT_REPEAT_INTERVAL_CHIPS)));
    owner.submitted = Some(owner.desired);
    owner.last_submitted_end = Some(at(1));
    owner.grant_transmissions = 1;
    assert!(!owner.repeat_due(at(0)));
    assert!(!owner.repeat_due(at(GRANT_REPEAT_INTERVAL_CHIPS)));
    assert!(owner.repeat_due(at(GRANT_REPEAT_INTERVAL_CHIPS + 1)));
    for transmitted in 1..=GRANT_TRANSMISSIONS {
        owner.grant_transmissions = transmitted;
        assert_eq!(
            owner.repeat_due(at(GRANT_REPEAT_INTERVAL_CHIPS + 1)),
            transmitted < GRANT_TRANSMISSIONS,
        );
    }
}

#[test]
fn periodic_refresh_requires_completed_airtime_and_fresh_nonempty_demand() {
    use crate::hsupa_rx::SchedulingReport;
    use umts_mac::hsupa::scheduling::SchedulingInformation;
    use umts_types::time::CHIPS_PER_SECOND;

    let source = source(true);
    let (_resources, _bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let period = std::num::NonZeroU64::new(CHIPS_PER_SECOND).unwrap();
    let mut owner = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 29)
        .unwrap()
        .with_refresh_period(Some(period))
        .unwrap();
    let mut peer = Grants::new(identities[1], Tti::TwoMilliseconds, at(0), 0.1, 29)
        .unwrap()
        .with_refresh_period(Some(period))
        .unwrap();
    let now = at(CHIPS_PER_SECOND);
    let demand = SchedulingReport {
        start: at(CHIPS_PER_SECOND - 1),
        information: SchedulingInformation {
            power_headroom_index: 31,
            total_buffer_index: 31,
            priority_buffer_index: 15,
            highest_priority_logical_channel: 4,
        },
    };
    owner.observe_uplink(
        now,
        crate::hsupa_rx::GrantActivity {
            data: None,
            scheduling: Some(demand),
            happy: None,
            unhappy_streak: 0,
            ..Default::default()
        },
    );
    peer.observe_uplink(
        now,
        crate::hsupa_rx::GrantActivity {
            data: None,
            scheduling: Some(demand),
            happy: None,
            unhappy_streak: 0,
            ..Default::default()
        },
    );
    assert!(!owner.periodic_refresh_due(now));
    owner.last_submitted_end = Some(at(0));
    assert!(!owner.periodic_refresh_due(at(CHIPS_PER_SECOND - 1)));
    assert!(owner.periodic_refresh_due(now));
    assert!(!peer.periodic_refresh_due(now));
    assert!(!owner.periodic_refresh_due(at(2 * CHIPS_PER_SECOND)));
    owner.observe_uplink(
        now,
        crate::hsupa_rx::GrantActivity {
            data: None,
            scheduling: Some(SchedulingReport {
                information: SchedulingInformation {
                    total_buffer_index: 0,
                    ..demand.information
                },
                ..demand
            }),
            happy: None,
            unhappy_streak: 0,
            ..Default::default()
        },
    );
    assert!(!owner.periodic_refresh_due(now));
    owner.refresh_period = None;
    owner.observe_uplink(
        now,
        crate::hsupa_rx::GrantActivity {
            data: None,
            scheduling: Some(demand),
            happy: None,
            unhappy_streak: 0,
            ..Default::default()
        },
    );
    assert!(!owner.periodic_refresh_due(now));
    assert!(
        Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 29)
            .unwrap()
            .with_refresh_period(Some(std::num::NonZeroU64::MIN))
            .is_err()
    );
}

#[test]
fn grant_adapts_between_floor_and_ceiling_from_happy_bit_and_idle_time() {
    use crate::hsupa_rx::{GrantActivity, SchedulingReport};
    use umts_mac::hsupa::scheduling::SchedulingInformation;
    let source = source(true);
    let (_resources, _bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let mut grants = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 10)
        .unwrap()
        .with_bounds(10, 12)
        .unwrap();
    let unhappy = |streak: u32, now: u64| GrantActivity {
        data: Some(at(now)),
        scheduling: Some(SchedulingReport {
            start: at(now),
            information: SchedulingInformation::from_word(0x0002_27a3).unwrap(),
        }),
        happy: Some((at(now), false)),
        unhappy_streak: streak,
        ..Default::default()
    };
    // Seven unhappy TTIs change nothing, the eighth raises one index, and the streak must
    // grow by another eight before the next step.
    grants.observe_uplink(at(70), unhappy(7, 70));
    assert_eq!(grants.desired.value(), 10);
    grants.observe_uplink(at(80), unhappy(8, 80));
    assert_eq!(grants.desired.value(), 11);
    grants.observe_uplink(at(90), unhappy(9, 90));
    assert_eq!(grants.desired.value(), 11);
    grants.observe_uplink(at(160), unhappy(16, 160));
    assert_eq!(grants.desired.value(), 12);
    grants.observe_uplink(at(240), unhappy(24, 240));
    assert_eq!(grants.desired.value(), 12);
    assert_eq!(grants.counters().raised, 2);
    // An empty buffer never raises the grant.
    let mut empty = unhappy(40, 400);
    empty.scheduling = Some(SchedulingReport {
        start: at(400),
        information: SchedulingInformation::from_word(0).unwrap(),
    });
    grants.observe_uplink(at(400), empty);
    assert_eq!(grants.desired.value(), 12);
    // Idle time lowers one index per 200 ms down to the floor and no further.
    let idle = |_now: u64| GrantActivity {
        data: Some(at(400)),
        scheduling: None,
        happy: Some((at(400), true)),
        unhappy_streak: 0,
        ..Default::default()
    };
    let step = IDLE_CHIPS;
    grants.observe_uplink(at(400 + step), idle(400 + step));
    assert_eq!(grants.desired.value(), 11);
    grants.observe_uplink(at(400 + step + 1), idle(400 + step + 1));
    assert_eq!(grants.desired.value(), 11);
    grants.observe_uplink(at(400 + 2 * step + 1), idle(400 + 2 * step + 1));
    assert_eq!(grants.desired.value(), 10);
    grants.observe_uplink(at(400 + 3 * step + 2), idle(400 + 3 * step + 2));
    assert_eq!(grants.desired.value(), 10);
    assert_eq!(grants.counters().lowered, 2);
    // Equal bounds keep the grant fixed.
    let mut fixed = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 10).unwrap();
    fixed.observe_uplink(at(80), unhappy(8, 80));
    assert_eq!(fixed.desired.value(), 10);
}

fn adaptive_owner() -> Grants {
    let source = source(true);
    let (_, _, _, identities) = crate::hsupa_tx::tests::setup(&source);
    Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 10)
        .unwrap()
        .with_bounds(10, 12)
        .unwrap()
}

fn demand(tick: u64, streak: u32) -> crate::hsupa_rx::GrantActivity {
    crate::hsupa_rx::GrantActivity {
        data: Some(at(tick)),
        scheduling: None,
        happy: Some((at(tick), false)),
        unhappy_streak: streak,
        ..Default::default()
    }
}

#[test]
fn stale_unhappy_snapshot_cannot_undo_an_idle_reduction() {
    let mut owner = adaptive_owner();
    let tti = owner.interval.chips();
    owner.observe_uplink(at(8 * tti), demand(8 * tti, 8));
    owner.observe_uplink(at(16 * tti), demand(16 * tti, 16));
    let old = demand(24 * tti, 24);
    owner.observe_uplink(at(24 * tti), old);
    assert_eq!(owner.desired.value(), 12);
    let idle = 24 * tti + IDLE_CHIPS;
    owner.observe_uplink(at(idle), old);
    assert_eq!(owner.desired.value(), 11);
    owner.observe_uplink(at(idle + 1), old);
    assert_eq!(owner.desired.value(), 11);
    owner.observe_uplink(at(idle + IDLE_CHIPS), old);
    assert_eq!(owner.desired.value(), 10);
    owner.observe_uplink(
        at(idle + IDLE_CHIPS + tti),
        demand(idle + IDLE_CHIPS + tti, 1),
    );
    assert_eq!(owner.desired.value(), 10);
    owner.observe_uplink(
        at(idle + IDLE_CHIPS + 8 * tti),
        demand(idle + IDLE_CHIPS + 8 * tti, 8),
    );
    assert_eq!(owner.desired.value(), 11);
}

#[test]
fn stale_scheduling_neither_pins_grant_nor_overrides_newer_unhappy() {
    use crate::hsupa_rx::SchedulingReport;
    use umts_mac::hsupa::scheduling::SchedulingInformation;
    let mut owner = adaptive_owner();
    let mut activity = demand(1, 8);
    activity.scheduling = Some(SchedulingReport {
        start: at(1),
        information: SchedulingInformation::from_word(0x0002_27a3).unwrap(),
    });
    owner.observe_uplink(at(1), activity);
    assert_eq!(owner.desired.value(), 11);
    owner.observe_uplink(at(1 + IDLE_CHIPS), activity);
    assert_eq!(owner.desired.value(), 10);
    let now = 1 + IDLE_CHIPS + owner.interval.chips();
    let mut newer = demand(now, 16);
    newer.scheduling = Some(SchedulingReport {
        start: at(1),
        information: SchedulingInformation::from_word(0).unwrap(),
    });
    owner.observe_uplink(at(now), newer);
    assert_eq!(owner.desired.value(), 11);
}

#[test]
fn grant_increase_requires_new_fresh_feedback_and_consistent_demand() {
    use crate::hsupa_rx::SchedulingReport;
    use umts_mac::hsupa::scheduling::SchedulingInformation;
    let mut owner = adaptive_owner();
    let stale = demand(1, 8);
    owner.observe_uplink(
        at(1 + owner.interval.chips() * FEEDBACK_MAX_AGE_TTIS + 1),
        stale,
    );
    assert_eq!(owner.desired.value(), 10);
    let tick = IDLE_CHIPS;
    let mut empty = demand(tick, 16);
    empty.scheduling = Some(SchedulingReport {
        start: at(tick),
        information: SchedulingInformation::from_word(0).unwrap(),
    });
    owner.observe_uplink(at(tick), empty);
    assert_eq!(owner.desired.value(), 10);
    let next = tick + owner.interval.chips();
    empty.happy = Some((at(next), false));
    owner.observe_uplink(at(next), empty);
    assert_eq!(owner.desired.value(), 11);
    owner.observe_uplink(at(next + 1), empty);
    assert_eq!(owner.desired.value(), 11);
}

#[test]
fn first_transmission_failures_lower_the_grant_and_successes_restore_it() {
    use crate::hsupa_rx::GrantActivity;
    let source = source(true);
    let (_resources, _bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let mut grants = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 31).unwrap();
    let quality = |decoded: u64, failed: u64| GrantActivity {
        first_decoded: decoded,
        first_failed: failed,
        ..Default::default()
    };
    grants.observe_uplink(at(10), quality(0, 0));
    grants.observe_uplink(at(20), quality(0, 19));
    assert_eq!(grants.desired.value(), 31);
    grants.observe_uplink(at(30), quality(0, 20));
    assert_eq!(grants.desired.value(), 30);
    assert_eq!(grants.counters().lowered, 1);
    grants.observe_uplink(at(40), quality(0, 40));
    grants.observe_uplink(at(50), quality(0, 60));
    assert_eq!(grants.desired.value(), 28);
    grants.observe_uplink(at(60), quality(190, 70));
    assert_eq!(grants.desired.value(), 28);
    let second = umts_types::time::CHIPS_PER_SECOND;
    grants.observe_uplink(at(50 + second - 1), quality(190 + 5000, 70));
    assert_eq!(grants.desired.value(), 28);
    grants.observe_uplink(at(50 + second), quality(190 + 5380, 70));
    assert_eq!(grants.desired.value(), 29);
    grants.observe_uplink(at(60 + second), quality(190 + 5380 + 1900, 70 + 100));
    assert_eq!(grants.desired.value(), 29);
    grants.observe_uplink(at(70 + second), quality(190 + 5380 + 2280, 70 + 100));
    assert_eq!(grants.desired.value(), 30);
    grants.observe_uplink(at(80 + second), quality(190 + 5380 + 2660, 70 + 100));
    assert_eq!(grants.desired.value(), 31);
    assert_eq!(grants.counters().raised, 3);
    grants.observe_uplink(at(110), quality(0, 0));
    grants.observe_uplink(at(120), quality(0, 0));
    assert_eq!(grants.desired.value(), 31);
}

#[test]
fn handsets_wanting_uplink_share_processes_and_keep_their_own_grant() {
    use crate::hsupa_rx::{GrantActivity, SchedulingReport};
    use umts_mac::hsupa::scheduling::SchedulingInformation;
    let source = source(true);
    let (_resources, _bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let mut grants = Grants::new(identities[0], Tti::TwoMilliseconds, at(0), 0.1, 31).unwrap();
    let tti = grants.interval.chips();
    let reported = |tick: u64, buffer: u8, bits: u64| GrantActivity {
        scheduling: Some(SchedulingReport {
            start: at(tick),
            information: SchedulingInformation {
                power_headroom_index: 20,
                total_buffer_index: buffer,
                priority_buffer_index: 0,
                highest_priority_logical_channel: 4,
            },
        }),
        payload_bits: bits,
        ..Default::default()
    };
    for step in 0..=1000u64 {
        grants.observe_uplink(at(step * tti), reported(step * tti, 15, step * 36));
        assert!(!grants.demand(at(step * tti)));
    }
    assert_eq!(grants.used_bits_per_interval(), Some(36.0));
    let start = 10_000 * tti;
    for step in 0..500u64 {
        grants.observe_uplink(at(start + step * tti), reported(start + step * tti, 24, 0));
        assert!(!grants.demand(at(start + step * tti)));
    }
    let end = start + 500 * tti;
    grants.observe_uplink(at(end), reported(end, 24, 0));
    assert!(grants.demand(at(end)));
    grants.observe_uplink(at(end + 200 * tti), reported(end, 24, 0));
    assert!(!grants.backlog());
    assert!(grants.demand(at(end + 1000 * tti)));
    assert!(!grants.demand(at(end + 1001 * tti)));
    let unhappy = end + 5000 * tti;
    grants.observe_uplink(
        at(unhappy),
        GrantActivity {
            happy: Some((at(unhappy), false)),
            unhappy_streak: 8,
            ..Default::default()
        },
    );
    assert!(!grants.demand(at(unhappy)));
    let unhappy = unhappy + 500 * tti;
    grants.observe_uplink(
        at(unhappy),
        GrantActivity {
            happy: Some((at(unhappy), false)),
            unhappy_streak: 8,
            ..Default::default()
        },
    );
    assert!(grants.demand(at(unhappy)));
    assert_eq!(grants.desired.value(), 31);
    assert!(grants.allocate(at(unhappy), 0x55));
    assert_eq!((grants.desired.value(), grants.allocation), (31, 0x55));
    assert!(!grants.allocate(at(unhappy + 100 * tti), 0xaa));
    let quality = |decoded: u64, failed: u64| GrantActivity {
        first_decoded: decoded,
        first_failed: failed,
        ..Default::default()
    };
    let later = unhappy + 2000 * tti;
    grants.observe_uplink(at(later), quality(0, 0));
    assert!(grants.allocate(at(later), 0xaa));
    grants.observe_uplink(at(later + 20 * tti), quality(0, 400));
    assert_eq!(grants.desired.value(), 31);
    grants.observe_uplink(at(later + 400 * tti), quality(0, 420));
    assert_eq!(grants.desired.value(), 30);
}
