// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod rab;
mod release;
use crate::{
    hnbap::{AccessMode, CsgCapability, Imsi, Release},
    wire::{Outcome, Packet},
};

fn key(slot: u32) -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot,
        generation: NonZeroU64::MIN,
    }
}
fn identity(slot: u32) -> UeIdentity {
    UeIdentity::Imsi(Imsi::from_digits(&format!("001010{:09}", slot)).unwrap())
}
fn capabilities() -> Capabilities {
    Capabilities {
        release: Release::R8AndLater,
        csg: CsgCapability::NotCapable,
    }
}
fn association() -> AssociationId {
    AssociationId(NonZeroU64::MIN)
}
fn session() -> Session {
    Session::new(
        key(0).epoch,
        association(),
        HnbParameters {
            identity: b"Rust HNB".to_vec(),
            plmn: [0, 0xf1, 0x10],
            lac: 23,
            rac: 7,
            sac: 42,
            cell: 0x1234567,
            access: AccessMode::Open,
            csg: None,
        },
        SessionLimits {
            maximum_rab_requests_per_domain: NonZeroUsize::new(4).unwrap(),
            maximum_rab_release_requests_per_domain: NonZeroUsize::new(2).unwrap(),
            maximum_ues: NonZeroUsize::new(4).unwrap(),
            request_timeout_ms: NonZeroU64::new(1000).unwrap(),
            codec: Limits::default(),
        },
        ControlTime(0),
    )
    .unwrap()
}
fn ready(session: &mut Session) {
    session
        .register_hnb_with(ControlTime(0), |frame| {
            assert!(matches!(
                hnbap::Message::decode(&frame.data, Limits::default())
                    .unwrap()
                    .message,
                hnbap::Message::HnbRegister(_)
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    let response = hnbap::Message::HnbAccepted {
        rnc: 1,
        mux_port: None,
    }
    .encode(Limits::default())
    .unwrap();
    let decoded = hnbap::Message::decode(&response, Limits::default()).unwrap();
    session
        .receive_hnbap_with(association(), ControlTime(1), &decoded, |_| Ok::<_, ()>(()))
        .unwrap();
}
fn register(session: &mut Session, slot: u32, context: u32) {
    session
        .register_ue_with(
            key(slot),
            identity(slot),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(2),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let response = hnbap::Message::UeAccepted {
        identity: identity(slot),
        context: ContextId::new(context).unwrap(),
        membership: None,
    }
    .encode(Limits::default())
    .unwrap();
    session
        .receive_hnbap_with(
            association(),
            ControlTime(2),
            &hnbap::Message::decode(&response, Limits::default()).unwrap(),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
}
fn nas<'a>(slot: u32, domain: Domain, bytes: &'a [u8]) -> UplinkNas<'a> {
    UplinkNas {
        key: key(slot),
        domain,
        nas: bytes,
        selector: None,
        cause: rua::EstablishmentCause::Normal,
    }
}
fn send(session: &mut Session, slot: u32, domain: Domain) -> Frame {
    session
        .publish_nas_with(nas(slot, domain, &[0x08, 0x41]), ControlTime(3), |frame| {
            Ok::<_, ()>(frame)
        })
        .unwrap()
}

fn security_command(domain: Domain, status: ranap::KeyStatus) -> rua::Decoded {
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0), ranap::Algorithm(1)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: Some(ranap::Protection {
            algorithms: vec![ranap::Algorithm(1), ranap::Algorithm(0)],
            key: ranap::Key::new([0x22; 16]),
        }),
        key_status: status,
    };
    let message = rua::Message::Direct {
        domain,
        context: ContextId::new(42).unwrap(),
        ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
    };
    rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap()
}

fn security_request(
    session: &mut Session,
    domain: Domain,
    status: ranap::KeyStatus,
    now: u64,
) -> SecurityRequest {
    session
        .receive_rua_with(
            association(),
            ControlTime(now),
            &security_command(domain, status),
            |event| {
                let Event::SecurityCommand { request, .. } = event else {
                    panic!("not a security request")
                };
                Ok::<_, ()>(*request)
            },
        )
        .unwrap()
}

#[test]
fn security_request_and_response_are_generation_scoped_and_atomic_under_backpressure() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 42);
    send(&mut session, 0, Domain::Cs);
    let mut rejected = None;
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(4),
            &security_command(Domain::Cs, ranap::KeyStatus::Old),
            |event| {
                let Event::SecurityCommand { request, .. } = event else {
                    panic!("not a security request")
                };
                rejected = Some(*request);
                Err::<(), _>(7)
            }
        ),
        Err(PublishError::Downstream(7))
    );
    let request = security_request(&mut session, Domain::Cs, ranap::KeyStatus::Old, 4);
    assert_eq!(Some(request), rejected);
    assert_eq!(request.key(), key(0));
    assert_eq!(request.context().get(), 42);
    assert_eq!(request.domain(), Domain::Cs);
    assert_eq!(request.association(), association());
    let complete = ranap::SecurityResponse::Complete {
        integrity: ranap::Algorithm(0),
        encryption: Some(ranap::Algorithm(1)),
    };
    let invalid = ranap::SecurityResponse::Complete {
        integrity: ranap::Algorithm(15),
        encryption: Some(ranap::Algorithm(1)),
    };
    assert_eq!(
        session.publish_security_with(request, invalid, ControlTime(5), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Unsupported))
    );
    assert_eq!(
        session.publish_security_with(request, complete, ControlTime(5), |_| Err::<(), _>(8)),
        Err(PublishError::Downstream(8))
    );
    let frame = session
        .publish_security_with(request, complete, ControlTime(5), |frame| {
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
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = rua::Message::decode(&frame.data, Limits::default())
        .unwrap()
        .message
    else {
        panic!("not RUA direct")
    };
    assert_eq!(domain, Domain::Cs);
    assert_eq!(context.get(), 42);
    assert_eq!(
        ranap::SecurityResponse::decode(&ranap, Limits::default())
            .unwrap()
            .response,
        complete
    );
    assert_eq!(
        session.publish_security_with(request, complete, ControlTime(5), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Origin))
    );
}

