// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_rx::{
        ChipReceiveSettings, DedicatedChipReceiver, DedicatedReceiveSession,
        DedicatedSampleReceiver, DedicatedSampleRuntime, ReceiveCommandTicket, ReceiveLimits,
        ReceiveResources, SampleReceiveSettings, SampleRuntimeLimits, ShardLimits, TfciAcceptance,
    },
    radio_io::MatchedSamples,
};
use num_complex::Complex32;
use umts_dsp::{
    carrier::CarrierOffsetHz,
    dedicated::{CarrierTrackingSettings, LikelihoodSettings, UplinkDpchCode},
    fractional_sampling::SamplePosition,
    pulse_shaping::SamplesPerChip,
    receive_filter::FilteredWindow,
    uplink_scrambling::UplinkScramblingCode,
};

mod atomic;
pub(in crate::signalling::control::tests::frames) mod release;

pub(in crate::signalling::control::tests::frames) fn receive_owner(
    identity: ReceiveIdentity,
    timing: MappingActivation,
    plan: &CompiledRel99,
) -> Box<DedicatedSampleReceiver> {
    let session = DedicatedReceiveSession::new(
        identity,
        timing.uplink,
        plan.uplink().clone(),
        ReceiveLimits {
            output_slots: count(8),
            maximum_output_bits: count(10_000),
            maximum_output_bytes: 100_000,
            maximum_coded_bits: count(200_000),
            maximum_frame_bits: count(100_000),
            maximum_decodes_per_step: count(8),
        },
    )
    .unwrap();
    let settings = ChipReceiveSettings {
        code: Arc::new(
            UplinkDpchCode::new(
                UplinkScramblingCode::new(plan.configuration().physical().uplink_scrambling)
                    .unwrap(),
            )
            .unwrap(),
        ),
        control: plan.uplink_control(),
        carrier: CarrierTrackingSettings {
            initial: CarrierOffsetHz::new(0.0).unwrap(),
            maximum_absolute_hz: 2000.0,
            update_fraction: 0.5,
            minimum_coherence: 0.8,
        },
        likelihood: LikelihoodSettings::new(128.0).unwrap(),
        tfci: TfciAcceptance {
            minimum_score: 0.5,
            minimum_margin: 0.1,
        },
        maximum_physical_bits: count(57_600),
    };
    let gains = plan
        .uplink_configured()
        .combinations()
        .iter()
        .map(|combination| {
            (
                combination.tfci,
                plan.uplink_gains(combination.tfci).unwrap(),
            )
        })
        .collect();
    let chips = DedicatedChipReceiver::new(session, settings, gains).unwrap();
    Box::new(
        DedicatedSampleReceiver::new(
            chips,
            SampleReceiveSettings {
                radio_link: Default::default(),
                rate: SamplesPerChip::Two,
                first_receive: SamplePosition::new(
                    SampleInstant {
                        epoch: identity.ue.epoch,
                        index: timing.uplink.time.index * 2 + 32,
                    },
                    0.0,
                )
                .unwrap(),
                search_radius_samples: 8,
                minimum_coherent_fraction: 0.05,
            },
        )
        .unwrap(),
    )
}
fn rx_result(
    runtime: &mut DedicatedSampleRuntime,
    ticket: &mut ReceiveCommandTicket,
) -> ReceiveCommandResult {
    for _ in 0..16 {
        runtime.service(|_, result| {
            result.unwrap();
        });
        if let Some(result) = ticket.try_result() {
            return result;
        }
    }
    panic!("bounded RX command did not complete");
}
pub(in crate::signalling::control::tests::frames) fn receive_runtime(
    epoch: umts_types::RadioEpoch,
) -> (
    DedicatedSampleRuntime,
    crate::dedicated_rx::DedicatedReceivePort,
) {
    DedicatedSampleRuntime::new(
        epoch,
        SamplesPerChip::Two,
        SampleRuntimeLimits {
            shard: ShardLimits {
                maximum_ues: count(1),
                maximum_sessions_per_step: count(2),
                resources: ReceiveResources {
                    physical_sample_bytes: 8_000_000,
                    coded_bits: 1_000_000,
                    output_bytes: 1_000_000,
                    demultiplexed_bits: 1_000_000,
                    channels: 16,
                },
            },
            history_samples: count(CHIPS_PER_FRAME as usize * 4),
            maximum_shared_bytes: 4_000_000,
            command_capacity: count(2),
            commands_per_step: count(2),
            output_slots_per_ue: count(8),
            maximum_output_bits: count(10_000),
            maximum_output_bytes: 200_000,
        },
    )
    .unwrap()
}
fn retire_receive(
    f: &Fixture,
    next: ReceiveIdentity,
    plan: &CompiledRel99,
) -> ReceiveCommandResult {
    let (mut runtime, mut port) = receive_runtime(f.key.epoch);
    let mut ticket = port
        .install(receive_owner(f.bank.identity(), activation(0), &f.plan))
        .ok()
        .unwrap();
    assert!(matches!(
        rx_result(&mut runtime, &mut ticket),
        ReceiveCommandResult::Installed(_)
    ));
    let mut ticket = port
        .stage_replacement(f.bank.identity(), receive_owner(next, activation(4), plan))
        .ok()
        .unwrap();
    assert!(matches!(
        rx_result(&mut runtime, &mut ticket),
        ReceiveCommandResult::ReplacementPrepared { .. }
    ));
    let mut ticket = port.commit_replacement(f.bank.identity(), next).unwrap();
    assert!(matches!(
        rx_result(&mut runtime, &mut ticket),
        ReceiveCommandResult::ReplacementCommitted { .. }
    ));
    let first = SampleInstant {
        epoch: f.key.epoch,
        index: 4 * CHIPS_PER_FRAME * 2 + 100,
    };
    runtime
        .feed(MatchedSamples {
            window: FilteredWindow {
                first_center: Some(first),
                next_input: SampleInstant {
                    index: first.index + 256,
                    ..first
                },
                sample_stride: 1,
            },
            samples: &[Complex32::default(); 256],
            generation: 2,
            discontinuity: true,
        })
        .unwrap();
    for _ in 0..8 {
        runtime.service(|_, result| {
            result.unwrap();
        });
    }
    let mut ticket = port.retire_drained(f.bank.identity()).unwrap();
    let retired = rx_result(&mut runtime, &mut ticket);
    assert!(
        matches!(&retired, ReceiveCommandResult::Retired { identity, .. } if *identity == f.bank.identity())
    );
    retired
}
fn prepare_empty(f: &mut Fixture, pool: &mut FramePool, frame: u64) {
    let draft = f
        .bank
        .begin_pooled_frame(
            pool,
            &f.ports.control,
            at(frame * CHIPS_PER_FRAME),
            Tfci::new(0).unwrap(),
        )
        .unwrap();
    assert!(draft.complete());
    f.ports
        .control
        .frame(draft.finish().ok().unwrap())
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    let TransmitCommandOutcome::Frame {
        input,
        result: Ok(_),
    } = f.ports.control.try_event().unwrap().outcome
    else {
        panic!("empty frame failed preparation");
    };
    pool.recycle(input).ok().unwrap();
}

