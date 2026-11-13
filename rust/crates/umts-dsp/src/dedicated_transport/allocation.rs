// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, dedicated::UplinkLayout};
use std::num::NonZeroU16;
use umts_types::time::SLOTS_PER_FRAME;

pub(super) const MAX_TRANSPORTS: usize = 32;
const MAX_RATE_ATTRIBUTE: u16 = 256;
const MIN_PUNCTURING_PERCENT: u8 = 40;
const PUNCTURING_PERCENT_STEP: u8 = 4;
const PERCENT_SCALE: u128 = 100;
const MAX_UPLINK_LAYOUTS: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransportId(u8);
impl TransportId {
    pub fn new(id: u8) -> Result<Self, DspError> {
        if id == 0 || usize::from(id) > MAX_TRANSPORTS {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(id))
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PuncturingLimit(u8);
impl PuncturingLimit {
    pub fn new(percent: u8) -> Result<Self, DspError> {
        if !(MIN_PUNCTURING_PERCENT..=PERCENT_SCALE as u8).contains(&percent)
            || !(percent - MIN_PUNCTURING_PERCENT).is_multiple_of(PUNCTURING_PERCENT_STEP)
        {
            return Err(DspError::InvalidRateMatching);
        }
        Ok(Self(percent))
    }
    pub fn percent(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UplinkRateInput {
    pub id: TransportId,
    pub frame_bits: usize,
    pub attribute: NonZeroU16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchedTransport {
    pub id: TransportId,
    pub input_bits: usize,
    pub output_bits: usize,
}

pub struct UplinkAllocation {
    layout: UplinkLayout,
    channels: Vec<MatchedTransport>,
    total_bits: usize,
}

fn capacity(layout: UplinkLayout) -> usize {
    layout.channel(0).map_or(0, |channel| {
        channel.bits_per_slot() * SLOTS_PER_FRAME as usize * layout.len()
    })
}

impl UplinkAllocation {
    // TS 25.212 3.1 orders dedicated TrCHs by identity. 4.2.7.1.1 selects SET1/SET2 before Equation 1 allocates bits.
    pub fn new(
        channels: &[UplinkRateInput],
        allowed: &[UplinkLayout],
        limit: PuncturingLimit,
    ) -> Result<Self, DspError> {
        if channels.is_empty()
            || channels.len() > MAX_TRANSPORTS
            || allowed.len() > MAX_UPLINK_LAYOUTS
            || channels.windows(2).any(|pair| pair[0].id >= pair[1].id)
            || channels
                .iter()
                .any(|channel| channel.attribute.get() > MAX_RATE_ATTRIBUTE)
        {
            return Err(DspError::InvalidCode);
        }
        let total = channels
            .iter()
            .try_fold(0_u128, |sum, channel| {
                sum.checked_add(channel.frame_bits as u128 * u128::from(channel.attribute.get()))
            })
            .ok_or(DspError::Capacity)?;
        let minimum = channels
            .iter()
            .map(|channel| channel.attribute.get())
            .min()
            .expect("nonempty transports");
        let mut candidates = allowed.to_vec();
        if candidates.iter().any(|layout| layout.is_empty()) {
            return Err(DspError::InvalidCode);
        }
        candidates.sort_unstable_by_key(|layout| capacity(*layout));
        if candidates
            .windows(2)
            .any(|pair| capacity(pair[0]) == capacity(pair[1]))
        {
            return Err(DspError::InvalidCode);
        }
        let layout = if total == 0 {
            UplinkLayout::control_only()
        } else {
            let first = candidates
                .iter()
                .copied()
                .find(|layout| u128::from(minimum) * capacity(*layout) as u128 >= total);
            if let Some(layout) = first.filter(|layout| layout.len() == 1) {
                layout
            } else {
                let mut eligible = candidates.iter().copied().filter(|layout| {
                    u128::from(minimum) * capacity(*layout) as u128 * PERCENT_SCALE
                        >= u128::from(limit.0) * total
                });
                let first = eligible.next().ok_or(DspError::Capacity)?;
                eligible
                    .take_while(|layout| layout.len() == first.len())
                    .last()
                    .unwrap_or(first)
            }
        };
        let total_bits = capacity(layout);
        let mut matched = Vec::with_capacity(channels.len());
        let mut cumulative = 0_u128;
        let mut previous = 0;
        for channel in channels {
            cumulative += channel.frame_bits as u128 * u128::from(channel.attribute.get());
            let end = usize::try_from(
                (cumulative * total_bits as u128)
                    .checked_div(total)
                    .unwrap_or(0),
            )
            .map_err(|_| DspError::Capacity)?;
            matched.push(MatchedTransport {
                id: channel.id,
                input_bits: channel.frame_bits,
                output_bits: end - previous,
            });
            previous = end;
        }
        Ok(Self {
            layout,
            channels: matched,
            total_bits,
        })
    }
    pub fn layout(&self) -> UplinkLayout {
        self.layout
    }
    pub fn total_bits(&self) -> usize {
        self.total_bits
    }
    pub fn channels(&self) -> &[MatchedTransport] {
        &self.channels
    }
}
