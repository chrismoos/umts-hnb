// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_rx::ReceiveIdentity,
    rel99_admission::{CoreReleaseError, RabReleaseRequest},
};
use umts_iuh::{
    ranap::rab::{AssignmentResponse, Cause, Release},
    session::{RabOperation, RabRequest},
};
use umts_rrc::dedicated::NewRab;

struct Environment {
    _source: CommonTx,
    radio: CommonSignalling,
    link: CoreLink<Mock>,
    peer: Peer,
    context: ContextId,
    source: RabRequest,
    identity: ReceiveIdentity,
    plan: Arc<crate::rel99_plan::CompiledRel99>,
    inventory: Vec<NewRab>,
    domain: Domain,
}
fn assignment(
    env: &mut Environment,
    now: u64,
    setup: Vec<SetupOrModify>,
    release: &[u8],
    connect: bool,
) -> PendingRabAssignment {
    let ranap = Assignment {
        setup,
        release: release
            .iter()
            .map(|id| Release::new(*id, Cause::from_code(32).unwrap()))
            .collect(),
    }
    .encode(Limits::default())
    .unwrap();
    env.peer.rua(if connect {
        rua::Message::Connect {
            domain: env.domain,
            context: env.context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap,
        }
    } else {
        rua::Message::Direct {
            domain: env.domain,
            context: env.context,
            ranap,
        }
    });
    let mut handoff = RabHandoff::new(Limits::default());
    env.link
        .drive(ControlTime(now), &mut env.radio, |_, event| {
            handoff.offer(event).map(|_| ())
        })
        .unwrap();
    handoff.take().unwrap().unwrap()
}
fn environment(domain: Domain) -> Environment {
    let (source, mut radio, mut link, peer, key, context) = connected();
    let item = if domain == Domain::Ps {
        ps_request(PACKET_RATE)
    } else {
        cs_request()
    };
    let mut handoff = RabHandoff::new(Limits::default());
    let ranap = Assignment {
        setup: vec![item.clone()],
        release: vec![],
    }
    .encode(Limits::default())
    .unwrap();
    peer.rua(if domain == Domain::Ps {
        rua::Message::Connect {
            domain,
            context,
            cause: rua::EstablishmentCause::Normal,
            selector: None,
            membership: None,
            ranap,
        }
    } else {
        rua::Message::Direct {
            domain,
            context,
            ranap,
        }
    });
    link.drive(ControlTime(6), &mut radio, |_, event| {
        handoff.offer(event).map(|_| ())
    })
    .unwrap();
    let source_request = handoff.take().unwrap().unwrap().request();
    let policy = configured(&radio, key);
    let planned = if domain == Domain::Ps {
        let selected = policy.services().select(&[(CnDomain::Ps, &item)]).unwrap();
        policy.plan_initial(key, &radio, selected.rabs()).unwrap()
    } else {
        let circuit = circuit(&item);
        policy
            .plan_initial(
                key,
                &radio,
                &[rab(CnDomain::Cs, &item, Service::Circuit(&circuit))],
            )
            .unwrap()
    };
    Environment {
        _source: source,
        radio,
        link,
        peer,
        context,
        source: source_request,
        identity: ReceiveIdentity {
            ue: key,
            configuration: NonZeroU64::MIN,
        },
        plan: planned.compiled().clone(),
        inventory: planned.setup().rabs().to_vec(),
        domain,
    }
}
fn prepare(
    env: &Environment,
    pending: PendingRabAssignment,
) -> Result<RabReleaseRequest, crate::rel99_admission::RejectedRabRelease> {
    RabReleaseRequest::prepare(
        pending,
        env.source,
        env.identity,
        env.plan.clone(),
        &env.inventory,
        env.link.gateway(),
    )
}
fn disconnect(env: &mut Environment, now: u64) {
    env.peer.rua(rua::Message::Disconnect {
        domain: env.domain,
        context: env.context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    crate::core_link::tests::finish_peer_disconnect(&mut env.link, &mut env.radio, now);
}

#[test]
fn fresh_release_uses_new_same_connection_authority_and_waits_for_actual_radio_commit() {
    let mut env = environment(Domain::Ps);
    let pending = assignment(&mut env, 7, vec![], &[PS_RAB], false);
    let request = pending.request();
    assert_ne!(request, env.source);
    assert!(request.same_connection(env.source));
    assert!(
        env.link
            .gateway()
            .authorize_rab(request, PS_RAB, RabOperation::Release)
            .is_ok()
    );
    assert!(
        env.link
            .gateway()
            .authorize_rab(request, PS_RAB, RabOperation::SetupOrModify)
            .is_err()
    );
    let mut release = prepare(&env, pending).unwrap();
    assert_eq!(release.request(), request);
    assert!(release.needs_radio());
    assert_eq!(
        release
            .radio_bearers()
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>(),
        env.inventory[0].bearers
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(4))
            .unwrap(),
        0
    );
    assert!(!release.outcomes_recorded());
    assert!(
        !env.link
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
}

#[test]
fn original_or_setup_assignment_cannot_be_reused_as_release_authority() {
    let mut env = environment(Domain::Ps);
    let pending = assignment(&mut env, 7, vec![], &[PS_RAB], false);
    let request = pending.request();
    let rejected = RabReleaseRequest::prepare(
        pending,
        request,
        env.identity,
        env.plan.clone(),
        &env.inventory,
        env.link.gateway(),
    )
    .unwrap_err();
    assert!(matches!(rejected.error, CoreReleaseError::Identity));
    assert_eq!(rejected.pending.request(), request);
    let pending = assignment(&mut env, 8, vec![ps_request(PACKET_RATE)], &[], false);
    let request = pending.request();
    let rejected = prepare(&env, pending).unwrap_err();
    assert!(matches!(rejected.error, CoreReleaseError::Identity));
    assert_eq!(rejected.pending.request(), request);
}

#[test]
fn release_collects_every_cs_subflow_and_rejects_partial_or_duplicate_metadata() {
    let mut env = environment(Domain::Cs);
    assert_eq!(env.inventory[0].bearers.len(), 3);
    let pending = assignment(&mut env, 7, vec![], &[CS_RAB], false);
    let release = prepare(&env, pending).unwrap();
    assert_eq!(release.radio_bearers().len(), 3);
    for case in 0..3 {
        let pending = assignment(&mut env, 8 + case, vec![], &[CS_RAB], false);
        let mut inventory = env.inventory.clone();
        match case {
            0 => inventory[0].bearers.push(31),
            1 => {
                let repeated = inventory[0].bearers[0];
                inventory[0].bearers.push(repeated);
            }
            _ => inventory[0].bearers.truncate(1),
        }
        let request = pending.request();
        let rejected = RabReleaseRequest::prepare(
            pending,
            env.source,
            env.identity,
            env.plan.clone(),
            &inventory,
            env.link.gateway(),
        )
        .unwrap_err();
        assert!(matches!(rejected.error, CoreReleaseError::Mapping));
        assert_eq!(rejected.pending.request(), request);
    }
}

#[test]
fn mixed_known_unknown_release_records_only_invalid_id_until_radio_proof_exists() {
    let mut env = environment(Domain::Ps);
    let pending = assignment(&mut env, 7, vec![], &[PS_RAB, 245], false);
    let request = pending.request();
    let mut release = prepare(&env, pending).unwrap();
    assert!(release.needs_radio());
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(4))
            .unwrap(),
        1
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(4))
            .unwrap(),
        0
    );
    assert!(!release.outcomes_recorded());
    assert!(
        env.link
            .gateway()
            .authorize_rab(request, PS_RAB, RabOperation::Release)
            .is_ok()
    );
    assert!(
        env.link
            .gateway()
            .authorize_rab(request, 245, RabOperation::Release)
            .is_err()
    );
    assert!(
        !env.link
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
}

