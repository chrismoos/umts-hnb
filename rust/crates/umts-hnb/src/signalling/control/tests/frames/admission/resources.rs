// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_resources::{AllocatedRadio, RadioResources};

const MAXIMUM_CODED_BITS: usize = 200_000;
const MAXIMUM_MAPPING_ENTRIES: usize = 300_000;

fn transport_limits() -> TransportPlanLimits {
    TransportPlanLimits {
        maximum_coded_bits: count(MAXIMUM_CODED_BITS),
        maximum_mapping_entries: count(MAXIMUM_MAPPING_ENTRIES),
        decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
    }
}

fn allocated_environment(
    noncanonical: bool,
) -> (
    Environment,
    Box<InitialAdmission>,
    RadioResources,
    AllocatedRadio,
) {
    let cell = crate::test_support::plan(false);
    let mut resources =
        RadioResources::new(at(0).epoch, cell.information(), count(2), 17..=18).unwrap();
    let mut allocation = None;
    let (env, request) = environment_with_plan(|radio, key| {
        let template = packet_only_configuration(&radio.retained_signalling(key).unwrap());
        let prepared = resources
            .prepare(
                key,
                &template,
                count(MAXIMUM_CODED_BITS),
                transport_limits(),
            )
            .unwrap();
        let plan = if noncanonical {
            Arc::new(
                CompiledRel99::new(
                    prepared.plan().configuration().clone(),
                    count(MAXIMUM_CODED_BITS),
                    transport_limits(),
                )
                .unwrap(),
            )
        } else {
            prepared.plan().clone()
        };
        allocation = Some(prepared);
        plan
    });
    (env, request, resources, allocation.unwrap())
}

#[test]
fn cancellation_keeps_physical_claims_until_actual_owners_are_returned() {
    for phase in [
        AdmissionPhase::Preparing,
        AdmissionPhase::ArmingReceive,
        AdmissionPhase::Prefilling,
        AdmissionPhase::ArmingTransmit,
        AdmissionPhase::Starting,
    ] {
        let (mut env, request, mut resources, allocation) = allocated_environment(false);
        assert!(Arc::ptr_eq(allocation.plan(), &env.plan));
        assert_eq!(allocation.key(), env.key);
        let identity = env
            .admission
            .begin_allocated(request, allocation, &env.radio, &env.receive)
            .unwrap();
        assert_eq!(identity, env.identity);
        if phase != AdmissionPhase::Preparing {
            until(&mut env, phase, at(0));
        }
        assert_eq!(resources.outstanding(), 1);
        assert!(env.admission.take_retired(identity).is_err());
        env.admission.cancel(identity).unwrap();
        assert!(env.admission.take_retired(identity).is_err());
        assert_eq!(resources.outstanding(), 1);
        until(&mut env, AdmissionPhase::Retired, at(0));
        assert_eq!(env.receive.used_bearer_bytes(), 0);
        assert_eq!(resources.outstanding(), 1);
        assert!(
            resources
                .prepare(
                    env.key,
                    env.plan.configuration(),
                    count(MAXIMUM_CODED_BITS),
                    transport_limits(),
                )
                .is_err()
        );
        let retired = env.admission.take_retired(identity).unwrap();
        assert_eq!(retired.error, AdmissionError::Cancelled);
        assert!(retired.receive.is_some() || retired.unadmitted_receive.is_some());
        assert!(retired.transmit.is_some());
        let lease = retired.resources.unwrap();
        assert_eq!(lease.key(), env.key);
        assert_eq!(resources.outstanding(), 1);
        resources.release(lease).unwrap();
        assert_eq!(resources.outstanding(), 0);
        let replacement = resources
            .prepare(
                env.key,
                env.plan.configuration(),
                count(MAXIMUM_CODED_BITS),
                transport_limits(),
            )
            .unwrap();
        resources.cancel(replacement).unwrap();
        assert!(env.admission.take_retired(identity).is_err());
    }
}

#[test]
fn published_setup_retains_claims_through_real_rx_tx_retirement() {
    let (mut env, request, mut resources, allocation) = allocated_environment(false);
    env.admission
        .begin_allocated(request, allocation, &env.radio, &env.receive)
        .unwrap();
    until(&mut env, AdmissionPhase::Prefilling, at(0));
    env.feedback
        .publish(TpcPlan {
            identity: env.identity,
            first_slot: at(0),
            commands: vec![Bit::Zero; 60],
        })
        .ok()
        .unwrap();
    env.transmit.service().unwrap();
    assert!(matches!(
        env.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback { result: Ok(()), .. }
    ));
    until(&mut env, AdmissionPhase::Starting, at(0));
    let mut samples = vec![num_complex::Complex32::default(); env.source.samples_per_slot()];
    for _ in 0..31 {
        let window = env
            .transmit
            .generate_slot(&mut env.source, &mut samples)
            .unwrap();
        assert!(env.writer.accepted(window));
        let now = env.source.next_chip();
        step(&mut env, now);
    }
    let now = env.source.next_chip();
    until(&mut env, AdmissionPhase::AwaitingComplete, now);
    assert_eq!(
        env.admission.procedure(env.identity).unwrap().phase(),
        SetupPhase::AwaitingComplete
    );
    assert_eq!(resources.outstanding(), 1);
    env.admission.cancel(env.identity).unwrap();
    assert!(env.admission.take_retired(env.identity).is_err());
    assert_eq!(resources.outstanding(), 1);
    until(&mut env, AdmissionPhase::Retired, now);
    let retired = env.admission.take_retired(env.identity).unwrap();
    assert!(retired.receive.is_some());
    assert!(retired.transmit.is_some());
    assert_eq!(resources.outstanding(), 1);
    resources.release(retired.resources.unwrap()).unwrap();
    assert_eq!(resources.outstanding(), 0);
}

#[test]
fn wrong_canonical_plan_rejection_returns_both_original_admission_and_allocation() {
    let (mut env, request, mut resources, allocation) = allocated_environment(true);
    assert!(!Arc::ptr_eq(allocation.plan(), &env.plan));
    assert!(Arc::ptr_eq(
        allocation.plan().configuration(),
        env.plan.configuration()
    ));
    let original_request = std::ptr::from_ref(request.as_ref());
    let original_plan = allocation.plan().clone();
    let charged = env.radio.ledger.used();
    let rejected = env
        .admission
        .begin_allocated(request, allocation, &env.radio, &env.receive)
        .unwrap_err();
    assert_eq!(rejected.admission.error, AdmissionError::Configuration);
    assert_eq!(
        std::ptr::from_ref(rejected.admission.request.as_ref()),
        original_request
    );
    assert!(Arc::ptr_eq(rejected.allocation.plan(), &original_plan));
    assert_eq!(rejected.allocation.key(), env.key);
    assert_eq!(env.admission.phase(env.identity), None);
    assert_eq!(env.receive.used_bearer_bytes(), 0);
    assert_eq!(env.radio.ledger.used(), charged);
    assert_eq!(resources.outstanding(), 1);
    resources.cancel(rejected.allocation).unwrap();
    assert_eq!(resources.outstanding(), 0);
    env.radio
        .retire_frame_pool(rejected.admission.request.pool)
        .unwrap();
}
