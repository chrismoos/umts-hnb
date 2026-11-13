// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, bits::Bit};
use std::{num::NonZeroU8, sync::Arc};

mod incremental;
mod interleaver;
mod transport;
pub use interleaver::TurboInterleaver;
pub(crate) use transport::Crc24TurboDecoder;
pub use transport::{TurboBlockDecoder, TurboBlockEncoder, TurboBlockFormat, TurboBlockPlan};

pub const MIN_BLOCK_BITS: usize = 40;
pub const MAX_BLOCK_BITS: usize = 5114;
const STATES: usize = 8;
const TAIL_STEPS: usize = 3;
const TAIL_CODED_BITS: usize = 12;
const CODE_RATE: usize = 3;
const ENCODE_GROUP_BITS: usize = 8;
const ENCODE_INPUTS: usize = 1 << ENCODE_GROUP_BITS;
pub(crate) const MAX_ITERATIONS: u8 = 32;

pub fn coded_bits(information: usize) -> Result<usize, DspError> {
    if !(MIN_BLOCK_BITS..=MAX_BLOCK_BITS).contains(&information) {
        return Err(DspError::InvalidLength);
    }
    Ok(CODE_RATE * information + TAIL_CODED_BITS)
}

const fn transition(state: usize, input: usize) -> (usize, usize) {
    // TS 25.212 4.2.3.2.1: feedback 1+D²+D³, parity numerator 1+D+D³.
    let feedback = (input ^ (state >> 1) ^ (state >> 2)) & 1;
    let parity = (feedback ^ state ^ (state >> 2)) & 1;
    (((state << 1) | feedback) & (STATES - 1), parity)
}

const fn encode_groups() -> [u16; STATES * ENCODE_INPUTS] {
    let mut entries = [0; STATES * ENCODE_INPUTS];
    let mut index = 0;
    while index < entries.len() {
        let mut state = index / ENCODE_INPUTS;
        let input = index % ENCODE_INPUTS;
        let mut parity = 0;
        let mut bit = 0;
        while bit < ENCODE_GROUP_BITS {
            let (next, output) = transition(state, (input >> bit) & 1);
            state = next;
            parity |= output << bit;
            bit += 1;
        }
        entries[index] = ((state << ENCODE_GROUP_BITS) | parity) as u16;
        index += 1;
    }
    entries
}

const ENCODE_GROUPS: [u16; STATES * ENCODE_INPUTS] = encode_groups();

pub fn encode(plan: &TurboInterleaver, input: &[Bit], output: &mut [Bit]) -> Result<(), DspError> {
    if input.len() != plan.len() || output.len() != coded_bits(plan.len())? {
        return Err(DspError::InvalidLength);
    }
    let mut first = 0;
    let mut second = 0;
    let grouped = input.len() / ENCODE_GROUP_BITS * ENCODE_GROUP_BITS;
    for ((input_group, source_group), output_group) in input[..grouped]
        .chunks_exact(ENCODE_GROUP_BITS)
        .zip(plan.source()[..grouped].chunks_exact(ENCODE_GROUP_BITS))
        .zip(output[..CODE_RATE * grouped].chunks_exact_mut(CODE_RATE * ENCODE_GROUP_BITS))
    {
        let mut original = 0;
        let mut interleaved = 0;
        for (offset, (&bit, &source)) in input_group.iter().zip(source_group).enumerate() {
            original |= (bit as usize) << offset;
            interleaved |= (input[source] as usize) << offset;
        }
        let a = ENCODE_GROUPS[first * ENCODE_INPUTS + original];
        let b = ENCODE_GROUPS[second * ENCODE_INPUTS + interleaved];
        first = usize::from(a >> ENCODE_GROUP_BITS);
        second = usize::from(b >> ENCODE_GROUP_BITS);
        for (offset, (triple, &bit)) in output_group
            .chunks_exact_mut(CODE_RATE)
            .zip(input_group)
            .enumerate()
        {
            triple[0] = bit;
            triple[1] = Bit::from(a & (1 << offset) != 0);
            triple[2] = Bit::from(b & (1 << offset) != 0);
        }
    }
    for (offset, triple) in output[CODE_RATE * grouped..CODE_RATE * input.len()]
        .chunks_exact_mut(CODE_RATE)
        .enumerate()
    {
        let index = grouped + offset;
        let (next_first, parity_first) = transition(first, input[index] as usize);
        let (next_second, parity_second) = transition(second, input[plan.source()[index]] as usize);
        triple.copy_from_slice(&[
            input[index],
            Bit::from(parity_first != 0),
            Bit::from(parity_second != 0),
        ]);
        first = next_first;
        second = next_second;
    }
    // TS 25.212 4.2.3.2.2 sends three (systematic, parity) tails per constituent, in order.
    for (state, tail) in [first, second]
        .into_iter()
        .zip(output[CODE_RATE * input.len()..].chunks_exact_mut(TAIL_STEPS * 2))
    {
        let mut state = state;
        for pair in tail.chunks_exact_mut(2) {
            let input = ((state >> 1) ^ (state >> 2)) & 1;
            let (next, parity) = transition(state, input);
            pair.copy_from_slice(&[Bit::from(input != 0), Bit::from(parity != 0)]);
            state = next;
        }
        debug_assert_eq!(state, 0);
    }
    Ok(())
}

