// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    core_link::{
        CoreLink,
        tests::{Mock, Peer, ready, register},
    },
    signalling::{CommonSignalling, tests::controller},
    test_support::source,
};
use umts_iuh::{
    ContextId, Domain,
    ranap::rab::{Cause, First, Release, Second, SetupOrModify},
    rua,
    wire::{Criticality, InformationElement, Packet, Protocol},
};

fn setup(id: u8) -> SetupOrModify {
    SetupOrModify {
        first: First::modification(id),
        second: Second::default(),
    }
}

fn connected() -> (CoreLink<Mock>, CommonSignalling, Peer, ContextId) {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, peer) = ready(&mut radio);
    let (_, context, _) = register(&mut link, &peer, &mut radio, &source, 0, 3);
    (link, radio, peer, context)
}

fn send(peer: &Peer, context: ContextId, assignment: &Assignment) {
    peer.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: assignment.encode(Limits::default()).unwrap(),
    });
}

fn send_notified(peer: &Peer, context: ContextId, id: u8, rua_notify: u16, ranap_notify: u16) {
    let assignment = Assignment {
        setup: vec![setup(id)],
        release: vec![],
    };
    let mut ranap = Packet::decode(
        Protocol::Ranap,
        &assignment.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    ranap
        .ies
        .push(InformationElement::new(ranap_notify, Criticality::Notify, &true).unwrap());
    let rua = rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap.encode(Limits::default()).unwrap(),
    };
    let mut packet = Packet::decode(
        Protocol::Rua,
        &rua.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    packet
        .ies
        .push(InformationElement::new(rua_notify, Criticality::Notify, &true).unwrap());
    peer.rua_packet(packet.encode(Limits::default()).unwrap());
}

fn drive(
    link: &mut CoreLink<Mock>,
    radio: &mut CommonSignalling,
    handoff: &mut RabHandoff,
    now: u64,
) {
    link.drive(ControlTime(now), radio, |_, event| {
        handoff.offer(event).map(|_| ())
    })
    .unwrap();
}

fn pending() -> (
    CoreLink<Mock>,
    CommonSignalling,
    Peer,
    ContextId,
    PendingRabAssignment,
) {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff = RabHandoff::new(Limits::default());
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(5), setup(6)],
            release: vec![],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 6);
    let pending = handoff.take().unwrap().unwrap();
    (link, radio, peer, context, pending)
}

#[test]
fn authority_is_unavailable_before_callback_commit_and_valid_after_drive() {
    let (mut link, mut radio, peer, context) = connected();
    let assignment = Assignment {
        setup: vec![setup(5)],
        release: vec![],
    };
    send(&peer, context, &assignment);
    let mut handoff = RabHandoff::new(Limits::default());
    let result = link.drive(ControlTime(6), &mut radio, |_, event| {
        handoff.offer(event).unwrap();
        Err::<(), _>("uncommitted diagnostic")
    });
    assert!(result.is_err());
    assert_eq!(handoff.confirm(link.gateway()), 0);
    assert!(
        handoff
            .take_committed_ready(|_| panic!("uncommitted request reached predicate"))
            .unwrap()
            .is_none()
    );
    let mut before = handoff.take().unwrap().unwrap();
    assert!(!before.is_committed());
    assert!(!before.confirm(link.gateway()));
    assert!(before.authorize_items(link.gateway()).is_err());
    drive(&mut link, &mut radio, &mut handoff, 7);
    assert_eq!(handoff.confirm(link.gateway()), 1);
    assert_eq!(handoff.confirm(link.gateway()), 0);
    let after = handoff.take().unwrap().unwrap();
    assert!(after.is_committed());
    assert_eq!(before.request, after.request);
    assert_eq!(after.assignment, assignment);
    let proofs = after.authorize_items(link.gateway()).unwrap();
    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].request(), after.request);
    assert_eq!(proofs[0].rab(), 5);
    assert_eq!(proofs[0].operation(), RabOperation::SetupOrModify);
    assert!(!handoff.is_pending());
}

#[test]
fn raw_take_stays_unconfirmed_until_exact_committed_request_is_witnessed() {
    let (link, _, _, _, mut pending) = pending();
    assert!(!pending.is_committed());
    assert!(pending.confirm(link.gateway()));
    assert!(pending.is_committed());
    assert!(pending.confirm(link.gateway()));
}

