// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::radio_io::MatchedSamples;
use num_complex::Complex32;
use umts_dsp::{
    fractional_sampling::SamplePosition,
    pulse_shaping::{PulseShaper, SamplesPerChip},
    receive_filter::{FilteredWindow, ReceiveFilter},
};
use umts_types::time::{CHIPS_PER_SLOT, SampleInstant};

mod feedback;
mod hsupa;
mod routing;
mod switching;

fn sample(index: u64) -> SampleInstant {
    SampleInstant {
        epoch: epoch(),
        index,
    }
}
fn runtime_limits() -> SampleRuntimeLimits {
    SampleRuntimeLimits {
        shard: ShardLimits {
            maximum_ues: count(2),
            maximum_sessions_per_step: count(2),
            resources: ReceiveResources {
                physical_sample_bytes: 8_000_000,
                coded_bits: 1_000_000,
                output_bytes: 1_000_000,
                demultiplexed_bits: 1_000_000,
                channels: 8,
            },
        },
        history_samples: count(CHIPS_PER_FRAME as usize * 4),
        maximum_shared_bytes: 4_000_000,
        command_capacity: count(2),
        commands_per_step: count(1),
        output_slots_per_ue: count(1),
        maximum_output_bits: count(5000),
        maximum_output_bytes: 10000,
    }
}
fn owner(slot: u32, config: Arc<UplinkTfcs>, first: u64) -> Box<DedicatedSampleReceiver> {
    owner_at(slot, 1, 0, config, first)
}
fn owner_at(
    slot: u32,
    generation: u64,
    frame: u64,
    config: Arc<UplinkTfcs>,
    first: u64,
) -> Box<DedicatedSampleReceiver> {
    Box::new(
        DedicatedSampleReceiver::new(
            chips::receiver_at(slot, generation, frame, config),
            SampleReceiveSettings {
                radio_link: Default::default(),
                rate: SamplesPerChip::Two,
                first_receive: SamplePosition::new(sample(first + 2), 0.0).unwrap(),
                search_radius_samples: 8,
                minimum_coherent_fraction: 0.05,
            },
        )
        .unwrap(),
    )
}
fn install(
    port: &mut DedicatedReceivePort,
    receiver: Box<DedicatedSampleReceiver>,
) -> ReceiveCommandTicket {
    match port.install(receiver) {
        Ok(ticket) => ticket,
        Err(_) => panic!("expected command capacity"),
    }
}
fn run(runtime: &mut DedicatedSampleRuntime) {
    runtime.service(|_, result| {
        result.unwrap();
    });
}
fn drain(port: &mut DedicatedReceivePort, results: &mut Vec<(DecodedTransport, Vec<Bit>)>) {
    while port.process_one(|delivery| {
        results.push((delivery.decoded, delivery.payload.to_vec()));
        true
    }) {}
}
fn matched<'a>(
    first: u64,
    input: &'a [Complex32],
    generation: u64,
    discontinuity: bool,
) -> MatchedSamples<'a> {
    MatchedSamples {
        window: FilteredWindow {
            first_center: (!input.is_empty()).then_some(sample(first)),
            next_input: sample(first + input.len() as u64),
            sample_stride: 1,
        },
        generation,
        discontinuity,
        samples: input,
    }
}
fn waveform(config: &UplinkTfcs, frames: usize, ues: usize, gain: f32) -> Vec<Complex32> {
    let mut all = vec![Complex32::default(); 32];
    for frame in 0..frames {
        let mut combined = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        for ue in 0..ues {
            let wave = chips::waveform(
                config,
                chips::WaveformSettings {
                    frame: frame as u64,
                    voice: 1,
                    packet: 1,
                    seed: 7 + ue,
                    gain: gain * if ue == 0 { 1.0 } else { 0.7 },
                    cfo: 0.0,
                    code: 0x123456 + ue as u32,
                },
            );
            for (output, input) in combined.iter_mut().zip(wave) {
                *output += input;
            }
        }
        all.extend(combined);
    }
    all.resize(all.len() + 64, Complex32::default());
    let mut tx = PulseShaper::new(SamplesPerChip::Two, 32).unwrap();
    let mut raw = vec![Complex32::default(); tx.output_length(all.len()).unwrap()];
    tx.process(&all, &mut raw).unwrap();
    raw
}
fn filtered(raw: &[Complex32]) -> (u64, Vec<Complex32>) {
    let mut filter = ReceiveFilter::full_rate(SamplesPerChip::Two, 32, sample(20_000)).unwrap();
    let mut output = vec![Complex32::default(); filter.output_length(raw.len()).unwrap()];
    let window = filter.process(sample(20_000), raw, &mut output).unwrap();
    (window.first_center.unwrap().index, output)
}

