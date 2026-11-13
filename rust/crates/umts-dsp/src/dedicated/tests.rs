// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    DspError,
    bits::{Bit, TxBit},
    channel_codes::OvsfCode,
    pilots::UplinkPilots,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
    tfci::{Tfci, TfciDecoder},
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use num_complex::{Complex32, Complex64};
use std::{num::NonZeroU64, sync::Arc};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant},
};

mod coherent;
mod front_end;

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}
fn ovsf(sf: usize, index: usize) -> Vec<f32> {
    let mut values = vec![1.0];
    for bit in (0..sf.ilog2()).rev() {
        let parent = values.clone();
        let sign = if index & (1 << bit) == 0 { 1.0 } else { -1.0 };
        values.extend(parent.iter().map(|value| value * sign));
    }
    values
}
fn sign(value: Bit) -> f32 {
    if value == Bit::Zero { 1.0 } else { -1.0 }
}
fn data_bit(channel: usize, symbol: usize) -> Bit {
    Bit::from((symbol * 13 + channel * 7) % 17 < 8)
}
fn channel(slot: usize, gain: f32) -> Complex32 {
    Complex32::from_polar(
        gain * (1.0 + (slot % 3) as f32 * 0.2),
        0.4 + slot as f32 * 0.19,
    )
}

fn uplink_waveform(format: u8, spreading: &[(usize, usize, bool)], gain: f32) -> Vec<Complex32> {
    let fields = [
        (6, 2, 0, 2),
        (8, 0, 0, 2),
        (5, 2, 1, 2),
        (7, 0, 1, 2),
        (6, 0, 0, 4),
    ][usize::from(format)];
    let pilot_kind = [
        UplinkPilots::Six,
        UplinkPilots::Eight,
        UplinkPilots::Five,
        UplinkPilots::Seven,
        UplinkPilots::Six,
    ][usize::from(format)];
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    UplinkLongSequence::new(UplinkScramblingCode::new(0x123456).unwrap(), 0)
        .unwrap()
        .write(&mut scrambling);
    let codes: Vec<_> = spreading
        .iter()
        .map(|(sf, code, _)| ovsf(*sf, *code))
        .collect();
    let word = Tfci::new(511).unwrap().encode();
    (0..CHIPS_PER_FRAME as usize)
        .map(|chip| {
            let slot = chip / CHIPS_PER_SLOT as usize;
            let control_symbol = (chip % CHIPS_PER_SLOT as usize) / 256;
            let bit = if control_symbol < fields.0 {
                Bit::from(
                    pilot_kind.word(slot as u8).unwrap() & (1 << (fields.0 - 1 - control_symbol))
                        != 0,
                )
            } else if control_symbol < fields.0 + fields.1 {
                word[slot * 2 + control_symbol - fields.0]
            } else if control_symbol < fields.0 + fields.1 + fields.2 {
                Bit::from(!slot.is_multiple_of(2))
            } else {
                Bit::from(slot.is_multiple_of(2))
            };
            let mut value = Complex32::new(0.0, sign(bit) * 0.6);
            for (index, ((sf, _, quadrature), code)) in spreading.iter().zip(&codes).enumerate() {
                let amplitude = sign(data_bit(index, chip / sf)) * code[chip % sf] * 0.9;
                if *quadrature {
                    value.im += amplitude;
                } else {
                    value.re += amplitude;
                }
            }
            value * scrambling[chip] * channel(slot, gain)
        })
        .collect()
}

fn receiver(format: u8) -> UplinkDpchReceiver {
    UplinkDpchReceiver::new(
        Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(0x123456).unwrap()).unwrap()),
        UplinkControlFormat::new(format).unwrap(),
    )
    .unwrap()
}
fn receive_frame(receiver: &mut UplinkDpchReceiver, start: u64, input: &[Complex32]) {
    receiver.begin_frame(at(start)).unwrap();
    for (slot, input) in input.chunks_exact(CHIPS_PER_SLOT as usize).enumerate() {
        receiver
            .receive(at(start + slot as u64 * CHIPS_PER_SLOT), input)
            .unwrap();
    }
}

