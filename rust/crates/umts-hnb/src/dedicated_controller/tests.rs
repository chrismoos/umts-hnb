// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::MappingPhase,
    dedicated_rx::{
        ChipReceiveSettings, DedicatedChipReceiver, DedicatedReceiveSession,
        DedicatedSampleRuntime, FrameAnchor, ReceiveResources, SampleRuntimeLimits, ShardLimits,
    },
    rel99_plan::CompiledRel99,
    rel99_planner::tests::admission as owners,
    test_support::{at, count},
};
use std::num::NonZeroU64;
use umts_dsp::{
    dedicated::UplinkDpchCode, pulse_shaping::SamplesPerChip,
    uplink_scrambling::UplinkScramblingCode,
};
use umts_types::time::CHIPS_PER_FRAME;

const OUTPUT_SLOTS: usize = 8;
const OUTPUT_BITS: usize = 10_000;
const CONFIGURED_PAYLOAD_BITS: usize = 320;
const LIVE_CONFIGURATIONS: usize = 3;

fn timing(frame: u64) -> MappingActivation {
    let anchor = FrameAnchor {
        time: at(frame * CHIPS_PER_FRAME),
        cfn: frame as u8,
    };
    MappingActivation {
        uplink: anchor,
        downlink: anchor,
        uplink_frame: frame,
        downlink_frame: frame,
    }
}

fn receiver(
    identity: ReceiveIdentity,
    frame: u64,
    plan: &CompiledRel99,
) -> Box<DedicatedSampleReceiver> {
    let settings = owners::settings(identity.ue.epoch);
    let session = DedicatedReceiveSession::new(
        identity,
        timing(frame).uplink,
        plan.uplink().clone(),
        settings.receive,
    )
    .unwrap();
    let gains = plan
        .uplink_configured()
        .combinations()
        .iter()
        .map(|row| (row.tfci, plan.uplink_gains(row.tfci).unwrap()))
        .collect();
    let chips = DedicatedChipReceiver::new(
        session,
        ChipReceiveSettings {
            code: Arc::new(
                UplinkDpchCode::new(
                    UplinkScramblingCode::new(plan.configuration().physical().uplink_scrambling)
                        .unwrap(),
                )
                .unwrap(),
            ),
            control: plan.uplink_control(),
            carrier: settings.carrier,
            likelihood: settings.likelihood,
            tfci: settings.tfci,
            maximum_physical_bits: settings.maximum_physical_bits,
        },
        gains,
    )
    .unwrap();
    let mut samples = settings.samples;
    samples.first_receive = umts_dsp::fractional_sampling::SamplePosition::new(
        umts_types::time::SampleInstant {
            epoch: identity.ue.epoch,
            index: frame * CHIPS_PER_FRAME * samples.rate.get() as u64 + 32,
        },
        0.0,
    )
    .unwrap();
    Box::new(DedicatedSampleReceiver::new(chips, samples).unwrap())
}

fn fixture() -> (
    crate::dedicated_rx::DedicatedSampleRuntime,
    DedicatedController,
    ReceiveIdentity,
    Arc<CompiledRel99>,
) {
    let (_source, radio, _resources, prepared) = owners::prepared();
    let key = prepared.allocation.key();
    let identity = ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::MIN,
    };
    let plan = prepared.allocation.plan().clone();
    let mut endpoints = radio.signalling_endpoints(key).unwrap();
    endpoints.extend(prepared.profiles.placeholders(key));
    let bank = DedicatedBearers::from_compiled(
        identity,
        timing(0),
        plan.clone(),
        endpoints,
        owners::settings(key.epoch).bearers,
    )
    .ok()
    .unwrap();
    let (mut runtime, port) = DedicatedSampleRuntime::new(
        key.epoch,
        SamplesPerChip::Two,
        SampleRuntimeLimits {
            shard: ShardLimits {
                maximum_ues: count(2),
                maximum_sessions_per_step: count(2),
                resources: ReceiveResources {
                    physical_sample_bytes: 8_000_000,
                    coded_bits: 1_000_000,
                    output_bytes: 1_000_000,
                    demultiplexed_bits: 1_000_000,
                    channels: 32,
                },
            },
            history_samples: count(CHIPS_PER_FRAME as usize * 4),
            maximum_shared_bytes: 4_000_000,
            command_capacity: count(2),
            commands_per_step: count(2),
            output_slots_per_ue: count(OUTPUT_SLOTS),
            maximum_output_bits: count(OUTPUT_BITS),
            maximum_output_bytes: LIVE_CONFIGURATIONS * OUTPUT_SLOTS * OUTPUT_BITS,
        },
    )
    .unwrap();
    let mut controller = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: 4_000_000,
        },
    );
    controller
        .stage(Box::new(PreparedDedicated {
            receiver: receiver(identity, 0, &plan),
            bearers: Box::new(bank),
        }))
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    controller.poll_receipts();
    controller.arm(identity).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    controller.poll_receipts();
    assert_eq!(controller.phase(identity), Some(DedicatedPhase::Active));
    (runtime, controller, identity, plan)
}

