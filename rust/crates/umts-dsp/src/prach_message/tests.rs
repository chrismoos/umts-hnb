// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::tfci::Tfci;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn instant() -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
        index: 12345,
    }
}

fn ovsf(sf: usize, index: usize) -> Vec<f32> {
    let mut code = vec![1.0];
    for bit in (0..sf.ilog2()).rev() {
        let parent = code.clone();
        let sign = if index & (1 << bit) == 0 { 1.0 } else { -1.0 };
        code.extend(parent.into_iter().map(|value| value * sign));
    }
    code
}

fn channel(slot: usize, gain: f32) -> Complex32 {
    Complex32::from_polar(
        gain * (1.0 + (slot % 3) as f32 * 0.1) * if slot < 15 { 1.0 } else { 0.5 },
        0.3 + slot as f32 * 0.17,
    )
}

fn synthesize(sf: usize, frames: usize, tfci: Tfci, data: &[f64], gain: f32) -> Vec<Complex32> {
    let pilots = [
        "11111110", "10101110", "10111011", "10101010", "11101011", "11111110", "11111010",
        "11101010", "10111110", "11111111", "10111011", "11101111", "11101010", "10101111",
        "10101111",
    ];
    let code = PrachCode::new(7504).unwrap();
    let mut scramble = vec![Complex32::default(); 38400];
    code.write_message_scrambling(&mut scramble).unwrap();
    let control = ovsf(256, 16 * 13 + 15);
    let spreading = ovsf(sf, sf * 13 / 16);
    let word = tfci.encode();
    (0..frames * 38400)
        .map(|chip| {
            let slot = chip / 2560;
            let symbol = (chip % 2560) / 256;
            let bit = if symbol < 8 {
                pilots[slot % 15].as_bytes()[symbol] == b'1'
            } else {
                word[(slot % 15) * 2 + symbol - 8] == crate::bits::Bit::One
            };
            let q = if bit { -1.0 } else { 1.0 } * control[chip % 256] * (8.0 / 15.0);
            let value = data[chip / sf];
            let i = (if value < 0.0 {
                -1.0
            } else if value > 0.0 {
                1.0
            } else {
                0.0
            }) * spreading[chip % sf];
            Complex32::new(i, q) * scramble[chip % 38400] * channel(slot, gain)
        })
        .collect()
}

#[test]
fn all_data_rates_and_both_ttis_recover_pilots_tfci_and_coherent_data() {
    let code = Arc::new(PrachMessageCode::new(PrachCode::new(7504).unwrap()).unwrap());
    let decoder = TfciDecoder::new(&[0, 1, 2, 3].map(|index| Tfci::new(index).unwrap())).unwrap();
    for interval in [Rel99Tti::Ms10, Rel99Tti::Ms20] {
        for sf in [256, 128, 64, 32] {
            for gain in [1e-16, 1.0, 10.0] {
                let mut receiver = PrachMessageReceiver::new(
                    code.clone(),
                    PreambleSignature::new(13).unwrap(),
                    interval,
                )
                .unwrap();
                let data: Vec<_> = (0..interval.frames() * 38400 / sf)
                    .map(|index| if index % 7 < 3 { 1.0 } else { -1.0 })
                    .collect();
                let input = synthesize(sf, interval.frames(), Tfci::new(3).unwrap(), &data, gain);
                let message = receiver.receive(instant(), &input).unwrap();
                assert_eq!(message.start(), instant());
                assert_eq!(message.code(), code.code());
                assert_eq!(message.signature().number(), 13);
                let control = message.control();
                assert_eq!(control.pilots().len(), 15 * interval.frames());
                for (slot, pilot) in control.pilots().iter().enumerate() {
                    let expected = channel(slot, gain) * (8.0 / 15.0);
                    let expected = Complex64::new(f64::from(expected.re), f64::from(expected.im));
                    assert!((pilot.channel / expected - Complex64::new(1.0, 0.0)).norm() < 1e-6);
                    assert!(pilot.residual_power / pilot.observed_power < 1e-10);
                }
                let decision = control.decode_tfci(&decoder).unwrap().unwrap();
                assert_eq!(decision.tfci.index(), 3);
                let weights: [f64; 15] = std::array::from_fn(|slot| {
                    (0..interval.frames())
                        .map(|frame| {
                            let h = channel(slot + frame * 15, gain);
                            (f64::from(h.re).powi(2) + f64::from(h.im).powi(2))
                                * (8.0_f64 / 15.0).powi(2)
                        })
                        .sum()
                });
                let expected_score = 2.0 * weights.iter().sum::<f64>()
                    / (30.0 * 2.0 * weights.iter().map(|value| value * value).sum::<f64>()).sqrt();
                assert!((decision.normalized_score - expected_score).abs() < 1e-6);
                let mut actual = vec![0.0; data.len()];
                message.despread_data(sf as u16, &mut actual).unwrap();
                for (index, (value, expected)) in actual.iter().zip(&data).enumerate() {
                    let channel = channel(index * sf / 2560, gain);
                    let power = f64::from(channel.re).powi(2) + f64::from(channel.im).powi(2);
                    assert!((value / (expected * power * (8.0 / 15.0)) - 1.0).abs() < 1e-6);
                }
            }
        }
    }
}

