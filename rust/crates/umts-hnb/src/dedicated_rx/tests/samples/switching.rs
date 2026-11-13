// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn result(
    runtime: &mut DedicatedSampleRuntime,
    ticket: &mut ReceiveCommandTicket,
) -> ReceiveCommandResult {
    for _ in 0..16 {
        run(runtime);
        if let Some(result) = ticket.try_result() {
            return result;
        }
    }
    panic!("bounded command did not complete");
}
fn staged(runtime: &mut DedicatedSampleRuntime, port: &mut DedicatedReceivePort, first: u64) {
    let config = configuration();
    let mut installed = install(port, owner(0, config.clone(), 20_096));
    assert!(matches!(
        result(runtime, &mut installed),
        ReceiveCommandResult::Installed(_)
    ));
    let mut pending = port
        .stage_replacement(identity(0, 1), owner_at(0, 2, 4, config, first))
        .ok()
        .unwrap();
    assert!(
        matches!(result(runtime, &mut pending), ReceiveCommandResult::ReplacementPrepared { previous, identity: next }
        if previous == identity(0, 1) && next == identity(0, 2))
    );
}
fn observed(
    runtime: &mut DedicatedSampleRuntime,
    port: &mut DedicatedReceivePort,
    id: ReceiveIdentity,
) -> ReceivePlanStatus {
    let mut ticket = port.observe(id).unwrap();
    let ReceiveCommandResult::Observed {
        identity,
        status: Some(status),
    } = result(runtime, &mut ticket)
    else {
        panic!("missing owner observation");
    };
    assert_eq!(identity, id);
    status
}

#[test]
fn staged_owner_has_real_admission_and_cannot_use_standalone_arm() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    staged(&mut runtime, &mut port, 20_096 + 4 * CHIPS_PER_FRAME * 2);
    assert_eq!(port.capacity(), 2);
    assert_eq!(port.reserved_output_bytes(), 10_000);
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 1)).phase,
        ReceivePlanPhase::Active
    );
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 2)).phase,
        ReceivePlanPhase::Staged {
            previous: identity(0, 1)
        }
    );
    let mut arm = port.arm(identity(0, 2)).unwrap();
    assert!(matches!(
        result(&mut runtime, &mut arm),
        ReceiveCommandResult::ArmRejected {
            error: ReceiveError::Configuration,
            ..
        }
    ));
    assert_eq!(runtime.counters().sampled_slots, 0);
    assert!(
        port.stage_replacement(
            identity(0, 2),
            owner_at(0, 3, 8, configuration(), 20_096 + 8 * CHIPS_PER_FRAME * 2)
        )
        .is_err()
    );
}

#[test]
fn cancelled_staging_keeps_old_owner_and_rejects_reused_generation() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let first = 20_096 + 4 * CHIPS_PER_FRAME * 2;
    staged(&mut runtime, &mut port, first);
    let mut remove = port.remove(identity(0, 2)).unwrap();
    assert!(matches!(
        result(&mut runtime, &mut remove),
        ReceiveCommandResult::Removed {
            receiver: Some(_),
            ..
        }
    ));
    assert_eq!(port.retire_output(identity(0, 2)), Ok(0));
    assert_eq!(port.reserved_output_bytes(), 5000);
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 1)).phase,
        ReceivePlanPhase::Active
    );
    let mut retry = port
        .stage_replacement(identity(0, 1), owner_at(0, 2, 4, configuration(), first))
        .ok()
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut retry),
        ReceiveCommandResult::InstallRejected {
            error: ReceiveError::Identity,
            ..
        }
    ));
    assert_eq!(port.retire_output(identity(0, 2)), Ok(0));
    let mut fresh = port
        .stage_replacement(identity(0, 1), owner_at(0, 3, 4, configuration(), first))
        .ok()
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut fresh),
        ReceiveCommandResult::ReplacementPrepared { .. }
    ));
}

