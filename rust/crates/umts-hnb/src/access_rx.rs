// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use std::sync::Arc;
use umts_dsp::{
    DspError,
    prach_search::{PreambleSearchPlan, PreambleTimingSearch, TimingWindow},
    pulse_shaping::SamplesPerChip,
};
use umts_types::time::{ChipInstant, SampleInstant};

use crate::radio_io::FilteredRx;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessRxError {
    NotArmed,
    Epoch,
    Phase,
    Gap,
    Time,
    Dsp(DspError),
}

pub struct PrachWindowReceiver {
    search: PreambleTimingSearch,
    chips: Vec<Complex32>,
    rate: usize,
    start: Option<ChipInstant>,
    generation: Option<u64>,
    next_input: Option<SampleInstant>,
    filled: usize,
}

impl PrachWindowReceiver {
    pub fn new(plan: Arc<PreambleSearchPlan>, rate: SamplesPerChip) -> Self {
        Self {
            chips: vec![Complex32::default(); plan.input_length()],
            search: PreambleTimingSearch::new(plan),
            rate: rate.get(),
            start: None,
            generation: None,
            next_input: None,
            filled: 0,
        }
    }

    pub fn arm(&mut self, first_reference_chip: ChipInstant) -> Result<(), AccessRxError> {
        first_reference_chip
            .index
            .checked_add(self.chips.len() as u64)
            .and_then(|end| end.checked_mul(self.rate as u64))
            .ok_or(AccessRxError::Time)?;
        self.start = Some(first_reference_chip);
        self.generation = None;
        self.next_input = None;
        self.filled = 0;
        Ok(())
    }

    pub fn abort(&mut self) {
        self.start = None;
        self.filled = 0;
        self.generation = None;
        self.next_input = None;
    }

    pub fn remaining_chips(&self) -> Option<usize> {
        self.start.map(|_| self.chips.len() - self.filled)
    }

    pub fn feed(
        &mut self,
        input: FilteredRx<'_>,
    ) -> Result<Option<TimingWindow<'_>>, AccessRxError> {
        let start = self.start.ok_or(AccessRxError::NotArmed)?;
        let validate = |this: &Self| {
            if input.window.next_input.epoch != start.epoch {
                return Err(AccessRxError::Epoch);
            }
            if input.window.sample_stride != this.rate {
                return Err(AccessRxError::Phase);
            }
            if this
                .generation
                .is_some_and(|generation| generation != input.generation || input.discontinuity)
            {
                return Err(AccessRxError::Gap);
            }
            let Some(first) = input.window.first_center else {
                if !input.chips.is_empty() {
                    return Err(AccessRxError::Time);
                }
                return Ok(None);
            };
            if first.epoch != start.epoch {
                return Err(AccessRxError::Epoch);
            }
            if !first.index.is_multiple_of(this.rate as u64) {
                return Err(AccessRxError::Phase);
            }
            if this.next_input.is_some_and(|next| next != first) {
                return Err(AccessRxError::Gap);
            }
            let next = first
                .index
                .checked_add(
                    (input.chips.len() as u64)
                        .checked_mul(this.rate as u64)
                        .ok_or(AccessRxError::Time)?,
                )
                .ok_or(AccessRxError::Time)?;
            Ok(Some((
                first,
                SampleInstant {
                    index: next,
                    ..first
                },
            )))
        };
        let range = match validate(self) {
            Ok(range) => range,
            Err(error) => {
                self.abort();
                return Err(error);
            }
        };
        self.generation = Some(input.generation);
        let Some((first, next)) = range else {
            return Ok(None);
        };
        self.next_input = Some(next);
        let first_chip = first.index / self.rate as u64;
        let required = start.index + self.filled as u64;
        if first_chip > required {
            self.abort();
            return Err(AccessRxError::Gap);
        }
        let skip = required - first_chip;
        if skip >= input.chips.len() as u64 {
            return Ok(None);
        }
        let source = &input.chips[skip as usize..];
        let count = source.len().min(self.chips.len() - self.filled);
        self.chips[self.filled..self.filled + count].copy_from_slice(&source[..count]);
        self.filled += count;
        if self.filled != self.chips.len() {
            return Ok(None);
        }
        self.start = None;
        self.search
            .search(start, &self.chips)
            .map(Some)
            .map_err(AccessRxError::Dsp)
    }
}

#[cfg(test)]
mod tests;
