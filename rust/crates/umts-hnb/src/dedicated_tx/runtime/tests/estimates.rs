// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn estimate(owner: ReceiveIdentity, until: u64, observed: u64, sir: f64) -> TpcEstimate {
    TpcEstimate::new(owner, at(until * CHIPS_PER_SLOT), at(observed), sir, 10.0).unwrap()
}

#[test]
fn valid_measurement_generates_each_slot_and_expiry_never_restarts_initial_power_ramping() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    let measured = estimate(identity(0, 1), 6, 1000, 20.0);
    assert_eq!(take_initial(&mut lane, 0), Ok(Bit::Zero));
    assert_eq!(take_initial(&mut lane, 1), Ok(Bit::One));
    lane.apply_estimate(measured, at(2 * CHIPS_PER_SLOT))
        .unwrap();
    for slot in 2..6 {
        assert_eq!(take_initial(&mut lane, slot), Ok(Bit::Zero));
    }
    assert_eq!(
        take_initial(&mut lane, 6),
        Err(TransmitError::TpcUnavailable)
    );
    assert_eq!(measured.observed_at(), at(1000));
    assert_eq!(measured.valid_until(), at(6 * CHIPS_PER_SLOT));
    assert_eq!(
        lane.estimates.counters,
        TpcEstimateCounters {
            installed: 1,
            activated: 1,
            held_slots: 3,
            superseded: 0,
            maximum_observation_age_chips: 5 * CHIPS_PER_SLOT - 1000,
            expired_slots: 1,
            last_expiry: Some(TpcExpiryEvidence {
                identity: identity(0, 1),
                requested: at(6 * CHIPS_PER_SLOT),
                active: measured.into(),
                latest: Some(measured.into()),
                pending_updates: 0,
            }),
            ..TpcEstimateCounters::default()
        }
    );
}

#[test]
fn future_measurements_remain_causal_and_aggregation_keeps_latest_expiry() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    let active = estimate(identity(0, 1), 4, 1000, 20.0);
    let future = estimate(identity(0, 1), 9, 6 * CHIPS_PER_SLOT, 1.0);
    lane.apply_estimate(active, at(0)).unwrap();
    lane.apply_estimate(future, at(0)).unwrap();
    assert_eq!(take_initial(&mut lane, 2), Ok(Bit::Zero));
    assert_eq!(
        take_initial(&mut lane, 4),
        Err(TransmitError::TpcUnavailable)
    );
    let early = lane.estimates.counters;
    assert_eq!(
        early.last_expiry,
        Some(TpcExpiryEvidence {
            identity: identity(0, 1),
            requested: at(4 * CHIPS_PER_SLOT),
            active: active.into(),
            latest: Some(future.into()),
            pending_updates: 1,
        })
    );
    assert_eq!(take_initial(&mut lane, 6), Ok(Bit::One));
    assert_eq!(
        take_initial(&mut lane, 9),
        Err(TransmitError::TpcUnavailable)
    );
    let late = lane.estimates.counters;
    for ordered in [[early, late], [late, early]] {
        let mut counters = TpcEstimateCounters::default();
        for value in ordered {
            counters.add(value);
        }
        assert_eq!(
            counters.last_expiry.unwrap().requested,
            at(9 * CHIPS_PER_SLOT)
        );
    }
}

#[test]
fn newer_measurement_is_selected_at_the_next_unrendered_slot_without_a_reserved_command_time() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    lane.apply_estimate(estimate(identity(0, 1), 6, 1000, 20.0), at(0))
        .unwrap();
    assert_eq!(take_initial(&mut lane, 2), Ok(Bit::Zero));
    lane.apply_estimate(
        estimate(identity(0, 1), 9, 7000, 1.0),
        at(3 * CHIPS_PER_SLOT),
    )
    .unwrap();
    for slot in 3..9 {
        assert_eq!(take_initial(&mut lane, slot), Ok(Bit::One));
    }
    assert_eq!(
        take_initial(&mut lane, 9),
        Err(TransmitError::TpcUnavailable)
    );
    assert_eq!(lane.estimates.counters.activated, 2);
    assert_eq!(lane.estimates.counters.held_slots, 5);
}