#[test]
fn complete_slot_preflight_preserves_sample_emission_and_payloads_across_partitions() {
    let config = configuration();
    let raw = waveform(&config, 3, 1, 1.0);
    let (first, samples) = filtered(&raw);
    for chunk in [127, 511, 5120] {
        for shift in [-6_i64, 0, 6] {
            let mut reference = None;
            for enabled in [false, true] {
                let mut receiver = owner(0, config.clone(), (20_096_i64 + shift) as u64);
                // Isolate readiness preflight from the separate peak-reuse optimization.
                receiver.disable_timing_peak_reuse_for_test();
                if !enabled {
                    receiver.disable_slot_preflight_for_test();
                }
                let (mut runtime, mut port) =
                    DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits())
                        .unwrap();
                let mut ticket = install(&mut port, receiver);
                run(&mut runtime);
                assert!(matches!(
                    ticket.try_result(),
                    Some(ReceiveCommandResult::Installed(_))
                ));
                let mut results = Vec::new();
                let mut sampled_at = Vec::new();
                for (index, input) in samples.chunks(chunk).enumerate() {
                    let start = first + (index * chunk) as u64;
                    runtime.feed(matched(start, input, 1, false)).unwrap();
                    runtime.service(|_, result| {
                        if matches!(result.unwrap(), SampleProgress::Slot(_)) {
                            sampled_at.push(start + input.len() as u64);
                        }
                    });
                    drain(&mut port, &mut results);
                }
                for _ in 0..32 {
                    runtime.service(|_, result| {
                        if matches!(result.unwrap(), SampleProgress::Slot(_)) {
                            sampled_at.push(first + samples.len() as u64);
                        }
                    });
                    drain(&mut port, &mut results);
                }
                assert!(!sampled_at.is_empty());
                assert!(!results.is_empty());
                let mut removed = port.remove(identity(0, 1)).unwrap();
                run(&mut runtime);
                let Some(ReceiveCommandResult::Removed {
                    receiver: Some(receiver),
                    ..
                }) = removed.try_result()
                else {
                    panic!("missing preflight receiver")
                };
                let calls = receiver.counters().startup_timing_search_calls;
                if let Some((old_samples, old_results, old_calls)) = &reference {
                    assert_eq!(
                        &sampled_at, old_samples,
                        "slot latency changed chunk={chunk} shift={shift}"
                    );
                    assert_eq!(&results, old_results);
                    assert!(calls <= *old_calls);
                    if chunk == 127 {
                        assert!(calls < *old_calls);
                    }
                } else {
                    reference = Some((sampled_at, results, calls));
                }
            }
        }
    }
}

