// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    Bit, DspError, STATES, TAIL_STEPS, TurboDecoder, branch, normalize, transition, trellis_max,
};
use crate::coding::DecodeWork;
use wide::f64x4;

#[inline]
fn observe_output(posterior: &[f64], source: &[usize], unobserved_until: &mut usize) {
    let mut chunks = posterior.chunks_exact(4);
    for (values, indices) in chunks.by_ref().zip(source.chunks_exact(4)) {
        let lanes = f64x4::new([values[0], values[1], values[2], values[3]]);
        if !(lanes.is_finite() & lanes.simd_ne(f64x4::ZERO)).all() {
            for (&value, &index) in values.iter().zip(indices) {
                if value == 0.0 || !value.is_finite() {
                    *unobserved_until = (*unobserved_until).max(index + 1);
                }
            }
        }
    }
    let start = posterior.len() - chunks.remainder().len();
    for (&value, &index) in chunks.remainder().iter().zip(&source[start..]) {
        if value == 0.0 || !value.is_finite() {
            *unobserved_until = (*unobserved_until).max(index + 1);
        }
    }
}

#[cfg(target_feature = "avx2")]
mod vector;
#[cfg(not(target_feature = "avx2"))]
use self::{backward_step_scalar as backward_step, forward_step_scalar as forward_step};
#[cfg(target_feature = "avx2")]
use vector::{backward_step, forward_step};

const INPUTS: usize = 2;
// Max-log comparisons are invariant to a common additive offset in all state metrics.
const NORMALIZATION_INTERVAL: usize = 64;

#[derive(Clone, Copy)]
struct Edge {
    state: usize,
    input: usize,
    parity: usize,
}

const PREDECESSORS: [[Edge; INPUTS]; STATES] = {
    let mut edges = [[Edge {
        state: 0,
        input: 0,
        parity: 0,
    }; INPUTS]; STATES];
    let mut counts = [0; STATES];
    let mut state = 0;
    while state < STATES {
        let mut input = 0;
        while input < INPUTS {
            let (target, parity) = transition(state, input);
            edges[target][counts[target]] = Edge {
                state,
                input,
                parity,
            };
            counts[target] += 1;
            input += 1;
        }
        state += 1;
    }
    edges
};

const SUCCESSORS: [[(usize, usize); INPUTS]; STATES] = {
    let mut edges = [[(0, 0); INPUTS]; STATES];
    let mut state = 0;
    while state < STATES {
        edges[state] = [transition(state, 0), transition(state, 1)];
        state += 1;
    }
    edges
};

fn branch_metrics(systematic: f64, parity: f64) -> [[f64; INPUTS]; INPUTS] {
    [
        [
            branch(0, 0, systematic, parity),
            branch(0, 1, systematic, parity),
        ],
        [
            branch(1, 0, systematic, parity),
            branch(1, 1, systematic, parity),
        ],
    ]
}

#[inline]
#[cfg(any(test, not(target_feature = "avx2")))]
fn forward_step_scalar(row: &[f64; STATES], branches: &[[f64; INPUTS]; INPUTS]) -> [f64; STATES] {
    let mut next = [0.0; STATES];
    for (metric, [first, second]) in next.iter_mut().zip(PREDECESSORS) {
        *metric = trellis_max(
            row[first.state] + branches[first.input][first.parity],
            row[second.state] + branches[second.input][second.parity],
        );
    }
    next
}

#[inline]
#[cfg(any(test, not(target_feature = "avx2")))]
fn backward_step_scalar(
    metrics: &[f64; STATES],
    row: &[f64; STATES],
    branches: &[[f64; INPUTS]; INPUTS],
) -> ([f64; STATES], [f64; INPUTS]) {
    let mut previous = [0.0; STATES];
    let mut best = [f64::NEG_INFINITY; INPUTS];
    for (state, metric) in previous.iter_mut().enumerate() {
        let [(zero_target, zero_parity), (one_target, one_parity)] = SUCCESSORS[state];
        let zero = metrics[zero_target] + branches[0][zero_parity];
        let one = metrics[one_target] + branches[1][one_parity];
        *metric = trellis_max(zero, one);
        best[0] = trellis_max(best[0], row[state] + zero);
        best[1] = trellis_max(best[1], row[state] + one);
    }
    (previous, best)
}