#[test]
fn captured_expiry_boundary_uses_the_newer_available_measurement_without_extending_either_expiry() {
    const REQUESTED: u64 = 347_207_680;
    for fresh_available in [false, true] {
        let mut lane = initialization_lane(0, initialization(true, 1, 100));
        let old =
            TpcEstimate::new(identity(0, 1), at(347_205_307), at(347_183_160), 20.0, 10.0).unwrap();
        lane.apply_estimate(old, at(347_202_560)).unwrap();
        assert_eq!(
            take_initial(&mut lane, 347_202_560 / CHIPS_PER_SLOT),
            Ok(Bit::Zero)
        );
        if fresh_available {
            let fresh =
                TpcEstimate::new(identity(0, 1), at(347_220_667), at(347_198_520), 1.0, 10.0)
                    .unwrap();
            lane.apply_estimate(fresh, at(REQUESTED)).unwrap();
            assert_eq!(
                take_initial(&mut lane, REQUESTED / CHIPS_PER_SLOT),
                Ok(Bit::One)
            );
            assert_eq!(lane.estimates.counters.activated, 2);
            assert_eq!(fresh.valid_until(), at(347_220_667));
        } else {
            assert_eq!(
                take_initial(&mut lane, REQUESTED / CHIPS_PER_SLOT),
                Err(TransmitError::TpcUnavailable)
            );
        }
        assert_eq!(old.valid_until(), at(347_205_307));
    }
}

#[test]
fn newest_unused_measurement_supersedes_backlog_and_expired_updates_do_not_replace_it() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    for (observed, sir) in [(0, 20.0), (1000, 20.0), (2000, 1.0)] {
        lane.apply_estimate(
            estimate(identity(0, 1), 6, observed, sir),
            at(2 * CHIPS_PER_SLOT),
        )
        .unwrap();
    }
    assert_eq!(lane.estimates.counters.installed, 3);
    assert_eq!(lane.estimates.counters.superseded, 2);
    assert_eq!(take_initial(&mut lane, 2), Ok(Bit::One));
    assert_eq!(lane.estimates.counters.activated, 1);
    assert_eq!(
        lane.apply_estimate(
            estimate(identity(0, 1), 3, 3000, 20.0),
            at(3 * CHIPS_PER_SLOT)
        ),
        Err(TransmitError::FeedbackLate)
    );
    assert_eq!(lane.estimates.counters.installed, 3);
    assert_eq!(take_initial(&mut lane, 3), Ok(Bit::One));
}

#[test]
fn mixed_authorities_and_old_measurements_are_rejected_without_modifying_valid_state() {
    let mut lane = initialization_lane(0, initialization(true, 1, 100));
    let first = estimate(identity(0, 1), 6, 1000, 20.0);
    lane.apply_estimate(first, at(0)).unwrap();
    assert_eq!(
        lane.apply_commands(identity(0, 1), at(8 * CHIPS_PER_SLOT), &[Bit::One], at(0)),
        Err(TransmitError::Conflict)
    );
    assert!(lane.decisions.iter().all(Option::is_none));
    assert_eq!(
        lane.apply_estimate(estimate(identity(0, 1), 7, 999, 1.0), at(0)),
        Err(TransmitError::FeedbackOrder)
    );
    assert_eq!(
        lane.apply_estimate(estimate(identity(0, 2), 7, 2000, 1.0), at(0)),
        Err(TransmitError::Identity)
    );
    let horizon = ESTIMATE_SLOTS as u64 * CHIPS_PER_SLOT;
    assert_eq!(
        lane.apply_estimate(
            estimate(identity(0, 1), ESTIMATE_SLOTS as u64 + 2, horizon + 1, 1.0),
            at(0),
        ),
        Err(TransmitError::FeedbackFuture)
    );
    let rejected = lane.estimates.counters.last_rejection.unwrap();
    assert_eq!(rejected.observed_at, at(horizon + 1));
    assert_eq!(rejected.frontier, at(0));
    assert_eq!(rejected.previous, Some(at(1000)));
    assert_eq!(take_initial(&mut lane, 2), Ok(Bit::Zero));
    assert_eq!(lane.estimates.counters.installed, 1);
    assert_eq!(
        lane.apply_estimate(
            estimate(identity(0, 1), 5, 1500, 1.0),
            at(5 * CHIPS_PER_SLOT)
        ),
        Err(TransmitError::FeedbackLate)
    );
}