#[test]
fn complete_tti_normalization_preserves_relative_frame_weights_and_extreme_gain() {
    for scale in [1e-200_f64, 1.0, 1e200] {
        let mut output = [0.0; 4];
        normalize_soft_bits(&[scale, -scale * 0.5, scale * 0.25, 0.0], &mut output).unwrap();
        assert_eq!(output, [1.0, -0.5, 0.25, 0.0]);
    }
    let mut output = [0.5; 4];
    assert!(normalize_soft_bits(&[0.0, 1.0, f64::NAN, 0.0], &mut output).is_err());
    assert_eq!(output, [0.5; 4]);
    normalize_soft_bits(&[0.0; 4], &mut output).unwrap();
    assert_eq!(output, [0.0; 4]);
}

#[test]
fn silence_invalid_samples_and_layouts_do_not_produce_control_decisions() {
    let code = Arc::new(PrachMessageCode::new(PrachCode::new(7504).unwrap()).unwrap());
    let signature = PreambleSignature::new(13).unwrap();
    assert!(PrachMessageReceiver::new(code.clone(), signature, Rel99Tti::Ms40).is_err());
    let mut receiver = PrachMessageReceiver::new(code, signature, Rel99Tti::Ms10).unwrap();
    let mut input = vec![Complex32::default(); 38400];
    assert!(receiver.receive(instant(), &input[..1]).is_err());
    input[0].im = f32::NAN;
    assert!(receiver.receive(instant(), &input).is_err());
    input[0].im = 0.0;
    let message = receiver.receive(instant(), &input).unwrap();
    let decoder = TfciDecoder::new(&[Tfci::new(0).unwrap()]).unwrap();
    assert!(message.control().decode_tfci(&decoder).unwrap().is_none());
    let mut output = vec![1.0; 600];
    assert!(message.despread_data(16, &mut output).is_err());
    assert!(output.iter().all(|value| *value == 1.0));
    assert!(message.despread_data(32, &mut output).is_err());
    assert!(output.iter().all(|value| *value == 1.0));
    message.despread_data(64, &mut output).unwrap();
    assert!(output.iter().all(|value| *value == 0.0));
}

