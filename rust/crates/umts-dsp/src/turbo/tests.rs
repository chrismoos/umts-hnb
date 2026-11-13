// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    crc::TransportCrc,
    interleaving::{Interleaver, Rel99Tti},
    rate_matching::RateMatching,
    transport_blocks::BlockOutcome,
};
use std::num::NonZeroUsize;

fn iterations(value: u8) -> NonZeroU8 {
    NonZeroU8::new(value).unwrap()
}

#[test]
fn generic_transport_crc_rejects_tied_decisions_but_accepts_observed_zero_words() {
    for crc in [
        TransportCrc::None,
        TransportCrc::Crc8,
        TransportCrc::Crc12,
        TransportCrc::Crc16,
        TransportCrc::Crc24,
    ] {
        for bits in [120, 7000] {
            let plan = TurboBlockPlan::new(
                TurboBlockFormat {
                    block_bits: bits,
                    blocks: 2,
                    crc,
                },
                budget(),
            )
            .unwrap();
            let mut decoder = TurboBlockDecoder::new(plan, iterations(4)).unwrap();
            let mut evidence = vec![0.0; plan.coded_bits()];
            evidence[1] = 4.0;
            let mut output = vec![Bit::One; plan.payload_bits()];
            assert_eq!(
                decoder.begin(&evidence, &mut output).unwrap(),
                crate::coding::DecodeProgress::Pending
            );
            let mut work = crate::coding::DecodeWork::new(NonZeroUsize::MAX);
            let progress = decoder.step(&evidence, &mut output, &mut work).unwrap();
            assert_eq!(
                progress,
                crate::coding::DecodeProgress::Complete(BlockOutcome::Erased),
                "unsupported posterior ties under {crc:?}"
            );
            let used = usize::MAX - work.remaining();
            assert_eq!(
                used,
                plan.decode_state_updates(iterations(4)).unwrap() / plan.code_block_count()
            );
            assert!(output.iter().all(|bit| *bit == Bit::One));
            assert_eq!(decoder.crc_counts().passed, 0);
            evidence.fill(4.0);
            assert_eq!(
                decoder.decode(&evidence, &mut output).unwrap(),
                BlockOutcome::Decoded
            );
            assert!(output.iter().all(|bit| *bit == Bit::Zero));
            assert_eq!(
                decoder.crc_counts().passed,
                if crc.width() == 0 { 0 } else { 2 }
            );
            let width = plan.coded_bits() / plan.code_block_count();
            for missing in 0..plan.code_block_count() {
                evidence.fill(4.0);
                evidence[missing * width..(missing + 1) * width].fill(0.0);
                output.fill(Bit::One);
                assert_eq!(
                    decoder.decode(&evidence, &mut output).unwrap(),
                    BlockOutcome::Erased
                );
                assert!(output.iter().all(|bit| *bit == Bit::One));
                assert_eq!(decoder.crc_counts().passed, 0);
            }
        }
        let plan = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: 1,
                blocks: 1,
                crc,
            },
            budget(),
        )
        .unwrap();
        assert!(plan.filler_bits() > 0);
        let mut evidence = vec![0.0; plan.coded_bits()];
        for index in plan.filler_bits()..plan.code_block_bits() {
            evidence[3 * index] = 4.0;
        }
        let mut output = [Bit::One];
        let mut decoder = TurboBlockDecoder::new(plan, iterations(4)).unwrap();
        assert_eq!(
            decoder.decode(&evidence, &mut output).unwrap(),
            BlockOutcome::Decoded
        );
        assert_eq!(output, [Bit::Zero]);
    }
}

#[test]
fn periodic_normalization_preserves_extreme_finite_evidence_at_maximum_iterations() {
    for length in [MIN_BLOCK_BITS, MAX_BLOCK_BITS] {
        let plan = Arc::new(TurboInterleaver::new(length).unwrap());
        let input: Vec<_> = (0..length).map(|index| Bit::from(index % 11 < 5)).collect();
        let mut encoded = vec![Bit::Zero; coded_bits(length).unwrap()];
        encode(&plan, &input, &mut encoded).unwrap();
        for gain in [f32::from_bits(1), f32::MIN_POSITIVE, f32::MAX] {
            let evidence: Vec<_> = encoded
                .iter()
                .map(|bit| if *bit == Bit::Zero { gain } else { -gain })
                .collect();
            let mut decoder =
                TurboDecoder::new(Arc::clone(&plan), iterations(MAX_ITERATIONS)).unwrap();
            let mut output = vec![Bit::Zero; length];
            decoder.decode(&evidence, &mut output).unwrap();
            assert_eq!(output, input);
            assert!(
                decoder
                    .prior
                    .iter()
                    .chain(&decoder.posterior)
                    .flatten()
                    .all(|value| value.is_finite())
            );
        }
    }
}

