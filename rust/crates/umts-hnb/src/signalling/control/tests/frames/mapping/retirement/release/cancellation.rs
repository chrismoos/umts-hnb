// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_controller::{
    DedicatedControlError, DedicatedController, DedicatedControllerLimits, DedicatedPhase,
    PreparedDedicated,
};

#[test]
fn controller_cancel_release_refunds_only_mapping_and_returns_reusable_candidate() {
    let f = fixture_with_activation(true, false);
    let old = f.bank.identity();
    let next = next_identity(&f);
    let plan = subset(&f, true, false);
    let old_plan = f.plan.clone();
    let initial_bytes = f.bank.storage_bytes();
    let (mut runtime, port) = receive_runtime(f.key.epoch);
    let mut controller = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: initial_bytes * 2,
        },
    );
    controller
        .stage(Box::new(PreparedDedicated {
            receiver: receive_owner(old, activation(0), &f.plan),
            bearers: Box::new(f.bank),
        }))
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    controller.poll_receipts();
    controller.arm(old).unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    controller.poll_receipts();
    assert_eq!(controller.phase(old), Some(DedicatedPhase::Active));
    let prepared = controller
        .prepare_release_mapping(old, next, activation(4), plan.clone())
        .unwrap();
    let bytes = prepared.storage_bytes().unwrap();
    controller.stage_mapping(prepared).unwrap();
    assert_eq!(controller.used_bearer_bytes(), initial_bytes + bytes);
    let mut ticket = controller
        .stage_mapping_receiver(old, next, receive_owner(next, activation(4), &plan))
        .ok()
        .unwrap();
    let returned = controller.cancel_release(old, next).unwrap();
    assert_eq!(returned.identity(), next);
    assert_eq!(returned.storage_bytes().unwrap(), bytes);
    assert_eq!(controller.used_bearer_bytes(), initial_bytes);
    controller
        .with_active_mapping(old, |bank| {
            assert_eq!(bank.mapping_phase(old).unwrap(), MappingPhase::Active);
            assert!(!bank.contains_identity(next));
            assert!(Arc::ptr_eq(bank.configured(), old_plan.uplink_configured()));
        })
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    assert!(
        matches!(ticket.try_result(), Some(ReceiveCommandResult::ReplacementPrepared {
        previous, identity,
    }) if previous == old && identity == next)
    );
    assert!(controller.cancel_release(old, next).is_err());
    assert_eq!(controller.used_bearer_bytes(), initial_bytes);
    controller.stage_mapping(returned).unwrap();
    assert_eq!(controller.used_bearer_bytes(), initial_bytes + bytes);
    controller.commit_mapping(next).unwrap();
    assert!(controller.cancel_release(old, next).is_err());
    assert_eq!(controller.used_bearer_bytes(), initial_bytes + bytes);
}

#[test]
fn release_cancellation_rejects_wrong_identity_rebind_and_foreign_prepared_owner() {
    let mut f = fixture(true);
    let old = f.bank.identity();
    let next = next_identity(&f);
    let plan = subset(&f, false, true);
    let prepared = f
        .bank
        .prepare_release_mapping(next, activation(4), plan)
        .unwrap();
    let original = f.bank.storage_bytes();
    let bytes = prepared.storage_bytes().unwrap();
    f.bank.stage_mapping(prepared).unwrap();
    let wrong = ReceiveIdentity {
        configuration: NonZeroU64::new(next.configuration.get() + 1).unwrap(),
        ..next
    };
    assert!(f.bank.cancel_release(old, wrong).is_err());
    assert!(f.bank.cancel_release(wrong, next).is_err());
    assert_eq!(f.bank.storage_bytes(), original + bytes);
    let prepared = f.bank.cancel_release(old, next).unwrap();
    assert_eq!(f.bank.storage_bytes(), original);
    let mut foreign = fixture(true);
    let rejected = foreign.bank.stage_mapping(prepared).unwrap_err();
    assert_eq!(rejected.error, BearerError::Owner);
    assert_eq!(foreign.bank.storage_bytes(), original);
    f.bank.stage_mapping(rejected.prepared).unwrap();
    let _returned = f.bank.cancel_release(old, next).unwrap();
    let replacement = remapped(&f);
    let rebind = f
        .bank
        .prepare_mapping(next, activation(4), replacement)
        .unwrap();
    f.bank.stage_mapping(rebind).unwrap();
    let before = f.bank.storage_bytes();
    assert!(matches!(
        f.bank.cancel_release(old, next),
        Err(BearerError::UnsupportedBinding)
    ));
    assert_eq!(f.bank.storage_bytes(), before);
}

#[test]
fn inactive_controller_cannot_cancel_staged_release() {
    let f = fixture_with_activation(true, false);
    let old = f.bank.identity();
    let next = next_identity(&f);
    let bytes = f.bank.storage_bytes();
    let (mut runtime, port) = receive_runtime(f.key.epoch);
    let mut controller = DedicatedController::new(
        port,
        DedicatedControllerLimits {
            receipt_visits: count(2),
            service_visits: count(2),
            maximum_bearer_bytes: bytes * 2,
        },
    );
    controller
        .stage(Box::new(PreparedDedicated {
            receiver: receive_owner(old, activation(0), &f.plan),
            bearers: Box::new(f.bank),
        }))
        .unwrap();
    runtime.service(|_, result| {
        result.unwrap();
    });
    controller.poll_receipts();
    assert_eq!(controller.phase(old), Some(DedicatedPhase::Prepared));
    assert!(matches!(
        controller.cancel_release(old, next),
        Err(DedicatedControlError::Identity)
    ));
    assert_eq!(controller.used_bearer_bytes(), bytes);
}
