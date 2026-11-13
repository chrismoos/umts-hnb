// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::signalling::{AdmissionError, ResourceLedger, Resources};
use crate::{
    dedicated_bearers::{FramePool, FramePoolError, FramePoolLimits},
    dedicated_tx::FrameInput,
};

fn ledger(f: &Fixture, bytes: u64) -> ResourceLedger {
    ResourceLedger::new(
        f.key.epoch,
        Resources {
            queue_bytes: bytes,
            ..Resources::default()
        },
        8,
    )
}
fn limits(payloads: usize) -> FramePoolLimits {
    FramePoolLimits {
        payload_slots: count(payloads),
        receipt_slots: count(32),
        maximum_buffer_bytes: 2_000_000,
    }
}
fn pool(f: &Fixture, ledger: &mut ResourceLedger, payloads: usize) -> FramePool {
    FramePool::new(
        f.bank.identity(),
        f.plan.clone(),
        &f.ports.control,
        limits(payloads),
        ledger,
    )
    .unwrap()
}
fn prepared(
    f: &mut Fixture,
    pool: &mut FramePool,
    frame: u64,
) -> crate::tx_evidence::TransportSubmission {
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
    let receipt = draft.submission();
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
        panic!("prepared frame");
    };
    pool.recycle(input).ok().unwrap();
    receipt
}
fn remove(f: &mut Fixture, pool: &mut FramePool) {
    f.ports.control.remove(f.bank.identity()).unwrap();
    f.runtime.service().unwrap();
    let TransmitCommandOutcome::Removed {
        result: Ok(mut session),
        ..
    } = f.ports.control.try_event().unwrap().outcome
    else {
        panic!("removed session");
    };
    pool.reclaim_retired(&mut session).unwrap();
}
fn fingerprint(input: &FrameInput) -> Vec<(u8, usize, usize)> {
    let mut values: Vec<_> = input
        .transports
        .iter()
        .chain(&input.storage.spare)
        .map(|transport| {
            (
                transport.transport.get(),
                transport.bits.as_ptr() as usize,
                transport.bits.capacity(),
            )
        })
        .collect();
    values.sort_by_key(|entry| entry.0);
    values
}