#[test]
fn estimate_constructor_rejects_nonfinite_and_unbounded_evidence() {
    for sir in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            TpcEstimate::new(identity(0, 1), at(10240), at(1000), sir, 10.0),
            Err(TransmitError::Payload)
        );
    }
    assert_eq!(
        TpcEstimate::new(identity(0, 1), at(1000), at(1000), 1.0, 10.0),
        Err(TransmitError::Time)
    );
    assert_eq!(
        TpcEstimate::new(
            identity(0, 1),
            at(1000 + (ESTIMATE_SLOTS as u64 + 1) * CHIPS_PER_SLOT),
            at(1000),
            1.0,
            10.0
        ),
        Err(TransmitError::Time)
    );
}

#[test]
fn one_ue_expiry_and_removal_do_not_change_peer_estimates_or_aggregate_accounting() {
    let (_, mut runtime, mut ports) = runtime(4, 4);
    install(&mut runtime, &mut ports, owner(0, 1));
    install(&mut runtime, &mut ports, owner(1, 1));
    for (id, sir) in [(identity(0, 1), 20.0), (identity(1, 1), 1.0)] {
        let update = estimate(id, 6, 1000, sir);
        ports.feedback.publish_estimate(update).unwrap();
        runtime.service().unwrap();
        assert!(
            matches!(ports.feedback.try_event().unwrap().outcome, TransmitCommandOutcome::Estimate { estimate: accepted, result: Ok(()) } if accepted == update)
        );
    }
    for (id, bit) in [(identity(0, 1), Bit::Zero), (identity(1, 1), Bit::One)] {
        let lane = runtime.tpc_lane_mut(id).unwrap();
        assert_eq!(
            lane.take(TpcRequest {
                identity: id,
                physical_slot_start: at(2 * CHIPS_PER_SLOT),
                synthesis_at: at(2 * CHIPS_PER_SLOT)
            }),
            Ok(bit)
        );
    }
    assert_eq!(runtime.estimate_counters().installed, 2);
    ports.control.remove(identity(0, 1)).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Removed { result: Ok(_), .. }
    ));
    assert_eq!(runtime.estimate_counters().installed, 2);
    let id = identity(1, 1);
    assert_eq!(
        runtime.tpc_lane_mut(id).unwrap().take(TpcRequest {
            identity: id,
            physical_slot_start: at(3 * CHIPS_PER_SLOT),
            synthesis_at: at(3 * CHIPS_PER_SLOT)
        }),
        Ok(Bit::One)
    );
    assert_eq!(runtime.estimate_counters().held_slots, 1);
}

#[test]
fn measured_and_explicit_control_generate_identical_full_carrier_samples() {
    let mut outputs = Vec::new();
    for measured in [false, true] {
        let (mut source, mut runtime, mut ports) = runtime(4, 4);
        let owner = owner(0, 1);
        let data = input(&owner);
        install(&mut runtime, &mut ports, owner);
        frame(&mut runtime, &mut ports, data);
        ports.control.arm(identity(0, 1)).unwrap();
        runtime.service().unwrap();
        assert!(matches!(
            ports.control.try_event().unwrap().outcome,
            TransmitCommandOutcome::Armed { result: Ok(()), .. }
        ));
        if measured {
            ports
                .feedback
                .publish_estimate(estimate(identity(0, 1), 15, 0, 20.0))
                .unwrap();
            runtime.service().unwrap();
            assert!(matches!(
                ports.feedback.try_event().unwrap().outcome,
                TransmitCommandOutcome::Estimate { result: Ok(()), .. }
            ));
        } else {
            feedback(
                &mut runtime,
                &mut ports,
                identity(0, 1),
                0,
                vec![Bit::Zero; 15],
            );
        }
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        let mut output = Vec::new();
        for _ in 0..15 {
            runtime.generate_slot(&mut source, &mut samples).unwrap();
            output.extend_from_slice(&samples);
        }
        assert_eq!(
            runtime
                .bank
                .session(identity(0, 1))
                .unwrap()
                .counters()
                .consumed_frames,
            1
        );
        if measured {
            assert_eq!(runtime.estimate_counters().activated, 1);
            assert_eq!(runtime.estimate_counters().held_slots, 14);
        }
        outputs.push(output);
    }
    assert_eq!(outputs[0], outputs[1]);
}
