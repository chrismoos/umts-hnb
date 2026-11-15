// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_control::{ControlDecision, ControlIngress},
    common_tx::{CommonTx, ControlPorts},
    radio_io::{PumpProgress, RxSettings, TxPreparation, TxPump, TxSettings, prepare_tx},
    test_support::{at, count, source},
};
use sdr_hal::{
    Radio,
    memory::{IqBlock, MemoryRadio, MemoryRadioHandle},
};
use std::time::Duration;
use umts_dsp::{
    bits::{Bit, unpack_msb},
    prach::PrachCode,
    pulse_shaping::PulseShaper,
    receive_filter::FilteredWindow,
};
use umts_rlc::transparent::TmLimits;
use umts_types::{
    bits::Bits,
    time::{CHIPS_PER_FRAME, SampleInstant},
};

const NOMINAL: u64 = 23040;
const CYCLE: u64 = 61440;
const MESSAGE_DELAY: u64 = 20480;

#[test]
fn dedicated_progress_prevents_idle_wait_without_new_radio_input() {
    let idle = CycleProgress {
        received: false,
        access: DecodeProgress::Idle,
        sampled_slots: 0,
        decode_state_updates: 0,
    };
    assert!(idle.is_idle());
    assert!(
        CycleProgress {
            access: DecodeProgress::Backpressure,
            ..idle
        }
        .is_idle()
    );
    for progress in [
        CycleProgress {
            received: true,
            ..idle
        },
        CycleProgress {
            sampled_slots: 1,
            ..idle
        },
        CycleProgress {
            decode_state_updates: 1,
            ..idle
        },
        CycleProgress {
            access: DecodeProgress::Published,
            ..idle
        },
        CycleProgress {
            access: DecodeProgress::Backpressure,
            decode_state_updates: 1,
            ..idle
        },
    ] {
        assert!(!progress.is_idle());
    }
}

fn settings() -> AccessRxSettings {
    AccessRxSettings {
        maximum_channels: count(2),
        messages: MessageLimits {
            capacity: count(2),
            maximum_payload_bits: count(512),
            maximum_formats: count(2),
            maximum_buffer_bytes: 4 * 1024 * 1024,
        },
        minimum_peak_to_background_ratio: 20.0,
        minimum_tx_lead_chips: 0,
    }
}
fn queue(capacity: usize) -> RachQueueLimits {
    RachQueueLimits {
        capacity: count(capacity),
        maximum_transport_bits: count(512),
        maximum_buffer_bytes: 4096,
    }
}
fn view(first: u64, chips: &[Complex32], generation: u64, discontinuity: bool) -> FilteredRx<'_> {
    FilteredRx {
        window: FilteredWindow {
            first_center: Some(SampleInstant {
                epoch: at(0).epoch,
                index: first * 2,
            }),
            sample_stride: 2,
            next_input: SampleInstant {
                epoch: at(0).epoch,
                index: (first + chips.len() as u64) * 2,
            },
        },
        generation,
        discontinuity,
        chips,
    }
}
fn preamble(chips: &mut [Complex32], nominal: u64, delay: usize, amplitude: f32) {
    let mut preamble = vec![Complex32::default(); PREAMBLE_CHIPS];
    PrachCode::new(7504)
        .unwrap()
        .write_preamble(PreambleSignature::new(13).unwrap(), &mut preamble)
        .unwrap();
    for (target, value) in chips[nominal as usize + delay..].iter_mut().zip(preamble) {
        *target += value * Complex32::new(amplitude * 0.8, amplitude * 0.6);
    }
}
fn feed(
    access: &mut AccessReceiver,
    tx: &mut CommonTx,
    first: u64,
    chips: &[Complex32],
    generation: u64,
) {
    for (chunk, samples) in chips.chunks(997).enumerate() {
        access
            .feed(
                tx,
                view(first + (chunk * 997) as u64, samples, generation, false),
            )
            .unwrap();
    }
}

