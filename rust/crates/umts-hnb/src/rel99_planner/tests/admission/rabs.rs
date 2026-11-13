// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod release;
mod revocation;
use crate::{
    core_link::{
        CoreLink,
        tests::{Mock, Peer, ready, register},
    },
    rab_handoff::{PendingRabAssignment, RabHandoff},
    rel99_admission::{InitialRabHost, RabAdmissionError},
};
use umts_iuh::{
    ContextId, Domain, ranap::rab::Assignment, rua, session::ControlTime, wire::Limits,
};

fn connected() -> (
    CommonTx,
    CommonSignalling,
    CoreLink<Mock>,
    Peer,
    UeKey,
    ContextId,
) {
    let source = test_support::source(false);
    let mut radio = crate::signalling::tests::controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    let mut peer = crate::signalling::tests::peer(&source);
    peer.enqueue(&crate::signalling::tests::COMPLETE).unwrap();
    peer.publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    crate::signalling::tests::send_capability_report(&mut radio, key, &mut peer);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    (source, radio, link, core, key, context)
}

pub(crate) fn offer(
    link: &mut CoreLink<Mock>,
    radio: &mut CommonSignalling,
    peer: &Peer,
    context: ContextId,
    now: u64,
) -> PendingRabAssignment {
    offer_assignment(
        link,
        radio,
        peer,
        context,
        now,
        Assignment {
            setup: vec![ps_request(PACKET_RATE)],
            release: vec![],
        },
    )
}

fn offer_assignment(
    link: &mut CoreLink<Mock>,
    radio: &mut CommonSignalling,
    peer: &Peer,
    context: ContextId,
    now: u64,
    assignment: Assignment,
) -> PendingRabAssignment {
    peer.rua(rua::Message::Connect {
        domain: Domain::Ps,
        context,
        cause: rua::EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: assignment.encode(Limits::default()).unwrap(),
    });
    let mut handoff = RabHandoff::new(Limits::default());
    link.drive(ControlTime(now), radio, |_, event| {
        handoff.offer(event).map(|_| ())
    })
    .unwrap();
    handoff.take().unwrap().unwrap()
}

pub(crate) fn configured(radio: &CommonSignalling, key: UeKey) -> ConfiguredPolicy {
    let mut config: umts_config::Config = serde_yaml::from_str(include_str!(
        "../../../../../../../rust/test-support/fixtures/config/rel99.yaml"
    ))
    .unwrap();
    let retained = radio.retained_signalling(key).unwrap();
    let control = retained.iter().find(|rb| rb.id == 2).unwrap();
    config.rel99.physical.planner.signalling.um_uplink_bits = control.uplink_pdu_bits;
    config.rel99.physical.planner.signalling.um_downlink_bits = control.downlink_pdu_bits;
    let am = config.rel99.physical.planner.services.packet[0]
        .am
        .as_mut()
        .unwrap();
    am.ue.uplink_window = 32;
    am.ue.downlink_window = 32;
    let buffers = &mut am.buffers;
    buffers.maximum_sdu_bytes = 2048;
    buffers.maximum_buffer_bytes = 100_000;
    ConfiguredPolicy::new(&config).unwrap()
}