#[test]
fn unknown_only_release_keeps_canonical_outcome_through_time_failure_and_records_once() {
    let mut env = environment(Domain::Ps);
    let pending = assignment(&mut env, 7, vec![], &[245, 246], false);
    let mut release = prepare(&env, pending).unwrap();
    assert!(!release.needs_radio());
    assert!(release.radio_bearers().is_empty());
    assert!(
        release
            .record_ready(&mut env.link, ControlTime(6), count(1))
            .is_err()
    );
    assert!(!release.outcomes_recorded());
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(1))
            .unwrap(),
        1
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(8), count(1))
            .unwrap(),
        1
    );
    assert!(release.outcomes_recorded());
    env.peer.set_blocked(true);
    assert!(
        env.link
            .drive(ControlTime(8), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    env.peer.set_blocked(false);
    env.link
        .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected release outcome")
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, Limits::default())
        .unwrap()
        .0;
    assert!(response.released.is_empty());
    assert!(response.setup.is_empty());
    assert_eq!(
        response
            .release_failed
            .iter()
            .map(|item| (item.id.get(), item.cause.code()))
            .collect::<Vec<_>>(),
        [(245, 30), (246, 30)]
    );
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(9), count(4))
            .unwrap(),
        0
    );
    assert!(
        !env.link
            .drive(ControlTime(10), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
}

#[test]
fn revoked_and_disconnected_release_requests_settle_without_recording_or_reauthorization() {
    for disconnected in [false, true] {
        let mut env = environment(Domain::Ps);
        let pending = assignment(&mut env, 7, vec![], &[245], false);
        let mut release = prepare(&env, pending).unwrap();
        let request = release.request();
        if disconnected {
            disconnect(&mut env, 8);
        } else {
            let _replacement = assignment(&mut env, 8, vec![], &[245], false);
        }
        assert_eq!(
            release
                .record_ready(&mut env.link, ControlTime(9), count(1))
                .unwrap(),
            0
        );
        assert!(release.outcomes_recorded());
        assert_eq!(
            release.progress(),
            crate::rel99_admission::ReleaseOutcomeProgress {
                items: 1,
                recorded: 0,
                superseded: 1,
            }
        );
        assert!(
            env.link
                .gateway()
                .authorize_rab(request, 245, RabOperation::Release)
                .is_err()
        );
        assert_eq!(
            release
                .record_ready(&mut env.link, ControlTime(9), count(1))
                .unwrap(),
            0
        );
        assert!(
            !env.link
                .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
                .unwrap()
                .rab_published
        );
    }
    let mut env = environment(Domain::Ps);
    let pending = assignment(&mut env, 7, vec![], &[245], false);
    let _replacement = assignment(&mut env, 8, vec![], &[245], false);
    let rejected = prepare(&env, pending).unwrap_err();
    assert!(matches!(rejected.error, CoreReleaseError::Authority(_)));
    let mut env = environment(Domain::Ps);
    disconnect(&mut env, 7);
    let pending = assignment(&mut env, 8, vec![], &[245], true);
    assert!(!pending.request().same_connection(env.source));
    let mut release = prepare(&env, pending).unwrap();
    assert!(!release.needs_radio());
    assert_eq!(
        release
            .record_ready(&mut env.link, ControlTime(9), count(1))
            .unwrap(),
        1
    );
    assert_eq!(
        release.progress(),
        crate::rel99_admission::ReleaseOutcomeProgress {
            items: 1,
            recorded: 1,
            superseded: 0,
        }
    );
    assert!(
        env.link
            .drive(ControlTime(9), &mut env.radio, |_, _| Ok::<_, ()>(()))
            .unwrap()
            .rab_published
    );
    let rua::Message::Direct {
        domain,
        context,
        ranap,
    } = env.peer.last_rua()
    else {
        panic!("expected reopened-domain unknown release response")
    };
    assert_eq!((domain, context), (Domain::Ps, env.context));
    let response = AssignmentResponse::decode(&ranap, Limits::default())
        .unwrap()
        .0;
    assert!(response.released.is_empty());
    assert!(response.setup.is_empty());
    assert!(response.failed.is_empty());
    assert_eq!(
        response
            .release_failed
            .iter()
            .map(|item| (item.id.get(), item.cause.code()))
            .collect::<Vec<_>>(),
        [(245, 30)]
    );
}
