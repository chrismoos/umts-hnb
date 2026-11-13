// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{
        DedicatedTransmitPorts, DedicatedTransmitRuntime, TransmitCommandOutcome,
        TransmitRuntimeLimits,
    },
    hsupa_tx::{CommandOutcome, Error},
    test_support::{count, source},
};

fn runtime() -> (DedicatedTransmitRuntime, DedicatedTransmitPorts) {
    DedicatedTransmitRuntime::new(
        &source(true),
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
    .unwrap()
}

#[test]
fn completed_reply_keeps_capacity_until_consumed_and_foreign_reply_is_returned() {
    let (mut runtime, mut ports) = runtime();
    let mut commands = Commands::new(count(1)).unwrap();
    let first = commands
        .port(&mut ports.control)
        .queue_hsupa(Command::TakeCompletion)
        .unwrap_or_else(|_| panic!("first command"));
    assert!(
        commands
            .port(&mut ports.control)
            .queue_hsupa(Command::TakeCompletion)
            .is_err()
    );
    assert_eq!(ports.control.outstanding(), 1);
    let foreign = ports
        .control
        .queue_hsupa(Command::TakeCompletion)
        .unwrap_or_else(|_| panic!("foreign command"));
    runtime.service().unwrap();
    assert!(commands.route(ports.control.try_event().unwrap()).is_none());
    let returned = commands.route(ports.control.try_event().unwrap()).unwrap();
    assert_eq!(returned.id, foreign);
    let mut control = commands.port(&mut ports.control);
    assert_eq!(control.outstanding(), 1);
    assert!(control.queue_hsupa(Command::TakeCompletion).is_err());
    let completed = control.take_result().unwrap();
    assert_eq!(completed.id, first);
    assert!(matches!(
        completed.outcome,
        TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(Err(Error::Unavailable)))
    ));
    assert_eq!(control.outstanding(), 0);
    assert!(control.take_result().is_none());
    assert!(control.queue_hsupa(Command::TakeCompletion).is_ok());
}

#[test]
fn duplicate_and_unexpected_results_cannot_replace_retained_result() {
    let (_runtime, mut ports) = runtime();
    let mut commands = Commands::new(count(1)).unwrap();
    let id = commands
        .port(&mut ports.control)
        .queue_hsupa(Command::TakeCompletion)
        .unwrap_or_else(|_| panic!("command"));
    assert!(
        commands
            .route(TransmitEvent {
                id,
                outcome: TransmitCommandOutcome::HsdpaCompletion(None),
            })
            .is_none()
    );
    let duplicate = commands
        .route(TransmitEvent {
            id,
            outcome: TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(Ok(None))),
        })
        .unwrap();
    assert_eq!(duplicate.id, id);
    let retained = commands.port(&mut ports.control).take_result().unwrap();
    assert!(matches!(
        retained.outcome,
        TransmitCommandOutcome::HsdpaCompletion(None)
    ));
}

#[test]
fn worker_publication_failure_does_not_reserve_registry_capacity() {
    let (runtime, mut ports) = runtime();
    let mut commands = Commands::new(count(1)).unwrap();
    drop(runtime);
    let mut control = commands.port(&mut ports.control);
    assert!(!control.available());
    assert!(control.queue_hsupa(Command::TakeCompletion).is_err());
    assert_eq!(control.outstanding(), 0);
}

#[test]
fn wrapped_replies_preserve_admission_order_when_completion_arrives_first() {
    let (mut runtime, mut ports) = runtime();
    let mut commands = Commands::new(count(2)).unwrap();
    for _ in 0..5 {
        let admission = commands
            .port(&mut ports.control)
            .queue_hsupa(Command::TakeCompletion)
            .unwrap_or_else(|_| panic!("admission command"));
        let completion = commands
            .port(&mut ports.control)
            .queue_hsupa(Command::TakeCompletion)
            .unwrap_or_else(|_| panic!("completion command"));
        runtime.service().unwrap();
        let older = ports.control.try_event().unwrap();
        let newer = ports.control.try_event().unwrap();
        assert_eq!(older.id, admission);
        assert_eq!(newer.id, completion);
        assert!(commands.route(newer).is_none());
        assert!(commands.port(&mut ports.control).take_result().is_none());
        assert_eq!(commands.port(&mut ports.control).outstanding(), 2);
        assert!(commands.route(older).is_none());
        assert_eq!(
            commands.port(&mut ports.control).take_result().unwrap().id,
            admission
        );
        let next = commands
            .port(&mut ports.control)
            .queue_hsupa(Command::TakeCompletion)
            .unwrap_or_else(|_| panic!("wrapped command"));
        assert_eq!(
            commands.port(&mut ports.control).take_result().unwrap().id,
            completion
        );
        runtime.service().unwrap();
        assert!(commands.route(ports.control.try_event().unwrap()).is_none());
        assert_eq!(
            commands.port(&mut ports.control).take_result().unwrap().id,
            next
        );
        assert_eq!(commands.port(&mut ports.control).outstanding(), 0);
    }
}
