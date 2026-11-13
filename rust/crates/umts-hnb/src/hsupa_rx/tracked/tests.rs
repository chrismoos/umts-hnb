// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use num_complex::Complex32;
use std::num::NonZeroU64;
use umts_dsp::{
    bits::Bit,
    dedicated::{UplinkControlFormat, UplinkDpchReceiver},
    hsupa::{ControlInformation, TransportEncoder},
    pilots::UplinkPilots,
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use umts_mac::hsupa::Reception;
use umts_types::{
    RadioEpoch, UeKey,
    hsupa::{Rel6Category, Rsn},
    time::CHIPS_PER_FRAME,
};

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

#[test]
fn control_loss_record_requires_prior_admission_and_sustained_rejection() {
    let observation = |index, admitted| ControlObservation {
        start: at(index),
        decision: None,
        admitted,
        evidence: [index as f64; umts_dsp::hsupa::CONTROL_BITS],
        measurements: None,
        data_measurements: [None; MAX_DATA_CHANNELS],
        presence_log_likelihood: None,
    };
    let mut recorder = ControlLossRecorder::default();
    for index in 0..CONTROL_LOSS_REJECTIONS * 2 {
        assert!(recorder.observe(observation(index, false)).is_none());
    }
    assert!(recorder.observe(observation(500, true)).is_none());
    for index in 0..CONTROL_LOSS_REJECTIONS * 2 {
        assert!(recorder.observe(observation(501 + index, false)).is_none());
    }
    for index in 0..CONTROL_LOSS_ADMISSIONS {
        assert!(recorder.observe(observation(1000 + index, true)).is_none());
    }
    for index in 1..CONTROL_LOSS_REJECTIONS {
        assert!(recorder.observe(observation(1100 + index, false)).is_none());
    }
    assert!(recorder.observe(observation(2000, true)).is_none());
    for index in 1..CONTROL_LOSS_REJECTIONS {
        assert!(recorder.observe(observation(2000 + index, false)).is_none());
    }
    let loss = recorder
        .observe(observation(2000 + CONTROL_LOSS_REJECTIONS, false))
        .unwrap();
    assert_eq!(loss.last_admitted.start, at(2000));
    assert_eq!(loss.rejected.start, at(2000 + CONTROL_LOSS_REJECTIONS));
    assert_eq!(loss.last_admitted.evidence[0], 2000.0);
    assert_eq!(loss.consecutive_rejections, CONTROL_LOSS_REJECTIONS);
    assert!(recorder.observe(observation(3000, true)).is_none());
    assert!(recorder.observe(observation(3001, false)).is_none());
}

fn nz(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

#[test]
fn tracked_chips_decode_transport_and_early_poll_preserves_partial_tti() {
    for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
        let format = Etfci::new(10).unwrap();
        let information = ControlInformation {
            format,
            sequence: Rsn::new(0).unwrap(),
            happy: true,
        };
        let plan = Arc::new(
            TransportPlan::new(
                nz(120),
                interval,
                Rel6Category::new(6).unwrap(),
                &[16],
                nz(100_000),
            )
            .unwrap(),
        );
        let payload: Vec<_> = (0..120).map(|n| Bit::from(n % 3 == 0)).collect();
        let mut encoded = vec![Bit::Zero; plan.physical_bits()];
        TransportEncoder::new(Arc::clone(&plan))
            .unwrap()
            .encode(&payload, 0, &mut encoded)
            .unwrap();
        let scrambling_code = UplinkScramblingCode::new(12345).unwrap();
        let code = Arc::new(UplinkDpchCode::new(scrambling_code).unwrap());
        let identity = ReceiveIdentity {
            ue: UeKey {
                epoch: at(0).epoch,
                slot: 2,
                generation: NonZeroU64::MIN,
            },
            configuration: NonZeroU64::MIN,
        };
        let mut receiver = TrackedReceiver::new(
            Arc::clone(&code),
            TrackedSettings {
                identity,
                anchor: FrameAnchor {
                    time: at(0),
                    cfn: 255,
                },
                interval,
                maximum_dpdch: 1,
                hsdpa_configured: true,
                control_gain_ratio: 0.7,
                likelihood: LikelihoodSettings::new(100.0).unwrap(),
                admission: ControlAdmission::new(0.8, 0.01).unwrap(),
                rv_zero_only: true,
                iterations: NonZeroU8::new(6).unwrap(),
                retained_sample_bytes: nz(1_000_000),
                soft_buffer_bytes: nz(100_000),
                scratch_bytes: nz(100_000),
            },
            vec![TrackedFormat {
                identity: format,
                plan,
                layout: CodeLayout::new(&[16]).unwrap(),
                gain_ratios: vec![0.5],
            }],
        )
        .unwrap();
        let mut port = receiver.connect(nz(1), nz(100_000)).unwrap();
        let before = receiver.retained_bytes().unwrap();
        assert!(
            receiver
                .trace_control(0, nz(MAX_CONTROL_OBSERVATIONS + 1))
                .is_err()
        );
        let mut trace = receiver.trace_control(0, nz(1)).unwrap();
        assert!(receiver.retained_bytes().unwrap() > before);
        assert!(receiver.trace_control(0, nz(1)).is_err());
        let mut tracked =
            UplinkDpchReceiver::new(code, UplinkControlFormat::new(0).unwrap()).unwrap();
        tracked.begin_frame(at(0)).unwrap();
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        UplinkLongSequence::new(scrambling_code, 0)
            .unwrap()
            .write(&mut scrambling);
        let control = information.encode();
        let sign = |bit: Bit| if bit == Bit::Zero { 1.0 } else { -1.0 };
        let waveform: Vec<_> = (0..CHIPS_PER_FRAME as usize)
            .map(|chip| {
                let slot = chip / CHIPS_PER_SLOT as usize;
                let symbol = (chip % CHIPS_PER_SLOT as usize) / 256;
                let pilot = if symbol < 6
                    && UplinkPilots::Six.word(slot as u8).unwrap() & (1 << (5 - symbol)) != 0
                {
                    -1.0
                } else {
                    1.0
                };
                let control_code = if chip % 256 < 128 { 1.0 } else { -1.0 };
                // SF16 code 8 alternates each chip, E-DPCCH uses SF256 code 1.
                let data_code = if chip % 2 == 0 { 1.0 } else { -1.0 };
                let data = sign(encoded[(chip / 16) % encoded.len()]);
                Complex32::new(
                    0.7 * sign(control[(slot % 3) * 10 + symbol]) * control_code
                        + 0.5 * data * data_code,
                    pilot,
                ) * scrambling[chip]
                    * Complex32::from_polar(0.03, 0.8 + slot as f32 * 0.03)
            })
            .collect();
        let mut decoded = 0;
        for (slot, samples) in waveform.chunks_exact(CHIPS_PER_SLOT as usize).enumerate() {
            tracked
                .receive(at(slot as u64 * CHIPS_PER_SLOT), samples)
                .unwrap();
            let progress = receiver.feed(&tracked.last_slot().unwrap()).unwrap();
            if progress != CollectionProgress::Complete {
                assert!(matches!(
                    receiver.begin_ready(),
                    Err(ReceiveError::NotReady)
                ));
                continue;
            }
            receiver.discard_incomplete();
            assert!(receiver.pending());
            let final_slot = at(slot as u64 * CHIPS_PER_SLOT);
            let collected_at = at((slot as u64 + 2) * CHIPS_PER_SLOT);
            receiver.observe_collection_clock(at(final_slot.index + CHIPS_PER_SLOT), collected_at);
            assert!(receiver.collection.unwrap().clock_upper.is_none());
            receiver.observe_collection_clock(final_slot, collected_at);
            receiver.observe_collection_clock(final_slot, at(collected_at.index + CHIPS_PER_SLOT));
            let mut completed = false;
            let decode_quantum = nz(17);
            for _ in 0..10_000 {
                if receiver
                    .service_worker(&mut DecodeWork::new(decode_quantum))
                    .unwrap()
                    == Publication::Published
                {
                    assert_eq!(receiver.outstanding_reports(), 1);
                    let result = port.peek().unwrap();
                    let timing = result.report.timing.unwrap();
                    assert_eq!(timing.collection_clock_upper, Some(collected_at));
                    assert!(timing.service_calls > 1);
                    assert!(timing.elapsed >= timing.preparation + timing.service);
                    assert_eq!(result.report.identity, identity);
                    assert_eq!(result.report.start, at(decoded * interval.chips()));
                    assert_eq!(
                        result.report.absolute_tti,
                        u64::from(interval.number(255, 0).unwrap()) + decoded
                    );
                    assert_eq!(result.report.feedback, Reception::Ack { deliver: true });
                    assert_eq!(result.payload, payload.as_slice());
                    assert!(port.release());
                    assert_eq!(receiver.outstanding_reports(), 0);
                    completed = true;
                    break;
                }
            }
            assert!(completed);
            if decoded == 0 {
                let observation = trace.try_recv().unwrap();
                assert_eq!(observation.start, at(0));
                assert!(observation.admitted);
                assert_eq!(observation.decision.unwrap().information, information);
                assert!(observation.evidence.iter().all(|value| value.is_finite()));
            }
            assert!(trace.try_recv().is_none());
            decoded += 1;
        }
        assert_eq!(decoded, CHIPS_PER_FRAME / interval.chips());
    }
}