#[test]
fn planned_decode_work_matches_incremental_segmented_execution() {
    for bits in [18, 979, 5567, 11487] {
        for rounds in [1, 4, 8] {
            let plan = TurboBlockPlan::new(
                TurboBlockFormat {
                    block_bits: bits,
                    blocks: 1,
                    crc: TransportCrc::Crc24,
                },
                budget(),
            )
            .unwrap();
            let iterations = iterations(rounds);
            let updates = plan.decode_state_updates(iterations).unwrap();
            let mut decoder = TurboBlockDecoder::new(plan, iterations).unwrap();
            let input = vec![1.0; plan.coded_bits()];
            let mut output = vec![Bit::Zero; bits];
            assert!(matches!(
                decoder.begin(&input, &mut output).unwrap(),
                crate::coding::DecodeProgress::Pending
            ));
            let mut work = crate::coding::DecodeWork::new(NonZeroUsize::new(updates - 1).unwrap());
            assert!(matches!(
                decoder.step(&input, &mut output, &mut work).unwrap(),
                crate::coding::DecodeProgress::Pending
            ));
            assert_eq!(work.remaining(), 0);
            let mut final_work = crate::coding::DecodeWork::new(NonZeroUsize::MIN);
            assert!(matches!(
                decoder.step(&input, &mut output, &mut final_work).unwrap(),
                crate::coding::DecodeProgress::Complete(_)
            ));
            assert_eq!(final_work.remaining(), 0);
        }
    }
}
fn budget() -> NonZeroUsize {
    NonZeroUsize::new(200_000).unwrap()
}
fn payload(length: usize, mut seed: u32) -> Vec<Bit> {
    (0..length)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            Bit::from(seed & 1 != 0)
        })
        .collect()
}
fn soft(bits: &[Bit]) -> Vec<f32> {
    bits.iter()
        .map(|bit| if *bit == Bit::Zero { 1.0 } else { -1.0 })
        .collect()
}

#[test]
fn literal_forty_bit_interleaver_checks_row_direction_and_full_matrix_swap() {
    assert_eq!(
        TurboInterleaver::new(40).unwrap().source(),
        &[
            39, 25, 17, 9, 1, 35, 27, 21, 11, 5, 34, 26, 20, 10, 4, 38, 30, 22, 14, 6, 36, 28, 18,
            12, 2, 37, 29, 19, 13, 3, 32, 24, 16, 8, 0, 33, 31, 23, 15, 7,
        ]
    );
}

#[test]
fn every_legal_interleaver_length_is_a_bijection_and_every_root_spans_its_field() {
    for &(prime, root) in &interleaver::PRIMES {
        let mut seen = vec![false; prime];
        let mut value = 1;
        for _ in 0..prime - 1 {
            assert!(!seen[value]);
            seen[value] = true;
            value = value * root % prime;
        }
        assert_eq!(value, 1);
        assert!(!seen[0]);
    }
    for length in MIN_BLOCK_BITS..=MAX_BLOCK_BITS {
        let plan = TurboInterleaver::new(length).unwrap();
        let mut seen = vec![false; length];
        for &index in plan.source() {
            assert!(!seen[index], "duplicate at length {length}");
            seen[index] = true;
        }
        assert!(seen.into_iter().all(|present| present));
    }
    for length in [0, 39, 5115, usize::MAX] {
        assert!(TurboInterleaver::new(length).is_err());
    }
}

