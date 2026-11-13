// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::at;
use num_complex::Complex32;
use umts_dsp::{
    bits::Bit,
    carrier::CarrierOffsetHz,
    channel_codes::OvsfCode,
    dedicated::{CarrierTrackingSettings, UplinkControlFormat, UplinkDpchReceiver},
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SECOND};

struct Fixture {
    dpch: UplinkDpchReceiver,
    feedback: FeedbackReceiver,
    scrambling: Vec<Complex32>,
    spreading: [f32; SYMBOL_CHIPS as usize],
    offset_symbols: usize,
    gain: f32,
    carrier_hz: f64,
}

#[test]
fn ambiguous_order_accepts_common_ack_but_data_remains_unresolved() {
    let mut fixture = Fixture::new(1, 1.0);
    for slot in 0..3 {
        fixture.feed(slot, true);
    }
    let start = at(SYMBOL_CHIPS);
    let schedule = FeedbackModeSchedule::fixed(start, FeedbackMode::Ambiguous);
    let Readout::Observed(order) = fixture.feedback.ack_order(start, schedule).unwrap() else {
        panic!("complete order feedback window");
    };
    let Readout::Observed(data) = fixture.feedback.ack_cells(start, 1, schedule).unwrap() else {
        panic!("complete data feedback window");
    };
    let limits = EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    let policy = FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    };
    assert_eq!(
        policy.classify_ack_cells(&order),
        [
            umts_mac::hsdpa::Feedback::Ack,
            umts_mac::hsdpa::Feedback::Dtx
        ]
    );
    assert_eq!(
        policy.classify_ack_cells(&data),
        [umts_mac::hsdpa::Feedback::Dtx; 2]
    );
}

#[test]
fn interpolation_coherence_rejects_phase_cancellation_not_amplitude_changes() {
    let pilot = |center, channel| Pilot {
        center,
        channel,
        coherent_fraction: 1.0,
        relative_disturbance: 0.0,
    };
    for scale in [0.001, 1.0, 1000.0] {
        let first = pilot(0, Complex64::new(2.0 * scale, 0.0));
        let aligned = pilot(CHIPS_PER_SLOT, Complex64::new(scale, 0.0));
        let opposed = pilot(CHIPS_PER_SLOT, Complex64::new(-scale, 0.0));
        for center in [0, CHIPS_PER_SLOT / 2, CHIPS_PER_SLOT] {
            let (channel, coherence) = interpolate(first, aligned, center);
            assert!(channel.re > 0.0);
            assert!((coherence - 1.0).abs() < 1e-12);
        }
        let (_, coherence) = interpolate(first, opposed, CHIPS_PER_SLOT / 2);
        assert!((coherence - 1.0 / 9.0).abs() < 1e-12);
        assert!(coherence < 0.5);
        let vanished = pilot(0, Complex64::default());
        let (_, coherence) = interpolate(vanished, pilot(CHIPS_PER_SLOT, Complex64::default()), 0);
        assert_eq!(coherence, 0.0);
    }
}

#[test]
fn coherent_ack_survives_a_gain_change_between_pilot_slots() {
    let mut fixture = Fixture::new(1, 2.0);
    fixture.feed(0, true);
    fixture.gain = 1.0;
    fixture.feed(1, true);
    fixture.feed(2, true);
    let Readout::Observed(observation) = fixture.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap() else {
        panic!("complete feedback window");
    };
    let limits = EvidenceLimits::new(0.5, 0.1, 0.5, 1.0, 0.01).unwrap();
    let policy = FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    };
    assert!(observation.minimum_pilot_fraction > 0.99);
    assert_eq!(
        policy.classify_ack_with_reason(&observation),
        (umts_mac::hsdpa::Feedback::Ack, None)
    );
}

