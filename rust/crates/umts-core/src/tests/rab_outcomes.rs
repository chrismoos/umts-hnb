// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_iuh::{
    ranap::rab::{
        Assignment, AssignmentResponse, Cause, Failed, First, Release, Released, Second, Setup,
        SetupOrModify,
    },
    session::{RabOperation, RabOutcome, RabRequest},
};

fn connected(entries: usize) -> (Gateway<Mock>, Peer, ContextId) {
    let (mut gateway, peer) = ready(entries);
    let context = register(&mut gateway, &peer, 0, 3);
    nas(&mut gateway, 0, Domain::Cs, 6).unwrap();
    drive(&mut gateway, 7);
    nas(&mut gateway, 0, Domain::Ps, 8).unwrap();
    drive(&mut gateway, 9);
    (gateway, peer, context)
}

fn setup(id: u8) -> SetupOrModify {
    SetupOrModify {
        first: First::modification(id),
        second: Second::default(),
    }
}

fn request(
    gateway: &mut Gateway<Mock>,
    peer: &Peer,
    context: ContextId,
    domain: Domain,
    assignment: &Assignment,
    at: u64,
) -> RabRequest {
    peer.rua(rua::Message::Direct {
        domain,
        context,
        ranap: assignment.encode(CodecLimits::default()).unwrap(),
    });
    let events = drive(gateway, at);
    let [
        Event::RabAssignment {
            request,
            assignment: actual,
            ..
        },
    ] = events.as_slice()
    else {
        panic!("expected RAB assignment")
    };
    assert_eq!(actual, assignment);
    *request
}

fn response(peer: &Peer, context: ContextId, domain: Domain) -> AssignmentResponse {
    let (_, ppid, bytes) = peer.last();
    assert_eq!(ppid, Protocol::Rua.ppid().unwrap());
    let rua::Message::Direct {
        context: actual_context,
        domain: actual_domain,
        ranap,
    } = rua::Message::decode(&bytes, CodecLimits::default())
        .unwrap()
        .message
    else {
        panic!("expected RUA direct response")
    };
    assert_eq!((actual_context, actual_domain), (context, domain));
    AssignmentResponse::decode(&ranap, CodecLimits::default())
        .unwrap()
        .0
}

#[test]
fn outcome_commit_is_authorized_before_side_effects_and_failed_commit_is_retryable() {
    let (mut gateway, peer, context) = connected(3);
    let assignment = Assignment {
        setup: vec![setup(5)],
        release: vec![],
    };
    let token = request(&mut gateway, &peer, context, Domain::Ps, &assignment, 10);
    let outcome = RabOutcome::Setup(Setup::new(5));
    assert!(matches!(
        gateway.record_rab_outcome_with(time(11), token, outcome.clone(), |authority| {
            assert_eq!(authority.request(), token);
            assert_eq!(authority.rab(), 5);
            assert_eq!(authority.operation(), RabOperation::SetupOrModify);
            Err::<(), _>("not committed")
        }),
        Err(RabCommitError::Commit("not committed"))
    ));
    assert!(
        gateway
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_ok()
    );
    assert!(!gateway.publish_ready_rab(time(11)).unwrap());
    assert_eq!(
        gateway
            .record_rab_outcome_with(time(12), token, outcome.clone(), |_| Ok::<_, ()>(7))
            .unwrap(),
        7
    );
    assert!(matches!(
        gateway.record_rab_outcome_with(time(12), token, outcome, |_| -> Result<(), ()> {
            panic!("duplicate outcome must not commit")
        }),
        Err(RabCommitError::Core(_))
    ));
    assert!(gateway.publish_ready_rab(time(12)).unwrap());
}

#[test]
fn gateway_aggregates_every_item_before_enqueuing_one_complete_response() {
    let (mut gateway, peer, context) = connected(3);
    let assignment = Assignment {
        setup: vec![setup(1), setup(2)],
        release: vec![Release::new(3, Cause::from_code(83).unwrap())],
    };
    let token = request(&mut gateway, &peer, context, Domain::Ps, &assignment, 10);
    let failed = Failed::new(2, Cause::from_code(32).unwrap());
    gateway
        .record_rab_outcome(time(11), token, RabOutcome::SetupFailed(failed.clone()))
        .unwrap();
    gateway
        .record_rab_outcome(time(11), token, RabOutcome::Released(Released::new(3)))
        .unwrap();
    let sent = peer.0.lock().unwrap().sent.len();
    assert!(!gateway.publish_ready_rab(time(11)).unwrap());
    assert_eq!(gateway.queued_frames(), 0);
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent);
    gateway
        .record_rab_outcome(time(12), token, RabOutcome::Setup(Setup::new(1)))
        .unwrap();
    assert!(gateway.publish_ready_rab(time(12)).unwrap());
    assert_eq!(gateway.queued_frames(), 1);
    assert!(
        gateway
            .authorize_rab(token, 1, RabOperation::SetupOrModify)
            .is_err()
    );
    assert!(!gateway.publish_ready_rab(time(12)).unwrap());
    drive(&mut gateway, 13);
    assert_eq!(
        response(&peer, context, Domain::Ps),
        AssignmentResponse {
            setup: vec![Setup::new(1)],
            failed: vec![failed],
            released: vec![Released::new(3)],
            ..Default::default()
        }
    );
    assert_eq!(peer.0.lock().unwrap().sent.len(), sent + 1);
}

