// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod aggregation;
mod registration_reuse;
mod ue_session;
use crate::ranap::rab::{
    Assignment, AssignmentResponse, Cause, Failed, First, Release, Released, Second, Setup,
    SetupOrModify,
};

fn command(slot: u32, domain: Domain, assignment: &Assignment) -> rua::Decoded {
    let message = rua::Message::Direct {
        context: ContextId::new(42 + slot).unwrap(),
        domain,
        ranap: assignment.encode(Limits::default()).unwrap(),
    };
    rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap()
}
fn setup(id: u8) -> Assignment {
    Assignment {
        setup: vec![SetupOrModify {
            first: First::modification(id),
            second: Second::default(),
        }],
        release: vec![],
    }
}
fn release(id: u8) -> Assignment {
    Assignment {
        setup: vec![],
        release: vec![Release::new(id, Cause::from_code(83).unwrap())],
    }
}
fn request(
    session: &mut Session,
    slot: u32,
    domain: Domain,
    assignment: &Assignment,
    now: u64,
) -> RabRequest {
    session
        .receive_rua_with(
            association(),
            ControlTime(now),
            &command(slot, domain, assignment),
            |event| {
                let Event::RabAssignment {
                    request,
                    received_at,
                    assignment: decoded,
                    ..
                } = event
                else {
                    panic!("not a RAB request")
                };
                assert_eq!(*received_at, ControlTime(now));
                assert_eq!(decoded, assignment);
                Ok::<_, ()>(*request)
            },
        )
        .unwrap()
}
fn accepted(id: u8) -> AssignmentResponse {
    AssignmentResponse {
        setup: vec![Setup::new(id)],
        ..Default::default()
    }
}
fn released(id: u8) -> AssignmentResponse {
    AssignmentResponse {
        released: vec![Released::new(id)],
        ..Default::default()
    }
}
fn live() -> Session {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 42);
    register(&mut session, 1, 43);
    for slot in 0..2 {
        for domain in [Domain::Cs, Domain::Ps] {
            send(&mut session, slot, domain);
        }
    }
    session
}