#[test]
fn uplink_normal_control_formats_and_every_legal_data_allocation_match_the_spec() {
    for (number, (pilots, tfci, fbi, tpc)) in [
        (6, 2, 0, 2),
        (8, 0, 0, 2),
        (5, 2, 1, 2),
        (7, 0, 1, 2),
        (6, 0, 0, 4),
    ]
    .into_iter()
    .enumerate()
    {
        let format = UplinkControlFormat::new(number as u8).unwrap();
        assert_eq!(
            (
                format.pilots().bits(),
                format.tfci_bits(),
                format.fbi_bits(),
                format.tpc_bits()
            ),
            (pilots, tfci, fbi, tpc)
        );
        assert_eq!(pilots + tfci + fbi + tpc, 10);
    }
    assert!(UplinkControlFormat::new(5).is_err());
    for sf in [4, 8, 16, 32, 64, 128, 256] {
        let layout = UplinkLayout::single(sf).unwrap();
        let data = layout.channel(0).unwrap();
        assert_eq!(data.code(), OvsfCode::new(sf, sf / 4).unwrap());
        assert_eq!(data.branch(), UplinkBranch::I);
        assert_eq!(data.bits_per_slot(), 2560 / usize::from(sf));
        assert!(layout.channel(1).is_none());
    }
    for count in 2..=6 {
        let layout = UplinkLayout::multicode(count).unwrap();
        for (index, code) in [1, 1, 3, 3, 2, 2].into_iter().enumerate().take(count) {
            assert_eq!(
                layout.channel(index).unwrap().code(),
                OvsfCode::new(4, code).unwrap()
            );
            assert_eq!(
                layout.channel(index).unwrap().branch(),
                if index % 2 == 0 {
                    UplinkBranch::I
                } else {
                    UplinkBranch::Q
                }
            );
        }
    }
    for sf in [0, 1, 2, 3, 5, 512] {
        assert!(UplinkLayout::single(sf).is_err());
    }
    for count in [0, 1, 7] {
        assert!(UplinkLayout::multicode(count).is_err());
    }
    assert!(UplinkLayout::control_only().is_empty());
}

#[test]
fn control_format_changes_only_at_an_admitted_frame_boundary() {
    let mut receiver = receiver(0);
    let bytes = receiver.sample_storage_bytes();
    receive_frame(&mut receiver, 0, &uplink_waveform(0, &[], 1.0));
    let successor = UplinkControlFormat::new(1).unwrap();
    assert!(
        receiver
            .begin_frame_with_format(at(CHIPS_PER_SLOT), successor)
            .is_err()
    );
    assert_eq!(receiver.format().number(), 0);
    receiver
        .begin_frame_with_format(at(CHIPS_PER_FRAME), successor)
        .unwrap();
    assert_eq!(receiver.format().pilots().bits(), 8);
    for (slot, samples) in uplink_waveform(1, &[], 1.0)
        .chunks_exact(CHIPS_PER_SLOT as usize)
        .enumerate()
    {
        let control = receiver
            .receive(at(CHIPS_PER_FRAME + slot as u64 * CHIPS_PER_SLOT), samples)
            .unwrap();
        assert!(control.tfci.is_none());
        assert!(control.fbi.is_none());
        assert!(control.tpc.is_finite());
    }
    assert_eq!(receiver.sample_storage_bytes(), bytes);
}

#[test]
fn every_control_format_recovers_complex_pilots_and_tpc_without_gain_thresholds() {
    for format in 0..5 {
        for gain in [1e-12, 0.7, 1e12] {
            let input = uplink_waveform(format, &[], gain);
            let mut rx = receiver(format);
            receive_frame(&mut rx, 1024, &input);
            let frame = rx.frame().unwrap();
            assert_eq!(frame.start(), at(1024));
            for (slot, control) in frame.controls().iter().enumerate() {
                let expected = channel(slot, gain) * 0.6;
                let expected = Complex64::new(f64::from(expected.re), f64::from(expected.im));
                assert!((control.pilot.channel - expected).norm() / expected.norm() < 2e-6);
                assert!(control.pilot.residual_power / control.pilot.observed_power < 1e-12);
                assert_eq!(control.tpc < 0.0, slot % 2 == 0);
                if let Some(value) = control.fbi {
                    assert_eq!(value < 0.0, slot % 2 != 0);
                }
            }
            let decoder = TfciDecoder::new(&[
                Tfci::new(0).unwrap(),
                Tfci::new(511).unwrap(),
                Tfci::new(1023).unwrap(),
            ])
            .unwrap();
            let decision = frame.decode_tfci(&decoder).unwrap();
            assert_eq!(
                decision.map(|value| value.tfci.index()),
                matches!(format, 0 | 2).then_some(511)
            );
        }
    }
}

