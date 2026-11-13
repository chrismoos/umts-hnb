// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::{Complex32, Complex64};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::{
    channel_codes::OvsfCode,
    dedicated::{LikelihoodSettings, UplinkControlFormat, UplinkDpchCode, UplinkDpchReceiver},
    hsupa::{CollectionProgress, TtiCollector, UplinkChannels},
    pilots::UplinkPilots,
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use umts_types::{
    RadioEpoch,
    hsupa::Tti,
    time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant},
};

fn data(channel: usize, symbol: usize) -> f32 {
    if (channel.is_multiple_of(2) && symbol.is_multiple_of(2))
        || (channel % 2 == 1 && symbol % 4 < 2)
    {
        1.0
    } else {
        -1.0
    }
}

#[test]
fn tracked_pairs_preserve_channel_bits_and_refine_phase_in_both_branch_orders() {
    let epoch = RadioEpoch::new(NonZeroU64::MIN);
    let at = |index| ChipInstant { epoch, index };
    let scrambling = UplinkScramblingCode::new(12345).unwrap();
    let mut sequence = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
    UplinkLongSequence::new(scrambling, 0)
        .unwrap()
        .write(&mut sequence);
    for layout in [&[4, 4][..], &[2, 2][..], &[2, 2, 4, 4][..]] {
        for maximum_dpdch in [0, 1] {
            if layout.len() == 4 && maximum_dpdch == 1 {
                continue;
            }
            for hsdpa in [false, true] {
                let codes: Vec<_> = layout
                    .iter()
                    .map(|&sf| {
                        let index = if maximum_dpdch == 1 {
                            sf / 2
                        } else {
                            (sf / 4).max(1)
                        };
                        let mut code = vec![0.0; sf as usize];
                        OvsfCode::new(sf, index).unwrap().write(&mut code).unwrap();
                        code
                    })
                    .collect();
                for gain in [0.001_f32, 1.0, 1000.0] {
                    for degrees in [-20.0_f32, 20.0] {
                        let control_channel = Complex32::from_polar(0.03, 0.7);
                        let data_channel =
                            control_channel * Complex32::from_polar(1.0, degrees.to_radians());
                        let waveform: Vec<_> = sequence
                            .iter()
                            .enumerate()
                            .map(|(chip, &scrambling)| {
                                let slot = chip / CHIPS_PER_SLOT as usize;
                                let symbol = (chip % CHIPS_PER_SLOT as usize) / 256;
                                let word = UplinkPilots::Six.word(slot as u8).unwrap();
                                let sign = if symbol < 6 && word & (1 << (5 - symbol)) != 0 {
                                    -1.0
                                } else {
                                    1.0
                                };
                                // Zero-mean pilot scatter gives finite pilot phase uncertainty.
                                let scatter = if symbol < 6 {
                                    Complex32::new(
                                        if symbol.is_multiple_of(2) { 0.3 } else { -0.3 },
                                        if symbol.is_multiple_of(3) {
                                            0.1
                                        } else if symbol % 3 == 1 {
                                            -0.1
                                        } else {
                                            0.0
                                        },
                                    )
                                } else {
                                    Complex32::default()
                                };
                                let mut value = Complex32::new(0.0, 1.0)
                                    * (Complex32::new(1.0, 0.0) + scatter)
                                    * sign
                                    * control_channel;
                                for (channel, (&sf, code)) in layout.iter().zip(&codes).enumerate()
                                {
                                    let symbol = chip / sf as usize;
                                    let amplitude =
                                        gain * data(channel, symbol) * code[chip % code.len()];
                                    let quadrature =
                                        (channel % 2 == 1) ^ (maximum_dpdch == 1 && !hsdpa);
                                    value += data_channel
                                        * if quadrature {
                                            Complex32::new(0.0, amplitude)
                                        } else {
                                            Complex32::new(amplitude, 0.0)
                                        };
                                }
                                value * scrambling
                            })
                            .collect();
                        for interval in [Tti::TwoMilliseconds, Tti::TenMilliseconds] {
                            let code = Arc::new(UplinkDpchCode::new(scrambling).unwrap());
                            let channels = UplinkChannels::new(
                                code.clone(),
                                at(0),
                                maximum_dpdch,
                                hsdpa,
                                layout,
                            )
                            .unwrap();
                            let mut receiver = UplinkDpchReceiver::new(
                                code.clone(),
                                UplinkControlFormat::new(0).unwrap(),
                            )
                            .unwrap();
                            receiver.begin_frame(at(0)).unwrap();
                            let settings = LikelihoodSettings::new(1e12).unwrap();
                            let mut collector = TtiCollector::new(
                                code,
                                at(0),
                                interval,
                                0.7,
                                settings,
                                NonZeroUsize::new(1_000_000).unwrap(),
                            )
                            .unwrap();
                            let slots = (interval.chips() / CHIPS_PER_SLOT) as usize;
                            for (slot, input) in waveform
                                .chunks_exact(CHIPS_PER_SLOT as usize)
                                .take(slots)
                                .enumerate()
                            {
                                receiver
                                    .receive(at(slot as u64 * CHIPS_PER_SLOT), input)
                                    .unwrap();
                                let progress =
                                    collector.feed(&receiver.last_slot().unwrap()).unwrap();
                                assert_eq!(
                                    progress == CollectionProgress::Complete,
                                    slot + 1 == slots
                                );
                            }
                            let tti = collector.complete().unwrap();
                            for first in (0..layout.len()).step_by(2) {
                                let per_slot = CHIPS_PER_SLOT as usize / layout[first] as usize;
                                let mut scratch = vec![Complex64::default(); per_slot];
                                let mut a = vec![0.0; per_slot * slots];
                                let mut b = a.clone();
                                tti.data_pair_tracked_llrs(
                                    &channels,
                                    first,
                                    [gain as f64; 2],
                                    settings,
                                    &mut scratch,
                                    [&mut a, &mut b],
                                )
                                .unwrap();
                                let mut calibrated_a = vec![0.0; per_slot * slots];
                                let mut calibrated_b = calibrated_a.clone();
                                tti.data_pair_calibrated_llrs(
                                    &channels,
                                    first,
                                    [gain as f64; 2],
                                    settings,
                                    &mut scratch,
                                    [&mut calibrated_a, &mut calibrated_b],
                                )
                                .unwrap();
                                for (actual, phase_only) in calibrated_a
                                    .iter()
                                    .chain(&calibrated_b)
                                    .zip(a.iter().chain(&b))
                                {
                                    assert!((actual / phase_only - 1.0).abs() < 0.001);
                                }
                                for (channel, llrs) in [(first, a), (first + 1, b)] {
                                    let coefficients: Vec<_> = llrs
                                        .iter()
                                        .enumerate()
                                        .map(|(symbol, llr)| llr * f64::from(data(channel, symbol)))
                                        .collect();
                                    let low =
                                        coefficients.iter().copied().fold(f64::INFINITY, f64::min);
                                    let high = coefficients
                                        .iter()
                                        .copied()
                                        .fold(f64::NEG_INFINITY, f64::max);
                                    assert!(low > 0.0);
                                    assert!(
                                        (high - low) / high < 0.01,
                                        "layout={layout:?} maxDPDCH={maximum_dpdch} HSDPA={hsdpa} gain={gain} phase={degrees} channel={channel} range={low}..{high}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