enum Phase {
    Forward(usize),
    Backward { next: usize, metrics: [f64; STATES] },
    Exchange(usize),
    Output(usize),
    BetweenIterations,
}

pub(super) struct Progress {
    iteration: u8,
    constituent: usize,
    phase: Phase,
    target: Option<u8>,
}

impl Progress {
    pub(super) fn new() -> Self {
        Self {
            iteration: 0,
            constituent: 0,
            phase: Phase::Forward(0),
            target: None,
        }
    }
}

impl TurboDecoder {
    pub(crate) fn step(
        &mut self,
        output: &mut [Bit],
        work: &mut DecodeWork,
    ) -> Result<bool, DspError> {
        self.step_to_iteration(output, work, self.iterations.get())
    }

    // Completed intermediate rounds retain both constituent priors for CRC-gated continuation.
    pub(crate) fn step_to_iteration(
        &mut self,
        output: &mut [Bit],
        work: &mut DecodeWork,
        target: u8,
    ) -> Result<bool, DspError> {
        let length = self.plan.len();
        if output.len() != length {
            return Err(DspError::InvalidLength);
        }
        let state = self.progress.as_ref().ok_or(DspError::DiscontinuousTime)?;
        if target == 0
            || target > self.iterations.get()
            || state.target.is_some_and(|previous| previous != target)
            || (state.target.is_none() && target <= state.iteration)
        {
            return Err(DspError::DiscontinuousTime);
        }
        let mut progress = self.progress.take().ok_or(DspError::DiscontinuousTime)?;
        progress.target = Some(target);
        'decode: loop {
            let constituent = progress.constituent;
            match &mut progress.phase {
                Phase::BetweenIterations => {
                    progress.constituent = 0;
                    self.forward[0].fill(f64::NEG_INFINITY);
                    self.forward[0][0] = 0.0;
                    progress.phase = Phase::Forward(0);
                }
                Phase::Forward(index) => {
                    if *index == length + TAIL_STEPS {
                        let mut metrics = [f64::NEG_INFINITY; STATES];
                        metrics[0] = 0.0;
                        progress.phase = Phase::Backward {
                            next: *index,
                            metrics,
                        };
                        continue;
                    }
                    let systematic = &self.systematic[constituent][..length + TAIL_STEPS];
                    let parity = &self.parity[constituent][..length + TAIL_STEPS];
                    let prior = &self.prior[constituent][..length];
                    let forward = &mut self.forward[..length + TAIL_STEPS + 1];
                    while *index < length + TAIL_STEPS {
                        if !work.consume(STATES) {
                            break 'decode;
                        }
                        let evidence =
                            systematic[*index] + prior.get(*index).copied().unwrap_or(0.0);
                        let branches = branch_metrics(evidence, parity[*index]);
                        let row = &forward[*index];
                        let mut next = forward_step(row, &branches);
                        let completed = *index + 1;
                        if completed.is_multiple_of(NORMALIZATION_INTERVAL)
                            || completed == length + TAIL_STEPS
                        {
                            normalize(&mut next);
                        }
                        forward[*index + 1] = next;
                        *index += 1;
                    }
                }
                Phase::Backward { next, metrics } => {
                    if *next == 0 {
                        progress.phase = Phase::Exchange(0);
                        continue;
                    }
                    let systematic = &self.systematic[constituent][..length + TAIL_STEPS];
                    let parity = &self.parity[constituent][..length + TAIL_STEPS];
                    let prior = &self.prior[constituent][..length];
                    let forward = &mut self.forward[..length + TAIL_STEPS + 1];
                    while *next != 0 {
                        if !work.consume(STATES) {
                            break 'decode;
                        }
                        *next -= 1;
                        let index = *next;
                        let evidence = systematic[index] + prior.get(index).copied().unwrap_or(0.0);
                        let branches = branch_metrics(evidence, parity[index]);
                        let (mut previous, best) =
                            backward_step(metrics, &forward[index], &branches);
                        if index < length {
                            self.posterior[constituent][index] = best[0] - best[1];
                        }
                        let completed = length + TAIL_STEPS - index;
                        if completed.is_multiple_of(NORMALIZATION_INTERVAL) || index == 0 {
                            normalize(&mut previous);
                        }
                        *metrics = previous;
                    }
                }
                Phase::Exchange(index) => {
                    if *index == length {
                        if constituent == 1 {
                            progress.iteration += 1;
                            if progress.iteration == target {
                                self.unobserved_until = 0;
                                progress.phase = Phase::Output(0);
                                continue;
                            }
                        }
                        progress.constituent ^= 1;
                        self.forward[0].fill(f64::NEG_INFINITY);
                        self.forward[0][0] = 0.0;
                        progress.phase = Phase::Forward(0);
                        continue;
                    }
                    while *index < length {
                        if !work.consume(1) {
                            break 'decode;
                        }
                        let source = self.plan.source()[*index];
                        if constituent == 0 {
                            self.prior[1][*index] = self.posterior[0][source]
                                - self.systematic[0][source]
                                - self.prior[0][source];
                        } else {
                            self.prior[0][source] = self.posterior[1][*index]
                                - self.systematic[1][*index]
                                - self.prior[1][*index];
                        }
                        *index += 1;
                    }
                }
                Phase::Output(index) => {
                    if *index == length {
                        if progress.iteration < self.iterations.get() {
                            progress.phase = Phase::BetweenIterations;
                            progress.target = None;
                            self.progress = Some(progress);
                        }
                        return Ok(true);
                    }
                    let count = (length - *index).min(work.remaining());
                    if count == 0 {
                        break 'decode;
                    }
                    let end = *index + count;
                    let posterior = &self.posterior[1][*index..end];
                    let source = &self.plan.source()[*index..end];
                    let consumed = work.consume(count);
                    debug_assert!(consumed);
                    observe_output(posterior, source, &mut self.unobserved_until);
                    for (&value, &source) in posterior.iter().zip(source) {
                        output[source] = Bit::from(value < 0.0);
                    }
                    *index = end;
                }
            }
        }
        self.progress = Some(progress);
        Ok(false)
    }
}