#[test]
fn mapping_retirement_joins_real_rx_drain_and_tx_driver_tail_without_replacing_rlc() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let plan = remapped(&f);
    let next = next_identity(&f);
    let initial_bytes = f.bank.storage_bytes();
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), b"retained packet")
        .unwrap();
    let mut ledger = ResourceLedger::new(
        f.key.epoch,
        Resources {
            queue_bytes: 4_000_000,
            ..Resources::default()
        },
        8,
    );
    let limits = FramePoolLimits {
        payload_slots: count(1),
        receipt_slots: count(32),
        maximum_buffer_bytes: 2_000_000,
    };
    let mut old_pool =
        FramePool::new(old, f.plan.clone(), &f.ports.control, limits, &mut ledger).unwrap();
    for frame in 0..4 {
        prepare_empty(&mut f, &mut old_pool, frame);
    }
    f.ports.control.arm(old).unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Armed { result: Ok(()), .. }
    ));
    let candidate = DedicatedTransmitSession::new(
        next,
        activation(4).downlink,
        plan.clone(),
        umts_dsp::dedicated::DpchAmplitudes {
            data: 0.2,
            tpc: 0.2,
            tfci: 0.2,
            pilot: 0.2,
        },
        TransmitLimits {
            frame_slots: count(8),
            maximum_frame_bits: count(100_000),
            maximum_retained_bits: count(1_000_000),
        },
    )
    .unwrap();
    f.ports
        .control
        .stage_replacement(old, Box::new(candidate))
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::ReplacementPrepared { .. }
    ));
    stage(&mut f, 4, plan.clone());
    let mut new_pool =
        FramePool::new(next, plan.clone(), &f.ports.control, limits, &mut ledger).unwrap();
    for frame in 4..8 {
        prepare_empty(&mut f, &mut new_pool, frame);
    }
    f.ports.control.commit_replacement(old, next).unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::ReplacementCommitted { result: Ok(_), .. }
    ));
    f.bank.commit_mapping(next).unwrap();
    for (identity, frame) in [(old, 0), (next, 4)] {
        f.ports
            .feedback
            .publish(TpcPlan {
                identity,
                first_slot: at(frame * CHIPS_PER_FRAME),
                commands: vec![Bit::Zero; 60],
            })
            .ok()
            .unwrap();
        f.runtime.service().unwrap();
        f.ports.feedback.try_event().unwrap();
    }
    let receive = retire_receive(&f, next, &plan);
    let mut writer = f.source.submission_writer().unwrap();
    let mut samples = vec![Complex32::default(); f.source.samples_per_slot()];
    for _ in 0..60 {
        assert!(
            writer.accepted(
                f.runtime
                    .generate_slot(&mut f.source, &mut samples)
                    .unwrap()
            )
        );
    }
    f.ports.control.retire_submitted(old).unwrap();
    f.runtime.service().unwrap();
    let pending = f.ports.control.try_event().unwrap().outcome;
    assert!(matches!(
        &pending,
        TransmitCommandOutcome::Retired {
            result: Err(TransmitError::SubmissionPending),
            ..
        }
    ));
    let charged = f.bank.storage_bytes();
    assert_eq!(
        f.bank.retire_mapping(old, &receive, &pending),
        Err(BearerError::Phase)
    );
    assert_eq!(f.bank.storage_bytes(), charged);
    assert!(
        writer.accepted(
            f.runtime
                .generate_slot(&mut f.source, &mut samples)
                .unwrap()
        )
    );
    f.ports.control.retire_submitted(old).unwrap();
    f.runtime.service().unwrap();
    let retired = f.ports.control.try_event().unwrap().outcome;
    assert!(matches!(
        &retired,
        TransmitCommandOutcome::Retired { result: Ok(_), .. }
    ));
    f.bank.retire_mapping(old, &receive, &retired).unwrap();
    assert_eq!(f.bank.identity(), next);
    assert_eq!(f.bank.mapping_phase(next), Ok(MappingPhase::Active));
    assert!(!f.bank.contains_identity(old));
    assert_eq!(f.bank.storage_bytes(), initial_bytes);
    assert!(Arc::ptr_eq(f.bank.configured(), plan.uplink_configured()));
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            assert!(
                f.bank
                    .publish_sequenced(grant(next, 19, 20, 1, 8), at(0), ciphers, |_| Ok::<_, ()>(
                        ()
                    ))
                    .unwrap()
                    .is_some()
            );
        })
        .unwrap();
    let third = ReceiveIdentity {
        configuration: NonZeroU64::new(3).unwrap(),
        ..next
    };
    let prepared = f
        .bank
        .prepare_mapping(third, activation(8), f.plan.clone())
        .unwrap();
    f.bank.stage_mapping(prepared).unwrap();
    assert_eq!(
        f.bank.prepare_mapping(old, activation(8), plan).err(),
        Some(BearerError::Phase)
    );
}