#[test]
fn queued_confirmation_marks_only_the_matching_core_owner() {
    let (mut link, mut radio, peer, context) = connected();
    let (foreign, _, _, _) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    for (now, id) in [(6, 5), (7, 6)] {
        send(
            &peer,
            context,
            &Assignment {
                setup: vec![setup(id)],
                release: vec![],
            },
        );
        drive(&mut link, &mut radio, &mut handoff, now);
    }
    assert_eq!(handoff.confirm(foreign.gateway()), 0);
    assert_eq!(handoff.confirm(link.gateway()), 2);
    assert_eq!(handoff.confirm(link.gateway()), 0);
    let first = handoff.take().unwrap().unwrap();
    let second = handoff.take().unwrap().unwrap();
    assert!(first.is_committed());
    assert!(second.is_committed());
    assert_ne!(first.request(), second.request());
}

#[test]
fn committed_selection_skips_unconfirmed_request_without_calling_its_readiness_predicate() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(5)],
            release: vec![],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 6);
    let mut calls = 0;
    assert!(
        handoff
            .take_committed_ready(|_| {
                calls += 1;
                true
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(calls, 0);

    send(
        &peer,
        context,
        &Assignment {
            setup: vec![],
            release: vec![Release::new(5, Cause::from_code(32).unwrap())],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 7);
    assert_eq!(handoff.confirm(link.gateway()), 1);
    let release = handoff
        .take_committed_ready(|_| {
            calls += 1;
            true
        })
        .unwrap()
        .unwrap();
    assert_eq!(calls, 1);
    assert!(release.is_committed());
    assert_eq!(release.assignment().release.len(), 1);
    release.authorize_items(link.gateway()).unwrap();

    assert!(
        handoff
            .take_committed_ready(|_| panic!("unconfirmed request reached predicate"))
            .unwrap()
            .is_none()
    );
    let original = handoff.take().unwrap().unwrap();
    assert!(!original.is_committed());
    assert_eq!(original.assignment().setup[0].first.id.get(), 5);
    assert!(!handoff.is_pending());
}

#[test]
fn assignment_kind_is_derived_from_setup_and_release_presence() {
    assert_eq!(
        AssignmentKind::from_assignment(&Assignment {
            setup: vec![setup(5)],
            release: vec![],
        }),
        AssignmentKind::SetupOnly
    );
    assert_eq!(
        AssignmentKind::from_assignment(&Assignment {
            setup: vec![],
            release: vec![Release::new(5, Cause::from_code(32).unwrap())],
        }),
        AssignmentKind::ReleaseOnly
    );
    assert_eq!(
        AssignmentKind::from_assignment(&Assignment {
            setup: vec![setup(6)],
            release: vec![Release::new(5, Cause::from_code(32).unwrap())],
        }),
        AssignmentKind::Mixed
    );
    assert_eq!(
        AssignmentKind::from_assignment(&Assignment {
            setup: vec![],
            release: vec![],
        }),
        AssignmentKind::Empty
    );
}

#[test]
fn committed_matching_filters_kind_without_removing_deferred_owner() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(5)],
            release: vec![],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 6);
    assert_eq!(handoff.confirm(link.gateway()), 1);
    assert!(
        handoff
            .take_committed_matching(|_, kind| kind == AssignmentKind::ReleaseOnly)
            .unwrap()
            .is_none()
    );
    assert_eq!(handoff.pending.len(), 1);

    send(
        &peer,
        context,
        &Assignment {
            setup: vec![],
            release: vec![Release::new(6, Cause::from_code(32).unwrap())],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 7);
    assert_eq!(handoff.confirm(link.gateway()), 1);
    let release = handoff
        .take_committed_matching(|_, kind| kind == AssignmentKind::ReleaseOnly)
        .unwrap()
        .unwrap();
    assert_eq!(release.kind(), AssignmentKind::ReleaseOnly);
    let setup = handoff.take().unwrap().unwrap();
    assert_eq!(setup.kind(), AssignmentKind::SetupOnly);
}

#[test]
fn committed_matching_scans_bounded_queue_and_selects_later_peer() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    for (now, id) in [(6, 5), (7, 6)] {
        send(
            &peer,
            context,
            &Assignment {
                setup: vec![setup(id)],
                release: vec![],
            },
        );
        drive(&mut link, &mut radio, &mut handoff, now);
    }
    let first = handoff.pending.front().unwrap().request;
    let second = handoff.pending.back().unwrap().request;
    assert_ne!(first, second);
    assert_eq!(handoff.confirm(link.gateway()), 2);
    let selected = handoff
        .take_committed_matching(|request, kind| {
            kind == AssignmentKind::SetupOnly && request == second
        })
        .unwrap()
        .unwrap();
    assert_eq!(selected.request(), second);
    let deferred = handoff.take().unwrap().unwrap();
    assert_eq!(deferred.request(), first);
}

