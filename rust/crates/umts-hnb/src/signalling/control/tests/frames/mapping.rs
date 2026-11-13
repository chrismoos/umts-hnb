// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::signalling::{ResourceLedger, Resources};
use crate::{
    dedicated_bearers::{FramePool, FramePoolLimits, MappingPhase, ResetPermission},
    dedicated_rx::{DecodedTransport, ReceiveCommandResult, TransportDelivery},
};
use umts_rlc::acknowledged::ReceiveProvenance;

pub(super) mod retirement;
mod traffic;

#[test]
fn directional_extended_frame_counters_need_matching_cfn_not_matching_hfn() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let plan = remapped(&f);
    f.bank.retire();
    let endpoints = f.bank.into_endpoints().ok().unwrap();
    let mut initial = activation(0);
    initial.uplink_frame = 256;
    f.bank = DedicatedBearers::from_compiled(
        old,
        initial,
        f.plan.clone(),
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: f.radio.limits.maximum_sdu_bytes,
            maximum_buffer_bytes: 2_000_000,
        },
    )
    .unwrap();
    f.bank.activate(old).unwrap();
    let mut timing = activation(4);
    timing.uplink_frame = 260;
    let next = next_identity(&f);
    let prepared = f.bank.prepare_mapping(next, timing, plan).unwrap();
    f.bank.stage_mapping(prepared).unwrap();
    f.bank.commit_mapping(next).unwrap();
    assert!(
        matches!(f.bank.mapping_phase(old), Ok(MappingPhase::Draining { next: id, .. }) if id == next)
    );
}

#[test]
fn successor_metadata_respects_the_same_per_ue_buffer_budget() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let bytes = f.bank.storage_bytes();
    let plan = remapped(&f);
    f.bank.retire();
    let endpoints = f.bank.into_endpoints().ok().unwrap();
    f.bank = DedicatedBearers::from_compiled(
        old,
        activation(0),
        f.plan.clone(),
        endpoints,
        BearerLimits {
            maximum_transport_bits: count(10_000),
            output_slots: count(8),
            maximum_sdu_bytes: f.radio.limits.maximum_sdu_bytes,
            maximum_buffer_bytes: bytes,
        },
    )
    .unwrap();
    f.bank.activate(old).unwrap();
    assert_eq!(f.bank.storage_bytes(), bytes);
    assert_eq!(
        f.bank
            .prepare_mapping(next_identity(&f), activation(4), plan)
            .err(),
        Some(BearerError::Capacity)
    );
    assert_eq!(f.bank.storage_bytes(), bytes);
    assert_eq!(f.bank.mapping_phase(old), Ok(MappingPhase::Active));
}

#[test]
fn partially_consumed_predecessor_frame_requires_connection_wide_recovery() {
    let mut f = fixture(true);
    let plan = remapped(&f);
    let next = stage(&mut f, 4, plan.clone());
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), &[0x31; 200])
        .unwrap();
    let mut draft = f
        .bank
        .begin_frame(
            f.plan.clone(),
            &f.ports.control,
            at(0),
            Tfci::new(6).unwrap(),
        )
        .unwrap();
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            assert!(
                draft
                    .publish_sequenced(grant(draft.identity(), 9, 20, 1, 0), at(0), ciphers)
                    .unwrap()
            );
        })
        .unwrap();
    drop(draft);
    let mut ledger = ResourceLedger::new(
        f.key.epoch,
        Resources {
            queue_bytes: 4_000_000,
            ..Resources::default()
        },
        8,
    );
    let mut pool = FramePool::new(
        next,
        plan,
        &f.ports.control,
        FramePoolLimits {
            payload_slots: count(1),
            receipt_slots: count(32),
            maximum_buffer_bytes: 2_000_000,
        },
        &mut ledger,
    )
    .unwrap();
    assert_eq!(
        f.bank
            .begin_pooled_frame(
                &mut pool,
                &f.ports.control,
                at(4 * CHIPS_PER_FRAME),
                Tfci::new(0).unwrap()
            )
            .err(),
        Some(FrameBuildError::Bearer(BearerError::RecoveryRequired))
    );
    assert_eq!(
        f.bank.commit_mapping(next),
        Err(BearerError::RecoveryRequired)
    );
}

