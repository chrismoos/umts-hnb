// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn plan(bits: usize) -> TurboBlockPlan {
    TurboBlockPlan::new(
        TurboBlockFormat {
            block_bits: bits,
            blocks: 1,
            crc: TransportCrc::Crc24,
        },
        NonZeroUsize::new(100_000).unwrap(),
    )
    .unwrap()
}

#[test]
fn crc_zero_word_requires_observed_information_but_allows_known_filler() {
    let iterations = NonZeroU8::new(4).unwrap();
    let format = plan(120);
    let mut decoder = Crc24TurboDecoder::new(&[format], iterations).unwrap();
    let mut ambiguous = vec![0.0; format.coded];
    ambiguous[1] = 4.0;
    // A trellis step consumes eight state updates atomically.
    for quota in [8, 17, 257] {
        let mut output = vec![Bit::One; format.payload_bits()];
        assert_eq!(
            decoder.begin(&ambiguous, &mut output).unwrap(),
            DecodeProgress::Pending
        );
        let mut consumed = 0;
        loop {
            let mut work = DecodeWork::new(NonZeroUsize::new(quota).unwrap());
            let progress = decoder.step(&ambiguous, &mut output, &mut work).unwrap();
            consumed += quota - work.remaining();
            assert!(output.iter().all(|bit| *bit == Bit::One));
            assert!(consumed <= format.crc_round_state_updates(iterations).unwrap());
            if progress != DecodeProgress::Pending {
                assert_eq!(progress, DecodeProgress::Complete(BlockOutcome::Erased));
                break;
            }
        }
        // A genuinely observed zero payload remains valid after reusing storage.
        let strong = vec![4.0; format.coded];
        assert_eq!(
            decoder.decode(&strong, &mut output).unwrap(),
            BlockOutcome::Decoded
        );
        assert!(output.iter().all(|bit| *bit == Bit::Zero));
        let mut parity_only = strong;
        for index in 0..format.width {
            parity_only[3 * index] = 0.0;
        }
        assert_eq!(
            decoder.decode(&parity_only, &mut output).unwrap(),
            BlockOutcome::Decoded
        );
        assert!(output.iter().all(|bit| *bit == Bit::Zero));
    }
    let padded = plan(1);
    assert!(padded.filler > 0);
    let mut evidence = vec![0.0; padded.coded];
    for index in padded.filler..padded.width {
        evidence[3 * index] = 4.0;
    }
    let mut decoder = Crc24TurboDecoder::new(&[padded], iterations).unwrap();
    let mut output = [Bit::One];
    assert_eq!(
        decoder.decode(&evidence, &mut output).unwrap(),
        BlockOutcome::Decoded
    );
    assert_eq!(output, [Bit::Zero]);
    assert!(decoder.decoders[0].unobserved_until > 0);
    assert!(decoder.decoders[0].unobserved_until <= padded.filler);
}

#[test]
fn systematic_crc_byte_boundaries_preserve_work_and_atomic_output() {
    for bits in (120..128).chain([5567, 11484]) {
        let plan = plan(bits);
        let payload: Vec<_> = (0..bits)
            .map(|i| Bit::from((i * 17 + i / 11) % 7 < 3))
            .collect();
        let mut coded = vec![Bit::Zero; plan.coded_bits()];
        TurboBlockEncoder::new(plan)
            .unwrap()
            .encode(&payload, &mut coded)
            .unwrap();
        let evidence: Vec<_> = coded
            .iter()
            .map(|b| if *b == Bit::Zero { 4.0 } else { -4.0 })
            .collect();
        for quota in [1, 2, 3, 7, 8, 9, 257] {
            let mut decoder = Crc24TurboDecoder::new(&[plan], NonZeroU8::new(4).unwrap()).unwrap();
            let mut output = vec![Bit::One; bits];
            decoder.begin(&evidence, &mut output).unwrap();
            let mut consumed = 0;
            loop {
                let mut work = DecodeWork::new(NonZeroUsize::new(quota).unwrap());
                let result = decoder.step(&evidence, &mut output, &mut work).unwrap();
                consumed += quota - work.remaining();
                if result == DecodeProgress::Complete(BlockOutcome::Decoded) {
                    break;
                }
                assert_eq!(result, DecodeProgress::Pending);
                assert!(output.iter().all(|bit| *bit == Bit::One));
                assert!(matches!(decoder.progress, Some(Progress::Systematic(_))));
            }
            assert_eq!(consumed, plan.width * plan.count);
            assert_eq!(output, payload);
        }
    }
}