pub struct TurboDecoder {
    plan: Arc<TurboInterleaver>,
    iterations: NonZeroU8,
    systematic: [Vec<f64>; 2],
    parity: [Vec<f64>; 2],
    prior: [Vec<f64>; 2],
    posterior: [Vec<f64>; 2],
    forward: Vec<[f64; STATES]>,
    progress: Option<incremental::Progress>,
    // One past the greatest original information index with a tied or invalid
    // posterior in the current output pass. Zero means none.
    unobserved_until: usize,
}

impl TurboDecoder {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.systematic
            .iter()
            .chain(&self.parity)
            .chain(&self.prior)
            .chain(&self.posterior)
            .map(|values| values.capacity() * std::mem::size_of::<f64>())
            .sum::<usize>()
            + self.forward.capacity() * std::mem::size_of::<[f64; STATES]>()
    }
    pub(crate) fn cancel(&mut self) {
        self.progress = None;
    }
    pub(crate) fn reserve_width(&mut self, maximum: usize) -> Result<(), DspError> {
        if maximum < self.plan.len() || maximum > MAX_BLOCK_BITS {
            return Err(DspError::Capacity);
        }
        for values in self.systematic.iter_mut().chain(&mut self.parity) {
            values.resize(maximum + TAIL_STEPS, 0.0);
        }
        for values in self.prior.iter_mut().chain(&mut self.posterior) {
            values.resize(maximum, 0.0);
        }
        self.forward.resize(maximum + TAIL_STEPS + 1, [0.0; STATES]);
        Ok(())
    }
    pub(crate) fn configure(&mut self, plan: Arc<TurboInterleaver>) -> Result<(), DspError> {
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        if plan.len() > self.prior[0].len() {
            return Err(DspError::Capacity);
        }
        self.plan = plan;
        Ok(())
    }
    pub fn new(plan: Arc<TurboInterleaver>, iterations: NonZeroU8) -> Result<Self, DspError> {
        if iterations.get() > MAX_ITERATIONS {
            return Err(DspError::Capacity);
        }
        let length = plan.len();
        Ok(Self {
            plan,
            iterations,
            systematic: std::array::from_fn(|_| vec![0.0; length + TAIL_STEPS]),
            parity: std::array::from_fn(|_| vec![0.0; length + TAIL_STEPS]),
            prior: std::array::from_fn(|_| vec![0.0; length]),
            posterior: std::array::from_fn(|_| vec![0.0; length]),
            forward: vec![[0.0; STATES]; length + TAIL_STEPS + 1],
            progress: None,
            unobserved_until: 0,
        })
    }

    /// Positive evidence is log(P(0)/P(1)). Inputs must preserve relative reliability across bits.
    pub fn decode(&mut self, input: &[f32], output: &mut [Bit]) -> Result<(), DspError> {
        if output.len() != self.plan.len() {
            return Err(DspError::InvalidLength);
        }
        self.begin(input)?;
        let mut work = crate::coding::DecodeWork::new(std::num::NonZeroUsize::MAX);
        if !self.step(output, &mut work)? {
            return Err(DspError::Capacity);
        }
        Ok(())
    }

    pub(crate) fn begin(&mut self, input: &[f32]) -> Result<(), DspError> {
        if self.progress.is_some() {
            return Err(DspError::DiscontinuousTime);
        }
        let length = self.plan.len();
        if input.len() != coded_bits(length)? {
            return Err(DspError::InvalidLength);
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err(DspError::NonFiniteEvidence);
        }
        for (index, triple) in input[..CODE_RATE * length]
            .chunks_exact(CODE_RATE)
            .enumerate()
        {
            self.systematic[0][index] = f64::from(triple[0]);
            self.parity[0][index] = f64::from(triple[1]);
            self.parity[1][index] = f64::from(triple[2]);
        }
        for (index, &source) in self.plan.source().iter().enumerate() {
            self.systematic[1][index] = self.systematic[0][source];
        }
        for (constituent, tail) in input[CODE_RATE * length..]
            .chunks_exact(TAIL_STEPS * 2)
            .enumerate()
        {
            for (index, pair) in tail.chunks_exact(2).enumerate() {
                self.systematic[constituent][length + index] = f64::from(pair[0]);
                self.parity[constituent][length + index] = f64::from(pair[1]);
            }
        }
        self.prior[0][..length].fill(0.0);
        self.unobserved_until = 0;
        self.forward[0].fill(f64::NEG_INFINITY);
        self.forward[0][0] = 0.0;
        self.progress = Some(incremental::Progress::new());
        Ok(())
    }
}