#[test]
fn full_outbox_retains_completed_outcomes_until_response_admission_succeeds() {
    let (mut gateway, peer, context) = connected(3);
    let token = request(
        &mut gateway,
        &peer,
        context,
        Domain::Cs,
        &Assignment {
            setup: vec![setup(5)],
            release: vec![],
        },
        10,
    );
    peer.block(true);
    for _ in 0..3 {
        nas(&mut gateway, 0, Domain::Cs, 11).unwrap();
    }
    gateway
        .record_rab_outcome(time(12), token, RabOutcome::Setup(Setup::new(5)))
        .unwrap();
    for _ in 0..2 {
        assert_eq!(gateway.publish_ready_rab(time(12)), Err(Error::Capacity));
        assert_eq!(gateway.queued_frames(), 3);
        assert!(
            gateway
                .authorize_rab(token, 5, RabOperation::SetupOrModify)
                .is_err()
        );
    }
    peer.block(false);
    drive(&mut gateway, 13);
    assert_eq!(gateway.queued_frames(), 2);
    assert!(gateway.publish_ready_rab(time(13)).unwrap());
    assert_eq!(gateway.queued_frames(), 3);
    assert!(
        gateway
            .authorize_rab(token, 5, RabOperation::SetupOrModify)
            .is_err()
    );
    for at in 14..=16 {
        drive(&mut gateway, at);
    }
    assert_eq!(
        response(&peer, context, Domain::Cs),
        AssignmentResponse {
            setup: vec![Setup::new(5)],
            ..Default::default()
        }
    );
    assert!(!gateway.publish_ready_rab(time(17)).unwrap());
}

#[test]
fn incoming_release_removes_only_its_matching_staged_old_outcome() {
    let (mut gateway, peer, context) = connected(3);
    let old = request(
        &mut gateway,
        &peer,
        context,
        Domain::Ps,
        &Assignment {
            setup: vec![setup(1), setup(2)],
            release: vec![],
        },
        10,
    );
    gateway
        .record_rab_outcome(time(11), old, RabOutcome::Setup(Setup::new(1)))
        .unwrap();
    let releasing = request(
        &mut gateway,
        &peer,
        context,
        Domain::Ps,
        &Assignment {
            setup: vec![],
            release: vec![Release::new(1, Cause::from_code(83).unwrap())],
        },
        12,
    );
    assert!(
        gateway
            .record_rab_outcome(time(13), old, RabOutcome::Setup(Setup::new(1)))
            .is_err()
    );
    gateway
        .record_rab_outcome(time(13), old, RabOutcome::Setup(Setup::new(2)))
        .unwrap();
    assert!(gateway.publish_ready_rab(time(13)).unwrap());
    drive(&mut gateway, 14);
    assert_eq!(
        response(&peer, context, Domain::Ps),
        AssignmentResponse {
            setup: vec![Setup::new(2)],
            ..Default::default()
        }
    );
    gateway
        .record_rab_outcome(time(15), releasing, RabOutcome::Released(Released::new(1)))
        .unwrap();
    assert!(gateway.publish_ready_rab(time(15)).unwrap());
    drive(&mut gateway, 16);
    assert_eq!(
        response(&peer, context, Domain::Ps),
        AssignmentResponse {
            released: vec![Released::new(1)],
            ..Default::default()
        }
    );
}

#[test]
fn connection_lifetime_outlasts_request_consumption_but_not_disconnect_or_loss() {
    let (mut gateway, peer, context) = connected(3);
    let assignment = Assignment {
        setup: vec![setup(5)],
        release: vec![],
    };
    let cs = request(&mut gateway, &peer, context, Domain::Cs, &assignment, 10);
    let ps = request(&mut gateway, &peer, context, Domain::Ps, &assignment, 11);
    for token in [cs, ps] {
        assert!(gateway.rab_connection_current(token));
        gateway
            .record_rab_outcome(time(12), token, RabOutcome::Setup(Setup::new(5)))
            .unwrap();
    }
    for _ in 0..2 {
        assert!(gateway.publish_ready_rab(time(12)).unwrap());
    }
    for token in [cs, ps] {
        assert!(gateway.rab_connection_current(token));
        assert!(
            gateway
                .authorize_rab(token, 5, RabOperation::SetupOrModify)
                .is_err()
        );
    }
    drive(&mut gateway, 13);
    drive(&mut gateway, 14);
    peer.rua(rua::Message::Disconnect {
        context,
        domain: Domain::Cs,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    let events = drive(&mut gateway, 15);
    let [
        Event::Disconnected {
            request: release, ..
        },
    ] = events.as_slice()
    else {
        panic!("owned disconnect required")
    };
    assert!(!gateway.rab_connection_current(cs));
    assert!(gateway.rab_connection_current(ps));
    assert!(nas(&mut gateway, 0, Domain::Cs, 16).is_err());
    gateway
        .complete_iu_release_without_reports(time(16), *release)
        .unwrap();
    nas(&mut gateway, 0, Domain::Cs, 16).unwrap();
    drive(&mut gateway, 16);
    assert!(!gateway.rab_connection_current(cs));
    assert!(gateway.rab_connection_current(ps));
    let replacement = request(&mut gateway, &peer, context, Domain::Cs, &assignment, 17);
    assert!(!cs.same_connection(replacement));
    assert!(gateway.rab_connection_current(replacement));
    peer.put(Input::Closed);
    drive(&mut gateway, 18);
    assert!(!gateway.rab_connection_current(cs));
    assert!(!gateway.rab_connection_current(ps));
}