impl Fixture {
    fn new(offset_symbols: usize, gain: f32) -> Self {
        let code = UplinkScramblingCode::new(0x123456).unwrap();
        let shared = Arc::new(UplinkDpchCode::new(code).unwrap());
        let carrier_hz = 900.0;
        let dpch = UplinkDpchReceiver::with_tracking(
            shared.clone(),
            UplinkControlFormat::new(0).unwrap(),
            CarrierTrackingSettings {
                initial: CarrierOffsetHz::new(carrier_hz).unwrap(),
                maximum_absolute_hz: 2000.0,
                update_fraction: 0.1,
                minimum_coherence: 0.8,
            },
        )
        .unwrap();
        let feedback = FeedbackReceiver::new(
            UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: std::num::NonZeroU64::MIN,
            },
            shared,
            1,
            at(0),
        )
        .unwrap();
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        UplinkLongSequence::new(code, 0)
            .unwrap()
            .write(&mut scrambling);
        let mut spreading = [0.0; SYMBOL_CHIPS as usize];
        OvsfCode::new(256, 64)
            .unwrap()
            .write(&mut spreading)
            .unwrap();
        Self {
            dpch,
            feedback,
            scrambling,
            spreading,
            offset_symbols,
            gain,
            carrier_hz,
        }
    }

    fn feed(&mut self, slot: usize, present: bool) {
        let start = slot as u64 * CHIPS_PER_SLOT;
        if start.is_multiple_of(CHIPS_PER_FRAME) {
            self.dpch.begin_frame(at(start)).unwrap();
        }
        let format = self.dpch.format();
        let pilots = format.pilots().word((slot % 15) as u8).unwrap();
        let ack = HarqAck::Ack.encode();
        let cqi = Cqi::new(23).unwrap().encode();
        let sign = |bit| if bit == Bit::Zero { 1.0 } else { -1.0 };
        let input: Vec<_> = (0..CHIPS_PER_SLOT as usize)
            .map(|index| {
                let absolute = start as usize + index;
                let symbol = index / SYMBOL_CHIPS as usize;
                let pilot = if symbol < format.pilots().bits() {
                    if pilots & (1 << (format.pilots().bits() - 1 - symbol)) == 0 {
                        1.0
                    } else {
                        -1.0
                    }
                } else {
                    1.0
                };
                let hs = if present {
                    (absolute / SYMBOL_CHIPS as usize)
                        .checked_sub(self.offset_symbols)
                        .map(|symbol| {
                            let position = symbol % SUBFRAME_SYMBOLS;
                            sign(if position < SLOT_SYMBOLS {
                                ack[position]
                            } else {
                                cqi[position - SLOT_SYMBOLS]
                            })
                        })
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                let value = Complex32::new(
                    0.0,
                    pilot + 0.5 * hs * self.spreading[index % SYMBOL_CHIPS as usize],
                );
                let phase = 0.37
                    + std::f64::consts::TAU * self.carrier_hz * absolute as f64
                        / CHIPS_PER_SECOND as f64;
                value
                    * self.scrambling[absolute % self.scrambling.len()]
                    * Complex32::from_polar(self.gain, phase as f32)
            })
            .collect();
        self.dpch.receive(at(start), &input).unwrap();
        self.feedback.feed(&self.dpch.last_slot().unwrap()).unwrap();
    }
}

