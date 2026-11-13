// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_iuh::session::Event;

#[test]
fn peer_disconnect_with_final_ranap_is_admitted_and_completed() {
    let mut env = environment();
    let config = config();
    let mut plan = BearerCoordinator::new(
        &config,
        at(0).epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let mut traffic = traffic(&config);
    let (key, context) = register(&mut env);
    env.with_parts(|core, radio, _, _, gateway, _| {
        gateway.rua(rua::Message::Disconnect {
            domain: Domain::Cs,
            context,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: Some(
                umts_iuh::ranap::release::Command {
                    cause: umts_iuh::ranap::rab::Cause::from_code(83).unwrap(),
                }
                .encode(Limits::default())
                .unwrap(),
            ),
        });
        core.drive(ControlTime(6), radio, |radio, event| {
            assert!(matches!(
                event,
                Event::Disconnected {
                    final_ranap: Some(_),
                    ..
                }
            ));
            plan.offer_core(radio, event)
        })
        .unwrap();
    });
    assert_eq!(plan.iu_releases.pending(), 1);
    for _ in 0..20 {
        service(&mut plan, &mut env, &mut traffic);
        if plan.iu_releases.pending() == 0 {
            break;
        }
    }
    assert_eq!(plan.iu_release_counts(), (1, 1));
    assert_eq!(
        env.controller
            .radio()
            .connection(key, umts_types::CnDomain::Cs),
        None
    );
}

#[test]
fn signalling_only_iu_release_uses_real_core_then_am_notification_without_retiring_rrc() {
    let mut env = environment();
    let config = config();
    let mut plan = BearerCoordinator::new(
        &config,
        at(0).epoch,
        env._preparation.source().information(),
        None,
    )
    .unwrap();
    let mut traffic = traffic(&config);
    plan.assignments = RabHandoff::new(Limits::default());
    let (key, context) = register(&mut env);
    let before = env.controller.radio().resources();
    let mut token = None;
    env.with_parts(|core, radio, _, _, gateway, _| {
        gateway.rua(rua::Message::Direct {
            domain: Domain::Cs,
            context,
            ranap: umts_iuh::ranap::release::Command {
                cause: umts_iuh::ranap::rab::Cause::from_code(83).unwrap(),
            }
            .encode(Limits::default())
            .unwrap(),
        });
        core.drive(ControlTime(6), radio, |_, event| {
            let Event::IuRelease { request, .. } = event else {
                panic!("expected Iu Release");
            };
            token = Some(*request);
            assert_eq!(plan.offer_iu_release(event), Ok(true));
            assert_eq!(plan.offer_iu_release(event), Ok(true));
            Ok::<_, ()>(())
        })
        .unwrap();
    });
    assert_eq!(plan.iu_releases.pending(), 1);
    for _ in 0..(usize::try_from(config.runtime.maximum_ues).unwrap() * 6 + 2) {
        service(&mut plan, &mut env, &mut traffic);
        if plan.iu_releases.pending() == 0 {
            break;
        }
    }
    assert_eq!(plan.iu_releases.pending(), 0);
    assert_eq!(plan.iu_release_counts(), (1, 1));
    assert!(
        !env.controller
            .core()
            .gateway()
            .owns_iu_release(token.unwrap())
    );
    assert_eq!(
        env.controller
            .radio()
            .connection(key, umts_types::CnDomain::Cs),
        None
    );
    assert!(env.controller.radio().phase(key).is_some());
    assert_eq!(env.controller.radio().resources(), before);
    assert_eq!(plan.outstanding(), 0);
    env.with_parts(|core, radio, _, _, gateway, _| {
        core.drive(ControlTime(11), radio, |_, _| Err::<(), _>(())).unwrap();
        assert!(matches!(gateway.last_rua(), rua::Message::Disconnect {
            domain: Domain::Cs, cause: rua::Cause::Radio(rua::RadioCause::Normal), ranap: Some(bytes), ..
        } if bytes == [0x20, 1, 0, 3, 0, 0, 0]));
    });
}
