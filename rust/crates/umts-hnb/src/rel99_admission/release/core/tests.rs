// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::CommonTx,
    core_link::tests::{Mock, Peer, ready, register},
    rab_handoff::{RabHandoff, SetupAssignment},
    signalling::tests::controller,
    test_support::{count, rel99 as fixture, source},
};
use std::num::{NonZeroU8, NonZeroU64};
use umts_dsp::{coding::DecodeBudget, dedicated_transport::TransportPlanLimits};
use umts_iuh::{
    ContextId,
    ranap::rab::{Assignment, AssignmentResponse, First, Release, Second, SetupOrModify},
    rua,
    wire::Limits,
};
use umts_types::UeKey;

struct Environment {
    _source: CommonTx,
    radio: CommonSignalling,
    link: CoreLink<Mock>,
    peer: Peer,
    context: ContextId,
    key: UeKey,
}
fn environment() -> Environment {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, peer) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &peer, &mut radio, &source, 0, 3);
    Environment {
        _source: source,
        radio,
        link,
        peer,
        context,
        key,
    }
}
fn plan(voice: bool, packet: bool) -> Arc<CompiledRel99> {
    Arc::new(
        CompiledRel99::new(
            fixture::configuration(voice, packet, 10),
            count(200_000),
            TransportPlanLimits {
                maximum_coded_bits: count(200_000),
                maximum_mapping_entries: count(300_000),
                decode: DecodeBudget::new(NonZeroU8::new(8).unwrap()).unwrap(),
            },
        )
        .unwrap(),
    )
}
fn identity(env: &Environment) -> ReceiveIdentity {
    ReceiveIdentity {
        ue: env.key,
        configuration: NonZeroU64::MIN,
    }
}
fn setup(id: u8) -> SetupOrModify {
    SetupOrModify {
        first: First::modification(id),
        second: Second::default(),
    }
}
fn wire(setup: Vec<SetupOrModify>, release: &[u8]) -> Vec<u8> {
    Assignment {
        setup,
        release: release
            .iter()
            .map(|id| Release::new(*id, Cause::from_code(32).unwrap()))
            .collect(),
    }
    .encode(Limits::default())
    .unwrap()
}
fn offer(
    env: &mut Environment,
    domain: Domain,
    now: u64,
    setup: Vec<SetupOrModify>,
    release: &[u8],
    connect: bool,
) -> PendingRabAssignment {
    let ranap = wire(setup, release);
    env.peer.rua(if connect {
        rua::Message::Connect {
            domain,
            context: env.context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap,
        }
    } else {
        rua::Message::Direct {
            domain,
            context: env.context,
            ranap,
        }
    });
    let mut handoff = RabHandoff::new(Limits::default());
    env.link
        .drive(ControlTime(now), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    handoff.confirm(env.link.gateway());
    handoff.take().unwrap().unwrap()
}
fn prepare_absent(env: &Environment, pending: PendingRabAssignment) -> RabReleaseRequest {
    RabReleaseRequest::prepare_checked(
        pending,
        None,
        identity(env),
        plan(false, false),
        &[] as &[crate::rel99_admission::ActiveRab],
        env.link.gateway(),
    )
    .unwrap()
}
fn disconnect(env: &mut Environment, domain: Domain, now: u64) {
    env.peer.rua(rua::Message::Disconnect {
        domain,
        context: env.context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    crate::core_link::tests::finish_peer_disconnect(&mut env.link, &mut env.radio, now);
}

#[test]
fn unanchored_domain_reports_unknown_ids_with_bounded_recording_and_retry() {
    let mut env = environment();
    let pending = offer(&mut env, Domain::Ps, 6, vec![], &[245, 246], true);
    let request = pending.request();
    let mut release = prepare_absent(&env, pending);
    assert!(!release.needs_radio());
    assert!(
        release
            .record_ready(&mut env.link, ControlTime(5), count(1))
            .is_err()
    );
    assert_eq!(
        release.progress(),
        ReleaseOutcomeProgress {
            items: 2,
            recorded: 0,
            superseded: 0
        }
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(7), count(1))
            .unwrap(),
        1
    );
    assert!(!release.outcomes_recorded());
    assert!(
        !env.link
            .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(1))
            .unwrap(),
        1
    );
    assert!(release.outcomes_recorded());
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(1))
            .unwrap(),
        0
    );
    env.peer.set_blocked(true);
    assert!(
        env.link
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    env.peer.set_blocked(false);
    env.link
        .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("canonical release response")
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, Limits::default())
        .unwrap()
        .0;
    assert_eq!(
        response
            .release_failed
            .iter()
            .map(|item| (item.id.get(), item.cause.code()))
            .collect::<Vec<_>>(),
        [(245, INVALID_RAB_ID), (246, INVALID_RAB_ID)]
    );
    assert!(!env.link.gateway().has_pending_rab_request(request));
}