fn branch(input: usize, parity: usize, systematic: f64, parity_llr: f64) -> f64 {
    let systematic = if input == 0 { systematic } else { -systematic };
    let parity_llr = if parity == 0 { parity_llr } else { -parity_llr };
    (systematic + parity_llr) * 0.5
}

fn normalize(metrics: &mut [f64; STATES]) {
    let maximum = metrics.iter().copied().fold(f64::NEG_INFINITY, trellis_max);
    for metric in metrics {
        *metric -= maximum;
    }
}

// Validated finite f32 evidence and bounded iterations keep metrics finite or negative infinity, never NaN.
#[inline]
fn trellis_max(first: f64, second: f64) -> f64 {
    if first > second { first } else { second }
}

#[cfg(test)]
fn constituent(
    systematic: &[f64],
    parity: &[f64],
    prior: &[f64],
    posterior: &mut [f64],
    forward: &mut [[f64; STATES]],
) {
    forward[0].fill(f64::NEG_INFINITY);
    forward[0][0] = 0.0;
    for index in 0..systematic.len() {
        let evidence = systematic[index] + prior.get(index).copied().unwrap_or(0.0);
        let mut next = [f64::NEG_INFINITY; STATES];
        for (state, &metric) in forward[index].iter().enumerate() {
            for input in 0..2 {
                let (target, bit) = transition(state, input);
                next[target] =
                    next[target].max(metric + branch(input, bit, evidence, parity[index]));
            }
        }
        normalize(&mut next);
        forward[index + 1] = next;
    }
    let mut backward = [f64::NEG_INFINITY; STATES];
    backward[0] = 0.0;
    for index in (0..systematic.len()).rev() {
        let evidence = systematic[index] + prior.get(index).copied().unwrap_or(0.0);
        let mut previous = [f64::NEG_INFINITY; STATES];
        let mut best = [f64::NEG_INFINITY; 2];
        for state in 0..STATES {
            for (input, best_input) in best.iter_mut().enumerate() {
                let (target, bit) = transition(state, input);
                let suffix = backward[target] + branch(input, bit, evidence, parity[index]);
                previous[state] = previous[state].max(suffix);
                *best_input = best_input.max(forward[index][state] + suffix);
            }
        }
        if index < posterior.len() {
            posterior[index] = best[0] - best[1];
        }
        normalize(&mut previous);
        backward = previous;
    }
}

#[cfg(test)]
mod tests;
