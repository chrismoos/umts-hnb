// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::CommonTxPort, dedicated_controller::DedicatedPhase, dedicated_rx::ReceiveIdentity,
};
use umts_iuh::{
    ranap::rab::{Cause, Release},
    session::RabRequest,
};

struct Environment {
    _source: CommonTx,
    _writer: crate::tx_evidence::Writer,
    _feedback: crate::dedicated_tx::DedicatedTpcPort,
    radio: CommonSignalling,
    link: CoreLink<Mock>,
    peer: Peer,
    context: ContextId,
    resources: RadioResources,
    rx: DedicatedSampleRuntime,
    receive: DedicatedController,
    tx: DedicatedTransmitRuntime,
    admission: Rel99Admission,
    common: CommonTxPort,
    identity: ReceiveIdentity,
    request: RabRequest,
}

fn environment() -> Environment {
    let (mut source, mut radio, mut link, peer, key, context) = connected();
    let pending = offer(&mut link, &mut radio, &peer, context, 6);
    let request = pending.request();
    let policy = configured(&radio, key);
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let (tx, ports) = transmit(&source);
    let (rx, receive) = receive(key.epoch, 8);
    let mut admission = Rel99Admission::new(ports.control, count(8), admission_limits())
        .ok()
        .unwrap();
    let identity = InitialRabHost {
        gateway: link.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    }
    .begin(pending, settings(key.epoch))
    .unwrap_or_else(|error| panic!("{:?}", error.error));
    let writer = source.submission_writer().unwrap();
    let common = source.control_port(count(8), count(8)).unwrap();
    Environment {
        _source: source,
        _writer: writer,
        _feedback: ports.feedback,
        radio,
        link,
        peer,
        context,
        resources,
        rx,
        receive,
        tx,
        admission,
        common,
        identity,
        request,
    }
}

fn revoke(env: &mut Environment, release: bool) {
    let message = if release {
        rua::Message::Direct {
            domain: Domain::Ps,
            context: env.context,
            ranap: Assignment {
                setup: vec![],
                release: vec![Release::new(PS_RAB, Cause::from_code(32).unwrap())],
            }
            .encode(Limits::default())
            .unwrap(),
        }
    } else {
        rua::Message::Disconnect {
            domain: Domain::Ps,
            context: env.context,
            cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
            ranap: None,
        }
    };
    env.peer.rua(message);
    env.link
        .drive(ControlTime(7), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    assert!(
        env.admission
            .rab_assignment(env.identity)
            .unwrap()
            .authorize_items(env.link.gateway())
            .is_err()
    );
}

fn retire(mut env: Environment, with_core: bool) {
    assert!(env.admission.take_retired(env.identity).is_err());
    assert_eq!(env.resources.outstanding(), 1);
    for _ in 0..32 {
        env.rx.service(|_, result| {
            result.unwrap();
        });
        env.tx.service().unwrap();
        let mut traffic = TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024));
        let report = if with_core {
            env.admission.step_with_core(
                at(0),
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut traffic,
                env.link.gateway(),
            )
        } else {
            env.admission.step_with_policy(
                at(0),
                &mut env.radio,
                &mut env.receive,
                &mut env.common,
                &mut traffic,
            )
        }
        .unwrap();
        assert_eq!(report.published, 0);
        assert_eq!(env.resources.outstanding(), 1);
        assert_eq!(
            env.admission
                .rab_assignment(env.identity)
                .unwrap()
                .request(),
            env.request
        );
        if env.admission.phase(env.identity) == Some(AdmissionPhase::Retired) {
            break;
        }
    }
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Retired)
    );
    let retired = env.admission.take_retired(env.identity).unwrap();
    assert_eq!(retired.error, AdmissionError::Cancelled);
    assert!(retired.procedure.receipt().is_none());
    assert_eq!(retired.rab_assignment.unwrap().request(), env.request);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    env.resources.release(retired.resources.unwrap()).unwrap();
    assert_eq!(env.resources.outstanding(), 0);
}

#[test]
fn release_or_disconnect_before_radio_progress_revokes_initial_admission() {
    for release in [true, false] {
        let mut env = environment();
        revoke(&mut env, release);
        retire(env, true);
    }
}

#[test]
fn fresh_authority_reaches_actual_receive_worker_then_revocation_returns_it() {
    let mut env = environment();
    env.admission
        .step_with_core(
            at(0),
            &mut env.radio,
            &mut env.receive,
            &mut env.common,
            &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
            env.link.gateway(),
        )
        .unwrap();
    assert_eq!(
        env.receive.phase(env.identity),
        Some(DedicatedPhase::Preparing)
    );
    for _ in 0..8 {
        env.rx.service(|_, result| {
            result.unwrap();
        });
        env.receive.poll_receipts();
        if env.receive.phase(env.identity) == Some(DedicatedPhase::Prepared) {
            break;
        }
    }
    assert!(env.rx.counters().commands > 0);
    assert_eq!(
        env.receive.phase(env.identity),
        Some(DedicatedPhase::Prepared)
    );
    assert_eq!(
        env.admission.phase(env.identity),
        Some(AdmissionPhase::Preparing)
    );
    revoke(&mut env, false);
    retire(env, true);
}

#[test]
fn legacy_step_fails_closed_for_core_correlated_initial_setup() {
    let env = environment();
    env.admission
        .rab_assignment(env.identity)
        .unwrap()
        .authorize_items(env.link.gateway())
        .unwrap();
    retire(env, false);
}
