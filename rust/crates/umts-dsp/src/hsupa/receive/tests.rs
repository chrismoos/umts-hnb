// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    bits::Bit,
    dedicated::{UplinkControlFormat, UplinkDpchReceiver},
    hsupa::ControlInformation,
    pilots::UplinkPilots,
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use num_complex::Complex32;
use std::num::NonZeroU64;
use umts_types::{
    RadioEpoch,
    hsupa::{Etfci, Rsn},
    time::CHIPS_PER_FRAME,
};

fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index,
    }
}

fn ovsf(sf: usize, index: usize) -> Vec<f32> {
    let mut code = vec![1.0];
    for bit in (0..sf.ilog2()).rev() {
        let copy = code.clone();
        let sign = if index & (1 << bit) == 0 { 1.0 } else { -1.0 };
        code.extend(copy.iter().map(|chip| chip * sign));
    }
    code
}

fn data(channel: usize, symbol: usize) -> f32 {
    if (symbol * 7 + channel * 3) % 11 < 5 {
        1.0
    } else {
        -1.0
    }
}

fn waveform(sfs: &[u16], dpdch: u8, information: ControlInformation) -> Vec<Complex32> {
    let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    UplinkLongSequence::new(UplinkScramblingCode::new(12345).unwrap(), 0)
        .unwrap()
        .write(&mut scrambling);
    let control = information.encode();
    let control_code = ovsf(256, 1);
    let codes: Vec<_> = sfs
        .iter()
        .map(|sf| {
            ovsf(
                usize::from(*sf),
                usize::from(if dpdch == 1 { sf / 2 } else { (sf / 4).max(1) }),
            )
        })
        .collect();
    (0..CHIPS_PER_FRAME as usize)
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
            let control_sign = if control[(slot % 3) * 10 + symbol] == Bit::Zero {
                1.0
            } else {
                -1.0
            };
            let mut value = Complex32::new(0.7 * control_sign * control_code[chip % 256], pilot);
            for (channel, (sf, code)) in sfs.iter().zip(&codes).enumerate() {
                let symbol = chip / usize::from(*sf);
                let sample = 0.5 * data(channel, symbol) * code[chip % code.len()];
                if channel % 2 == 0 {
                    value.re += sample;
                } else {
                    value.im += sample;
                }
            }
            value * scrambling[chip] * Complex32::from_polar(0.03, 0.8 + slot as f32 * 0.03)
        })
        .collect()
}

