// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_tx::{
    DedicatedTransmitPorts, DedicatedTransmitRuntime, TransmitCommandOutcome, TransmitRuntimeLimits,
};

fn runtime(source: &CommonTx) -> (DedicatedTransmitRuntime, DedicatedTransmitPorts) {
    DedicatedTransmitRuntime::new(
        source,
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

fn execute(
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    command: Command,
) -> CommandOutcome {
    assert!(ports.control.queue_hsupa(command).is_ok());
    runtime.service().unwrap();
    let TransmitCommandOutcome::Hsupa(outcome) = ports.control.try_event().unwrap().outcome else {
        panic!("HSUPA command result")
    };
    assert_eq!(ports.control.outstanding(), 0);
    assert_eq!(ports.control.retained_bits(), 0);
    outcome
}

#[test]
fn production_commands_compose_and_retire_controls_without_fabricating_delivery() {
    for lost in [false, true] {
        let mut source = source(true);
        let mut writer = source.submission_writer().unwrap();
        let (mut resources, bank, radios, identities) = setup(&source);
        let (mut runtime, mut ports) = runtime(&source);
        assert!(runtime.attach_hsupa(bank).is_ok());
        for identity in &identities {
            assert!(
                matches!(execute(&mut runtime, &mut ports, Command::Feedback {
                request: feedback(*identity), amplitude: 0.1,
            }), CommandOutcome::Queued { identity: actual, result: Ok(_) } if actual == *identity)
            );
        }
        assert!(matches!(
            execute(
                &mut runtime,
                &mut ports,
                Command::Grant {
                    identity: identities[0],
                    start: at(umts_dsp::hsupa::GRANT_FRAME_OFFSET_CHIPS),
                    grant: AbsoluteGrant::new(10, GrantScope::AllProcesses).unwrap(),
                    amplitude: 0.1,
                }
            ),
            CommandOutcome::Queued { result: Ok(_), .. }
        ));
        assert!(matches!(
            execute(&mut runtime, &mut ports, Command::TakeCompletion),
            CommandOutcome::Completion(Ok(None))
        ));
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        for _ in 0..6 {
            let window = if lost {
                runtime
                    .generate_lost_slot(&mut source, &mut samples)
                    .unwrap()
            } else {
                runtime.generate_slot(&mut source, &mut samples).unwrap()
            };
            assert!(samples.iter().any(|sample| sample.norm_sqr() > 0.0));
            assert!(if lost {
                writer.lost(window)
            } else {
                writer.accepted(window)
            });
        }
        for _ in 0..3 {
            let CommandOutcome::Completion(Ok(Some(completion))) =
                execute(&mut runtime, &mut ports, Command::TakeCompletion)
            else {
                panic!("resolved control receipt")
            };
            assert_eq!(completion.submitted, !lost);
        }
        for (identity, radio) in identities.into_iter().zip(radios) {
            assert!(matches!(
                execute(
                    &mut runtime,
                    &mut ports,
                    Command::BeginRetirement(identity.ue)
                ),
                CommandOutcome::RetirementStarted { result: Ok(()), .. }
            ));
            assert!(matches!(
                execute(
                    &mut runtime,
                    &mut ports,
                    Command::FinishRetirement {
                        ue: identity.ue,
                        air_now: at(0),
                    }
                ),
                CommandOutcome::RetirementFinished {
                    result: Ok(None),
                    ..
                }
            ));
            let CommandOutcome::RetirementFinished {
                result: Ok(Some(lease)),
                ..
            } = execute(
                &mut runtime,
                &mut ports,
                Command::FinishRetirement {
                    ue: identity.ue,
                    air_now: at(6 * CHIPS_PER_SLOT),
                },
            )
            else {
                panic!("retired resource ownership")
            };
            resources.release_hsupa_user(lease).unwrap();
            resources.release(radio).unwrap();
        }
        runtime.close();
        let pool = runtime
            .take_closed_hsupa()
            .unwrap()
            .release()
            .unwrap_or_else(|_| panic!("empty control bank"));
        resources.release_hsupa(pool).unwrap();
        assert_eq!(resources.outstanding(), 0);
    }
}

#[test]
fn registration_preserves_lease_on_backpressure_and_missing_bank() {
    let source = source(true);
    let (mut resources, mut bank, radios, identities) = setup(&source);
    let identity = identities[0];
    bank.begin_retirement(identity.ue).unwrap();
    let lease = bank.take_retired(identity.ue, at(0)).unwrap().unwrap();
    let (mut runtime, mut ports) = runtime(&source);
    for _ in 0..4 {
        assert!(ports.control.queue_hsupa(Command::TakeCompletion).is_ok());
    }
    let command = ports
        .control
        .queue_hsupa(Command::Register {
            lease,
            identity,
            interval: Tti::TwoMilliseconds,
        })
        .expect_err("bounded command capacity");
    for _ in 0..2 {
        runtime.service().unwrap();
        for _ in 0..2 {
            ports.control.try_event().unwrap();
        }
    }
    let CommandOutcome::Registered {
        result: Err((Error::Unavailable, lease)),
        ..
    } = execute(&mut runtime, &mut ports, command)
    else {
        panic!("returned registration lease")
    };
    assert_eq!(lease.key(), identity.ue);
    assert!(runtime.attach_hsupa(bank).is_ok());
    assert!(matches!(
        execute(
            &mut runtime,
            &mut ports,
            Command::Register {
                lease,
                identity,
                interval: Tti::TwoMilliseconds,
            }
        ),
        CommandOutcome::Registered { result: Ok(()), .. }
    ));
    for (identity, radio) in identities.into_iter().zip(radios) {
        execute(
            &mut runtime,
            &mut ports,
            Command::BeginRetirement(identity.ue),
        );
        let CommandOutcome::RetirementFinished {
            result: Ok(Some(lease)),
            ..
        } = execute(
            &mut runtime,
            &mut ports,
            Command::FinishRetirement {
                ue: identity.ue,
                air_now: at(0),
            },
        )
        else {
            panic!("returned registered lease")
        };
        resources.release_hsupa_user(lease).unwrap();
        resources.release(radio).unwrap();
    }
    runtime.close();
    resources
        .release_hsupa(
            runtime
                .take_closed_hsupa()
                .unwrap()
                .release()
                .unwrap_or_else(|_| panic!("empty control bank")),
        )
        .unwrap();
    assert_eq!(resources.outstanding(), 0);
}