#[test]
fn pending_peak_reuse_preserves_slot_timing_across_discontinuity_and_history_eviction() {
    let config = configuration();
    let raw = waveform(&config, 3, 1, 1.0);
    let (first, samples) = filtered(&raw);
    for gap in [false, true] {
        for tiny_history in [false, true] {
            let mut reference = None;
            for enabled in [false, true] {
                let mut receiver = Box::new(
                    DedicatedSampleReceiver::new(
                        chips::receiver_at(0, 1, 0, config.clone()),
                        SampleReceiveSettings {
                            radio_link: Default::default(),
                            rate: SamplesPerChip::Two,
                            first_receive: SamplePosition::new(sample(20_098), 0.0).unwrap(),
                            search_radius_samples: 8,
                            minimum_coherent_fraction: 0.95,
                        },
                    )
                    .unwrap(),
                );
                receiver.confirm_startup_slots_for_test();
                if !enabled {
                    receiver.disable_timing_peak_reuse_for_test();
                }
                let mut limits = runtime_limits();
                if tiny_history {
                    limits.history_samples = count((CHIPS_PER_SLOT * 4 + 64) as usize);
                }
                let (mut runtime, mut port) =
                    DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
                let mut ticket = install(&mut port, receiver);
                run(&mut runtime);
                assert!(matches!(
                    ticket.try_result(),
                    Some(ReceiveCommandResult::Installed(_))
                ));
                let mut results = Vec::new();
                let mut sampled_at = Vec::new();
                for (index, input) in samples.chunks(127).enumerate() {
                    let start = first + (index * 127) as u64;
                    runtime
                        .feed(matched(start, input, 1, gap && index == 50))
                        .unwrap();
                    runtime.service(|_, result| {
                        if matches!(result.unwrap(), SampleProgress::Slot(_)) {
                            sampled_at.push(start + input.len() as u64);
                        }
                    });
                    drain(&mut port, &mut results);
                }
                for _ in 0..32 {
                    runtime.service(|_, result| {
                        if matches!(result.unwrap(), SampleProgress::Slot(_)) {
                            sampled_at.push(first + samples.len() as u64);
                        }
                    });
                    drain(&mut port, &mut results);
                }
                let mut removed = port.remove(identity(0, 1)).unwrap();
                run(&mut runtime);
                let Some(ReceiveCommandResult::Removed {
                    receiver: Some(receiver),
                    ..
                }) = removed.try_result()
                else {
                    panic!("missing reuse receiver")
                };
                let counts = receiver.counters();
                if !tiny_history {
                    assert!(!results.is_empty());
                }
                if let Some((old_samples, old_results, old_calls, old_lost)) = &reference {
                    assert_eq!(&sampled_at, old_samples, "gap={gap} tiny={tiny_history}");
                    assert_eq!(&results, old_results);
                    assert_eq!(counts.history_lost_frames, *old_lost);
                    assert!(counts.startup_timing_search_calls <= *old_calls);
                    if !tiny_history {
                        assert_eq!(counts.startup_timing_search_calls, if gap { 2 } else { 1 });
                        assert!(counts.startup_timing_search_calls < *old_calls);
                        assert!(counts.timing_peak_reuses > 0);
                    }
                } else {
                    reference = Some((
                        sampled_at,
                        results,
                        counts.startup_timing_search_calls,
                        counts.history_lost_frames,
                    ));
                }
            }
        }
    }
}

#[test]
fn startup_slot_probe_requires_three_pilots_and_preserves_next_frame_fallback() {
    for (probe, damage, expected_first) in [
        (false, false, CHIPS_PER_FRAME),
        (true, false, 0),
        (false, true, CHIPS_PER_FRAME),
        (true, true, CHIPS_PER_FRAME),
    ] {
        let config = configuration();
        let raw = waveform(&config, 3, 1, 1.0);
        let (first, mut samples) = filtered(&raw);
        if damage {
            let begin = (20_096 + 2 * CHIPS_PER_SLOT - first) as usize;
            let end = (20_096 + 6 * CHIPS_PER_SLOT - first) as usize;
            samples[begin..end].fill(Complex32::default());
        }
        let mut receiver = Box::new(
            DedicatedSampleReceiver::new(
                chips::receiver_at(0, 1, 0, config),
                SampleReceiveSettings {
                    radio_link: Default::default(),
                    rate: SamplesPerChip::Two,
                    first_receive: SamplePosition::new(sample(20_098), 0.0).unwrap(),
                    search_radius_samples: 8,
                    minimum_coherent_fraction: 0.95,
                },
            )
            .unwrap(),
        );
        if probe {
            receiver.confirm_startup_slots_for_test();
        }
        receiver.trace_acquisition_for_test(0);
        let (mut runtime, mut port) =
            DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
        let mut ticket = install(&mut port, receiver);
        run(&mut runtime);
        assert!(matches!(
            ticket.try_result(),
            Some(ReceiveCommandResult::Installed(_))
        ));
        let mut results = Vec::new();
        for (index, input) in samples.chunks(5120).enumerate() {
            runtime
                .feed(matched(first + (index * 5120) as u64, input, 1, false))
                .unwrap();
            run(&mut runtime);
            drain(&mut port, &mut results);
        }
        for _ in 0..32 {
            run(&mut runtime);
            drain(&mut port, &mut results);
        }
        let mut removed = port.remove(identity(0, 1)).unwrap();
        run(&mut runtime);
        let Some(ReceiveCommandResult::Removed {
            receiver: Some(receiver),
            ..
        }) = removed.try_result()
        else {
            panic!("missing probe receiver")
        };
        let accepted = receiver
            .acquisition_trace_for_test()
            .iter()
            .find(|row| row.accepted)
            .unwrap();
        assert_eq!(
            accepted.frame.index,
            at(0).index + expected_first,
            "probe={probe} damage={damage}"
        );
        assert!(accepted.peak.coherent_fraction < 0.95);
        assert_eq!(accepted.confirmed, expected_first != 0);
    }
}

