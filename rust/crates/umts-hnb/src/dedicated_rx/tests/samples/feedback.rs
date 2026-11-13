// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_tx::{
        DedicatedTransmitPort, DedicatedTransmitRuntime, DedicatedTransmitSession,
        TransmitCommandOutcome, TransmitError, TransmitLimits, TransmitRuntimeLimits,
    },
    radio_io::{AirClockOrigin, ClockFailure, RadioClock},
    rel99_plan::CompiledRel99,
    test_support,
};
use umts_dsp::dedicated::DpchAmplitudes;
use umts_rrc::dedicated::Rel99Configuration;
use umts_types::time::{CHIPS_PER_SECOND, CHIPS_PER_SLOT, SLOTS_PER_FRAME};

struct FeedbackEnvironment {
    rx: DedicatedSampleRuntime,
    port: DedicatedReceivePort,
    tx: DedicatedTransmitRuntime,
    control: DedicatedTransmitPort,
    _source: crate::common_tx::CommonTx,
    _clock: Box<dyn FnMut(u64) -> Result<(), ClockFailure>>,
}

fn environment(ues: usize, tx_ues: usize) -> FeedbackEnvironment {
    environment_with_capacity(ues, tx_ues, 1)
}

fn environment_with_capacity(ues: usize, tx_ues: usize, credits: usize) -> FeedbackEnvironment {
    environment_at_clock(ues, tx_ues, credits, 100_000)
}

fn environment_at_clock(
    ues: usize,
    tx_ues: usize,
    credits: usize,
    initial_tick: u64,
) -> FeedbackEnvironment {
    let source = test_support::source(false);
    let (mut tx, ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(8),
        count(2_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(2),
            commands_per_step: count(1),
            feedback_capacity: count(credits),
            feedback_per_step: count(1),
            maximum_frame_payload_bits: count(100_000),
            maximum_inflight_retained_bits: count(2_000_000),
        },
    )
    .unwrap();
    let mut control = ports.control;
    for slot in 0..tx_ues as u32 {
        let (ul, dl, bearers, mut physical) = test_support::rel99::components(false, false, 10);
        physical.uplink_scrambling = 0x123456 + slot;
        physical.frame_offset_256_chips = 4;
        physical.default_offset_512_chips = 2;
        physical.downlink_codes = vec![3 + slot as u16];
        let plan = Arc::new(
            CompiledRel99::new(
                Arc::new(Rel99Configuration::new(ul, dl, bearers, physical).unwrap()),
                count(200_000),
                TransportPlanLimits {
                    maximum_coded_bits: count(200_000),
                    maximum_mapping_entries: count(300_000),
                    decode: DecodeBudget::new(std::num::NonZeroU8::new(8).unwrap()).unwrap(),
                },
            )
            .unwrap(),
        );
        let session = DedicatedTransmitSession::new(
            identity(slot, 1),
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            plan,
            DpchAmplitudes {
                data: 0.2,
                tpc: 0.2,
                tfci: 0.2,
                pilot: 0.2,
            },
            TransmitLimits {
                frame_slots: count(2),
                maximum_frame_bits: count(100_000),
                maximum_retained_bits: count(1_000_000),
            },
        )
        .unwrap();
        let command = control.prepare(Box::new(session)).ok().unwrap();
        assert_eq!(tx.service(), Ok(1));
        let event = control.try_event().unwrap();
        assert_eq!(event.id, command);
        assert!(
            matches!(event.outcome, TransmitCommandOutcome::Prepared(id) if id == identity(slot, 1))
        );
    }
    let (clock, mut advance) = RadioClock::controlled(
        AirClockOrigin {
            chip: test_support::at(0),
            chip_rate: umts_types::time::SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
            tick: 0,
            tick_rate: NonZeroU64::new(CHIPS_PER_SECOND).unwrap(),
        },
        0,
    );
    // A controlled device clock isolates lane ownership from wall-clock scheduling, not RT performance.
    advance(initial_tick).unwrap();
    let (mut rx, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    assert!(
        rx.attach_feedback(Box::new(DedicatedFeedback::new(ports.feedback, clock)))
            .is_ok()
    );
    let config = configuration();
    for slot in 0..ues as u32 {
        let mut receiver = owner(slot, config.clone(), 20_096);
        receiver
            .configure_power_control(PilotTpcSettings {
                target: PilotSirTarget::from_db(if slot == 0 { 100.0 } else { -100.0 }).unwrap(),
                minimum_coherent_fraction: 0.05,
                uplink_anchor: at(0),
                downlink_anchor: at(0),
                rate: SamplesPerChip::Two,
                lead_chips: NonZeroU64::new(2 * CHIPS_PER_SLOT).unwrap(),
                maximum_observation_age_chips: NonZeroU64::new(100_000).unwrap(),
            })
            .unwrap();
        assert_eq!(
            receiver.power_control_status().unwrap().counters.produced,
            0
        );
        let mut ticket = install(&mut port, receiver);
        run(&mut rx);
        assert!(
            matches!(ticket.try_result(), Some(ReceiveCommandResult::Installed(id)) if id == identity(slot, 1))
        );
    }
    FeedbackEnvironment {
        rx,
        port,
        tx,
        control,
        _source: source,
        _clock: Box::new(advance),
    }
}

fn status(env: &mut FeedbackEnvironment, slot: u32) -> PilotTpcStatus {
    let mut ticket = env.port.observe(identity(slot, 1)).unwrap();
    for _ in 0..8 {
        env.rx.service(|_, _| {});
        if let Some(ReceiveCommandResult::Observed {
            identity: id,
            status: Some(observed),
        }) = ticket.try_result()
        {
            assert_eq!(id, identity(slot, 1));
            return observed.power_control.unwrap();
        }
    }
    panic!("bounded power-control observation did not complete");
}

fn pump(env: &mut FeedbackEnvironment, visits: usize) {
    let mut output = Vec::new();
    for _ in 0..visits {
        env.tx.service().unwrap();
        run(&mut env.rx);
        drain(&mut env.port, &mut output);
    }
}

fn feed(
    env: &mut FeedbackEnvironment,
    first: u64,
    samples: &[Complex32],
    generation: u64,
    discontinuity: bool,
) {
    let maximum = CHIPS_PER_SLOT as usize * SamplesPerChip::Two.get();
    for (index, input) in samples.chunks(maximum).enumerate() {
        env.rx
            .feed(matched(
                first + (index * maximum) as u64,
                input,
                generation,
                discontinuity && index == 0,
            ))
            .unwrap();
    }
}

#[test]
fn current_pilot_is_submitted_before_transport_decoder_runs() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment(1, 1);
    feed(&mut env, first, &samples, 1, false);
    let mut decodes = 0;
    env.rx.service_with_decoder(
        |session| {
            decodes += 1;
            assert_eq!(env.tx.service(), Ok(1));
            session.service()
        },
        |id, result| {
            assert_eq!(id, identity(0, 1));
            assert!(matches!(result, Ok(SampleProgress::Slot(_))));
        },
    );
    assert_eq!(decodes, 1);
    assert_eq!(env.rx.counters().feedback_submitted, 1);
    assert_eq!(env.rx.counters().failed_owners, 0);
}

