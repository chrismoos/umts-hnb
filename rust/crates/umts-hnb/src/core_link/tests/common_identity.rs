// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_types::identity::Imsi;

fn imsi() -> Imsi {
    Imsi::from_digits("001010000000001").unwrap()
}

fn event(key: UeKey, domain: Domain, identity: Imsi) -> Event {
    Event::Ranap {
        key: Some(key),
        domain: Some(domain),
        message: ranap::Decoded {
            message: Inbound::CommonIdentity(identity),
            notify_ies: Vec::new(),
        },
    }
}

#[test]
fn common_id_retires_the_real_gateway_head_before_following_nas() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let before = radio.resources();
    for _ in 0..2 {
        core.rua(rua::Message::Direct {
            domain: Domain::Cs,
            context,
            ranap: ranap::common_identity(imsi(), Limits::default()).unwrap(),
        });
    }
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x02], None, Limits::default()).unwrap(),
    });
    for now in 6..9 {
        assert!(drive(&mut link, &mut radio, now).gateway.delivered);
    }
    assert_eq!(radio.resources(), before);
    assert_eq!(radio.subscriber_hint(key).unwrap().to_string(), "****0001");
    assert_eq!(radio.permanent_identity(key), Some(imsi()));
    assert_eq!(
        radio.publish_bearer(
            key,
            umts_rrc::nas::SignallingBearer::NasAm,
            &mut source,
            at(0),
            at(0),
        ),
        Ok(true)
    );
    assert_eq!(radio.registration_failure(key), None);
    assert!(!drive(&mut link, &mut radio, 9).gateway.delivered);
}

#[test]
fn identity_requires_the_exact_active_connection_and_never_rebinds_a_peer() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (first, _, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let (second, _, _) = register(&mut link, &core, &mut radio, &source, 1, 6);
    let first_connection = radio.connection(first, CnDomain::Cs).unwrap();
    let second_connection = radio.connection(second, CnDomain::Cs).unwrap();
    let first_hint = radio.subscriber_hint(first);
    let second_hint = radio.subscriber_hint(second);
    let before = radio.resources();
    radio
        .associate_core_identity(first_connection, imsi())
        .unwrap();
    let different_same_suffix = Imsi::from_digits("991010000000001").unwrap();
    assert_eq!(
        radio.associate_core_identity(first_connection, different_same_suffix),
        Err(SignallingError::IdentityConflict)
    );
    let mut wrong = first_connection;
    wrong.context = second_connection.context;
    assert_eq!(
        radio.associate_core_identity(wrong, imsi()),
        Err(SignallingError::Origin)
    );
    wrong = first_connection;
    wrong.key.domain = CnDomain::Ps;
    assert_eq!(
        radio.associate_core_identity(wrong, imsi()),
        Err(SignallingError::Origin)
    );
    wrong = first_connection;
    wrong.key.ue.generation = wrong.key.ue.generation.checked_add(1).unwrap();
    assert_eq!(
        radio.associate_core_identity(wrong, imsi()),
        Err(SignallingError::Origin)
    );
    assert_eq!(radio.resources(), before);
    assert_eq!(radio.subscriber_hint(first), first_hint);
    assert_eq!(radio.subscriber_hint(second), second_hint);
    assert_eq!(
        radio.connection(first, CnDomain::Cs),
        Some(first_connection)
    );
    assert_eq!(
        radio.connection(second, CnDomain::Cs),
        Some(second_connection)
    );
}

#[test]
fn contradictory_identity_is_counted_and_consumed_without_blocking_following_nas() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::common_identity(imsi(), Limits::default()).unwrap(),
    });
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::common_identity(
            Imsi::from_digits("991010000000001").unwrap(),
            Limits::default(),
        )
        .unwrap(),
    });
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context,
        ranap: ranap::downlink_direct(&[5, 0x02], None, Limits::default()).unwrap(),
    });
    assert!(drive(&mut link, &mut radio, 6).gateway.delivered);
    assert!(drive(&mut link, &mut radio, 7).gateway.delivered);
    assert_eq!(radio.counters().conflicting_core_identities, 1);
    assert!(drive(&mut link, &mut radio, 8).gateway.delivered);
    radio
        .associate_core_identity(radio.connection(key, CnDomain::Cs).unwrap(), imsi())
        .unwrap();
    assert_eq!(radio.registration_failure(key), None);
}

#[test]
fn both_domains_confirm_one_rrc_identity_and_stale_events_do_not_recreate_it() {
    let source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, _, mut ue) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let mut ps = INITIAL;
    ps[1] |= 0x80;
    send(
        &mut radio,
        key,
        umts_rrc::nas::SignallingBearer::NasAm,
        &mut ue,
        &ps,
    );
    assert_eq!(radio.process_one(), Some(Ok(key)));
    drive(&mut link, &mut radio, 6);
    for domain in [Domain::Cs, Domain::Ps] {
        dispatch(
            &mut radio,
            &event(key, domain, imsi()),
            ControlTime(7),
            &mut |_, _| Err::<(), _>(()),
        )
        .unwrap();
    }
    let old = radio.connection(key, CnDomain::Cs).unwrap();
    radio.retire(key).unwrap();
    assert_eq!(
        radio.associate_core_identity(old, imsi()),
        Err(SignallingError::Origin)
    );
    dispatch(
        &mut radio,
        &event(key, Domain::Cs, imsi()),
        ControlTime(8),
        &mut |_, _| Err::<(), _>(()),
    )
    .unwrap();
    assert!(radio.phase(key).is_none());
    assert!(radio.subscriber_hint(key).is_none());
    assert!(radio.permanent_identity(key).is_none());
}