fn polynomial_constituent(input: &[Bit]) -> Vec<(Bit, Bit)> {
    let mut recursion = vec![false; input.len() + TAIL_STEPS];
    let mut output = Vec::new();
    for index in 0..recursion.len() {
        let delayed = |delay| {
            index
                .checked_sub(delay)
                .is_some_and(|previous| recursion[previous])
        };
        let feedback = delayed(2) ^ delayed(3);
        let bit = input.get(index).map_or(feedback, |bit| *bit == Bit::One);
        let current = bit ^ feedback;
        let parity = current ^ delayed(1) ^ delayed(3);
        recursion[index] = current;
        output.push((Bit::from(bit), Bit::from(parity)));
    }
    assert!(recursion[input.len()..].iter().all(|bit| !bit));
    output
}

fn reference_encoded(plan: &TurboInterleaver, input: &[Bit]) -> Vec<Bit> {
    let first = polynomial_constituent(input);
    let interleaved: Vec<_> = plan.source().iter().map(|&source| input[source]).collect();
    let second = polynomial_constituent(&interleaved);
    let mut output = Vec::new();
    for index in 0..input.len() {
        output.extend([input[index], first[index].1, second[index].1]);
    }
    for coded in [&first, &second] {
        for &(systematic, parity) in &coded[input.len()..] {
            output.extend([systematic, parity]);
        }
    }
    output
}

#[test]
fn encoder_matches_direct_polynomial_recurrences_with_distinct_terminated_tails() {
    for length in [
        40, 41, 159, 160, 200, 201, 480, 481, 530, 531, 2280, 2281, 2480, 2481, 3160, 3161, 3210,
        3211, 5114,
    ] {
        let plan = TurboInterleaver::new(length).unwrap();
        let input = payload(length, 0x1234 + length as u32);
        let mut actual = vec![Bit::Zero; coded_bits(length).unwrap()];
        encode(&plan, &input, &mut actual).unwrap();
        assert_eq!(actual, reference_encoded(&plan, &input));
        let mut output = vec![Bit::Zero; length];
        TurboDecoder::new(Arc::new(plan), iterations(6))
            .unwrap()
            .decode(&soft(&actual), &mut output)
            .unwrap();
        assert_eq!(output, input, "length {length}");
    }
}

#[test]
fn grouped_encoder_matches_polynomials_at_every_remainder_and_large_hsdpa_width() {
    for length in (MIN_BLOCK_BITS..MIN_BLOCK_BITS + ENCODE_GROUP_BITS).chain([
        4608,
        4691,
        4825,
        4912,
        5001,
        5091,
        MAX_BLOCK_BITS,
    ]) {
        let plan = TurboInterleaver::new(length).unwrap();
        for input in [
            vec![Bit::Zero; length],
            vec![Bit::One; length],
            payload(length, 0x1234 + length as u32),
        ] {
            let mut actual = vec![Bit::One; coded_bits(length).unwrap()];
            encode(&plan, &input, &mut actual).unwrap();
            assert_eq!(actual, reference_encoded(&plan, &input), "length={length}");
        }
    }
}

#[test]
fn constituent_max_log_posterior_matches_exhaustive_terminated_word_metrics() {
    let length = 5;
    let systematic: Vec<_> = (0..length + TAIL_STEPS)
        .map(|index| (index * 17 % 11) as f64 - 4.7)
        .collect();
    let parity: Vec<_> = (0..length + TAIL_STEPS)
        .map(|index| (index * 13 % 17) as f64 - 7.3)
        .collect();
    let prior: Vec<_> = (0..length).map(|index| index as f64 * 0.47 - 0.9).collect();
    let mut expected = [[f64::NEG_INFINITY; 2]; 5];
    for word in 0..1 << length {
        let bits: Vec<_> = (0..length)
            .map(|index| Bit::from(word & (1 << index) != 0))
            .collect();
        let encoded = polynomial_constituent(&bits);
        let metric: f64 = encoded
            .iter()
            .enumerate()
            .map(|(index, &(bit, parity_bit))| {
                let evidence = systematic[index] + prior.get(index).copied().unwrap_or(0.0);
                (if bit == Bit::Zero {
                    evidence
                } else {
                    -evidence
                } + if parity_bit == Bit::Zero {
                    parity[index]
                } else {
                    -parity[index]
                }) / 2.0
            })
            .sum();
        for (index, &bit) in bits.iter().enumerate() {
            expected[index][bit as usize] = expected[index][bit as usize].max(metric);
        }
    }
    let mut posterior = [0.0; 5];
    constituent(
        &systematic,
        &parity,
        &prior,
        &mut posterior,
        &mut vec![[0.0; STATES]; length + TAIL_STEPS + 1],
    );
    for (actual, expected) in posterior.into_iter().zip(expected) {
        assert!((actual - (expected[0] - expected[1])).abs() < 1e-10);
    }
}