#[test]
fn wrong_predecessor_commit_is_atomic_and_late_commit_cannot_backdate_activation() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let first = 20_096 + 4 * CHIPS_PER_FRAME * 2;
    staged(&mut runtime, &mut port, first);
    let mut wrong = port
        .commit_replacement(identity(1, 1), identity(0, 2))
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut wrong),
        ReceiveCommandResult::CommitRejected {
            error: ReceiveError::Identity,
            ..
        }
    ));
    runtime
        .feed(matched(first + 10, &[Complex32::default(); 8], 1, false))
        .unwrap();
    let mut late = port
        .commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut late),
        ReceiveCommandResult::CommitRejected {
            error: ReceiveError::Time,
            ..
        }
    ));
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 1)).phase,
        ReceivePlanPhase::Active
    );
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 2)).phase,
        ReceivePlanPhase::Staged {
            previous: identity(0, 1)
        }
    );
}

#[test]
fn output_and_worker_budgets_reject_overlap_without_disturbing_old_owner() {
    let config = configuration();
    let mut limits = runtime_limits();
    limits.maximum_output_bytes = 5000;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    let mut installed = install(&mut port, owner(0, config.clone(), 20_096));
    result(&mut runtime, &mut installed);
    assert!(
        port.stage_replacement(
            identity(0, 1),
            owner_at(0, 2, 4, config, 20_096 + 4 * CHIPS_PER_FRAME * 2)
        )
        .is_err()
    );
    assert_eq!(port.reserved_output_bytes(), 5000);
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 1)).phase,
        ReceivePlanPhase::Active
    );
}

#[test]
fn two_ue_replacements_reserve_configured_payloads_not_global_width_ceiling() {
    let mut limits = runtime_limits();
    limits.maximum_output_bits = count(262_144);
    limits.output_slots_per_ue = count(8);
    limits.maximum_output_bytes = 8 * 1024 * 1024;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    for slot in 0..2 {
        let mut ticket = install(&mut port, owner(slot, configuration(), 20_096));
        assert!(matches!(
            result(&mut runtime, &mut ticket),
            ReceiveCommandResult::Installed(_)
        ));
    }
    for slot in 0..2 {
        let candidate = owner_at(
            slot,
            2,
            4,
            configuration(),
            20_096 + 4 * CHIPS_PER_FRAME * 2,
        );
        let mut ticket = port
            .stage_replacement(identity(slot, 1), candidate)
            .ok()
            .unwrap();
        assert!(matches!(
            result(&mut runtime, &mut ticket),
            ReceiveCommandResult::ReplacementPrepared { .. }
        ));
    }
    assert_eq!(port.reserved_output_bytes(), 4 * 8 * 5000);
}

#[test]
fn commit_keeps_retirement_explicit_and_does_not_mean_received_samples() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    staged(&mut runtime, &mut port, 20_096 + 4 * CHIPS_PER_FRAME * 2);
    let mut commit = port
        .commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    assert!(
        matches!(result(&mut runtime, &mut commit), ReceiveCommandResult::ReplacementCommitted { boundary, .. } if boundary.time == at(4) && boundary.cfn == 4)
    );
    assert_eq!(runtime.counters().sampled_slots, 0);
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 2)).phase,
        ReceivePlanPhase::Committed {
            previous: identity(0, 1)
        }
    );
    let mut early = port.retire_drained(identity(0, 1)).unwrap();
    assert!(matches!(
        result(&mut runtime, &mut early),
        ReceiveCommandResult::RetireRejected {
            error: ReceiveError::Occupied,
            ..
        }
    ));
    assert_eq!(port.reserved_output_bytes(), 10_000);
}