fn next_identity(f: &Fixture) -> ReceiveIdentity {
    ReceiveIdentity {
        configuration: NonZeroU64::new(2).unwrap(),
        ue: f.key,
    }
}
fn activation(frame: u64) -> MappingActivation {
    let anchor = FrameAnchor {
        time: at(frame * CHIPS_PER_FRAME),
        cfn: frame as u8,
    };
    MappingActivation {
        uplink: anchor,
        uplink_frame: frame,
        downlink: anchor,
        downlink_frame: frame,
    }
}
fn remapped(f: &Fixture) -> Arc<CompiledRel99> {
    recompile(
        f,
        |bearers, _, _| {
            for bearer in bearers {
                for mapping in &mut bearer.mapping.uplink {
                    mapping.transport += 10;
                }
                for mapping in &mut bearer.mapping.downlink {
                    mapping.transport += 10;
                }
            }
        },
        true,
    )
}
fn recompile(
    f: &Fixture,
    edit: impl FnOnce(
        &mut Vec<umts_rrc::dedicated::Rel99Bearer>,
        &mut umts_rrc::dedicated::NormalDpch,
        &mut Vec<umts_rrc::dedicated::DchTransport>,
    ),
    remap: bool,
) -> Arc<CompiledRel99> {
    let config = f.plan.configuration();
    let mut ul = config.uplink().transports().to_vec();
    let mut dl = config.downlink().transports().to_vec();
    if remap {
        for transport in ul.iter_mut().chain(&mut dl) {
            transport.id += 10;
        }
    }
    let mut bearers = config.bearers().to_vec();
    let mut physical = config.physical().clone();
    edit(&mut bearers, &mut physical, &mut ul);
    let config = Arc::new(
        Rel99Configuration::new(
            DchTfcs::new(
                DchDirection::Uplink,
                ul,
                config.uplink().combinations().to_vec(),
            )
            .unwrap(),
            DchTfcs::new(
                DchDirection::Downlink,
                dl,
                config.downlink().combinations().to_vec(),
            )
            .unwrap(),
            bearers,
            physical,
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
fn stage(f: &mut Fixture, frame: u64, plan: Arc<CompiledRel99>) -> ReceiveIdentity {
    let next = next_identity(f);
    let prepared = f
        .bank
        .prepare_mapping(next, activation(frame), plan)
        .unwrap();
    assert_eq!(f.bank.stage_mapping(prepared).unwrap(), next);
    next
}
fn receiver() -> AmEntity {
    AmEntity::new(
        SignallingProfile.network_am_configuration(count(40), count(40), 0, 0),
        AmLimits {
            maximum_sdu_bytes: count(512),
            queued_sdus: count(8),
            maximum_buffer_bytes: 1_000_000,
        },
        at(0),
    )
    .unwrap()
}
fn enqueue_packet(
    f: &mut Fixture,
    plan: &CompiledRel99,
    identity: ReceiveIdentity,
    transport: u8,
    frame: u64,
    sender: &mut AmEntity,
) -> Result<(), BearerError> {
    let transport = TransportId::new(transport).unwrap();
    let format = plan
        .uplink_configured()
        .definition(transport)
        .unwrap()
        .formats[1];
    let mut payload = Vec::new();
    for _ in 0..format.blocks {
        sender
            .publish_next(at(frame * CHIPS_PER_FRAME), |pdu| {
                assert_eq!(pdu.bytes.len() * BITS_PER_BYTE, format.block_bits);
                payload.extend((0..format.block_bits).map(|bit| {
                    Bit::from(
                        Bits::new(pdu.bytes, format.block_bits)
                            .unwrap()
                            .bit(bit)
                            .unwrap(),
                    )
                }));
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
    }
    f.bank.enqueue_transport(TransportDelivery {
        decoded: DecodedTransport {
            identity,
            transport,
            format: 1,
            start: at(frame * CHIPS_PER_FRAME),
            block_bits: format.block_bits,
            blocks: format.blocks,
            crc_checked: format.crc.width() != 0,
        },
        payload: &payload,
    })
}
fn service(f: &mut Fixture, frame: u64) {
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            for _ in 0..64 {
                f.bank
                    .service(at(frame * CHIPS_PER_FRAME), ciphers, ResetPermission::Defer)
                    .unwrap();
            }
        })
        .unwrap();
}

#[test]
fn successor_map_prepares_downlink_without_relabeling_predecessor() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let old_plan = f.plan.clone();
    let plan = remapped(&f);
    let before = f.bank.storage_bytes();
    let next = stage(&mut f, 4, plan.clone());
    assert!(f.bank.storage_bytes() > before);
    assert_eq!(f.bank.identity(), old);
    assert_eq!(f.bank.mapping_phase(old), Ok(MappingPhase::Active));
    assert_eq!(
        f.bank.mapping_phase(next),
        Ok(MappingPhase::Staged { previous: old })
    );
    assert!(Arc::ptr_eq(
        f.bank.configured(),
        old_plan.uplink_configured()
    ));
    let mut ledger = ResourceLedger::new(
        f.key.epoch,
        Resources {
            queue_bytes: 4_000_000,
            ..Resources::default()
        },
        8,
    );
    let mut pool = FramePool::new(
        next,
        plan.clone(),
        &f.ports.control,
        FramePoolLimits {
            payload_slots: count(1),
            receipt_slots: count(32),
            maximum_buffer_bytes: 2_000_000,
        },
        &mut ledger,
    )
    .unwrap();
    let draft = f
        .bank
        .begin_pooled_frame(
            &mut pool,
            &f.ports.control,
            at(4 * CHIPS_PER_FRAME),
            Tfci::new(0).unwrap(),
        )
        .unwrap();
    assert_eq!(draft.identity(), next);
    let input = draft.finish().ok().unwrap();
    pool.recycle(input).ok().unwrap();
    assert_eq!(f.bank.identity(), old);
    let mut sender = receiver();
    sender.enqueue(&[0x41; 400]).unwrap();
    assert_eq!(
        enqueue_packet(&mut f, &plan, next, 14, 4, &mut sender),
        Err(BearerError::Phase)
    );
    f.bank.commit_mapping(next).unwrap();
    assert_eq!(
        f.bank.mapping_phase(next),
        Ok(MappingPhase::Scheduled { previous: old })
    );
    assert!(
        matches!(f.bank.mapping_phase(old), Ok(MappingPhase::Draining { next: id, .. }) if id == next)
    );
}

#[test]
fn one_packet_rlc_preserves_reassembly_and_provenance_across_mapping_change() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let old_plan = f.plan.clone();
    let plan = remapped(&f);
    let next = stage(&mut f, 4, plan.clone());
    let mut sender = receiver();
    sender.enqueue(&[0x51; 200]).unwrap();
    sender.enqueue(&[0x72; 200]).unwrap();
    enqueue_packet(&mut f, &old_plan, old, 4, 2, &mut sender).unwrap();
    f.bank.commit_mapping(next).unwrap();
    assert_eq!(
        enqueue_packet(&mut f, &plan, next, 14, 4, &mut receiver_with_data()),
        Err(BearerError::Capacity)
    );
    service(&mut f, 4);
    enqueue_packet(&mut f, &plan, next, 14, 4, &mut sender).unwrap();
    service(&mut f, 6);
    let mut sdus = Vec::new();
    while f.bank.process_one(|delivery| {
        let mut bytes = vec![0; delivery.data.len().div_ceil(BITS_PER_BYTE)];
        delivery.data.copy_to(&mut bytes).unwrap();
        sdus.push((delivery.metadata, bytes));
        true
    }) {}
    assert_eq!(sdus.len(), 1);
    assert_eq!(sdus[0].1, [0x51; 200]);
    assert_eq!(
        sdus[0].0.coverage.unwrap().provenance(),
        ReceiveProvenance::Mixed
    );
    assert_eq!(f.bank.counters().malformed, 0);
}
fn receiver_with_data() -> AmEntity {
    let mut sender = receiver();
    sender.enqueue(&[0x88; 400]).unwrap();
    sender
}

#[test]
fn pending_and_completed_old_outputs_keep_their_original_mapping_identity() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let plan = remapped(&f);
    let old_plan = f.plan.clone();
    let next = stage(&mut f, 4, plan);
    let mut sender = receiver();
    sender.enqueue(b"old output").unwrap();
    sender.enqueue(&[0x63; 400]).unwrap();
    enqueue_packet(&mut f, &old_plan, old, 4, 2, &mut sender).unwrap();
    f.bank.commit_mapping(next).unwrap();
    service(&mut f, 4);
    let mut seen = false;
    assert!(f.bank.process_one(|delivery| {
        assert_eq!(delivery.metadata.identity, old);
        assert_eq!(
            delivery.metadata.coverage.unwrap().provenance(),
            ReceiveProvenance::Path(old.configuration)
        );
        let mut bytes = vec![0; delivery.data.len() / BITS_PER_BYTE];
        delivery.data.copy_to(&mut bytes).unwrap();
        assert_eq!(bytes, b"old output");
        seen = true;
        true
    }));
    assert!(seen);
}

