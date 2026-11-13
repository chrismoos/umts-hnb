// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{
        BearerEndpoint, BearerId, BearerLimits, DedicatedBearers, ResetPermission,
        TransparentSettings, TransportBinding, TransportChannels,
    },
    dedicated_controller::{
        DedicatedControlError, DedicatedController, DedicatedControllerLimits, DedicatedPhase,
        PreparedDedicated,
    },
};
use umts_rlc::transparent::{ErrorDelivery, Segmentation, TmLimits};
use umts_security::{
    ciphering::{
        BearerCipherConfiguration, BearerCiphers, Configuration, CountAnchor, Position, Scope,
    },
    counters::CipherMode,
};
use umts_types::CnDomain;

fn router(port: DedicatedReceivePort) -> DedicatedController {
    DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 4_000_000,
        },
    )
}
fn prepared(slot: u32, config: Arc<UplinkTfcs>, mismatched_bank: bool) -> Box<PreparedDedicated> {
    let receiver = owner(slot, config.clone(), 20_096);
    let identity = receiver.session().identity();
    let endpoints = [(10, CnDomain::Cs, 80), (20, CnDomain::Ps, 5000)]
        .into_iter()
        .map(|(id, domain, bits)| {
            BearerEndpoint::transparent(
                identity.ue,
                BearerId::new(id).unwrap(),
                Scope::User(domain),
                TransparentSettings {
                    uplink_segmentation: Segmentation::Disabled,
                    downlink_segmentation: Segmentation::Disabled,
                    errors: ErrorDelivery::Indicate,
                    limits: TmLimits {
                        maximum_pdus: count(1),
                        maximum_pdu_bits: count(bits),
                        maximum_sdu_bits: count(bits),
                    },
                },
            )
            .unwrap()
        })
        .collect();
    let configured = if mismatched_bank {
        configuration().configured().clone()
    } else {
        config.configured().clone()
    };
    let bank = DedicatedBearers::new(
        identity,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        0,
        configured,
        vec![
            TransportBinding {
                transport: id(1),
                channels: TransportChannels::Single(BearerId::new(10).unwrap()),
            },
            TransportBinding {
                transport: id(5),
                channels: TransportChannels::Single(BearerId::new(20).unwrap()),
            },
        ],
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(2),
            maximum_sdu_bytes: count(625),
            maximum_buffer_bytes: 1_000_000,
        },
    )
    .unwrap();
    Box::new(PreparedDedicated {
        receiver,
        bearers: Box::new(bank),
    })
}
fn protection(slot: u32) -> BearerCiphers {
    let key = identity(slot, 1).ue;
    let mut bank = BearerCiphers::new(key, count(2)).unwrap();
    for (bearer, domain) in [(10, CnDomain::Cs), (20, CnDomain::Ps)] {
        let anchor = CountAnchor::new(CipherMode::Transparent, Position::Frame(0), 0).unwrap();
        bank.register_with(
            BearerCipherConfiguration {
                bearer,
                scope: Scope::User(domain),
                transmit: anchor,
                receive: anchor,
                initial: Configuration::Unprotected,
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    }
    bank
}

#[test]
fn rejected_lane_cannot_starve_another_ue_and_release_preserves_the_retained_results() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 4, 2, 1.0));
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let _a = install(&mut port, owner(0, config.clone(), 20_096));
    let _b = install(&mut port, owner(1, config.clone(), 20_096));
    run(&mut runtime);
    run(&mut runtime);
    let mut peer = Vec::new();
    for (chunk, input) in samples.chunks(5120).enumerate() {
        runtime
            .feed(matched(first + (chunk * 5120) as u64, input, 1, false))
            .unwrap();
        run(&mut runtime);
        port.route_one(|delivery| {
            if delivery.decoded.identity.ue.slot == 0 {
                return false;
            }
            peer.push((delivery.decoded, delivery.payload.to_vec()));
            true
        });
    }
    for _ in 0..40 {
        run(&mut runtime);
        port.route_one(|delivery| {
            if delivery.decoded.identity.ue.slot == 0 {
                return false;
            }
            peer.push((delivery.decoded, delivery.payload.to_vec()));
            true
        });
    }
    assert_eq!(peer.len(), 6);
    for (decoded, bits) in peer {
        assert_eq!(bits, payload(&config, decoded.transport, 1, 8));
    }
    assert_eq!(port.reserved_output_bytes(), 10_000);
    let mut retained = Vec::new();
    for _ in 0..40 {
        run(&mut runtime);
        port.route_one(|delivery| {
            assert_eq!(delivery.decoded.identity, identity(0, 1));
            retained.push((delivery.decoded, delivery.payload.to_vec()));
            true
        });
    }
    assert_eq!(retained.len(), 6);
    for (decoded, bits) in retained {
        assert_eq!(bits, payload(&config, decoded.transport, 1, 7));
    }
    assert_eq!(runtime.counters().failed_owners, 0);
}

