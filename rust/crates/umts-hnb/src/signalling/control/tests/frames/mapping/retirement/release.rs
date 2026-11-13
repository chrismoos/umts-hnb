// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::{MappingReleaseError, ReleaseOutputPolicy};
use umts_rrc::dedicated::{
    Combination, ExistingDchRelease, RadioBearerReleaseProcedure, ScheduledActivation,
};

mod cancellation;
mod packet_handoff;

pub(in crate::signalling::control::tests::frames) fn subset(
    f: &Fixture,
    remove_cs: bool,
    remove_ps: bool,
) -> Arc<CompiledRel99> {
    let config = f.plan.configuration();
    let bearers: Vec<_> = config
        .bearers()
        .iter()
        .filter(|rb| {
            !matches!(rb.domain,
        Some(CnDomain::Cs) if remove_cs)
                && !matches!(rb.domain, Some(CnDomain::Ps) if remove_ps)
        })
        .cloned()
        .collect();
    let tfcs = |old: &DchTfcs, direction| {
        let indices: Vec<_> = old
            .transports()
            .iter()
            .enumerate()
            .filter(|(_, trch)| {
                bearers
                    .iter()
                    .any(|rb| rb.mappings(direction).any(|map| map.transport == trch.id))
            })
            .map(|(index, _)| index)
            .collect();
        let transports = indices
            .iter()
            .map(|index| old.transports()[*index].clone())
            .collect();
        let mut combinations: Vec<Combination> = Vec::new();
        for old in old.combinations() {
            let formats: Vec<_> = indices.iter().map(|index| old.formats[*index]).collect();
            if !combinations.iter().any(|row| row.formats == formats) {
                combinations.push(Combination {
                    formats,
                    gain: old.gain,
                });
            }
        }
        DchTfcs::new(direction, transports, combinations).unwrap()
    };
    let config = Arc::new(
        Rel99Configuration::new(
            tfcs(config.uplink(), DchDirection::Uplink),
            tfcs(config.downlink(), DchDirection::Downlink),
            bearers,
            config.physical().clone(),
        )
        .unwrap(),
    );
    Arc::new(
        CompiledRel99::new(
            config,
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}

#[test]
fn explicit_release_subset_preserves_rebind_guards_and_rejects_wrong_activation_cycle() {
    let f = fixture(true);
    for (cs, ps) in [(true, false), (false, true), (true, true)] {
        let plan = subset(&f, cs, ps);
        assert_eq!(
            f.bank
                .prepare_mapping(next_identity(&f), activation(4), plan.clone())
                .err(),
            Some(BearerError::UnsupportedBinding)
        );
        assert!(
            f.bank
                .prepare_release_mapping(next_identity(&f), activation(4), plan.clone())
                .is_ok()
        );
        let mut timing = activation(4);
        timing.uplink_frame += 256;
        timing.uplink.time.index += 256 * CHIPS_PER_FRAME;
        assert_eq!(
            f.bank
                .prepare_release_mapping(next_identity(&f), timing, plan)
                .err(),
            Some(BearerError::Time)
        );
    }
    assert_eq!(
        f.bank
            .prepare_release_mapping(next_identity(&f), activation(4), f.plan.clone())
            .err(),
        Some(BearerError::UnsupportedBinding)
    );
}

pub(in crate::signalling::control::tests::frames) fn accepted(
    f: &mut Fixture,
    next: ReceiveIdentity,
    plan: &Arc<CompiledRel99>,
) -> crate::signalling::AcceptedBearerRelease {
    let old = f.bank.identity();
    let (integrity, fresh) = install_integrity(&mut f.radio, f.key);
    f.radio.registry.get_mut(f.key).unwrap().dedicated_route = Some(old);
    let mut sender = peer(&f.source);
    sender.enqueue(&COMPLETE).unwrap();
    sender
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let mut procedure = RadioBearerReleaseProcedure::new(
        f.key,
        f.radio.next_bearer_release_transaction(f.key).unwrap(),
        ExistingDchRelease::new(
            f.plan.configuration().clone(),
            plan.configuration().clone(),
            ScheduledActivation::new(at(0), 0, std::num::NonZeroU8::new(4).unwrap()).unwrap(),
        )
        .unwrap(),
        at(0),
        at(1000),
        NonZeroU64::new(CHIPS_PER_FRAME * 10).unwrap(),
    )
    .unwrap();
    let receipt = f
        .radio
        .publish_bearer_release_on(&mut procedure, old, next, at(0))
        .unwrap();
    publish(&mut f.radio, f.key, receipt, &mut sender);
    let value =
        ULDCCHMessageType::radioBearerReleaseComplete(pdu::RadioBearerReleaseComplete::new(
            ie::RRCTransactionIdentifier(procedure.transaction().get()),
            None,
            None,
            None,
            None,
            None,
            None,
        ));
    let bytes = integrity::protected(&integrity, fresh, 2, 2, value);
    uplink_on(
        &mut f.radio,
        f.key,
        &mut sender,
        &bytes,
        CHIPS_PER_FRAME * 4,
        ReceiveProvenance::Path(next.configuration),
    );
    let mut accepted = None;
    assert_eq!(
        f.radio.process_one_at_with_control(
            at(CHIPS_PER_FRAME * 4 + 20),
            security_time(),
            |message, mut security| {
                let (_, result) = message.bearer_release_response_with(
                    &mut procedure,
                    &mut security,
                    |_, _| Ok(()),
                )?;
                accepted = result.map(|(token, ())| token);
                Ok(())
            }
        ),
        Some(Ok(f.key))
    );
    accepted.unwrap()
}

pub(in crate::signalling::control::tests::frames) fn retire_workers(
    f: &mut Fixture,
    next: ReceiveIdentity,
    plan: &Arc<CompiledRel99>,
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
        matches!(f.ports.control.try_event().unwrap().outcome, TransmitCommandOutcome::ReplacementPrepared { previous, identity } if previous == old && identity == next)
    );
    let prepared = f
        .bank
        .prepare_release_mapping(next, activation(4), plan.clone())
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
    let receive = retire_receive(f, next, plan);
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
    let retired = f.ports.control.try_event().unwrap().outcome;
    assert!(matches!(
        &retired,
        TransmitCommandOutcome::Retired { result: Ok(_), .. }
    ));
    (receive, retired, writer)
}

#[test]
fn protected_release_joins_exact_workers_and_atomically_retires_only_selected_user_and_cipher_owners()
 {
    for (cs, ps, removed) in [
        (true, false, vec![10, 11, 12]),
        (false, true, vec![20]),
        (true, true, vec![10, 11, 12, 20]),
    ] {
        let mut f = fixture(true);
        let old = f.bank.identity();
        let next = next_identity(&f);
        let plan = subset(&f, cs, ps);
        if !ps {
            f.bank
                .enqueue_sdu(BearerId::new(20).unwrap(), b"retained packet")
                .unwrap();
        }
        let token = accepted(&mut f, next, &plan);
        let (receive, transmit, _writer) = retire_workers(&mut f, next, &plan);
        if cs && !ps {
            let other_next = ReceiveIdentity {
                configuration: NonZeroU64::new(3).unwrap(),
                ..next
            };
            let wrong_receive = retire_receive(&f, other_next, &plan);
            let mut other = fixture_parts_with_plan(true, true, true, false, |_, _| f.plan.clone());
            assert_eq!(other.bank.identity(), old);
            let (_, wrong_transmit, _wrong_writer) = retire_workers(&mut other, other_next, &plan);
            f.radio
                .with_bearer_access(f.key, |ciphers, _, _| {
                    for (rx, tx) in [(&wrong_receive, &transmit), (&receive, &wrong_transmit)] {
                        assert!(matches!(
                            f.bank.retire_release_mapping_with(
                                &token,
                                rx,
                                tx,
                                ReleaseOutputPolicy::Deliver,
                                ciphers,
                                |_| -> Result<(), ()> { panic!("different actual successor") }
                            ),
                            Err(MappingReleaseError::Bearer(BearerError::Phase))
                        ));
                    }
                })
                .unwrap();
        }
        if ps {
            assert_eq!(
                f.bank.enqueue_sdu(BearerId::new(20).unwrap(), b"late"),
                Err(BearerError::Phase)
            );
        }
        let charged = f.bank.storage_bytes();
        f.radio
            .with_bearer_access(f.key, |ciphers, _, _| {
                assert_eq!(
                    f.bank.retire_mapping(old, &receive, &transmit),
                    Err(BearerError::UnsupportedBinding)
                );
                assert!(matches!(
                    f.bank.retire_release_mapping_with(
                        &token,
                        &ReceiveCommandResult::Closed,
                        &transmit,
                        ReleaseOutputPolicy::Deliver,
                        ciphers,
                        |_| -> Result<(), ()> { panic!("missing RX proof") }
                    ),
                    Err(MappingReleaseError::Bearer(BearerError::Phase))
                ));
                assert_eq!(
                    f.bank.retire_release_mapping_with(
                        &token,
                        &receive,
                        &transmit,
                        ReleaseOutputPolicy::Deliver,
                        ciphers,
                        |ids| {
                            assert_eq!(ids.iter().map(|id| id.get()).collect::<Vec<_>>(), removed);
                            Err::<(), _>("busy")
                        }
                    ),
                    Err(MappingReleaseError::Cleanup("busy"))
                );
                assert_eq!(f.bank.storage_bytes(), charged);
                assert_eq!(f.bank.identity(), old);
                for id in &removed {
                    assert!(ciphers.layout(f.key, *id).is_ok());
                }
            })
            .unwrap();
        let result = f
            .radio
            .commit_bearer_release_with(&token, |ciphers| {
                f.bank.retire_release_mapping_with(
                    &token,
                    &receive,
                    &transmit,
                    ReleaseOutputPolicy::Deliver,
                    ciphers,
                    |_| Ok::<_, ()>(()),
                )
            })
            .unwrap();
        assert_eq!(result.released_bearers, removed.len());
        assert_eq!(result.discarded_outputs, 0);
        assert_eq!(result.discarded_receive_bearers, 0);
        assert_eq!(f.bank.storage_bytes(), charged - result.released_bytes);
        assert_eq!(f.bank.identity(), next);
        f.radio
            .with_bearer_access(f.key, |ciphers, _, _| {
                for id in &removed {
                    assert!(ciphers.layout(f.key, *id).is_err());
                }
                assert!(ciphers.layout(f.key, 2).is_ok());
                if !ps {
                    assert!(
                        f.bank
                            .publish_sequenced(
                                grant(next, 9, 20, 1, 8),
                                at(CHIPS_PER_FRAME * 8),
                                ciphers,
                                |_| Ok::<_, ()>(())
                            )
                            .unwrap()
                            .is_some()
                    );
                }
                if !cs {
                    assert!(ciphers.layout(f.key, 10).is_ok());
                }
            })
            .unwrap();
        assert_eq!(f.radio.dedicated_route(f.key), Some(next));
        assert!(!f.bank.contains_identity(old));
    }
}

#[test]
fn removed_packet_output_and_partial_receive_require_explicit_counted_discard() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let old_plan = f.plan.clone();
    let next = next_identity(&f);
    let plan = subset(&f, false, true);
    let mut sender = receiver();
    sender.enqueue(b"old output").unwrap();
    sender.enqueue(&[0x63; 400]).unwrap();
    enqueue_packet(&mut f, &old_plan, old, 4, 0, &mut sender).unwrap();
    service(&mut f, 2);
    let token = accepted(&mut f, next, &plan);
    let (receive, transmit, _writer) = retire_workers(&mut f, next, &plan);
    let charged = f.bank.storage_bytes();
    assert!(matches!(
        f.radio
            .commit_bearer_release_with(&token, |ciphers| f.bank.retire_release_mapping_with(
                &token,
                &receive,
                &transmit,
                ReleaseOutputPolicy::Deliver,
                ciphers,
                |_| -> Result<(), ()> { panic!("receive data not settled") }
            )),
        Err(crate::signalling::BearerReleaseCommitError::Commit(
            MappingReleaseError::Bearer(BearerError::Capacity)
        ))
    ));
    assert_eq!(f.bank.storage_bytes(), charged);
    assert_eq!(f.radio.dedicated_route(f.key), Some(old));
    let released = f
        .radio
        .commit_bearer_release_with(&token, |ciphers| {
            f.bank.retire_release_mapping_with(
                &token,
                &receive,
                &transmit,
                ReleaseOutputPolicy::DiscardReleased,
                ciphers,
                |_| Ok::<_, ()>(()),
            )
        })
        .unwrap();
    assert_eq!(released.discarded_outputs, 1);
    assert_eq!(released.discarded_receive_bearers, 1);
    assert_eq!(f.bank.counters().retired_outputs, 1);
    assert_eq!(f.bank.counters().released_receive_bearers, 1);
    assert_eq!(f.radio.dedicated_route(f.key), Some(next));
    assert!(
        !f.bank
            .process_one(|_| panic!("removed packet output leaked"))
    );
}