#[test]
fn single_dpdch_ladder_and_six_channel_iq_multicode_recover_ordered_soft_bits() {
    let mut cases = Vec::new();
    for sf in [256, 128, 64, 32, 16, 8, 4] {
        cases.push((
            UplinkLayout::single(sf).unwrap(),
            vec![(usize::from(sf), usize::from(sf / 4), false)],
        ));
    }
    for count in 2..=6 {
        cases.push((
            UplinkLayout::multicode(count).unwrap(),
            [1, 1, 3, 3, 2, 2]
                .iter()
                .enumerate()
                .take(count)
                .map(|(index, code)| (4, *code, index % 2 != 0))
                .collect(),
        ));
    }
    for (layout, spreading) in cases {
        let input = uplink_waveform(0, &spreading, 0.8);
        let mut rx = receiver(0);
        receive_frame(&mut rx, 1024, &input);
        let frame = rx.frame().unwrap();
        let decoder = TfciDecoder::new(&[Tfci::new(17).unwrap(), Tfci::new(511).unwrap()]).unwrap();
        assert_eq!(
            frame.decode_tfci(&decoder).unwrap().unwrap().tfci.index(),
            511
        );
        for index in 0..layout.len() {
            let allocation = layout.channel(index).unwrap();
            let mut data = vec![0.0; allocation.bits_per_slot() * 15];
            frame.despread_data(layout, index, &mut data).unwrap();
            for (symbol, value) in data.into_iter().enumerate() {
                let slot = symbol / allocation.bits_per_slot();
                let expected = f64::from(sign(data_bit(index, symbol)))
                    * f64::from(channel(slot, 0.8).norm_sqr())
                    * 0.6
                    * 0.9;
                assert!(
                    (value - expected).abs() / expected.abs() < 3e-6,
                    "channel {index} symbol {symbol}: {value} vs {expected}"
                );
            }
        }
    }
}

#[test]
fn missing_repeated_foreign_and_nonfinite_slots_cannot_create_a_complete_frame() {
    let input = uplink_waveform(0, &[(64, 16, false)], 1.0);
    let mut rx = receiver(0);
    assert!(rx.receive(at(0), &input[..2560]).is_err());
    rx.begin_frame(at(1024)).unwrap();
    rx.receive(at(1024), &input[..2560]).unwrap();
    assert!(rx.receive(at(1024), &input[..2560]).is_err());
    assert!(rx.receive(at(1024 + 5120), &input[..2560]).is_err());
    let foreign = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
        index: 1024 + 2560,
    };
    assert!(rx.receive(foreign, &input[..2560]).is_err());
    let mut bad = input[2560..5120].to_vec();
    bad[19].re = f32::NAN;
    assert_eq!(
        rx.receive(at(1024 + 2560), &bad).unwrap_err(),
        DspError::NonFiniteEvidence
    );
    assert!(rx.frame().is_none());
    assert!(rx.begin_frame(at(1024)).is_err());
    receive_frame(&mut rx, 1024 + 38400, &input);
    assert!(rx.frame().is_some());
    assert!(
        rx.begin_frame(ChipInstant {
            index: 1024 + 76800,
            ..foreign
        })
        .is_err()
    );
    assert!(receiver(0).begin_frame(at(u64::MAX)).is_err());
}

