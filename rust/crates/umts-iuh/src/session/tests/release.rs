// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn malformed_final_ranap_does_not_prevent_peer_disconnect() {
    let mut session = both_domains();
    let decoded = rua::Decoded {
        notify_ies: vec![],
        message: rua::Message::Disconnect {
            context: ContextId::new(42).unwrap(),
            domain: Domain::Cs,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: Some(vec![0xff]),
        },
    };
    let request = session
        .receive_rua_with(association(), ControlTime(5), &decoded, |event| {
            let Event::Disconnected {
                request,
                final_ranap: None,
                ..
            } = event
            else {
                panic!("disconnect not delivered");
            };
            Ok::<_, ()>(*request)
        })
        .unwrap();
    assert!(session.peer_disconnected_release(request));
    session
        .publish_nas_with(
            nas(1, Domain::Cs, &[1]),
            ControlTime(6),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    session
        .complete_peer_disconnect(request, ControlTime(7))
        .unwrap();
}

#[test]
fn peer_disconnect_preserves_an_admitted_release_owner_and_other_connections() {
    let mut session = both_domains();
    let request = admit(&mut session, Domain::Cs, 4);
    let peer_connections = session.ues[&key(1)].connections;
    let disconnect = rua::Decoded {
        message: rua::Message::Disconnect {
            context: request.context(),
            domain: Domain::Cs,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        },
        notify_ies: vec![],
    };
    session
        .receive_rua_with(association(), ControlTime(5), &disconnect, |event| {
            assert!(
                matches!(event, Event::Disconnected { request: actual, .. } if *actual == request)
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(session.peer_disconnected_release(request));
    assert_eq!(session.ues[&key(1)].connections, peer_connections);
    session
        .publish_nas_with(
            nas(0, Domain::Ps, &[1]),
            ControlTime(6),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    session
        .publish_nas_with(
            nas(1, Domain::Cs, &[1]),
            ControlTime(6),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    session
        .complete_peer_disconnect(request, ControlTime(7))
        .unwrap();
    session
        .publish_nas_with(
            nas(0, Domain::Cs, &[1]),
            ControlTime(8),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(!session.owns_iu_release(request));
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
}

#[test]
fn inactivity_request_keeps_core_authority_and_cannot_normally_retire_open_connections() {
    let mut session = both_domains();
    assert_eq!(
        session.retire_rrc_release_with(key(0), ControlTime(4), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::State))
    );
    let context = ContextId::new(42).unwrap();
    assert_eq!(
        session.request_inactivity_release_with(
            key(0),
            context,
            Domain::Ps,
            ControlTime(4),
            |frame| {
                let decoded = rua::Message::decode(&frame.data, Limits::default()).unwrap();
                let rua::Message::Direct { ranap, domain, .. } = decoded.message else {
                    panic!("direct RANAP required")
                };
                assert_eq!(domain, Domain::Ps);
                assert_eq!(ranap, [0, 11, 0x40, 9, 0, 0, 1, 0, 4, 0x40, 2, 3, 0xc0]);
                Ok::<_, ()>(())
            }
        ),
        Ok(true)
    );
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
    assert_eq!(session.ues[&key(0)].release_requests, [None; 2]);
    assert_eq!(
        session.ues[&key(0)].inactivity_release_requested,
        [false, true]
    );
    assert_eq!(
        session.request_signalling_release_with(
            key(0),
            context,
            Domain::Ps,
            ControlTime(5),
            |_| Ok::<_, ()>(())
        ),
        Ok(true)
    );
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false, true]
    );
    assert_eq!(
        session.ues[&key(1)].signalling_release_requested,
        [false; 2]
    );
}

#[test]
fn ue_requested_release_is_transactional_idempotent_and_scoped_to_its_domain() {
    let mut session = both_domains();
    let context = ContextId::new(42).unwrap();
    let connections = session.ues[&key(0)].connections;
    assert_eq!(
        session.request_signalling_release_with(
            key(0),
            context,
            Domain::Cs,
            ControlTime(4),
            |_| Err::<(), _>(7)
        ),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false; 2]
    );
    assert_eq!(session.request_signalling_release_with(key(0), context, Domain::Cs, ControlTime(5), |frame| {
        assert_eq!(frame.scope, FrameScope::Ue { key: key(0), domain: Some(Domain::Cs) });
        let decoded = rua::Message::decode(&frame.data, Limits::default()).unwrap();
        assert!(matches!(decoded.message, rua::Message::Direct { context: actual, domain: Domain::Cs, .. } if actual == context));
        Ok::<_, ()>(())
    }), Ok(true));
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
    assert_eq!(session.ues[&key(0)].connections, connections);
    assert_eq!(session.ues[&key(0)].release_requests, [None; 2]);
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [true, false]
    );
    assert_eq!(
        session.ues[&key(1)].signalling_release_requested,
        [false; 2]
    );
    assert_eq!(
        session.request_signalling_release_with(
            key(0),
            context,
            Domain::Cs,
            ControlTime(6),
            |_| -> Result<(), ()> { panic!("duplicate request publication") }
        ),
        Ok(false)
    );
    let request = admit(&mut session, Domain::Cs, 7);
    session
        .complete_iu_release_without_reports_with(request, ControlTime(8), |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false; 2]
    );
    assert_eq!(
        session.ues[&key(0)].domains,
        [LinkState::Closed, LinkState::Open]
    );
    assert_eq!(
        session.ues[&key(1)].domains,
        [LinkState::Open, LinkState::Closed]
    );
}