#[test]
fn observation_reports_initial_tracking_state_without_advancing_samples() {
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let receiver = owner(0, configuration(), 20_096);
    let expected = receiver.first_receive();
    let mut installed = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        installed.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    let mut observed = port.observe(identity(0, 1)).unwrap();
    run(&mut runtime);
    let Some(ReceiveCommandResult::Observed {
        status: Some(status),
        ..
    }) = observed.try_result()
    else {
        panic!("expected tracking observation");
    };
    assert_eq!(status.tracking.predicted, expected);
    assert!(status.tracking.acquired.is_none());
    assert!(status.tracking.confirmation.is_none());
    assert!(!status.tracking.contiguous);
    assert_eq!(status.tracking.carrier_seed.hz(), 0.0);
    assert_eq!(status.samples, SampleReceiveCounters::default());
}

#[test]
fn shared_sample_history_recovers_two_ue_payloads_and_fairly_drains_one_output_slot() {
    for gain in [0.001, 1.0, 100.0] {
        let config = configuration();
        let raw = waveform(&config, 4, 2, gain);
        let (first, samples) = filtered(&raw);
        let (mut runtime, mut port) =
            DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
        let mut a = install(&mut port, owner(0, config.clone(), 20_096));
        let mut b = install(&mut port, owner(1, config.clone(), 20_096));
        assert!(a.try_result().is_none());
        run(&mut runtime);
        assert!(
            matches!(a.try_result(), Some(ReceiveCommandResult::Installed(key)) if key == identity(0, 1))
        );
        assert!(b.try_result().is_none());
        run(&mut runtime);
        assert!(
            matches!(b.try_result(), Some(ReceiveCommandResult::Installed(key)) if key == identity(1, 1))
        );
        for (index, input) in samples.chunks(5120).enumerate() {
            runtime
                .feed(matched(first + (index * 5120) as u64, input, 1, false))
                .unwrap();
            run(&mut runtime);
        }
        for _ in 0..40 {
            run(&mut runtime);
        }
        assert!(runtime.counters().backpressure > 0);
        let mut results = Vec::new();
        let mut attempted = None;
        assert!(!port.process_one(|delivery| {
            attempted = Some((delivery.decoded, delivery.payload.to_vec()));
            false
        }));
        assert!(port.process_one(|delivery| {
            assert_eq!(attempted.as_ref().unwrap().0, delivery.decoded);
            assert_eq!(attempted.as_ref().unwrap().1, delivery.payload);
            results.push((delivery.decoded, delivery.payload.to_vec()));
            true
        }));
        for _ in 0..32 {
            run(&mut runtime);
            drain(&mut port, &mut results);
        }
        assert_eq!(results.len(), 12);
        for ue in 0..2 {
            let selected: Vec<_> = results
                .iter()
                .filter(|(decoded, _)| decoded.identity == identity(ue, 1))
                .collect();
            assert_eq!(selected.len(), 6);
            assert_eq!(
                selected
                    .iter()
                    .filter(|(decoded, _)| decoded.transport == id(1))
                    .count(),
                2
            );
            assert_eq!(
                selected
                    .iter()
                    .filter(|(decoded, _)| decoded.transport == id(5))
                    .count(),
                4
            );
            for (decoded, bytes) in selected {
                assert!(decoded.crc_checked);
                assert_eq!(
                    *bytes,
                    payload(&config, decoded.transport, 1, 7 + ue as usize)
                );
            }
        }
        assert_ne!(results[0].0.identity, results[1].0.identity);
        assert_eq!(runtime.counters().sampled_slots, 120);
        assert_eq!(runtime.counters().failed_owners, 0);
    }
}