#[test]
fn cs_ps_security_requests_remain_independent_and_cannot_report_conflicting_algorithms() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 42);
    send(&mut session, 0, Domain::Cs);
    send(&mut session, 0, Domain::Ps);
    let cs = security_request(&mut session, Domain::Cs, ranap::KeyStatus::Old, 4);
    let ps = security_request(&mut session, Domain::Ps, ranap::KeyStatus::Old, 4);
    assert_ne!(cs.serial(), ps.serial());
    let complete = ranap::SecurityResponse::Complete {
        integrity: ranap::Algorithm(0),
        encryption: Some(ranap::Algorithm(1)),
    };
    session
        .publish_security_with(cs, complete, ControlTime(5), |_| Ok::<_, ()>(()))
        .unwrap();
    let conflict = ranap::SecurityResponse::Complete {
        integrity: ranap::Algorithm(0),
        encryption: Some(ranap::Algorithm(0)),
    };
    assert_eq!(
        session.publish_security_with(ps, conflict, ControlTime(5), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::State))
    );
    session
        .publish_security_with(
            ps,
            ranap::SecurityResponse::Reject(ranap::SecurityReject::Conflict),
            ControlTime(5),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let old = security_request(&mut session, Domain::Cs, ranap::KeyStatus::Old, 6);
    assert_eq!(
        session.publish_security_with(old, complete, ControlTime(6), |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::State))
    );
    session
        .publish_security_with(
            old,
            ranap::SecurityResponse::Reject(ranap::SecurityReject::Conflict),
            ControlTime(6),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
}

#[test]
fn disconnected_domain_invalidates_its_request_before_any_response_can_be_published() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 42);
    send(&mut session, 0, Domain::Cs);
    let request = security_request(&mut session, Domain::Cs, ranap::KeyStatus::Old, 4);
    let message = rua::Message::Disconnect {
        domain: Domain::Cs,
        context: ContextId::new(42).unwrap(),
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    };
    let decoded = rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    session
        .receive_rua_with(association(), ControlTime(5), &decoded, |_| Ok::<_, ()>(()))
        .unwrap();
    let reject = ranap::SecurityResponse::Reject(ranap::SecurityReject::RadioProcedureFailure);
    assert!(
        session
            .publish_security_with(request, reject, ControlTime(5), |_| -> Result<(), ()> {
                panic!("stale security response was published")
            })
            .is_err()
    );
}

