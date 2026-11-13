// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::rab_handoff::PendingRabAssignment;
use crate::{
    dedicated_bearers::ResetPermission,
    dedicated_rx::{DecodedTransport, TransportDelivery},
    rel99_admission::FailedChangeRequest,
};
use umts_dsp::dedicated_transport::TransportId;
use umts_rlc::acknowledged::{AmEntity, AmLimits};
use umts_rrc::establishment::SignallingProfile;

pub(super) fn active() -> InitialEnvironment {
    let mut env = environment();
    prepare(&mut env);
    start(&mut env);
    complete_radio(&mut env);
    env.admission
        .complete_packets(
            env.identity,
            &mut env.core,
            &mut env.packets,
            &mut env.receive,
            ControlTime(7),
        )
        .unwrap();
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    for _ in 0..COMMAND_VISITS {
        if env.admission.active_radio(env.identity).is_ok() {
            break;
        }
        let now = at(env.source.next_chip().index + 120);
        step(&mut env, now);
    }
    env
}
fn request(env: &mut InitialEnvironment, time: u64) -> PendingRabAssignment {
    env.peer.rua(rua::Message::Direct {
        domain: Domain::Ps,
        context: env.context,
        ranap: Assignment {
            setup: vec![],
            release: vec![umts_iuh::ranap::rab::Release::new(
                INITIAL_RAB,
                umts_iuh::ranap::rab::Cause::from_code(NORMAL_RELEASE).unwrap(),
            )],
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    let mut handoff = RabHandoff::new(WireLimits::default());
    env.core
        .drive(ControlTime(time), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    handoff.confirm(env.core.gateway());
    handoff.take().unwrap().unwrap()
}
fn start_release(env: &mut InitialEnvironment) -> (ReceiveIdentity, umts_iuh::session::RabRequest) {
    let pending = request(env, 9);
    let original = pending.request();
    let authority = env
        .admission
        .prepare_core_release(env.identity, pending, env.core.gateway())
        .unwrap();
    let now = at(env.source.next_chip().index + 120);
    let scheduled = scheduled(env, now);
    let successor = env
        .admission
        .begin_core_release(
            scheduled,
            authority,
            env.core.gateway(),
            &env.radio,
            &mut env.receive,
        )
        .unwrap();
    (successor, original)
}
fn recover_step(env: &mut InitialEnvironment, now: ChipInstant) {
    env.rx.service(|_, result| {
        result.unwrap();
    });
    env.transmit.service().unwrap();
    let report = env
        .admission
        .step_with_core(
            now,
            &mut env.radio,
            &mut env.receive,
            &mut env.common,
            &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
            env.core.gateway(),
        )
        .unwrap();
    assert!(env.admission.take_unclaimed().is_none(), "{report:?}");
}

struct UserPeer {
    sender: AmEntity,
    plan: Arc<CompiledRel99>,
    transport: TransportId,
    format: usize,
    pdu_bytes: usize,
    blocks: usize,
    next_start: ChipInstant,
    interval_chips: u64,
}
impl UserPeer {
    fn new(env: &mut InitialEnvironment) -> Self {
        let plan = env
            .admission
            .active_radio(env.identity)
            .unwrap()
            .plan()
            .clone();
        let bearer = plan
            .configuration()
            .bearers()
            .iter()
            .find(|rb| rb.domain == Some(CnDomain::Ps))
            .unwrap();
        let transport = TransportId::new(bearer.mapping.uplink[0].transport).unwrap();
        assert!(bearer.mapping.uplink[0].logical.is_none());
        let definition = plan.uplink_configured().definition(transport).unwrap();
        let (format, selected) = definition
            .formats
            .iter()
            .enumerate()
            .find(|(_, format)| format.blocks != 0)
            .unwrap();
        let pdu_bytes = selected.block_bits / BITS_PER_BYTE;
        let blocks = selected.blocks;
        let interval_chips = definition.interval.frames() as u64 * CHIPS_PER_FRAME;
        let next_start = env
            .receive
            .with_active_bank(env.identity, |bank| bank.frame_anchor().time)
            .unwrap();
        let sender = AmEntity::new(
            SignallingProfile.network_am_configuration(count(pdu_bytes), count(pdu_bytes), 0, 0),
            AmLimits {
                maximum_sdu_bytes: count(4096),
                queued_sdus: count(8),
                maximum_buffer_bytes: 1_000_000,
            },
            at(0),
        )
        .unwrap();
        Self {
            sender,
            plan,
            transport,
            format,
            pdu_bytes,
            blocks,
            next_start,
            interval_chips,
        }
    }
    fn deliver(&mut self, env: &mut InitialEnvironment, now: ChipInstant) -> Vec<Vec<u8>> {
        let start = self.next_start;
        self.next_start.index = self
            .next_start
            .index
            .checked_add(self.interval_chips)
            .unwrap();
        let now = at(now.index.max(self.next_start.index));
        let mut payload = Vec::new();
        for _ in 0..self.blocks {
            self.sender
                .publish_next(now, |pdu| {
                    assert_eq!(pdu.bytes.len(), self.pdu_bytes);
                    let bits = Bits::new(pdu.bytes, self.pdu_bytes * BITS_PER_BYTE).unwrap();
                    payload
                        .extend((0..bits.len()).map(|index| Bit::from(bits.bit(index).unwrap())));
                    Ok::<_, ()>(())
                })
                .unwrap()
                .unwrap();
        }
        let definition = self
            .plan
            .uplink_configured()
            .definition(self.transport)
            .unwrap();
        let selected = definition.formats[self.format];
        let mut delivered = Vec::new();
        env.radio
            .with_bearer_access(env.key, |ciphers, _, _| {
                env.receive
                    .with_active_bank(env.identity, |bank| {
                        bank.enqueue_transport(TransportDelivery {
                            decoded: DecodedTransport {
                                identity: env.identity,
                                transport: self.transport,
                                format: self.format,
                                start,
                                block_bits: selected.block_bits,
                                blocks: selected.blocks,
                                crc_checked: true,
                            },
                            payload: &payload,
                        })
                        .unwrap();
                        for _ in 0..64 {
                            bank.service(now, ciphers, ResetPermission::Defer).unwrap();
                            bank.process_one(|sdu| {
                                assert_eq!(
                                    sdu.metadata.scope,
                                    umts_security::ciphering::Scope::User(CnDomain::Ps)
                                );
                                let mut bytes = vec![0; sdu.data.len().div_ceil(BITS_PER_BYTE)];
                                sdu.data.copy_into(&mut bytes, 0).unwrap();
                                delivered.push(bytes);
                                true
                            });
                        }
                    })
                    .unwrap();
            })
            .unwrap();
        delivered
    }
}

#[test]
fn unpublished_supersession_rolls_back_candidate_and_preserves_partial_am_sdu_order() {
    let mut env = active();
    let previous = env.identity;
    let original_plan = env.admission.active_radio(previous).unwrap().plan().clone();
    let mut peer = UserPeer::new(&mut env);
    let first = vec![0x3d; peer.pdu_bytes * peer.blocks + 7];
    let second = vec![0x8a; peer.pdu_bytes * peer.blocks / 2 + 3];
    peer.sender.enqueue(&first).unwrap();
    peer.sender.enqueue(&second).unwrap();
    peer.sender
        .enqueue(&vec![0xc5; peer.pdu_bytes * peer.blocks * 4])
        .unwrap();
    let now = at(env.source.next_chip().index + 120);
    assert!(peer.deliver(&mut env, now).is_empty());
    let (successor, original_request) = start_release(&mut env);
    let _replacement = request(&mut env, 10);
    let mut ready = false;
    for _ in 0..COMMAND_VISITS {
        recover_step(&mut env, now);
        env.admission.visit_failed_changes(count(1), |_, identity| {
            assert_eq!(identity, previous);
            ready = true
        });
        if ready {
            break;
        }
    }
    assert!(
        ready,
        "candidate rollback must reach actual returned owners"
    );
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
    let recovered = env
        .admission
        .take_failed_change(
            previous,
            &mut env.radio,
            &mut env.receive,
            &mut env.resources,
        )
        .unwrap()
        .unwrap();
    let FailedChangeRequest::Release(rejected) = recovered else {
        panic!("original release failure owner")
    };
    assert_eq!(rejected.pending.request(), original_request);
    assert!(rejected.pool.is_none() && rejected.change.is_none());
    assert!(Arc::ptr_eq(
        env.admission.active_radio(previous).unwrap().plan(),
        &original_plan
    ));
    assert_eq!(env.resources.outstanding(), 1);
    env.receive
        .with_active_bank(previous, |bank| {
            assert!(bank.mapping_phase(successor).is_err())
        })
        .unwrap();
    let mut delivered = peer.deliver(&mut env, now);
    delivered.extend(peer.deliver(&mut env, now));
    assert_eq!(delivered, [first, second]);
}

#[test]
fn unpublished_worker_preparation_is_reclaimed_without_removing_original_receiver() {
    let mut env = active();
    let previous = env.identity;
    let (successor, _) = start_release(&mut env);
    let now = at(env.source.next_chip().index + 120);
    for _ in 0..4 {
        recover_step(&mut env, now)
    }
    assert!(env.transmit.bank().session(successor).is_some());
    assert_eq!(
        env.receive.phase(previous),
        Some(crate::dedicated_controller::DedicatedPhase::Active)
    );
    let _replacement = request(&mut env, 10);
    let mut ready = false;
    for _ in 0..COMMAND_VISITS {
        recover_step(&mut env, now);
        env.admission
            .visit_failed_changes(count(1), |_, _| ready = true);
        if ready {
            break;
        }
    }
    assert!(ready);
    assert_eq!(
        env.receive.phase(previous),
        Some(crate::dedicated_controller::DedicatedPhase::Active)
    );
    assert!(env.transmit.bank().session(successor).is_none());
    assert!(
        env.admission
            .take_failed_change(
                previous,
                &mut env.radio,
                &mut env.receive,
                &mut env.resources
            )
            .unwrap()
            .is_some()
    );
    assert!(env.admission.active_radio(previous).is_ok());
}

#[test]
fn closed_worker_transfers_bounded_quarantine_once_without_inventing_retirement() {
    let mut env = active();
    let previous = env.identity;
    let (successor, original) = start_release(&mut env);
    let now = at(env.source.next_chip().index + 120);
    for _ in 0..4 {
        recover_step(&mut env, now)
    }
    assert!(env.transmit.bank().session(successor).is_some());
    env.transmit.close();
    let _replacement = request(&mut env, 10);
    let mut failures = 0;
    for _ in 0..COMMAND_VISITS {
        env.rx.service(|_, result| {
            result.unwrap();
        });
        let report = env
            .admission
            .step_with_core(
                now,
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
                env.core.gateway(),
            )
            .unwrap();
        failures += usize::from(report.failure.is_some());
    }
    assert_eq!(
        failures, 1,
        "terminal failure must not repeat through ordinary service"
    );
    let owner = env
        .admission
        .take_failed_change(
            previous,
            &mut env.radio,
            &mut env.receive,
            &mut env.resources,
        )
        .unwrap()
        .unwrap();
    let FailedChangeRequest::Quarantined(owner) = owner else {
        panic!("unproven worker retirement must retain its complete original admission")
    };
    assert_eq!(owner.identity(), previous);
    assert_eq!(owner.release_request(), Some(original));
    assert_eq!(
        owner.error(),
        &crate::rel99_admission::AdmissionError::Closed
    );
    assert!(owner.has_radio_change());
    assert!(owner.pending_receiver_commands() + owner.retained_receivers() > 0);
    assert_eq!(env.resources.outstanding(), 1);
    assert!(env.radio.phase(env.key).is_some());
    assert!(env.admission.phase(previous).is_none());
    let mut visited = 0;
    env.admission
        .visit_failed_changes(count(1), |_, _| visited += 1);
    assert_eq!(visited, 0);
    assert!(env.transmit.bank().session(previous).is_some());
    assert!(env.transmit.bank().session(successor).is_some());
    crate::serving::assert_quarantine_recovery(owner);
}

#[test]
fn promoted_failure_removes_current_successor_and_retains_actual_predecessor_proofs() {
    managed_release_with(true, false, |env, previous, current, mut now| {
        assert_eq!(env.radio.dedicated_route(env.key), Some(current));
        assert!(env.transmit.bank().session(previous).is_none());
        assert!(env.transmit.bank().session(current).is_some());
        env.radio.retire(env.key).unwrap();
        let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
        for _ in 0..COMMAND_VISITS {
            recover_step(env, now);
            if env.admission.phase(current) == Some(AdmissionPhase::Retired) {
                break;
            }
            assert!(
                env.writer.accepted(
                    env.transmit
                        .generate_slot(&mut env.source, &mut samples)
                        .unwrap()
                )
            );
            now = env.source.next_chip();
        }
        assert_eq!(env.admission.phase(current), Some(AdmissionPhase::Retired));
        assert!(env.transmit.bank().session(previous).is_none());
        assert!(env.transmit.bank().session(current).is_none());
        assert!(env.receive.phase(current).is_none());
        let retired = env.admission.take_retired(current).unwrap();
        assert_eq!(retired.identity, current);
        assert_eq!(retired.receive.as_ref().unwrap().bank.identity(), current);
        let failed = retired.failed_change.as_ref().unwrap();
        assert_eq!(
            failed.retained_receivers(),
            1,
            "original predecessor proof stays distinct from current-bank retirement"
        );
        assert_eq!(failed.key(), env.key);
        assert!(failed.release_request().is_some());
        assert_eq!(env.resources.outstanding(), 1);
        true
    });
}

#[test]
fn admitted_removal_keeps_command_authority_until_its_closed_result_is_retained() {
    let mut env = active();
    let previous = env.identity;
    let (successor, original) = start_release(&mut env);
    let now = at(env.source.next_chip().index + 120);
    for _ in 0..4 {
        recover_step(&mut env, now)
    }
    assert!(env.transmit.bank().session(successor).is_some());
    let _replacement = request(&mut env, 10);
    env.transmit.service().unwrap();
    for _ in 0..COMMAND_VISITS {
        env.rx.service(|_, result| {
            result.unwrap();
        });
        env.admission
            .step_with_core(
                now,
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
                env.core.gateway(),
            )
            .unwrap();
        if env.admission.release_phase(previous).is_none() {
            break;
        }
    }
    assert_eq!(
        env.admission.release_phase(previous),
        None,
        "failed owner has entered joined removal"
    );
    assert!(
        matches!(
            env.admission.take_failed_change(
                previous,
                &mut env.radio,
                &mut env.receive,
                &mut env.resources
            ),
            Err(crate::rel99_admission::AdmissionError::Phase)
        ),
        "admitted but uncollected removal cannot transfer command authority"
    );
    assert!(env.transmit.bank().session(successor).is_some());
    env.transmit.close();
    for _ in 0..COMMAND_VISITS {
        env.rx.service(|_, result| {
            result.unwrap();
        });
        env.admission
            .step_with_core(
                now,
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
                env.core.gateway(),
            )
            .unwrap();
        assert!(env.admission.take_unclaimed().is_none());
    }
    let failed = env
        .admission
        .take_failed_change(
            previous,
            &mut env.radio,
            &mut env.receive,
            &mut env.resources,
        )
        .unwrap()
        .unwrap();
    let FailedChangeRequest::Quarantined(owner) = failed else {
        panic!("Closed is a diagnostic, not returned radio ownership")
    };
    assert_eq!(owner.release_request(), Some(original));
    assert_eq!(owner.retained_transmit_results(), 1);
    assert!(owner.has_radio_change());
    assert!(env.transmit.bank().session(previous).is_some());
    assert!(env.transmit.bank().session(successor).is_some());
    assert_eq!(env.resources.outstanding(), 1);
}

#[test]
fn published_one_sided_commit_retains_both_generations_until_actual_driver_tail_drains() {
    let mut env = active();
    let previous = env.identity;
    let (_integrity, _fresh) = control::install_integrity(&mut env.radio, env.key);
    let (successor, request_id) = start_release(&mut env);
    let mut now = at(env.source.next_chip().index + 120);
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..ACTIVATION_FRAME * CHIPS_PER_FRAME / CHIPS_PER_SLOT {
        for _ in 0..4 {
            recover_step(&mut env, now);
            if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::Switching) {
                break;
            }
        }
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::Switching) {
            break;
        }
        replenish_feedback(&mut env);
        assert!(
            env.writer.accepted(
                env.transmit
                    .generate_slot(&mut env.source, &mut samples)
                    .unwrap()
            )
        );
        now = env.source.next_chip();
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::Switching)
    );
    assert!(
        env.admission
            .release_procedure(previous)
            .unwrap()
            .receipt()
            .is_some()
    );
    env.rx.service(|_, result| {
        result.unwrap();
    });
    let _replacement = request(&mut env, 10);
    for _ in 0..COMMAND_VISITS {
        let report = env
            .admission
            .step_with_core(
                now,
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
                env.core.gateway(),
            )
            .unwrap();
        assert!(report.failure.is_none());
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::RecoveryRequired) {
            break;
        }
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::RecoveryRequired)
    );
    assert!(env.radio.phase(env.key).is_some());
    env.transmit.service().unwrap();
    let withheld = env
        .transmit
        .generate_slot(&mut env.source, &mut samples)
        .unwrap();
    now = env.source.next_chip();
    for _ in 0..COMMAND_VISITS {
        recover_step(&mut env, now)
    }
    assert!(
        env.radio.phase(env.key).is_some(),
        "failed RRC owner must outlive unsubmitted IQ"
    );
    assert_ne!(env.admission.phase(previous), Some(AdmissionPhase::Retired));
    assert_eq!(env.resources.outstanding(), 1);
    assert!(env.writer.accepted(withheld));
    for _ in 0..8 {
        assert!(
            env.writer.accepted(
                env.transmit
                    .generate_slot(&mut env.source, &mut samples)
                    .unwrap()
            )
        );
        now = env.source.next_chip();
        for _ in 0..8 {
            recover_step(&mut env, now)
        }
        if env.admission.phase(previous) == Some(AdmissionPhase::Retired) {
            break;
        }
    }
    assert_eq!(env.admission.phase(previous), Some(AdmissionPhase::Retired));
    assert!(env.radio.phase(env.key).is_none());
    assert!(env.transmit.bank().session(previous).is_none());
    assert!(env.transmit.bank().session(successor).is_none());
    let mut retired = env.admission.take_retired(previous).unwrap();
    let failed = retired
        .failed_change
        .as_mut()
        .expect("retained failed command and original request");
    assert_eq!(failed.release_request(), Some(request_id));
    assert_eq!(failed.retained_receivers(), 2);
    assert_eq!(failed.key(), env.key);
    assert!(
        failed
            .finish(
                &mut env.core,
                &mut env.resources,
                now,
                ControlTime(11),
                umts_iuh::ranap::rab::Cause::from_code(14).unwrap()
            )
            .unwrap()
    );
    assert_eq!(
        env.resources.outstanding(),
        1,
        "original lease remains owned by retired admission"
    );
}