#[test]
fn kind_mismatch_does_not_decode_or_remove_owner() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff = RabHandoff::new(Limits::default());
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(5)],
            release: vec![],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 6);
    assert_eq!(handoff.confirm(link.gateway()), 1);
    handoff.pending.front_mut().unwrap().wire = vec![0xff];
    assert!(
        handoff
            .take_committed_matching(|_, kind| kind == AssignmentKind::ReleaseOnly)
            .unwrap()
            .is_none()
    );
    assert!(handoff.is_pending());
}

#[test]
fn committed_stale_request_remains_transferable_and_readiness_can_select_later_peer() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(5)],
            release: vec![],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 6);
    assert_eq!(handoff.confirm(link.gateway()), 1);
    let original = handoff.pending.front().unwrap().request;
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![],
            release: vec![Release::new(5, Cause::from_code(32).unwrap())],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 7);
    assert!(!link.gateway().has_pending_rab_request(original));
    assert_eq!(handoff.confirm(link.gateway()), 1);
    let release = handoff
        .take_committed_ready(|request| request != original)
        .unwrap()
        .unwrap();
    assert!(release.is_committed());
    release.authorize_items(link.gateway()).unwrap();
    let stale = handoff
        .take_committed_ready(|request| request == original)
        .unwrap()
        .unwrap();
    assert!(stale.is_committed());
    assert_eq!(stale.request(), original);
    assert!(stale.authorize_items(link.gateway()).is_err());
    assert!(!handoff.is_pending());
}

#[test]
fn busy_handoff_preserves_first_assignment_and_retries_the_core_event() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff = RabHandoff::new(Limits::default());
    let first = Assignment {
        setup: vec![setup(5)],
        release: vec![],
    };
    let second = Assignment {
        setup: vec![setup(6)],
        release: vec![],
    };
    send(&peer, context, &first);
    drive(&mut link, &mut radio, &mut handoff, 6);
    send(&peer, context, &second);
    assert!(
        link.drive(ControlTime(7), &mut radio, |_, event| handoff
            .offer(event)
            .map(|_| ()))
            .is_err()
    );
    let retained = handoff.take().unwrap().unwrap();
    assert_eq!(retained.assignment, first);
    retained.authorize_items(link.gateway()).unwrap();
    drive(&mut link, &mut radio, &mut handoff, 8);
    let retried = handoff.take().unwrap().unwrap();
    assert_eq!(retried.assignment, second);
    assert_ne!(retained.request, retried.request);
    retried.authorize_items(link.gateway()).unwrap();
}

#[test]
fn exact_metadata_notify_order_and_mixed_operation_proofs_are_preserved() {
    let (mut link, mut radio, peer, context) = connected();
    let assignment = Assignment {
        setup: vec![setup(6)],
        release: vec![Release::new(5, Cause::from_code(32).unwrap())],
    };
    send(&peer, context, &assignment);
    let mut handoff = RabHandoff::new(Limits::default());
    drive(&mut link, &mut radio, &mut handoff, 6);
    let pending = handoff.take().unwrap().unwrap();
    let event = Event::RabAssignment {
        request: pending.request,
        received_at: pending.received_at,
        assignment: pending.assignment.clone(),
        notify_ies: vec![91, 42, 91],
    };
    assert!(handoff.offer(&event).unwrap());
    assert!(matches!(handoff.offer(&event), Err(HandoffError::Busy)));
    let actual = handoff.take().unwrap().unwrap();
    assert_eq!(actual.request, pending.request);
    assert_eq!(actual.received_at, pending.received_at);
    assert_eq!(actual.assignment, assignment);
    assert_eq!(actual.notify_ies, [91, 42, 91]);
    let proofs = actual.authorize_items(link.gateway()).unwrap();
    assert_eq!(
        proofs
            .iter()
            .map(|proof| (proof.rab(), proof.operation()))
            .collect::<Vec<_>>(),
        [(6, RabOperation::SetupOrModify), (5, RabOperation::Release)]
    );
}

