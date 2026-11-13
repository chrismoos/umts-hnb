// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_rx::FrameAnchor,
    dedicated_tx::{DedicatedTransmitRuntime, TransmitRuntimeLimits},
    hsupa_rx::{DecodedTti, Publication, Received, delivery::Delivery},
    test_support::{at, count, source},
};
use std::num::NonZeroU64;
use umts_dsp::{
    bits::{Bit, unpack_msb},
    dedicated::DpchTiming,
    hsupa::{ControlDecision, ControlInformation},
    transport_blocks::BlockOutcome,
};
use umts_mac::hsupa::{Reception, payload::Mapping};
use umts_types::{
    bits::write_u32,
    hsupa::{Etfci, Rsn, Tti},
    time::CHIPS_PER_SLOT,
};

#[test]
fn unhappy_streak_requires_adjacent_ttis_and_resets_on_happy_or_gap() {
    for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
        let mut counters = IngressCounters::default();
        let chips = interval.chips();
        counters.observe_happy(at(0), false, chips);
        counters.observe_happy(at(chips), false, chips);
        assert_eq!(counters.unhappy_streak, 2);
        counters.observe_happy(at(3 * chips), false, chips);
        assert_eq!(counters.unhappy_streak, 1);
        counters.observe_happy(at(4 * chips), true, chips);
        assert_eq!(counters.unhappy_streak, 0);
        counters.observe_happy(at(5 * chips), false, chips);
        assert_eq!(counters.unhappy_streak, 1);
        counters.observe_happy(at(5 * chips), false, chips);
        assert_eq!(counters.unhappy_streak, 1);
    }
}

fn packet(tsn: u32) -> Vec<Bit> {
    let fields = [(1, 6), (1, 6), (tsn, 6), (0xab, 8)];
    let bits = fields.iter().map(|(_, width)| width).sum::<usize>();
    let mut packed = vec![0; bits.div_ceil(8)];
    let mut offset = 0;
    for (value, width) in fields {
        write_u32(&mut packed, offset, width, value).unwrap();
        offset += width;
    }
    let mut output = vec![Bit::Zero; bits];
    unpack_msb(&packed, &mut output).unwrap();
    output
}

fn ingress(identity: ReceiveIdentity) -> (Ingress, Delivery) {
    let (delivery, receive) = Delivery::new(count(3), count(256), count(1_000_000)).unwrap();
    let packets = Box::new(
        PacketDelivery::new(
            identity,
            &[(
                1,
                Mapping {
                    logical_channel: 1,
                    flow: 0,
                    pdu_bits: count(8),
                },
            )],
            count(256),
            count(1),
            NonZeroU64::new(10).unwrap(),
            0,
            count(1_000_000),
        )
        .unwrap(),
    );
    let association = FeedbackAssociation::new(
        identity,
        FrameAnchor {
            time: at(0),
            cfn: 0,
        },
        at(0),
        Tti::TwoMilliseconds,
        DpchTiming::new(0).unwrap(),
    )
    .unwrap();
    let ingress = Ingress::new(
        IngressSettings {
            identity,
            activation: at(0),
            feedback_amplitude: 0.1,
            feedback_capacity: count(4),
        },
        IngressInputs {
            receive,
            packets,
            association,
        },
    )
    .unwrap_or_else(|_| panic!("ingress admission"));
    (ingress, delivery)
}

fn publish(delivery: &mut Delivery, identity: ReceiveIdentity, tti: u64, payload: &[Bit]) {
    assert_eq!(delivery.reserve(), Publication::Pending);
    delivery.publish(DecodedTti {
        timing: None,
        identity,
        start: at(tti * Tti::TwoMilliseconds.chips()),
        absolute_tti: tti,
        control: ControlDecision {
            information: ControlInformation {
                format: Etfci::new(1).unwrap(),
                sequence: Rsn::new(0).unwrap(),
                happy: true,
            },
            normalized_score: 1.0,
            normalized_margin: Some(0.5),
        },
        received: Received {
            feedback: Reception::Ack { deliver: true },
            outcome: BlockOutcome::Decoded,
            combined: false,
            reset_reason: None,
            payload: Some(payload),
        },
    });
}

#[test]
fn feedback_history_separates_process_reuse_from_stale_completions() {
    let identity = ReceiveIdentity {
        ue: umts_types::UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        },
        configuration: NonZeroU64::MIN,
    };
    let (mut ingress, mut delivery) = ingress(identity);
    publish(&mut delivery, identity, 0, &packet(0));
    let first = ingress.inputs.receive.peek().unwrap().report;
    for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
        let mut history = FeedbackHistory::new(interval);
        let cycle = u64::from(interval.processes());
        history.observe(first, AckDisposition::Queued);
        history.disposition(0, AckDisposition::Submitted);
        let mut retry = first;
        retry.absolute_tti = cycle;
        retry.start = at(cycle * interval.chips());
        retry.control.information.sequence = Rsn::new(1).unwrap();
        history.observe(retry, AckDisposition::Late);
        assert_eq!(history.counters().retry_after_submitted_ack, 1);
        history.disposition(0, AckDisposition::Submitted);
        retry.absolute_tti += cycle;
        retry.start.index += cycle * interval.chips();
        retry.control.information.sequence = Rsn::new(2).unwrap();
        history.observe(retry, AckDisposition::Queued);
        assert_eq!(history.counters().retry_after_late_ack, 1);
        assert_eq!(history.counters().retry_after_submitted_ack, 1);
        history.disposition(retry.absolute_tti, AckDisposition::Submitted);
        retry.absolute_tti += cycle;
        retry.start.index += cycle * interval.chips();
        retry.control.information.sequence = Rsn::new(0).unwrap();
        history.observe(retry, AckDisposition::Queued);
        assert_eq!(history.counters().new_after_submitted_ack, 1);
        retry.absolute_tti += 2 * cycle;
        retry.start.index += 2 * cycle * interval.chips();
        retry.control.information.sequence = Rsn::new(1).unwrap();
        history.observe(retry, AckDisposition::Queued);
        assert_eq!(history.counters().observed_process_reuses, 3);
        assert_eq!(history.counters().retry_before_completion, 0);
    }
}