#[test]
fn feedback_code_transition_preserves_preceding_symbols_and_delivery_owner() {
    use std::num::NonZeroUsize;

    let mut fixture = Fixture::new(0, 1.0);
    let mut port = fixture
        .feedback
        .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
        .unwrap();
    let preceding = at(CHIPS_PER_FRAME - 3 * CHIPS_PER_SLOT);
    let request = port
        .request(Request::Cqi {
            start: preceding,
            repetitions: 1,
        })
        .unwrap();
    for slot in 0..15 {
        fixture.feed(slot, true);
    }
    assert!(port.try_report().is_none());
    let mut replacement = FeedbackChannel::new(fixture.dpch.code().clone(), 0, at(0)).unwrap();
    assert!(
        fixture
            .feedback
            .replace_channel_at(&mut replacement, at(CHIPS_PER_FRAME - 1))
            .is_err()
    );
    fixture
        .feedback
        .replace_channel_at(&mut replacement, at(CHIPS_PER_FRAME))
        .unwrap();
    OvsfCode::new(256, 33)
        .unwrap()
        .write(&mut fixture.spreading)
        .unwrap();
    for slot in 15..19 {
        fixture.feed(slot, true);
    }
    let report = port.try_report().unwrap();
    assert_eq!(report.id, request);
    let Ok(Measurement::Cqi(Evidence::Measured(observation))) = report.result else {
        panic!("preceding CQI lost during code transition");
    };
    assert_eq!(observation.evidence.candidate, Some(Cqi::new(23).unwrap()));
    let Readout::Observed(observation) = fixture.feedback.ack(at(CHIPS_PER_FRAME), 1).unwrap()
    else {
        panic!("successor ACK missing");
    };
    assert_eq!(observation.evidence.candidate, Some(HarqAck::Ack));
    let Readout::Observed(observation) = fixture.feedback.cqi(at(CHIPS_PER_FRAME), 1).unwrap()
    else {
        panic!("successor CQI missing");
    };
    assert_eq!(observation.evidence.candidate, Some(Cqi::new(23).unwrap()));
    assert!(
        fixture
            .feedback
            .replace_channel_at(&mut replacement, at(CHIPS_PER_FRAME))
            .is_err()
    );
}

#[test]
fn scheduled_cqi_request_selects_the_final_two_slots_exactly_once() {
    use crate::{dedicated_rx::FrameAnchor, hsdpa_session::FeedbackTiming};
    use std::num::NonZeroUsize;
    use umts_rrc::hsdpa::FeedbackCycle;

    let anchor = FrameAnchor {
        time: at(0),
        cfn: 0,
    };
    let timing = FeedbackTiming::new(anchor, anchor, at(5120)).unwrap();
    let start = timing
        .cqi_subframe(at(5120), FeedbackCycle::Ms2)
        .unwrap()
        .unwrap();
    assert_eq!(start, timing.acknowledgement(at(5120)).unwrap());
    for gain in [0.001, 1.0, 1000.0] {
        let mut fixture = Fixture::new((start.index / SYMBOL_CHIPS) as usize, gain);
        let mut port = fixture
            .feedback
            .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
            .unwrap();
        let id = port
            .request(Request::Cqi {
                start,
                repetitions: 1,
            })
            .unwrap();
        let end = start.index + 3 * CHIPS_PER_SLOT;
        for slot in 0..=end.div_ceil(CHIPS_PER_SLOT) as usize {
            fixture.feed(slot, true);
        }
        let report = port.try_report().expect("scheduled CQI report");
        assert_eq!(report.id, id);
        let Ok(Measurement::Cqi(Evidence::Measured(observation))) = report.result else {
            panic!("missing scheduled CQI evidence");
        };
        assert_eq!(observation.evidence.candidate, Some(Cqi::new(23).unwrap()));
        assert!(observation.evidence.normalized_correlation > 0.999);
        assert_eq!(port.outstanding(), 0);
    }
}

#[test]
fn shared_tracked_samples_decode_all_symbol_phases_and_repetitions_across_frames() {
    for gain in [0.001, 1.0, 1000.0] {
        for offset in [0, 1, 9, 149] {
            for repetitions in 1..=MAX_REPETITIONS {
                let mut fixture = Fixture::new(offset, gain);
                let start = at(offset as u64 * SYMBOL_CHIPS);
                let end_symbols = offset + usize::from(repetitions) * SUBFRAME_SYMBOLS;
                for slot in 0..=end_symbols.div_ceil(SLOT_SYMBOLS) {
                    fixture.feed(slot, true);
                }
                let Readout::Observed(ack) = fixture.feedback.ack(start, repetitions).unwrap()
                else {
                    panic!("missing ACK evidence");
                };
                let Readout::Observed(cqi) = fixture.feedback.cqi(start, repetitions).unwrap()
                else {
                    panic!("missing CQI evidence");
                };
                assert_eq!(ack.evidence.candidate, Some(HarqAck::Ack));
                assert_eq!(cqi.evidence.candidate.unwrap().value(), 23);
                assert!(ack.evidence.normalized_correlation > 0.999);
                assert!(cqi.evidence.normalized_correlation > 0.999);
                assert!(ack.minimum_pilot_fraction > 0.999);
            }
        }
    }
}