#[test]
fn silence_and_erasure_are_not_promoted_to_a_transport_format_or_data_bits() {
    let mut rx = receiver(0);
    receive_frame(&mut rx, 0, &vec![Complex32::default(); 38400]);
    let frame = rx.frame().unwrap();
    assert!(
        frame
            .controls()
            .iter()
            .all(|control| control.pilot.channel == Complex64::default() && control.tpc == 0.0)
    );
    assert!(
        frame
            .decode_tfci(
                &TfciDecoder::new(&[Tfci::new(0).unwrap(), Tfci::new(511).unwrap()]).unwrap()
            )
            .unwrap()
            .is_none()
    );
    let mut data = vec![9.0; 600];
    frame
        .despread_data(UplinkLayout::single(64).unwrap(), 0, &mut data)
        .unwrap();
    assert!(data.iter().all(|value| *value == 0.0));
    assert_eq!(
        frame.despread_data(UplinkLayout::control_only(), 0, &mut data),
        Err(DspError::InvalidCode)
    );
    assert_eq!(
        frame.despread_data(UplinkLayout::single(64).unwrap(), 0, &mut data[..599]),
        Err(DspError::InvalidLength)
    );
}

#[test]
fn acquired_suffix_preserves_real_slots_without_exposing_an_incomplete_frame() {
    for format in 0..=4 {
        let input = uplink_waveform(format, &[(64, 16, false)], 1.0);
        let mut reference = receiver(format);
        reference.begin_frame(at(1024)).unwrap();
        let mut reference_samples = Vec::new();
        for (slot, samples) in input.chunks_exact(CHIPS_PER_SLOT as usize).enumerate() {
            reference
                .receive(at(1024 + slot as u64 * CHIPS_PER_SLOT), samples)
                .unwrap();
            reference_samples.extend_from_slice(reference.last_slot().unwrap().corrected_samples());
        }
        for tracked in [false, true] {
            for first in 0..15_u8 {
                let mut rx = if tracked {
                    UplinkDpchReceiver::with_tracking(
                        reference.code().clone(),
                        reference.format(),
                        CarrierTrackingSettings {
                            initial: crate::carrier::CarrierOffsetHz::new(0.0).unwrap(),
                            maximum_absolute_hz: 1000.0,
                            update_fraction: 0.1,
                            minimum_coherence: 0.5,
                        },
                    )
                    .unwrap()
                } else {
                    receiver(format)
                };
                rx.begin_frame_suffix(at(1024), reference.format(), first)
                    .unwrap();
                assert!(rx.last_slot().is_none());
                assert!(rx.frame().is_none());
                assert!(
                    rx.begin_frame_suffix(at(1024 + 38400), reference.format(), 15)
                        .is_err()
                );
                assert!(
                    rx.begin_frame_suffix(at(u64::MAX), reference.format(), first)
                        .is_err()
                );
                assert!(
                    rx.begin_frame_suffix(
                        ChipInstant {
                            epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
                            index: 1024 + 38400
                        },
                        reference.format(),
                        first
                    )
                    .is_err()
                );
                for slot in usize::from(first)..15 {
                    let start = slot * CHIPS_PER_SLOT as usize;
                    let control = rx
                        .receive(
                            at(1024 + slot as u64 * CHIPS_PER_SLOT),
                            &input[start..start + CHIPS_PER_SLOT as usize],
                        )
                        .unwrap();
                    let expected = reference.frame().unwrap().controls()[slot];
                    assert_eq!(control.pilot.channel, expected.pilot.channel);
                    assert_eq!(control.pilot.observed_power, expected.pilot.observed_power);
                    assert_eq!(control.pilot.residual_power, expected.pilot.residual_power);
                    assert_eq!(control.tfci, expected.tfci);
                    assert_eq!(control.fbi, expected.fbi);
                    assert_eq!(control.tpc, expected.tpc);
                    let last = rx.last_slot().unwrap();
                    assert_eq!(last.start(), at(1024 + slot as u64 * CHIPS_PER_SLOT));
                    assert_eq!(
                        last.corrected_samples(),
                        &reference_samples[start..start + CHIPS_PER_SLOT as usize]
                    );
                    assert_eq!(rx.frame().is_some(), first == 0 && slot == 14);
                }
                receive_frame(&mut rx, 1024 + 38400, &input);
                assert!(rx.frame().is_some());
                assert_eq!(
                    rx.frame()
                        .unwrap()
                        .tfci_likelihoods(LikelihoodSettings::new(64.0).unwrap())
                        .unwrap(),
                    reference
                        .frame()
                        .unwrap()
                        .tfci_likelihoods(LikelihoodSettings::new(64.0).unwrap())
                        .unwrap()
                );
            }
        }
    }
}

