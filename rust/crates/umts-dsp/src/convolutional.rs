// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, bits::Bit};
use wide::f64x4;

pub(crate) const MEMORY_BITS: usize = 8;
const STATES: usize = 1 << MEMORY_BITS;
const TRACE_WORDS: usize = STATES / u64::BITS as usize;
pub(crate) const MAX_INFORMATION_BITS: usize = 504;
const MAX_STEPS: usize = MAX_INFORMATION_BITS + MEMORY_BITS;

#[derive(Clone, Copy, Debug)]
pub enum CodeRate {
    Half,
    Third,
}

impl CodeRate {
    pub fn denominator(self) -> usize {
        self.generators().len()
    }

    fn generators(self) -> &'static [u16] {
        // TS 25.212 4.2.3.1 figure 3, current input at register bit 8.
        match self {
            Self::Half => &[0o561, 0o753],
            Self::Third => &[0o557, 0o663, 0o711],
        }
    }
}

pub fn encode(rate: CodeRate, input: &[Bit], output: &mut [Bit]) -> Result<(), DspError> {
    if input.len() > MAX_INFORMATION_BITS
        || output.len() != (input.len() + MEMORY_BITS) * rate.denominator()
    {
        return Err(DspError::InvalidLength);
    }
    let mut memory = 0_u16;
    for (step, coded) in output.chunks_exact_mut(rate.denominator()).enumerate() {
        let bit = input.get(step).copied().unwrap_or(Bit::Zero);
        let register = memory | ((bit as u16) << MEMORY_BITS);
        for (out, &generator) in coded.iter_mut().zip(rate.generators()) {
            *out = Bit::from(!(register & generator).count_ones().is_multiple_of(2));
        }
        memory = register >> 1;
    }
    Ok(())
}

pub struct TerminatedDecoder {
    rate: CodeRate,
    metrics: [f64; STATES],
    next: [f64; STATES],
    trace: Vec<u64>,
    // A bit records two finite, equal-cost predecessors at the same trace node.
    ties: Vec<u64>,
    filler: usize,
    unambiguous: bool,
    patterns: [[u8; 2]; STATES],
    progress: Option<DecodePosition>,
}

enum DecodePosition {
    Forward(usize),
    Traceback {
        next: usize,
        state: usize,
        tied: bool,
    },
    Qualify {
        next: usize,
        same: [u64; TRACE_WORDS],
        different: [u64; TRACE_WORDS],
    },
}

impl TerminatedDecoder {
    pub(crate) fn cancel(&mut self) {
        self.progress = None;
    }
    pub fn new(rate: CodeRate) -> Self {
        let patterns = std::array::from_fn(|state| {
            std::array::from_fn(|high| {
                let previous = ((state << 1) & (STATES - 1)) | high;
                let register = previous | ((state >> (MEMORY_BITS - 1)) << MEMORY_BITS);
                rate.generators()
                    .iter()
                    .enumerate()
                    .fold(0, |mask, (g, &generator)| {
                        mask | ((((register as u16 & generator).count_ones() & 1) as u8) << g)
                    })
            })
        });
        Self {
            rate,
            metrics: [0.0; STATES],
            next: [0.0; STATES],
            trace: vec![0; MAX_STEPS * TRACE_WORDS],
            ties: vec![0; MAX_STEPS * TRACE_WORDS],
            filler: 0,
            unambiguous: false,
            patterns,
            progress: None,
        }
    }