#[test]
fn acknowledgement_is_available_before_frame_end_and_missing_history_is_not_dtx() {
    let mut fixture = Fixture::new(1, 1.0);
    fixture.feed(0, true);
    fixture.feed(1, true);
    assert!(matches!(
        fixture.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap(),
        Readout::Observed(_)
    ));
    fixture.feed(2, true);
    assert!(matches!(
        fixture.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap(),
        Readout::Observed(_)
    ));
    assert!(fixture.feedback.processed_until().unwrap().index < CHIPS_PER_FRAME);
    fixture.feed(15, true);
    assert!(matches!(
        fixture.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap(),
        Readout::Missing
    ));
    assert!(fixture.feedback.ack(at(1), 1).is_err());
    assert!(fixture.feedback.ack(at(SYMBOL_CHIPS), 0).is_err());
    assert!(fixture.feedback.ack(at(SYMBOL_CHIPS), 5).is_err());
}

#[test]
fn ready_prefix_preserves_delayed_interpolation_evidence() {
    for gain in [0.001, 1.0, 1000.0] {
        let mut early = Fixture::new(1, gain);
        let mut delayed = Fixture::new(1, gain);
        for slot in 0..2 {
            early.feed(slot, true);
            delayed.feed(slot, true);
            delayed.feedback.pending.as_mut().unwrap().resolved_prefix = 0;
            delayed.feedback.processed_until = Some(at(slot as u64 * CHIPS_PER_SLOT));
        }
        let Readout::Observed(before) = early.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap() else {
            panic!("ready prefix should complete ACK");
        };
        assert!(matches!(
            delayed.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap(),
            Readout::Pending
        ));
        delayed.feed(2, true);
        let Readout::Observed(reference) = delayed.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap()
        else {
            panic!("following pilot should complete delayed ACK");
        };
        assert_eq!(before.evidence.candidate, reference.evidence.candidate);
        assert_eq!(
            before.evidence.normalized_correlation,
            reference.evidence.normalized_correlation
        );
        assert_eq!(
            before.evidence.normalized_margin,
            reference.evidence.normalized_margin
        );
        assert_eq!(
            before.evidence.mean_soft_power,
            reference.evidence.mean_soft_power
        );
        assert_eq!(
            before.minimum_pilot_fraction,
            reference.minimum_pilot_fraction
        );
        assert_eq!(
            before.mean_relative_pilot_disturbance,
            reference.mean_relative_pilot_disturbance
        );
        early.feed(2, true);
        for (actual, expected) in early.feedback.symbols.iter().zip(&delayed.feedback.symbols) {
            if let (Some(actual), Some(expected)) = (actual, expected) {
                assert_eq!(actual.start, expected.start);
                assert_eq!(actual.soft, expected.soft);
                assert_eq!(actual.coherent_fraction, expected.coherent_fraction);
                assert_eq!(actual.relative_disturbance, expected.relative_disturbance);
            }
        }
    }
}

#[test]
fn no_hs_signal_has_negligible_power_even_with_a_coherent_dpcch() {
    let mut fixture = Fixture::new(1, 1000.0);
    for slot in 0..5 {
        fixture.feed(slot, false);
    }
    let Readout::Observed(observation) = fixture.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap() else {
        panic!("expected measured absence");
    };
    assert!(observation.evidence.mean_soft_power < 1e-10);
    assert!(observation.minimum_pilot_fraction > 0.99);
}