#[test]
fn controller_charges_successor_maps_and_keeps_the_original_candidate_on_cell_pressure() {
    use crate::dedicated_controller::{
        DedicatedController, DedicatedControllerLimits, DedicatedPhase, PreparedDedicated,
    };
    for allow_second in [false, true] {
        let f = fixture_with_activation(true, false);
        let old = f.bank.identity();
        let next = next_identity(&f);
        let plan = remapped(&f);
        let initial_bytes = f.bank.storage_bytes();
        let (mut runtime, port) = receive_runtime(f.key.epoch);
        let mut controller = DedicatedController::new(
            port,
            DedicatedControllerLimits {
                receipt_visits: count(2),
                service_visits: count(2),
                maximum_bearer_bytes: initial_bytes * if allow_second { 2 } else { 1 },
            },
        );
        let receiver = receive_owner(old, activation(0), &f.plan);
        controller
            .stage(Box::new(PreparedDedicated {
                receiver,
                bearers: Box::new(f.bank),
            }))
            .unwrap();
        runtime.service(|_, result| {
            result.unwrap();
        });
        controller.poll_receipts();
        assert_eq!(controller.phase(old), Some(DedicatedPhase::Prepared));
        controller.arm(old).unwrap();
        runtime.service(|_, result| {
            result.unwrap();
        });
        controller.poll_receipts();
        assert_eq!(controller.phase(old), Some(DedicatedPhase::Active));
        let prepared = controller
            .prepare_mapping(old, next, activation(4), plan)
            .unwrap();
        let bytes = prepared.storage_bytes().unwrap();
        if allow_second {
            controller.stage_mapping(prepared).unwrap();
            assert_eq!(controller.used_bearer_bytes(), initial_bytes + bytes);
            controller.commit_mapping(next).unwrap();
            assert!(
                controller
                    .retire_mapping(
                        old,
                        &ReceiveCommandResult::Closed,
                        &TransmitCommandOutcome::Closed
                    )
                    .is_err()
            );
            assert_eq!(controller.used_bearer_bytes(), initial_bytes + bytes);
        } else {
            let rejected = controller.stage_mapping(prepared).unwrap_err();
            assert_eq!(rejected.error, BearerError::Capacity);
            assert_eq!(rejected.prepared.identity(), next);
            assert_eq!(rejected.prepared.storage_bytes().unwrap(), bytes);
            assert_eq!(controller.used_bearer_bytes(), initial_bytes);
        }
    }
}