    /// Positive input is log(P(bit=0)/P(bit=1)). Returns information bits without the eight zero tails.
    pub fn decode(&mut self, llrs: &[f32], output: &mut [Bit]) -> Result<(), DspError> {
        self.begin(llrs, output.len())?;
        let mut work = crate::coding::DecodeWork::new(std::num::NonZeroUsize::MAX);
        if !self.step(llrs, output, &mut work)? {
            return Err(DspError::Capacity);
        }
        Ok(())
    }
    pub(crate) fn begin(&mut self, llrs: &[f32], information: usize) -> Result<(), DspError> {
        let rate = self.rate.denominator();
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if information > MAX_INFORMATION_BITS || llrs.len() != (information + MEMORY_BITS) * rate {
            return Err(DspError::InvalidLength);
        }
        if llrs.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        self.metrics.fill(f64::INFINITY);
        self.metrics[0] = 0.0;
        self.filler = 0;
        self.unambiguous = true;
        self.progress = Some(DecodePosition::Forward(0));
        Ok(())
    }
    pub(crate) fn known_filler(&mut self, filler: usize) {
        self.filler = filler;
    }
    pub(crate) fn unambiguous(&self) -> bool {
        self.unambiguous
    }
    /// Evidence must remain unchanged from begin until completion or cancellation.
    pub(crate) fn step(
        &mut self,
        llrs: &[f32],
        output: &mut [Bit],
        work: &mut crate::coding::DecodeWork,
    ) -> Result<bool, DspError> {
        let rate = self.rate.denominator();
        if output.len() > MAX_INFORMATION_BITS || llrs.len() != (output.len() + MEMORY_BITS) * rate
        {
            return Err(DspError::InvalidLength);
        }
        let mut progress = self.progress.take().ok_or(DspError::DiscontinuousTime)?;
        loop {
            match &mut progress {
                DecodePosition::Forward(step) => {
                    if *step == llrs.len() / rate {
                        progress = DecodePosition::Traceback {
                            next: *step,
                            state: 0,
                            tied: false,
                        };
                        continue;
                    }
                    if !work.consume(STATES) {
                        break;
                    }
                    let evidence = &llrs[*step * rate..(*step + 1) * rate];
                    let mut branch = [0.0; 8];
                    for (pattern, metric) in branch.iter_mut().enumerate().take(1 << rate) {
                        for (g, &llr) in evidence.iter().enumerate() {
                            if pattern & (1 << g) != 0 {
                                *metric += f64::from(llr);
                            }
                        }
                    }
                    let trace = &mut self.trace[*step * TRACE_WORDS..(*step + 1) * TRACE_WORDS];
                    let ties = &mut self.ties[*step * TRACE_WORDS..(*step + 1) * TRACE_WORDS];
                    for word in 0..TRACE_WORDS {
                        // Keep each packed word in registers instead of updating it per state.
                        let mut trace_word = 0_u64;
                        let mut tie_word = 0_u64;
                        for offset in (0..64).step_by(4) {
                            let state = word * 64 + offset;
                            let previous = (state << 1) & (STATES - 1);
                            let low = f64x4::new(std::array::from_fn(|lane| {
                                self.metrics[previous + 2 * lane]
                            })) + f64x4::new(std::array::from_fn(|lane| {
                                branch[self.patterns[state + lane][0] as usize]
                            }));
                            let high = f64x4::new(std::array::from_fn(|lane| {
                                self.metrics[previous + 2 * lane + 1]
                            })) + f64x4::new(std::array::from_fn(|lane| {
                                branch[self.patterns[state + lane][1] as usize]
                            }));
                            let choose_high = high.simd_lt(low);
                            self.next[state..state + 4]
                                .copy_from_slice(&choose_high.select(high, low).to_array());
                            trace_word |= u64::from(choose_high.to_bitmask()) << offset;
                            tie_word |= u64::from(
                                // Finite f32 evidence cannot overflow these bounded f64 paths.
                                // Positive infinity represents an unreachable predecessor.
                                (low.simd_eq(high) & low.simd_lt(f64x4::splat(f64::INFINITY)))
                                    .to_bitmask(),
                            ) << offset;
                        }
                        trace[word] = trace_word;
                        ties[word] = tie_word;
                    }
                    std::mem::swap(&mut self.metrics, &mut self.next);
                    *step += 1;
                }
                DecodePosition::Traceback { next, state, tied } => {
                    if *next == 0 {
                        if *tied && self.filler != 0 {
                            // Some tied paths may differ only in known-zero filler.
                            let mut same = [0; TRACE_WORDS];
                            same[0] = 1;
                            progress = DecodePosition::Qualify {
                                next: llrs.len() / rate,
                                same,
                                different: [0; TRACE_WORDS],
                            };
                            continue;
                        }
                        self.unambiguous = !*tied;
                        return Ok(true);
                    }
                    if !work.consume(1) {
                        break;
                    }
                    *next -= 1;
                    *tied |= self.ties[*next * TRACE_WORDS + *state / u64::BITS as usize]
                        & (1 << (*state % u64::BITS as usize))
                        != 0;
                    if *next < output.len() {
                        output[*next] = Bit::from(*state >> (MEMORY_BITS - 1) != 0);
                    }
                    let high = (self.trace[*next * TRACE_WORDS + *state / u64::BITS as usize]
                        >> (*state % u64::BITS as usize))
                        & 1;
                    *state = ((*state << 1) & (STATES - 1)) | high as usize;
                }
                DecodePosition::Qualify {
                    next,
                    same,
                    different,
                } => {
                    if *next == 0 {
                        self.unambiguous = different[0] & 1 == 0;
                        return Ok(true);
                    }
                    if !work.consume(STATES) {
                        break;
                    }
                    *next -= 1;
                    let mut previous_same = [0_u64; TRACE_WORDS];
                    let mut previous_different = [0_u64; TRACE_WORDS];
                    // Traverse the equal-cost survivor DAG merging paths by state and difference
                    // from the selected output.
                    for state in 0..STATES {
                        let word = state / u64::BITS as usize;
                        let mask = 1_u64 << (state % u64::BITS as usize);
                        let was_same = same[word] & mask != 0;
                        let was_different = different[word] & mask != 0;
                        if !was_same && !was_different {
                            continue;
                        }
                        let bit = Bit::from(state >> (MEMORY_BITS - 1) != 0);
                        if *next < self.filler && bit != Bit::Zero {
                            continue;
                        }
                        let differs =
                            *next >= self.filler && *next < output.len() && bit != output[*next];
                        let high = usize::from(self.trace[*next * TRACE_WORDS + word] & mask != 0);
                        let previous = ((state << 1) & (STATES - 1)) | high;
                        let tied = self.ties[*next * TRACE_WORDS + word] & mask != 0;
                        for predecessor in [Some(previous), tied.then_some(previous ^ 1)]
                            .into_iter()
                            .flatten()
                        {
                            let index = predecessor / u64::BITS as usize;
                            let mask = 1_u64 << (predecessor % u64::BITS as usize);
                            if was_same && !differs {
                                previous_same[index] |= mask;
                            }
                            if was_different || (was_same && differs) {
                                previous_different[index] |= mask;
                            }
                        }
                    }
                    *same = previous_same;
                    *different = previous_different;
                }
            }
        }
        self.progress = Some(progress);
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(input: &[Bit], rate: CodeRate) -> Vec<Bit> {
        let mut output = Vec::new();
        for step in 0..input.len() + MEMORY_BITS {
            for &generator in rate.generators() {
                let mut bit = false;
                for delay in 0..=MEMORY_BITS {
                    if step >= delay
                        && step - delay < input.len()
                        && generator & (1 << (MEMORY_BITS - delay)) != 0
                    {
                        bit ^= input[step - delay] == Bit::One;
                    }
                }
                output.push(Bit::from(bit));
            }
        }
        output
    }

    #[test]
    fn spec_polynomial_reference_checks_encoder_and_terminated_decode() {
        for rate in [CodeRate::Half, CodeRate::Third] {
            for length in [0, 1, 8, 81, 244, MAX_INFORMATION_BITS] {
                let bits: Vec<_> = (0..length)
                    .map(|i| Bit::from((i * 19 + i / 3) % 7 < 3))
                    .collect();
                let reference = reference(&bits, rate);
                let mut coded = vec![Bit::Zero; reference.len()];
                encode(rate, &bits, &mut coded).unwrap();
                assert_eq!(coded, reference);
                let llrs: Vec<_> = reference
                    .iter()
                    .enumerate()
                    .map(|(i, bit)| {
                        if i % 7 == 0 {
                            0.0
                        } else if *bit == Bit::Zero {
                            8.0
                        } else {
                            -8.0
                        }
                    })
                    .collect();
                let mut actual = vec![Bit::Zero; length];
                TerminatedDecoder::new(rate)
                    .decode(&llrs, &mut actual)
                    .unwrap();
                assert_eq!(actual, bits);
            }
        }
    }

    #[test]
    fn exhaustive_small_messages_independently_check_maximum_likelihood() {
        let mut seed = 12345_u32;
        for rate in [CodeRate::Half, CodeRate::Third] {
            let mut decoder = TerminatedDecoder::new(rate);
            for _ in 0..8 {
                let llrs: Vec<_> = (0..16 * rate.denominator())
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed % 2001) as f32 / 100.0 - 10.0
                    })
                    .collect();
                let metric = |bits: &[Bit]| -> f64 {
                    reference(bits, rate)
                        .iter()
                        .zip(&llrs)
                        .filter(|(bit, _)| **bit == Bit::One)
                        .map(|(_, llr)| f64::from(*llr))
                        .sum()
                };
                let best = (0..256)
                    .map(|word| {
                        let bits: Vec<_> =
                            (0..8).map(|i| Bit::from(word & (1 << i) != 0)).collect();
                        metric(&bits)
                    })
                    .fold(f64::INFINITY, f64::min);
                let mut output = [Bit::Zero; 8];
                decoder.decode(&llrs, &mut output).unwrap();
                assert!((metric(&output) - best).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn packed_survivors_preserve_scalar_metrics_and_every_trace_bit() {
        let mut seed = 991_u32;
        for rate in [CodeRate::Half, CodeRate::Third] {
            for length in [0, 1, 8, 81, 244, MAX_INFORMATION_BITS] {
                for mode in 0..4 {
                    let llrs: Vec<_> = (0..(length + MEMORY_BITS) * rate.denominator())
                        .map(|index| {
                            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            match mode {
                                0 => (seed % 2001) as f32 / 100.0 - 10.0,
                                1 => [0.0, -0.0, 1.0, -1.0][index % 4],
                                2 => [f32::MAX, -f32::MAX, f32::from_bits(1), -f32::from_bits(1)]
                                    [index % 4],
                                _ => 0.0,
                            }
                        })
                        .collect();
                    let mut decoder = TerminatedDecoder::new(rate);
                    let mut metrics = [f64::INFINITY; STATES];
                    metrics[0] = 0.0;
                    let mut trace = vec![0_u64; (length + MEMORY_BITS) * TRACE_WORDS];
                    let mut ties = trace.clone();
                    for (step, evidence) in llrs.chunks_exact(rate.denominator()).enumerate() {
                        let mut branch = [0.0; 8];
                        for (pattern, metric) in
                            branch.iter_mut().enumerate().take(1 << rate.denominator())
                        {
                            for (g, &llr) in evidence.iter().enumerate() {
                                if pattern & (1 << g) != 0 {
                                    *metric += f64::from(llr);
                                }
                            }
                        }
                        let mut next = [0.0; STATES];
                        for state in 0..STATES {
                            let previous = (state << 1) & (STATES - 1);
                            let low =
                                metrics[previous] + branch[decoder.patterns[state][0] as usize];
                            let high =
                                metrics[previous | 1] + branch[decoder.patterns[state][1] as usize];
                            next[state] = if high < low { high } else { low };
                            let slot = step * TRACE_WORDS + state / 64;
                            if high < low {
                                trace[slot] |= 1 << (state % 64);
                            }
                            if low == high && low.is_finite() {
                                ties[slot] |= 1 << (state % 64);
                            }
                        }
                        metrics = next;
                    }
                    let mut output = vec![Bit::One; length];
                    decoder.decode(&llrs, &mut output).unwrap();
                    assert_eq!(decoder.metrics.map(f64::to_bits), metrics.map(f64::to_bits));
                    assert_eq!(decoder.trace[..trace.len()], trace);
                    assert_eq!(decoder.ties[..ties.len()], ties);
                }
            }
        }
    }

    #[test]
    fn survivor_ambiguity_matches_exhaustive_words_with_known_filler() {
        let mut seed = 739_u32;
        for rate in [CodeRate::Half, CodeRate::Third] {
            let words: Vec<Vec<Bit>> = (0..256)
                .map(|word| (0..8).map(|i| Bit::from(word & (1 << i) != 0)).collect())
                .collect();
            let encoded: Vec<_> = words.iter().map(|bits| reference(bits, rate)).collect();
            for filler in [0, 1, 3] {
                for case in 0..64 {
                    let llrs: Vec<_> = (0..16 * rate.denominator())
                        .map(|_| {
                            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            if case == 0 {
                                0.0
                            } else {
                                (seed % 3) as f32 - 1.0
                            }
                        })
                        .collect();
                    let scores: Vec<f64> = encoded
                        .iter()
                        .map(|coded| {
                            coded
                                .iter()
                                .zip(&llrs)
                                .filter(|(bit, _)| **bit == Bit::One)
                                .map(|(_, llr)| f64::from(*llr))
                                .sum()
                        })
                        .collect();
                    let best = scores.iter().copied().fold(f64::INFINITY, f64::min);
                    let mut decoder = TerminatedDecoder::new(rate);
                    decoder.begin(&llrs, 8).unwrap();
                    decoder.known_filler(filler);
                    let mut output = [Bit::One; 8];
                    // Forward work is atomic per 256-state step.
                    let mut insufficient =
                        crate::coding::DecodeWork::new(std::num::NonZeroUsize::new(255).unwrap());
                    assert!(!decoder.step(&llrs, &mut output, &mut insufficient).unwrap());
                    let mut calls = 0;
                    loop {
                        let mut work = crate::coding::DecodeWork::new(
                            std::num::NonZeroUsize::new(257).unwrap(),
                        );
                        calls += 1;
                        assert!(calls <= 64);
                        if decoder.step(&llrs, &mut output, &mut work).unwrap() {
                            break;
                        }
                    }
                    let selected = words.iter().position(|word| word == &output).unwrap();
                    assert_eq!(scores[selected], best);
                    if output[..filler].iter().all(|bit| *bit == Bit::Zero) {
                        let valid_best = words
                            .iter()
                            .zip(&scores)
                            .filter(|(word, score)| {
                                **score == best
                                    && word[..filler].iter().all(|bit| *bit == Bit::Zero)
                            })
                            .count();
                        assert_eq!(
                            decoder.unambiguous(),
                            valid_best == 1,
                            "rate={rate:?} filler={filler} case={case}"
                        );
                    }
                }
            }
            // Erasing only a filler bit's encoded support must not erase data.
            let mut filler_word = [Bit::Zero; 8];
            filler_word[0] = Bit::One;
            let llrs: Vec<_> = reference(&filler_word, rate)
                .iter()
                .map(|bit| if *bit == Bit::One { 0.0 } else { 4.0 })
                .collect();
            for filler in [0, 1] {
                let mut decoder = TerminatedDecoder::new(rate);
                decoder.begin(&llrs, 8).unwrap();
                decoder.known_filler(filler);
                let mut output = [Bit::One; 8];
                let mut work = crate::coding::DecodeWork::new(std::num::NonZeroUsize::MAX);
                assert!(decoder.step(&llrs, &mut output, &mut work).unwrap());
                assert_eq!(output, [Bit::Zero; 8]);
                assert_eq!(decoder.unambiguous(), filler == 1);
            }
        }
    }

    #[test]
    fn bad_lengths_and_nonfinite_evidence_do_not_mutate_output() {
        let mut decoder = TerminatedDecoder::new(CodeRate::Half);
        let mut output = [Bit::One];
        assert_eq!(
            decoder.decode(&[f32::NAN; 18], &mut output),
            Err(DspError::NonFiniteEvidence)
        );
        assert_eq!(output, [Bit::One]);
        assert_eq!(
            decoder.decode(&[0.0; 17], &mut output),
            Err(DspError::InvalidLength)
        );
    }
}