#[test]
fn discarded_frames_resolve_missing_feedback_without_waiting_for_reacquisition() {
    use std::num::NonZeroUsize;
    let mut fixture = Fixture::new(1, 1.0);
    let mut port = fixture
        .feedback
        .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
        .unwrap();
    fixture.feed(0, true);
    fixture.feed(1, true);
    fixture.feed(2, true);
    let request = port
        .request(Request::Cqi {
            start: at(CHIPS_PER_FRAME),
            repetitions: 1,
        })
        .unwrap();
    fixture.feedback.service_requests();
    assert!(port.try_report().is_none());
    fixture
        .feedback
        .discard_before(at(2 * CHIPS_PER_FRAME))
        .unwrap();
    let report = port.try_report().expect("discarded interval is terminal");
    assert_eq!(report.id, request);
    assert!(matches!(
        report.result,
        Ok(Measurement::Cqi(Evidence::Missing))
    ));
    assert_eq!(report.processed_until, Some(at(2 * CHIPS_PER_FRAME)));
    assert!(matches!(
        fixture.feedback.ack(at(SYMBOL_CHIPS), 1).unwrap(),
        Readout::Observed(_)
    ));
    assert!(
        fixture
            .feedback
            .discard_before(at(CHIPS_PER_FRAME))
            .is_err()
    );
    assert!(
        fixture
            .feedback
            .discard_before(at(2 * CHIPS_PER_FRAME + 1))
            .is_err()
    );
    fixture.feed(30, true);
    fixture.feed(31, true);
    fixture.feed(32, true);
    assert!(matches!(
        fixture
            .feedback
            .ack(at(2 * CHIPS_PER_FRAME + SYMBOL_CHIPS), 1)
            .unwrap(),
        Readout::Observed(_)
    ));
}

#[test]
fn transmission_readout_retains_identity_and_rejects_another_ue() {
    use std::num::NonZeroU64;
    use umts_mac::hsdpa::{HarqProcess, ProcessId};
    let mut fixture = Fixture::new(1, 1.0);
    for slot in 0..3 {
        fixture.feed(slot, true);
    }
    let mut owner = HarqProcess::new(
        fixture.feedback.ue,
        NonZeroU64::MIN,
        ProcessId::new(2).unwrap(),
    );
    owner.replace(());
    let scheduled = owner.schedule(at(0), at(SYMBOL_CHIPS)).unwrap();
    let readout = fixture.feedback.observe(scheduled, 1).unwrap();
    assert_eq!(readout.key, scheduled.key);
    assert_eq!(readout.expected_ack, scheduled.expected_ack);
    assert!(matches!(readout.acknowledgement, Readout::Observed(_)));
    let mut peer = HarqProcess::new(
        UeKey {
            slot: 1,
            ..fixture.feedback.ue
        },
        NonZeroU64::MIN,
        ProcessId::new(2).unwrap(),
    );
    peer.replace(());
    assert!(
        fixture
            .feedback
            .observe(peer.schedule(at(0), at(SYMBOL_CHIPS)).unwrap(), 1)
            .is_err()
    );
}