#[test]
fn reopened_domain_uses_new_request_for_unknown_ids_but_not_known_old_rabs() {
    let mut env = environment();
    let old = offer(&mut env, Domain::Ps, 6, vec![setup(9)], &[], true).request();
    disconnect(&mut env, Domain::Ps, 7);
    let pending = offer(&mut env, Domain::Ps, 8, vec![], &[245], true);
    assert!(!pending.request().same_connection(old));
    let release = RabReleaseRequest::prepare_checked(
        pending,
        Some(old),
        identity(&env),
        plan(false, true),
        &fixture::rabs(false, true),
        env.link.gateway(),
    )
    .unwrap();
    assert!(!release.needs_radio());
    let pending = offer(&mut env, Domain::Ps, 9, vec![], &[9], false);
    let request = pending.request();
    let rejected = RabReleaseRequest::prepare_checked(
        pending,
        Some(old),
        identity(&env),
        plan(false, true),
        &fixture::rabs(false, true),
        env.link.gateway(),
    )
    .unwrap_err();
    assert!(matches!(rejected.error, CoreReleaseError::Identity));
    assert_eq!(rejected.pending.request(), request);
}

#[test]
fn incomplete_inventory_cannot_prove_requested_rab_absent() {
    let mut env = environment();
    let pending = offer(&mut env, Domain::Ps, 6, vec![], &[9], true);
    let rejected = RabReleaseRequest::prepare_checked(
        pending,
        None,
        identity(&env),
        plan(false, true),
        &[] as &[crate::rel99_admission::ActiveRab],
        env.link.gateway(),
    )
    .unwrap_err();
    assert!(matches!(rejected.error, CoreReleaseError::Mapping));
}

#[test]
fn superseded_items_settle_independently_without_replaying_recorded_results() {
    let mut env = environment();
    let pending = offer(&mut env, Domain::Ps, 6, vec![], &[245, 246], true);
    let mut release = prepare_absent(&env, pending);
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(7), count(1))
            .unwrap(),
        1
    );
    let _replacement = offer(&mut env, Domain::Ps, 8, vec![], &[246], false);
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(9), count(1))
            .unwrap(),
        0
    );
    assert_eq!(
        release.progress(),
        ReleaseOutcomeProgress {
            items: 2,
            recorded: 1,
            superseded: 1
        }
    );
    assert!(release.outcomes_recorded());
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(9), count(2))
            .unwrap(),
        0
    );
}

#[test]
fn disconnected_or_fully_superseded_request_reaches_terminal_disposition() {
    for disconnecting in [false, true] {
        let mut env = environment();
        let pending = offer(&mut env, Domain::Ps, 6, vec![], &[245], true);
        let mut release = prepare_absent(&env, pending);
        if disconnecting {
            disconnect(&mut env, Domain::Ps, 7)
        } else {
            let _replacement = offer(&mut env, Domain::Ps, 7, vec![], &[245], false);
        }
        assert_eq!(
            release
                .record_ready(&mut env.link, ControlTime(8), count(1))
                .unwrap(),
            0
        );
        assert_eq!(
            release.progress(),
            ReleaseOutcomeProgress {
                items: 1,
                recorded: 0,
                superseded: 1
            }
        );
        assert!(release.outcomes_recorded());
    }
}