#[test]
fn automatic_search_is_gain_normalized_and_follows_only_advertised_occasions() {
    for amplitude in [1e-12, 0.05, 5.0] {
        let mut tx = source(false);
        let mut access = AccessReceiver::new(&tx, SamplesPerChip::Two, settings()).unwrap();
        let mut chips = vec![Complex32::default(); (NOMINAL + CYCLE + 8192) as usize];
        preamble(&mut chips, NOMINAL, 17, amplitude);
        preamble(&mut chips, NOMINAL + 5120, 17, amplitude);
        for (index, chip) in chips.iter_mut().enumerate() {
            *chip += Complex32::new(
                ((index * 13 % 19) as f32 - 9.0) * amplitude * 0.01,
                ((index * 7 % 23) as f32 - 11.0) * amplitude * 0.01,
            );
        }
        feed(&mut access, &mut tx, 0, &chips, 1);
        assert_eq!(access.counters().searched, 2);
        assert_eq!(access.counters().candidates, 1);
        assert_eq!(access.counters().admitted, 1);
        assert_eq!(access.counters().failed_crc, 0);
        let evidence = access.search_evidence();
        assert_eq!(evidence.signature_peaks, 2);
        let strongest = evidence.strongest_coherence.unwrap();
        assert_eq!(strongest.nominal_start, at(NOMINAL));
        assert_eq!(strongest.correlation_start, at(NOMINAL + 17));
        assert_eq!(strongest.signature.number(), 13);
        assert!(strongest.peak_to_background_ratio >= settings().minimum_peak_to_background_ratio);
        assert!(strongest.mean_input_power > 0.0);
        assert_eq!(evidence.last.unwrap().nominal_start, at(NOMINAL + CYCLE));
    }
}

#[test]
fn search_recovers_at_the_next_occasion_after_missing_samples_without_manual_arm() {
    let mut tx = source(false);
    let mut access = AccessReceiver::new(&tx, SamplesPerChip::Two, settings()).unwrap();
    let mut chips = vec![Complex32::default(); (NOMINAL + CYCLE + 8192) as usize];
    preamble(&mut chips, NOMINAL, 17, 0.05);
    preamble(&mut chips, NOMINAL + CYCLE, 31, 0.05);
    let split = NOMINAL as usize + 2048;
    feed(&mut access, &mut tx, 0, &chips[..split], 1);
    let resumed = split + 100;
    feed(&mut access, &mut tx, resumed as u64, &chips[resumed..], 2);
    assert_eq!(access.counters().discontinuities, 1);
    assert_eq!(access.counters().candidates, 1);
    assert_eq!(access.counters().admitted, 1);
    let channel = &access.channels[0];
    assert_eq!(channel.messages.pending(), 1);
}

#[test]
fn history_retains_overlapping_windows_across_wrap_and_rejects_unretained_or_missing_data() {
    let mut history = History::new(12);
    history.reset(3);
    let chips: Vec<_> = (0..20)
        .map(|index| Complex32::new(index as f32, -(index as f32)))
        .collect();
    history.append(3, &chips[..9]).unwrap();
    history.append(12, &chips[9..]).unwrap();
    assert_eq!(history.window(11, 12).unwrap(), &chips[8..]);
    assert_eq!(history.window(14, 8).unwrap(), &chips[11..19]);
    assert_eq!(history.window(10, 4), Err(AccessError::Time));
    assert_eq!(history.append(24, &chips[..1]), Err(AccessError::Time));
}

#[test]
fn automatic_access_rejects_invalid_policy_metadata_and_keeps_failure_terminal() {
    let mut tx = source(false);
    for threshold in [0.0, -0.1, 1.0, f64::NAN] {
        let mut invalid = settings();
        invalid.minimum_peak_to_background_ratio = threshold;
        assert!(matches!(
            AccessReceiver::new(&tx, SamplesPerChip::Two, invalid),
            Err(AccessError::Configuration)
        ));
    }
    let mut invalid = settings();
    invalid.minimum_tx_lead_chips = 12800;
    assert!(matches!(
        AccessReceiver::new(&tx, SamplesPerChip::Two, invalid),
        Err(AccessError::Configuration)
    ));
    assert!(AccessReceiver::new(&tx, SamplesPerChip::Four, settings()).is_ok());
    let mut access = AccessReceiver::new(&tx, SamplesPerChip::Two, settings()).unwrap();
    assert_eq!(
        access.feed(&mut tx, view(0, &[Complex32::new(f32::NAN, 0.0)], 1, false)),
        Err(AccessError::Dsp(DspError::NonFiniteEvidence))
    );
    assert_eq!(
        access.feed(&mut tx, view(0, &[Complex32::default()], 1, false)),
        Err(AccessError::Poisoned)
    );
}

fn request_payload(identity: u8) -> Vec<Bit> {
    let mut request = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];
    request[7] = identity;
    let mut bytes = [0; 32];
    umts_mac::encode_common(
        umts_mac::FddCommonChannel::Rach,
        umts_mac::CommonHeader::CommonControl,
        Bits::new(&request, 78).unwrap(),
        &mut bytes,
    )
    .unwrap();
    let mut payload = vec![Bit::Zero; 256];
    unpack_msb(&bytes, &mut payload).unwrap();
    payload
}