#[test]
fn scheduled_sample_pipeline_preserves_crc_payloads_order_and_peer_progress() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 8, 2, 1.0));
    let mut limits = runtime_limits();
    limits.maximum_output_bytes = 20_000;
    limits.shard.maximum_sessions_per_step = count(3);
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    staged(&mut runtime, &mut port, 20_096 + 4 * CHIPS_PER_FRAME * 2);
    let mut peer = install(&mut port, owner(1, config.clone(), 20_096));
    result(&mut runtime, &mut peer);
    let mut commit = port
        .commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut commit),
        ReceiveCommandResult::ReplacementCommitted { .. }
    ));
    let mut deliveries = Vec::new();
    for (chunk, input) in samples.chunks(5120).enumerate() {
        runtime
            .feed(matched(first + (chunk * 5120) as u64, input, 1, false))
            .unwrap();
        for _ in 0..2 {
            run(&mut runtime);
            port.route_one(|delivery| {
                assert_ne!(delivery.decoded.identity, identity(0, 2));
                if delivery.decoded.identity == identity(0, 1) {
                    return false;
                }
                deliveries.push((delivery.decoded, delivery.payload.to_vec()));
                true
            });
        }
    }
    for _ in 0..64 {
        run(&mut runtime);
        port.route_one(|delivery| {
            assert_ne!(delivery.decoded.identity, identity(0, 2));
            if delivery.decoded.identity == identity(0, 1) {
                return false;
            }
            deliveries.push((delivery.decoded, delivery.payload.to_vec()));
            true
        });
    }
    assert_eq!(deliveries.len(), 12);
    assert!(
        deliveries
            .iter()
            .all(|(decoded, _)| decoded.identity == identity(1, 1))
    );
    let mut blocked = port.retire_drained(identity(0, 1)).unwrap();
    assert!(matches!(
        result(&mut runtime, &mut blocked),
        ReceiveCommandResult::RetireRejected {
            error: ReceiveError::Occupied,
            ..
        }
    ));
    for _ in 0..100 {
        run(&mut runtime);
        port.route_one(|delivery| {
            assert_ne!(delivery.decoded.identity, identity(0, 2));
            deliveries.push((delivery.decoded, delivery.payload.to_vec()));
            true
        });
    }
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 1)).next_frame,
        at(4)
    );
    let mut retired = port.retire_drained(identity(0, 1)).unwrap();
    let ReceiveCommandResult::Retired { receiver, .. } = result(&mut runtime, &mut retired) else {
        panic!("old receiver did not drain");
    };
    assert_eq!(receiver.counters().sampled_slots, 4 * 15);
    assert_eq!(receiver.session().counters().frames, 4);
    for _ in 0..100 {
        run(&mut runtime);
        port.route_one(|delivery| {
            deliveries.push((delivery.decoded, delivery.payload.to_vec()));
            true
        });
    }
    for (id, expected, seed) in [
        (identity(0, 1), 6, 7),
        (identity(0, 2), 6, 7),
        (identity(1, 1), 12, 8),
    ] {
        let own: Vec<_> = deliveries
            .iter()
            .filter(|(decoded, _)| decoded.identity == id)
            .collect();
        assert_eq!(own.len(), expected);
        for (decoded, bytes) in own {
            assert!(decoded.crc_checked);
            assert_eq!(*bytes, payload(&config, decoded.transport, 1, seed));
            if id.configuration.get() == 1 && id.ue.slot == 0 {
                assert!(decoded.start.index < at(4).index);
            }
            if id.configuration.get() == 2 {
                assert!(decoded.start.index >= at(4).index);
            }
        }
    }
    let ue: Vec<_> = deliveries
        .iter()
        .filter(|(decoded, _)| decoded.identity.ue.slot == 0)
        .map(|(decoded, _)| decoded.identity.configuration.get())
        .collect();
    assert_eq!(ue, [vec![1; 6], vec![2; 6]].concat());
    assert_eq!(runtime.counters().failed_owners, 0);
    assert_eq!(port.retire_output(identity(0, 1)), Ok(0));
}

#[test]
fn incompatible_sample_origin_cannot_forge_a_new_configuration_frame() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut installed = install(&mut port, owner(0, configuration(), 20_096));
    result(&mut runtime, &mut installed);
    let mut ticket = port
        .stage_replacement(identity(0, 1), owner_at(0, 2, 4, configuration(), 20_096))
        .ok()
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut ticket),
        ReceiveCommandResult::InstallRejected {
            error: ReceiveError::Time,
            ..
        }
    ));
    assert_eq!(
        observed(&mut runtime, &mut port, identity(0, 1)).phase,
        ReceivePlanPhase::Active
    );
    assert_eq!(port.retire_output(identity(0, 2)), Ok(0));
    assert_eq!(port.reserved_output_bytes(), 5000);
}

