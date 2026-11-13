// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn removed(
    runtime: &mut DedicatedTransmitRuntime,
    ports: &mut DedicatedTransmitPorts,
    key: ReceiveIdentity,
) -> Box<DedicatedTransmitSession> {
    ports.control.remove(key).unwrap();
    runtime.service().unwrap();
    match ports.control.try_event().unwrap().outcome {
        TransmitCommandOutcome::Removed {
            identity,
            result: Ok(session),
        } if identity == key => session,
        _ => panic!("checked removal did not return the original owner"),
    }
}

#[test]
fn never_admitted_is_local_but_direct_bank_removal_remains_unknown() {
    let (_, mut runtime, _ports) = runtime(2, 2);
    let session = owner(0, 1);
    assert!(matches!(session.drain(), TransmitDrain::LocalUnsubmitted));
    assert!(runtime.bank.insert_boxed(session).is_ok());
    assert!(matches!(
        runtime.bank.session(identity(0, 1)).unwrap().drain(),
        TransmitDrain::Unknown
    ));
    let session = runtime.bank.remove_boxed(identity(0, 1)).unwrap();
    assert!(matches!(session.drain(), TransmitDrain::Unknown));
}

#[test]
fn prepared_payloads_without_generated_common_samples_have_no_contribution() {
    let (_source, mut runtime, mut ports) = runtime(2, 2);
    install(&mut runtime, &mut ports, owner(0, 1));
    let data = input(runtime.bank.session(identity(0, 1)).unwrap());
    frame(&mut runtime, &mut ports, data);
    let session = removed(&mut runtime, &mut ports, identity(0, 1));
    assert_eq!(session.counters().prepared_frames, 1);
    assert_eq!(session.counters().mixed_chips, 0);
    assert!(matches!(session.drain(), TransmitDrain::NoContribution));
}

#[test]
fn lost_dedicated_slot_keeps_tpc_order_and_allows_original_owner_tail_retirement() {
    let (mut source, mut runtime, mut ports) = runtime(4, 4);
    let mut writer = source.submission_writer().unwrap();
    let key = identity(0, 1);
    install(&mut runtime, &mut ports, owner(0, 1));
    let data = input(runtime.bank.session(key).unwrap());
    frame(&mut runtime, &mut ports, data);
    ports.control.arm(key).unwrap();
    runtime.service().unwrap();
    ports.control.try_event().unwrap();
    feedback(&mut runtime, &mut ports, key, 0, vec![Bit::One, Bit::Zero]);
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    let lost = runtime
        .generate_lost_slot(&mut source, &mut samples)
        .unwrap();
    assert!(writer.lost(lost));
    assert!(runtime.bank.session(key).unwrap().failure().is_none());
    let session = removed(&mut runtime, &mut ports, key);
    let TransmitDrain::ResolvedTail(receipt) = session.drain() else {
        panic!("lost contribution still needs ownership resolution")
    };
    assert!(receipt.viable());
    assert!(!receipt.resolved());
    let tail = runtime.generate_slot(&mut source, &mut samples).unwrap();
    assert!(writer.accepted(tail));
    assert!(receipt.resolved());
    assert_eq!(session.identity(), key);
}