#[test]
fn committed_rab_builds_real_owners_and_keeps_correlation_until_retirement() {
    let (mut source, mut radio, mut link, peer, key, context) = connected();
    let pending = offer(&mut link, &mut radio, &peer, context, 6);
    let request = pending.request();
    let policy = configured(&radio, key);
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let (_tx, ports) = transmit(&source);
    let (_rx, mut receive) = receive(key.epoch, 8);
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
    assert_eq!(admission.phase(identity), Some(AdmissionPhase::Preparing));
    assert_eq!(
        admission.rab_assignment(identity).unwrap().request(),
        request
    );
    assert_eq!(resources.outstanding(), 1);
    assert!(radio.dedicated_route(key).is_none());
    for _ in 0..2 {
        let mut callbacks = 0;
        let mut observed = None;
        for _ in 0..admission.capacity() {
            let before = callbacks;
            let visited = admission.visit_rab_assignments(count(1), |_, current| {
                callbacks += 1;
                observed = Some(current);
            });
            assert_eq!(visited, callbacks - before);
        }
        assert_eq!(callbacks, 1);
        assert_eq!(observed, Some(identity));
    }
    let visited = admission.visit_rab_assignments(count(16), |owner, current| {
        assert_eq!(current, identity);
        owner.cancel(current).unwrap();
    });
    assert_eq!(visited, 1);
    let _writer = source.submission_writer().unwrap();
    let mut common = source.control_port(count(8), count(8)).unwrap();
    for _ in 0..16 {
        admission
            .step_with_policy(
                at(0),
                &mut radio,
                &mut receive,
                &mut common,
                &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
            )
            .unwrap();
        if admission.phase(identity) == Some(AdmissionPhase::Retired) {
            break;
        }
    }
    let retired = admission.take_retired(identity).unwrap();
    assert_eq!(
        admission.visit_rab_assignments(count(16), |_, _| panic!("retired assignment revisited")),
        0
    );
    assert_eq!(retired.rab_assignment.unwrap().request(), request);
    resources.release(retired.resources.unwrap()).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn invalid_owner_timing_rolls_back_codes_and_preserves_request_for_retry() {
    let (source, mut radio, mut link, peer, key, context) = connected();
    let pending = offer(&mut link, &mut radio, &peer, context, 6);
    let request = pending.request();
    let policy = configured(&radio, key);
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let (_tx, ports) = transmit(&source);
    let (_rx, receive) = receive(key.epoch, 8);
    let mut admission = Rel99Admission::new(ports.control, count(8), admission_limits())
        .ok()
        .unwrap();
    let mut bad = settings(key.epoch);
    bad.samples.search_radius_samples = 0;
    let rejected = match (InitialRabHost {
        gateway: link.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    })
    .begin(pending, bad)
    {
        Err(rejected) => rejected,
        Ok(_) => panic!("invalid timing admitted"),
    };
    assert!(matches!(
        rejected.error,
        RabAdmissionError::Owners(OwnerBuildError::Receive(_))
    ));
    assert!(rejected.rollback_error.is_none());
    assert!(rejected.allocation.is_none());
    assert_eq!(rejected.pending.request(), request);
    assert_eq!(resources.outstanding(), 0);
    peer.rua(rua::Message::Disconnect {
        domain: Domain::Ps,
        context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    link.drive(ControlTime(7), &mut radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let stale = match (InitialRabHost {
        gateway: link.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    })
    .begin(rejected.pending, settings(key.epoch))
    {
        Err(rejected) => rejected,
        Ok(_) => panic!("stale request admitted"),
    };
    assert!(matches!(stale.error, RabAdmissionError::Authority(_)));
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn selected_settings_factory_failure_preserves_all_rabs_and_retries_the_actual_allocation() {
    for items in [1, 2] {
        let (mut source, mut radio, mut link, peer, key, context) = connected();
        let mut requests = Vec::new();
        for index in 0..items {
            let mut item = ps_request(PACKET_RATE);
            item.first.id = umts_iuh::ranap::rab::First::modification(PS_RAB + index).id;
            requests.push(item);
        }
        let pending = offer_assignment(
            &mut link,
            &mut radio,
            &peer,
            context,
            6,
            Assignment {
                setup: requests,
                release: vec![],
            },
        );
        let request = pending.request();
        let original = pending.assignment().encode(Limits::default()).unwrap();
        let policy = configured(&radio, key);
        let cell = test_support::plan(false);
        let mut resources =
            RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
        let (tx, ports) = transmit(&source);
        let (_rx, mut receive) = receive(key.epoch, 8);
        let mut admission = Rel99Admission::new(ports.control, count(8), admission_limits())
            .ok()
            .unwrap();
        let charged = radio.resources();
        let queued = link.gateway().queued_frames();
        let mut calls = 0;
        let mut failed_identity = None;
        let rejected = match (InitialRabHost {
            gateway: link.gateway(),
            policy: &policy,
            radio: &mut radio,
            resources: &mut resources,
            receive: &receive,
            admission: &mut admission,
        })
        .begin_with_settings(pending, |prepared| {
            calls += 1;
            assert_eq!(prepared.allocation.key(), key);
            assert_eq!(prepared.rabs.len(), usize::from(items));
            assert_eq!(prepared.setup.rabs().len(), usize::from(items));
            for (index, rab) in prepared.rabs.iter().enumerate() {
                assert_eq!(rab.domain, CnDomain::Ps);
                assert_eq!(rab.rab, PS_RAB + index as u8);
                assert_eq!(rab.bearers.len(), 1);
            }
            let plan = prepared.allocation.plan();
            assert!(std::sync::Arc::ptr_eq(
                prepared.setup.configuration(),
                plan.configuration()
            ));
            assert!(std::sync::Arc::ptr_eq(prepared.profiles.plan(), plan));
            assert!((17..=24).contains(&plan.configuration().physical().uplink_scrambling));
            failed_identity = Some(prepared.allocation.receive_identity());
            Err(OwnerBuildError::Phase)
        }) {
            Err(rejected) => rejected,
            Ok(_) => panic!("rejected factory admitted owners"),
        };
        assert_eq!(calls, 1);
        assert!(matches!(
            rejected.error,
            RabAdmissionError::OwnerSettings(OwnerBuildError::Phase)
        ));
        assert_eq!(rejected.pending.request(), request);
        assert_eq!(
            rejected
                .pending
                .assignment()
                .encode(Limits::default())
                .unwrap(),
            original
        );
        assert!(rejected.rollback_error.is_none());
        assert!(rejected.allocation.is_none());
        assert_eq!(resources.outstanding(), 0);
        assert_eq!(radio.resources(), charged);
        assert_eq!(link.gateway().queued_frames(), queued);
        assert!(radio.dedicated_route(key).is_none());
        assert!(tx.bank().session(failed_identity.unwrap()).is_none());
        assert!(receive.phase(failed_identity.unwrap()).is_none());
        for item in &rejected.pending.assignment().setup {
            assert!(
                link.gateway()
                    .authorize_rab(
                        request,
                        item.first.id.get(),
                        umts_iuh::session::RabOperation::SetupOrModify
                    )
                    .is_ok()
            );
        }
        let mut accepted_plan = None;
        let identity = InitialRabHost {
            gateway: link.gateway(),
            policy: &policy,
            radio: &mut radio,
            resources: &mut resources,
            receive: &receive,
            admission: &mut admission,
        }
        .begin_with_settings(rejected.pending, |prepared| {
            calls += 1;
            assert_ne!(
                Some(prepared.allocation.receive_identity()),
                failed_identity
            );
            assert_eq!(prepared.rabs.len(), usize::from(items));
            accepted_plan = Some(prepared.allocation.plan().clone());
            Ok(settings(key.epoch))
        })
        .unwrap_or_else(|error| panic!("factory retry rejected: {:?}", error.error));
        assert_eq!(calls, 2);
        assert_eq!(resources.outstanding(), 1);
        assert_eq!(admission.phase(identity), Some(AdmissionPhase::Preparing));
        assert!(std::sync::Arc::ptr_eq(
            admission.procedure(identity).unwrap().configuration(),
            accepted_plan.as_ref().unwrap().configuration()
        ));
        assert_eq!(
            admission.rab_assignment(identity).unwrap().request(),
            request
        );
        assert_eq!(link.gateway().queued_frames(), queued);
        admission.cancel(identity).unwrap();
        let _writer = source.submission_writer().unwrap();
        let mut common = source.control_port(count(8), count(8)).unwrap();
        for _ in 0..16 {
            admission
                .step_with_policy(
                    at(0),
                    &mut radio,
                    &mut receive,
                    &mut common,
                    &mut TrafficFramePolicy::new(EmptyTransparentTraffic, count(1024)),
                )
                .unwrap();
            if admission.phase(identity) == Some(AdmissionPhase::Retired) {
                break;
            }
        }
        let retired = admission.take_retired(identity).unwrap();
        assert_eq!(retired.rab_assignment.unwrap().request(), request);
        resources.release(retired.resources.unwrap()).unwrap();
        assert_eq!(resources.outstanding(), 0);
    }
}

#[test]
fn stale_authority_rejects_before_the_selected_settings_factory_can_run() {
    let (source, mut radio, mut link, peer, key, context) = connected();
    let pending = offer(&mut link, &mut radio, &peer, context, 6);
    let request = pending.request();
    peer.rua(rua::Message::Disconnect {
        domain: Domain::Ps,
        context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    link.drive(ControlTime(7), &mut radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let policy = configured(&radio, key);
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let (_tx, ports) = transmit(&source);
    let (_rx, receive) = receive(key.epoch, 8);
    let mut admission = Rel99Admission::new(ports.control, count(8), admission_limits())
        .ok()
        .unwrap();
    let mut calls = 0;
    let rejected = match (InitialRabHost {
        gateway: link.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    })
    .begin_with_settings(pending, |_| {
        calls += 1;
        Ok(settings(key.epoch))
    }) {
        Err(rejected) => rejected,
        Ok(_) => panic!("stale request admitted"),
    };
    assert_eq!(calls, 0);
    assert!(matches!(rejected.error, RabAdmissionError::Authority(_)));
    assert_eq!(rejected.pending.request(), request);
    assert!(rejected.allocation.is_none());
    assert!(rejected.rollback_error.is_none());
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn core_revocation_during_preparation_cancels_unpublished_allocation() {
    let (source, mut radio, mut link, peer, key, context) = connected();
    let pending = offer(&mut link, &mut radio, &peer, context, 6);
    let request = pending.request();
    let policy = configured(&radio, key);
    let cell = test_support::plan(false);
    let mut resources =
        RadioResources::new(key.epoch, cell.information(), count(8), 17..=24).unwrap();
    let (_tx, ports) = transmit(&source);
    let (_rx, receive) = receive(key.epoch, 8);
    let mut admission = Rel99Admission::new(ports.control, count(8), admission_limits())
        .ok()
        .unwrap();
    let charged = radio.resources();
    let prepared = (InitialRabHost {
        gateway: link.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    })
    .prepare(pending)
    .unwrap_or_else(|_| panic!("prepare canonical packet request"));
    assert_eq!(resources.outstanding(), 1);
    assert_eq!(radio.resources(), charged);
    assert!(radio.dedicated_route(key).is_none());
    assert_eq!(prepared.radio().allocation.key(), key);

    peer.rua(rua::Message::Disconnect {
        domain: Domain::Ps,
        context,
        cause: rua::Cause::Radio(rua::RadioCause::NetworkRelease),
        ranap: None,
    });
    link.drive(ControlTime(7), &mut radio, |_, _| Ok::<_, ()>(()))
        .unwrap();
    let rejected = (InitialRabHost {
        gateway: link.gateway(),
        policy: &policy,
        radio: &mut radio,
        resources: &mut resources,
        receive: &receive,
        admission: &mut admission,
    })
    .begin_prepared(prepared, |_| {
        panic!("revoked request must not construct workers")
    })
    .expect_err("revoked request");
    assert!(matches!(rejected.error, RabAdmissionError::Authority(_)));
    assert_eq!(rejected.pending.request(), request);
    assert!(rejected.allocation.is_none());
    assert!(rejected.rollback_error.is_none());
    assert_eq!(resources.outstanding(), 0);
    assert!(radio.dedicated_route(key).is_none());
}