#[test]
fn frame_storage_and_receipt_nodes_reuse_the_same_allocations_across_streaming_frames() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    assert_eq!(ledger.used().queue_bytes, pool.storage_bytes() as u64);
    let mut writer = f.source.submission_writer().unwrap();
    let mut buffers = None;
    let mut storage = None;
    let mut nodes = std::collections::BTreeSet::new();
    let mut samples = vec![num_complex::Complex32::default(); f.source.samples_per_slot()];
    for frame in 0..272 {
        let draft = f
            .bank
            .begin_pooled_frame(
                &mut pool,
                &f.ports.control,
                at(frame * CHIPS_PER_FRAME),
                Tfci::new(0).unwrap(),
            )
            .unwrap();
        let input = draft.finish().ok().unwrap();
        let current = fingerprint(&input);
        assert_eq!(buffers.get_or_insert_with(|| current.clone()), &current);
        let address = &*input.storage as *const _ as usize;
        assert_eq!(*storage.get_or_insert(address), address);
        nodes.insert(input.emission.as_ref().unwrap().allocation_id());
        f.ports.control.frame(input).ok().unwrap();
        f.runtime.service().unwrap();
        let TransmitCommandOutcome::Frame {
            input,
            result: Ok(_),
        } = f.ports.control.try_event().unwrap().outcome
        else {
            panic!("prepared");
        };
        pool.recycle(input).ok().unwrap();
        if frame == 0 {
            f.ports.control.arm(f.bank.identity()).unwrap();
            f.runtime.service().unwrap();
            f.ports.control.try_event().unwrap();
        }
        feedback(&mut f, frame * 15, 15);
        for _ in 0..15 {
            let window = f
                .runtime
                .generate_slot(&mut f.source, &mut samples)
                .unwrap();
            assert!(writer.accepted(window));
        }
    }
    assert_eq!(nodes.len(), 3);
    assert_eq!(pool.available_payloads(), 1);
    remove(&mut f, &mut pool);
    assert_eq!(pool.available_receipts(), 32);
    pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn retained_receipts_apply_backpressure_without_resetting_old_submission_proof() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    let mut writer = f.source.submission_writer().unwrap();
    let mut receipts = Vec::new();
    let mut samples = vec![num_complex::Complex32::default(); f.source.samples_per_slot()];
    for frame in 0..32 {
        receipts.push(prepared(&mut f, &mut pool, frame));
        if frame == 0 {
            f.ports.control.arm(f.bank.identity()).unwrap();
            f.runtime.service().unwrap();
            f.ports.control.try_event().unwrap();
        }
        feedback(&mut f, frame * 15, 15);
        for _ in 0..15 {
            let window = f
                .runtime
                .generate_slot(&mut f.source, &mut samples)
                .unwrap();
            assert!(writer.accepted(window));
        }
    }
    assert_eq!(pool.available_receipts(), 0);
    assert!(receipts[0].submitted());
    assert!(matches!(
        f.bank.begin_pooled_frame(
            &mut pool,
            &f.ports.control,
            at(32 * CHIPS_PER_FRAME),
            Tfci::new(6).unwrap()
        ),
        Err(FrameBuildError::Pool(FramePoolError::Capacity))
    ));
    assert_eq!(
        f.radio
            .registry
            .get(f.key)
            .unwrap()
            .control
            .published_status(f.key)
            .unwrap(),
        None
    );
    let first = receipts.remove(0);
    drop(receipts);
    let mut draft = f
        .bank
        .begin_pooled_frame(
            &mut pool,
            &f.ports.control,
            at(32 * CHIPS_PER_FRAME),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    assert!(
        draft
            .publish_signalling(
                grant(draft.identity(), 10, 2, 1, 32),
                at(32 * CHIPS_PER_FRAME),
                &mut f.radio
            )
            .unwrap()
    );
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
        panic!("prepared");
    };
    pool.recycle(input).ok().unwrap();
    assert!(first.submitted());
    remove(&mut f, &mut pool);
    let rejection = pool.retire(&mut ledger).unwrap_err();
    assert_eq!(rejection.error, FramePoolError::Pending);
    drop(first);
    drop(f.radio);
    rejection.pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn payload_credit_exhaustion_and_untouched_draft_drop_preserve_the_original_credit() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    let draft = f
        .bank
        .begin_pooled_frame(&mut pool, &f.ports.control, at(0), Tfci::new(0).unwrap())
        .unwrap();
    let input = draft.finish().ok().unwrap();
    assert_eq!(pool.available_payloads(), 0);
    assert!(matches!(
        f.bank.begin_pooled_frame(
            &mut pool,
            &f.ports.control,
            at(CHIPS_PER_FRAME),
            Tfci::new(0).unwrap()
        ),
        Err(FrameBuildError::Pool(FramePoolError::Capacity))
    ));
    pool.recycle(input).ok().unwrap();
    assert_eq!(pool.available_payloads(), 1);
    let draft = f
        .bank
        .begin_pooled_frame(
            &mut pool,
            &f.ports.control,
            at(CHIPS_PER_FRAME),
            Tfci::new(0).unwrap(),
        )
        .unwrap();
    let receipt = draft.submission();
    drop(draft);
    assert!(!receipt.viable());
    assert_eq!(pool.available_payloads(), 1);
    drop(receipt);
    pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn discarded_partially_admitted_pooled_frame_requires_recovery_and_preserves_pinned_proof() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    let mut draft = f
        .bank
        .begin_pooled_frame(&mut pool, &f.ports.control, at(0), Tfci::new(6).unwrap())
        .unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    drop(draft);
    assert_eq!(pool.available_payloads(), 1);
    assert_eq!(pool.available_receipts(), 31);
    assert!(matches!(
        f.bank
            .begin_pooled_frame(&mut pool, &f.ports.control, at(0), Tfci::new(6).unwrap()),
        Err(FrameBuildError::Bearer(BearerError::RecoveryRequired))
    ));
    let rejection = pool.retire(&mut ledger).unwrap_err();
    drop(f.radio);
    rejection.pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn pool_memory_leases_and_recycled_frames_cannot_cross_identical_looking_owners() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut first = pool(&f, &mut ledger, 1);
    let mut second = pool(&f, &mut ledger, 1);
    let charged = ledger.used();
    let draft = f
        .bank
        .begin_pooled_frame(&mut first, &f.ports.control, at(0), Tfci::new(0).unwrap())
        .unwrap();
    let input = draft.finish().ok().unwrap();
    let input = second.recycle(input).err().unwrap();
    first.recycle(input).ok().unwrap();
    assert_eq!(ledger.used(), charged);
    let mut other = ResourceLedger::new(
        f.key.epoch,
        Resources {
            queue_bytes: 4_000_000,
            ..Resources::default()
        },
        8,
    );
    let rejected = first.retire(&mut other).unwrap_err();
    assert_eq!(
        rejected.error,
        FramePoolError::Admission(AdmissionError::InvalidLease)
    );
    rejected.pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used().queue_bytes, second.storage_bytes() as u64);
    second.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn undersized_or_globally_unfunded_pools_do_not_consume_resource_leases() {
    let f = fixture(false);
    let mut small = ledger(&f, 1);
    assert!(matches!(
        FramePool::new(
            f.bank.identity(),
            f.plan.clone(),
            &f.ports.control,
            limits(1),
            &mut small
        ),
        Err(FramePoolError::Admission(AdmissionError::Capacity))
    ));
    assert_eq!(small.used(), Resources::default());
    let mut funded = ledger(&f, 4_000_000);
    for limits in [
        FramePoolLimits {
            maximum_buffer_bytes: 1,
            ..limits(1)
        },
        FramePoolLimits {
            receipt_slots: count(1),
            ..limits(1)
        },
        FramePoolLimits {
            payload_slots: count(17),
            ..limits(1)
        },
    ] {
        assert!(matches!(
            FramePool::new(
                f.bank.identity(),
                f.plan.clone(),
                &f.ports.control,
                limits,
                &mut funded
            ),
            Err(FramePoolError::Capacity)
        ));
        assert_eq!(funded.used(), Resources::default());
    }
}