#[test]
fn complete_crc_stops_early_across_segments_and_storage_is_reusable() {
    let plans = [plan(120), plan(5567), plan(11484)];
    let iterations = NonZeroU8::new(4).unwrap();
    let mut decoder = Crc24TurboDecoder::new(&plans, iterations).unwrap();
    let retained = decoder.heap_bytes();
    for plan in plans.into_iter().chain(plans) {
        let payload: Vec<_> = (0..plan.payload_bits())
            .map(|i| Bit::from((i * 17 + i / 11) % 7 < 3))
            .collect();
        let mut coded = vec![Bit::Zero; plan.coded_bits()];
        TurboBlockEncoder::new(plan)
            .unwrap()
            .encode(&payload, &mut coded)
            .unwrap();
        let evidence: Vec<_> = coded
            .iter()
            .map(|b| if *b == Bit::Zero { 4.0 } else { -4.0 })
            .collect();
        decoder
            .configure(plan, plan.interleaver().unwrap().as_ref())
            .unwrap();
        let mut output = vec![Bit::One; payload.len()];
        assert_eq!(
            decoder.begin(&evidence, &mut output).unwrap(),
            DecodeProgress::Pending
        );
        let mut consumed = 0;
        loop {
            let quota = 257;
            let mut work = DecodeWork::new(NonZeroUsize::new(quota).unwrap());
            let result = decoder.step(&evidence, &mut output, &mut work).unwrap();
            consumed += quota - work.remaining();
            if result == DecodeProgress::Complete(BlockOutcome::Decoded) {
                break;
            }
            assert_eq!(result, DecodeProgress::Pending);
            assert!(output.iter().all(|b| *b == Bit::One));
            assert!(consumed <= plan.crc_round_state_updates(iterations).unwrap());
        }
        assert_eq!(output, payload);
        assert_eq!(consumed, plan.width * plan.count);
        assert!(
            decoder
                .decoders
                .iter()
                .all(|decoder| decoder.progress.is_none())
        );
        assert!(consumed < plan.decode_state_updates(iterations).unwrap() / 2);
        assert_eq!(decoder.heap_bytes(), retained);
    }
}

#[test]
fn observed_systematic_word_needs_complete_crc_and_no_parity() {
    let plan = plan(5567);
    let payload: Vec<_> = (0..plan.payload_bits())
        .map(|i| Bit::from(i % 3 == 0))
        .collect();
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    TurboBlockEncoder::new(plan)
        .unwrap()
        .encode(&payload, &mut coded)
        .unwrap();
    let width = coded_bits(plan.width).unwrap();
    let evidence: Vec<_> = coded
        .iter()
        .enumerate()
        .map(|(index, bit)| {
            let within = index % width;
            if within < CODE_RATE * plan.width && within.is_multiple_of(CODE_RATE) {
                if *bit == Bit::Zero { 4.0 } else { -4.0 }
            } else {
                0.0
            }
        })
        .collect();
    let mut decoder = Crc24TurboDecoder::new(&[plan], NonZeroU8::new(4).unwrap()).unwrap();
    let mut output = vec![Bit::One; payload.len()];
    decoder.begin(&evidence, &mut output).unwrap();
    let mut work = DecodeWork::new(NonZeroUsize::new(plan.width * plan.count - 1).unwrap());
    assert_eq!(
        decoder.step(&evidence, &mut output, &mut work).unwrap(),
        DecodeProgress::Pending
    );
    assert!(output.iter().all(|bit| *bit == Bit::One));
    let mut work = DecodeWork::new(NonZeroUsize::MIN);
    assert_eq!(
        decoder.step(&evidence, &mut output, &mut work).unwrap(),
        DecodeProgress::Complete(BlockOutcome::Decoded)
    );
    assert_eq!(output, payload);
    assert!(
        decoder
            .decoders
            .iter()
            .all(|decoder| decoder.progress.is_none())
    );
}

