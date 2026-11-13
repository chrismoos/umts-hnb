// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{INPUTS, PREDECESSORS, STATES, SUCCESSORS, trellis_max};
use wide::f64x4;

const LANES: usize = 4;

// This module requires AVX2. Its packed maximum preserves the scalar
// comparison's second-operand choice on equal finite or negative-infinity metrics.
#[inline]
fn maximum(first: f64x4, second: f64x4) -> f64x4 {
    first.fast_max(second)
}

#[inline]
fn posterior_maximum(scores: [f64x4; 4]) -> [f64; INPUTS] {
    // Rows are zero/one scores for states0..3, then zero/one for states4..7.
    // Reduce contiguous state ranges so every equal maximum still selects the
    // rightmost state, including its signed-zero bits. Metrics cannot be NaN.
    let columns = f64x4::transpose(scores);
    let groups = maximum(
        maximum(columns[0], columns[1]),
        maximum(columns[2], columns[3]),
    )
    .to_array();
    [
        trellis_max(groups[0], groups[2]),
        trellis_max(groups[1], groups[3]),
    ]
}

#[inline]
pub(super) fn forward_step(
    row: &[f64; STATES],
    branches: &[[f64; INPUTS]; INPUTS],
) -> [f64; STATES] {
    let mut next = [0.0; STATES];
    for group in 0..STATES / LANES {
        let start = group * LANES;
        let first = f64x4::new(std::array::from_fn(|lane| {
            let edge = PREDECESSORS[start + lane][0];
            row[edge.state] + branches[edge.input][edge.parity]
        }));
        let second = f64x4::new(std::array::from_fn(|lane| {
            let edge = PREDECESSORS[start + lane][1];
            row[edge.state] + branches[edge.input][edge.parity]
        }));
        next[start..start + LANES].copy_from_slice(&maximum(first, second).to_array());
    }
    next
}

#[inline]
pub(super) fn backward_step(
    metrics: &[f64; STATES],
    row: &[f64; STATES],
    branches: &[[f64; INPUTS]; INPUTS],
) -> ([f64; STATES], [f64; INPUTS]) {
    let mut previous = [0.0; STATES];
    let mut scores = [f64x4::ZERO; 4];
    for group in 0..STATES / LANES {
        let start = group * LANES;
        let zero = f64x4::new(std::array::from_fn(|lane| {
            let (target, parity) = SUCCESSORS[start + lane][0];
            metrics[target] + branches[0][parity]
        }));
        let one = f64x4::new(std::array::from_fn(|lane| {
            let (target, parity) = SUCCESSORS[start + lane][1];
            metrics[target] + branches[1][parity]
        }));
        previous[start..start + LANES].copy_from_slice(&maximum(zero, one).to_array());
        let row = f64x4::new(std::array::from_fn(|lane| row[start + lane]));
        scores[group * INPUTS] = row + zero;
        scores[group * INPUTS + 1] = row + one;
    }
    (previous, posterior_maximum(scores))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posterior_reduction_preserves_all_signed_zero_and_unreachable_tie_orders() {
        let palette = [f64::NEG_INFINITY, -0.0, 0.0, 1.0];
        for case in 0..(1usize << (2 * STATES)) {
            let zero: [f64; STATES] =
                std::array::from_fn(|state| palette[(case >> (2 * state)) & 3]);
            let one: [f64; STATES] = std::array::from_fn(|state| zero[STATES - state - 1]);
            let actual = posterior_maximum([
                f64x4::new(zero[..LANES].try_into().unwrap()),
                f64x4::new(one[..LANES].try_into().unwrap()),
                f64x4::new(zero[LANES..].try_into().unwrap()),
                f64x4::new(one[LANES..].try_into().unwrap()),
            ]);
            for (actual, values) in actual.into_iter().zip([zero, one]) {
                let expected = values.into_iter().fold(f64::NEG_INFINITY, trellis_max);
                assert_eq!(actual.to_bits(), expected.to_bits(), "case={case}");
            }
        }
    }
}