#[test]
fn wrong_owner_duplicate_and_third_mapping_do_not_change_resources() {
    let mut f = fixture(true);
    let mut other = fixture(true);
    let plan = remapped(&f);
    let prepared = f
        .bank
        .prepare_mapping(next_identity(&f), activation(4), plan.clone())
        .unwrap();
    let before = other.bank.storage_bytes();
    let rejected = other.bank.stage_mapping(prepared).unwrap_err();
    assert_eq!(rejected.error, BearerError::Owner);
    assert_eq!(other.bank.storage_bytes(), before);
    f.bank.stage_mapping(rejected.prepared).unwrap();
    let bytes = f.bank.storage_bytes();
    assert_eq!(
        f.bank
            .prepare_mapping(
                ReceiveIdentity {
                    configuration: NonZeroU64::new(3).unwrap(),
                    ue: f.key
                },
                activation(8),
                plan
            )
            .err(),
        Some(BearerError::Phase)
    );
    assert_eq!(f.bank.storage_bytes(), bytes);
}

#[test]
fn fractional_or_cfn_misaligned_activation_cannot_replace_the_map() {
    let f = fixture(true);
    let plan = remapped(&f);
    let mut fractional = activation(4);
    fractional.uplink.time.index += 1;
    let mut downlink_fractional = activation(4);
    downlink_fractional.downlink.time.index += 1;
    let mut wrong_cfn = activation(4);
    wrong_cfn.downlink.cfn = 8;
    for timing in [activation(3), fractional, downlink_fractional, wrong_cfn] {
        assert_eq!(
            f.bank
                .prepare_mapping(next_identity(&f), timing, plan.clone())
                .err(),
            Some(BearerError::Time)
        );
    }
    let mut timing = activation(4);
    timing.downlink_frame = 260;
    assert_eq!(
        f.bank
            .prepare_mapping(next_identity(&f), timing, plan)
            .err(),
        Some(BearerError::Time)
    );
}

