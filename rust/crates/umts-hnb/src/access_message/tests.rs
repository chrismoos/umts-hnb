// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count, source};
use umts_dsp::{
    code_blocks::ConvolutionalBlocks, interleaving::Interleaver, rate_matching::RateMatching,
    receive_filter::FilteredWindow,
};
use umts_types::time::{CHIPS_PER_SLOT, SampleInstant};

const BUFFER_BUDGET: usize = 4 * 1024 * 1024;
const NOMINAL: u64 = 23_040;
const MESSAGE_DELAY: u64 = 20_480;
const ACCESS_CYCLE: u64 = 61_440;
const BLOCK_BITS: usize = 256;

fn pool(source: &dyn AccessTxTarget, capacity: usize) -> AccessMessages {
    AccessMessages::new(source, 0, SamplesPerChip::Two, limits(capacity)).unwrap()
}

fn limits(capacity: usize) -> MessageLimits {
    MessageLimits {
        capacity: count(capacity),
        maximum_payload_bits: count(512),
        maximum_formats: count(2),
        maximum_buffer_bytes: BUFFER_BUDGET,
    }
}

fn observation(nominal: u64, delay: u64) -> AccessObservation {
    AccessObservation {
        code: PrachCode::new(7504).unwrap(),
        signature: PreambleSignature::new(13).unwrap(),
        nominal_start: at(nominal),
        received_start: at(nominal + delay),
    }
}

