// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn successor() -> Box<DedicatedTransmitSession> {
    Box::new(session(
        compile(true, false, true, 3, 0),
        identity(0, 2),
        FrameAnchor {
            time: at(4 * CHIPS_PER_FRAME),
            cfn: 4,
        },
        4,
    ))
}
fn fill(owner: &mut DedicatedTransmitSession) {
    let frame = input(owner);
    let payloads: Vec<_> = frame
        .transports
        .iter()
        .map(|transport| TransportPayload {
            transport: transport.transport,
            bits: &transport.bits,
        })
        .collect();
    owner
        .prepare_frame(frame.identity, frame.start, frame.tfci, &payloads)
        .unwrap();
}
fn setup(runtime: &mut DedicatedTransmitRuntime, ports: &mut DedicatedTransmitPorts) {
    install(runtime, ports, owner(0, 1));
    for _ in 0..4 {
        let data = input(runtime.bank.session(identity(0, 1)).unwrap());
        frame(runtime, ports, data);
    }
    ports.control.arm(identity(0, 1)).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Armed { result: Ok(()), .. }
    ));
}
fn stage(runtime: &mut DedicatedTransmitRuntime, ports: &mut DedicatedTransmitPorts) {
    assert!(
        ports
            .control
            .stage_replacement(identity(0, 1), successor())
            .is_ok()
    );
    runtime.service().unwrap();
    assert!(
        matches!(ports.control.try_event().unwrap().outcome, TransmitCommandOutcome::ReplacementPrepared { previous, identity: next }
        if previous == identity(0, 1) && next == identity(0, 2))
    );
    let data = input(runtime.bank.session(identity(0, 2)).unwrap());
    frame(runtime, ports, data);
}
fn commit(runtime: &mut DedicatedTransmitRuntime, ports: &mut DedicatedTransmitPorts) {
    ports
        .control
        .commit_replacement(identity(0, 1), identity(0, 2))
        .unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::ReplacementCommitted { result: Ok(_), .. }
    ));
}

#[test]
fn staged_generations_have_independent_tpc_rings_and_removal_clears_only_its_owner() {
    let (_, mut runtime, mut ports) = runtime(4, 4);
    setup(&mut runtime, &mut ports);
    feedback(&mut runtime, &mut ports, identity(0, 1), 0, vec![Bit::One]);
    stage(&mut runtime, &mut ports);
    feedback(
        &mut runtime,
        &mut ports,
        identity(0, 2),
        4 * CHIPS_PER_FRAME,
        vec![Bit::Zero],
    );
    assert_eq!(runtime.tpc[0].decisions[0].unwrap().command, Bit::One);
    assert_eq!(runtime.tpc[1].decisions[0].unwrap().command, Bit::Zero);
    ports.control.remove(identity(0, 1)).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Removed { result: Ok(_), .. }
    ));
    assert!(runtime.tpc[0].identity.is_none());
    assert_eq!(runtime.tpc[1].identity, Some(identity(0, 2)));
    assert_eq!(runtime.tpc[1].decisions[0].unwrap().command, Bit::Zero);
}

#[test]
fn commit_trims_old_future_decisions_and_rejects_feedback_beyond_the_old_boundary() {
    let (_, mut runtime, mut ports) = runtime(4, 4);
    setup(&mut runtime, &mut ports);
    feedback(
        &mut runtime,
        &mut ports,
        identity(0, 1),
        0,
        vec![Bit::One; 64],
    );
    stage(&mut runtime, &mut ports);
    commit(&mut runtime, &mut ports);
    assert!(runtime.tpc[0].decisions[..60].iter().all(Option::is_some));
    assert!(runtime.tpc[0].decisions[60..].iter().all(Option::is_none));
    ports
        .feedback
        .publish(TpcPlan {
            identity: identity(0, 1),
            first_slot: at(4 * CHIPS_PER_FRAME),
            commands: vec![Bit::Zero],
        })
        .ok()
        .unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.feedback.try_event().unwrap().outcome,
        TransmitCommandOutcome::Feedback {
            result: Err(TransmitError::Time),
            ..
        }
    ));
}

