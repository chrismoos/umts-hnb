// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_packet::Operation;

#[test]
fn retained_hsdpa_results_do_not_block_rel99_admission_or_steal_its_responses() {
    let (mut env, request) = environment();
    assert!(env.admission.hsdpa_control().is_none());
    env.admission.configure_hsdpa_control(count(2)).unwrap();
    {
        let mut control = env.admission.hsdpa_control().unwrap();
        control.request_completion().unwrap();
        control.begin_retirement(env.key).unwrap();
        assert_eq!(control.outstanding(), 2);
    }
    env.admission
        .begin_initial(request, &env.radio, &env.receive)
        .unwrap();
    until(&mut env, AdmissionPhase::Prefilling, at(0));
    assert!(env.admission.take_unclaimed().is_none());

    let mut control = env.admission.hsdpa_control().unwrap();
    assert_eq!(control.outstanding(), 2);
    assert_eq!(
        control.finish_retirement(env.key),
        Err(crate::dedicated_tx::TransmitError::Capacity)
    );
    let first = control.take_result().unwrap();
    assert!(first.matches_request());
    assert_eq!(first.operation, Operation::Completion);
    assert!(matches!(
        first.event.outcome,
        TransmitCommandOutcome::HsdpaCompletion(None)
    ));
    let second = control.take_result().unwrap();
    assert!(second.matches_request());
    assert_eq!(second.operation, Operation::BeginRetirement(env.key));
    assert_eq!(control.outstanding(), 0);
    assert!(control.take_result().is_none());
    control.request_completion().unwrap();
}

#[test]
fn hsdpa_control_configuration_and_completion_polling_cannot_replace_pending_owners() {
    let (mut env, _) = environment();
    assert_eq!(
        env.admission.configure_hsdpa_control(count(65)),
        Err(crate::dedicated_tx::TransmitError::Capacity)
    );
    assert!(env.admission.hsdpa_control().is_none());
    env.admission.configure_hsdpa_control(count(2)).unwrap();
    let id = env
        .admission
        .hsdpa_control()
        .unwrap()
        .request_completion()
        .unwrap();
    assert_eq!(
        env.admission.configure_hsdpa_control(count(1)),
        Err(crate::dedicated_tx::TransmitError::Phase)
    );
    step(&mut env, at(0));
    let mut control = env.admission.hsdpa_control().unwrap();
    assert_eq!(
        control.request_completion(),
        Err(crate::dedicated_tx::TransmitError::Phase)
    );
    assert_eq!(control.take_result().unwrap().event.id, id);
    assert!(control.request_completion().is_ok());
}
