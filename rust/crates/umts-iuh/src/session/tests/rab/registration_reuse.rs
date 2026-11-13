// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn identical_registration_after_removal_cannot_revive_a_consumed_rab_connection() {
    for remote in [false, true] {
        let mut session = live();
        let old = request(&mut session, 0, Domain::Ps, &setup(5), 4);
        session
            .publish_rab_with(old, &accepted(5), ControlTime(5), |_| Ok::<_, ()>(()))
            .unwrap();
        assert!(session.check_rab_connection(old).is_ok());
        if remote {
            let bytes = hnbap::Message::UeDeregister {
                context: old.context(),
                cause: hnbap::Cause::Radio(hnbap::RadioCause::ConnectionLost),
            }
            .encode(Limits::default())
            .unwrap();
            let decoded = hnbap::Message::decode(&bytes, Limits::default()).unwrap();
            session
                .receive_hnbap_with(association(), ControlTime(6), &decoded, |event| {
                    assert!(matches!(event, Event::UeDeregistered { key: actual, .. } if *actual == key(0)));
                    Ok::<_, ()>(())
                })
                .unwrap();
        } else {
            assert_eq!(
                session
                    .retire_with(key(0), ControlTime(6), |frames| {
                        assert_eq!(frames.len(), 3);
                        assert!(matches!(
                            hnbap::Message::decode(&frames[2].data, Limits::default())
                                .unwrap()
                                .message,
                            hnbap::Message::UeDeregister { context, .. } if context == old.context()
                        ));
                        Ok::<_, ()>(())
                    })
                    .unwrap(),
                Some(())
            );
        }
        assert!(!session.contains_ue(key(0)));
        assert!(session.check_rab_connection(old).is_err());
        session
            .register_ue_with(
                key(0),
                identity(0),
                capabilities(),
                RegistrationCause::Normal,
                ControlTime(7),
                |frame| {
                    assert!(matches!(
                        hnbap::Message::decode(&frame.data, Limits::default())
                            .unwrap()
                            .message,
                        hnbap::Message::UeRegister { identity: actual, .. } if actual == identity(0)
                    ));
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        let accepted_registration = hnbap::Message::UeAccepted {
            identity: identity(0),
            context: old.context(),
            membership: None,
        }
        .encode(Limits::default())
        .unwrap();
        session
            .receive_hnbap_with(
                association(),
                ControlTime(8),
                &hnbap::Message::decode(&accepted_registration, Limits::default()).unwrap(),
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        session
            .publish_nas_with(nas(0, Domain::Ps, &[8, 0x41]), ControlTime(9), |frame| {
                assert!(matches!(
                    rua::Message::decode(&frame.data, Limits::default()).unwrap().message,
                    rua::Message::Connect { context, domain: Domain::Ps, .. } if context == old.context()
                ));
                Ok::<_, ()>(())
            })
            .unwrap();
        let replacement = request(&mut session, 0, Domain::Ps, &setup(5), 10);
        assert_eq!(old.key(), replacement.key());
        assert_eq!(old.context(), replacement.context());
        assert_eq!(old.association(), replacement.association());
        assert_eq!(old.domain(), replacement.domain());
        assert!(!old.same_connection(replacement));
        assert!(session.check_rab_connection(old).is_err());
        assert!(session.check_rab_connection(replacement).is_ok());
        assert!(
            session
                .authorize_rab(old, 5, RabOperation::SetupOrModify)
                .is_err()
        );
        assert!(
            session
                .authorize_rab(replacement, 5, RabOperation::SetupOrModify)
                .is_ok()
        );
    }
}
