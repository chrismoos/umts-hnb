// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa_packet::{Operation, control::Commands};

#[test]
fn hsdpa_dispatch_preserves_foreign_and_malformed_results_and_rejects_duplicate_custody() {
    let (_source, mut runtime, mut ports) = runtime(4, 4);
    let mut commands = Commands::new(count(2)).unwrap();
    let ue = identity(0, 1).ue;
    let id = commands
        .port(&mut ports.control)
        .begin_retirement(ue)
        .unwrap();
    let foreign = ports.control.observe(identity(1, 1)).unwrap();
    runtime.service().unwrap();
    let mut event = ports.control.try_event().unwrap();
    assert_eq!(event.id, id);
    let TransmitCommandOutcome::HsdpaRetirementStarted { ue, .. } = &mut event.outcome else {
        panic!("retirement result")
    };
    *ue = identity(1, 1).ue;
    assert!(commands.route(event).is_none());
    let event = ports.control.try_event().unwrap();
    assert_eq!(commands.route(event).unwrap().id, foreign);
    assert!(
        commands
            .route(TransmitEvent {
                id,
                outcome: TransmitCommandOutcome::Closed,
            })
            .is_some()
    );
    let result = commands.port(&mut ports.control).take_result().unwrap();
    assert!(!result.matches_request());
    assert_eq!(
        result.operation,
        Operation::BeginRetirement(identity(0, 1).ue)
    );
    assert!(matches!(
        result.event.outcome,
        TransmitCommandOutcome::HsdpaRetirementStarted { ue, .. } if ue == identity(1, 1).ue
    ));
}

#[test]
fn rejected_publication_does_not_reserve_a_result_slot() {
    let (_source, _runtime, mut ports) = runtime(1, 1);
    let mut commands = Commands::new(count(2)).unwrap();
    let mut control = commands.port(&mut ports.control);
    control.begin_retirement(identity(0, 1).ue).unwrap();
    assert_eq!(
        control.finish_retirement(identity(0, 1).ue),
        Err(TransmitError::Capacity)
    );
    assert_eq!(control.outstanding(), 1);
}

#[test]
fn session_retirement_keeps_exact_command_identity_and_does_not_treat_closed_as_complete() {
    use crate::hsdpa_session::{RetirementError, RetirementProgress, retirement::Retirement};
    let (_source, mut runtime, mut ports) = runtime(4, 4);
    let mut commands = Commands::new(count(2)).unwrap();
    let mut retirement = Retirement::new(identity(0, 1).ue);
    retirement.begin();
    assert_eq!(
        retirement.service(&mut commands.port(&mut ports.control)),
        Ok(RetirementProgress::Pending)
    );
    retirement
        .service(&mut commands.port(&mut ports.control))
        .unwrap();
    assert_eq!(commands.port(&mut ports.control).outstanding(), 1);
    runtime.service().unwrap();
    assert!(commands.route(ports.control.try_event().unwrap()).is_none());
    let mut result = commands.port(&mut ports.control).take_result().unwrap();
    let operation = result.operation;
    result.operation = Operation::BeginRetirement(identity(1, 1).ue);
    assert_eq!(retirement.result(&result), Err(RetirementError::Identity));
    result.operation = operation;
    assert_eq!(
        retirement.result(&result),
        Err(RetirementError::Transmit(
            crate::hsdpa_tx::Error::Unavailable
        ))
    );
    assert!(!retirement.complete());
    retirement
        .service(&mut commands.port(&mut ports.control))
        .unwrap();
    assert_eq!(retirement.result(&result), Err(RetirementError::Identity));
    runtime.service().unwrap();
    assert!(commands.route(ports.control.try_event().unwrap()).is_none());
    let mut result = commands.port(&mut ports.control).take_result().unwrap();
    result.event.outcome = TransmitCommandOutcome::Closed;
    assert_eq!(retirement.result(&result), Err(RetirementError::Closed));
    assert!(!retirement.complete());
}
