// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{MAX_CODES, Modulation, collect_harq_bits};
use crate::{
    DspError,
    rate_matching::{Operation, Parameters, RateMatching},
};
use std::num::NonZeroUsize;

const TURBO_STREAMS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Redundancy {
    pub prioritize_systematic: bool,
    pub version: u8,
}

pub struct HarqRateMatching {
    input_bits: usize,
    source: Vec<usize>,
}

impl HarqRateMatching {
    // TS 25.212 4.5.4 uses independent virtual IR storage for each HARQ process.
    pub fn new(
        input_bits: NonZeroUsize,
        ir_bits: NonZeroUsize,
        modulation: Modulation,
        codes: usize,
        redundancy: Redundancy,
        maximum_input_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        let input = input_bits.get();
        if input > maximum_input_bits.get() {
            return Err(DspError::Capacity);
        }
        let versions = match modulation {
            Modulation::Qpsk => 4,
            Modulation::Qam16 | Modulation::Qam64 => 2,
        };
        if !input.is_multiple_of(TURBO_STREAMS)
            || !(1..=MAX_CODES).contains(&codes)
            || usize::from(redundancy.version) >= versions
        {
            return Err(DspError::InvalidRateMatching);
        }
        let width = input / TURBO_STREAMS;
        let removed = input.saturating_sub(ir_bits.get());
        let removals = [0, removed.div_ceil(2), removed / 2];
        let mut streams = [Vec::new(), Vec::new(), Vec::new()];
        for (lane, stream) in streams.iter_mut().enumerate() {
            let retained = width
                .checked_sub(removals[lane])
                .ok_or(DspError::InvalidRateMatching)?;
            let original: Vec<_> = (0..width)
                .map(|index| index * TURBO_STREAMS + lane)
                .collect();
            stream.resize(retained, 0);
            if removals[lane] == 0 {
                stream.copy_from_slice(&original);
            } else {
                let factor = if lane == 1 { 2 } else { 1 };
                RateMatching::new(
                    width,
                    retained,
                    Operation::Puncture,
                    Parameters {
                        initial: width as u64,
                        plus: scaled(width, factor)?,
                        minus: scaled(removals[lane], factor)?,
                    },
                )?
                .apply(&original, stream)?;
            }
        }
        let sizes = streams.each_ref().map(|stream| stream.len());
        let available = sizes.iter().sum::<usize>();
        let output = codes * modulation.bits_per_code();
        let repetition = output > available;
        let systematic = if repetition {
            // Table 10 deliberately uses twice Np1, including unequal parity lengths.
            let divisor = sizes[0] + 2 * sizes[1];
            usize::try_from(sizes[0] as u128 * output as u128 / divisor as u128)
                .map_err(|_| DspError::Capacity)?
        } else if redundancy.prioritize_systematic {
            sizes[0].min(output)
        } else {
            output.saturating_sub(sizes[1] + sizes[2])
        };
        let parity = output
            .checked_sub(systematic)
            .ok_or(DspError::InvalidRateMatching)?;
        let targets = [systematic, parity / 2, parity.div_ceil(2)];
        let mut matched = [Vec::new(), Vec::new(), Vec::new()];
        for lane in 0..TURBO_STREAMS {
            if sizes[lane] == 0 {
                if targets[lane] != 0 {
                    return Err(DspError::InvalidRateMatching);
                }
                continue;
            }
            let factor = if lane == 1 { 2 } else { 1 };
            let plus = scaled(sizes[lane], factor)?;
            let phase = if repetition {
                (usize::from(redundancy.prioritize_systematic)
                    + 2 * usize::from(redundancy.version)) as u128
                    * u128::from(plus)
                    / (2 * versions) as u128
            } else {
                usize::from(redundancy.version) as u128 * u128::from(plus) / versions as u128
            };
            let initial =
                (sizes[lane] as i128 - phase as i128 - 1).rem_euclid(i128::from(plus)) + 1;
            matched[lane].resize(targets[lane], 0);
            RateMatching::new(
                sizes[lane],
                targets[lane],
                if targets[lane] < sizes[lane] {
                    Operation::Puncture
                } else {
                    Operation::Repeat
                },
                Parameters {
                    initial: initial as u64,
                    plus,
                    minus: scaled(sizes[lane].abs_diff(targets[lane]), factor)?,
                },
            )?
            .apply(&streams[lane], &mut matched[lane])?;
        }
        let mut source = vec![0; output];
        collect_harq_bits(
            modulation,
            &matched[0],
            &matched[1],
            &matched[2],
            &mut source,
        )?;
        Ok(Self {
            input_bits: input,
            source,
        })
    }

    pub fn output_bits(&self) -> usize {
        self.source.len()
    }

    pub(super) fn source_indices(&self) -> &[usize] {
        &self.source
    }

    pub fn apply<T: Copy>(&self, input: &[T], output: &mut [T]) -> Result<(), DspError> {
        if input.len() != self.input_bits || output.len() != self.source.len() {
            return Err(DspError::InvalidLength);
        }
        for (value, &index) in output.iter_mut().zip(&self.source) {
            *value = input[index];
        }
        Ok(())
    }
}

fn scaled(value: usize, factor: usize) -> Result<u64, DspError> {
    value
        .checked_mul(factor)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(DspError::Capacity)
}
