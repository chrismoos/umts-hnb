// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn measured(lane: &mut TpcLane, observed: u64, until: u64, sir: f64) {
    let estimate = TpcEstimate::new(
        lane.identity.unwrap(),
        at(lane.anchor.index + until * CHIPS_PER_SLOT),
        at(lane.anchor.index + observed * CHIPS_PER_SLOT),
        sir,
        10.0,
    )
    .unwrap();
    lane.apply_estimate(estimate, estimate.observed_at())
        .unwrap();
}

fn command(lane: &mut TpcLane, slot: u64) -> Result<Bit, TransmitError> {
    let start = at(lane.anchor.index + slot * CHIPS_PER_SLOT);
    lane.take(TpcRequest {
        identity: lane.identity.unwrap(),
        physical_slot_start: start,
        synthesis_at: start,
    })
}

#[test]
fn algorithm2_holds_and_adjustments_follow_each_ues_offset_frame_across_cfn_wrap() {
    let mut first = initialization_lane(255, initialization(true, 1, 100));
    first.anchor = at(512);
    first.grouped = Some(GroupedTpc::new(2));
    let mut second = initialization_lane(254, initialization(true, 1, 100));
    second.anchor = at(1024);
    second.identity = Some(identity(1, 2));
    second.grouped = Some(GroupedTpc::new(0));
    measured(&mut first, 0, 64, 1.0);
    measured(&mut second, 0, 64, 20.0);
    for slot in 0_u64..30 {
        if slot == 11 {
            measured(&mut first, 11, 70, 20.0);
        }
        let expected = match slot {
            10..=14 => Bit::One,
            25..=29 => Bit::Zero,
            _ => Bit::from(slot.is_multiple_of(2)),
        };
        assert_eq!(command(&mut first, slot), Ok(expected), "slot {slot}");
        assert_eq!(command(&mut second, slot), Ok(Bit::Zero));
    }
    assert_eq!(first.estimates.counters.algorithm2_control_groups, 2);
    assert_eq!(first.estimates.counters.algorithm2_hold_groups, 4);
    assert_eq!(first.estimates.counters.algorithm2_hold_slots, 20);
    assert_eq!(second.estimates.counters.algorithm2_control_groups, 6);
}

#[test]
fn stale_algorithm2_measurements_hold_without_restarting_initialization_and_resume_at_a_boundary() {
    let mut lane = initialization_lane(0, initialization(true, 0, 10));
    lane.grouped = Some(GroupedTpc::new(0));
    measured(&mut lane, 0, 5, 1.0);
    for slot in 0..5 {
        assert_eq!(command(&mut lane, slot), Ok(Bit::One));
    }
    for slot in 5..103 {
        assert_eq!(
            command(&mut lane, slot),
            Ok(Bit::from(slot.is_multiple_of(2)))
        );
    }
    measured(&mut lane, 103, 120, 20.0);
    for slot in 103..105 {
        assert_eq!(
            command(&mut lane, slot),
            Ok(Bit::from(slot.is_multiple_of(2)))
        );
    }
    for slot in 105..110 {
        assert_eq!(command(&mut lane, slot), Ok(Bit::Zero));
    }
    assert_eq!(lane.estimates.counters.algorithm2_control_groups, 2);
    assert!(lane.estimates.counters.algorithm2_stale_hold_groups > 0);
    assert_eq!(
        lane.estimates
            .counters
            .last_expiry
            .unwrap()
            .active
            .valid_until,
        at(5 * CHIPS_PER_SLOT)
    );
}