#[test]
fn complete_message_path_decodes_independent_transport_vectors_without_format_hunting() {
    use crate::{
        convolutional::CodeRate,
        crc::TransportCrc,
        prach_transport::{PrachFormat, tests::reference},
    };
    let source = PrachCode::new(7504).unwrap();
    let code = Arc::new(PrachMessageCode::new(source).unwrap());
    let signature = PreambleSignature::new(13).unwrap();
    let tfci = Tfci::new(1).unwrap();
    for (bits, blocks, rate, interval) in [
        (48, 1, CodeRate::Half, Rel99Tti::Ms10),
        (48, 1, CodeRate::Third, Rel99Tti::Ms10),
        (256, 1, CodeRate::Half, Rel99Tti::Ms10),
        (256, 2, CodeRate::Half, Rel99Tti::Ms10),
        (256, 2, CodeRate::Half, Rel99Tti::Ms20),
        (256, 4, CodeRate::Third, Rel99Tti::Ms20),
    ] {
        let format = PrachFormat {
            block_bits: bits,
            blocks,
            crc: TransportCrc::Crc16,
            rate,
            interval,
        };
        let plan = PrachTransportPlan::new(format, 32, 40).unwrap();
        let sf = plan.spreading_factor();
        let mut decoder = PrachMessageDecoder::new(source, 1 << 13, vec![(tfci, plan)]).unwrap();
        let mut receiver = PrachMessageReceiver::new(code.clone(), signature, interval).unwrap();
        let payload: Vec<_> = (0..bits * blocks)
            .map(|i| Bit::from((i * 11 + i / 3) % 13 < 6))
            .collect();
        for gain in [1e-12, 10.0] {
            for bad_crc in [false, true, false] {
                let data: Vec<_> = reference(format, &payload, sf, bad_crc, false)
                    .iter()
                    .map(|value| f64::from(*value))
                    .collect();
                let input = synthesize(usize::from(sf), interval.frames(), tfci, &data, gain);
                let message = receiver.receive(instant(), &input).unwrap();
                let mut output = vec![Bit::One; payload.len() + 8];
                let saved = output.clone();
                let result = decoder.decode(&message, &mut output).unwrap();
                if bad_crc {
                    assert!(result.is_none());
                    assert_eq!(output, saved);
                } else {
                    let result = result.unwrap();
                    assert_eq!(result.payload_bits, payload.len());
                    assert_eq!(result.tfci.tfci, tfci);
                    assert_eq!(result.start, instant());
                    assert_eq!(result.code, source);
                    assert_eq!(result.signature, signature);
                    assert_eq!(output[..payload.len()], payload);
                    assert_eq!(output[payload.len()..], [Bit::One; 8]);
                }
            }
        }
    }
}

#[test]
fn configuration_and_message_identity_are_checked_before_payload_publication() {
    use crate::{convolutional::CodeRate, crc::TransportCrc, prach_transport::PrachFormat};
    let code = PrachCode::new(7504).unwrap();
    let tfci = Tfci::new(0).unwrap();
    let plan = |interval| {
        PrachTransportPlan::new(
            PrachFormat {
                block_bits: 256,
                blocks: 1,
                crc: TransportCrc::Crc16,
                rate: CodeRate::Half,
                interval,
            },
            32,
            100,
        )
        .unwrap()
    };
    assert!(PrachMessageDecoder::new(code, 0, vec![(tfci, plan(Rel99Tti::Ms10))]).is_err());
    assert!(PrachMessageDecoder::new(code, 1, Vec::new()).is_err());
    assert!(
        PrachMessageDecoder::new(
            code,
            1,
            vec![(tfci, plan(Rel99Tti::Ms10)), (tfci, plan(Rel99Tti::Ms10))]
        )
        .is_err()
    );
    assert!(
        PrachMessageDecoder::new(
            code,
            1,
            vec![
                (tfci, plan(Rel99Tti::Ms10)),
                (Tfci::new(1).unwrap(), plan(Rel99Tti::Ms20))
            ]
        )
        .is_err()
    );
    let mut receiver = PrachMessageReceiver::new(
        Arc::new(PrachMessageCode::new(code).unwrap()),
        PreambleSignature::new(13).unwrap(),
        Rel99Tti::Ms10,
    )
    .unwrap();
    let input = vec![Complex32::default(); 38400];
    let message = receiver.receive(instant(), &input).unwrap();
    let mut output = vec![Bit::One; 256];
    for (expected_code, signatures, interval) in [
        (PrachCode::new(0).unwrap(), 1 << 13, Rel99Tti::Ms10),
        (code, 1 << 12, Rel99Tti::Ms10),
        (code, 1 << 13, Rel99Tti::Ms20),
    ] {
        let mut decoder =
            PrachMessageDecoder::new(expected_code, signatures, vec![(tfci, plan(interval))])
                .unwrap();
        assert!(decoder.decode(&message, &mut output).is_err());
        assert!(output.iter().all(|bit| *bit == Bit::One));
    }
    let mut decoder =
        PrachMessageDecoder::new(code, 1 << 13, vec![(tfci, plan(Rel99Tti::Ms10))]).unwrap();
    assert!(decoder.decode(&message, &mut output).unwrap().is_none());
    assert!(output.iter().all(|bit| *bit == Bit::One));
}