#[test]
fn changed_rlc_or_domain_requires_a_distinct_construction() {
    let f = fixture(true);
    let plan = recompile(
        &f,
        |bearers, _, _| {
            bearers
                .iter_mut()
                .find(|bearer| bearer.id == 20)
                .unwrap()
                .domain = Some(CnDomain::Cs);
        },
        false,
    );
    assert_eq!(
        f.bank
            .prepare_mapping(next_identity(&f), activation(4), plan)
            .err(),
        Some(BearerError::UnsupportedBinding)
    );
    let plan = recompile(
        &f,
        |bearers, _, _| {
            let bearer = bearers.iter_mut().find(|bearer| bearer.id == 20).unwrap();
            let umts_rrc::dedicated::Rel99Rlc::Acknowledged { uplink, .. } = &mut bearer.rlc else {
                unreachable!()
            };
            uplink.transmission_window_size = ie::TransmissionWindowSize::tw16;
        },
        false,
    );
    assert_eq!(
        f.bank
            .prepare_mapping(next_identity(&f), activation(4), plan)
            .err(),
        Some(BearerError::UnsupportedBinding)
    );
}

#[test]
fn late_old_receive_work_prevents_backdated_mapping_commit() {
    let mut f = fixture(true);
    let plan = remapped(&f);
    let old_plan = f.plan.clone();
    let old = f.bank.identity();
    let next = stage(&mut f, 4, plan);
    enqueue_packet(&mut f, &old_plan, old, 4, 4, &mut receiver_with_data()).unwrap();
    assert_eq!(f.bank.commit_mapping(next), Err(BearerError::Time));
    assert_eq!(f.bank.mapping_phase(old), Ok(MappingPhase::Active));
    assert_eq!(
        f.bank.mapping_phase(next),
        Ok(MappingPhase::Staged { previous: old })
    );
}

#[test]
fn committed_old_mapping_rejects_new_time_grants_and_requires_real_retirement() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let plan = remapped(&f);
    let old_plan = f.plan.clone();
    let next = stage(&mut f, 4, plan);
    f.bank.commit_mapping(next).unwrap();
    let before = f.bank.storage_bytes();
    assert_eq!(
        enqueue_packet(&mut f, &old_plan, old, 4, 4, &mut receiver_with_data()),
        Err(BearerError::Time)
    );
    f.bank
        .enqueue_sdu(BearerId::new(20).unwrap(), b"packet")
        .unwrap();
    f.radio
        .with_bearer_access(f.key, |ciphers, _, _| {
            assert!(matches!(
                f.bank
                    .publish_sequenced(
                        grant(old, 9, 20, 1, 4),
                        at(0),
                        ciphers,
                        |_| Ok::<_, ()>(())
                    ),
                Err(crate::dedicated_bearers::TransmitError::Bearer(
                    BearerError::Time
                ))
            ));
            assert!(
                f.bank
                    .publish_sequenced(grant(next, 19, 20, 1, 4), at(0), ciphers, |_| Ok::<_, ()>(
                        ()
                    ))
                    .unwrap()
                    .is_some()
            );
        })
        .unwrap();
    assert_eq!(
        f.bank.retire_mapping(
            old,
            &ReceiveCommandResult::Closed,
            &TransmitCommandOutcome::Closed
        ),
        Err(BearerError::Phase)
    );
    assert_eq!(f.bank.storage_bytes(), before);
    assert_eq!(f.bank.identity(), old);
}