#[test]
fn worker_rejects_pool_replacement_before_mutating_the_active_session() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut first = pool(&f, &mut ledger, 1);
    let mut second = pool(&f, &mut ledger, 1);
    drop(prepared(&mut f, &mut first, 0));
    let draft = f
        .bank
        .begin_pooled_frame(
            &mut second,
            &f.ports.control,
            at(CHIPS_PER_FRAME),
            Tfci::new(0).unwrap(),
        )
        .unwrap();
    let receipt = draft.submission();
    f.ports
        .control
        .frame(draft.finish().ok().unwrap())
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    let TransmitCommandOutcome::Frame {
        input,
        result: Err(TransmitError::Identity),
    } = f.ports.control.try_event().unwrap().outcome
    else {
        panic!("pool mismatch");
    };
    assert_eq!(
        f.runtime
            .bank()
            .session(f.bank.identity())
            .unwrap()
            .next_prepare(),
        at(CHIPS_PER_FRAME)
    );
    second.recycle(input).ok().unwrap();
    assert!(!receipt.viable());
    drop(receipt);
    remove(&mut f, &mut first);
    first.retire(&mut ledger).unwrap();
    second.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn pooled_combined_voice_packet_and_signalling_preserve_buffers_through_continuation() {
    let mut f = fixture(true);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), &[0x51; 200])
        .unwrap();
    let mut draft = f
        .bank
        .begin_pooled_frame(&mut pool, &f.ports.control, at(0), Tfci::new(23).unwrap())
        .unwrap();
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            for (transport, bearer, format, width) in
                [(6, 10, 5, 81), (7, 11, 1, 103), (8, 12, 1, 60)]
            {
                draft
                    .publish_transparent(
                        grant(draft.identity(), transport, bearer, format, 0),
                        &[Bits::new(&[0x55; 13], width).unwrap()],
                        ciphers,
                    )
                    .unwrap();
            }
            for _ in 0..4 {
                assert!(
                    draft
                        .publish_sequenced(grant(draft.identity(), 9, 20, 1, 0), at(0), ciphers)
                        .unwrap()
                );
            }
        })
        .unwrap();
    assert!(
        draft
            .publish_signalling(grant(draft.identity(), 10, 2, 1, 0), at(0), &mut f.radio)
            .unwrap()
    );
    let input = draft.finish().ok().unwrap();
    let original = fingerprint(&input);
    f.ports.control.frame(input).ok().unwrap();
    f.runtime.service().unwrap();
    let TransmitCommandOutcome::Frame {
        input,
        result: Ok(_),
    } = f.ports.control.try_event().unwrap().outcome
    else {
        panic!("combined frame");
    };
    pool.recycle(input).ok().unwrap();
    let draft = f
        .bank
        .begin_pooled_frame(
            &mut pool,
            &f.ports.control,
            at(CHIPS_PER_FRAME),
            Tfci::new(23).unwrap(),
        )
        .unwrap();
    assert!(draft.complete());
    let input = draft.finish().ok().unwrap();
    assert_eq!(fingerprint(&input), original);
    f.ports.control.frame(input).ok().unwrap();
    f.runtime.service().unwrap();
    let TransmitCommandOutcome::Frame {
        input,
        result: Ok(_),
    } = f.ports.control.try_event().unwrap().outcome
    else {
        panic!("continuation frame");
    };
    pool.recycle(input).ok().unwrap();
    remove(&mut f, &mut pool);
    drop(f.radio);
    pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn pools_use_the_cell_ledger_and_remain_charged_after_connection_retirement() {
    let mut f = fixture(false);
    let base = f.radio.resources();
    let identity = f.bank.identity();
    let first = f
        .radio
        .allocate_frame_pool(identity, f.plan.clone(), &f.ports.control, limits(1))
        .unwrap();
    let first_bytes = first.storage_bytes() as u64;
    assert_eq!(
        f.radio.resources().queue_bytes,
        base.queue_bytes + first_bytes
    );
    assert!(matches!(
        f.radio
            .allocate_frame_pool(identity, f.plan.clone(), &f.ports.control, limits(1)),
        Err(SignallingError::Phase)
    ));
    let second_identity = ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ..identity
    };
    let second = f
        .radio
        .allocate_frame_pool(second_identity, f.plan.clone(), &f.ports.control, limits(1))
        .unwrap();
    let second_bytes = second.storage_bytes() as u64;
    let charged = f.radio.resources();
    assert!(matches!(
        f.radio.allocate_frame_pool(
            ReceiveIdentity {
                configuration: NonZeroU64::new(3).unwrap(),
                ..identity
            },
            f.plan.clone(),
            &f.ports.control,
            limits(1)
        ),
        Err(SignallingError::Capacity)
    ));
    assert_eq!(f.radio.resources(), charged);
    f.radio.retire_frame_pool(first).unwrap();
    assert_eq!(
        f.radio.resources().queue_bytes,
        base.queue_bytes + second_bytes
    );
    let first = f
        .radio
        .allocate_frame_pool(identity, f.plan.clone(), &f.ports.control, limits(1))
        .unwrap();
    f.radio.retire(f.key).unwrap();
    assert_eq!(
        f.radio.resources().queue_bytes,
        f.radio.shared_bytes + first_bytes + second_bytes
    );
    assert!(matches!(
        f.radio
            .allocate_frame_pool(identity, f.plan.clone(), &f.ports.control, limits(1)),
        Err(SignallingError::Origin)
    ));
    f.radio.retire_frame_pool(first).unwrap();
    f.radio.retire_frame_pool(second).unwrap();
    assert_eq!(f.radio.resources().queue_bytes, f.radio.shared_bytes);
}