#[test]
fn closed_lane_keeps_its_cell_reservation_until_the_consumer_retires_it() {
    let config = configuration();
    let mut limits = runtime_limits();
    limits.maximum_output_bytes = 5000;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    let _a = install(&mut port, owner(0, config.clone(), 20_096));
    run(&mut runtime);
    assert_eq!(port.reserved_output_bytes(), 5000);
    assert!(port.install(owner(1, config.clone(), 20_096)).is_err());
    assert_eq!(
        port.retire_output(identity(0, 1)),
        Err(ReceiveError::Occupied)
    );
    let mut ticket = port.remove(identity(0, 1)).ok().unwrap();
    run(&mut runtime);
    assert!(matches!(
        ticket.try_result(),
        Some(ReceiveCommandResult::Removed {
            receiver: Some(_),
            ..
        })
    ));
    assert_eq!(port.reserved_output_bytes(), 5000);
    assert_eq!(
        port.retire_output(identity(0, 2)),
        Err(ReceiveError::Identity)
    );
    assert_eq!(port.reserved_output_bytes(), 5000);
    assert_eq!(port.retire_output(identity(0, 1)), Ok(0));
    assert_eq!(port.reserved_output_bytes(), 0);
    let mut b = install(&mut port, owner(1, config, 20_096));
    run(&mut runtime);
    assert!(matches!(
        b.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
}

#[test]
fn preparation_and_arming_are_separate_worker_acknowledged_steps() {
    let config = configuration();
    let (mut runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut router = router(port);
    router.stage(prepared(0, config, false)).unwrap();
    assert_eq!(
        router.phase(identity(0, 1)),
        Some(DedicatedPhase::Preparing)
    );
    assert_eq!(
        router.arm(identity(0, 1)),
        Err(DedicatedControlError::Phase)
    );
    run(&mut runtime);
    assert_eq!(
        router.phase(identity(0, 1)),
        Some(DedicatedPhase::Preparing)
    );
    router.poll_receipts();
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Prepared));
    assert_eq!(runtime.counters().sampled_slots, 0);
    router.arm(identity(0, 1)).unwrap();
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Arming));
    router.poll_receipts();
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Arming));
    run(&mut runtime);
    router.poll_receipts();
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Active));
    assert_eq!(router.counters().armed, 1);
}

#[test]
fn late_arm_fails_without_decoding_and_remove_ack_returns_both_owners() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let (mut runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut router = router(port);
    router.stage(prepared(0, config, false)).unwrap();
    run(&mut runtime);
    router.poll_receipts();
    runtime
        .feed(matched(first, &samples[..5120], 1, false))
        .unwrap();
    run(&mut runtime);
    assert_eq!(runtime.counters().sampled_slots, 0);
    router.arm(identity(0, 1)).unwrap();
    run(&mut runtime);
    router.poll_receipts();
    assert_eq!(
        router.phase(identity(0, 1)),
        Some(DedicatedPhase::ArmFailed)
    );
    assert_eq!(
        router.failure(identity(0, 1)),
        Some(&DedicatedControlError::Receive(ReceiveError::Time))
    );
    router.remove(identity(0, 1)).unwrap();
    assert!(matches!(
        router.take_retired(identity(0, 1)),
        Err(DedicatedControlError::Phase)
    ));
    run(&mut runtime);
    router.poll_receipts();
    let retired = router.take_retired(identity(0, 1)).ok().unwrap();
    assert!(retired.receiver.is_some());
    assert_eq!(
        retired.bank.phase(),
        crate::dedicated_bearers::BearerPhase::Retired
    );
    assert_eq!(router.reserved_output_bytes(), 0);
    assert_eq!(router.used_bearer_bytes(), 0);
}

#[test]
fn mismatched_tfcs_snapshot_is_rejected_before_publication_without_dropping_prepared_state() {
    let (_runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut router = router(port);
    let rejected = router
        .stage(prepared(0, configuration(), true))
        .unwrap_err();
    assert_eq!(rejected.error, DedicatedControlError::Configuration);
    assert_eq!(
        rejected.prepared.receiver.session().identity(),
        identity(0, 1)
    );
    assert_eq!(
        rejected.prepared.bearers.phase(),
        crate::dedicated_bearers::BearerPhase::Prepared
    );
    assert_eq!(router.reserved_output_bytes(), 0);
    assert_eq!(router.used_bearer_bytes(), 0);
    assert_eq!(router.phase(identity(0, 1)), None);
}

#[test]
fn failed_worker_admission_never_activates_the_bank_and_returns_reserved_owners() {
    let mut limits = runtime_limits();
    limits.shard.resources.physical_sample_bytes = 1;
    let (mut runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    let mut router = router(port);
    router.stage(prepared(0, configuration(), false)).unwrap();
    run(&mut runtime);
    router.poll_receipts();
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Rejected));
    assert_eq!(
        router.arm(identity(0, 1)),
        Err(DedicatedControlError::Phase)
    );
    let retired = router.take_retired(identity(0, 1)).ok().unwrap();
    assert!(retired.receiver.is_some());
    assert_eq!(router.reserved_output_bytes(), 0);
    assert_eq!(router.used_bearer_bytes(), 0);
}