#[test]
fn normal_downlink_format_fields_and_reserved_tfci_match_table_eleven() {
    let rows = [
        (512, 0, 4, 2, 0, 4),
        (512, 0, 2, 2, 2, 4),
        (256, 2, 14, 2, 0, 2),
        (256, 2, 12, 2, 2, 2),
        (256, 2, 12, 2, 0, 4),
        (256, 2, 10, 2, 2, 4),
        (256, 2, 8, 2, 0, 8),
        (256, 2, 6, 2, 2, 8),
        (128, 6, 28, 2, 0, 4),
        (128, 6, 26, 2, 2, 4),
        (128, 6, 24, 2, 0, 8),
        (128, 6, 22, 2, 2, 8),
        (64, 12, 48, 4, 8, 8),
        (32, 28, 112, 4, 8, 8),
        (16, 56, 232, 8, 8, 16),
        (8, 120, 488, 8, 8, 16),
        (4, 248, 1000, 8, 8, 16),
    ];
    for (number, (sf, d1, d2, tpc, tfci, pilot)) in rows.into_iter().enumerate() {
        let format = DownlinkFormat::new(number as u8, tfci != 0).unwrap();
        assert_eq!(
            (
                format.spreading_factor(),
                format.data1_bits(),
                format.data2_bits(),
                format.tpc_bits(),
                format.tfci_bits(),
                format.pilots().bits()
            ),
            (sf, d1, d2, tpc, tfci, pilot)
        );
        assert_eq!(d1 + d2 + tpc + tfci + pilot, 5120 / usize::from(sf));
        assert_eq!(
            DownlinkFormat::new(number as u8, false).is_ok(),
            tfci == 0 || sf < 128
        );
    }
    assert!(DownlinkFormat::new(17, true).is_err());
    assert!(DpchTiming::new(150).is_err());
}

#[test]
fn downlink_non_slot_offsets_keep_cell_scrambling_and_local_field_order() {
    for number in 0..17 {
        let with_tfci = matches!(number, 1 | 3 | 5 | 7 | 9 | 11 | 12 | 13 | 14 | 15 | 16);
        let format = DownlinkFormat::new(number, with_tfci).unwrap();
        for units in [1, 149] {
            let timing = DpchTiming::new(units).unwrap();
            let code = OvsfCode::new(format.spreading_factor(), 3).unwrap();
            let primary = PrimaryScramblingCode::new(17).unwrap();
            let amplitudes = DpchAmplitudes {
                data: 0.2,
                tpc: 0.3,
                tfci: 0.4,
                pilot: 0.5,
            };
            let tx =
                DownlinkDpch::new(primary, code, format, timing, DpchRole::Primary, amplitudes)
                    .unwrap();
            let data: Vec<_> = (0..format.data_bits())
                .map(|index| {
                    if index % 7 == 0 {
                        TxBit::Dtx
                    } else {
                        TxBit::Data(data_bit(0, index))
                    }
                })
                .collect();
            let mut scrambling = vec![Complex32::default(); 38400];
            downlink_sequence(primary.sequence_number(), &mut scrambling).unwrap();
            let spreading = ovsf(usize::from(format.spreading_factor()), 3);
            for slot in [0, 1, 14] {
                let mut output = vec![Complex32::new(0.02, -0.01); 2560];
                let tfci = with_tfci.then(|| Tfci::new(777).unwrap());
                tx.add_slot(slot, &data, tfci, Bit::One, &mut output)
                    .unwrap();
                let mut bits: Vec<f32> = data[..format.data1_bits()]
                    .iter()
                    .map(|bit| bit.amplitude() * 0.2)
                    .collect();
                bits.extend(vec![-0.3; format.tpc_bits()]);
                if let Some(tfci) = tfci {
                    let word = tfci.encode();
                    bits.extend((0..format.tfci_bits()).map(|index| {
                        sign(word[(usize::from(slot) * format.tfci_bits() + index) % 32]) * 0.4
                    }));
                }
                bits.extend(
                    data[format.data1_bits()..]
                        .iter()
                        .map(|bit| bit.amplitude() * 0.2),
                );
                let pilots = format.pilots().word(slot).unwrap();
                bits.extend((0..format.pilots().bits()).map(|index| {
                    if pilots & (1 << (format.pilots().bits() - 1 - index)) == 0 {
                        0.5
                    } else {
                        -0.5
                    }
                }));
                for (chip, output) in output.iter().enumerate() {
                    let symbol = chip / spreading.len();
                    let value = Complex32::new(bits[symbol * 2], bits[symbol * 2 + 1]);
                    let index = (timing.chips() as usize + usize::from(slot) * 2560 + chip) % 38400;
                    let expected = Complex32::new(0.02, -0.01)
                        + value * scrambling[index] * spreading[chip % spreading.len()] * 0.5;
                    assert!(
                        (*output - expected).norm() < 1e-6,
                        "format {number} offset {units} slot {slot} chip {chip}"
                    );
                }
            }
        }
    }
}