#[test]
fn receive_diagnostics_are_single_flight_and_return_exact_active_identity() {
    let (mut runtime, mut controller, identity, _) = fixture();
    for _ in 0..4 {
        assert_eq!(
            controller.service_receive_diagnostics(|_, _| panic!("not serviced")),
            0
        );
    }
    let mut other = controller.port.observe(identity).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(matches!(
        other.try_result(),
        Some(ReceiveCommandResult::Observed { .. })
    ));
    let mut seen = None;
    assert_eq!(
        controller.service_receive_diagnostics(|id, status| {
            seen = Some(id);
            assert_eq!(status.phase, crate::dedicated_rx::ReceivePlanPhase::Active);
        }),
        1
    );
    assert_eq!(seen, Some(identity));
}

#[test]
fn late_diagnostic_reply_does_not_report_a_retired_owner_as_active() {
    let (mut runtime, mut controller, identity, _) = fixture();
    controller.service_receive_diagnostics(|_, _| panic!("not serviced"));
    controller.remove(identity).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    controller.poll_receipts();
    assert_ne!(controller.phase(identity), Some(DedicatedPhase::Active));
    assert_eq!(
        controller.service_receive_diagnostics(|_, _| panic!("retired owner")),
        0
    );
    assert!(controller.observation.is_none());
}

#[test]
fn exact_staged_receiver_removal_returns_owner_without_retiring_bank_or_predecessor() {
    let (mut runtime, mut controller, old, plan) = fixture();
    let peer = ReceiveIdentity {
        ue: UeKey {
            slot: old.ue.slot + 1,
            ..old.ue
        },
        ..old
    };
    let mut peer_install = controller
        .port
        .install(receiver(peer, 0, &plan))
        .ok()
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(
        matches!(peer_install.try_result(), Some(ReceiveCommandResult::Installed(id)) if id == peer)
    );
    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..old
    };
    let prepared = controller
        .prepare_mapping(old, next, timing(4), plan.clone())
        .unwrap();
    controller.stage_mapping(prepared).unwrap();
    let mut install = controller
        .stage_mapping_receiver(old, next, receiver(next, 4, &plan))
        .ok()
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(matches!(
        install.try_result(),
        Some(ReceiveCommandResult::ReplacementPrepared { .. })
    ));
    assert_eq!(
        controller.reserved_output_bytes(),
        LIVE_CONFIGURATIONS * OUTPUT_SLOTS * CONFIGURED_PAYLOAD_BITS
    );
    let charged = controller.used_bearer_bytes();
    let mut removed = controller.remove_mapping_receiver(old, next).unwrap();
    assert!(removed.try_result().is_none());
    runtime.service(|_, result| {
        result.unwrap();
    });
    let result = removed.try_result().unwrap();
    assert!(
        matches!(&result, ReceiveCommandResult::Removed { identity, receiver: Some(owner) }
        if *identity == next && owner.session().identity() == next)
    );
    assert_eq!(
        controller
            .retire_removed_mapping_output(old, &result)
            .unwrap(),
        0
    );
    assert_eq!(controller.phase(old), Some(DedicatedPhase::Active));
    assert_eq!(controller.used_bearer_bytes(), charged);
    let mut predecessor = controller.port.observe(old).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(matches!(
        predecessor.try_result(),
        Some(ReceiveCommandResult::Observed { .. })
    ));
    let mut peer_observe = controller.port.observe(peer).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(
        matches!(peer_observe.try_result(), Some(ReceiveCommandResult::Observed { identity, status: Some(_) }) if identity == peer)
    );
}