#[test]
fn registration_nas_and_both_domain_connections_use_one_gateway_assigned_ue_context() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 0x123456);
    for domain in [Domain::Cs, Domain::Ps] {
        let first = send(&mut session, 0, domain);
        let rua::Message::Connect {
            context,
            ranap,
            domain: actual,
            ..
        } = rua::Message::decode(&first.data, Limits::default())
            .unwrap()
            .message
        else {
            panic!("Connect");
        };
        assert_eq!(context.get(), 0x123456);
        assert_eq!(actual, domain);
        let packet = Packet::decode(Protocol::Ranap, &ranap, Limits::default()).unwrap();
        assert_eq!(packet.procedure, 19);
        assert_eq!(packet.outcome, Outcome::Initiating);
        assert_eq!(
            packet.required(79).unwrap().value.as_bytes(),
            &[0x12, 0x34, 0x56]
        );
        let second = send(&mut session, 0, domain);
        let rua::Message::Direct { context, ranap, .. } =
            rua::Message::decode(&second.data, Limits::default())
                .unwrap()
                .message
        else {
            panic!("Direct");
        };
        assert_eq!(context.get(), 0x123456);
        assert!(
            Packet::decode(Protocol::Ranap, &ranap, Limits::default())
                .unwrap()
                .optional(59)
                .is_none()
        );
    }
    assert_eq!(session.registered_ues(), 1);
}

#[test]
fn queue_rejection_preserves_registration_and_first_nas_connect_decision() {
    let mut session = session();
    assert_eq!(
        session.register_hnb_with(ControlTime(0), |_| Err::<(), _>("full")),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(session.state(), HnbState::Idle);
    ready(&mut session);
    assert_eq!(
        session.register_ue_with(
            key(0),
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(2),
            |_| Err::<(), _>("full")
        ),
        Err(PublishError::Downstream("full"))
    );
    assert!(session.identities.is_empty());
    register(&mut session, 0, 0x123456);
    assert_eq!(
        session.publish_nas_with(nas(0, Domain::Ps, &[1]), ControlTime(3), |_| Err::<(), _>(
            "full"
        )),
        Err(PublishError::Downstream("full"))
    );
    assert!(matches!(
        rua::Message::decode(&send(&mut session, 0, Domain::Ps).data, Limits::default())
            .unwrap()
            .message,
        rua::Message::Connect { .. }
    ));
}

#[test]
fn peer_ue_context_collision_and_another_association_cannot_cross_deliver() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 0x123456);
    session
        .register_ue_with(
            key(1),
            identity(1),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(2),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let response = hnbap::Decoded {
        message: hnbap::Message::UeAccepted {
            identity: identity(1),
            context: ContextId::new(0x123456).unwrap(),
            membership: None,
        },
        notify_ies: vec![],
    };
    assert_eq!(
        session.receive_hnbap_with(
            association(),
            ControlTime(2),
            &response,
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::Identity))
    );
    assert_eq!(session.context(key(1)), None);
    assert_eq!(session.context(key(0)).unwrap().get(), 0x123456);
    let other = AssociationId(NonZeroU64::new(2).unwrap());
    assert_eq!(
        session.receive_hnbap_with(other, ControlTime(2), &response, |_| Ok::<_, ()>(())),
        Err(PublishError::Session(Error::Origin))
    );
}