#[test]
fn ps_transport_release_preserves_connection_ownership_until_core_completion() {
    let mut session = both_domains();
    let context = ContextId::new(42).unwrap();
    let connections = session.ues[&key(0)].connections;
    let stale_key = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key(0)
    };
    for (owner, target) in [
        (key(0), ContextId::new(43).unwrap()),
        (key(1), context),
        (stale_key, context),
    ] {
        assert_eq!(
            session.request_ps_transport_release_with(
                owner,
                target,
                ControlTime(4),
                |_| -> Result<(), ()> { panic!("foreign connection publication") }
            ),
            Err(PublishError::Session(Error::Origin))
        );
    }
    assert_eq!(
        session.request_ps_transport_release_with(key(0), context, ControlTime(4), |_| {
            Err::<(), _>(7)
        }),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false; 2]
    );
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
    assert_eq!(session.ues[&key(0)].connections, connections);
    assert_eq!(
        session.request_ps_transport_release_with(key(0), context, ControlTime(5), |frame| {
            assert_eq!(frame.association, association());
            assert_eq!(frame.protocol, Protocol::Rua);
            assert_eq!(
                frame.scope,
                FrameScope::Ue {
                    key: key(0),
                    domain: Some(Domain::Ps),
                }
            );
            let rua::Message::Direct {
                context: actual,
                domain,
                ranap: bytes,
            } = rua::Message::decode(&frame.data, Limits::default())
                .unwrap()
                .message
            else {
                panic!("connection-oriented release request required")
            };
            assert_eq!(actual, context);
            assert_eq!(domain, Domain::Ps);
            let packet = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
            assert_eq!(packet.outcome, Outcome::Initiating);
            assert_eq!(packet.procedure, 11);
            let cause: ranap::rab::Cause = packet.required(4).unwrap().decode().unwrap();
            assert!(matches!(cause, ranap::rab::Cause::Radio(_)));
            assert_eq!(cause.code(), 15);
            Ok::<_, ()>(())
        }),
        Ok(true)
    );
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
    assert_eq!(session.ues[&key(0)].connections, connections);
    assert_eq!(session.ues[&key(0)].release_requests, [None; 2]);
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false, true]
    );
    assert_eq!(
        session.ues[&key(1)].signalling_release_requested,
        [false; 2]
    );
    session
        .publish_nas_with(nas(0, Domain::Cs, &[1]), ControlTime(6), |frame| {
            assert!(matches!(
                rua::Message::decode(&frame.data, Limits::default()).unwrap().message,
                rua::Message::Direct { context: actual, domain: Domain::Cs, .. }
                    if actual == context
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        session.request_ps_transport_release_with(
            key(0),
            context,
            ControlTime(7),
            |_| -> Result<(), ()> { panic!("duplicate release request publication") }
        ),
        Ok(false)
    );
    let request = admit(&mut session, Domain::Ps, 8);
    assert_eq!(request.association(), association());
    assert_eq!(request.key(), key(0));
    assert_eq!(request.context(), context);
    assert_eq!(request.domain(), Domain::Ps);
    session
        .complete_iu_release_without_reports_with(request, ControlTime(9), |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        session.ues[&key(0)].domains,
        [LinkState::Open, LinkState::Closed]
    );
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false; 2]
    );
    assert_eq!(
        session.ues[&key(1)].domains,
        [LinkState::Open, LinkState::Closed]
    );
    assert_eq!(
        session.request_ps_transport_release_with(
            key(0),
            context,
            ControlTime(10),
            |_| -> Result<(), ()> { panic!("closed connection publication") }
        ),
        Ok(false)
    );
    session
        .publish_nas_with(nas(0, Domain::Ps, &[1]), ControlTime(11), |frame| {
            assert!(matches!(
                rua::Message::decode(&frame.data, Limits::default()).unwrap().message,
                rua::Message::Connect { context: actual, domain: Domain::Ps, .. }
                    if actual == context
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(session.ues[&key(0)].connections[0], connections[0]);
    assert_ne!(session.ues[&key(0)].connections[1], connections[1]);
    assert_eq!(
        session.request_ps_transport_release_with(key(0), context, ControlTime(12), |_| {
            Ok::<_, ()>(())
        }),
        Ok(true)
    );
}

#[test]
fn ps_transport_failure_can_escalate_an_outstanding_inactivity_request() {
    let mut session = both_domains();
    let context = ContextId::new(42).unwrap();
    assert_eq!(
        session.request_inactivity_release_with(
            key(0),
            context,
            Domain::Ps,
            ControlTime(4),
            |_| Ok::<_, ()>(())
        ),
        Ok(true)
    );
    assert_eq!(
        session.request_ps_transport_release_with(key(0), context, ControlTime(5), |_| {
            Ok::<_, ()>(())
        }),
        Ok(true)
    );
    assert_eq!(
        session.request_ps_transport_release_with(
            key(0),
            context,
            ControlTime(6),
            |_| -> Result<(), ()> { panic!("duplicate release request publication") }
        ),
        Ok(false)
    );
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
    assert_eq!(
        session.ues[&key(0)].signalling_release_requested,
        [false, true]
    );
}

#[test]
fn release_cancels_only_its_exact_pending_security_response() {
    let mut session = both_domains();
    let cs = security_request(&mut session, Domain::Cs, ranap::KeyStatus::Old, 4);
    let ps = security_request(&mut session, Domain::Ps, ranap::KeyStatus::Old, 4);
    assert!(!session.security_response_cancelled_by_iu_release(cs));
    let release = admit(&mut session, Domain::Cs, 5);
    assert!(session.owns_iu_release(release));
    assert!(session.security_response_cancelled_by_iu_release(cs));
    assert!(!session.security_response_cancelled_by_iu_release(ps));
}

fn command(domain: Domain) -> rua::Decoded {
    rua::Message::decode(
        &rua::Message::Direct {
            context: ContextId::new(42).unwrap(),
            domain,
            ranap: ranap::release::Command {
                cause: ranap::rab::Cause::from_code(83).unwrap(),
            }
            .encode(Limits::default())
            .unwrap(),
        }
        .encode(Limits::default())
        .unwrap(),
        Limits::default(),
    )
    .unwrap()
}

fn admit(session: &mut Session, domain: Domain, now: u64) -> IuReleaseRequest {
    session
        .receive_rua_with(association(), ControlTime(now), &command(domain), |event| {
            let Event::IuRelease {
                request,
                received_at,
                command,
                notify_ies,
            } = event
            else {
                panic!("expected owned Iu Release")
            };
            assert_eq!(*received_at, ControlTime(now));
            assert_eq!(command.cause.code(), 83);
            assert!(notify_ies.is_empty());
            Ok::<_, ()>(*request)
        })
        .unwrap()
}

fn both_domains() -> Session {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 42);
    register(&mut session, 1, 43);
    send(&mut session, 0, Domain::Cs);
    send(&mut session, 0, Domain::Ps);
    send(&mut session, 1, Domain::Cs);
    session
}

#[test]
fn release_admission_backpressure_preserves_the_open_connection() {
    let mut session = both_domains();
    let before = session.ues[&key(0)].connections;
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(4),
            &command(Domain::Cs),
            |_| Err::<(), _>(7)
        ),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Open; 2]);
    assert_eq!(session.ues[&key(0)].connections, before);
    assert_eq!(session.ues[&key(0)].release_requests, [None; 2]);
    let request = admit(&mut session, Domain::Cs, 5);
    assert!(session.owns_iu_release(request));
    assert_eq!(
        session.ues[&key(0)].domains,
        [LinkState::Releasing, LinkState::Open]
    );
    assert!(
        session
            .publish_nas_with(
                nas(0, Domain::Cs, &[1]),
                ControlTime(5),
                |_| -> Result<(), ()> { panic!("NAS after release admission") }
            )
            .is_err()
    );
    assert!(
        session
            .publish_nas_with(
                nas(0, Domain::Ps, &[1]),
                ControlTime(5),
                |_| Ok::<_, ()>(())
            )
            .is_ok()
    );
}