#[test]
fn same_driver_progress_does_not_authorize_another_runtime_or_its_pool() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    let (_other_runtime, other_ports) = DedicatedTransmitRuntime::new(
        &f.source,
        count(1),
        count(4),
        count(2_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(2),
            commands_per_step: count(2),
            feedback_capacity: count(1),
            feedback_per_step: count(1),
            maximum_frame_payload_bits: count(10_000),
            maximum_inflight_retained_bits: count(2_000_000),
        },
    )
    .unwrap();
    assert!(matches!(
        f.bank.begin_pooled_frame(
            &mut pool,
            &other_ports.control,
            at(0),
            Tfci::new(0).unwrap()
        ),
        Err(FrameBuildError::Pool(FramePoolError::Transmit(
            TransmitError::Identity
        )))
    ));
    assert_eq!(pool.available_payloads(), 1);
    assert_eq!(pool.available_receipts(), 32);
    let draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &other_ports.control,
            at(0),
            Tfci::new(0).unwrap(),
        )
        .unwrap();
    let receipt = draft.submission();
    f.ports
        .control
        .frame(draft.finish().ok().unwrap())
        .ok()
        .unwrap();
    f.runtime.service().unwrap();
    assert!(matches!(
        f.ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Frame {
            result: Err(TransmitError::Identity),
            ..
        }
    ));
    assert!(!receipt.viable());
    assert_eq!(
        f.runtime
            .bank()
            .session(f.bank.identity())
            .unwrap()
            .next_prepare(),
        at(0)
    );
    pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn outstanding_payload_keeps_its_memory_lease_until_the_exact_input_returns() {
    let mut f = fixture(false);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    let draft = f
        .bank
        .begin_pooled_frame(&mut pool, &f.ports.control, at(0), Tfci::new(0).unwrap())
        .unwrap();
    let input = draft.finish().ok().unwrap();
    let charged = ledger.used();
    let mut rejection = pool.retire(&mut ledger).unwrap_err();
    assert_eq!(rejection.error, FramePoolError::Pending);
    assert_eq!(ledger.used(), charged);
    rejection.pool.recycle(input).ok().unwrap();
    rejection.pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}