#[test]
fn authorization_tracks_session_item_kind_and_committed_outcomes() {
    let mut session = live();
    let token = request(&mut session, 0, Domain::Ps, &setup(5), 4);
    assert!(
        session
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_ok()
    );
    assert!(
        session
            .authorize_rab(token, 6, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(
        session
            .authorize_rab(token, 5, RabOperation::Release)
            .is_err()
    );
    assert!(
        session
            .publish_rab_with(token, &accepted(5), ControlTime(5), |_| Err::<(), _>(
                "full"
            ))
            .is_err()
    );
    assert!(
        session
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_ok()
    );
    session
        .publish_rab_with(token, &accepted(5), ControlTime(5), |_| Ok::<_, ()>(()))
        .unwrap();
    assert!(
        session
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    let replacement = request(&mut session, 0, Domain::Ps, &setup(5), 6);
    assert!(token.same_connection(replacement));
    assert_ne!(token, replacement);
    let released = request(&mut session, 0, Domain::Ps, &release(5), 7);
    assert!(
        session
            .authorize_rab(replacement, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(
        session
            .authorize_rab(released, 5, RabOperation::Release)
            .is_ok()
    );
}

#[test]
fn identical_external_identifiers_do_not_authorize_another_session() {
    let mut first = live();
    let mut second = live();
    let a = request(&mut first, 0, Domain::Ps, &setup(5), 4);
    let b = request(&mut second, 0, Domain::Ps, &setup(5), 4);
    assert_eq!(a.association(), b.association());
    assert_eq!(a.context(), b.context());
    assert_eq!(a.key(), b.key());
    assert_eq!(a.serial(), b.serial());
    assert!(!a.same_connection(b));
    assert!(
        first
            .authorize_rab(b, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(
        second
            .authorize_rab(a, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(
        first
            .authorize_rab(a, 5, RabOperation::SetupOrModify)
            .is_ok()
    );
    assert!(
        second
            .authorize_rab(b, 5, RabOperation::SetupOrModify)
            .is_ok()
    );
}

#[test]
fn request_and_response_backpressure_preserve_exact_token_and_domain() {
    let mut session = live();
    let assignment = setup(5);
    let command = command(0, Domain::Cs, &assignment);
    let mut rejected = None;
    assert_eq!(
        session.receive_rua_with(association(), ControlTime(4), &command, |event| {
            let Event::RabAssignment { request, .. } = event else {
                panic!("not a RAB request")
            };
            rejected = Some(*request);
            Err::<(), _>("full")
        }),
        Err(PublishError::Downstream("full"))
    );
    let token = request(&mut session, 0, Domain::Cs, &assignment, 4);
    assert_eq!(Some(token), rejected);
    let response = accepted(5);
    assert_eq!(
        session.publish_rab_with(token, &response, ControlTime(5), |_| Err::<(), _>("full")),
        Err(PublishError::Downstream("full"))
    );
    let frame = session
        .publish_rab_with(token, &response, ControlTime(5), Ok::<_, ()>)
        .unwrap();
    assert_eq!(
        frame.scope,
        FrameScope::Ue {
            key: key(0),
            domain: Some(Domain::Cs)
        }
    );
    let rua::Message::Direct {
        context,
        domain,
        ranap,
    } = rua::Message::decode(&frame.data, Limits::default())
        .unwrap()
        .message
    else {
        panic!("not RUA direct")
    };
    assert_eq!(context, ContextId::new(42).unwrap());
    assert_eq!(domain, Domain::Cs);
    assert_eq!(
        AssignmentResponse::decode(&ranap, Limits::default())
            .unwrap()
            .0,
        response
    );
    assert_eq!(
        session.publish_rab_with(token, &response, ControlTime(6), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Origin))
    );
}

#[test]
fn release_supersedes_only_matching_bearer_after_publication_commits() {
    let mut session = live();
    let cs = request(&mut session, 0, Domain::Cs, &setup(5), 4);
    let ps = request(&mut session, 0, Domain::Ps, &setup(5), 4);
    let peer = request(&mut session, 1, Domain::Cs, &setup(5), 4);
    let release_command = command(0, Domain::Cs, &release(5));
    assert_eq!(
        session.receive_rua_with(association(), ControlTime(5), &release_command, |_| Err::<
            (),
            _,
        >(
            "full"
        )),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(
        session.publish_rab_with(cs, &accepted(5), ControlTime(5), |_| Err::<(), _>("probe")),
        Err(PublishError::Downstream("probe"))
    );
    let release_token = request(&mut session, 0, Domain::Cs, &release(5), 5);
    assert_eq!(
        session.publish_rab_with(cs, &accepted(5), ControlTime(6), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Origin))
    );
    session
        .publish_rab_with(release_token, &released(5), ControlTime(6), |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    session
        .publish_rab_with(ps, &accepted(5), ControlTime(6), |_| Ok::<_, ()>(()))
        .unwrap();
    session
        .publish_rab_with(peer, &accepted(5), ControlTime(6), |_| Ok::<_, ()>(()))
        .unwrap();
    let replacement = request(&mut session, 0, Domain::Cs, &setup(5), 7);
    let superseded = AssignmentResponse {
        failed: vec![Failed::new(5, Cause::from_code(39).unwrap())],
        ..Default::default()
    };
    assert_eq!(
        session.publish_rab_with(cs, &superseded, ControlTime(8), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Origin))
    );
    session
        .publish_rab_with(replacement, &accepted(5), ControlTime(8), |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn responses_cover_every_requested_bearer_and_keep_operation_kind() {
    let mut session = live();
    let mut assignment = setup(5);
    assignment.release = release(6).release;
    let token = request(&mut session, 0, Domain::Cs, &assignment, 4);
    for response in [
        accepted(5),
        released(6),
        AssignmentResponse {
            setup: vec![Setup::new(6)],
            released: vec![Released::new(5)],
            ..Default::default()
        },
    ] {
        assert!(
            session
                .publish_rab_with(token, &response, ControlTime(5), |_| Ok::<_, ()>(()))
                .is_err()
        );
    }
    let response = AssignmentResponse {
        setup: vec![Setup::new(5)],
        release_failed: vec![Failed::new(6, Cause::from_code(30).unwrap())],
        ..Default::default()
    };
    session
        .publish_rab_with(token, &response, ControlTime(5), |_| Ok::<_, ()>(()))
        .unwrap();
}

#[test]
fn pending_assignments_are_bounded_and_disconnect_invalidates_only_its_domain() {
    let mut session = live();
    let cs = request(&mut session, 0, Domain::Cs, &setup(1), 4);
    let ps = request(&mut session, 0, Domain::Ps, &setup(1), 4);
    for id in 2..=4 {
        request(&mut session, 0, Domain::Cs, &setup(id), 4);
    }
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(5),
            &command(0, Domain::Cs, &setup(5)),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::Capacity))
    );
    let disconnect = rua::Decoded {
        notify_ies: vec![],
        message: rua::Message::Disconnect {
            context: ContextId::new(42).unwrap(),
            domain: Domain::Cs,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        },
    };
    session
        .receive_rua_with(association(), ControlTime(5), &disconnect, |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        session.publish_rab_with(cs, &accepted(1), ControlTime(6), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::State))
    );
    session
        .publish_rab_with(ps, &accepted(1), ControlTime(6), |_| Ok::<_, ()>(()))
        .unwrap();
}

#[test]
fn connectionless_assignment_cannot_escape_scoped_ownership() {
    let mut session = live();
    let message = rua::Message::Connectionless {
        ranap: setup(5).encode(Limits::default()).unwrap(),
    };
    let decoded = rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        session.receive_rua_with(association(), ControlTime(4), &decoded, |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::State))
    );
}

#[test]
fn connectionless_common_id_cannot_escape_scoped_ownership() {
    let mut session = live();
    let message = rua::Message::Connectionless {
        ranap: ranap::common_identity(
            Imsi::from_digits("001010000000001").unwrap(),
            Limits::default(),
        )
        .unwrap(),
    };
    let decoded = rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(4),
            &decoded,
            |_| -> Result<(), ()> { panic!("connectionless Common ID must not be published") }
        ),
        Err(PublishError::Session(Error::State))
    );
}

#[test]
fn setup_saturation_cannot_consume_the_bounded_release_reservation() {
    let mut session = live();
    let first = request(&mut session, 0, Domain::Cs, &setup(1), 4);
    for id in 2..=4 {
        request(&mut session, 0, Domain::Cs, &setup(id), 4);
    }
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(5),
            &command(0, Domain::Cs, &setup(5)),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::Capacity))
    );
    let mut mixed = setup(4);
    mixed.release = release(1).release;
    let release_first = request(&mut session, 0, Domain::Cs, &mixed, 5);
    let release_second = request(&mut session, 0, Domain::Cs, &release(2), 5);
    assert_eq!(
        session.ues[&key(0)].rab_requests[domain_index(Domain::Cs)].len(),
        4
    );
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(6),
            &command(0, Domain::Cs, &release(3)),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::Capacity))
    );
    assert_eq!(
        session.publish_rab_with(first, &accepted(1), ControlTime(6), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Origin))
    );
    session
        .publish_rab_with(release_second, &released(2), ControlTime(6), |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    request(&mut session, 0, Domain::Cs, &release(3), 7);
    let response = AssignmentResponse {
        failed: vec![Failed::new(4, Cause::from_code(32).unwrap())],
        released: vec![Released::new(1)],
        ..Default::default()
    };
    session
        .publish_rab_with(
            release_first,
            &response,
            ControlTime(7),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
}

#[test]
fn cancellation_keeps_unrelated_items_and_reclaims_prior_release_capacity() {
    let mut session = live();
    let mut old = setup(1);
    old.setup.extend(setup(2).setup);
    let old_token = request(&mut session, 0, Domain::Cs, &old, 4);
    let first_release = request(&mut session, 0, Domain::Cs, &release(1), 5);
    let peer_release = request(&mut session, 0, Domain::Cs, &release(3), 5);
    let replacement_release = request(&mut session, 0, Domain::Cs, &release(1), 6);
    assert_eq!(
        session.publish_rab_with(
            first_release,
            &released(1),
            ControlTime(6),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::Origin))
    );
    assert_eq!(
        session.publish_rab_with(old_token, &accepted(1), ControlTime(6), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Identity))
    );
    session
        .publish_rab_with(old_token, &accepted(2), ControlTime(6), |_| Ok::<_, ()>(()))
        .unwrap();
    session
        .publish_rab_with(replacement_release, &released(1), ControlTime(7), |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    let replacement_setup = request(&mut session, 0, Domain::Cs, &setup(1), 8);
    session
        .publish_rab_with(replacement_setup, &accepted(1), ControlTime(8), |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    session
        .publish_rab_with(peer_release, &released(3), ControlTime(8), |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
}
