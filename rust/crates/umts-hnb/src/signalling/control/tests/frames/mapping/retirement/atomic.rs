// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::MappingRetirementError;

fn retired_workers(
    f: &mut Fixture,
    next: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
) -> (
    ReceiveCommandResult,
    TransmitCommandOutcome,
    tx_evidence::Writer,
) {
    let old = f.bank.identity();
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
        prepare_empty(f, &mut old_pool, frame);
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
    assert!(
        matches!(f.ports.control.try_event().unwrap().outcome,TransmitCommandOutcome::ReplacementPrepared {previous,identity} if previous==old && identity==next)
    );
    let prepared = f
        .bank
        .prepare_mapping(next, activation(4), plan.clone())
        .unwrap();
    f.bank.stage_mapping(prepared).unwrap();
    let mut new_pool =
        FramePool::new(next, plan.clone(), &f.ports.control, limits, &mut ledger).unwrap();
    for frame in 4..8 {
        prepare_empty(f, &mut new_pool, frame);
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
    let receive = retire_receive(f, next, &plan);
    let mut writer = f.source.submission_writer().unwrap();
    let mut samples = vec![Complex32::default(); f.source.samples_per_slot()];
    for _ in 0..61 {
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
    let transmit = f.ports.control.try_event().unwrap().outcome;
    assert!(
        matches!(&transmit,TransmitCommandOutcome::Retired {identity,result:Ok(_)} if *identity==old)
    );
    (receive, transmit, writer)
}

#[test]
fn mapping_retirement_callback_is_atomic_with_real_worker_proofs_and_preserves_endpoints() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let initial_storage = f.bank.storage_bytes();
    let old_plan = f.plan.clone();
    let next = next_identity(&f);
    let plan = remapped(&f);
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), b"retained downlink")
        .unwrap();
    let (receive, transmit, _writer) = retired_workers(&mut f, next, plan.clone());
    let charged = f.bank.storage_bytes();
    assert!(charged > initial_storage);
    let mut other = old;
    other.configuration = NonZeroU64::new(99).unwrap();
    let wrong_receive = retire_receive(&f, other, &plan);
    for (identity, rx, tx) in [
        (other, &receive, &transmit),
        (old, &wrong_receive, &transmit),
        (old, &ReceiveCommandResult::Closed, &transmit),
        (old, &receive, &TransmitCommandOutcome::Closed),
    ] {
        assert_eq!(
            f.bank
                .retire_mapping_with(identity, rx, tx, |_| -> Result<(), ()> {
                    panic!("invalid retirement reached resource commit")
                }),
            Err(MappingRetirementError::Bearer(BearerError::Phase))
        );
    }
    let mut sender = receiver();
    sender.enqueue(b"retained uplink").unwrap();
    sender.enqueue(&[0x63; 400]).unwrap();
    enqueue_packet(&mut f, &old_plan, old, 4, 2, &mut sender).unwrap();
    assert_eq!(
        f.bank
            .retire_mapping_with(old, &receive, &transmit, |_| -> Result<(), ()> {
                panic!("pending predecessor data reached resource commit")
            }),
        Err(MappingRetirementError::Bearer(BearerError::Phase))
    );
    assert_eq!(f.bank.identity(), old);
    assert_eq!(f.bank.storage_bytes(), charged);
    service(&mut f, 4);
    let mut calls = 0;
    assert_eq!(
        f.bank.retire_mapping_with(old, &receive, &transmit, |_| {
            calls += 1;
            Err::<(), _>("resource pressure")
        }),
        Err(MappingRetirementError::Commit("resource pressure"))
    );
    assert_eq!(calls, 1);
    assert_eq!(f.bank.identity(), old);
    assert_eq!(f.bank.storage_bytes(), charged);
    assert!(f.bank.contains_identity(old));
    assert!(f.bank.contains_identity(next));
    assert!(Arc::ptr_eq(
        f.bank.configured(),
        old_plan.uplink_configured()
    ));
    assert!(
        matches!(f.bank.mapping_phase(old),Ok(MappingPhase::Draining {next:identity,..}) if identity==next)
    );
    assert_eq!(
        f.bank.mapping_phase(next),
        Ok(MappingPhase::Scheduled { previous: old })
    );
    assert_eq!(
        f.bank.retire_mapping_with(old, &receive, &transmit, |_| {
            calls += 1;
            Ok::<_, &str>(37)
        }),
        Ok(37)
    );
    assert_eq!(calls, 2);
    assert_eq!(f.bank.identity(), next);
    assert_eq!(f.bank.storage_bytes(), initial_storage);
    assert!(!f.bank.contains_identity(old));
    assert_eq!(f.bank.mapping_phase(next), Ok(MappingPhase::Active));
    assert!(Arc::ptr_eq(f.bank.configured(), plan.uplink_configured()));
    let mut delivered = false;
    assert!(f.bank.process_one(|delivery| {
        assert_eq!(delivery.metadata.identity, old);
        assert_eq!(delivery.metadata.bearer, BearerId::new(20).unwrap());
        assert_eq!(
            delivery.metadata.coverage.unwrap().provenance(),
            ReceiveProvenance::Path(old.configuration)
        );
        let mut bytes = vec![0; delivery.data.len() / BITS_PER_BYTE];
        delivery.data.copy_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained uplink");
        delivered = true;
        true
    }));
    assert!(delivered);
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            assert!(
                f.bank
                    .publish_sequenced(
                        grant(next, 19, 20, 1, 8),
                        at(8 * CHIPS_PER_FRAME),
                        ciphers,
                        |_| Ok::<_, ()>(())
                    )
                    .unwrap()
                    .is_some()
            );
        })
        .unwrap();
    assert_eq!(
        f.bank
            .retire_mapping_with(old, &receive, &transmit, |_| -> Result<(), ()> {
                panic!("retirement replay")
            }),
        Err(MappingRetirementError::Bearer(BearerError::Phase))
    );
}