#[test]
fn newer_raw_rx_waits_for_shared_clock_publication_without_failing_the_receive_owner() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment_at_clock(1, 1, 1, 0);
    feed(&mut env, first, &samples, 1, false);
    run(&mut env.rx);
    let waiting = status(&mut env, 0);
    assert_eq!(waiting.failure, None);
    assert!(waiting.counters.publication_clock_waits > 0);
    assert_eq!(waiting.counters.produced, 1);
    assert_eq!(env.rx.counters().feedback_submitted, 0);
    assert_eq!(env.rx.first_failure(), None);
    (env._clock)(100_000).unwrap();
    pump(&mut env, 64);
    let resumed = status(&mut env, 0);
    assert_eq!(resumed.failure, None);
    assert_eq!(resumed.counters.accepted, SLOTS_PER_FRAME);
    assert_eq!(
        resumed.last_accepted.unwrap().uplink_slot.index,
        at(0).index + 14 * CHIPS_PER_SLOT
    );
    assert_eq!(env.rx.first_failure(), None);
}

#[test]
fn decoder_failure_reports_once_after_feedback_and_does_not_stop_peer() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 2, 1.0));
    let mut env = environment_with_capacity(2, 2, 2);
    feed(&mut env, first, &samples, 1, false);
    let mut failures = Vec::new();
    env.rx.service_with_decoder(
        |session| {
            if session.identity() == identity(0, 1) {
                assert_eq!(env.tx.service(), Ok(1));
                Err(ReceiveError::Configuration)
            } else {
                session.service()
            }
        },
        |id, result| {
            if let Err(error) = result {
                failures.push((id, error));
            }
        },
    );
    assert_eq!(
        failures,
        vec![(identity(0, 1), ReceiveError::Configuration)]
    );
    pump(&mut env, 96);
    assert_eq!(env.rx.counters().failed_owners, 1);
    assert_eq!(status(&mut env, 1).counters.accepted, SLOTS_PER_FRAME);
    assert_eq!(status(&mut env, 1).failure, None);
}