#[test]
fn synthesis_alone_and_lost_submission_never_release_old_resources() {
    for failed in [false, true] {
        let (mut source, mut runtime, mut ports) = runtime(4, 4);
        let writer = source.submission_writer().unwrap();
        setup(&mut runtime, &mut ports);
        feedback(
            &mut runtime,
            &mut ports,
            identity(0, 1),
            0,
            vec![Bit::Zero; 60],
        );
        stage(&mut runtime, &mut ports);
        commit(&mut runtime, &mut ports);
        let mut samples = vec![Complex32::default(); source.samples_per_slot()];
        for _ in 0..60 {
            runtime.generate_slot(&mut source, &mut samples).unwrap();
        }
        assert!(matches!(
            runtime.bank.session(identity(0, 1)).unwrap().phase(),
            TransmitPhase::Stopped { .. }
        ));
        if failed {
            writer.fail();
        }
        ports.control.retire_submitted(identity(0, 1)).unwrap();
        runtime.service().unwrap();
        let TransmitCommandOutcome::Retired {
            result: Err(error), ..
        } = ports.control.try_event().unwrap().outcome
        else {
            panic!("synthesis was accepted as submission");
        };
        assert_eq!(
            error,
            if failed {
                TransmitError::SubmissionLost
            } else {
                TransmitError::SubmissionPending
            }
        );
        assert!(runtime.bank.session(identity(0, 1)).is_some());
        assert_eq!(runtime.tpc[0].identity, Some(identity(0, 1)));
    }
}

#[test]
fn full_control_credit_returns_staged_owner_before_allocations_transfer() {
    let (_, mut runtime, mut ports) = runtime(2, 2);
    setup(&mut runtime, &mut ports);
    ports.control.observe(identity(0, 1)).unwrap();
    ports.control.observe(identity(0, 1)).unwrap();
    runtime.service().unwrap();
    let candidate = successor();
    let address = std::ptr::from_ref(candidate.as_ref());
    let returned = ports
        .control
        .stage_replacement(identity(0, 1), candidate)
        .err()
        .unwrap();
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert!(runtime.bank.session(identity(0, 2)).is_none());
    assert_eq!(ports.control.outstanding(), 2);
}

#[test]
fn same_epoch_cell_and_cursor_cannot_substitute_another_submission_pipeline() {
    let (_source, mut runtime, _ports) = runtime(4, 4);
    let mut foreign = source(false);
    let mut samples = vec![Complex32::default(); foreign.samples_per_slot()];
    assert!(matches!(
        runtime.generate_slot(&mut foreign, &mut samples),
        Err(crate::common_tx::CommonTxError::Poisoned)
    ));
    assert_eq!(foreign.next_chip(), at(0));
    assert_eq!(runtime.bank.next_chip(), at(0));
    assert!(!runtime.available());
}

