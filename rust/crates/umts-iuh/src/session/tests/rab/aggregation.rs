// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::session::RabOutcome;

#[test]
fn iu_release_cancels_ready_assignment_without_blocking_the_other_ue() {
    let mut session = live();
    let stale = request(&mut session, 0, Domain::Cs, &setup(5), 4);
    let other = request(&mut session, 1, Domain::Cs, &setup(6), 4);
    for (request, id) in [(stale, 5), (other, 6)] {
        session
            .record_rab_outcome(
                request,
                RabOutcome::SetupFailed(Failed::new(id, Cause::from_code(32).unwrap())),
            )
            .unwrap();
    }
    let command = crate::ranap::release::Command {
        cause: Cause::from_code(83).unwrap(),
    };
    session
        .receive_rua_with(
            association(),
            ControlTime(5),
            &rua::Decoded {
                notify_ies: vec![],
                message: rua::Message::Direct {
                    context: stale.context(),
                    domain: Domain::Cs,
                    ranap: command.encode(Limits::default()).unwrap(),
                },
            },
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(session.ues[&key(0)].rab_requests[domain_index(Domain::Cs)].is_empty());
    assert!(
        session
            .publish_ready_rab_with(ControlTime(6), Ok::<_, ()>)
            .unwrap()
            .is_some()
    );
    assert!(
        session
            .publish_ready_rab_with(ControlTime(6), Ok::<_, ()>)
            .unwrap()
            .is_none()
    );
}

fn close_cs(session: &mut Session) {
    let request = session
        .receive_rua_with(
            association(),
            ControlTime(5),
            &rua::Decoded {
                notify_ies: vec![],
                message: rua::Message::Disconnect {
                    context: ContextId::new(42).unwrap(),
                    domain: Domain::Cs,
                    cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
                    ranap: None,
                },
            },
            |event| {
                let Event::Disconnected { request, .. } = event else {
                    panic!("disconnect owner required")
                };
                Ok::<_, ()>(*request)
            },
        )
        .unwrap();
    session
        .complete_peer_disconnect(request, ControlTime(5))
        .unwrap();
}

#[test]
fn rejected_nas_reopen_does_not_advance_connection_generation_or_wrap_it() {
    let mut session = live();
    close_cs(&mut session);
    let before = session.ues.get(&key(0)).unwrap().connections;
    let next = session.next_connection;
    let nas = UplinkNas {
        key: key(0),
        domain: Domain::Cs,
        nas: &[5, 0x24],
        selector: None,
        cause: rua::EstablishmentCause::Normal,
    };
    assert!(
        session
            .publish_nas_with(nas, ControlTime(6), |_| Err::<(), _>("full"))
            .is_err()
    );
    assert_eq!(session.ues.get(&key(0)).unwrap().connections, before);
    assert_eq!(session.next_connection, next);
    session.next_connection = None;
    let nas = UplinkNas {
        key: key(0),
        domain: Domain::Cs,
        nas: &[5, 0x24],
        selector: None,
        cause: rua::EstablishmentCause::Normal,
    };
    assert_eq!(
        session.publish_nas_with(nas, ControlTime(6), |_| -> Result<(), ()> {
            panic!("exhausted incarnation published")
        }),
        Err(PublishError::Session(Error::Capacity))
    );
}

#[test]
fn inbound_connect_commits_one_new_connection_generation_only_after_acceptance() {
    let mut session = live();
    let old = request(&mut session, 0, Domain::Cs, &setup(5), 4);
    close_cs(&mut session);
    let message = rua::Message::Connect {
        domain: Domain::Cs,
        context: ContextId::new(42).unwrap(),
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: setup(5).encode(Limits::default()).unwrap(),
    };
    let decoded = rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    let mut attempted = None;
    assert!(
        session
            .receive_rua_with(association(), ControlTime(6), &decoded, |event| {
                let Event::RabAssignment { request, .. } = event else {
                    panic!("expected RAB")
                };
                attempted = Some(*request);
                Err::<(), _>("full")
            })
            .is_err()
    );
    assert!(session.check_rab_connection(old).is_err());
    let fresh = session
        .receive_rua_with(association(), ControlTime(6), &decoded, |event| {
            let Event::RabAssignment { request, .. } = event else {
                panic!("expected RAB")
            };
            Ok::<_, ()>(*request)
        })
        .unwrap();
    assert_eq!(Some(fresh), attempted);
    assert!(!old.same_connection(fresh));
    assert!(session.check_rab_connection(old).is_err());
    assert!(session.check_rab_connection(fresh).is_ok());
}

#[test]
fn a_direct_response_cannot_overwrite_a_recorded_final_outcome() {
    let mut session = live();
    let token = request(&mut session, 0, Domain::Ps, &setup(5), 4);
    let failed = Failed::new(5, Cause::from_code(32).unwrap());
    session
        .record_rab_outcome(token, RabOutcome::SetupFailed(failed.clone()))
        .unwrap();
    assert!(
        session
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(
        session
            .publish_rab_with(token, &accepted(5), ControlTime(5), |_| -> Result<(), ()> {
                panic!("contradictory outcome published")
            })
            .is_err()
    );
    let frame = session
        .publish_ready_rab_with(ControlTime(5), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    assert_eq!(decoded(&frame, 0, Domain::Ps).failed, [failed]);
}

fn decoded(frame: &Frame, slot: u32, domain: Domain) -> AssignmentResponse {
    assert_eq!(frame.association, association());
    assert_eq!(
        frame.scope,
        FrameScope::Ue {
            key: key(slot),
            domain: Some(domain),
        }
    );
    let rua::Message::Direct {
        context,
        domain: actual_domain,
        ranap,
    } = rua::Message::decode(&frame.data, Limits::default())
        .unwrap()
        .message
    else {
        panic!("not RUA direct")
    };
    assert_eq!(context, ContextId::new(42 + slot).unwrap());
    assert_eq!(actual_domain, domain);
    AssignmentResponse::decode(&ranap, Limits::default())
        .unwrap()
        .0
}

#[test]
fn mixed_outcomes_publish_only_when_every_requested_item_is_answered() {
    let mut session = live();
    let mut assignment = setup(1);
    assignment.setup.extend(setup(2).setup);
    assignment.release.extend(release(3).release);
    assignment.release.extend(release(4).release);
    let token = request(&mut session, 0, Domain::Ps, &assignment, 4);
    let setup_failure = Failed::new(2, Cause::from_code(32).unwrap());
    let release_failure = Failed::new(4, Cause::from_code(30).unwrap());
    for outcome in [
        RabOutcome::ReleaseFailed(release_failure.clone()),
        RabOutcome::SetupFailed(setup_failure.clone()),
        RabOutcome::Released(Released::new(3)),
    ] {
        session.record_rab_outcome(token, outcome).unwrap();
        assert!(
            session
                .publish_ready_rab_with(ControlTime(5), |_| -> Result<(), ()> {
                    panic!("partial first response")
                })
                .unwrap()
                .is_none()
        );
    }
    session
        .record_rab_outcome(token, RabOutcome::Setup(Setup::new(1)))
        .unwrap();
    let frame = session
        .publish_ready_rab_with(ControlTime(5), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    assert_eq!(
        decoded(&frame, 0, Domain::Ps),
        AssignmentResponse {
            setup: vec![Setup::new(1)],
            failed: vec![setup_failure],
            released: vec![Released::new(3)],
            release_failed: vec![release_failure],
            ..Default::default()
        }
    );
    assert!(
        session
            .authorize_rab(token, 1, RabOperation::SetupOrModify)
            .is_err()
    );
}

#[test]
fn out_of_order_setup_results_are_retained_without_early_publication() {
    let mut session = live();
    let mut assignment = setup(1);
    assignment.setup.extend(setup(2).setup);
    assignment.setup.extend(setup(3).setup);
    let token = request(&mut session, 0, Domain::Cs, &assignment, 4);
    for id in [3, 1] {
        session
            .record_rab_outcome(token, RabOutcome::Setup(Setup::new(id)))
            .unwrap();
    }
    assert_eq!(
        session.publish_ready_rab_with(ControlTime(5), |_| Ok::<_, ()>(())),
        Ok(None)
    );
    session
        .record_rab_outcome(token, RabOutcome::Setup(Setup::new(2)))
        .unwrap();
    let frame = session
        .publish_ready_rab_with(ControlTime(5), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    let response = decoded(&frame, 0, Domain::Cs);
    assert_eq!(response.setup.len(), 3);
    for id in [1, 2, 3] {
        assert!(response.setup.contains(&Setup::new(id)));
    }
    assert!(response.failed.is_empty());
    assert!(response.released.is_empty());
    assert!(response.release_failed.is_empty());
    assert!(response.queued.is_empty());
}

#[test]
fn recording_rejects_duplicate_wrong_operation_unknown_id_and_foreign_owner() {
    let mut session = live();
    let mut foreign = live();
    let mut assignment = setup(1);
    assignment.release = release(2).release;
    let token = request(&mut session, 0, Domain::Ps, &assignment, 4);
    let foreign_token = request(&mut foreign, 0, Domain::Ps, &assignment, 4);
    for (request, outcome) in [
        (foreign_token, RabOutcome::Setup(Setup::new(1))),
        (token, RabOutcome::Released(Released::new(1))),
        (token, RabOutcome::Setup(Setup::new(2))),
        (token, RabOutcome::Setup(Setup::new(3))),
        (
            token,
            RabOutcome::ReleaseFailed(Failed::new(1, Cause::from_code(30).unwrap())),
        ),
        (
            token,
            RabOutcome::SetupFailed(Failed::new(2, Cause::from_code(32).unwrap())),
        ),
    ] {
        assert!(session.record_rab_outcome(request, outcome).is_err());
    }
    session
        .record_rab_outcome(token, RabOutcome::Setup(Setup::new(1)))
        .unwrap();
    assert!(
        session
            .record_rab_outcome(token, RabOutcome::Setup(Setup::new(1)))
            .is_err()
    );
    assert!(
        session
            .record_rab_outcome(
                token,
                RabOutcome::SetupFailed(Failed::new(1, Cause::from_code(32).unwrap())),
            )
            .is_err()
    );
    session
        .record_rab_outcome(token, RabOutcome::Released(Released::new(2)))
        .unwrap();
    let frame = session
        .publish_ready_rab_with(ControlTime(5), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    let response = decoded(&frame, 0, Domain::Ps);
    assert_eq!(response.setup, vec![Setup::new(1)]);
    assert_eq!(response.released, vec![Released::new(2)]);
    assert!(
        session
            .record_rab_outcome(token, RabOutcome::Setup(Setup::new(1)))
            .is_err()
    );
}

#[test]
fn downstream_backpressure_preserves_outcomes_and_identical_response_bytes() {
    let mut session = live();
    let token = request(&mut session, 0, Domain::Cs, &setup(5), 4);
    session
        .record_rab_outcome(token, RabOutcome::Setup(Setup::new(5)))
        .unwrap();
    let mut attempted = None;
    assert_eq!(
        session.publish_ready_rab_with(ControlTime(5), |frame| {
            attempted = Some(frame);
            Err::<(), _>("full")
        }),
        Err(PublishError::Downstream("full"))
    );
    assert!(
        session
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    let frame = session
        .publish_ready_rab_with(ControlTime(5), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    assert_eq!(Some(frame.clone()), attempted);
    assert_eq!(decoded(&frame, 0, Domain::Cs), accepted(5));
    assert_eq!(
        session.publish_ready_rab_with(ControlTime(6), |_| Ok::<_, ()>(())),
        Ok(None)
    );
}

#[test]
fn release_discards_only_matching_recorded_outcome_before_id_reuse() {
    let mut session = live();
    let mut assignment = setup(1);
    assignment.setup.extend(setup(2).setup);
    let old = request(&mut session, 0, Domain::Ps, &assignment, 4);
    session
        .record_rab_outcome(old, RabOutcome::Setup(Setup::new(1)))
        .unwrap();
    let releasing = request(&mut session, 0, Domain::Ps, &release(1), 5);
    assert!(
        session
            .record_rab_outcome(old, RabOutcome::Setup(Setup::new(1)))
            .is_err()
    );
    session
        .record_rab_outcome(old, RabOutcome::Setup(Setup::new(2)))
        .unwrap();
    let frame = session
        .publish_ready_rab_with(ControlTime(6), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    assert_eq!(decoded(&frame, 0, Domain::Ps), accepted(2));
    session
        .record_rab_outcome(releasing, RabOutcome::Released(Released::new(1)))
        .unwrap();
    let frame = session
        .publish_ready_rab_with(ControlTime(6), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    assert_eq!(decoded(&frame, 0, Domain::Ps), released(1));
    let replacement = request(&mut session, 0, Domain::Ps, &setup(1), 7);
    assert_ne!(old, replacement);
    assert!(
        session
            .record_rab_outcome(old, RabOutcome::Setup(Setup::new(1)))
            .is_err()
    );
    session
        .record_rab_outcome(replacement, RabOutcome::Setup(Setup::new(1)))
        .unwrap();
    let frame = session
        .publish_ready_rab_with(ControlTime(8), Ok::<_, ()>)
        .unwrap()
        .unwrap();
    assert_eq!(decoded(&frame, 0, Domain::Ps), accepted(1));
}

#[test]
fn each_publication_emits_one_ready_request_on_its_original_context_and_domain() {
    let mut session = live();
    let first = request(&mut session, 0, Domain::Cs, &setup(5), 4);
    let second = request(&mut session, 1, Domain::Ps, &setup(5), 4);
    for token in [second, first] {
        session
            .record_rab_outcome(token, RabOutcome::Setup(Setup::new(5)))
            .unwrap();
    }
    let mut frames = Vec::new();
    for _ in 0..2 {
        let mut calls = 0;
        let frame = session
            .publish_ready_rab_with(ControlTime(5), |frame| {
                calls += 1;
                Ok::<_, ()>(frame)
            })
            .unwrap()
            .unwrap();
        assert_eq!(calls, 1);
        frames.push(frame);
    }
    assert_ne!(frames[0].scope, frames[1].scope);
    for frame in frames {
        let (slot, domain) = match frame.scope {
            FrameScope::Ue {
                key: actual,
                domain: Some(Domain::Cs),
            } if actual == key(0) => (0, Domain::Cs),
            FrameScope::Ue {
                key: actual,
                domain: Some(Domain::Ps),
            } if actual == key(1) => (1, Domain::Ps),
            _ => panic!("wrong response owner"),
        };
        assert_eq!(decoded(&frame, slot, domain), accepted(5));
    }
    assert_eq!(
        session.publish_ready_rab_with(ControlTime(6), |_| Ok::<_, ()>(())),
        Ok(None)
    );
}