#[test]
fn worker_resource_rejection_returns_exact_receiver_and_retains_old_resources() {
    let mut limits = runtime_limits();
    limits.shard.resources.channels = 2;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    let mut installed = install(&mut port, owner(0, configuration(), 20_096));
    result(&mut runtime, &mut installed);
    let candidate = owner_at(0, 2, 4, configuration(), 20_096 + 4 * CHIPS_PER_FRAME * 2);
    let address = std::ptr::from_ref(candidate.as_ref());
    let mut ticket = port
        .stage_replacement(identity(0, 1), candidate)
        .ok()
        .unwrap();
    let ReceiveCommandResult::InstallRejected { error, receiver } =
        result(&mut runtime, &mut ticket)
    else {
        panic!("resource overlap was admitted");
    };
    assert!(
        matches!(error, ReceiveError::ResourceBudget { resource: crate::dedicated_rx::ReceiveResource::Channels, required, maximum }
        if required > maximum)
    );
    assert_eq!(std::ptr::from_ref(receiver.as_ref()), address);
    assert_eq!(port.retire_output(identity(0, 2)), Ok(0));
    assert_eq!(port.reserved_output_bytes(), 5000);
}

#[test]
fn full_command_queue_returns_receiver_without_leaking_an_output_lane() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut installed = install(&mut port, owner(0, configuration(), 20_096));
    result(&mut runtime, &mut installed);
    let _first = port.observe(identity(0, 1)).unwrap();
    let _second = port.observe(identity(0, 1)).unwrap();
    let candidate = owner_at(0, 2, 4, configuration(), 20_096 + 4 * CHIPS_PER_FRAME * 2);
    let address = std::ptr::from_ref(candidate.as_ref());
    let Err(returned) = port.stage_replacement(identity(0, 1), candidate) else {
        panic!("full command queue admitted receiver");
    };
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(port.reserved_output_bytes(), 5000);
}

#[test]
fn staging_includes_removed_long_tti_and_preserves_cfn_wrap() {
    let old_config = configuration_with_interval(Rel99Tti::Ms40);
    let resources = session(0, 8, old_config.clone()).resources();
    let mut shard = DedicatedRxShard::new(
        epoch(),
        ShardLimits {
            maximum_ues: count(1),
            maximum_sessions_per_step: count(1),
            resources: ReceiveResources {
                coded_bits: resources.coded_bits * 4,
                output_bytes: resources.output_bytes * 4,
                demultiplexed_bits: resources.demultiplexed_bits * 4,
                channels: 8,
                ..resources
            },
        },
    )
    .unwrap();
    let old = DedicatedReceiveSession::new(
        identity(0, 1),
        FrameAnchor {
            time: at(0),
            cfn: 252,
        },
        old_config,
        limits(8),
    )
    .unwrap();
    assert!(shard.install(Box::new(old)).is_ok());
    let next = |frame, cfn| {
        Box::new(
            DedicatedReceiveSession::new(
                identity(0, 2),
                FrameAnchor {
                    time: at(frame),
                    cfn,
                },
                configuration(),
                limits(8),
            )
            .unwrap(),
        )
    };
    assert!(matches!(
        shard.stage_replacement(identity(0, 1), next(2, 254)),
        Err((ReceiveError::Time, _))
    ));
    assert!(matches!(
        shard.stage_replacement(identity(0, 1), next(4, 4)),
        Err((ReceiveError::Time, _))
    ));
    assert!(shard.stage_replacement(identity(0, 1), next(4, 0)).is_ok());
    assert!(shard.session_mut(identity(0, 1)).is_some());
    assert!(shard.session_mut(identity(0, 2)).is_some());
    let mut seen = Vec::new();
    for _ in 0..4 {
        assert_eq!(
            shard.service(|identity, result| {
                result.unwrap();
                seen.push(identity);
            }),
            1
        );
    }
    assert_eq!(
        seen,
        [
            identity(0, 1),
            identity(0, 2),
            identity(0, 1),
            identity(0, 2)
        ]
    );
}