#[test]
fn mixed_continuation_retains_original_authority_and_first_response_aggregation() {
    let mut env = environment();
    let pending = offer(&mut env, Domain::Ps, 6, vec![setup(10)], &[245], true);
    let request = pending.request();
    assert!(pending.setup_selection().is_none());
    let release = prepare_absent(&env, pending);
    let mut release = release
        .into_setup_after_release(env.link.gateway())
        .unwrap_err();
    release
        .record_ready(&mut env.link, ControlTime(7), count(1))
        .unwrap();
    assert!(
        !env.link
            .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let continuation = release
        .into_setup_after_release(env.link.gateway())
        .unwrap();
    assert_eq!(continuation.request(), request);
    assert_eq!(continuation.pending().assignment().release[0].id.get(), 245);
    assert_eq!(
        continuation.pending().assignment().setup[0].first.id.get(),
        10
    );
    let selected = SetupAssignment::after_release(&continuation);
    assert_eq!(selected.pending().request(), request);
    assert_eq!(
        selected.authorize_items(env.link.gateway()).unwrap().len(),
        1
    );
    assert!(
        continuation
            .pending()
            .authorize_items(env.link.gateway())
            .is_err()
    );
    env.link
        .gateway_mut()
        .record_rab_outcome(
            ControlTime(8),
            request,
            RabOutcome::SetupFailed(Failed::new(10, Cause::from_code(32).unwrap())),
        )
        .unwrap();
    assert!(
        env.link
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("one combined response")
    };
    let response = AssignmentResponse::decode(&ranap, Limits::default())
        .unwrap()
        .0;
    assert_eq!(response.failed[0].id.get(), 10);
    assert_eq!(response.release_failed[0].id.get(), 245);
    assert_eq!(continuation.into_pending().request(), request);
}

#[test]
fn revoked_setup_cannot_mint_a_continuation_and_returns_original_release_owner() {
    let mut env = environment();
    let pending = offer(&mut env, Domain::Ps, 6, vec![setup(10)], &[245], true);
    let mut release = prepare_absent(&env, pending);
    release
        .record_ready(&mut env.link, ControlTime(7), count(1))
        .unwrap();
    let request = release.request();
    let _replacement = offer(&mut env, Domain::Ps, 8, vec![], &[10], false);
    let rejected = release
        .into_setup_after_release(env.link.gateway())
        .unwrap_err();
    assert_eq!(rejected.request(), request);
    assert!(rejected.outcomes_recorded());
}

#[test]
fn fresh_selection_excludes_release_and_never_accepts_foreign_core_authority() {
    let mut env = environment();
    let pending = offer(&mut env, Domain::Ps, 6, vec![setup(10)], &[], true);
    let selected = pending.setup_selection().unwrap();
    assert_eq!(
        selected.authorize_items(env.link.gateway()).unwrap().len(),
        1
    );
    let foreign = environment();
    assert!(selected.authorize_items(foreign.link.gateway()).is_err());
    let pending = offer(&mut env, Domain::Ps, 7, vec![], &[245], false);
    let rejected = RabReleaseRequest::prepare_checked(
        pending,
        None,
        identity(&env),
        plan(false, false),
        &[] as &[crate::rel99_admission::ActiveRab],
        foreign.link.gateway(),
    )
    .unwrap_err();
    assert!(matches!(
        rejected.error,
        CoreReleaseError::Authority(_) | CoreReleaseError::Identity
    ));
}

#[test]
fn precommit_event_is_not_authority_and_original_owner_can_retry_after_commit() {
    let mut env = environment();
    env.peer.rua(rua::Message::Connect {
        domain: Domain::Ps,
        context: env.context,
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: wire(vec![], &[245]),
    });
    let mut handoff = RabHandoff::new(Limits::default());
    let previous = identity(&env);
    let configured = plan(false, false);
    assert!(
        env.link
            .drive(ControlTime(6), &mut env.radio, |_, event| {
                handoff.offer(event).unwrap();
                Err::<(), ()>(())
            })
            .is_err()
    );
    let pending = handoff.take().unwrap().unwrap();
    let rejected = RabReleaseRequest::prepare_checked(
        pending,
        None,
        previous,
        configured.clone(),
        &[] as &[crate::rel99_admission::ActiveRab],
        env.link.gateway(),
    )
    .unwrap_err();
    assert!(!rejected.pending.is_committed());
    let request = rejected.pending.request();
    env.link
        .drive(ControlTime(6), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let release = RabReleaseRequest::prepare_checked(
        *rejected.pending,
        None,
        previous,
        configured,
        &[] as &[crate::rel99_admission::ActiveRab],
        env.link.gateway(),
    )
    .unwrap();
    assert_eq!(release.request(), request);
    assert!(!release.needs_radio());
}

#[test]
fn superseded_known_radio_release_cannot_mint_a_setup_continuation() {
    let mut env = environment();
    let source = offer(&mut env, Domain::Cs, 6, vec![setup(7)], &[], false).request();
    let pending = offer(&mut env, Domain::Cs, 7, vec![setup(8)], &[7], false);
    let mut release = RabReleaseRequest::prepare(
        pending,
        source,
        identity(&env),
        plan(true, false),
        &fixture::rabs(true, false),
        env.link.gateway(),
    )
    .unwrap();
    assert!(release.needs_radio());
    let _replacement = offer(&mut env, Domain::Cs, 8, vec![], &[7], false);
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(9), count(1))
            .unwrap(),
        0
    );
    assert!(release.outcomes_recorded());
    assert_eq!(release.progress().superseded, 1);
    assert!(!release.authorized(env.link.gateway()));
    assert!(
        env.link
            .gateway()
            .authorize_rab(release.request(), 8, RabOperation::SetupOrModify)
            .is_ok()
    );
    let original = release
        .into_setup_after_release(env.link.gateway())
        .unwrap_err();
    assert!(original.needs_radio());
}