#[test]
fn cancelled_pending_identity_is_quarantined_until_its_reply_can_be_deregistered() {
    let mut session = session();
    ready(&mut session);
    session
        .register_ue_with(
            key(0),
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(2),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        session.retire_with(key(0), ControlTime(3), |_| Ok::<_, ()>(())),
        Ok(None)
    );
    let replacement = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key(0)
    };
    assert_eq!(
        session.register_ue_with(
            replacement,
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(3),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::State))
    );
    let response = hnbap::Decoded {
        message: hnbap::Message::UeAccepted {
            identity: identity(0),
            context: ContextId::new(0x123456).unwrap(),
            membership: None,
        },
        notify_ies: vec![],
    };
    session.receive_hnbap_with(association(), ControlTime(4), &response, |event| { assert!(matches!(event, Event::CancelledRegistrationAccepted { key: actual, .. } if *actual == key(0))); Ok::<_, ()>(()) }).unwrap();
    assert_eq!(session.context(key(0)), None);
    assert_eq!(
        session.retire_with(key(0), ControlTime(5), |frames| {
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].protocol, Protocol::Hnbap);
            Ok::<_, ()>(())
        }),
        Ok(Some(()))
    );
    session
        .register_ue_with(
            replacement,
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(6),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
}

#[test]
fn retirement_admits_both_disconnects_and_deregistration_atomically_and_preserves_peer() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 1);
    register(&mut session, 1, 2);
    send(&mut session, 0, Domain::Cs);
    send(&mut session, 0, Domain::Ps);
    send(&mut session, 1, Domain::Ps);
    assert_eq!(
        session.retire_with(key(0), ControlTime(4), |_| Err::<(), _>("full")),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(session.context(key(0)), None);
    let retained = session.retained_context(key(0)).unwrap();
    assert!(session.context_is_closing(retained));
    assert!(!session.context_is_closing(session.context(key(1)).unwrap()));
    assert!(session.ues.contains_key(&key(0)));
    assert_eq!(
        session.publish_nas_with(
            nas(0, Domain::Cs, &[1]),
            ControlTime(4),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::State))
    );
    assert_eq!(
        session.retire_with(key(0), ControlTime(4), |frames| {
            assert_eq!(frames.len(), 3);
            for frame in &frames[..2] {
                assert!(matches!(
                    rua::Message::decode(&frame.data, Limits::default())
                        .unwrap()
                        .message,
                    rua::Message::Disconnect { ranap: None, .. }
                ));
            }
            assert!(matches!(
                hnbap::Message::decode(&frames[2].data, Limits::default())
                    .unwrap()
                    .message,
                hnbap::Message::UeDeregister { .. }
            ));
            Ok::<_, ()>(())
        }),
        Ok(Some(()))
    );
    assert_eq!(session.context(key(0)), None);
    assert!(session.context(key(1)).is_some());
}

#[test]
fn remote_disconnect_blocks_new_nas_during_upper_backpressure_without_closing_peer_domain() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 1);
    send(&mut session, 0, Domain::Cs);
    send(&mut session, 0, Domain::Ps);
    let disconnect = rua::Decoded {
        message: rua::Message::Disconnect {
            domain: Domain::Cs,
            context: ContextId::new(1).unwrap(),
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        },
        notify_ies: vec![],
    };
    assert_eq!(
        session.receive_rua_with(
            association(),
            ControlTime(4),
            &disconnect,
            |_| Err::<(), _>("full")
        ),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(
        session.publish_nas_with(
            nas(0, Domain::Cs, &[1]),
            ControlTime(4),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::State))
    );
    session
        .publish_nas_with(
            nas(0, Domain::Ps, &[1]),
            ControlTime(4),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let request = session
        .receive_rua_with(association(), ControlTime(4), &disconnect, |event| {
            let Event::Disconnected { request, .. } = event else {
                panic!("disconnect owner required")
            };
            Ok::<_, ()>(*request)
        })
        .unwrap();
    assert!(session.owns_iu_release(request));
    assert_eq!(
        session.ues[&key(0)].domains,
        [LinkState::Releasing, LinkState::Open]
    );
    session
        .publish_nas_with(
            nas(0, Domain::Ps, &[2]),
            ControlTime(5),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(
        session
            .complete_iu_release_without_reports_with(
                request,
                ControlTime(5),
                |_| -> Result<(), ()> {
                    panic!("peer disconnect must not produce another wire release")
                }
            )
            .is_err()
    );
    session
        .complete_peer_disconnect(request, ControlTime(5))
        .unwrap();
    assert!(!session.owns_iu_release(request));
    assert_eq!(
        session.complete_peer_disconnect(request, ControlTime(5)),
        Err(Error::Origin)
    );
    assert_eq!(
        session.ues.get(&key(0)).unwrap().domains,
        [LinkState::Closed, LinkState::Open]
    );
}

#[test]
fn overload_backoff_and_observed_association_loss_deny_transfers_until_admitted() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 1);
    let deregister = hnbap::Decoded {
        message: hnbap::Message::HnbDeregistered {
            cause: hnbap::Cause::Radio(hnbap::RadioCause::Overload),
            backoff_seconds: Some(42),
        },
        notify_ies: vec![],
    };
    assert_eq!(
        session.receive_hnbap_with(
            association(),
            ControlTime(3),
            &deregister,
            |_| Err::<(), _>("full")
        ),
        Err(PublishError::Downstream("full"))
    );
    assert_eq!(
        session.state(),
        HnbState::Draining {
            backoff_until: Some(ControlTime(42003))
        }
    );
    assert_eq!(
        session.publish_nas_with(
            nas(0, Domain::Ps, &[1]),
            ControlTime(3),
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Session(Error::State))
    );
    session.expire_with(ControlTime(4), |event| { assert!(matches!(event, Event::AssociationLost { backoff_until: Some(ControlTime(42003)), owners } if owners == &[key(0)])); Ok::<_, ()>(()) }).unwrap();
    assert_eq!(
        session.state(),
        HnbState::Backoff {
            until: ControlTime(42003)
        }
    );
    assert!(
        session
            .register_hnb_with(ControlTime(42002), |_| Ok::<_, ()>(()))
            .is_err()
    );
    session
        .register_hnb_with(ControlTime(42003), |_| Ok::<_, ()>(()))
        .unwrap();
}