#[test]
fn a_sample_gap_across_activation_never_decodes_new_frames_with_old_plan() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let first = 20_096 + 4 * CHIPS_PER_FRAME * 2;
    staged(&mut runtime, &mut port, first);
    let mut commit = port
        .commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut commit),
        ReceiveCommandResult::ReplacementCommitted { .. }
    ));
    runtime
        .feed(matched(first + 100, &[Complex32::default(); 256], 2, true))
        .unwrap();
    for _ in 0..8 {
        run(&mut runtime);
    }
    let old = observed(&mut runtime, &mut port, identity(0, 1));
    assert!(matches!(old.phase, ReceivePlanPhase::Stopped { boundary, .. } if boundary == at(4)));
    assert_eq!(old.last_consumed_frame, None);
    let mut retired = port.retire_drained(identity(0, 1)).unwrap();
    let ReceiveCommandResult::Retired { receiver, .. } = result(&mut runtime, &mut retired) else {
        panic!("gapped old owner did not retire");
    };
    assert_eq!(receiver.counters().sampled_slots, 0);
    assert!(receiver.counters().history_lost_frames > 0);
    assert!(!port.route_one(|_| panic!("gap created a transport")));
}

#[test]
fn retirement_proof_retains_the_actual_committed_successor_and_boundary() {
    for (generation, frame) in [(2, 4), (3, 4), (2, 8)] {
        let (mut runtime, mut port) =
            DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
        let first = 20_096 + frame * CHIPS_PER_FRAME * 2;
        let mut installed = install(&mut port, owner(0, configuration(), 20_096));
        assert!(matches!(
            result(&mut runtime, &mut installed),
            ReceiveCommandResult::Installed(_)
        ));
        let mut staged = port
            .stage_replacement(
                identity(0, 1),
                owner_at(0, generation, frame, configuration(), first),
            )
            .ok()
            .unwrap();
        assert!(matches!(
            result(&mut runtime, &mut staged),
            ReceiveCommandResult::ReplacementPrepared { .. }
        ));
        let mut committed = port
            .commit_replacement(identity(0, 1), identity(0, generation))
            .unwrap();
        assert!(matches!(
            result(&mut runtime, &mut committed),
            ReceiveCommandResult::ReplacementCommitted { .. }
        ));
        runtime
            .feed(matched(first + 100, &[Complex32::default(); 256], 2, true))
            .unwrap();
        for _ in 0..8 {
            run(&mut runtime);
        }
        let mut retired = port.retire_drained(identity(0, 1)).unwrap();
        let ReceiveCommandResult::Retired {
            receiver,
            retirement,
            identity: previous,
        } = result(&mut runtime, &mut retired)
        else {
            panic!("committed old receiver did not retire");
        };
        assert_eq!(previous, identity(0, 1));
        assert_eq!(retirement.previous(), previous);
        assert_eq!(retirement.old_anchor().time, at(0));
        assert_eq!(retirement.old_anchor().cfn, 0);
        assert_eq!(retirement.successor(), identity(0, generation));
        assert_ne!(
            retirement.successor(),
            identity(0, if generation == 2 { 3 } else { 2 })
        );
        assert_eq!(retirement.boundary(), at(frame));
        assert_ne!(retirement.boundary(), at(if frame == 4 { 8 } else { 4 }));
        assert!(receiver.next_frame().index >= retirement.boundary().index);
        assert!(!port.route_one(|_| panic!("gapped receiver created transport output")));
    }
}