#[test]
fn inferred_gap_with_unchanged_source_generation_reacquires_and_discards_partial_tti() {
    let config = configuration();
    let raw = waveform(&config, 6, 1, 1.0);
    let (first, samples) = filtered(&raw);
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let _ticket = install(&mut port, owner(0, config.clone(), 20_096));
    run(&mut runtime);
    let mut results = Vec::new();
    for (index, input) in samples.chunks(5120).enumerate() {
        if index == 23 {
            continue;
        }
        runtime
            .feed(matched(first + (index * 5120) as u64, input, 1, false))
            .unwrap();
        run(&mut runtime);
        drain(&mut port, &mut results);
    }
    for _ in 0..40 {
        run(&mut runtime);
        drain(&mut port, &mut results);
    }
    assert_eq!(runtime.counters().discontinuities, 1);
    assert_eq!(runtime.counters().failed_owners, 0);
    let packet_frames: Vec<_> = results
        .iter()
        .filter(|(d, _)| d.transport == id(5))
        .map(|(d, _)| (d.start.index - at(0).index) / CHIPS_PER_FRAME)
        .collect();
    let voice_frames: Vec<_> = results
        .iter()
        .filter(|(d, _)| d.transport == id(1))
        .map(|(d, _)| (d.start.index - at(0).index) / CHIPS_PER_FRAME)
        .collect();
    assert_eq!(packet_frames, vec![0, 2, 3, 4, 5]);
    assert_eq!(voice_frames, vec![2, 4]);
    for (decoded, bytes) in results {
        assert_eq!(bytes, payload(&config, decoded.transport, 1, 7));
    }
}