#[test]
fn request_expiry_uses_control_milliseconds_and_requires_association_recovery() {
    // An unanswered HNB REGISTER REQUEST still fails the association (TS 25.469 8.2).
    let mut session = session();
    session
        .register_hnb_with(ControlTime(0), |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        session.expire_with(ControlTime(999), |_| Ok::<_, ()>(())),
        Ok(None)
    );
    assert_eq!(
        session.expire_with(ControlTime(1000), |_| Ok::<_, ()>(())),
        Ok(Some(()))
    );
    assert_eq!(session.state(), HnbState::Failed);
    assert!(
        session
            .register_hnb_with(ControlTime(1001), |_| Ok::<_, ()>(()))
            .is_err()
    );
    // An unanswered UE REGISTER REQUEST fails that UE only (TS 25.469 8.4).
    let mut fresh = self::session();
    ready(&mut fresh);
    fresh
        .register_ue_with(
            key(0),
            identity(0),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(2),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        fresh.expire_with(ControlTime(1001), |_| Ok::<_, ()>(())),
        Ok(None)
    );
    assert_eq!(
        fresh.expire_with(ControlTime(1002), |_| Ok::<_, ()>(())),
        Ok(Some(()))
    );
    assert_eq!(fresh.state(), HnbState::Registered { rnc: 1 });
    assert!(!fresh.contains_ue(key(0)));
}

fn connectionless_reset(domain: Domain) -> rua::Decoded {
    let packet = crate::wire::Packet::initiating(
        Protocol::Ranap,
        ranap::RESET,
        crate::wire::Criticality::Reject,
        vec![
            crate::wire::InformationElement::new(3, crate::wire::Criticality::Reject, &domain)
                .unwrap(),
        ],
    );
    let message = rua::Message::Connectionless {
        ranap: packet.encode(Limits::default()).unwrap(),
    };
    rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap()
}