pub(crate) fn sampled_requests() -> Vec<Complex32> {
    let last = NOMINAL + CYCLE + MESSAGE_DELAY + 31 + CHIPS_PER_FRAME;
    let mut chips = vec![Complex32::default(); last as usize + 64];
    for (cycle, delay, identity) in [(0, 17, 1), (1, 31, 2)] {
        let nominal = NOMINAL + cycle * CYCLE;
        preamble(&mut chips, nominal, delay, 0.05);
        let message = crate::access_message::tests::waveform_payload(
            &request_payload(identity),
            0,
            0.05,
            false,
        );
        let first = (nominal + MESSAGE_DELAY) as usize + delay;
        chips[first..first + message.len()].copy_from_slice(&message);
    }
    let mut samples = vec![Complex32::default(); chips.len() * 2];
    PulseShaper::new(SamplesPerChip::Two, 32)
        .unwrap()
        .process(&chips, &mut samples)
        .unwrap();
    samples
}

pub(crate) fn pipeline(
    queue_capacity: usize,
) -> (
    TxPreparation,
    TxPump,
    crate::common_tx::CommonTxPort,
    CommonRx,
    RachIngress,
    MemoryRadioHandle,
) {
    let (mut radio, peer) = MemoryRadio::new(2);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    radio
        .setup_rx(0, "", 7_680_000.0, 5_000_000.0, None)
        .unwrap();
    let (tx, rx) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source(false),
        tx,
        TxSettings::offline(1_000_000_000, count(1), count(1), count(3)),
    )
    .unwrap();
    let ControlPorts {
        transport,
        acquisition,
    } = preparation
        .control_ports(count(4), count(1), count(4), count(2))
        .unwrap();
    let receiver = TimedRx::new(
        rx.unwrap(),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: 32,
            phase: 0,
            maximum_samples: count(997),
            timeout_us: 1000,
        },
    )
    .unwrap();
    let (runtime, ingress) = prepare_common_rx(
        receiver,
        acquisition,
        SamplesPerChip::Two,
        settings(),
        queue(queue_capacity),
    )
    .unwrap();
    pump.start().unwrap();
    (preparation, pump, transport, runtime, ingress, peer)
}

pub(crate) fn advance_tx(
    preparation: &mut TxPreparation,
    pump: &mut TxPump,
    peer: &MemoryRadioHandle,
    first: u64,
) {
    while preparation.source().next_chip().index < first {
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle(), Ok(PumpProgress::Submitted(_))));
        peer.output.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}

#[test]
fn sampled_automatic_rx_publishes_two_requests_with_bounded_backpressure_and_original_provenance() {
    let (mut preparation, mut pump, _transport, mut runtime, mut ingress, peer) = pipeline(1);
    let samples = sampled_requests();
    let sample_count = samples.len() as u64;
    peer.input
        .send(IqBlock {
            samples,
            time_ticks: Some(1_000_000_000),
            discontinuity: false,
        })
        .unwrap();
    runtime.start(None).unwrap();
    let mut reads = 0;
    while runtime.receiver.counters().input_samples < sample_count {
        reads += 1;
        assert!(reads < 1000);
        runtime.cycle().unwrap();
        let received = runtime.receiver.counters().input_samples;
        advance_tx(&mut preparation, &mut pump, &peer, received / 2);
    }
    assert_eq!(runtime.access.counters().admitted, 2);
    assert_eq!(runtime.access.counters().late, 0);
    assert_eq!(runtime.access.counters().decoded, 1);
    assert_eq!(runtime.published, 1);
    assert!(runtime.access.channels[0].messages.ready());
    assert_eq!(
        runtime.cycle().unwrap().access,
        DecodeProgress::Backpressure
    );
    let mut control = ControlIngress::new(
        at(0).epoch,
        TmLimits {
            maximum_pdus: count(2),
            maximum_pdu_bits: count(254),
            maximum_sdu_bits: count(254),
        },
        count(2),
    )
    .unwrap();
    for index in 0..2 {
        let expected = NOMINAL + index * CYCLE + MESSAGE_DELAY + if index == 0 { 17 } else { 31 };
        assert_eq!(
            ingress
                .process_one_with(|batch| {
                    assert_eq!(batch.transport().start, at(expected));
                    assert_eq!(batch.transport().signature.number(), 13);
                    assert_eq!(batch.pdus().len(), 1);
                    ControlDecision::Retry(())
                })
                .unwrap(),
            Some(ControlDecision::Retry(()))
        );
        assert_eq!(
            ingress
                .process_one_with(|batch| {
                    control.enqueue(&batch).unwrap();
                    ControlDecision::Consume(())
                })
                .unwrap(),
            Some(ControlDecision::Consume(()))
        );
        if index == 0 {
            assert_eq!(runtime.cycle().unwrap().access, DecodeProgress::Published);
        }
    }
    assert_eq!(runtime.access.counters().decoded, 2);
    assert_eq!(runtime.published, 2);
    assert!(!runtime.access.channels[0].messages.ready());
    assert_eq!(control.pending(), 2);
    let first = control.decode_one().unwrap().message.unwrap();
    let second = control.decode_one().unwrap().message.unwrap();
    assert_ne!(
        format!("{:?}", first.value.message),
        format!("{:?}", second.value.message)
    );
    runtime.stop().unwrap();
    pump.stop().unwrap();
}