#[test]
fn empty_actual_admission_reports_unmapped_releases_without_any_radio_plan() {
    use crate::rel99_planner::tests::admission::{admission_limits, transmit};
    let mut env = environment();
    let (_runtime, ports) = transmit(&env._source);
    let admission = Rel99Admission::new(ports.control, count(4), admission_limits())
        .ok()
        .unwrap();
    let pending = offer(&mut env, Domain::Ps, 6, vec![], &[245, 246], true);
    let mut release = admission
        .prepare_unmapped_release(pending, env.link.gateway())
        .unwrap();
    assert!(release.radio_target().is_none());
    assert!(!release.needs_radio());
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(7), count(1))
            .unwrap(),
        1
    );
    assert!(
        !env.link
            .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(1))
            .unwrap(),
        1
    );
    assert!(
        env.link
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
        panic!("unmapped release response")
    };
    let response = AssignmentResponse::decode(&ranap, Limits::default())
        .unwrap()
        .0;
    assert_eq!(
        response
            .release_failed
            .iter()
            .map(|item| (item.id.get(), item.cause.code()))
            .collect::<Vec<_>>(),
        [(245, INVALID_RAB_ID), (246, INVALID_RAB_ID)]
    );
}

#[test]
fn unmapped_mixed_request_preserves_original_pending_until_initial_setup() {
    use crate::rel99_planner::tests::admission::{admission_limits, transmit};
    let mut env = environment();
    let (_runtime, ports) = transmit(&env._source);
    let admission = Rel99Admission::new(ports.control, count(4), admission_limits())
        .ok()
        .unwrap();
    let pending = offer(&mut env, Domain::Ps, 6, vec![setup(10)], &[245], true);
    let request = pending.request();
    let mut release = admission
        .prepare_unmapped_release(pending, env.link.gateway())
        .unwrap();
    assert!(release.radio_target().is_none());
    release
        .record_ready(&mut env.link, ControlTime(7), count(1))
        .unwrap();
    assert!(
        !env.link
            .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let continuation = release
        .into_setup_after_release(env.link.gateway())
        .unwrap();
    assert_eq!(continuation.request(), request);
    assert_eq!(
        SetupAssignment::after_release(&continuation)
            .authorize_items(env.link.gateway())
            .unwrap()
            .len(),
        1
    );
    assert_eq!(continuation.pending().assignment().release[0].id.get(), 245);
}

#[test]
fn mixed_release_continuations_reject_forbidden_ps_and_preserve_recorded_release_outcomes() {
    use crate::{
        rab_handoff::FailedRabAssignment,
        radio_resources::RadioResources,
        rel99_admission::{AdditionalLaunch, AdditionalRabHost, InitialRabHost, RabAdmissionError},
        rel99_planner::{
            ConfiguredPolicy,
            tests::admission::{admission_limits, receive, transmit},
        },
        test_support::at,
    };
    const RADIO_INTERFACE_FAILURE: u16 = 14;
    for initial in [false, true] {
        let mut env = environment();
        let (_runtime, ports) = transmit(&env._source);
        let mut admission = Rel99Admission::new(ports.control, count(4), admission_limits())
            .ok()
            .unwrap();
        let (_rx, mut receive) = receive(env.key.epoch, 4);
        let mut resources =
            RadioResources::new(env.key.epoch, env._source.information(), count(4), 0..=3).unwrap();
        let pending = offer(&mut env, Domain::Ps, 6, vec![setup(10)], &[245], true);
        let request = pending.request();
        let mut release = admission
            .prepare_unmapped_release(pending, env.link.gateway())
            .unwrap();
        assert_eq!(
            release
                .record_ready(&mut env.link, ControlTime(7), count(1))
                .unwrap(),
            1
        );
        let continuation = release
            .into_setup_after_release(env.link.gateway())
            .unwrap();
        let mut config: umts_config::Config = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../rust/test-support/fixtures/config/rel99.yaml"
        )))
        .unwrap();
        config.cell.packet_modes = Some(vec![umts_config::PacketMode::HspaPlus]);
        config.hspa.downlink.enabled = true;
        config.hspa.uplink.enabled = true;
        config.hspa.downlink.qam64_enabled = true;
        let policy = ConfiguredPolicy::new(&config).unwrap();
        assert!(policy.validate_rel99_packet_setup(CnDomain::Cs).is_ok());
        let charges = env.radio.resources();
        let rejected_pending = if initial {
            let rejected = InitialRabHost {
                gateway: env.link.gateway(),
                policy: &policy,
                radio: &mut env.radio,
                resources: &mut resources,
                receive: &receive,
                admission: &mut admission,
            }
            .begin_after_release_with_settings(continuation, |_| {
                panic!("forbidden setup must reject before constructing owners")
            })
            .unwrap_err();
            assert!(
                matches!(rejected.error, RabAdmissionError::Hsdpa(ref error) if error.contains("packet_modes"))
            );
            assert!(rejected.allocation.is_none() && rejected.rollback_error.is_none());
            rejected.pending
        } else {
            let previous = identity(&env);
            let rejected = AdditionalRabHost {
                gateway: env.link.gateway(),
                policy: &policy,
                radio: &mut env.radio,
                resources: &mut resources,
                receive: &mut receive,
                admission: &mut admission,
            }
            .begin_after_release_with_delivery(
                previous,
                continuation,
                AdditionalLaunch {
                    now: at(0),
                    threshold: at(38_400),
                    maximum_combinations: count(1024),
                },
                |_, _| panic!("forbidden setup must reject before constructing owners"),
            )
            .unwrap_err();
            assert!(
                matches!(rejected.error, RabAdmissionError::Hsdpa(ref error) if error.contains("packet_modes"))
            );
            assert!(rejected.change.is_none() && rejected.rollback_error.is_none());
            rejected.pending
        };
        assert_eq!(rejected_pending.request(), request);
        assert_eq!(resources.outstanding(), 0);
        assert_eq!(env.radio.resources(), charges);
        let mut failed = FailedRabAssignment::new(
            rejected_pending,
            Cause::from_code(RADIO_INTERFACE_FAILURE).unwrap(),
            env.link.gateway(),
        )
        .unwrap();
        failed
            .record(&mut env.link, ControlTime(8), count(2))
            .unwrap();
        assert!(failed.is_complete());
        assert!(
            env.link
                .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
                .unwrap()
                .rab_published
        );
        let rua::Message::Direct { ranap, .. } = env.peer.last_rua() else {
            panic!("expected mixed request disposition");
        };
        let response = AssignmentResponse::decode(&ranap, Limits::default())
            .unwrap()
            .0;
        assert!(response.setup.is_empty() && response.released.is_empty());
        assert_eq!(response.failed.len(), 1);
        assert_eq!(response.failed[0].id.get(), 10);
        assert_eq!(response.release_failed.len(), 1);
        assert_eq!(response.release_failed[0].id.get(), 245);
        assert_eq!(response.release_failed[0].cause.code(), INVALID_RAB_ID);
        assert!(!env.link.gateway().has_pending_rab_request(request));
    }
}

#[test]
fn unmapped_release_rejects_setup_only_and_foreign_authority_retaining_requests() {
    use crate::rel99_planner::tests::admission::{admission_limits, transmit};
    let mut env = environment();
    let (_runtime, ports) = transmit(&env._source);
    let admission = Rel99Admission::new(ports.control, count(4), admission_limits())
        .ok()
        .unwrap();
    let pending = offer(&mut env, Domain::Ps, 6, vec![setup(10)], &[], true);
    let request = pending.request();
    let rejected = admission
        .prepare_unmapped_release(pending, env.link.gateway())
        .unwrap_err();
    assert!(matches!(rejected.error, CoreReleaseError::Identity));
    assert_eq!(rejected.pending.request(), request);
    let foreign = environment();
    let pending = offer(&mut env, Domain::Ps, 7, vec![], &[245], false);
    let request = pending.request();
    let rejected = admission
        .prepare_unmapped_release(pending, foreign.link.gateway())
        .unwrap_err();
    assert_eq!(rejected.pending.request(), request);
    assert!(matches!(
        rejected.error,
        CoreReleaseError::Identity | CoreReleaseError::Authority(_)
    ));
}