#[test]
fn cn_reset_releases_every_connection_of_its_domain_and_is_acknowledged() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 10);
    register(&mut session, 1, 11);
    send(&mut session, 0, Domain::Cs);
    send(&mut session, 0, Domain::Ps);
    send(&mut session, 1, Domain::Ps);
    let decoded = connectionless_reset(Domain::Ps);
    let event = session
        .receive_rua_with(association(), ControlTime(5), &decoded, |event| {
            Ok::<_, ()>(event.clone())
        })
        .unwrap();
    let Event::Reset { domain, releases } = event else {
        panic!("reset event expected, got {event:?}")
    };
    assert_eq!(domain, Domain::Ps);
    assert_eq!(releases.len(), 2);
    assert!(
        releases
            .iter()
            .all(|request| request.domain() == Domain::Ps)
    );
    assert!(
        releases
            .iter()
            .all(|request| session.peer_disconnected_release(*request))
    );
    // The CS connection of the first UE is untouched and still usable.
    let frame = session
        .publish_nas_with(nas(0, Domain::Cs, &[0x08, 0x41]), ControlTime(6), |frame| {
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
    let frame = session
        .publish_reset_acknowledge_with(Domain::Ps, ControlTime(7), Ok::<_, ()>)
        .unwrap();
    assert_eq!(frame.scope, FrameScope::Hnb);
    let rua::Message::Connectionless { ranap } =
        rua::Message::decode(&frame.data, Limits::default())
            .unwrap()
            .message
    else {
        panic!("connectionless acknowledgement expected")
    };
    let packet = crate::wire::Packet::decode(Protocol::Ranap, &ranap, Limits::default()).unwrap();
    assert_eq!(packet.procedure, ranap::RESET);
    assert_eq!(packet.outcome, crate::wire::Outcome::Successful);
    assert_eq!(
        packet.required(3).unwrap().decode::<Domain>().unwrap(),
        Domain::Ps
    );
    assert!(packet.required(86).is_ok());
    // A second RESET for the same domain finds nothing left to release.
    let event = session
        .receive_rua_with(association(), ControlTime(8), &decoded, |event| {
            Ok::<_, ()>(event.clone())
        })
        .unwrap();
    assert!(matches!(event, Event::Reset { releases, .. } if releases.is_empty()));
}

#[test]
fn radio_lost_release_request_carries_cause_radio_connection_with_ue_lost() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 10);
    send(&mut session, 0, Domain::Ps);
    let mut sent = None;
    assert!(
        session
            .request_radio_lost_release_with(
                key(0),
                ContextId::new(10).unwrap(),
                Domain::Ps,
                ControlTime(4),
                |frame| {
                    sent = Some(frame);
                    Ok::<_, ()>(())
                },
            )
            .unwrap()
    );
    let frame = sent.unwrap();
    let rua::Message::Direct { ranap, domain, .. } =
        rua::Message::decode(&frame.data, Limits::default())
            .unwrap()
            .message
    else {
        panic!("direct transfer expected")
    };
    assert_eq!(domain, Domain::Ps);
    let packet = crate::wire::Packet::decode(Protocol::Ranap, &ranap, Limits::default()).unwrap();
    assert_eq!(packet.procedure, 11);
    let cause: ranap::rab::Cause = packet.required(4).unwrap().decode().unwrap();
    assert_eq!(cause.code(), 46);
    // TS 25.413 8.4.2: the request is sent once per connection.
    assert!(
        !session
            .request_radio_lost_release_with(
                key(0),
                ContextId::new(10).unwrap(),
                Domain::Ps,
                ControlTime(5),
                |_| Ok::<_, ()>(()),
            )
            .unwrap()
    );
}

#[test]
fn ue_register_timeout_fails_only_that_ue() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 10);
    session
        .register_ue_with(
            key(1),
            identity(1),
            capabilities(),
            RegistrationCause::Normal,
            ControlTime(2),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        session
            .expire_with(ControlTime(1001), |_| Ok::<_, ()>(()))
            .unwrap(),
        None
    );
    let event = session
        .expire_with(ControlTime(1002), |event| Ok::<_, ()>(event.clone()))
        .unwrap()
        .unwrap();
    assert!(matches!(event, Event::UeRejected { key: rejected, .. } if rejected == key(1)));
    assert!(matches!(session.state(), HnbState::Registered { .. }));
    assert!(session.contains_ue(key(0)));
    assert!(!session.contains_ue(key(1)));
    assert_eq!(
        session
            .expire_with(ControlTime(1003), |_| Ok::<_, ()>(()))
            .unwrap(),
        None
    );
}