#[test]
fn sampled_slots_publish_exactly_once_through_the_final_frame_at_each_gain() {
    for gain in [0.001, 1.0, 100.0] {
        let config = configuration();
        let (first, samples) = filtered(&waveform(&config, 1, 1, gain));
        let mut env = environment(1, 1);
        feed(&mut env, first, &samples, 1, false);
        pump(&mut env, 48);
        let observed = status(&mut env, 0);
        assert_eq!(env.rx.counters().sampled_slots, SLOTS_PER_FRAME);
        assert!(env.rx.counters().maximum_decode_state_updates > 0);
        assert!(env.rx.counters().maximum_decode_state_updates <= 96 * 1024);
        assert_eq!(observed.counters.produced, SLOTS_PER_FRAME);
        assert_eq!(observed.counters.submitted, SLOTS_PER_FRAME);
        assert_eq!(observed.counters.accepted, SLOTS_PER_FRAME);
        assert_eq!(observed.counters.rejected, 0);
        assert_eq!(observed.failure, None);
        let last = observed.last_accepted.unwrap();
        assert_eq!(last.identity, identity(0, 1));
        assert_eq!(observed.last_command, Some(Bit::One));
        assert_eq!(last.uplink_slot.index, at(0).index + 14 * CHIPS_PER_SLOT);
        assert_eq!(last.sample_generation, 1);
        assert!(
            observed.last_sir_linear.unwrap().is_finite()
                && observed.last_sir_linear.unwrap() > 0.0
        );
        pump(&mut env, 16);
        assert_eq!(status(&mut env, 0).counters, observed.counters);
        assert_eq!(env.rx.counters().feedback_accepted, SLOTS_PER_FRAME);
        assert_eq!(env.rx.counters().feedback_protocol_errors, 0);
    }
}

#[test]
fn two_real_scrambling_codes_keep_targets_and_receipts_independent_on_one_credit() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 2, 1.0));
    let mut env = environment(2, 2);
    feed(&mut env, first, &samples, 1, false);
    pump(&mut env, 96);
    let a = status(&mut env, 0);
    let b = status(&mut env, 1);
    for observed in [&a, &b] {
        assert_eq!(observed.counters.accepted, SLOTS_PER_FRAME);
        assert_eq!(observed.counters.produced, SLOTS_PER_FRAME);
        assert_eq!(observed.failure, None);
    }
    assert!(a.counters.backpressure + b.counters.backpressure > 0);
    assert_eq!(a.last_accepted.unwrap().identity, identity(0, 1));
    assert_eq!(b.last_accepted.unwrap().identity, identity(1, 1));
    assert_eq!(a.last_command, Some(Bit::One));
    assert_eq!(b.last_command, Some(Bit::Zero));
    assert_eq!(env.rx.counters().sampled_slots, 2 * SLOTS_PER_FRAME);
    assert!(env.rx.counters().maximum_decode_state_updates > 0);
    assert!(env.rx.counters().maximum_decode_state_updates <= 96 * 1024);
    assert_eq!(env.rx.counters().feedback_accepted, 2 * SLOTS_PER_FRAME);
    assert_eq!(env.rx.counters().feedback_rejected, 0);
    assert_eq!(env.rx.counters().feedback_protocol_errors, 0);
}

#[test]
fn one_feedback_credit_serves_both_ues_before_either_finishes_its_frame() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 2, 1.0));
    let mut env = environment(2, 2);
    feed(&mut env, first, &samples, 1, false);
    pump(&mut env, 8);
    let a = status(&mut env, 0);
    let b = status(&mut env, 1);
    assert_eq!(a.failure, None);
    assert_eq!(b.failure, None);
    assert!(a.counters.accepted > 0);
    assert!(b.counters.accepted > 0);
    assert!(a.counters.accepted < SLOTS_PER_FRAME);
    assert!(b.counters.accepted < SLOTS_PER_FRAME);
    assert!(a.counters.accepted.abs_diff(b.counters.accepted) <= 1);
    assert_eq!(a.last_accepted.unwrap().identity, identity(0, 1));
    assert_eq!(b.last_accepted.unwrap().identity, identity(1, 1));
    assert_eq!(a.last_command, Some(Bit::One));
    assert_eq!(b.last_command, Some(Bit::Zero));
    assert_eq!(
        env.rx.counters().feedback_accepted,
        a.counters.accepted + b.counters.accepted
    );
    assert_eq!(env.rx.counters().feedback_protocol_errors, 0);
}