#[test]
fn sample_origin_validation_includes_fractional_position_at_the_window_edge() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut installed = install(&mut port, owner(0, configuration(), 20_096));
    result(&mut runtime, &mut installed);
    let candidate = Box::new(
        DedicatedSampleReceiver::new(
            chips::receiver_at(0, 2, 4, configuration()),
            SampleReceiveSettings {
                radio_link: Default::default(),
                rate: SamplesPerChip::Two,
                first_receive: SamplePosition::new(
                    sample(20_098 + 4 * CHIPS_PER_FRAME * 2 + 16),
                    0.5,
                )
                .unwrap(),
                search_radius_samples: 8,
                minimum_coherent_fraction: 0.05,
            },
        )
        .unwrap(),
    );
    let mut ticket = port
        .stage_replacement(identity(0, 1), candidate)
        .ok()
        .unwrap();
    assert!(matches!(
        result(&mut runtime, &mut ticket),
        ReceiveCommandResult::InstallRejected {
            error: ReceiveError::Time,
            ..
        }
    ));
}

#[test]
fn teardown_order_cannot_erase_the_newest_consumed_radio_coordinate() {
    for reversed in [false, true] {
        let config = configuration();
        let resources = session(0, 8, config.clone()).resources();
        let mut shard = DedicatedRxShard::new(
            epoch(),
            ShardLimits {
                maximum_ues: count(1),
                maximum_sessions_per_step: count(1),
                resources: ReceiveResources {
                    coded_bits: resources.coded_bits * 3,
                    output_bytes: resources.output_bytes * 3,
                    demultiplexed_bits: resources.demultiplexed_bits * 3,
                    channels: resources.channels * 3,
                    ..resources
                },
            },
        )
        .unwrap();
        assert!(
            shard
                .install(Box::new(session(0, 8, config.clone())))
                .is_ok()
        );
        let successor = |generation, frame| {
            Box::new(
                DedicatedReceiveSession::new(
                    identity(0, generation),
                    FrameAnchor {
                        time: at(frame),
                        cfn: frame as u8,
                    },
                    config.clone(),
                    limits(8),
                )
                .unwrap(),
            )
        };
        assert!(
            shard
                .stage_replacement(identity(0, 1), successor(2, 4))
                .is_ok()
        );
        receive(shard.session_mut(identity(0, 2)).unwrap(), 4, 4, 1, 1, 7).unwrap();
        if reversed {
            shard.remove(identity(0, 2)).unwrap();
            assert!(matches!(
                shard.stage_replacement(identity(0, 1), successor(3, 4)),
                Err((ReceiveError::Time, _))
            ));
            shard.remove(identity(0, 1)).unwrap();
        } else {
            shard.remove(identity(0, 1)).unwrap();
            shard.remove(identity(0, 2)).unwrap();
        }
        assert_eq!(shard.used(), ReceiveResources::default());
        assert!(matches!(
            shard.install(successor(3, 4)),
            Err((ReceiveError::Time, _))
        ));
        assert!(shard.install(successor(3, 6)).is_ok());
    }
}

#[test]
fn uncommitted_staging_does_not_consume_active_decode_budget_or_slow_peers() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 4, 2, 1.0));
    let mut limits = runtime_limits();
    limits.maximum_output_bytes = 20_000;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    staged(&mut runtime, &mut port, 20_096 + 4 * CHIPS_PER_FRAME * 2);
    let mut peer = install(&mut port, owner(1, config.clone(), 20_096));
    result(&mut runtime, &mut peer);
    let mut deliveries = Vec::new();
    for (chunk, input) in samples.chunks(5120).enumerate() {
        runtime
            .feed(matched(first + (chunk * 5120) as u64, input, 1, false))
            .unwrap();
        let serviced = runtime.service(|_, result| {
            result.unwrap();
        });
        assert_eq!(serviced, 2);
        drain(&mut port, &mut deliveries);
    }
    for _ in 0..30 {
        run(&mut runtime);
        drain(&mut port, &mut deliveries);
    }
    for slot in 0..2 {
        let own: Vec<_> = deliveries
            .iter()
            .filter(|(decoded, _)| decoded.identity == identity(slot, 1))
            .collect();
        assert_eq!(own.len(), 6);
        for (decoded, bytes) in own {
            assert_eq!(
                *bytes,
                payload(&config, decoded.transport, 1, 7 + slot as usize)
            );
        }
    }
    assert!(
        !deliveries
            .iter()
            .any(|(decoded, _)| decoded.identity == identity(0, 2))
    );
    assert_eq!(runtime.counters().failed_owners, 0);
}