#[test]
fn bounded_commands_return_rejected_owners_and_close_outstanding_tickets() {
    let config = configuration();
    let mut limits = runtime_limits();
    limits.command_capacity = count(1);
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    let mut accepted = install(&mut port, owner(0, config.clone(), 20_096));
    let returned = match port.install(owner(1, config.clone(), 20_096)) {
        Ok(_) => panic!("queue should be full"),
        Err(receiver) => receiver,
    };
    assert_eq!(returned.session().identity(), identity(1, 1));
    run(&mut runtime);
    assert!(matches!(
        accepted.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    assert!(accepted.try_result().is_none());
    let rejected = match port.install(owner(0, config.clone(), 20_096)) {
        Ok(_) => panic!("occupied control lane"),
        Err(receiver) => receiver,
    };
    assert_eq!(rejected.session().identity(), identity(0, 1));
    let mut stale = port.remove(identity(0, 2)).ok().unwrap();
    run(&mut runtime);
    assert!(matches!(
        stale.try_result(),
        Some(ReceiveCommandResult::Removed { receiver: None, .. })
    ));
    let mut removed = port.remove(identity(0, 1)).ok().unwrap();
    run(&mut runtime);
    let receiver = match removed.try_result().unwrap() {
        ReceiveCommandResult::Removed {
            receiver: Some(receiver),
            ..
        } => receiver,
        _ => panic!("missing removed owner"),
    };
    let mut replay = install(&mut port, receiver);
    run(&mut runtime);
    assert!(matches!(
        replay.try_result(),
        Some(ReceiveCommandResult::InstallRejected {
            error: ReceiveError::Identity,
            ..
        })
    ));
    let mut closed = install(&mut port, returned);
    drop(runtime);
    assert!(!port.available());
    assert!(matches!(
        closed.try_result(),
        Some(ReceiveCommandResult::Closed)
    ));
    assert!(closed.try_result().is_none());
    assert!(port.install(owner(0, config, 20_096)).is_err());
}

#[test]
fn invalid_sample_metadata_is_atomic_and_history_overwrite_skips_in_constant_work() {
    let config = configuration();
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let _ticket = install(&mut port, owner(0, config, 20_096));
    run(&mut runtime);
    let zero = [Complex32::default(); 5120];
    runtime.feed(matched(20_000, &zero, 1, false)).unwrap();
    let saved = runtime.counters();
    let mut bad = matched(25_120, &zero, 1, false);
    bad.window.sample_stride = 2;
    assert_eq!(runtime.feed(bad), Err(ReceiveError::Identity));
    bad = matched(25_120, &zero, 0, false);
    assert_eq!(runtime.feed(bad), Err(ReceiveError::Identity));
    bad = matched(25_120, &zero[..1], 1, false);
    bad.window.first_center = None;
    assert_eq!(runtime.feed(bad), Err(ReceiveError::Time));
    assert_eq!(runtime.counters(), saved);
    assert_eq!(
        runtime.feed(matched(19_999, &zero, 2, true)),
        Err(ReceiveError::Time)
    );
    let invalid = [Complex32::new(f32::NAN, 0.0)];
    assert!(matches!(
        runtime.feed(matched(25_120, &invalid, 1, false)),
        Err(ReceiveError::Dsp(_))
    ));
    assert_eq!(runtime.counters(), saved);
    runtime.feed(matched(25_120, &zero, 1, false)).unwrap();
    runtime
        .feed(matched(10_000_000_000, &zero, 1, false))
        .unwrap();
    run(&mut runtime);
    assert_eq!(runtime.counters().failed_owners, 0);
    assert_eq!(runtime.counters().discontinuities, 1);
    assert!(
        runtime
            .feed(matched(10_000_005_120, &zero, 0, false))
            .is_err()
    );
}

#[test]
fn rejected_acquisition_resolves_feedback_only_after_its_airtime_has_elapsed() {
    use crate::hsdpa_rx::{Evidence, Measurement, Request};
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let mut receiver = owner(0, configuration(), 20_096);
    let mut feedback = receiver
        .configure_hsdpa_feedback(count(2), count(2))
        .unwrap();
    feedback
        .request(Request::Cqi {
            start: at(0),
            repetitions: 1,
        })
        .unwrap();
    let _ticket = install(&mut port, receiver);
    run(&mut runtime);
    let zero = [Complex32::default(); 5120];
    runtime.feed(matched(20_000, &zero, 1, false)).unwrap();
    run(&mut runtime);
    assert!(feedback.try_report().is_none());
    for slot in 1..18 {
        runtime
            .feed(matched(20_000 + slot * 5120, &zero, 1, false))
            .unwrap();
        run(&mut runtime);
    }
    assert!(feedback.try_report().is_none());
    // Three-slot confirmation needs the final slot's interpolation guard before rejecting it.
    runtime
        .feed(matched(20_000 + 18 * 5120, &zero, 1, false))
        .unwrap();
    run(&mut runtime);
    let report = feedback
        .try_report()
        .expect("rejected frames do not require reacquisition");
    assert!(matches!(
        report.result,
        Ok(Measurement::Cqi(Evidence::Missing))
    ));
    assert_eq!(runtime.counters().failed_owners, 0);
}

#[test]
fn publication_racing_runtime_destruction_resolves_without_waiting_for_an_orphaned_queue() {
    use std::sync::Barrier;
    for _ in 0..8 {
        let (runtime, mut port) =
            DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
        let receiver = owner(0, configuration(), 20_096);
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            drop(runtime);
        });
        barrier.wait();
        let result = port.install(receiver);
        worker.join().unwrap();
        assert!(!port.available());
        if let Ok(mut ticket) = result {
            assert!(matches!(
                ticket.try_result(),
                Some(ReceiveCommandResult::Closed)
            ));
            assert!(ticket.try_result().is_none());
        }
    }
}

#[test]
fn empty_discontinuity_warmup_does_not_reuse_old_samples_or_replay_old_transport() {
    let config = configuration();
    let raw = waveform(&config, 6, 1, 1.0);
    let (first, samples) = filtered(&raw);
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let _ticket = install(&mut port, owner(0, config.clone(), 20_096));
    run(&mut runtime);
    let mut results = Vec::new();
    for (index, input) in samples.chunks(5120).enumerate() {
        let start = first + (index * 5120) as u64;
        if index == 23 {
            runtime.feed(matched(start, &[], 2, true)).unwrap();
            run(&mut runtime);
            continue;
        }
        runtime
            .feed(matched(start, input, if index > 23 { 2 } else { 1 }, false))
            .unwrap();
        run(&mut runtime);
        drain(&mut port, &mut results);
    }
    for _ in 0..40 {
        run(&mut runtime);
        drain(&mut port, &mut results);
    }
    assert_eq!(runtime.counters().discontinuities, 1);
    assert_eq!(runtime.counters().failed_owners, 0);
    let packet_frames: Vec<_> = results
        .iter()
        .filter(|(d, _)| d.transport == id(5))
        .map(|(d, _)| (d.start.index - at(0).index) / CHIPS_PER_FRAME)
        .collect();
    assert_eq!(packet_frames, vec![0, 2, 3, 4, 5]);
    assert!(
        !results
            .iter()
            .any(|(d, _)| d.transport == id(1) && d.start == at(0))
    );
    for (decoded, bytes) in results {
        assert_eq!(bytes, payload(&config, decoded.transport, 1, 7));
    }
}