#[test]
fn a_group_admits_fresh_evidence_then_finishes_unchanged_after_measurement_expiry() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    lane.grouped = Some(GroupedTpc::new(0));
    measured(&mut lane, 0, 3, 1.0);
    for slot in 0..5 {
        assert_eq!(command(&mut lane, slot), Ok(Bit::One));
    }
    measured(&mut lane, 5, 15, 1.0);
    assert_eq!(command(&mut lane, 5), Ok(Bit::One));
    measured(&mut lane, 6, 20, 20.0);
    for slot in 6..10 {
        assert_eq!(command(&mut lane, slot), Ok(Bit::One));
    }
    for slot in 10..15 {
        assert_eq!(command(&mut lane, slot), Ok(Bit::Zero));
    }
    assert_eq!(lane.estimates.counters.algorithm2_control_groups, 3);
    assert_eq!(lane.estimates.counters.algorithm2_hold_groups, 0);
    assert_eq!(lane.estimates.counters.groups.up, 2);
    assert_eq!(lane.estimates.counters.groups.down, 1);
    assert_eq!(
        lane.estimates
            .counters
            .groups
            .latched_beyond_measurement_window,
        1
    );
}

#[test]
fn an_expired_measurement_cannot_start_a_new_correction_group() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    lane.grouped = Some(GroupedTpc::new(0));
    measured(&mut lane, 0, 5, 1.0);
    for slot in 5..10 {
        assert_eq!(
            command(&mut lane, slot),
            Ok(Bit::from(slot.is_multiple_of(2)))
        );
    }
    assert_eq!(lane.estimates.counters.groups.up, 0);
    assert_eq!(lane.estimates.counters.groups.down, 0);
    assert_eq!(lane.estimates.counters.groups.missing_hold, 1);
}

#[test]
fn algorithm2_corrects_every_group_while_far_above_target() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    lane.grouped = Some(GroupedTpc::new(2));
    measured(&mut lane, 0, 64, 100.0);
    for slot in 0_u64..15 {
        assert_eq!(command(&mut lane, slot), Ok(Bit::Zero), "slot {slot}");
    }
    measured(&mut lane, 15, 79, 20.0);
    for slot in 15_u64..25 {
        assert_eq!(
            command(&mut lane, slot),
            Ok(Bit::from(slot.is_multiple_of(2))),
            "slot {slot}"
        );
    }
    assert_eq!(lane.estimates.counters.algorithm2_control_groups, 3);
}

#[test]
fn algorithm2_does_not_replace_explicit_command_exhaustion_with_a_hold() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    lane.grouped = Some(GroupedTpc::new(2));
    lane.apply_commands(identity(0, 1), at(0), &[Bit::One], at(0))
        .unwrap();
    assert_eq!(command(&mut lane, 0), Ok(Bit::One));
    assert_eq!(command(&mut lane, 1), Err(TransmitError::TpcUnavailable));
}

#[test]
fn admission_uses_the_advertised_algorithm_and_rejects_invalid_group_configuration() {
    use crate::dedicated_tx::tests::compile_with_algorithm;
    use umts_rrc::dedicated::UplinkPowerControl;

    let (_source, mut runtime, mut ports) = runtime(4, 4);
    for (slot, algorithm) in [
        UplinkPowerControl::Algorithm1 { step_db: 1 },
        UplinkPowerControl::Algorithm2,
    ]
    .into_iter()
    .enumerate()
    {
        let mut owner = Box::new(session(
            compile_with_algorithm(
                true,
                false,
                true,
                3 + slot as u16,
                2 * slot as u8,
                algorithm,
            ),
            identity(slot as u32, 1),
            FrameAnchor {
                time: at(512 * slot as u64),
                cfn: 0,
            },
            4,
        ));
        assert_eq!(
            owner.configure_algorithm2_hold_groups(9),
            Err(TransmitError::Payload)
        );
        owner.configure_algorithm2_hold_groups(2).unwrap();
        owner
            .configure_initial_tpc(initialization(true, 1, 100))
            .unwrap();
        install(&mut runtime, &mut ports, owner);
        let lane = runtime.tpc_lane_mut(identity(slot as u32, 1)).unwrap();
        assert_eq!(lane.grouped.is_some(), slot == 1);
        measured(lane, 0, 64, 1.0);
        for air_slot in 0_u64..5 {
            let expected = if slot == 0 {
                Bit::One
            } else {
                Bit::from(air_slot.is_multiple_of(2))
            };
            assert_eq!(command(lane, air_slot), Ok(expected));
        }
    }
}
