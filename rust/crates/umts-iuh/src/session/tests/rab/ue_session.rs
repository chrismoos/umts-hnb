// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn cross_domain_ue_session_identity_keeps_unique_session_owner_and_context() {
    let mut first = live();
    let mut second = live();
    let cs = request(&mut first, 0, Domain::Cs, &setup(5), 4);
    let ps = request(&mut first, 0, Domain::Ps, &setup(6), 4);
    let foreign_ps = request(&mut second, 0, Domain::Ps, &setup(6), 4);
    let another_ue = request(&mut first, 1, Domain::Cs, &setup(5), 4);
    assert_eq!(cs.association(), foreign_ps.association());
    assert_eq!(cs.context(), foreign_ps.context());
    assert_eq!(cs.key(), foreign_ps.key());
    assert!(cs.same_ue_session(ps));
    assert!(ps.same_ue_session(cs));
    assert!(!cs.same_connection(ps));
    assert!(!cs.same_ue_session(foreign_ps));
    assert!(!cs.same_ue_session(another_ue));
    first
        .publish_rab_with(ps, &accepted(6), ControlTime(5), |_| Ok::<_, ()>(()))
        .unwrap();
    assert!(cs.same_ue_session(ps));
}

#[test]
fn reopening_same_domain_preserves_session_identity_but_not_connection_identity() {
    let mut session = live();
    let old = request(&mut session, 0, Domain::Ps, &setup(5), 4);
    let closed = rua::Decoded {
        notify_ies: vec![],
        message: rua::Message::Disconnect {
            context: old.context(),
            domain: Domain::Ps,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        },
    };
    let release = session
        .receive_rua_with(association(), ControlTime(5), &closed, |event| {
            let Event::Disconnected { request, .. } = event else {
                panic!("disconnect owner required")
            };
            Ok::<_, ()>(*request)
        })
        .unwrap();
    session
        .complete_peer_disconnect(release, ControlTime(5))
        .unwrap();
    session
        .publish_nas_with(nas(0, Domain::Ps, &[0x08, 0x41]), ControlTime(6), |frame| {
            Ok::<_, ()>(frame)
        })
        .unwrap();
    let new = request(&mut session, 0, Domain::Ps, &setup(5), 7);
    assert!(old.same_ue_session(new));
    assert!(!old.same_connection(new));
    assert!(session.check_rab_connection(old).is_err());
    assert!(session.check_rab_connection(new).is_ok());
}