#[test]
fn tracked_waveform_recovers_control_and_selected_data_for_both_ttis() {
    let information = ControlInformation {
        format: Etfci::new(40).unwrap(),
        sequence: Rsn::new(2).unwrap(),
        happy: true,
    };
    let decoder = ControlDecoder::new(&[information.format, Etfci::new(41).unwrap()]).unwrap();
    for (sfs, dpdch) in [(&[16][..], 1), (&[2, 2, 4, 4][..], 0)] {
        let waveform = waveform(sfs, dpdch, information);
        for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
            let code =
                Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(12345).unwrap()).unwrap());
            let mut receiver =
                UplinkDpchReceiver::new(Arc::clone(&code), UplinkControlFormat::new(0).unwrap())
                    .unwrap();
            receiver.begin_frame(at(0)).unwrap();
            let settings = LikelihoodSettings::new(100.0).unwrap();
            let mut collector = TtiCollector::new(
                Arc::clone(&code),
                at(0),
                interval,
                0.7,
                settings,
                NonZeroUsize::new(1_000_000).unwrap(),
            )
            .unwrap();
            let channels = UplinkChannels::new(code, at(0), dpdch, true, sfs).unwrap();
            let mut completed = 0;
            for (slot, input) in waveform.chunks_exact(CHIPS_PER_SLOT as usize).enumerate() {
                receiver
                    .receive(at(slot as u64 * CHIPS_PER_SLOT), input)
                    .unwrap();
                if collector.feed(&receiver.last_slot().unwrap()).unwrap()
                    != CollectionProgress::Complete
                {
                    continue;
                }
                let tti = collector.complete().unwrap();
                let measured = tti.measure_control().unwrap();
                assert!(
                    (measured.control_in_phase_power / measured.pilot_power - 0.49).abs() < 1e-5
                );
                assert!(measured.control_quadrature_power / measured.pilot_power < 1e-10);
                let decoded = tti.decode_control(&decoder).unwrap().unwrap();
                assert_eq!(decoded.information.format, information.format);
                assert_eq!(decoded.information.sequence, information.sequence);
                assert_eq!(decoded.information.happy, information.happy);
                for (channel, sf) in sfs.iter().enumerate() {
                    let mut scratch =
                        vec![Complex64::default(); CHIPS_PER_SLOT as usize / usize::from(*sf)];
                    let mut llrs = vec![0.0; interval.chips() as usize / usize::from(*sf)];
                    tti.data_llrs(&channels, channel, 0.5, settings, &mut scratch, &mut llrs)
                        .unwrap();
                    let measured = tti
                        .measure_data(&channels, channel, 0.5, &mut scratch)
                        .unwrap();
                    assert_eq!(measured.symbols, llrs.len());
                    assert!((measured.in_phase_power - 1.0).abs() < 1e-5);
                    let paired = sfs.len() > 1;
                    let expected_quadrature = f64::from(u8::from(paired));
                    assert!((measured.quadrature_power - expected_quadrature).abs() < 1e-5);
                    assert!((measured.absolute_in_phase - 1.0).abs() < 1e-5);
                    let expected_fourth = if paired { -4.0 } else { 1.0 };
                    assert!(
                        (measured.fourth_mean - Complex64::new(expected_fourth, 0.0)).norm() < 1e-4
                    );
                    let doubled_gain = tti
                        .measure_data(&channels, channel, 1.0, &mut scratch)
                        .unwrap();
                    assert!((doubled_gain.in_phase_power - 0.25).abs() < 1e-5);
                    assert!(
                        tti.measure_data(&channels, channel, 0.0, &mut scratch)
                            .is_err()
                    );
                    let first = tti.start().index as usize / usize::from(*sf);
                    for (index, llr) in llrs.iter().enumerate() {
                        assert!(*llr * f64::from(data(channel, first + index)) > 0.0);
                    }
                }
                collector.release();
                completed += 1;
            }
            assert_eq!(completed, CHIPS_PER_FRAME / interval.chips());
        }
    }
}

#[test]
fn missing_slot_discards_partial_tti_and_budget_is_enforced() {
    let information = ControlInformation {
        format: Etfci::new(40).unwrap(),
        sequence: Rsn::new(0).unwrap(),
        happy: false,
    };
    let waveform = waveform(&[16], 1, information);
    let code = Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(12345).unwrap()).unwrap());
    let settings = LikelihoodSettings::new(100.0).unwrap();
    assert!(matches!(
        TtiCollector::new(
            Arc::clone(&code),
            at(0),
            Tti::TwoMilliseconds,
            0.7,
            settings,
            NonZeroUsize::MIN
        ),
        Err(DspError::Capacity)
    ));
    let mut collector = TtiCollector::new(
        Arc::clone(&code),
        at(0),
        Tti::TwoMilliseconds,
        0.7,
        settings,
        NonZeroUsize::new(1_000_000).unwrap(),
    )
    .unwrap();
    let mut receiver = UplinkDpchReceiver::new(code, UplinkControlFormat::new(0).unwrap()).unwrap();
    receiver.begin_frame(at(0)).unwrap();
    for (slot, input) in waveform
        .chunks_exact(CHIPS_PER_SLOT as usize)
        .take(6)
        .enumerate()
    {
        receiver
            .receive(at(slot as u64 * CHIPS_PER_SLOT), input)
            .unwrap();
        if slot == 1 {
            continue;
        }
        let result = collector.feed(&receiver.last_slot().unwrap()).unwrap();
        assert_eq!(
            result,
            match slot {
                2 => CollectionProgress::AwaitBoundary,
                5 => CollectionProgress::Complete,
                _ => CollectionProgress::Collecting,
            }
        );
    }
    assert_eq!(
        collector.complete().unwrap().start(),
        at(3 * CHIPS_PER_SLOT)
    );
}