#[test]
fn report_retry_does_not_duplicate_feedback_and_only_air_receipts_count_as_submission() {
    for lost in [false, true] {
        let mut source = source(true);
        let mut writer = source.submission_writer().unwrap();
        let (_, bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
        let identity = identities[0];
        let (mut ingress, mut delivery) = ingress(identity);
        let (mut runtime, mut ports) = DedicatedTransmitRuntime::new(
            &source,
            count(2),
            count(8),
            count(1_000_000),
            TransmitRuntimeLimits {
                command_capacity: count(4),
                commands_per_step: count(2),
                feedback_capacity: count(4),
                feedback_per_step: count(2),
                maximum_frame_payload_bits: count(100_000),
                maximum_inflight_retained_bits: count(8_000_000),
            },
        )
        .unwrap();
        assert!(runtime.attach_hsupa(bank).is_ok());
        let first = packet(0);
        let second = packet(1);
        publish(&mut delivery, identity, 0, &first);
        publish(&mut delivery, identity, 1, &second);
        let now = at(2 * Tti::TwoMilliseconds.chips());
        assert_eq!(
            ingress
                .service_one(now, at(0), &mut ports.control)
                .unwrap()
                .progress,
            IngressProgress::Admitted
        );
        for _ in 0..3 {
            assert_eq!(
                ingress
                    .service_one(now, at(0), &mut ports.control)
                    .unwrap()
                    .progress,
                IngressProgress::Backpressure
            );
        }
        assert_eq!(ingress.counters().feedback_published, 2);
        assert_eq!(delivery.outstanding(), 1);
        assert_eq!(ingress.counters().decoded, 1);
        assert_eq!(ingress.counters().stored_groups, 1);
        assert_eq!(
            ingress
                .packets()
                .peek(now.index)
                .unwrap()
                .unwrap()
                .bits
                .len(),
            8
        );
        assert!(ingress.packets().release());
        assert_eq!(
            ingress
                .service_one(now, at(0), &mut ports.control)
                .unwrap()
                .progress,
            IngressProgress::Admitted
        );
        assert_eq!(delivery.outstanding(), 0);
        assert_eq!(ingress.counters().feedback_published, 2);
        assert_eq!(ingress.counters().decoded, 2);
        assert_eq!(ingress.counters().decoded_by_rsn, [2, 0, 0, 0]);
        assert_eq!(ingress.counters().failed_by_rsn, [0; RSN_STATES]);
        assert_eq!(ingress.counters().last_decoded.unwrap().absolute_tti, 1);
        assert!(ingress.counters().last_failed.is_none());
        assert_eq!(ingress.counters().stored_groups, 2);
        assert_eq!(
            ingress.counters().decoded_payload_bits,
            (first.len() + second.len()) as u64
        );
        runtime.service().unwrap();
        for _ in 0..2 {
            assert!(
                ingress
                    .route_command(ports.control.try_event().unwrap())
                    .is_none()
            );
        }
        assert_eq!(ingress.counters().feedback_admitted, 2);
        assert_eq!(ingress.counters().feedback_submitted, 0);
        assert_eq!(ingress.outstanding_feedback(), 2);
        let mut samples = vec![num_complex::Complex32::default(); source.samples_per_slot()];
        for _ in 0..24 {
            let window = runtime.generate_slot(&mut source, &mut samples).unwrap();
            assert!(if lost {
                writer.lost(window)
            } else {
                writer.accepted(window)
            });
        }
        for _ in 0..2 {
            assert!(ports.control.queue_hsupa(Command::TakeCompletion).is_ok());
            runtime.service().unwrap();
            let TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(Ok(Some(completion)))) =
                ports.control.try_event().unwrap().outcome
            else {
                panic!("control completion")
            };
            ingress.accept_completion(completion).unwrap();
            assert!(ingress.accept_completion(completion).is_err());
        }
        assert_eq!(ingress.outstanding_feedback(), 0);
        assert_eq!(
            ingress.counters().feedback_submitted,
            if lost { 0 } else { 2 }
        );
        assert_eq!(ingress.counters().feedback_lost, if lost { 2 } else { 0 });
    }
}

#[test]
fn late_feedback_does_not_discard_a_decoded_packet_or_publish_stale_ack() {
    let source = source(true);
    let (_, _bank, _radios, identities) = crate::hsupa_tx::tests::setup(&source);
    let identity = identities[0];
    let (mut ingress, mut delivery) = ingress(identity);
    publish(&mut delivery, identity, 0, &packet(0));
    let (_runtime, mut ports) = DedicatedTransmitRuntime::new(
        &source,
        count(2),
        count(8),
        count(1_000_000),
        TransmitRuntimeLimits {
            command_capacity: count(4),
            commands_per_step: count(2),
            feedback_capacity: count(4),
            feedback_per_step: count(2),
            maximum_frame_payload_bits: count(100_000),
            maximum_inflight_retained_bits: count(8_000_000),
        },
    )
    .unwrap();
    let now = at(30 * CHIPS_PER_SLOT);
    let step = ingress.service_one(now, now, &mut ports.control).unwrap();
    assert_eq!(step.progress, IngressProgress::Admitted);
    assert_eq!(step.packet.unwrap().stored, 1);
    assert_eq!(ingress.counters().feedback_late, 1);
    assert_eq!(ports.control.outstanding(), 0);
}