#[test]
fn release_complete_is_atomic_domain_scoped_and_rejects_stale_reuse() {
    let mut session = both_domains();
    let ps_connection = session.ues[&key(0)].connections[1];
    let peer_connections = session.ues[&key(1)].connections;
    let request = admit(&mut session, Domain::Cs, 4);
    assert_eq!(
        session.complete_iu_release_without_reports_with(
            request,
            ControlTime(5),
            |_| Err::<(), _>(8)
        ),
        Err(PublishError::Downstream(8))
    );
    assert!(session.owns_iu_release(request));
    let frame = session
        .complete_iu_release_without_reports_with(request, ControlTime(6), |frame| {
            Ok::<_, ()>(frame)
        })
        .unwrap();
    assert_eq!(
        frame.scope,
        FrameScope::Ue {
            key: key(0),
            domain: Some(Domain::Cs)
        }
    );
    let decoded = rua::Message::decode(&frame.data, Limits::default()).unwrap();
    let rua::Message::Disconnect {
        context,
        domain,
        cause,
        ranap,
    } = decoded.message
    else {
        panic!("Release Complete must terminate RUA")
    };
    assert_eq!(context, ContextId::new(42).unwrap());
    assert_eq!(domain, Domain::Cs);
    assert_eq!(cause, rua::Cause::Radio(rua::RadioCause::Normal));
    assert_eq!(ranap.unwrap(), [0x20, 1, 0, 3, 0, 0, 0]);
    assert!(!session.owns_iu_release(request));
    assert_eq!(
        session.ues[&key(0)].domains,
        [LinkState::Closed, LinkState::Open]
    );
    assert_eq!(session.ues[&key(0)].connections[1], ps_connection);
    assert_eq!(session.ues[&key(1)].connections, peer_connections);
    assert_eq!(session.registered_ues(), 2);
    session
        .publish_nas_with(nas(0, Domain::Cs, &[1]), ControlTime(7), |frame| {
            assert!(matches!(
                rua::Message::decode(&frame.data, Limits::default())
                    .unwrap()
                    .message,
                rua::Message::Connect { .. }
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(
        session
            .complete_iu_release_without_reports_with(
                request,
                ControlTime(8),
                |_| -> Result<(), ()> { panic!("old connection completed twice") }
            )
            .is_err()
    );
}

#[test]
fn simultaneous_domains_have_independent_release_custody_and_session_identity() {
    let mut session = both_domains();
    let cs = admit(&mut session, Domain::Cs, 4);
    let ps = admit(&mut session, Domain::Ps, 4);
    assert_ne!(cs, ps);
    let other_session = both_domains();
    assert!(!other_session.owns_iu_release(cs));
    session
        .complete_iu_release_without_reports_with(cs, ControlTime(5), |_| Ok::<_, ()>(()))
        .unwrap();
    assert!(session.owns_iu_release(ps));
    session
        .complete_iu_release_without_reports_with(ps, ControlTime(5), |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Closed; 2]);
    assert_eq!(session.ues[&key(1)].domains[0], LinkState::Open);
}

#[test]
fn release_is_not_admitted_connectionlessly_or_as_an_opening_message() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 42);
    let bytes = ranap::release::Command {
        cause: ranap::rab::Cause::from_code(83).unwrap(),
    }
    .encode(Limits::default())
    .unwrap();
    for message in [
        rua::Message::Connectionless {
            ranap: bytes.clone(),
        },
        rua::Message::Connect {
            domain: Domain::Cs,
            context: ContextId::new(42).unwrap(),
            ranap: bytes,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
        },
    ] {
        let decoded = rua::Message::decode(
            &message.encode(Limits::default()).unwrap(),
            Limits::default(),
        )
        .unwrap();
        assert!(
            session
                .receive_rua_with(
                    association(),
                    ControlTime(4),
                    &decoded,
                    |_| -> Result<(), ()> { panic!("release requires an existing Iu connection") }
                )
                .is_err()
        );
    }
    assert_eq!(session.ues[&key(0)].domains, [LinkState::Closed; 2]);
}