#[test]
fn unacknowledged_feedback_bounds_sample_progress_until_the_actual_tx_receipt() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment(1, 1);
    feed(&mut env, first, &samples, 1, false);
    for _ in 0..16 {
        run(&mut env.rx);
    }
    let blocked = status(&mut env, 0);
    assert_eq!(blocked.counters.produced, 2);
    assert_eq!(blocked.counters.submitted, 1);
    assert_eq!(blocked.counters.accepted, 0);
    assert_eq!(env.rx.counters().sampled_slots, 2);
    assert_eq!(env.tx.service(), Ok(1));
    run(&mut env.rx);
    let resumed = status(&mut env, 0);
    assert_eq!(resumed.counters.accepted, 1);
    assert_eq!(resumed.counters.produced, 2);
    assert_eq!(resumed.counters.submitted, 2);
    assert_eq!(env.rx.counters().sampled_slots, 3);
    assert!(env.control.try_event().is_none());
}

#[test]
fn real_tx_identity_rejection_is_terminal_and_visible_to_the_rx_owner() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment(1, 0);
    feed(&mut env, first, &samples, 1, false);
    run(&mut env.rx);
    assert_eq!(env.tx.service(), Ok(1));
    let mut failures = Vec::new();
    for _ in 0..8 {
        env.rx.service(|id, result| {
            if let Err(error) = result {
                failures.push((id, error));
            }
        });
    }
    let observed = status(&mut env, 0);
    assert_eq!(observed.counters.produced, 1);
    assert_eq!(observed.counters.accepted, 0);
    assert_eq!(
        observed.failure,
        Some(ReceiveError::Feedback(TransmitError::Identity))
    );
    assert!(failures.iter().any(|(id, error)| *id == identity(0, 1)
        && *error == ReceiveError::Feedback(TransmitError::Identity)));
    assert_eq!(env.rx.counters().feedback_rejected, 1);
    assert_eq!(env.rx.counters().feedback_protocol_errors, 0);
    assert_eq!(env.rx.counters().sampled_slots, 1);
    assert_eq!(env.tx.service(), Ok(0));
}

#[test]
fn new_sample_generation_reacquires_and_publishes_only_new_provenance() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 2, 1, 1.0));
    let split = CHIPS_PER_FRAME as usize * 2;
    let mut env = environment(1, 1);
    let mut prepared = vec![Complex32::default(); env._source.samples_per_slot()];
    while env.tx.bank().next_chip().index < 100_000 {
        env.tx
            .generate_slot(&mut env._source, &mut prepared)
            .unwrap();
    }
    feed(&mut env, first, &samples[..split], 1, false);
    pump(&mut env, 48);
    let before = status(&mut env, 0);
    assert!(before.counters.accepted > 0);
    assert!(before.counters.accepted < SLOTS_PER_FRAME);
    feed(&mut env, first + split as u64, &samples[split..], 2, true);
    let mut failures = Vec::new();
    for _ in 0..24 {
        env.tx.service().unwrap();
        env.rx.service(|_, result| {
            if let Err(error) = result {
                failures.push(error);
            }
        });
    }
    let after = status(&mut env, 0);
    assert_eq!(after.failure, None);
    assert!(failures.is_empty());
    assert!(after.counters.produced > before.counters.produced);
    assert!(after.counters.accepted > before.counters.accepted);
    assert_eq!(after.last_accepted.unwrap().sample_generation, 2);
    assert_eq!(after.last_accepted.unwrap().continuity, 2);
    assert_eq!(env.rx.counters().feedback_protocol_errors, 0);
}