#[test]
fn additional_downlink_codes_and_absent_high_rate_tfci_use_dtx_not_binary_zero() {
    let format = DownlinkFormat::new(12, false).unwrap();
    let amplitudes = DpchAmplitudes {
        data: 0.2,
        tpc: 0.3,
        tfci: 0.4,
        pilot: 0.5,
    };
    for role in [DpchRole::Primary, DpchRole::Additional] {
        let tx = DownlinkDpch::new(
            PrimaryScramblingCode::new(0).unwrap(),
            OvsfCode::new(64, 7).unwrap(),
            format,
            DpchTiming::new(0).unwrap(),
            role,
            amplitudes,
        )
        .unwrap();
        let mut output = vec![Complex32::default(); 2560];
        tx.add_slot(
            0,
            &vec![TxBit::Dtx; format.data_bits()],
            None,
            Bit::One,
            &mut output,
        )
        .unwrap();
        let tfci_first = (format.data1_bits() + format.tpc_bits()) / 2 * 64;
        assert!(
            output[tfci_first..tfci_first + format.tfci_bits() / 2 * 64]
                .iter()
                .all(|value| *value == Complex32::default())
        );
        if role == DpchRole::Additional {
            assert!(output.iter().all(|value| *value == Complex32::default()));
        } else {
            assert!(output.iter().any(|value| value.norm_sqr() > 0.0));
        }
    }
}

#[test]
fn malformed_downlink_calls_do_not_partially_change_the_composite_waveform() {
    let format = DownlinkFormat::new(3, true).unwrap();
    let amplitudes = DpchAmplitudes {
        data: 0.2,
        tpc: 0.3,
        tfci: 0.4,
        pilot: 0.5,
    };
    let make = |code, amplitudes| {
        DownlinkDpch::new(
            PrimaryScramblingCode::new(0).unwrap(),
            code,
            format,
            DpchTiming::new(0).unwrap(),
            DpchRole::Primary,
            amplitudes,
        )
    };
    assert!(make(OvsfCode::new(128, 3).unwrap(), amplitudes).is_err());
    assert!(
        make(
            OvsfCode::new(256, 3).unwrap(),
            DpchAmplitudes {
                data: f32::NAN,
                ..amplitudes
            }
        )
        .is_err()
    );
    let tx = make(OvsfCode::new(256, 3).unwrap(), amplitudes).unwrap();
    let original = vec![Complex32::new(0.02, -0.01); 2560];
    let mut output = original.clone();
    assert!(
        tx.add_slot(
            0,
            &vec![TxBit::Dtx; format.data_bits()],
            None,
            Bit::One,
            &mut output
        )
        .is_err()
    );
    assert!(
        tx.add_slot(
            15,
            &vec![TxBit::Dtx; format.data_bits()],
            Some(Tfci::new(1).unwrap()),
            Bit::One,
            &mut output
        )
        .is_err()
    );
    assert_eq!(output, original);
}