#[test]
fn blocked_upper_consumer_does_not_prevent_peer_sample_to_bearer_progress() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 4, 2, 1.0));
    let (mut runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut router = router(port);
    router.stage(prepared(0, config.clone(), false)).unwrap();
    router.stage(prepared(1, config.clone(), false)).unwrap();
    run(&mut runtime);
    run(&mut runtime);
    router.poll_receipts();
    router.arm(identity(0, 1)).unwrap();
    router.arm(identity(1, 1)).unwrap();
    run(&mut runtime);
    run(&mut runtime);
    router.poll_receipts();
    let mut ciphers = [protection(0), protection(1)];
    let mut peer = Vec::new();
    for (chunk, input) in samples.chunks(5120).enumerate() {
        runtime
            .feed(matched(first + (chunk * 5120) as u64, input, 1, false))
            .unwrap();
        run(&mut runtime);
        let report = router
            .step(
                at(20),
                |key, bank| {
                    bank.service(
                        at(20),
                        &mut ciphers[key.slot as usize],
                        ResetPermission::Defer,
                    )
                },
                |delivery| {
                    if delivery.metadata.identity.ue.slot == 0 {
                        return false;
                    }
                    peer.push((
                        delivery.metadata,
                        (0..delivery.data.len())
                            .map(|i| Bit::from(delivery.data.bit(i).unwrap()))
                            .collect::<Vec<_>>(),
                    ));
                    true
                },
            )
            .unwrap();
        assert!(report.failure.is_none());
    }
    for _ in 0..100 {
        run(&mut runtime);
        router
            .step(
                at(20),
                |key, bank| {
                    bank.service(
                        at(20),
                        &mut ciphers[key.slot as usize],
                        ResetPermission::Defer,
                    )
                },
                |delivery| {
                    if delivery.metadata.identity.ue.slot == 0 {
                        return false;
                    }
                    peer.push((
                        delivery.metadata,
                        (0..delivery.data.len())
                            .map(|i| Bit::from(delivery.data.bit(i).unwrap()))
                            .collect::<Vec<_>>(),
                    ));
                    true
                },
            )
            .unwrap();
    }
    assert_eq!(peer.len(), 6);
    for (metadata, received) in peer {
        assert_eq!(
            received,
            payload(
                &config,
                if metadata.bearer.get() == 10 {
                    id(1)
                } else {
                    id(5)
                },
                1,
                8
            )
        );
    }
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Active));
    assert_eq!(router.phase(identity(1, 1)), Some(DedicatedPhase::Active));
    let mut retained = Vec::new();
    for _ in 0..100 {
        run(&mut runtime);
        router
            .step(
                at(20),
                |key, bank| {
                    bank.service(
                        at(20),
                        &mut ciphers[key.slot as usize],
                        ResetPermission::Defer,
                    )
                },
                |delivery| {
                    assert_eq!(delivery.metadata.identity, identity(0, 1));
                    retained.push((
                        delivery.metadata,
                        (0..delivery.data.len())
                            .map(|i| Bit::from(delivery.data.bit(i).unwrap()))
                            .collect::<Vec<_>>(),
                    ));
                    true
                },
            )
            .unwrap();
    }
    assert_eq!(retained.len(), 6);
    for (metadata, received) in retained {
        assert_eq!(
            received,
            payload(
                &config,
                if metadata.bearer.get() == 10 {
                    id(1)
                } else {
                    id(5)
                },
                1,
                7
            )
        );
    }
    assert_eq!(runtime.counters().failed_owners, 0);
}

#[test]
fn orphan_retirement_releases_only_the_matching_ue_after_worker_removal() {
    let config = configuration();
    let (mut runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut router = router(port);
    router.stage(prepared(0, config.clone(), false)).unwrap();
    router.stage(prepared(1, config, false)).unwrap();
    run(&mut runtime);
    run(&mut runtime);
    router.poll_receipts();
    router.arm(identity(0, 1)).unwrap();
    router.arm(identity(1, 1)).unwrap();
    run(&mut runtime);
    run(&mut runtime);
    router.poll_receipts();
    assert_eq!(router.retire_orphans_some(count(2), |key| key.slot == 1), 0);
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Removing));
    assert_eq!(router.reserved_output_bytes(), 10_000);
    run(&mut runtime);
    router.poll_receipts();
    assert_eq!(router.retire_orphans_some(count(2), |key| key.slot == 1), 1);
    assert_eq!(router.phase(identity(0, 1)), None);
    assert_eq!(router.phase(identity(1, 1)), Some(DedicatedPhase::Active));
    assert_eq!(router.reserved_output_bytes(), 5000);
}

#[test]
fn receive_shutdown_terminates_pending_controller_work_without_reporting_activation() {
    let (runtime, port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut router = router(port);
    router.stage(prepared(0, configuration(), false)).unwrap();
    drop(runtime);
    router.poll_receipts();
    assert_eq!(router.phase(identity(0, 1)), Some(DedicatedPhase::Closed));
    assert_eq!(router.counters().armed, 0);
    let retired = router.take_retired(identity(0, 1)).ok().unwrap();
    assert_eq!(
        retired.bank.phase(),
        crate::dedicated_bearers::BearerPhase::Retired
    );
    assert_eq!(router.reserved_output_bytes(), 0);
}