#[test]
fn zero_payload_with_coded_crc_still_requires_the_entire_transport_interval() {
    let mut f = fixture(false);
    let block = f
        .plan
        .downlink_configured()
        .block_plan(TransportId::new(6).unwrap(), 0)
        .unwrap();
    assert_eq!(block.payload_bits(), 0);
    assert!(block.coded_bits() > 0);
    let mut ledger = ledger(&f, 4_000_000);
    let mut pool = pool(&f, &mut ledger, 1);
    let mut writer = f.source.submission_writer().unwrap();
    let receipt = prepared(&mut f, &mut pool, 0);
    drop(prepared(&mut f, &mut pool, 1));
    f.ports.control.arm(f.bank.identity()).unwrap();
    f.runtime.service().unwrap();
    f.ports.control.try_event().unwrap();
    feedback(&mut f, 0, 30);
    let mut samples = vec![num_complex::Complex32::default(); f.source.samples_per_slot()];
    for slot in 0..30 {
        let window = f
            .runtime
            .generate_slot(&mut f.source, &mut samples)
            .unwrap();
        assert!(writer.accepted(window));
        assert!(!receipt.submitted(), "coded zero-length block slot {slot}");
    }
    let window = f.source.generate_slot(&mut samples).unwrap();
    assert!(writer.accepted(window));
    assert!(receipt.submitted());
    remove(&mut f, &mut pool);
    drop(receipt);
    pool.retire(&mut ledger).unwrap();
    assert_eq!(ledger.used(), Resources::default());
}