#[test]
fn normalized_noise_does_not_make_control_or_data_decode_depend_on_absolute_gain() {
    let clean = uplink_waveform(2, &[(64, 16, false)], 0.8);
    let mut state = 0x1234_5678_u32;
    let noisy: Vec<_> = clean
        .iter()
        .map(|value| {
            let mut noise = || {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as i32 as f32 / i32::MAX as f32) * 0.2
            };
            *value + Complex32::new(noise(), noise())
        })
        .collect();
    for gain in [0.001, 1.0, 100.0] {
        let input: Vec<_> = noisy.iter().map(|value| value * gain).collect();
        let mut rx = receiver(2);
        receive_frame(&mut rx, 1024, &input);
        let frame = rx.frame().unwrap();
        let decoder = TfciDecoder::new(&[Tfci::new(17).unwrap(), Tfci::new(511).unwrap()]).unwrap();
        assert_eq!(
            frame.decode_tfci(&decoder).unwrap().unwrap().tfci.index(),
            511
        );
        assert!(
            frame
                .controls()
                .iter()
                .all(|value| value.pilot.residual_power > 0.0)
        );
        let mut data = vec![0.0; 600];
        frame
            .despread_data(UplinkLayout::single(64).unwrap(), 0, &mut data)
            .unwrap();
        assert!(
            data.iter()
                .enumerate()
                .all(|(symbol, value)| (*value < 0.0) == (data_bit(0, symbol) == Bit::One))
        );
    }
}

#[test]
fn pulse_shaped_uplink_survives_matched_filtering_with_explicit_sample_centers() {
    use crate::{
        pulse_shaping::{PulseShaper, SamplesPerChip},
        receive_filter::ReceiveFilter,
    };
    use umts_types::time::SampleInstant;
    let mut chips = vec![Complex32::default(); 64];
    chips.extend(uplink_waveform(0, &[(64, 16, false)], 0.8));
    chips.extend(vec![Complex32::default(); 64]);
    let rate = SamplesPerChip::Two;
    let mut shaper = PulseShaper::new(rate, 32).unwrap();
    let mut samples = vec![Complex32::default(); chips.len() * rate.get()];
    shaper.process(&chips, &mut samples).unwrap();
    let start = SampleInstant {
        epoch: at(0).epoch,
        index: 0,
    };
    let mut filter = ReceiveFilter::new(rate, 32, 0, start).unwrap();
    let mut filtered = vec![Complex32::default(); filter.output_length(samples.len()).unwrap()];
    let window = filter.process(start, &samples, &mut filtered).unwrap();
    let first_data_center = 64 * rate.get() + shaper.group_delay_samples();
    let first =
        (first_data_center - window.first_center.unwrap().index as usize) / window.sample_stride;
    let mut rx = receiver(0);
    receive_frame(&mut rx, 1024, &filtered[first..first + 38400]);
    let frame = rx.frame().unwrap();
    assert_eq!(
        frame
            .decode_tfci(
                &TfciDecoder::new(&[Tfci::new(0).unwrap(), Tfci::new(511).unwrap()]).unwrap()
            )
            .unwrap()
            .unwrap()
            .tfci
            .index(),
        511
    );
    let mut data = vec![0.0; 600];
    frame
        .despread_data(UplinkLayout::single(64).unwrap(), 0, &mut data)
        .unwrap();
    assert!(
        data.iter()
            .enumerate()
            .all(|(symbol, value)| (*value < 0.0) == (data_bit(0, symbol) == Bit::One))
    );
}

#[test]
fn full_qpsk_amplitude_is_exact_and_composition_is_not_clipped_or_quantized() {
    let format = DownlinkFormat::new(3, true).unwrap();
    let amp = DpchAmplitudes {
        data: 0.8,
        tpc: 0.8,
        tfci: 0.8,
        pilot: 0.8,
    };
    let tx = DownlinkDpch::new(
        PrimaryScramblingCode::new(0).unwrap(),
        OvsfCode::new(256, 3).unwrap(),
        format,
        DpchTiming::new(1).unwrap(),
        DpchRole::Primary,
        amp,
    )
    .unwrap();
    let mut output = vec![Complex32::default(); 2560];
    let data = vec![TxBit::Data(Bit::Zero); format.data_bits()];
    tx.add_slot(
        0,
        &data,
        Some(Tfci::new(0).unwrap()),
        Bit::Zero,
        &mut output,
    )
    .unwrap();
    assert!(output.iter().all(|value| (value.norm() - 0.8).abs() < 1e-6));
    tx.add_slot(
        0,
        &data,
        Some(Tfci::new(0).unwrap()),
        Bit::Zero,
        &mut output,
    )
    .unwrap();
    assert!(output.iter().all(|value| (value.norm() - 1.6).abs() < 1e-6));
}