#[test]
fn failed_budgets_or_malformed_offer_do_not_occupy_the_slot() {
    let (_, _, _, _, pending) = pending();
    let event = Event::RabAssignment {
        request: pending.request,
        received_at: pending.received_at,
        assignment: pending.assignment.clone(),
        notify_ies: vec![1, 2],
    };
    for limits in [
        Limits {
            maximum_ies: 1,
            ..Limits::default()
        },
        Limits {
            maximum_packet_bytes: 1,
            ..Limits::default()
        },
        Limits {
            maximum_value_bytes: 1,
            ..Limits::default()
        },
        Limits {
            maximum_ies: 0,
            ..Limits::default()
        },
    ] {
        let mut handoff = RabHandoff::new(limits);
        assert!(handoff.offer(&event).is_err());
        assert!(!handoff.is_pending());
        assert!(handoff.take().unwrap().is_none());
    }
    let malformed = Event::RabAssignment {
        request: pending.request,
        received_at: pending.received_at,
        assignment: Assignment {
            setup: vec![setup(5), setup(5)],
            release: vec![],
        },
        notify_ies: vec![],
    };
    let mut handoff = RabHandoff::new(Limits::default());
    assert!(handoff.offer(&malformed).is_err());
    assert!(!handoff.is_pending());
    assert!(!handoff.offer(&Event::HnbAccepted { rnc: 1 }).unwrap());
    assert!(handoff.offer(&event).unwrap());
    assert!(!handoff.offer(&Event::HnbAccepted { rnc: 1 }).unwrap());
    assert!(handoff.is_pending());
}