#[test]
fn scheduled_pair_removes_both_exact_owners_and_rejects_foreign_or_conflicting_operations() {
    let (mut runtime, mut controller, old, plan) = fixture();
    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..old
    };
    let prepared = controller
        .prepare_mapping(old, next, timing(4), plan.clone())
        .unwrap();
    controller.stage_mapping(prepared).unwrap();
    let mut install = controller
        .stage_mapping_receiver(old, next, receiver(next, 4, &plan))
        .ok()
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(install.try_result().is_some());
    let mut commit = controller.commit_mapping_receiver(old, next).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(matches!(
        commit.try_result(),
        Some(ReceiveCommandResult::ReplacementCommitted { .. })
    ));
    controller.commit_mapping(next).unwrap();
    let foreign = ReceiveIdentity {
        ue: UeKey {
            slot: old.ue.slot + 1,
            ..old.ue
        },
        ..next
    };
    assert!(controller.remove_mapping_receiver(old, foreign).is_err());
    let stale = ReceiveIdentity {
        configuration: NonZeroU64::new(3).unwrap(),
        ..old
    };
    assert!(controller.remove_mapping_receiver(old, stale).is_err());
    controller.entries[old.ue.slot as usize]
        .as_mut()
        .unwrap()
        .phase = DedicatedPhase::Faulted;
    let charged = controller.used_bearer_bytes();
    let mut first = controller.remove_mapping_receiver(old, old).unwrap();
    let mut second = controller.remove_mapping_receiver(old, next).unwrap();
    assert!(matches!(
        controller.remove_mapping_receiver(old, old),
        Err(DedicatedControlError::Capacity)
    ));
    runtime.service(|_, result| {
        result.unwrap();
    });
    let first = first.try_result().unwrap();
    let second = second.try_result().unwrap();
    assert_eq!(
        controller.retire_orphans_some(count(2), |key| key == old.ue),
        0
    );
    assert_eq!(controller.phase(old), Some(DedicatedPhase::Faulted));
    assert!(matches!(
        &first,
        ReceiveCommandResult::Removed {
            receiver: Some(_),
            ..
        }
    ));
    assert!(matches!(
        &second,
        ReceiveCommandResult::Removed {
            receiver: Some(_),
            ..
        }
    ));
    assert_eq!(controller.used_bearer_bytes(), charged);
    controller.with_active_mapping(old, |_| ()).unwrap_err();
    let bank = &controller.entries[old.ue.slot as usize]
        .as_ref()
        .unwrap()
        .bank;
    assert!(
        matches!(bank.mapping_phase(old).unwrap(), MappingPhase::Draining { next: id, .. } if id == next)
    );
    assert_eq!(
        bank.mapping_phase(next).unwrap(),
        MappingPhase::Scheduled { previous: old }
    );
    let returned = controller
        .take_removed_mapping_pair(old, &first, &second)
        .unwrap();
    assert!(returned.receiver.is_none());
    assert_eq!(returned.bank.identity(), old);
    assert_eq!(controller.used_bearer_bytes(), 0);
    assert!(
        controller
            .take_removed_mapping_pair(old, &first, &second)
            .is_err()
    );
}

#[test]
fn pending_operation_and_closed_runtime_reject_removal_without_mutating_bank() {
    let (_runtime, mut controller, old, _) = fixture();
    controller.remove(old).unwrap();
    assert!(matches!(
        controller.remove_mapping_receiver(old, old),
        Err(DedicatedControlError::Phase)
    ));
    let (runtime, mut controller, old, _) = fixture();
    let charged = controller.used_bearer_bytes();
    drop(runtime);
    assert!(matches!(
        controller.remove_mapping_receiver(old, old),
        Err(DedicatedControlError::Closed)
    ));
    assert_eq!(controller.used_bearer_bytes(), charged);
}

#[test]
fn second_lane_failure_keeps_exact_partial_progress_without_double_refund_on_retry() {
    let (mut runtime, mut controller, old, plan) = fixture();
    let next = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..old
    };
    let prepared = controller
        .prepare_mapping(old, next, timing(4), plan.clone())
        .unwrap();
    controller.stage_mapping(prepared).unwrap();
    let mut install = controller
        .stage_mapping_receiver(old, next, receiver(next, 4, &plan))
        .ok()
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(install.try_result().is_some());
    let mut first = controller.remove_mapping_receiver(old, old).unwrap();
    let mut second = controller.remove_mapping_receiver(old, next).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    let first = first.try_result().unwrap();
    let second = second.try_result().unwrap();
    let charged = controller.used_bearer_bytes();
    controller.port.retire_output(next).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            controller.take_removed_mapping_pair(old, &first, &second),
            Err(DedicatedControlError::Receive(ReceiveError::Identity))
        ));
        assert_eq!(controller.used_bearer_bytes(), charged);
        let progress = controller.entries[old.ue.slot as usize]
            .as_ref()
            .unwrap()
            .mapping_removal
            .as_ref()
            .unwrap();
        assert_eq!((progress.previous, progress.successor), (old, next));
        assert_eq!(progress.retired, [true, false]);
        assert!(matches!(
            controller.remove(old),
            Err(DedicatedControlError::Phase)
        ));
    }
    let foreign = ReceiveIdentity {
        ue: UeKey {
            generation: NonZeroU64::new(old.ue.generation.get() + 1).unwrap(),
            ..old.ue
        },
        ..old
    };
    assert!(
        controller
            .take_removed_mapping_pair(foreign, &first, &second)
            .is_err()
    );
    assert_eq!(controller.used_bearer_bytes(), charged);
}
