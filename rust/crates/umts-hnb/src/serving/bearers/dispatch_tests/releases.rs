// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_iuh::ranap::rab::{AssignmentResponse, Cause, Release};

const UNKNOWN_RAB: u8 = 9;
const RELEASE_CAUSE: u16 = 32;
const INVALID_RAB_ID: u16 = 30;

fn offer(
    env: &mut Environment,
    assignments: &mut RabHandoff,
    context: ContextId,
    mixed: bool,
) -> umts_iuh::session::RabRequest {
    env.with_parts(|core, radio, _, _, gateway, _| {
        gateway.rua(rua::Message::Connect {
            domain: Domain::Ps,
            context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap: Assignment {
                setup: if mixed {
                    let mut setup = ps_request(PACKET_RATE);
                    setup.first.id = umts_iuh::ranap::rab::RabId::new(UNKNOWN_RAB + 1);
                    vec![setup]
                } else {
                    Vec::new()
                },
                release: vec![Release::new(
                    UNKNOWN_RAB,
                    Cause::from_code(RELEASE_CAUSE).unwrap(),
                )],
            }
            .encode(Limits::default())
            .unwrap(),
        });
        let mut request = None;
        core.drive(ControlTime(6), radio, |_, event| {
            if let umts_iuh::session::Event::RabAssignment {
                request: received, ..
            } = event
            {
                request = Some(*received);
            }
            assignments.offer(event).map(|_| ())
        })
        .unwrap();
        assert_eq!(assignments.confirm(core.gateway()), 1);
        request.expect("actual core request")
    })
}

fn completed_owner(mixed: bool) -> crate::rel99_admission::RabReleaseRequest {
    let mut env = environment();
    let (_, context) = register(&mut env);
    let mut assignments = RabHandoff::new(Limits::default());
    let request = offer(&mut env, &mut assignments, context, mixed);
    let pending = assignments.take().unwrap().unwrap();
    env.with_parts(|core, _, _, _, _, admission| {
        let mut owner = admission
            .prepare_unmapped_release(pending, core.gateway())
            .unwrap();
        assert_eq!(owner.request(), request);
        assert!(!owner.needs_radio());
        assert_eq!(
            owner.record_ready(core, ControlTime(7), count(1)).unwrap(),
            1
        );
        assert!(owner.outcomes_recorded());
        owner
    })
}

#[test]
fn reserved_release_completion_slot_cannot_be_overwritten_by_outcome_only_owner() {
    use crate::serving::bearers::releases::PendingReleases;
    let scheduled = completed_owner(false);
    let other = completed_owner(false);
    assert_ne!(scheduled.request(), other.request());
    let mut releases = PendingReleases::new(count(1)).unwrap();
    releases.reserve(0, scheduled.request());
    assert_eq!(releases.pending(), 1);
    assert_eq!(releases.vacant(), None);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| releases.insert(0, other)))
            .is_err()
    );
    assert_eq!(releases.pending(), 1);
    assert_eq!(releases.vacant(), None);
    releases.complete(scheduled);
    assert_eq!(releases.pending(), 0);
    assert_eq!(releases.vacant(), Some(0));
}

#[test]
fn pure_completion_frees_only_exact_request_reservation_and_preserves_peer_slot() {
    use crate::serving::bearers::releases::PendingReleases;
    let first = completed_owner(false);
    let second = completed_owner(false);
    let mut releases = PendingReleases::new(count(2)).unwrap();
    releases.reserve(0, first.request());
    releases.reserve(1, second.request());
    releases.complete(second);
    assert_eq!(releases.pending(), 1);
    assert_eq!(releases.vacant(), Some(1));
    let third = completed_owner(false);
    releases.insert(1, third);
    assert_eq!(releases.vacant(), None);
    releases.complete(first);
    assert_eq!(releases.pending(), 1);
    assert_eq!(releases.vacant(), Some(0));
}

#[test]
fn mixed_completion_retains_original_request_in_its_reserved_slot() {
    use crate::serving::bearers::releases::PendingReleases;
    let mixed = completed_owner(true);
    let peer = completed_owner(false);
    assert!(mixed.has_setup());
    let request = mixed.request();
    let mut releases = PendingReleases::new(count(2)).unwrap();
    releases.reserve(0, request);
    releases.reserve(1, peer.request());
    releases.complete(mixed);
    assert_eq!(releases.pending(), 2);
    assert_eq!(releases.vacant(), None);
    releases.complete(peer);
    assert_eq!(releases.pending(), 1);
    assert_eq!(releases.vacant(), Some(1));
}