#[test]
fn memory_hal_retirement_waits_for_actual_driver_acceptance_of_the_pulse_tail() {
    use crate::radio_io::{PumpProgress, TxSettings, prepare_tx};
    use sdr_hal::{Radio, memory::MemoryRadio};
    for (next_configuration, boundary_frames) in [(2, 4), (3, 4), (2, 8)] {
        let (mut radio, peer) = MemoryRadio::new(1);
        radio.set_tx_sample_rate(7_680_000.0).unwrap();
        let (tx, _) = Box::new(radio).split().unwrap();
        let (mut preparation, mut pump) = prepare_tx(
            source(false),
            tx,
            TxSettings::offline(1000, count(1), count(1), count(2)),
        )
        .unwrap();
        let mut ports = preparation
            .dedicated_ports(count(2), count(4), count(2_000_000), limits(8, 8))
            .unwrap();
        let mut old = Box::new(session(
            compile(true, false, true, 3, 0),
            identity(0, 1),
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            boundary_frames as usize,
        ));
        for _ in 0..boundary_frames {
            fill(&mut old);
        }
        let mut next = Box::new(session(
            compile(true, false, true, 3, 0),
            identity(0, next_configuration),
            FrameAnchor {
                time: at(boundary_frames * CHIPS_PER_FRAME),
                cfn: boundary_frames as u8,
            },
            4,
        ));
        fill(&mut next);
        assert!(ports.control.prepare(old).is_ok());
        ports.control.arm(identity(0, 1)).unwrap();
        assert!(
            ports
                .control
                .stage_replacement(identity(0, 1), next)
                .is_ok()
        );
        ports
            .control
            .commit_replacement(identity(0, 1), identity(0, next_configuration))
            .unwrap();
        ports
            .feedback
            .publish(TpcPlan {
                identity: identity(0, 1),
                first_slot: at(0),
                commands: vec![Bit::Zero; 60],
            })
            .ok()
            .unwrap();
        pump.start().unwrap();
        for slot in 0..boundary_frames * 15 {
            if boundary_frames == 8 && [32, 62].contains(&slot) {
                let first_slot = if slot == 32 { 60 } else { 90 };
                ports
                    .feedback
                    .publish(TpcPlan {
                        identity: identity(0, 1),
                        first_slot: at(first_slot * CHIPS_PER_SLOT),
                        commands: vec![Bit::Zero; 30],
                    })
                    .ok()
                    .unwrap();
            }
            if slot == 16 {
                ports
                    .feedback
                    .publish(TpcPlan {
                        identity: identity(0, next_configuration),
                        first_slot: at(boundary_frames * CHIPS_PER_FRAME),
                        commands: vec![Bit::One; 15],
                    })
                    .ok()
                    .unwrap();
            }
            preparation.prepare_one().unwrap().unwrap();
            assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
            let output = peer.output.recv().unwrap();
            assert!(
                output
                    .samples
                    .iter()
                    .all(|sample| sample.re.is_finite() && sample.im.is_finite())
            );
        }
        for _ in 0..4 {
            ports.control.try_event().unwrap();
        }
        for _ in 0..if boundary_frames == 8 { 4 } else { 2 } {
            ports.feedback.try_event().unwrap();
        }
        ports.control.retire_submitted(identity(0, 1)).unwrap();
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        peer.output.recv().unwrap();
        assert!(matches!(
            ports.control.try_event().unwrap().outcome,
            TransmitCommandOutcome::Retired {
                result: Err(TransmitError::SubmissionPending),
                ..
            }
        ));
        ports.control.retire_submitted(identity(0, 1)).unwrap();
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        peer.output.recv().unwrap();
        let TransmitCommandOutcome::Retired {
            identity: retired,
            result: Ok(retired_owner),
        } = ports.control.try_event().unwrap().outcome
        else {
            panic!("driver-accepted old owner did not retire");
        };
        assert_eq!(retired, identity(0, 1));
        assert_eq!(retired_owner.session.identity(), retired);
        assert_eq!(
            retired_owner.session.counters().consumed_frames,
            boundary_frames
        );
        assert_eq!(retired_owner.previous(), retired);
        assert_eq!(retired_owner.old_anchor().time, at(0));
        assert_eq!(retired_owner.old_anchor().cfn, 0);
        assert_eq!(retired_owner.successor(), identity(0, next_configuration));
        assert_ne!(
            retired_owner.successor(),
            identity(0, if next_configuration == 2 { 3 } else { 2 })
        );
        assert_eq!(
            retired_owner.boundary(),
            at(boundary_frames * CHIPS_PER_FRAME)
        );
        assert_ne!(
            retired_owner.boundary(),
            at(if boundary_frames == 4 { 8 } else { 4 } * CHIPS_PER_FRAME)
        );
        assert_eq!(retired_owner.session.phase(), TransmitPhase::Retired);
        assert!(retired_owner.submission.resolved());
        assert!(retired_owner.submission.air_end().index > boundary_frames * CHIPS_PER_FRAME);
        pump.stop().unwrap();
    }
}
