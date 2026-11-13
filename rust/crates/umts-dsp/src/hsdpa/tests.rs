// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroUsize;

fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

#[test]
fn composed_reference_power_matches_cpich_across_modulations_and_code_counts() {
    use crate::{
        broadcast::FRAME_BITS,
        common_downlink::{CommonChannelAmplitudes, CommonDownlink},
        scrambling::PrimaryScramblingCode,
    };
    use num_complex::Complex32;
    use std::num::NonZeroU64;
    use umts_types::{
        RadioEpoch,
        hsdpa::Category,
        time::{CHIPS_PER_SLOT, ChipInstant},
    };

    let primary = PrimaryScramblingCode::new(469).unwrap();
    let cpich = 0.1;
    let gamma = 6.0;
    let common = CommonDownlink::new(
        primary,
        CommonChannelAmplitudes {
            cpich,
            pccpch: 0.0,
            primary_sch: 0.0,
            secondary_sch: 0.0,
        },
    )
    .unwrap();
    let mean_power = |samples: &[Complex32]| {
        samples
            .iter()
            .map(|value| f64::from(value.norm_sqr()))
            .sum::<f64>()
            / samples.len() as f64
    };
    let mut pilot = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
    common
        .compose_slot(&[Bit::Zero; FRAME_BITS], 0, &mut pilot)
        .unwrap();
    let pilot_power = mean_power(&pilot);
    assert!((pilot_power - f64::from(cpich).powi(2)).abs() < 1e-8);
    let start = ChipInstant {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        index: 0,
    };
    for (category, cqi) in [(10, 1), (10, 15), (10, 16), (10, 22), (10, 30), (12, 30)] {
        let reference = Cqi::new(cqi)
            .unwrap()
            .reference(Category::new(category).unwrap())
            .unwrap();
        let spreader = PdschSpreader::new(primary, 1, usize::from(reference.codes)).unwrap();
        let width = reference.modulation.bits_per_symbol();
        let bits: Vec<_> = (0..usize::from(reference.codes) * reference.modulation.bits_per_code())
            .map(|index| {
                let label = (index / width) % (1 << width);
                Bit::from(label & (1 << (width - 1 - index % width)) != 0)
            })
            .collect();
        let amplitude = reference.amplitude_per_code(cpich, gamma).unwrap();
        let mut data = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
        spreader
            .add_slot(reference.modulation, &bits, start, 0, amplitude, &mut data)
            .unwrap();
        let measured = mean_power(&data) / pilot_power;
        let expected =
            10.0_f64.powf((f64::from(gamma) + f64::from(reference.power_adjustment_db)) / 10.0);
        assert!(
            (measured / expected - 1.0).abs() < 1e-5,
            "category {category} CQI {cqi}: {measured} versus {expected}"
        );
    }
}

#[test]
fn collection_fills_systematic_rows_then_parity_two_first() {
    let mut output = [0; 12];
    collect_harq_bits(
        Modulation::Qam16,
        &[1, 2, 3, 4, 5],
        &[11, 12, 13],
        &[21, 22, 23, 24],
        &mut output,
    )
    .unwrap();
    assert_eq!(output, [1, 2, 21, 11, 3, 4, 22, 12, 5, 23, 13, 24]);
}

#[test]
fn table_eleven_versions_use_rendered_spec_output_bits() {
    let interleaver = PhysicalChannelInterleaver::new().unwrap();
    for label in 0..16_u8 {
        let symbol = std::array::from_fn::<_, 4, _>(|bit| Bit::from(label & (1 << (3 - bit)) != 0));
        let input: Vec<_> = symbol.into_iter().cycle().take(1920).collect();
        let invert = |bit| Bit::from(bit == Bit::Zero);
        let mut base = vec![Bit::Zero; input.len()];
        interleaver
            .prepare(
                Modulation::Qam16,
                ConstellationVersion::Original,
                &input,
                &mut base,
            )
            .unwrap();
        for (index, version) in [
            ConstellationVersion::Original,
            ConstellationVersion::Swap,
            ConstellationVersion::Invert,
            ConstellationVersion::SwapInvert,
        ]
        .into_iter()
        .enumerate()
        {
            let mut output = vec![Bit::Zero; input.len()];
            interleaver
                .prepare(Modulation::Qam16, version, &input, &mut output)
                .unwrap();
            for (symbol, actual) in base.chunks_exact(4).zip(output.chunks_exact(4)) {
                let expected = [
                    [symbol[0], symbol[1], symbol[2], symbol[3]],
                    [symbol[2], symbol[3], symbol[0], symbol[1]],
                    [symbol[0], symbol[1], invert(symbol[2]), invert(symbol[3])],
                    [symbol[2], symbol[3], invert(symbol[0]), invert(symbol[1])],
                ];
                assert_eq!(actual, expected[index]);
            }
        }
    }
}

#[test]
fn harq_all_redundancies_cover_repetition_puncturing_and_ir_limits() {
    for input in [132, 15360, 30720] {
        let original: Vec<_> = (0..input).collect();
        for ir in [input, input - input / 6] {
            for modulation in [Modulation::Qpsk, Modulation::Qam16] {
                let versions = if modulation == Modulation::Qpsk { 4 } else { 2 };
                for codes in [1, 5, 15] {
                    for prioritize_systematic in [false, true] {
                        for version in 0..versions {
                            let plan = HarqRateMatching::new(
                                nonzero(input),
                                nonzero(ir),
                                modulation,
                                codes,
                                Redundancy {
                                    prioritize_systematic,
                                    version,
                                },
                                nonzero(40000),
                            )
                            .unwrap();
                            let mut output = vec![usize::MAX; plan.output_bits()];
                            plan.apply(&original, &mut output).unwrap();
                            assert_eq!(output.len(), codes * modulation.bits_per_code());
                            assert!(output.iter().all(|&source| source < input));
                            if !prioritize_systematic
                                && output.len() <= 2 * (input / 3) - (input - ir)
                            {
                                assert!(output.iter().all(|source| source % 3 != 0));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn invalid_geometry_does_not_modify_destination() {
    let mut output = [9; 12];
    assert!(collect_harq_bits(Modulation::Qpsk, &[1], &[2], &[3], &mut output).is_err());
    assert_eq!(output, [9; 12]);
    assert!(
        HarqRateMatching::new(
            nonzero(132),
            nonzero(132),
            Modulation::Qam16,
            1,
            Redundancy {
                prioritize_systematic: true,
                version: 2
            },
            nonzero(1000)
        )
        .is_err()
    );
}