#[test]
fn delivery_preserves_request_custody_and_reuses_capacity_after_report() {
    use std::num::{NonZeroU64, NonZeroUsize};
    use umts_mac::hsdpa::{HarqProcess, ProcessId};
    let mut fixture = Fixture::new(1, 1.0);
    let mut port = fixture
        .feedback
        .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
        .unwrap();
    let mut harq = HarqProcess::new(
        fixture.feedback.ue,
        NonZeroU64::MIN,
        ProcessId::new(0).unwrap(),
    );
    harq.replace(());
    let transmission = harq.schedule(at(0), at(SYMBOL_CHIPS)).unwrap();
    let request = Request::Ack {
        transmission,
        repetitions: 1,
    };
    let id = port.request(request).unwrap();
    assert!(port.request(request).is_err());
    fixture.feedback.service_requests();
    assert!(port.try_report().is_none());
    for slot in 0..3 {
        fixture.feed(slot, true);
    }
    let report = port.try_report().unwrap();
    assert_eq!(report.id, id);
    assert!(matches!(
        report.result,
        Ok(Measurement::Ack(Evidence::Measured(_)))
    ));
    assert_eq!(port.outstanding(), 0);
    let next = port.request(request).unwrap();
    assert_ne!(next, id);
    fixture.feedback.service_requests();
    assert_eq!(port.try_report().unwrap().id, next);
}

#[test]
fn decoded_repeated_feedback_reaches_exact_harq_flight_before_tx_completion() {
    use crate::{
        hsdpa_packet::{Flight, Resolution},
        test_support::count,
    };
    use std::num::{NonZeroU8, NonZeroU64, NonZeroUsize};
    use umts_mac::hsdpa::{Feedback, HarqEntity, ProcessId};
    use umts_types::hsdpa::Category;

    for gain in [0.001, 1.0, 1000.0] {
        for repetitions in 1..=MAX_REPETITIONS {
            let mut fixture = Fixture::new(1, gain);
            let mut port = fixture
                .feedback
                .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
                .unwrap();
            let mut harq = HarqEntity::new(
                fixture.feedback.ue,
                NonZeroU64::MIN,
                Category::new(10).unwrap(),
                at(0),
                &[count(28_800); 6],
                NonZeroU8::new(repetitions).unwrap(),
            )
            .unwrap();
            let process = ProcessId::new(0).unwrap();
            harq.insert(process, 77).unwrap();
            let transmission = harq.schedule(process, at(0), at(SYMBOL_CHIPS)).unwrap();
            let mut flight = Flight::new(transmission, &harq).unwrap();
            let request = flight.request_feedback(&mut port).unwrap();
            for slot in 0..usize::from(repetitions) * 3 {
                fixture.feed(slot, true);
            }
            let report = port.try_report().unwrap();
            assert_eq!(report.id, request);
            flight
                .admit_feedback_report(&report, |observation| {
                    assert_eq!(observation.evidence.candidate, Some(HarqAck::Ack));
                    assert!(observation.evidence.normalized_correlation > 0.99);
                    Feedback::Ack
                })
                .unwrap();
            assert_eq!(flight.resolve(&mut harq).unwrap(), None);
            flight.record_transmit(transmission.key, true).unwrap();
            assert_eq!(
                flight.resolve(&mut harq).unwrap(),
                Some(Resolution::Delivered(77))
            );
        }
    }
}

#[test]
fn future_request_does_not_block_ready_feedback_and_closed_receiver_retains_ids() {
    use std::num::NonZeroUsize;
    let mut fixture = Fixture::new(1, 1.0);
    let mut port = fixture
        .feedback
        .connect(NonZeroUsize::new(2).unwrap(), NonZeroUsize::MIN)
        .unwrap();
    let future = port
        .request(Request::Cqi {
            start: at(100 * CHIPS_PER_FRAME),
            repetitions: 1,
        })
        .unwrap();
    let ready = port
        .request(Request::Cqi {
            start: at(SYMBOL_CHIPS),
            repetitions: 1,
        })
        .unwrap();
    for slot in 0..5 {
        fixture.feed(slot, true);
    }
    for _ in 0..2 {
        fixture.feedback.service_requests();
    }
    let report = port.try_report().unwrap();
    assert_eq!(report.id, ready);
    assert!(matches!(
        report.result,
        Ok(Measurement::Cqi(Evidence::Measured(_)))
    ));
    assert!(port.try_report().is_none());
    drop(fixture);
    assert!(!port.available());
    assert_eq!(port.pending_requests().next().unwrap().0, future);
    assert_eq!(port.outstanding(), 1);
}