#[test]
fn undecodable_ranap_on_a_connection_is_reported_with_an_error_indication() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 10);
    send(&mut session, 0, Domain::Cs);
    let message = rua::Message::Direct {
        domain: Domain::Cs,
        context: ContextId::new(10).unwrap(),
        ranap: vec![0x00, 0x14, 0x40, 0x03, 0x00, 0x00, 0x00],
    };
    let decoded = rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    let event = session
        .receive_rua_with(association(), ControlTime(4), &decoded, |event| {
            Ok::<_, ()>(event.clone())
        })
        .unwrap();
    assert!(
        matches!(event, Event::ProtocolError { key: k, domain: Domain::Cs, .. } if k == key(0))
    );
    let frame = session
        .take_error_indication_frame(ControlTime(5))
        .unwrap()
        .unwrap();
    let rua::Message::Direct { ranap, .. } = rua::Message::decode(&frame.data, Limits::default())
        .unwrap()
        .message
    else {
        panic!("direct transfer expected")
    };
    let packet = crate::wire::Packet::decode(Protocol::Ranap, &ranap, Limits::default()).unwrap();
    assert_eq!(packet.procedure, ranap::ERROR_INDICATION);
    let cause: ranap::rab::Cause = packet.required(4).unwrap().decode().unwrap();
    assert!((97..=112).contains(&cause.code()));
    assert!(
        session
            .take_error_indication_frame(ControlTime(6))
            .unwrap()
            .is_none()
    );
}

#[test]
fn rab_release_request_names_each_rab_and_cause_on_its_connection() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 10);
    assert!(
        session
            .request_rab_release_with(key(0), Domain::Ps, &[(5, 263)], ControlTime(4), |_| {
                Ok::<_, ()>(())
            })
            .is_err()
    );
    send(&mut session, 0, Domain::Ps);
    let frame = session
        .request_rab_release_with(key(0), Domain::Ps, &[(5, 263)], ControlTime(5), Ok::<_, ()>)
        .unwrap();
    let rua::Message::Direct { ranap, domain, .. } =
        rua::Message::decode(&frame.data, Limits::default())
            .unwrap()
            .message
    else {
        panic!("direct transfer expected")
    };
    assert_eq!(domain, Domain::Ps);
    let packet = crate::wire::Packet::decode(Protocol::Ranap, &ranap, Limits::default()).unwrap();
    assert_eq!(packet.procedure, ranap::RAB_RELEASE_REQUEST);
    assert_eq!(packet.outcome, Outcome::Initiating);
    assert!(packet.required(41).is_ok());
}

#[test]
fn unsupported_reject_procedure_on_a_connection_is_rejected_with_diagnostics() {
    let mut session = session();
    ready(&mut session);
    register(&mut session, 0, 10);
    send(&mut session, 0, Domain::Cs);
    // Procedure 25 (Location Related Data) is not supported here.
    let ranap = crate::wire::Packet::initiating(
        Protocol::Ranap,
        25,
        crate::wire::Criticality::Reject,
        vec![],
    )
    .encode(Limits::default())
    .unwrap();
    let message = rua::Message::Direct {
        domain: Domain::Cs,
        context: ContextId::new(10).unwrap(),
        ranap,
    };
    let decoded = rua::Message::decode(
        &message.encode(Limits::default()).unwrap(),
        Limits::default(),
    )
    .unwrap();
    let event = session
        .receive_rua_with(association(), ControlTime(4), &decoded, |event| {
            Ok::<_, ()>(event.clone())
        })
        .unwrap();
    assert!(matches!(event, Event::ProtocolError { cause: 100, .. }));
    let frame = session
        .take_error_indication_frame(ControlTime(5))
        .unwrap()
        .unwrap();
    let rua::Message::Direct { ranap, .. } = rua::Message::decode(&frame.data, Limits::default())
        .unwrap()
        .message
    else {
        panic!("direct transfer expected")
    };
    let packet = crate::wire::Packet::decode(Protocol::Ranap, &ranap, Limits::default()).unwrap();
    assert_eq!(packet.procedure, ranap::ERROR_INDICATION);
    assert!(packet.required(9).is_ok());
}