fn view(first: u64, chips: &[Complex32], generation: u64, discontinuity: bool) -> FilteredRx<'_> {
    FilteredRx {
        window: FilteredWindow {
            first_center: (!chips.is_empty()).then_some(SampleInstant {
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

fn feed(pool: &mut AccessMessages, start: u64, chips: &[Complex32], generation: u64) {
    let mut offset = 0;
    for size in [1, 13, 256, 17, 2560].into_iter().cycle() {
        if offset == chips.len() {
            break;
        }
        let end = (offset + size).min(chips.len());
        pool.feed(view(
            start + offset as u64,
            &chips[offset..end],
            generation,
            false,
        ))
        .unwrap();
        offset = end;
    }
}

fn waveform(blocks: usize, tfci: u16, gain: f32, corrupt_crc: bool) -> (Vec<Complex32>, Vec<Bit>) {
    let payload: Vec<_> = (0..blocks * BLOCK_BITS)
        .map(|index| Bit::from((index * 7 + index / BLOCK_BITS) % 13 < 5))
        .collect();
    (waveform_payload(&payload, tfci, gain, corrupt_crc), payload)
}

pub(crate) fn waveform_payload(
    payload: &[Bit],
    tfci: u16,
    gain: f32,
    corrupt_crc: bool,
) -> Vec<Complex32> {
    let blocks = payload.len() / BLOCK_BITS;
    let mut joined = Vec::new();
    for (index, block) in payload.chunks_exact(BLOCK_BITS).enumerate() {
        joined.extend_from_slice(block);
        let mut parity = [Bit::Zero; 16];
        TransportCrc::Crc16.write(block, &mut parity).unwrap();
        if corrupt_crc && index == blocks - 1 {
            parity[0] = Bit::from(parity[0] == Bit::Zero);
        }
        joined.extend(parity);
    }
    let segmentation = ConvolutionalBlocks::new(joined.len(), CodeRate::Half).unwrap();
    let mut coded = vec![Bit::Zero; segmentation.coded_bits()];
    segmentation
        .encode(
            &joined,
            &mut coded,
            &mut vec![Bit::Zero; segmentation.block_bits()],
        )
        .unwrap();
    let sf = if blocks == 1 { 64 } else { 32 };
    let physical = CHIPS_PER_FRAME as usize / sf;
    let matching =
        RateMatching::uplink_convolutional(coded.len(), physical, Rel99Tti::Ms10, 0).unwrap();
    let mut matched = vec![Bit::Zero; physical];
    matching.apply(&coded, &mut matched).unwrap();
    let mut bits = vec![Bit::Zero; physical];
    Interleaver::second(physical)
        .unwrap()
        .interleave(&matched, &mut bits)
        .unwrap();
    let signature = PreambleSignature::new(13).unwrap();
    let (data, _) = signature.message_codes(sf as u16).unwrap();
    let (_, control) = signature.message_codes(256).unwrap();
    let mut data_code = vec![0.0; sf];
    data.write(&mut data_code).unwrap();
    let mut control_code = [0.0; 256];
    control.write(&mut control_code).unwrap();
    let mut scramble = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    PrachCode::new(7504)
        .unwrap()
        .write_message_scrambling(&mut scramble)
        .unwrap();
    let pilots = [
        "11111110", "10101110", "10111011", "10101010", "11101011", "11111110", "11111010",
        "11101010", "10111110", "11111111", "10111011", "11101111", "11101010", "10101111",
        "10101111",
    ];
    let word = Tfci::new(tfci).unwrap().encode();
    (0..CHIPS_PER_FRAME as usize)
        .map(|chip| {
            let slot = chip / CHIPS_PER_SLOT as usize;
            let symbol = (chip % CHIPS_PER_SLOT as usize) / 256;
            let q_bit = if symbol < 8 {
                pilots[slot].as_bytes()[symbol] == b'1'
            } else {
                word[slot * 2 + symbol - 8] == Bit::One
            };
            let i = if bits[chip / sf] == Bit::Zero {
                1.0
            } else {
                -1.0
            } * data_code[chip % sf];
            let q = if q_bit { -1.0 } else { 1.0 } * control_code[chip % 256] * 0.6;
            Complex32::new(i, q)
                * scramble[chip]
                * Complex32::from_polar(gain, 0.2 + slot as f32 * 0.07)
        })
        .collect()
}

#[test]
fn crc_valid_air_transport_dispatches_mac_tm_and_real_rrc_connection_requests() {
    use crate::common_control::{ControlIngress, RachMacBatch};
    use umts_mac::{CommonHeader, FddCommonChannel, encode_common};
    use umts_rlc::transparent::TmLimits;
    use umts_types::bits::Bits;

    let request = [0x20, 0x90, 0x01, 0x01, 0, 0, 0, 0, 0x01, 0x60];
    for blocks in [1, 2] {
        let mut transport = vec![0u8; blocks * 32];
        for block in transport.chunks_exact_mut(32) {
            encode_common(
                FddCommonChannel::Rach,
                CommonHeader::CommonControl,
                Bits::new(&request, 78).unwrap(),
                block,
            )
            .unwrap();
        }
        let mut payload = vec![Bit::Zero; blocks * 256];
        umts_dsp::bits::unpack_msb(&transport, &mut payload).unwrap();
        let chips = waveform_payload(&payload, (blocks - 1) as u16, 0.05, false);
        let mut tx = source(false);
        let mut messages = pool(&tx, 1);
        messages
            .admit(&mut tx, observation(NOMINAL, 0), at(NOMINAL + 4096), 0)
            .unwrap();
        feed(&mut messages, NOMINAL + MESSAGE_DELAY, &chips, 1);
        let Some(MessageOutcome::Decoded(delivery)) = messages.decode_one().unwrap() else {
            panic!("transport failed CRC")
        };
        let mut packed = vec![0; transport.len()];
        let batch = RachMacBatch::from_message(&delivery, &mut packed).unwrap();
        assert_eq!(batch.pdus().len(), blocks);
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
        control.enqueue(&batch).unwrap();
        let expected = blocks;
        assert_eq!(control.pending(), expected);
        for _ in 0..expected {
            let event = control.decode_one().unwrap();
            assert_eq!(event.transport.start, at(NOMINAL + MESSAGE_DELAY));
            assert_eq!(event.transport.code, delivery.decoded.code);
            assert_eq!(event.transport.signature, delivery.decoded.signature);
            assert_eq!(event.transport.tfci.tfci, delivery.decoded.tfci.tfci);
            assert_eq!(event.message.unwrap().consumed_bits, 78);
        }
        assert!(control.decode_one().is_none());
    }
}

#[test]
fn admission_binds_exact_signalled_profile_epoch_rate_and_explicit_buffer_budget() {
    let tx = source(false);
    assert_eq!(
        AccessMessages::new(&tx, 0, SamplesPerChip::Two, limits(65)).err(),
        Some(MessageError::Capacity)
    );
    assert_eq!(
        AccessMessages::new(
            &tx,
            0,
            SamplesPerChip::Two,
            MessageLimits {
                maximum_buffer_bytes: 1,
                ..limits(1)
            }
        )
        .err(),
        Some(MessageError::Capacity)
    );
    assert_eq!(
        AccessMessages::new(
            &tx,
            0,
            SamplesPerChip::Two,
            MessageLimits {
                maximum_payload_bits: count(256),
                ..limits(1)
            }
        )
        .err(),
        Some(MessageError::Capacity)
    );
    assert_eq!(
        AccessMessages::new(
            &tx,
            0,
            SamplesPerChip::Two,
            MessageLimits {
                maximum_formats: count(1),
                ..limits(1)
            }
        )
        .err(),
        Some(MessageError::Capacity)
    );
    assert_eq!(
        AccessMessages::new(&tx, 1, SamplesPerChip::Two, limits(1)).err(),
        Some(MessageError::Profile)
    );
    assert!(AccessMessages::new(&tx, 0, SamplesPerChip::Four, limits(1)).is_ok());
    let mut messages = pool(&tx, 1);
    let mut other = source(false);
    let mut wrong_code = observation(NOMINAL, 0);
    wrong_code.code = PrachCode::new(7505).unwrap();
    assert_eq!(
        messages
            .admit(&mut other, wrong_code, at(NOMINAL + 4096), 0)
            .err(),
        Some(MessageError::Profile)
    );
    let mut wrong_epoch = observation(NOMINAL, 0);
    wrong_epoch.received_start.epoch = RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap());
    assert_eq!(
        messages
            .admit(&mut other, wrong_epoch, at(NOMINAL + 4096), 0)
            .err(),
        Some(MessageError::Epoch)
    );
    assert_eq!(messages.pending(), 0);
}

#[test]
fn resource_exhaustion_never_publishes_positive_ai_and_failed_tx_rolls_back() {
    let mut tx = source(false);
    let mut messages = pool(&tx, 1);
    let first = observation(NOMINAL, 0);
    let admitted = messages
        .admit(&mut tx, first, at(NOMINAL + 4096), 0)
        .unwrap();
    let second = observation(NOMINAL + ACCESS_CYCLE, 0);
    assert_eq!(
        messages
            .admit(&mut tx, second, at(second.received_start.index + 4096), 0)
            .err(),
        Some(MessageError::Capacity)
    );
    tx.schedule_acquisition(
        0,
        second,
        Acquisition::Negative,
        at(second.received_start.index + 4096),
        0,
    )
    .unwrap();
    messages.cancel(admitted.ticket).unwrap();
    assert!(matches!(
        messages.admit(&mut tx, second, at(second.received_start.index + 4096), 0),
        Err(MessageError::Transmit(_))
    ));
    assert_eq!(messages.pending(), 0);
    let third = observation(NOMINAL + ACCESS_CYCLE * 2, 0);
    messages
        .admit(&mut tx, third, at(third.received_start.index + 4096), 0)
        .unwrap();
    assert_eq!(messages.pending(), 1);
}

#[test]
fn duplicate_observation_keeps_one_ticket_and_retired_access_cannot_be_replayed() {
    let mut tx = source(false);
    let mut messages = pool(&tx, 2);
    let observed = observation(NOMINAL, 17);
    let first = messages
        .admit(&mut tx, observed, at(NOMINAL + 4113), 0)
        .unwrap();
    let duplicate = messages
        .admit(&mut tx, observed, at(NOMINAL + 4113), 0)
        .unwrap();
    assert_eq!(first.ticket, duplicate.ticket);
    assert!(duplicate.window.duplicate);
    assert_eq!(messages.pending(), 1);
    assert_eq!(
        messages
            .admit(&mut tx, observation(NOMINAL, 18), at(NOMINAL + 4114), 0)
            .err(),
        Some(MessageError::Stale)
    );
    messages.cancel(first.ticket).unwrap();
    assert_eq!(
        messages
            .admit(&mut tx, observed, at(NOMINAL + 4113), 0)
            .err(),
        Some(MessageError::Stale)
    );
    assert_eq!(
        messages.cancel(first.ticket),
        Err(MessageError::UnknownTicket)
    );
    assert_eq!(messages.counters().admitted, 1);
    assert_eq!(messages.counters().duplicates, 1);
}

#[test]
fn complete_messages_preserve_arrival_offset_payload_blocks_and_gain_invariance() {
    for compact in [false, true] {
        for (blocks, tfci) in [(1, 0), (2, 1)] {
            for gain in [1e-12, 0.05, 5.0] {
                let mut tx = source(compact);
                let mut messages = pool(&tx, 2);
                let observed = observation(NOMINAL, 17);
                let admitted = messages
                    .admit(&mut tx, observed, at(NOMINAL + 4113), 0)
                    .unwrap();
                assert_eq!(admitted.window.message_start, at(NOMINAL + MESSAGE_DELAY));
                let (chips, payload) = waveform(blocks, tfci, gain, false);
                let mut input = vec![Complex32::default(); 31];
                input.extend(chips);
                input.extend([Complex32::default(); 29]);
                feed(&mut messages, NOMINAL + MESSAGE_DELAY + 17 - 31, &input, 1);
                let Some(MessageOutcome::Decoded(delivery)) = messages.decode_one().unwrap() else {
                    panic!("message was not decoded")
                };
                assert_eq!(delivery.ticket, admitted.ticket);
                assert_eq!(delivery.decoded.start, at(NOMINAL + MESSAGE_DELAY + 17));
                assert_eq!(delivery.decoded.tfci.tfci.index(), tfci);
                assert_eq!(delivery.block_bits, BLOCK_BITS);
                assert_eq!(delivery.blocks, blocks);
                assert_eq!(delivery.payload, payload);
                assert!(messages.decode_one().unwrap().is_none());
                assert_eq!(messages.pending(), 0);
                assert_eq!(messages.counters().decoded, 1);
            }
        }
    }
}

#[test]
fn corrupt_last_transport_block_and_silence_publish_no_partial_payload() {
    let mut tx = source(false);
    let mut messages = pool(&tx, 1);
    for (cycle, corrupt) in [true, false].into_iter().enumerate() {
        let nominal = NOMINAL + cycle as u64 * ACCESS_CYCLE;
        let admitted = messages
            .admit(&mut tx, observation(nominal, 0), at(nominal + 4096), 0)
            .unwrap();
        let (mut chips, _) = waveform(2, 1, 0.05, corrupt);
        if !corrupt {
            chips.fill(Complex32::default());
        }
        if cycle != 0 {
            feed(
                &mut messages,
                nominal + MESSAGE_DELAY - (ACCESS_CYCLE - CHIPS_PER_FRAME),
                &vec![Complex32::default(); (ACCESS_CYCLE - CHIPS_PER_FRAME) as usize],
                1,
            );
        }
        feed(&mut messages, nominal + MESSAGE_DELAY, &chips, 1);
        let Some(MessageOutcome::Rejected(ticket)) = messages.decode_one().unwrap() else {
            panic!("invalid message was published")
        };
        assert_eq!(ticket, admitted.ticket);
    }
    assert_eq!(messages.counters().failed, 2);
    assert_eq!(messages.counters().decoded, 0);
}

#[test]
fn decode_service_is_one_tti_and_complete_evidence_survives_a_later_gap() {
    let mut tx = source(false);
    let mut messages = pool(&tx, 2);
    let one = messages
        .admit(&mut tx, observation(NOMINAL, 0), at(NOMINAL + 4096), 0)
        .unwrap();
    let second_nominal = NOMINAL + ACCESS_CYCLE;
    let two = messages
        .admit(
            &mut tx,
            observation(second_nominal, 0),
            at(second_nominal + 4096),
            0,
        )
        .unwrap();
    let (chips, _) = waveform(1, 0, 0.05, false);
    feed(&mut messages, NOMINAL + MESSAGE_DELAY, &chips, 1);
    let gap = second_nominal - NOMINAL - CHIPS_PER_FRAME;
    feed(
        &mut messages,
        NOMINAL + MESSAGE_DELAY + CHIPS_PER_FRAME,
        &vec![Complex32::default(); gap as usize],
        1,
    );
    feed(&mut messages, second_nominal + MESSAGE_DELAY, &chips, 1);
    assert_eq!(messages.pending(), 2);
    assert_eq!(
        messages.feed(view(
            second_nominal + MESSAGE_DELAY + CHIPS_PER_FRAME,
            &[],
            2,
            true
        )),
        Err(MessageError::Gap)
    );
    assert_eq!(messages.pending(), 2);
    for ticket in [one.ticket, two.ticket] {
        let Some(MessageOutcome::Decoded(delivery)) = messages.decode_one().unwrap() else {
            panic!("ready message lost")
        };
        assert_eq!(delivery.ticket, ticket);
    }
    assert!(messages.decode_one().unwrap().is_none());
}

#[test]
fn gaps_fractional_phase_bad_evidence_and_stale_input_never_stitch_messages() {
    for variant in 0..6 {
        let mut tx = source(false);
        let mut messages = pool(&tx, 1);
        messages
            .admit(&mut tx, observation(NOMINAL, 0), at(NOMINAL + 4096), 0)
            .unwrap();
        let (mut chips, _) = waveform(1, 0, 0.05, false);
        let start = NOMINAL + MESSAGE_DELAY;
        feed(&mut messages, start, &chips[..100], 1);
        if variant == 5 {
            chips[100].re = f32::INFINITY;
        }
        let mut input = view(start + 100, &chips[100..200], 1, false);
        match variant {
            0 => input.generation = 2,
            1 => input.discontinuity = true,
            2 => input.window.first_center.as_mut().unwrap().index += 2,
            3 => input.window.first_center.as_mut().unwrap().index += 1,
            4 => input.window.sample_stride = 4,
            _ => {}
        }
        assert!(messages.feed(input).is_err());
        assert_eq!(messages.pending(), 0);
        assert!(messages.decode_one().unwrap().is_none());
        let next = NOMINAL + ACCESS_CYCLE;
        messages
            .admit(&mut tx, observation(next, 0), at(next + 4096), 0)
            .unwrap();
        let (chips, _) = waveform(1, 0, 0.05, false);
        feed(&mut messages, next + MESSAGE_DELAY, &chips, 3);
        assert!(matches!(
            messages.decode_one().unwrap(),
            Some(MessageOutcome::Decoded(_))
        ));
    }
}

#[test]
fn missed_message_window_expires_instead_of_using_a_later_burst() {
    let mut tx = source(false);
    let mut messages = pool(&tx, 1);
    messages
        .admit(&mut tx, observation(NOMINAL, 0), at(NOMINAL + 4096), 0)
        .unwrap();
    messages
        .feed(view(
            NOMINAL + MESSAGE_DELAY + 1,
            &[Complex32::default(); 30],
            1,
            false,
        ))
        .unwrap();
    assert_eq!(messages.pending(), 0);
    assert_eq!(messages.counters().discarded, 1);
    assert!(messages.decode_one().unwrap().is_none());
}

#[test]
fn foreign_pool_tickets_cannot_release_resources_and_timeouts_reclaim_silent_reservations() {
    let mut tx = source(false);
    let mut one = pool(&tx, 1);
    let mut two = pool(&tx, 1);
    let first = one
        .admit(&mut tx, observation(NOMINAL, 0), at(NOMINAL + 4096), 0)
        .unwrap();
    let second = two
        .admit(
            &mut tx,
            observation(NOMINAL + ACCESS_CYCLE, 0),
            at(NOMINAL + ACCESS_CYCLE + 4096),
            0,
        )
        .unwrap();
    assert_eq!(one.cancel(second.ticket), Err(MessageError::UnknownTicket));
    assert_eq!(two.cancel(first.ticket), Err(MessageError::UnknownTicket));
    let end = NOMINAL + MESSAGE_DELAY + CHIPS_PER_FRAME;
    assert_eq!(one.expire_before(at(end - 1)), Ok(0));
    assert_eq!(one.expire_before(at(end)), Ok(1));
    assert_eq!(one.pending(), 0);
    assert_eq!(two.pending(), 1);
    assert_eq!(one.expire_before(at(end)), Ok(0));
    let nominal = NOMINAL + ACCESS_CYCLE * 2;
    one.admit(&mut tx, observation(nominal, 0), at(nominal + 4096), 0)
        .unwrap();
    let (chips, _) = waveform(1, 0, 0.05, false);
    feed(&mut one, nominal + MESSAGE_DELAY, &chips, 2);
    assert_eq!(
        one.expire_before(at(nominal + MESSAGE_DELAY + CHIPS_PER_FRAME + 1)),
        Ok(0)
    );
    assert!(matches!(
        one.decode_one().unwrap(),
        Some(MessageOutcome::Decoded(_))
    ));
}

#[test]
fn sampled_access_runs_from_memory_hal_preamble_to_reserved_crc_valid_message() {
    use crate::{
        access_rx::PrachWindowReceiver,
        radio_io::{RxSettings, TimedRx, prepare_tx},
    };
    use sdr_hal::{
        Radio,
        memory::{IqBlock, MemoryRadio},
    };
    use umts_dsp::{prach_search::PreambleSearchPlan, pulse_shaping::PulseShaper};

    let delay = 17;
    let (message, payload) = waveform(2, 1, 0.05, false);
    let message_start = NOMINAL + MESSAGE_DELAY + delay;
    let span = 32;
    let mut chips = vec![Complex32::default(); message_start as usize + message.len() + span];
    let observed = observation(NOMINAL, delay);
    observed
        .code
        .write_preamble(
            observed.signature,
            &mut chips[(NOMINAL + delay) as usize..(NOMINAL + delay) as usize + 4096],
        )
        .unwrap();
    chips[message_start as usize..message_start as usize + message.len()].copy_from_slice(&message);
    let mut samples = vec![Complex32::default(); chips.len() * 2];
    PulseShaper::new(SamplesPerChip::Two, span)
        .unwrap()
        .process(&chips, &mut samples)
        .unwrap();
    let sample_count = samples.len() as u64;
    let (mut radio, peer) = MemoryRadio::new(1);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    radio
        .setup_rx(0, "", 7_680_000.0, 5_000_000.0, None)
        .unwrap();
    let (tx, rx) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source(false),
        tx,
        crate::radio_io::TxSettings::offline(1_000_000_000, count(2), count(2), count(2)),
    )
    .unwrap();
    let mut ports = preparation
        .control_ports(count(2), count(1), count(2), count(1))
        .unwrap();
    let mut messages = pool(&ports.acquisition, 2);
    pump.start().unwrap();
    let mut receiver = TimedRx::new(
        rx.unwrap(),
        preparation.air_origin(),
        RxSettings {
            rate: SamplesPerChip::Two,
            span_chips: span,
            phase: 0,
            maximum_samples: count(997),
            timeout_us: 1000,
        },
    )
    .unwrap();
    let plan = Arc::new(PreambleSearchPlan::new(observed.code, 1 << 13, 0..4096, 100).unwrap());
    let mut search = PrachWindowReceiver::new(plan, SamplesPerChip::Two);
    search.arm(at(NOMINAL)).unwrap();
    peer.input
        .send(IqBlock {
            samples,
            time_ticks: Some(1_000_000_000),
            discontinuity: false,
        })
        .unwrap();
    receiver.start(None).unwrap();
    let mut admitted = None;
    let mut reads = 0;
    while receiver.counters().input_samples < sample_count {
        reads += 1;
        assert!(
            reads < 1000,
            "memory receive failed to make bounded progress"
        );
        let Some(input) = receiver.read_filtered().unwrap() else {
            continue;
        };
        messages.feed(input).unwrap();
        if admitted.is_none()
            && let Some(window) = search.feed(input).unwrap()
        {
            let peak = window.strongest(observed.signature).unwrap();
            assert_eq!(peak.correlation_start, observed.received_start);
            assert!(peak.coherent_fraction > 0.9999);
            admitted = Some(
                messages
                    .admit(
                        &mut ports.acquisition,
                        AccessObservation {
                            received_start: peak.correlation_start,
                            ..observed
                        },
                        at(input.window.next_input.index / 2),
                        0,
                    )
                    .unwrap(),
            );
        }
        while preparation.source().next_chip().index < input.window.next_input.index / 2 {
            preparation.prepare_one().unwrap().unwrap();
            assert!(matches!(
                pump.cycle(),
                Ok(crate::radio_io::PumpProgress::Submitted(_))
            ));
            peer.output
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
        }
    }
    let admission = admitted.unwrap();
    assert_eq!(admission.window.indicator_start, at(35_840));
    assert_eq!(admission.window.message_start, at(NOMINAL + MESSAGE_DELAY));
    let response = messages
        .slots
        .iter()
        .find_map(|slot| slot.response.as_ref())
        .unwrap();
    assert_eq!(
        response.transport.plan_stage(),
        Some(crate::common_tx::PlanStage::Committed)
    );
    assert!(response.transport.submitted());
    let Some(MessageOutcome::Decoded(delivery)) = messages.decode_one().unwrap() else {
        panic!("sampled message failed CRC")
    };
    assert_eq!(delivery.ticket, admission.ticket);
    assert_eq!(delivery.decoded.start, at(message_start));
    assert_eq!(delivery.payload, payload);
    assert_eq!(delivery.blocks, 2);
    assert_eq!(receiver.counters().discontinuities, 0);
    receiver.stop().unwrap();
    pump.stop().unwrap();
}

fn access_pipeline(
    capacity: usize,
) -> (
    crate::radio_io::TxPreparation,
    crate::radio_io::TxPump,
    crate::common_tx::ControlPorts,
    sdr_hal::memory::MemoryRadioHandle,
) {
    use sdr_hal::{Radio, memory::MemoryRadio};
    let (mut radio, peer) = MemoryRadio::new(2);
    radio.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(radio).split().unwrap();
    let (mut preparation, mut pump) = crate::radio_io::prepare_tx(
        source(false),
        tx,
        crate::radio_io::TxSettings::offline(0, count(1), count(1), count(2)),
    )
    .unwrap();
    let ports = preparation
        .control_ports(count(2), count(1), count(capacity), count(1))
        .unwrap();
    pump.start().unwrap();
    (preparation, pump, ports, peer)
}

fn advance_access(
    preparation: &mut crate::radio_io::TxPreparation,
    pump: &mut crate::radio_io::TxPump,
    peer: &sdr_hal::memory::MemoryRadioHandle,
    count: usize,
) {
    for _ in 0..count {
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(
            pump.cycle(),
            Ok(crate::radio_io::PumpProgress::Submitted(_))
        ));
        peer.output
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
    }
}

#[test]
fn access_worker_rejection_releases_only_failed_receive_reservation_and_keeps_replay_floor() {
    let (mut preparation, mut pump, mut ports, peer) = access_pipeline(3);
    let mut messages = pool(&ports.acquisition, 1);
    let mut peer_messages = pool(&ports.acquisition, 1);
    advance_access(&mut preparation, &mut pump, &peer, 13);
    let nominal = NOMINAL + ACCESS_CYCLE;
    peer_messages
        .admit(
            &mut ports.acquisition,
            observation(nominal, 0),
            at(nominal + 4096),
            0,
        )
        .unwrap();
    let nominal = NOMINAL + 2 * ACCESS_CYCLE;
    let _ahead = ports
        .acquisition
        .publish_acquisition(
            0,
            observation(nominal, 0),
            Acquisition::Positive,
            at(nominal + 4096),
            0,
        )
        .unwrap();
    messages
        .admit(
            &mut ports.acquisition,
            observation(NOMINAL, 0),
            at(NOMINAL + 4096),
            0,
        )
        .unwrap();
    assert_eq!(messages.pending(), 1);
    advance_access(&mut preparation, &mut pump, &peer, 3);
    assert_eq!(messages.reconcile_transmit(), 1);
    assert_eq!(messages.pending(), 0);
    assert_eq!(peer_messages.reconcile_transmit(), 0);
    assert_eq!(peer_messages.pending(), 1);
    assert_eq!(messages.counters().transmit_rejected, 1);
    assert_eq!(messages.counters().discarded, 1);
    assert_eq!(
        messages
            .admit(
                &mut ports.acquisition,
                observation(NOMINAL, 0),
                at(NOMINAL + 4096),
                0
            )
            .err(),
        Some(MessageError::Stale)
    );
}

#[test]
fn access_receive_cancellation_and_sample_loss_cancel_uncommitted_positive_response() {
    use crate::common_tx::PlanStage;
    for gap in [false, true] {
        let (mut preparation, mut pump, mut ports, peer) = access_pipeline(2);
        let mut messages = pool(&ports.acquisition, 2);
        let first = messages
            .admit(
                &mut ports.acquisition,
                observation(NOMINAL, 0),
                at(NOMINAL + 4096),
                0,
            )
            .unwrap();
        let proof = messages.slots[0]
            .response
            .as_ref()
            .unwrap()
            .transport
            .clone();
        advance_access(&mut preparation, &mut pump, &peer, 1);
        assert_eq!(proof.plan_stage(), Some(PlanStage::Scheduled));
        if gap {
            messages
                .feed(view(0, &[Complex32::default(); 16], 1, false))
                .unwrap();
            assert_eq!(
                messages.feed(view(16, &[Complex32::default(); 16], 2, true)),
                Err(MessageError::Gap)
            );
        } else {
            messages.cancel(first.ticket).unwrap();
        }
        assert_eq!(messages.pending(), 0);
        assert_eq!(proof.plan_stage(), Some(PlanStage::Cancelled));
        advance_access(&mut preparation, &mut pump, &peer, 16);
        assert!(!proof.submitted());
    }
}

#[test]
fn closing_control_endpoint_reclaims_access_receive_resources_without_waiting_for_source_drop() {
    let (_preparation, _pump, mut ports, _peer) = access_pipeline(2);
    let mut messages = pool(&ports.acquisition, 2);
    messages
        .admit(
            &mut ports.acquisition,
            observation(NOMINAL, 0),
            at(NOMINAL + 4096),
            0,
        )
        .unwrap();
    drop(ports.transport);
    assert_eq!(messages.reconcile_transmit(), 1);
    assert_eq!(messages.pending(), 0);
    assert_eq!(messages.counters().transmit_rejected, 1);
}