#[test]
fn incremental_trellis_matches_full_scalar_posteriors_across_long_noisy_words() {
    for length in [40, 530, MAX_BLOCK_BITS] {
        for gain in [0.001, 1.0, 1000.0] {
            let plan = Arc::new(TurboInterleaver::new(length).unwrap());
            let input: Vec<_> = (0..coded_bits(length).unwrap())
                .map(|index| ((index * 17 % 101) as f32 - 50.0) * gain)
                .collect();
            let mut reference = TurboDecoder::new(Arc::clone(&plan), iterations(8)).unwrap();
            reference.begin(&input).unwrap();
            for _ in 0..reference.iterations.get() {
                constituent(
                    &reference.systematic[0][..length + TAIL_STEPS],
                    &reference.parity[0][..length + TAIL_STEPS],
                    &reference.prior[0][..length],
                    &mut reference.posterior[0][..length],
                    &mut reference.forward[..length + TAIL_STEPS + 1],
                );
                for (index, &source) in plan.source().iter().enumerate() {
                    reference.prior[1][index] = reference.posterior[0][source]
                        - reference.systematic[0][source]
                        - reference.prior[0][source];
                }
                constituent(
                    &reference.systematic[1][..length + TAIL_STEPS],
                    &reference.parity[1][..length + TAIL_STEPS],
                    &reference.prior[1][..length],
                    &mut reference.posterior[1][..length],
                    &mut reference.forward[..length + TAIL_STEPS + 1],
                );
                for (index, &source) in plan.source().iter().enumerate() {
                    reference.prior[0][source] = reference.posterior[1][index]
                        - reference.systematic[1][index]
                        - reference.prior[1][index];
                }
            }
            let mut decoder = TurboDecoder::new(Arc::clone(&plan), iterations(8)).unwrap();
            decoder.begin(&input).unwrap();
            let mut output = vec![Bit::Zero; length];
            let mut visits = 0;
            loop {
                let mut work = crate::coding::DecodeWork::new(NonZeroUsize::new(257).unwrap());
                let done = decoder.step(&mut output, &mut work).unwrap();
                visits += 1;
                assert!(visits < 100_000);
                if done {
                    break;
                }
            }
            assert!(visits > 1);
            assert_eq!(decoder.posterior, reference.posterior);
            assert_eq!(decoder.prior, reference.prior);
            for (mut actual, expected) in decoder.forward.iter().copied().zip(&reference.forward) {
                normalize(&mut actual);
                assert_eq!(&actual, expected);
            }
            for (index, &source) in plan.source().iter().enumerate() {
                assert_eq!(
                    output[source],
                    Bit::from(reference.posterior[1][index] < 0.0)
                );
            }
        }
    }
}