#[test]
fn release_supersedes_only_matching_setup_and_old_connection_cannot_regain_authority() {
    let (mut link, mut radio, peer, context, old) = pending();
    let mut handoff = RabHandoff::new(Limits::default());
    let release = Assignment {
        setup: vec![],
        release: vec![Release::new(5, Cause::from_code(32).unwrap())],
    };
    send(&peer, context, &release);
    drive(&mut link, &mut radio, &mut handoff, 7);
    let release = handoff.take().unwrap().unwrap();
    assert!(old.authorize_items(link.gateway()).is_err());
    link.gateway()
        .authorize_rab(old.request, 6, RabOperation::SetupOrModify)
        .unwrap();
    let proof = release.authorize_items(link.gateway()).unwrap();
    assert_eq!(proof[0].operation(), RabOperation::Release);
    drop(proof);
    peer.rua(rua::Message::Disconnect {
        domain: Domain::Cs,
        context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    crate::core_link::tests::finish_peer_disconnect(&mut link, &mut radio, 8);
    let replacement = Assignment {
        setup: vec![setup(5)],
        release: vec![],
    };
    peer.rua(rua::Message::Connect {
        domain: Domain::Cs,
        context,
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: replacement.encode(Limits::default()).unwrap(),
    });
    drive(&mut link, &mut radio, &mut handoff, 9);
    let replacement = handoff.take().unwrap().unwrap();
    replacement.authorize_items(link.gateway()).unwrap();
    assert!(old.authorize_items(link.gateway()).is_err());
    assert!(release.authorize_items(link.gateway()).is_err());
    assert!(!old.request.same_connection(replacement.request));
}

#[test]
fn codec_produced_rua_envelopes_keep_protocol_provenance_through_busy_retry() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff = RabHandoff::new(Limits::default());
    send_notified(&peer, context, 5, 61_000, 62_000);
    drive(&mut link, &mut radio, &mut handoff, 6);
    send_notified(&peer, context, 6, 61_001, 62_001);
    assert!(
        link.drive(ControlTime(7), &mut radio, |_, event| handoff
            .offer(event)
            .map(|_| ()))
            .is_err()
    );
    let first = handoff.take().unwrap().unwrap();
    assert_eq!(first.notify_ies(), [62_000]);
    assert_eq!(first.outer_notifications().len(), 1);
    assert_eq!(first.outer_notifications()[0].protocol(), Protocol::Rua);
    assert_eq!(first.outer_notifications()[0].ids(), [61_000]);
    assert_eq!(first.assignment().setup[0].first.id.get(), 5);
    first.authorize_items(link.gateway()).unwrap();
    drive(&mut link, &mut radio, &mut handoff, 8);
    let second = handoff.take().unwrap().unwrap();
    assert_eq!(second.notify_ies(), [62_001]);
    assert_eq!(second.outer_notifications()[0].protocol(), Protocol::Rua);
    assert_eq!(second.outer_notifications()[0].ids(), [61_001]);
    assert_eq!(second.assignment().setup[0].first.id.get(), 6);
    assert_ne!(first.request(), second.request());
    second.authorize_items(link.gateway()).unwrap();
}

#[test]
fn notifications_and_rab_ids_share_the_retained_byte_budget() {
    let (_, _, _, _, pending) = pending();
    let assignment_bytes = pending.assignment.encode(Limits::default()).unwrap().len();
    let event = Event::Notified {
        protocol: Protocol::Rua,
        notify_ies: vec![61_000, 61_001],
        event: Box::new(Event::RabAssignment {
            request: pending.request,
            received_at: pending.received_at,
            assignment: pending.assignment.clone(),
            notify_ies: vec![62_000, 62_001],
        }),
    };
    let required = assignment_bytes
        + pending.assignment.setup.len()
        + 4 * NOTIFY_ID_BYTES
        + size_of::<ProtocolNotifications>();
    for limits in [
        Limits {
            maximum_ies: 3,
            ..Limits::default()
        },
        Limits {
            maximum_packet_bytes: required - 1,
            ..Limits::default()
        },
    ] {
        let mut handoff = RabHandoff::new(limits);
        assert!(matches!(handoff.offer(&event), Err(HandoffError::Capacity)));
        assert!(!handoff.is_pending());
    }
    let mut handoff = RabHandoff::new(Limits {
        maximum_packet_bytes: required,
        ..Limits::default()
    });
    assert!(handoff.offer(&event).unwrap());
    let actual = handoff.take().unwrap().unwrap();
    assert_eq!(actual.notify_ies(), [62_000, 62_001]);
    assert_eq!(actual.outer_notifications()[0].ids(), [61_000, 61_001]);
}

#[test]
fn bounded_queue_can_defer_one_request_without_losing_other_authority() {
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(5)],
            release: vec![],
        },
    );
    drive(&mut link, &mut radio, &mut handoff, 6);
    let first = handoff.pending.front().unwrap().request;
    send_notified(&peer, context, 6, 61_001, 62_001);
    drive(&mut link, &mut radio, &mut handoff, 7);
    let second = handoff.pending.back().unwrap().request;
    assert_ne!(first, second);
    let mut visits = 0;
    assert!(
        handoff
            .take_ready(|_| {
                visits += 1;
                false
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(visits, 2);
    assert_eq!(handoff.pending.len(), 2);
    assert_eq!(handoff.confirm(link.gateway()), 2);
    let ready = handoff
        .take_committed_matching_rabs(|request, kind, rabs| {
            assert_eq!(kind, AssignmentKind::SetupOnly);
            assert_eq!(rabs, if request == second { &[6] } else { &[5] });
            request == second
        })
        .unwrap()
        .unwrap();
    ready.authorize_items(link.gateway()).unwrap();
    assert_eq!(ready.assignment().setup[0].first.id.get(), 6);
    assert_eq!(ready.notify_ies(), [62_001]);
    assert_eq!(ready.outer_notifications()[0].ids(), [61_001]);
    let deferred = handoff.take().unwrap().unwrap();
    assert_eq!(deferred.request(), first);
    deferred.authorize_items(link.gateway()).unwrap();
    assert!(!handoff.is_pending());
}

#[test]
fn bounded_queue_rejects_storage_overflow_and_preserves_full_queue() {
    assert!(matches!(
        RabHandoff::with_capacity(
            Limits {
                maximum_packet_bytes: usize::MAX,
                ..Limits::default()
            },
            NonZeroUsize::new(2).unwrap(),
        ),
        Err(HandoffError::Capacity)
    ));
    let (mut link, mut radio, peer, context) = connected();
    let mut handoff =
        RabHandoff::with_capacity(Limits::default(), NonZeroUsize::new(2).unwrap()).unwrap();
    for (now, id) in [(6, 5), (7, 6)] {
        send(
            &peer,
            context,
            &Assignment {
                setup: vec![setup(id)],
                release: vec![],
            },
        );
        drive(&mut link, &mut radio, &mut handoff, now);
    }
    send(
        &peer,
        context,
        &Assignment {
            setup: vec![setup(7)],
            release: vec![],
        },
    );
    assert!(
        link.drive(ControlTime(8), &mut radio, |_, event| handoff
            .offer(event)
            .map(|_| ()))
            .is_err()
    );
    assert_eq!(
        handoff.take().unwrap().unwrap().assignment().setup[0]
            .first
            .id
            .get(),
        5
    );
    drive(&mut link, &mut radio, &mut handoff, 9);
    assert_eq!(
        handoff.take().unwrap().unwrap().assignment().setup[0]
            .first
            .id
            .get(),
        6
    );
    assert_eq!(
        handoff.take().unwrap().unwrap().assignment().setup[0]
            .first
            .id
            .get(),
        7
    );
}
