// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Operation, Parameters, RateMatching};
use crate::{DspError, interleaving::Rel99Tti};
use std::num::NonZeroUsize;

impl RateMatching {
    pub fn downlink_turbo_flexible(
        input: usize,
        output: usize,
        budget: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if input > budget.get() || output > budget.get() {
            return Err(DspError::Capacity);
        }
        if !input.is_multiple_of(3) {
            return Err(DspError::InvalidRateMatching);
        }
        if output >= input {
            return Self::downlink_convolutional_flexible(input, output);
        }
        let removed = input - output;
        let width = input / 3;
        // TS 25.212 4.2.7.2.2.3 splits the negative delta using floor for the first parity stream.
        let removals = [removed.div_ceil(2), removed / 2];
        Self::turbo_puncture(input, output, [1, 2], |parity| {
            let a = 2 - parity;
            parity_plan(
                width,
                removals[parity],
                width,
                a * width,
                a * removals[parity],
            )
        })
    }

    pub fn downlink_turbo_fixed(
        input: usize,
        maximum: usize,
        delta: i64,
        budget: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if input > budget.get() || maximum > budget.get() {
            return Err(DspError::Capacity);
        }
        if maximum == 0 || input > maximum || !input.is_multiple_of(3) || !maximum.is_multiple_of(3)
        {
            return Err(DspError::InvalidRateMatching);
        }
        if delta >= 0 {
            return Self::downlink_convolutional_fixed(input, maximum, delta, budget);
        }
        let removed = usize::try_from(delta.unsigned_abs()).map_err(|_| DspError::Capacity)?;
        let maximum = maximum / 3;
        let width = input / 3;
        let removals = [removed.div_ceil(2), removed / 2];
        if removals.iter().any(|removed| *removed > maximum) {
            return Err(DspError::InvalidRateMatching);
        }
        // TS 25.212 4.2.7.2.1.4 rounds the first parity count to nearest and the second down.
        let first = ((2 * removals[0] as u128 * width as u128 + maximum as u128)
            / (2 * maximum as u128)) as usize;
        let second = (removals[1] as u128 * width as u128 / maximum as u128) as usize;
        let output = input
            .checked_sub(first + second)
            .ok_or(DspError::InvalidRateMatching)?;
        Self::turbo_puncture(input, output, [1, 2], |parity| {
            let a = 2 - parity;
            parity_plan(
                width,
                [first, second][parity],
                maximum,
                a * maximum,
                a * removals[parity],
            )
        })
    }

    pub fn uplink_turbo(
        input: usize,
        output: usize,
        interval: Rel99Tti,
        frame: usize,
        budget: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if input > budget.get() || output > budget.get() {
            return Err(DspError::Capacity);
        }
        let frames = interval.frames();
        if frame >= frames {
            return Err(DspError::InvalidLength);
        }
        if output >= input {
            return Self::uplink_convolutional(input, output, interval, frame);
        }
        let width = input / 3;
        let removed = input - output;
        let removals = [removed.div_ceil(2), removed / 2];
        if width == 0 || removals.iter().any(|removed| *removed > width) {
            return Err(DspError::InvalidRateMatching);
        }
        let alpha = match interval {
            Rel99Tti::Ms10 | Rel99Tti::Ms40 => [1, 2],
            Rel99Tti::Ms20 | Rel99Tti::Ms80 => [2, 1],
        };
        let offsets = [(alpha[0] + frame % 3) % 3, (alpha[1] + frame % 3) % 3];
        // TS 25.212 4.2.7.1.2.2 and 4.2.7.3 retain leftover bits in the protected stream.
        Self::turbo_puncture(input, output, offsets, |parity| {
            let removal = removals[parity];
            let a = 2 - parity;
            if removal == 0 {
                return parity_plan(width, 0, width, a * width, 0);
            }
            let q = width / removal;
            let b = parity + 2;
            let mut s = [0; 8];
            if q <= 2 {
                for r in 0..frames {
                    s[(3 * r + b - 1) % frames] = r % 2;
                }
            } else {
                let numerator = q * frames
                    - if q.is_multiple_of(2) {
                        gcd(q, frames)
                    } else {
                        0
                    };
                for x in 0..frames {
                    let value = (x * numerator).div_ceil(frames);
                    s[(3 * (value % frames) + b - 1) % frames] = value / frames;
                }
            }
            let plus = a * width;
            let initial = ((a as u128 * s[interval.permutation()[frame]] as u128 * removal as u128
                + width as u128)
                % plus as u128) as usize;
            parity_plan(
                width,
                removal,
                if initial == 0 { plus } else { initial },
                plus,
                a * removal,
            )
        })
    }

    fn turbo_puncture(
        input: usize,
        output: usize,
        offsets: [usize; 2],
        mut parameters: impl FnMut(usize) -> Result<Self, DspError>,
    ) -> Result<Self, DspError> {
        let width = input / 3;
        let mut keep = vec![true; input];
        for (parity, offset) in offsets.into_iter().enumerate() {
            let plan = parameters(parity)?;
            for index in 0..width {
                keep[3 * index + offset] = false;
            }
            for index in plan.source {
                keep[3 * index + offset] = true;
            }
        }
        let source: Vec<_> = keep
            .into_iter()
            .enumerate()
            .filter_map(|(index, keep)| keep.then_some(index))
            .collect();
        if source.len() != output {
            return Err(DspError::InvalidRateMatching);
        }
        Ok(Self {
            input_length: input,
            source,
        })
    }
}

fn parity_plan(
    width: usize,
    removal: usize,
    initial: usize,
    plus: usize,
    minus: usize,
) -> Result<RateMatching, DspError> {
    let output = width
        .checked_sub(removal)
        .ok_or(DspError::InvalidRateMatching)?;
    RateMatching::new(
        width,
        output,
        Operation::Puncture,
        Parameters {
            initial: u64::try_from(initial).map_err(|_| DspError::Capacity)?,
            plus: u64::try_from(plus).map_err(|_| DspError::Capacity)?,
            minus: u64::try_from(minus).map_err(|_| DspError::Capacity)?,
        },
    )
}

fn gcd(mut first: usize, mut second: usize) -> usize {
    while second != 0 {
        (first, second) = (second, first % second);
    }
    first
}