#[test]
fn ingress_expiry_is_one_transport_per_call_and_foreign_epochs_do_not_consume() {
    let (mut tx, mut pump, _transport, mut runtime, mut ingress, peer) = pipeline(2);
    let samples = sampled_requests();
    let count = samples.len() as u64;
    peer.input
        .send(IqBlock {
            samples,
            time_ticks: Some(1_000_000_000),
            discontinuity: false,
        })
        .unwrap();
    runtime.start(None).unwrap();
    while runtime.receiver.counters().input_samples < count {
        runtime.cycle().unwrap();
        advance_tx(
            &mut tx,
            &mut pump,
            &peer,
            runtime.receiver.counters().input_samples / 2,
        );
    }
    let mut foreign = at(200000);
    foreign.epoch = RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
    assert_eq!(ingress.expire_one_before(foreign), Err(IngressError::Epoch));
    assert_eq!(ingress.expire_one_before(at(200000)), Ok(true));
    assert_eq!(ingress.expire_one_before(at(200000)), Ok(true));
    assert_eq!(ingress.expire_one_before(at(200000)), Ok(false));
}

#[test]
fn rx_worker_owns_shutdown_and_propagates_endpoint_loss_to_the_shared_epoch() {
    use umts_runtime::worker::{StopToken, WorkerExit};
    for (closed, stopped) in [(false, true), (true, false), (true, true)] {
        let (_tx, _pump, transport, runtime, ingress, _peer) = pipeline(2);
        let stop = StopToken::default();
        if closed {
            drop(transport);
        }
        if stopped {
            stop.request_stop();
        }
        let mut worker =
            RxWorker::spawn(runtime, stop.clone(), None, Duration::from_micros(100)).unwrap();
        let report = worker.join(Duration::from_secs(1)).unwrap();
        if closed && !stopped {
            assert!(matches!(report.exit, WorkerExit::Failed(_)));
        } else {
            assert_eq!(report.exit, WorkerExit::Stopped);
        }
        assert!(stop.is_stopping());
        assert!(report.statistics.is_some());
        drop(ingress);
    }
}

#[test]
fn ingress_limits_are_checked_before_allocating_and_releasing_owner_stops_rx() {
    let mut limits = queue(65);
    assert!(matches!(
        ingress::channel(at(0).epoch, limits),
        Err(IngressError::Capacity)
    ));
    limits = queue(2);
    limits.maximum_buffer_bytes = 127;
    assert!(matches!(
        ingress::channel(at(0).epoch, limits),
        Err(IngressError::Capacity)
    ));
    let (_tx, _pump, _transport, mut runtime, ingress, _peer) = pipeline(2);
    drop(ingress);
    assert!(matches!(runtime.start(None), Err(CommonRxError::Closed)));
    assert!(matches!(runtime.cycle(), Err(CommonRxError::Closed)));
}

#[test]
fn message_expiry_uses_filtered_progress_not_raw_read_time_ahead_of_the_filter() {
    let mut tx = source(false);
    let mut access = AccessReceiver::new(&tx, SamplesPerChip::Two, settings()).unwrap();
    let payload = request_payload(3);
    let message = crate::access_message::tests::waveform_payload(&payload, 0, 0.05, false);
    let start = NOMINAL + MESSAGE_DELAY + 17;
    let end = start + message.len() as u64;
    let mut chips = vec![Complex32::default(); end as usize];
    preamble(&mut chips, NOMINAL, 17, 0.05);
    chips[start as usize..].copy_from_slice(&message);
    let before_end = chips.len() - 16;
    let mut first = 0;
    while first < before_end {
        let next = (first + 997).min(before_end);
        let mut input = view(first as u64, &chips[first..next], 1, false);
        input.window.next_input.index += 64 * 2;
        access.feed(&mut tx, input).unwrap();
        first = next;
    }
    assert_eq!(access.channels[0].messages.pending(), 1);
    assert!(!access.channels[0].messages.ready());
    assert_eq!(access.counters().expired, 0);
    let mut final_input = view(first as u64, &chips[first..], 1, false);
    final_input.window.next_input.index += 64 * 2;
    access.feed(&mut tx, final_input).unwrap();
    let delivery = access.decode_one().unwrap().unwrap();
    assert_eq!(delivery.payload, payload);
    assert_eq!(access.counters().expired, 0);
}