#[cfg(test)]
mod observation_tests {
    use super::observe_output;

    #[test]
    fn vector_observation_matches_scalar_across_partitions_and_invalid_values() {
        for length in 1..=33 {
            let source: Vec<_> = (0..length).rev().collect();
            for position in 0..length {
                for value in [
                    0.0,
                    -0.0,
                    f64::NAN,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    1.0,
                    -1.0,
                    f64::from_bits(1),
                    f64::MAX,
                ] {
                    let mut posterior = vec![1.0; length];
                    posterior[position] = value;
                    let expected = if value == 0.0 || !value.is_finite() {
                        5.max(source[position] + 1)
                    } else {
                        5
                    };
                    for partition in [1, 3, 4, 5, 17, 33] {
                        let mut actual = 5;
                        for (values, indices) in
                            posterior.chunks(partition).zip(source.chunks(partition))
                        {
                            observe_output(values, indices, &mut actual);
                        }
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
        let mut unchanged = 7;
        observe_output(&[], &[], &mut unchanged);
        assert_eq!(unchanged, 7);
    }
}

#[cfg(all(test, target_feature = "avx2"))]
mod tests {
    use super::*;

    #[test]
    fn vector_trellis_matches_scalar_bits_including_unreachable_states_and_zero_ties() {
        let mut seed = 12_345u64;
        let mut random = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed as i64) as f64 / i64::MAX as f64 * 100.0
        };
        let same = |first: &[f64], second: &[f64]| {
            assert_eq!(
                first
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>(),
                second
                    .iter()
                    .map(|value| value.to_bits())
                    .collect::<Vec<_>>()
            );
        };
        for case in 0..10_002 {
            let mut row = std::array::from_fn(|state| {
                if (case + state).is_multiple_of(11) {
                    f64::NEG_INFINITY
                } else if (case + state).is_multiple_of(17) {
                    -0.0
                } else {
                    random()
                }
            });
            let mut metrics = std::array::from_fn(|state| {
                if (case + state).is_multiple_of(13) {
                    f64::NEG_INFINITY
                } else {
                    random()
                }
            });
            let scale = if case.is_multiple_of(7) {
                f64::from(f32::MAX)
            } else if case.is_multiple_of(5) {
                f64::from(f32::from_bits(1))
            } else {
                1.0
            };
            let mut branches = branch_metrics(random() * scale, random() * scale);
            if case >= 10_000 {
                let zero = if case == 10_000 { 0.0 } else { -0.0 };
                row.fill(zero);
                metrics.fill(zero);
                branches = branch_metrics(zero, zero);
            }
            same(
                &forward_step(&row, &branches),
                &forward_step_scalar(&row, &branches),
            );
            let actual = backward_step(&metrics, &row, &branches);
            let expected = backward_step_scalar(&metrics, &row, &branches);
            same(&actual.0, &expected.0);
            same(&actual.1, &expected.1);
        }
    }

    #[test]
    fn vector_trellis_preserves_all_unreachable_states_and_mixed_zero_ties() {
        let same = |actual: &[f64], expected: &[f64]| {
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        };
        for mask in 0..16 {
            let zeros: [f64; 4] =
                std::array::from_fn(|index| if mask & (1 << index) == 0 { 0.0 } else { -0.0 });
            let branches = [[zeros[0], zeros[1]], [zeros[2], zeros[3]]];
            for row in [
                [f64::NEG_INFINITY; STATES],
                std::array::from_fn(|state| zeros[state % zeros.len()]),
            ] {
                for metrics in [
                    [f64::NEG_INFINITY; STATES],
                    std::array::from_fn(|state| zeros[(state + 1) % zeros.len()]),
                ] {
                    same(
                        &forward_step(&row, &branches),
                        &forward_step_scalar(&row, &branches),
                    );
                    let actual = backward_step(&metrics, &row, &branches);
                    let expected = backward_step_scalar(&metrics, &row, &branches);
                    same(&actual.0, &expected.0);
                    same(&actual.1, &expected.1);
                }
            }
        }
    }

    #[test]
    fn vector_iteration_boundaries_preserve_exact_metrics_when_resumed() {
        use super::super::{TurboInterleaver, coded_bits};
        use std::{
            num::{NonZeroU8, NonZeroUsize},
            sync::Arc,
        };

        const ITERATIONS: u8 = 3;
        let same = |actual: &[f64], expected: &[f64]| {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(actual.to_bits(), expected.to_bits());
            }
        };
        // Include the smallest state update budget, an unaligned budget, and
        // budgets that interrupt extrinsic/output copies as well as trellis work.
        for length in [40, 530] {
            for gain in [1e-30_f32, 1.0, 1e30] {
                let plan = Arc::new(TurboInterleaver::new(length).unwrap());
                let evidence: Vec<_> = (0..coded_bits(length).unwrap())
                    .map(|index| ((index * 17 % 101) as f32 - 50.0) * gain)
                    .collect();
                for budget in [STATES, 17, 257] {
                    let mut direct =
                        TurboDecoder::new(plan.clone(), NonZeroU8::new(ITERATIONS).unwrap())
                            .unwrap();
                    let mut resumed =
                        TurboDecoder::new(plan.clone(), NonZeroU8::new(ITERATIONS).unwrap())
                            .unwrap();
                    direct.begin(&evidence).unwrap();
                    resumed.begin(&evidence).unwrap();
                    let mut expected = vec![Bit::Zero; length];
                    let mut actual = expected.clone();
                    for target in 1..=ITERATIONS {
                        let mut unlimited = DecodeWork::new(NonZeroUsize::MAX);
                        assert!(
                            direct
                                .step_to_iteration(&mut expected, &mut unlimited, target)
                                .unwrap()
                        );
                        let mut calls = 0;
                        loop {
                            calls += 1;
                            assert!(
                                calls < 100_000,
                                "resumed decoding must make bounded progress"
                            );
                            let mut work = DecodeWork::new(NonZeroUsize::new(budget).unwrap());
                            if resumed
                                .step_to_iteration(&mut actual, &mut work, target)
                                .unwrap()
                            {
                                break;
                            }
                        }
                        assert!(calls > 1);
                        assert_eq!(actual, expected);
                        for (actual, expected) in resumed.prior.iter().zip(&direct.prior) {
                            same(actual, expected);
                        }
                        for (actual, expected) in resumed.posterior.iter().zip(&direct.posterior) {
                            same(actual, expected);
                        }
                        for (actual, expected) in resumed.forward.iter().zip(&direct.forward) {
                            same(actual, expected);
                        }
                    }
                    assert!(direct.progress.is_none() && resumed.progress.is_none());
                }
            }
        }
    }
}