#[test]
fn shared_plan_has_independent_decoder_histories_across_gain_and_call_order() {
    let plan = Arc::new(TurboInterleaver::new(530).unwrap());
    let handles: Vec<_> = (1..=3)
        .map(|seed| {
            let plan = Arc::clone(&plan);
            std::thread::spawn(move || {
                let mut decoder = TurboDecoder::new(Arc::clone(&plan), iterations(6)).unwrap();
                for gain in [f32::MIN_POSITIVE, 1e-12, 1.0, 1e20, f32::MAX / 8.0] {
                    let input = payload(plan.len(), seed);
                    let coded = reference_encoded(&plan, &input);
                    let evidence: Vec<_> = soft(&coded)
                        .iter()
                        .enumerate()
                        .map(|(index, value)| if index % 13 == 0 { 0.0 } else { value * gain })
                        .collect();
                    let mut actual = vec![Bit::Zero; plan.len()];
                    decoder.decode(&evidence, &mut actual).unwrap();
                    assert_eq!(actual, input);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
}

#[test]
fn iteration_boundaries_preserve_continuation_and_reject_target_changes() {
    let plan = Arc::new(TurboInterleaver::new(530).unwrap());
    let evidence: Vec<_> = (0..coded_bits(plan.len()).unwrap())
        .map(|index| ((index * 17 % 101) as f32 - 50.0) * 0.1)
        .collect();
    let mut direct = TurboDecoder::new(plan.clone(), iterations(4)).unwrap();
    let mut resumed = TurboDecoder::new(plan.clone(), iterations(4)).unwrap();
    let mut expected = vec![Bit::Zero; plan.len()];
    let mut actual = expected.clone();
    direct.decode(&evidence, &mut expected).unwrap();
    resumed.begin(&evidence).unwrap();
    for target in 1..=4 {
        let mut work = crate::coding::DecodeWork::new(std::num::NonZeroUsize::new(257).unwrap());
        assert!(
            !resumed
                .step_to_iteration(&mut actual, &mut work, target)
                .unwrap()
        );
        let mut unlimited = crate::coding::DecodeWork::new(std::num::NonZeroUsize::MAX);
        assert!(
            resumed
                .step_to_iteration(&mut actual, &mut unlimited, target + 1)
                .is_err()
        );
        assert!(
            resumed
                .step_to_iteration(&mut actual, &mut unlimited, target)
                .unwrap()
        );
        if target < 4 {
            assert!(
                resumed
                    .step_to_iteration(&mut actual, &mut unlimited, target)
                    .is_err()
            );
        }
    }
    assert_eq!(actual, expected);
    assert_eq!(resumed.posterior, direct.posterior);
    assert_eq!(resumed.prior, direct.prior);
    assert!(resumed.progress.is_none());
}

#[test]
fn turbo_rate_matching_literal_patterns_keep_systematic_bits_and_distinguish_fixed_positions() {
    let input: Vec<_> = (0..12).collect();
    for plan in [
        RateMatching::downlink_turbo_flexible(12, 9, budget()).unwrap(),
        RateMatching::uplink_turbo(12, 9, Rel99Tti::Ms10, 0, budget()).unwrap(),
    ] {
        let mut output = [0; 9];
        plan.apply(&input, &mut output).unwrap();
        assert_eq!(output, [0, 2, 3, 4, 5, 6, 8, 9, 10]);
        let mut combined = [1.0; 12];
        plan.combine_llrs(&[2.0; 9], &mut combined).unwrap();
        assert_eq!(
            combined,
            [2.0, 0.0, 2.0, 2.0, 2.0, 2.0, 2.0, 0.0, 2.0, 2.0, 2.0, 0.0]
        );
    }
    let fixed = RateMatching::downlink_turbo_fixed(6, 12, -3, budget()).unwrap();
    let mut output = [0; 5];
    fixed.apply(&input[..6], &mut output).unwrap();
    assert_eq!(output, [0, 2, 3, 4, 5]);
    let repeated = RateMatching::downlink_turbo_flexible(12, 15, budget()).unwrap();
    let mut output = [0; 15];
    repeated.apply(&input, &mut output).unwrap();
    assert_eq!(output, [0, 0, 1, 2, 3, 4, 4, 5, 6, 7, 8, 8, 9, 10, 11]);
}

#[test]
fn uplink_puncturing_preserves_systematic_bits_after_first_interleaving_for_every_tti_phase() {
    for length in [40, 41, 159, 200, 481, 530, 531, 5114] {
        let coded = coded_bits(length).unwrap();
        for interval in [
            Rel99Tti::Ms10,
            Rel99Tti::Ms20,
            Rel99Tti::Ms40,
            Rel99Tti::Ms80,
        ] {
            let frame_bits = coded.div_ceil(interval.frames());
            let total = frame_bits * interval.frames();
            let input: Vec<_> = (0..total).collect();
            let mut interleaved = vec![0; total];
            Interleaver::first(interval, total)
                .unwrap()
                .interleave(&input, &mut interleaved)
                .unwrap();
            for (frame, input) in interleaved.chunks_exact(frame_bits).enumerate() {
                let output_bits = frame_bits - frame_bits / 3;
                let plan =
                    RateMatching::uplink_turbo(frame_bits, output_bits, interval, frame, budget())
                        .unwrap();
                let mut output = vec![0; output_bits];
                plan.apply(input, &mut output).unwrap();
                for &source in input {
                    if source < 3 * length && source.is_multiple_of(3) {
                        assert!(
                            output.contains(&source),
                            "systematic bit {source}, {interval:?}, frame {frame}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn transport_segmentation_minimum_filler_and_maximum_block_boundaries_are_explicit() {
    for (bits, count, width, filler, coded) in [
        (0, 0, 0, 0, 0),
        (1, 1, 40, 39, 132),
        (39, 1, 40, 1, 132),
        (40, 1, 40, 0, 132),
        (5114, 1, 5114, 0, 15354),
        (5115, 2, 2558, 1, 15372),
        (10229, 3, 3410, 1, 30726),
    ] {
        let plan = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: bits.max(1),
                blocks: usize::from(bits != 0),
                crc: TransportCrc::None,
            },
            budget(),
        )
        .unwrap();
        assert_eq!(
            (
                plan.code_block_count(),
                plan.code_block_bits(),
                plan.filler_bits(),
                plan.coded_bits()
            ),
            (count, width, filler, coded)
        );
        let input = payload(bits, 0xcafe);
        let mut actual = vec![Bit::Zero; coded];
        TurboBlockEncoder::new(plan)
            .unwrap()
            .encode(&input, &mut actual)
            .unwrap();
        let mut output = vec![Bit::Zero; bits];
        let outcome = TurboBlockDecoder::new(plan, iterations(6))
            .unwrap()
            .decode(&soft(&actual), &mut output)
            .unwrap();
        assert_eq!(
            outcome,
            if bits == 0 {
                BlockOutcome::Absent
            } else {
                BlockOutcome::Decoded
            }
        );
        assert_eq!(input, output);
    }
}

#[test]
fn every_crc_survives_punctured_repeated_and_noisy_transport_evidence() {
    for crc in [
        TransportCrc::None,
        TransportCrc::Crc8,
        TransportCrc::Crc12,
        TransportCrc::Crc16,
        TransportCrc::Crc24,
    ] {
        let plan = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: 211,
                blocks: 3,
                crc,
            },
            budget(),
        )
        .unwrap();
        let input = payload(plan.payload_bits(), 0x4567);
        let mut coded = vec![Bit::Zero; plan.coded_bits()];
        TurboBlockEncoder::new(plan)
            .unwrap()
            .encode(&input, &mut coded)
            .unwrap();
        let mut decoder = TurboBlockDecoder::new(plan, iterations(8)).unwrap();
        for output_bits in [coded.len() * 2 / 3, coded.len(), coded.len() + 37] {
            let mapping =
                RateMatching::downlink_turbo_flexible(coded.len(), output_bits, budget()).unwrap();
            let mut matched = vec![Bit::Zero; output_bits];
            mapping.apply(&coded, &mut matched).unwrap();
            let evidence: Vec<_> = soft(&matched)
                .iter()
                .enumerate()
                .map(|(index, bit)| bit + (index * 37 % 19) as f32 / 16.0 - 0.6)
                .collect();
            let mut restored = vec![0.0; coded.len()];
            mapping.combine_llrs(&evidence, &mut restored).unwrap();
            let mut output = vec![Bit::Zero; input.len()];
            assert_eq!(
                decoder.decode(&restored, &mut output),
                Ok(BlockOutcome::Decoded)
            );
            assert_eq!(output, input);
        }
    }
}

#[test]
fn uplink_first_interleaving_frame_puncturing_and_turbo_crc_decode_are_connected() {
    let plan = TurboBlockPlan::new(
        TurboBlockFormat {
            block_bits: 6300,
            blocks: 1,
            crc: TransportCrc::Crc24,
        },
        budget(),
    )
    .unwrap();
    let input = payload(plan.payload_bits(), 0x9876);
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    TurboBlockEncoder::new(plan)
        .unwrap()
        .encode(&input, &mut coded)
        .unwrap();
    let mut decoder = TurboBlockDecoder::new(plan, iterations(8)).unwrap();
    for interval in [Rel99Tti::Ms10, Rel99Tti::Ms20, Rel99Tti::Ms40] {
        let width = coded.len().div_ceil(interval.frames());
        let mut equalized = coded.clone();
        equalized.resize(width * interval.frames(), Bit::One);
        let first = Interleaver::first(interval, equalized.len()).unwrap();
        let mut frames = vec![Bit::Zero; equalized.len()];
        first.interleave(&equalized, &mut frames).unwrap();
        let mut restored = vec![0.0; frames.len()];
        for (frame, (input, restored)) in frames
            .chunks_exact(width)
            .zip(restored.chunks_exact_mut(width))
            .enumerate()
        {
            let output_bits = width - width / 4 + frame;
            let mapping =
                RateMatching::uplink_turbo(width, output_bits, interval, frame, budget()).unwrap();
            let mut matched = vec![Bit::Zero; output_bits];
            mapping.apply(input, &mut matched).unwrap();
            mapping.combine_llrs(&soft(&matched), restored).unwrap();
        }
        let mut deinterleaved = vec![0.0; frames.len()];
        first.deinterleave(&restored, &mut deinterleaved).unwrap();
        let mut output = vec![Bit::Zero; input.len()];
        assert_eq!(
            decoder.decode(&deinterleaved[..coded.len()], &mut output),
            Ok(BlockOutcome::Decoded)
        );
        assert_eq!(input, output);
    }
}

#[test]
fn invalid_crc_filler_erasure_lengths_and_nonfinite_input_never_publish_payload() {
    let format = TurboBlockFormat {
        block_bits: 40,
        blocks: 2,
        crc: TransportCrc::Crc16,
    };
    let plan = TurboBlockPlan::new(format, budget()).unwrap();
    let mut joined = Vec::new();
    for block in payload(80, 0x5555).chunks_exact(40) {
        joined.extend_from_slice(block);
        let mut crc = [Bit::Zero; 16];
        format.crc.write(block, &mut crc).unwrap();
        joined.extend(crc);
    }
    let last = joined.last_mut().unwrap();
    *last = Bit::from(*last == Bit::Zero);
    let interleaver = TurboInterleaver::new(joined.len()).unwrap();
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    encode(&interleaver, &joined, &mut coded).unwrap();
    let mut output = [Bit::One; 80];
    let mut decoder = TurboBlockDecoder::new(plan, iterations(6)).unwrap();
    assert_eq!(
        decoder.decode(&soft(&coded), &mut output),
        Ok(BlockOutcome::InvalidCrc { block: 1 })
    );
    assert_eq!(
        decoder.decode(&vec![0.0; coded.len()], &mut output),
        Ok(BlockOutcome::Erased)
    );
    assert_eq!(
        decoder.decode(&vec![f32::NAN; coded.len()], &mut output),
        Err(DspError::NonFiniteEvidence)
    );
    assert_eq!(
        decoder.decode(&[], &mut output),
        Err(DspError::InvalidLength)
    );
    assert_eq!(output, [Bit::One; 80]);
    let filler_plan = TurboBlockPlan::new(
        TurboBlockFormat {
            block_bits: 1,
            blocks: 1,
            crc: TransportCrc::None,
        },
        budget(),
    )
    .unwrap();
    let mut invalid_filler = [Bit::Zero; 40];
    invalid_filler[0] = Bit::One;
    let mut coded = vec![Bit::Zero; filler_plan.coded_bits()];
    encode(
        &TurboInterleaver::new(40).unwrap(),
        &invalid_filler,
        &mut coded,
    )
    .unwrap();
    let mut output = [Bit::One];
    assert_eq!(
        TurboBlockDecoder::new(filler_plan, iterations(6))
            .unwrap()
            .decode(&soft(&coded), &mut output),
        Ok(BlockOutcome::InvalidFiller)
    );
    assert_eq!(output, [Bit::One]);
    assert!(TurboBlockPlan::new(format, NonZeroUsize::new(1).unwrap()).is_err());
    assert!(
        TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: usize::MAX,
                ..format
            },
            budget()
        )
        .is_err()
    );
    assert!(TurboDecoder::new(Arc::new(interleaver), iterations(33)).is_err());
    assert!(RateMatching::downlink_turbo_flexible(132, 43, budget()).is_err());
    assert!(RateMatching::uplink_turbo(132, 43, Rel99Tti::Ms10, 0, budget()).is_err());
    assert!(RateMatching::downlink_turbo_fixed(132, 132, -89, budget()).is_err());
    assert!(RateMatching::downlink_turbo_flexible(131, 130, budget()).is_err());
    assert!(RateMatching::uplink_turbo(132, 100, Rel99Tti::Ms20, 2, budget()).is_err());
}