#[test]
fn shared_and_per_owner_budgets_reject_before_registration() {
    let mut limits = runtime_limits();
    limits.maximum_shared_bytes = 1;
    assert!(DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).is_err());
    limits = runtime_limits();
    limits.maximum_output_bytes = 1;
    assert!(DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).is_err());
    limits = runtime_limits();
    limits.shard.resources.physical_sample_bytes = 1;
    let (mut runtime, mut port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, limits).unwrap();
    let mut ticket = install(&mut port, owner(0, configuration(), 20_096));
    run(&mut runtime);
    assert!(matches!(
        ticket.try_result(),
        Some(ReceiveCommandResult::InstallRejected {
            error: ReceiveError::ResourceBudget { required, maximum, .. },
            ..
        }) if required > maximum
    ));
}

#[test]
fn discontinuity_cannot_reintroduce_old_sample_coordinates_after_empty_warmup() {
    let (mut runtime, _port) =
        DedicatedSampleRuntime::new(epoch(), SamplesPerChip::Two, runtime_limits()).unwrap();
    let input = [Complex32::default(); 100];
    runtime.feed(matched(1000, &input, 1, false)).unwrap();
    runtime.feed(matched(1200, &[], 2, true)).unwrap();
    let saved = runtime.counters();
    let mut replay = matched(1000, &input, 2, false);
    replay.window.next_input = sample(1200);
    assert_eq!(runtime.feed(replay), Err(ReceiveError::Time));
    assert_eq!(runtime.counters(), saved);
    runtime.feed(matched(1200, &input, 2, false)).unwrap();
}

#[test]
fn hal_common_rx_dispatches_full_rate_dedicated_samples_without_another_radio_or_filter() {
    use crate::common_rx::tests::{advance_tx, pipeline};
    use sdr_hal::memory::IqBlock;
    let (mut preparation, mut pump, _transport, mut common, _ingress, peer) = pipeline(2);
    let config = configuration();
    let mut port = common.configure_dedicated(runtime_limits()).unwrap();
    let mut ticket = install(
        &mut port,
        owner(
            0,
            config.clone(),
            96 - preparation.source().group_delay_samples() as u64,
        ),
    );
    let raw = waveform(&config, 4, 1, 1.0);
    let sample_count = raw.len();
    peer.input
        .send(IqBlock {
            samples: raw,
            time_ticks: Some(1_000_000_000),
            discontinuity: false,
        })
        .unwrap();
    common.start(None).unwrap();
    assert!(matches!(
        common.configure_dedicated(runtime_limits()),
        Err(ReceiveError::Occupied)
    ));
    let mut results = Vec::new();
    while common.statistics().radio.input_samples < sample_count as u64 {
        common.cycle().unwrap();
        drain(&mut port, &mut results);
        advance_tx(
            &mut preparation,
            &mut pump,
            &peer,
            common.statistics().radio.input_samples / 2,
        );
    }
    for _ in 0..40 {
        common.cycle().unwrap();
        drain(&mut port, &mut results);
    }
    assert!(matches!(
        ticket.try_result(),
        Some(ReceiveCommandResult::Installed(_))
    ));
    assert_eq!(results.len(), 6);
    assert_eq!(common.statistics().dedicated.unwrap().sampled_slots, 60);
    for (decoded, bytes) in results {
        assert_eq!(bytes, payload(&config, decoded.transport, 1, 7));
    }
    common.stop().unwrap();
    pump.stop().unwrap();
}
