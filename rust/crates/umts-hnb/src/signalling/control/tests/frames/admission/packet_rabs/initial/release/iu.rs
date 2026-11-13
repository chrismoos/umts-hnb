// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn release_token(env: &mut InitialEnvironment) -> umts_iuh::session::IuReleaseRequest {
    env.peer.rua(rua::Message::Direct {
        context: env.context,
        domain: Domain::Ps,
        ranap: umts_iuh::ranap::release::Command {
            cause: umts_iuh::ranap::rab::Cause::from_code(83).unwrap(),
        }
        .encode(WireLimits::default())
        .unwrap(),
    });
    let mut token = None;
    env.core
        .drive(ControlTime(9), &mut env.radio, |_, event| {
            let umts_iuh::session::Event::IuRelease { request, .. } = event else {
                panic!("expected connection release");
            };
            token = Some(*request);
            Ok::<_, ()>(())
        })
        .unwrap();
    token.unwrap()
}

#[test]
fn connection_release_revokes_iu_media_and_publishes_radio_release_without_rab_outcomes() {
    let mut env = recovery::active();
    let previous = env.identity;
    let route = RouteKey {
        connection: umts_types::ConnectionKey {
            ue: env.key,
            domain: CnDomain::Ps,
        },
        rab: INITIAL_RAB,
    };
    assert!(env.packets.requested(route).is_some());
    let token = release_token(&mut env);
    assert!(token.same_connection(env.request));
    let media = env.packets.reconcile_core(env.core.gateway()).unwrap();
    assert_eq!(media.active_retired, 1);
    assert!(!env.packets.has_connection(route.connection));
    assert!(!env.packets.iu_volume_reports_pending(token));
    let authority = env
        .admission
        .prepare_iu_radio_release(previous, token, env.core.gateway())
        .unwrap();
    assert_eq!(authority.radio_bearers().len(), 1);
    let now = at(env.source.next_chip().index + 120);
    let old = env.admission.active_radio(previous).unwrap().plan().clone();
    control::install_integrity(&mut env.radio, env.key);
    let scheduled = scheduled(&mut env, now);
    let successor = scheduled.pool.identity();
    let target = scheduled.pool.plan().clone();
    let activation = scheduled.procedure.activation().time();
    assert_eq!(
        env.admission
            .begin_iu_radio_release(
                scheduled,
                authority,
                env.core.gateway(),
                &env.radio,
                &mut env.receive
            )
            .unwrap(),
        successor
    );
    assert_eq!(
        env.admission
            .record_release_outcomes(previous, &mut env.core, ControlTime(9), count(1))
            .unwrap(),
        0
    );
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert!(env.core.gateway().owns_iu_release(token));
    assert!(!env.admission.completed_iu_radio_release(token));
    let mut air = AirPeer {
        previous,
        successor,
        activation,
        old,
        target,
        pending: Vec::new(),
        statuses: 0,
    };
    for _ in 0..COMMAND_VISITS {
        release_step(&mut env, previous, now, &mut air);
        if env.admission.release_phase(previous) == Some(ScheduledReleasePhase::AwaitingComplete) {
            break;
        }
    }
    assert_eq!(
        env.admission.release_phase(previous),
        Some(ScheduledReleasePhase::AwaitingComplete)
    );
    assert!(env.core.gateway().owns_iu_release(token));
    assert_eq!(env.core.gateway().queued_frames(), 0);
    assert_eq!(env.radio.dedicated_route(env.key), Some(previous));
}

#[test]
fn requested_volume_report_remains_owned_after_iu_transport_removal() {
    let mut requested = item(INITIAL_RAB);
    requested.first.parameters.as_mut().unwrap().maximum =
        qos::MaximumRates::new(&[INITIAL_RATE]).unwrap();
    requested.second.reporting = Some(umts_iuh::ranap::rab::Reporting::Report);
    let mut env = environment_for(Domain::Ps, vec![requested], 2);
    prepare(&mut env);
    start(&mut env);
    complete_radio(&mut env);
    env.admission
        .complete_packets(
            env.identity,
            &mut env.core,
            &mut env.packets,
            &mut env.receive,
            ControlTime(7),
        )
        .unwrap();
    env.core
        .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let token = release_token(&mut env);
    assert!(!env.packets.iu_volume_reports_pending(token));
    let result = env.packets.reconcile_core(env.core.gateway()).unwrap();
    assert_eq!(result.active_retired, 1);
    assert!(env.packets.iu_volume_reports_pending(token));
    assert!(!env.packets.has_connection(umts_types::ConnectionKey {
        ue: env.key,
        domain: CnDomain::Ps
    }));
    assert_eq!(
        env.packets
            .reconcile_core(env.core.gateway())
            .unwrap()
            .active_retired,
        0
    );
    assert!(env.packets.iu_volume_reports_pending(token));
    env.peer.rua(rua::Message::Disconnect {
        context: env.context,
        domain: Domain::Ps,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    env.core.drive(ControlTime(10), &mut env.radio, |_, event| {
        assert!(matches!(event, umts_iuh::session::Event::Disconnected { request, .. } if *request == token));
        Ok::<_, ()>(())
    }).unwrap();
    assert!(env.core.gateway().owns_iu_release(token));
    env.packets.reconcile_core(env.core.gateway()).unwrap();
    assert!(!env.packets.iu_volume_reports_pending(token));
}