#[test]
fn delayed_publication_preserves_the_pending_measurement() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 2, 1.0));
    let mut env = environment(2, 2);
    feed(&mut env, first, &samples, 1, false);
    for _ in 0..8 {
        run(&mut env.rx);
    }
    let pending = status(&mut env, 1);
    assert_eq!(pending.counters.produced, 1);
    assert_eq!(pending.counters.submitted, 0);
    assert_eq!(pending.counters.accepted, 0);
    assert!(pending.counters.backpressure > 0);
    (env._clock)(120_000).unwrap();
    let mut failures = Vec::new();
    env.rx.service(|id, result| {
        if let Err(error) = result {
            failures.push((id, error));
        }
    });
    let expired = status(&mut env, 1);
    assert_eq!(expired.failure, None);
    assert!(failures.is_empty());
    assert_eq!(expired.counters.produced, 1);
    assert_eq!(expired.counters.submitted, 0);
    assert_eq!(expired.counters.accepted, 0);
    assert_eq!(expired.last_accepted, None);
    assert_eq!(env.tx.service(), Ok(1));
    run(&mut env.rx);
    let after_receipt = status(&mut env, 1);
    assert_eq!(after_receipt.counters.submitted, 1);
    assert_eq!(after_receipt.counters.accepted, 0);
    assert_eq!(after_receipt.failure, expired.failure);
    assert_eq!(env.tx.service(), Ok(1));
    run(&mut env.rx);
    let accepted = status(&mut env, 1);
    assert_eq!(accepted.counters.accepted, 1);
    assert!(
        accepted.last_accepted.unwrap().preparation_frontier.index >= 120_000 + 2 * CHIPS_PER_SLOT
    );
    assert_eq!(accepted.failure, None);
}

#[test]
fn eight_bounded_feedback_credits_decouple_sample_decode_from_tx_receipt_round_trips() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment_with_capacity(1, 1, 8);
    feed(&mut env, first, &samples, 1, false);
    for _ in 0..32 {
        run(&mut env.rx);
    }
    let blocked = status(&mut env, 0);
    assert_eq!(blocked.counters.produced, 8);
    assert_eq!(blocked.counters.submitted, 8);
    assert!(blocked.counters.backpressure > 0);
    assert_eq!(blocked.counters.accepted, 0);
    assert_eq!(env.rx.counters().sampled_slots, 8);
    assert_eq!(blocked.failure, None);
    pump(&mut env, 64);
    let finished = status(&mut env, 0);
    assert_eq!(finished.counters.accepted, SLOTS_PER_FRAME);
    assert_eq!(finished.counters.produced, SLOTS_PER_FRAME);
    assert_eq!(finished.failure, None);
    assert_eq!(env.rx.counters().feedback_protocol_errors, 0);
}

#[test]
fn tx_cursor_overtaking_an_estimate_retires_its_receipt_and_accepts_fresh_feedback() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment_with_capacity(1, 1, 8);
    feed(&mut env, first, &samples, 1, false);
    run(&mut env.rx);
    let mut output = vec![Complex32::default(); env._source.samples_per_slot()];
    while env.tx.bank().next_chip().index < 140_000 {
        env.tx.generate_slot(&mut env._source, &mut output).unwrap();
    }
    (env._clock)(140_000).unwrap();
    for _ in 0..64 {
        env.tx.service().unwrap();
        run(&mut env.rx);
    }
    let observed = status(&mut env, 0);
    assert_eq!(observed.failure, None);
    assert!(observed.counters.retired > 0);
    assert!(observed.counters.accepted > 0);
    assert_eq!(observed.counters.rejected, 0);
    assert_eq!(env.rx.first_failure(), None);
    assert_eq!(env.rx.counters().sampled_slots, SLOTS_PER_FRAME);
}

#[test]
fn dropping_the_clock_writer_fails_closed_before_any_new_pilot_decision() {
    let config = configuration();
    let (first, samples) = filtered(&waveform(&config, 1, 1, 1.0));
    let mut env = environment(1, 1);
    feed(&mut env, first, &samples, 1, false);
    env._clock = Box::new(|_| Ok(()));
    let mut failures = Vec::new();
    env.rx.service(|id, result| {
        if let Err(error) = result {
            failures.push((id, error));
        }
    });
    let observed = status(&mut env, 0);
    assert_eq!(observed.failure, Some(ReceiveError::FeedbackClosed));
    assert!(failures.contains(&(identity(0, 1), ReceiveError::FeedbackClosed)));
    let first_failure = env.rx.first_failure().unwrap().clone();
    assert_eq!(first_failure.identity, identity(0, 1));
    assert_eq!(first_failure.error, ReceiveError::FeedbackClosed);
    assert!(first_failure.latest_input_sample.is_some());
    assert_eq!(first_failure.sample_provenance, Some((1, 1)));
    for _ in 0..4 {
        env.rx.service(|_, _| {});
    }
    assert_eq!(env.rx.first_failure(), Some(&first_failure));
    assert_eq!(observed.counters.produced, 0);
    assert_eq!(observed.counters.submitted, 0);
    assert_eq!(observed.counters.accepted, 0);
    assert_eq!(env.rx.counters().sampled_slots, 0);
    assert_eq!(env.tx.service(), Ok(0));
}