#[test]
fn expired_symbol_history_reports_missing_without_inventing_acknowledgement() {
    use std::num::NonZeroUsize;
    let mut fixture = Fixture::new(1, 1.0);
    let mut port = fixture
        .feedback
        .connect(NonZeroUsize::MIN, NonZeroUsize::MIN)
        .unwrap();
    for slot in 0..20 {
        fixture.feed(slot, true);
    }
    let id = port
        .request(Request::Cqi {
            start: at(SYMBOL_CHIPS),
            repetitions: 1,
        })
        .unwrap();
    fixture.feedback.service_requests();
    let report = port.try_report().unwrap();
    assert_eq!(report.id, id);
    assert!(matches!(
        report.result,
        Ok(Measurement::Cqi(Evidence::Missing))
    ));
}

#[test]
fn dual_feedback_delivery_keeps_cells_and_request_identity() {
    use std::num::NonZeroUsize;
    use umts_dsp::hsdpa::{CellAck, CompositeCqi, DualHarqAck};
    use umts_mac::hsdpa::Feedback;
    let mut fixture = Fixture::new(1, 1.0);
    let start = at(SYMBOL_CHIPS);
    let ack = DualHarqAck::Both {
        primary: CellAck::Ack,
        secondary: CellAck::Nack,
    };
    let cqi = CompositeCqi {
        primary: Cqi::new(9).unwrap(),
        secondary: Cqi::new(23).unwrap(),
    };
    let mut symbols = [Bit::Zero; SUBFRAME_SYMBOLS];
    symbols[..SLOT_SYMBOLS].copy_from_slice(&ack.encode());
    symbols[SLOT_SYMBOLS..].copy_from_slice(&cqi.encode());
    for (index, bit) in symbols.into_iter().enumerate() {
        let time = start.index + index as u64 * SYMBOL_CHIPS;
        let ring = (((time - fixture.feedback.origin.index) / SYMBOL_CHIPS)
            % HISTORY_SYMBOLS as u64) as usize;
        fixture.feedback.symbols[ring] = Some(Symbol {
            start: time,
            soft: if bit == Bit::Zero { 1.0 } else { -1.0 },
            coherent_fraction: 1.0,
            relative_disturbance: 0.0,
        });
    }
    fixture.feedback.processed_until = Some(at(start.index + 3 * CHIPS_PER_SLOT));
    let mut port = fixture
        .feedback
        .connect(NonZeroUsize::new(2).unwrap(), NonZeroUsize::new(2).unwrap())
        .unwrap();
    let schedule = FeedbackModeSchedule::fixed(start, FeedbackMode::Dual);
    let ack_id = port
        .request(Request::AckCells {
            start,
            repetitions: 1,
            schedule,
        })
        .unwrap();
    let cqi_id = port
        .request(Request::CqiCells {
            start,
            repetitions: 1,
            schedule,
        })
        .unwrap();
    fixture.feedback.service_requests();
    let limits = EvidenceLimits::new(0.5, 0.01, 0.5, 1.0, 0.01).unwrap();
    let policy = FeedbackPolicy {
        acknowledgement: limits,
        cqi: limits,
    };
    let report = port.try_report().unwrap();
    assert_eq!(report.id, ack_id);
    let Ok(Measurement::AckCells(Evidence::Measured(observation))) = report.result else {
        panic!("dual ACK evidence");
    };
    assert_eq!(
        policy.classify_ack_cells(&observation),
        [Feedback::Ack, Feedback::Nack]
    );
    let report = port.try_report().unwrap();
    assert_eq!(report.id, cqi_id);
    let Ok(Measurement::CqiCells(Evidence::Measured(observation))) = report.result else {
        panic!("dual CQI evidence");
    };
    assert_eq!(
        policy.classify_cqi_cells(&observation),
        [Some(cqi.primary), Some(cqi.secondary)]
    );
}