#[test]
fn sampled_pulse_shaped_messages_recover_crc_valid_payloads() {
    use crate::{
        carrier::{CarrierCorrector, CarrierOffsetHz},
        convolutional::CodeRate,
        crc::TransportCrc,
        prach_transport::{PrachFormat, tests::reference},
        pulse_shaping::{PulseShaper, SamplesPerChip},
        receive_filter::ReceiveFilter,
    };
    use umts_types::time::{CHIPS_PER_SECOND, SampleInstant, SampleRate};
    for rate in [
        SamplesPerChip::Two,
        SamplesPerChip::Four,
        SamplesPerChip::Eight,
    ] {
        for interval in [Rel99Tti::Ms10, Rel99Tti::Ms20] {
            let source = PrachCode::new(7504).unwrap();
            let code = Arc::new(PrachMessageCode::new(source).unwrap());
            let tfci = Tfci::new(3).unwrap();
            let format = PrachFormat {
                block_bits: 256,
                blocks: 1,
                crc: TransportCrc::Crc16,
                rate: CodeRate::Half,
                interval,
            };
            let plan = PrachTransportPlan::new(format, 32, 100).unwrap();
            let sf = plan.spreading_factor();
            let payload: Vec<_> = (0..256).map(|i| Bit::from(i % 7 < 3)).collect();
            let data: Vec<_> = reference(format, &payload, sf, false, false)
                .into_iter()
                .map(f64::from)
                .collect();
            let chips = synthesize(usize::from(sf), interval.frames(), tfci, &data, 0.001);
            let span = 32;
            let mut padded = vec![Complex32::default(); span];
            padded.extend_from_slice(&chips);
            padded.resize(padded.len() + span * 2, Complex32::default());
            let mut tx = PulseShaper::new(rate, span).unwrap();
            let mut samples = vec![Complex32::default(); tx.output_length(padded.len()).unwrap()];
            tx.process(&padded, &mut samples).unwrap();
            let sample_rate = SampleRate::new(CHIPS_PER_SECOND * rate.get() as u64, 1).unwrap();
            let offset_hz = if matches!(interval, Rel99Tti::Ms10) {
                2400.0
            } else {
                -1700.0
            };
            for (index, sample) in samples.iter_mut().enumerate() {
                let phase =
                    0.37 + std::f64::consts::TAU * offset_hz * index as f64 / sample_rate.hz();
                let shifted = Complex64::new(f64::from(sample.re), f64::from(sample.im))
                    * Complex64::from_polar(1.0, phase);
                *sample = Complex32::new(shifted.re as f32, shifted.im as f32);
            }
            let mut carrier = CarrierCorrector::new(
                sample_rate,
                CarrierOffsetHz::new(offset_hz).unwrap(),
                -0.37,
                SampleInstant {
                    epoch: instant().epoch,
                    index: 0,
                },
            )
            .unwrap();
            carrier.process(carrier.next_input(), &mut samples).unwrap();
            let mut rx = ReceiveFilter::new(
                rate,
                span,
                0,
                SampleInstant {
                    epoch: instant().epoch,
                    index: 0,
                },
            )
            .unwrap();
            let mut filtered = vec![Complex32::default(); rx.output_length(samples.len()).unwrap()];
            let window = rx
                .process(rx.next_input(), &samples, &mut filtered)
                .unwrap();
            let message_center = span * rate.get() + tx.group_delay_samples();
            let offset =
                (message_center - window.first_center.unwrap().index as usize) / rate.get();
            let mut receiver =
                PrachMessageReceiver::new(code, PreambleSignature::new(13).unwrap(), interval)
                    .unwrap();
            let message = receiver
                .receive(instant(), &filtered[offset..offset + chips.len()])
                .unwrap();
            let mut decoder =
                PrachMessageDecoder::new(source, 1 << 13, vec![(tfci, plan)]).unwrap();
            let mut output = vec![Bit::Zero; payload.len()];
            assert!(decoder.decode(&message, &mut output).unwrap().is_some());
            assert_eq!(output, payload);
        }
    }
}
