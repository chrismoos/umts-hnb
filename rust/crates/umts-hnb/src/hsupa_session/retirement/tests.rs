// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{DedicatedTransmitRuntime, TransmitRuntimeLimits},
    hsupa_session::Phase,
    test_support::{at, count, source},
};
use umts_dsp::hsupa::{AbsoluteGrant, GRANT_FRAME_OFFSET_CHIPS, GrantScope};
use umts_types::hsupa::Tti;

#[test]
fn resource_release_preserves_lease_on_wrong_pool_and_is_idempotent() {
    let source = source(true);
    let (mut resources, mut bank, radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let identity = identities[0];
    bank.begin_retirement(identity.ue).unwrap();
    let lease = bank.take_retired(identity.ue, at(0)).unwrap().unwrap();
    let mut retirement = Retirement {
        identity,
        e_rnti: lease.e_rnti(),
        receiver: None,
        component: None,
        stage: Stage::Retired(lease),
    };
    let cell = crate::test_support::plan(false);
    let mut foreign = crate::radio_resources::RadioResources::new(
        identity.ue.epoch,
        cell.information(),
        count(2),
        17..=30,
    )
    .unwrap();
    assert!(retirement.release_resources(&mut foreign).is_err());
    assert_eq!(retirement.phase(), Phase::Retired);
    assert!(retirement.release_resources(&mut resources).unwrap());
    assert!(retirement.release_resources(&mut resources).unwrap());
    bank.begin_retirement(identities[1].ue).unwrap();
    resources
        .release_hsupa_user(bank.take_retired(identities[1].ue, at(0)).unwrap().unwrap())
        .unwrap();
    for radio in radios {
        resources.release(radio).unwrap();
    }
    resources
        .release_hsupa(bank.release().unwrap_or_else(|_| panic!("empty bank")))
        .unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn released_user_keeps_lease_until_controls_and_air_tail_are_retired() {
    let source = source(true);
    let (mut resources, mut bank, radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let identity = identities[0];
    bank.begin_retirement(identity.ue).unwrap();
    let lease = bank.take_retired(identity.ue, at(0)).unwrap().unwrap();
    let e_rnti = lease.e_rnti();
    bank.register(lease, identity, Tti::TwoMilliseconds)
        .unwrap();
    bank.queue_grant(
        identity,
        at(GRANT_FRAME_OFFSET_CHIPS),
        AbsoluteGrant::new(10, GrantScope::AllProcesses).unwrap(),
        0.1,
    )
    .unwrap();
    let (mut runtime, mut ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(8),
        count(1_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(4),
            commands_per_step: count(2),
            feedback_capacity: count(4),
            feedback_per_step: count(2),
            maximum_inflight_retained_bits: count(8_000_000),
            maximum_frame_payload_bits: count(100_000),
        },
    )
    .unwrap();
    assert!(runtime.attach_hsupa(bank).is_ok());
    let mut retirement = Retirement {
        identity,
        e_rnti,
        receiver: None,
        component: None,
        stage: Stage::Start,
    };
    retirement.service(true, false, at(0), &mut ports.control);
    runtime.service().unwrap();
    assert!(
        retirement
            .route(ports.control.try_event().unwrap())
            .is_none()
    );
    retirement.service(true, false, at(0), &mut ports.control);
    assert_eq!(ports.control.outstanding(), 0);
    assert_eq!(retirement.phase(), Phase::Retiring);
    assert!(ports.control.queue_hsupa(Command::TakeCompletion).is_ok());
    runtime.service().unwrap();
    let TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(Ok(Some(completion)))) =
        ports.control.try_event().unwrap().outcome
    else {
        panic!("canceled grant");
    };
    assert_eq!(completion.identity, identity);
    assert!(!completion.submitted);
    retirement.service(true, true, at(0), &mut ports.control);
    runtime.service().unwrap();
    assert!(
        retirement
            .route(ports.control.try_event().unwrap())
            .is_none()
    );
    assert_eq!(retirement.phase(), Phase::Retiring);
    retirement.service(true, true, completion.end, &mut ports.control);
    runtime.service().unwrap();
    assert!(
        retirement
            .route(ports.control.try_event().unwrap())
            .is_none()
    );
    assert_eq!(retirement.phase(), Phase::Retired);
    let lease = retirement
        .into_lease()
        .unwrap_or_else(|_| panic!("retired lease"));
    assert_eq!(lease.e_rnti(), e_rnti);
    resources.release_hsupa_user(lease).unwrap();
    runtime.close();
    let mut bank = runtime.take_closed_hsupa().unwrap();
    bank.begin_retirement(identities[1].ue).unwrap();
    resources
        .release_hsupa_user(bank.take_retired(identities[1].ue, at(0)).unwrap().unwrap())
        .unwrap();
    for radio in radios {
        resources.release(radio).unwrap();
    }
    resources
        .release_hsupa(bank.release().unwrap_or_else(|_| panic!("empty bank")))
        .unwrap();
    assert_eq!(resources.outstanding(), 0);
}