#[test]
fn disconnected_release_bypasses_full_completion_reservations_without_consuming_them() {
    let mut env = environment();
    let (key, context) = register(&mut env);
    let config = config();
    let mut plan = BearerCoordinator::new(
        &config,
        key.epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let mut traffic = traffic(&config);
    plan.assignments = RabHandoff::new(Limits::default());
    let request = offer(&mut env, &mut plan.assignments, context, false);
    let mut reserved = 0;
    while let Some(slot) = plan.releases.vacant() {
        plan.releases
            .reserve(slot, completed_owner(false).request());
        reserved += 1;
    }
    assert!(reserved > 0);
    env.with_parts(|core, radio, _, _, gateway, _| {
        gateway.rua(rua::Message::Disconnect {
            domain: Domain::Ps,
            context,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        });
        core.drive(ControlTime(7), radio, |_, _| Ok::<_, ()>(()))
            .unwrap();
        assert!(!core.gateway().rab_connection_current(request));
    });
    let report = service(&mut plan, &mut env, &mut traffic);
    assert_eq!((report.accepted, report.rejected), (0, 1));
    assert!(!plan.assignments.is_pending());
    assert_eq!(plan.releases.pending(), reserved);
    assert_eq!(plan.releases.vacant(), None);
    assert_eq!(plan.outstanding(), 0);
}

#[test]
fn automatic_unknown_release_needs_no_dedicated_radio_or_capability_exchange() {
    let mut env = environment();
    let (key, context) = register(&mut env);
    let config = config();
    let mut plan = BearerCoordinator::new(
        &config,
        key.epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let mut traffic = traffic(&config);
    plan.assignments = RabHandoff::new(Limits::default());
    offer(&mut env, &mut plan.assignments, context, false);
    let report = service(&mut plan, &mut env, &mut traffic);
    assert_eq!((report.accepted, report.rejected), (1, 0));
    assert!(report.failure.is_none());
    assert!(!plan.assignments.is_pending());
    for _ in 0..config.runtime.maximum_ues * 2 {
        let report = service(&mut plan, &mut env, &mut traffic);
        assert!(report.failure.is_none());
    }
    assert_eq!((plan.outstanding(), plan.pending_cleanups()), (0, 0));
    env.with_parts(|core, radio, _, _, gateway, _| {
        assert_eq!(radio.dedicated_route(key), None);
        assert!(radio.capabilities(key).is_none());
        assert!(
            core.drive(ControlTime(11), radio, |_, _| Ok::<_, ()>(()))
                .unwrap()
                .rab_published
        );
        let rua::Message::Direct {
            domain,
            context: actual,
            ranap,
        } = gateway.last_rua()
        else {
            panic!("missing canonical unknown-release response");
        };
        assert_eq!((domain, actual), (Domain::Ps, context));
        let response = AssignmentResponse::decode(&ranap, Limits::default())
            .unwrap()
            .0;
        assert_eq!(response.release_failed.len(), 1);
        assert_eq!(response.release_failed[0].id.get(), UNKNOWN_RAB);
        assert_eq!(response.release_failed[0].cause.code(), INVALID_RAB_ID);
        assert!(
            response.setup.is_empty() && response.released.is_empty() && response.failed.is_empty()
        );
    });
}

#[test]
fn automatic_unmapped_mixed_request_preserves_one_owner_until_initial_setup() {
    let mut env = environment();
    let (key, context) = register(&mut env);
    let mut config = config();
    adapt_signalling(&mut config, &mut env, key);
    let mut plan = BearerCoordinator::new(
        &config,
        key.epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let mut traffic = traffic(&config);
    plan.assignments = RabHandoff::new(Limits::default());
    let request = offer(&mut env, &mut plan.assignments, context, true);
    let report = service(&mut plan, &mut env, &mut traffic);
    assert_eq!((report.accepted, report.rejected), (1, 0));
    assert!(report.failure.is_none());
    for _ in 0..config.runtime.maximum_ues * 2 {
        let report = service(&mut plan, &mut env, &mut traffic);
        assert!(report.failure.is_none());
    }
    assert_eq!((plan.outstanding(), plan.pending_cleanups()), (0, 1));
    env.with_parts(|core, radio, _, source, _, _| {
        assert!(
            !core
                .drive(ControlTime(10), radio, |_, _| Ok::<_, ()>(()))
                .unwrap()
                .rab_published
        );
        assert!(core.gateway().has_pending_rab_request(request));
        let CapabilityReadiness::Pending(transaction) =
            radio.ensure_capabilities(key, at(0)).unwrap()
        else {
            panic!("missing actual mixed-request capability transaction");
        };
        let mut uplink = peer(source);
        uplink.enqueue(&COMPLETE).unwrap();
        uplink
            .publish_next(at(0), |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        send_capability_report_with_transaction(radio, key, &mut uplink, transaction);
        assert_eq!(radio.process_one(), Some(Ok(key)));
    });
    let mut accepted = 0;
    for _ in 0..config.runtime.maximum_ues * 2 {
        let report = service(&mut plan, &mut env, &mut traffic);
        assert!(report.failure.is_none(), "{:?}", report.failure);
        assert_eq!(report.rejected, 0);
        accepted += report.accepted;
    }
    assert_eq!(accepted, 1);
    assert_eq!((plan.outstanding(), plan.pending_cleanups()), (1, 0));
    env.with_parts(|core, radio, _, _, _, admission| {
        assert_eq!(
            admission.visit_rab_assignments(count(2), |owner, identity| {
                assert_eq!(owner.rab_assignment(identity).unwrap().request(), request);
                assert_eq!(
                    owner
                        .rab_assignment(identity)
                        .unwrap()
                        .assignment()
                        .release
                        .len(),
                    1
                );
                assert_eq!(owner.packet_progress(identity).unwrap().prepared, 1);
                assert_eq!(owner.phase(identity), Some(AdmissionPhase::Preparing));
            }),
            1
        );
        assert!(core.gateway().has_pending_rab_request(request));
        assert_eq!(radio.dedicated_route(key), None);
    });
}