#[test]
fn partial_final_frame_keeps_full_tail_receipt_distinct_from_elapsed_air_time() {
    let (mut source, mut runtime, mut ports) = runtime(2, 2);
    let mut writer = source.submission_writer().unwrap();
    let key = identity(0, 1);
    install(&mut runtime, &mut ports, owner(0, 1));
    let data = input(runtime.bank.session(key).unwrap());
    frame(&mut runtime, &mut ports, data);
    ports.control.arm(key).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Armed { result: Ok(()), .. }
    ));
    feedback(&mut runtime, &mut ports, key, 0, vec![Bit::One]);
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    let first = runtime.generate_slot(&mut source, &mut samples).unwrap();
    let session = removed(&mut runtime, &mut ports, key);
    assert_eq!(session.phase(), TransmitPhase::Retired);
    assert_eq!(session.counters().consumed_frames, 0);
    assert_eq!(session.counters().mixed_chips, CHIPS_PER_SLOT);
    let TransmitDrain::ResolvedTail(receipt) = session.drain() else {
        panic!("missing partial-frame tail proof")
    };
    assert!(receipt.viable());
    assert!(!receipt.resolved());
    assert!(writer.accepted(first));
    assert!(!receipt.resolved());
    let tail = runtime.generate_slot(&mut source, &mut samples).unwrap();
    assert!(writer.accepted(tail));
    assert!(receipt.resolved());
    assert!(receipt.air_end().index > CHIPS_PER_SLOT);
    let (clock, mut advance) = crate::radio_io::RadioClock::controlled(
        crate::radio_io::AirClockOrigin {
            chip: at(0),
            chip_rate: umts_types::time::SampleRate::new(umts_types::time::CHIPS_PER_SECOND, 1)
                .unwrap(),
            tick: 0,
            tick_rate: NonZeroU64::new(umts_types::time::CHIPS_PER_SECOND).unwrap(),
        },
        CHIPS_PER_SLOT,
    );
    assert!(clock.read().unwrap().chip.unwrap().index < receipt.air_end().index);
    advance(receipt.air_end().index).unwrap();
    assert_eq!(clock.read().unwrap().chip.unwrap(), receipt.air_end());
    drop(writer);
    assert!(!receipt.viable());
    assert!(!receipt.resolved());
}

#[test]
fn lost_writer_rejects_removal_without_losing_the_original_worker_owner() {
    let (mut source, mut runtime, mut ports) = runtime(2, 2);
    let writer = source.submission_writer().unwrap();
    let key = identity(0, 1);
    install(&mut runtime, &mut ports, owner(0, 1));
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    runtime.generate_slot(&mut source, &mut samples).unwrap();
    let address = runtime.bank.session(key).unwrap() as *const DedicatedTransmitSession;
    drop(writer);
    ports.control.remove(key).unwrap();
    runtime.service().unwrap();
    assert!(matches!(
        ports.control.try_event().unwrap().outcome,
        TransmitCommandOutcome::Removed {
            result: Err(TransmitError::SubmissionLost),
            ..
        }
    ));
    let original = runtime.bank.session(key).unwrap();
    assert_eq!(original as *const DedicatedTransmitSession, address);
    assert!(matches!(original.drain(), TransmitDrain::Unknown));
}

#[test]
fn nonzero_source_origin_is_not_mistaken_for_a_generated_prefix() {
    let mut source = CommonTx::new(
        crate::test_support::plan(false),
        at(4 * CHIPS_PER_FRAME),
        umts_dsp::pulse_shaping::SamplesPerChip::Two,
        32,
        crate::test_support::amplitudes(),
        count(8),
        100,
    )
    .unwrap();
    let mut writer = source.submission_writer().unwrap();
    let (mut runtime, mut ports) =
        DedicatedTransmitRuntime::new(&source, count(2), count(4), count(2_000_000), limits(2, 2))
            .unwrap();
    let make_owner = |configuration| {
        Box::new(session(
            compile(true, false, true, 3, 0),
            identity(0, configuration),
            FrameAnchor {
                time: at(4 * CHIPS_PER_FRAME),
                cfn: 4,
            },
            4,
        ))
    };
    install(&mut runtime, &mut ports, make_owner(1));
    assert!(matches!(
        removed(&mut runtime, &mut ports, identity(0, 1)).drain(),
        TransmitDrain::NoContribution
    ));
    install(&mut runtime, &mut ports, make_owner(2));
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    let first = runtime.generate_slot(&mut source, &mut samples).unwrap();
    let session = removed(&mut runtime, &mut ports, identity(0, 2));
    let TransmitDrain::ResolvedTail(receipt) = session.drain() else {
        panic!("missing nonzero-origin tail proof")
    };
    assert!(writer.accepted(first));
    assert!(!receipt.resolved());
    let tail = runtime.generate_slot(&mut source, &mut samples).unwrap();
    assert!(writer.accepted(tail));
    assert!(receipt.resolved());
    assert!(receipt.air_end().index > 4 * CHIPS_PER_FRAME + CHIPS_PER_SLOT);
}