#[test]
fn missing_or_wrong_systematic_evidence_falls_back_without_partial_publication() {
    let plan = plan(5567);
    let payload: Vec<_> = (0..plan.payload_bits())
        .map(|i| Bit::from(i % 3 == 0))
        .collect();
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    TurboBlockEncoder::new(plan)
        .unwrap()
        .encode(&payload, &mut coded)
        .unwrap();
    let baseline: Vec<_> = coded
        .iter()
        .map(|bit| if *bit == Bit::Zero { 4.0 } else { -4.0 })
        .collect();
    for (index, erase) in [
        (0, true),
        (0, false),
        (CODE_RATE * (plan.filler + 7), true),
        (CODE_RATE * (plan.filler + 7), false),
    ] {
        let mut evidence = baseline.clone();
        evidence[index] = if erase { 0.0 } else { -evidence[index] };
        let mut decoder = Crc24TurboDecoder::new(&[plan], NonZeroU8::new(4).unwrap()).unwrap();
        let mut output = vec![Bit::One; payload.len()];
        decoder.begin(&evidence, &mut output).unwrap();
        let mut work = DecodeWork::new(NonZeroUsize::new(plan.width * plan.count).unwrap());
        assert_eq!(
            decoder.step(&evidence, &mut output, &mut work).unwrap(),
            DecodeProgress::Pending
        );
        assert!(matches!(decoder.progress, Some(Progress::Iterative(_))));
        assert!(output.iter().all(|bit| *bit == Bit::One));
        let mut work = DecodeWork::new(NonZeroUsize::MAX);
        assert_eq!(
            decoder.step(&evidence, &mut output, &mut work).unwrap(),
            DecodeProgress::Complete(BlockOutcome::Decoded)
        );
        assert_eq!(output, payload);
    }
}

#[test]
fn invalid_crc_runs_to_limit_without_publication_and_cancel_releases_all_segments() {
    let plan = plan(5567);
    let iterations = NonZeroU8::new(4).unwrap();
    let mut decoder = Crc24TurboDecoder::new(&[plan], iterations).unwrap();
    let interleaver = plan.interleaver().unwrap().unwrap();
    let mut joined = vec![Bit::Zero; plan.layout.joined];
    joined[0] = Bit::One;
    let mut coded = vec![Bit::Zero; plan.coded_bits()];
    let width = coded_bits(plan.width).unwrap();
    let mut offset = 0;
    for (index, coded) in coded.chunks_exact_mut(width).enumerate() {
        let filler = if index == 0 { plan.filler } else { 0 };
        let mut block = vec![Bit::Zero; plan.width];
        let count = plan.width - filler;
        block[filler..].copy_from_slice(&joined[offset..offset + count]);
        encode(&interleaver, &block, coded).unwrap();
        offset += count;
    }
    let evidence: Vec<_> = coded
        .iter()
        .map(|b| if *b == Bit::Zero { 4.0 } else { -4.0 })
        .collect();
    let mut output = vec![Bit::One; plan.payload_bits()];
    decoder.begin(&evidence, &mut output).unwrap();
    let budget = plan.crc_round_state_updates(iterations).unwrap();
    let mut work = DecodeWork::new(NonZeroUsize::new(budget).unwrap());
    assert_eq!(
        decoder.step(&evidence, &mut output, &mut work).unwrap(),
        DecodeProgress::Complete(BlockOutcome::InvalidCrc { block: 0 })
    );
    assert_eq!(work.remaining(), 0);
    assert!(output.iter().all(|b| *b == Bit::One));
    decoder.begin(&evidence, &mut output).unwrap();
    decoder.cancel();
    decoder.configure(plan, Some(&interleaver)).unwrap();
    assert_eq!(
        decoder.decode(&evidence, &mut output).unwrap(),
        BlockOutcome::InvalidCrc { block: 0 }
    );
}
