// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ChipReceiveCounters, DedicatedReceiveSession, ReceiveError, TfciAcceptance};
use umts_dsp::{
    dedicated::{LikelihoodSettings, UplinkFrame, UplinkGains, UplinkLayout},
    dedicated_transport::UplinkTfcs,
    soft_bits::normalize_soft_bits,
    tfci::{Tfci, UPLINK_CODE_BITS},
};
use umts_types::time::{CHIPS_PER_FRAME, CONNECTION_FRAME_MODULUS, ChipInstant, SLOTS_PER_FRAME};

#[derive(Clone, Copy)]
struct Plan {
    frames: usize,
    layout: UplinkLayout,
    widths: [usize; UplinkLayout::MAX_CHANNELS],
    frame_bits: usize,
}

impl Plan {
    fn from_tfcs(tfcs: &UplinkTfcs) -> Option<Self> {
        let mut intervals = tfcs.configured().transport_ids().map(|id| {
            tfcs.configured()
                .definition(id)
                .expect("configured transport")
                .interval
                .frames()
        });
        let frames = intervals.next()?;
        if frames == 1 || intervals.any(|interval| interval != frames) {
            return None;
        }
        let mut layouts = tfcs.configured().combinations().iter().filter_map(|row| {
            let layout = tfcs
                .allocation(row.tfci)
                .expect("configured allocation")
                .layout();
            (!layout.is_empty()).then_some(layout)
        });
        let layout = layouts.next()?;
        if layouts.any(|value| value != layout) {
            return None;
        }
        let mut widths = [0; UplinkLayout::MAX_CHANNELS];
        for (index, width) in widths.iter_mut().enumerate().take(layout.len()) {
            *width = layout
                .channel(index)?
                .bits_per_slot()
                .checked_mul(SLOTS_PER_FRAME as usize)?;
        }
        let frame_bits = widths
            .iter()
            .try_fold(0usize, |total, width| total.checked_add(*width))?;
        Some(Self {
            frames,
            layout,
            widths,
            frame_bits,
        })
    }

    fn storage_bytes(self) -> Option<usize> {
        self.frame_bits
            .checked_mul(self.frames)?
            .checked_mul(size_of::<f64>())?
            .checked_add(size_of::<TtiReceiver>())
    }
}

pub(super) fn maximum_storage_bytes(tfcs: &UplinkTfcs) -> Option<usize> {
    Plan::from_tfcs(tfcs).map_or(Some(0), Plan::storage_bytes)
}

pub(super) struct TtiReceiver {
    plan: Plan,
    gains: UplinkGains,
    data: Vec<f64>,
    control: [f64; UPLINK_CODE_BITS],
    first: Option<ChipInstant>,
    collected: usize,
}

impl TtiReceiver {
    pub(super) fn new(tfcs: &UplinkTfcs, gains: &[(Tfci, UplinkGains)]) -> Option<Self> {
        let plan = Plan::from_tfcs(tfcs)?;
        let mut data_gains = gains.iter().filter_map(|(tfci, gains)| {
            (!tfcs
                .allocation(*tfci)
                .expect("configured allocation")
                .layout()
                .is_empty())
            .then_some(*gains)
        });
        let gains = data_gains.next()?;
        if data_gains
            .any(|value| value.control() != gains.control() || value.data() != gains.data())
        {
            return None;
        }
        Some(Self {
            plan,
            gains,
            data: vec![0.0; plan.frame_bits.checked_mul(plan.frames)?],
            control: [0.0; UPLINK_CODE_BITS],
            first: None,
            collected: 0,
        })
    }

    pub(super) fn storage_bytes(&self) -> usize {
        size_of::<Self>() + self.data.capacity() * size_of::<f64>()
    }

    pub(super) fn feed(
        &mut self,
        frame: &UplinkFrame<'_>,
        session: &mut DedicatedReceiveSession,
        likelihood: LikelihoodSettings,
        acceptance: TfciAcceptance,
        counters: &mut ChipReceiveCounters,
        selection: Option<&super::PreparedDchSelection>,
    ) -> Result<Option<Tfci>, ReceiveError> {
        let start = frame.start();
        let anchor = session.frame_anchor();
        let frame_number = (start.index - anchor.time.index) / CHIPS_PER_FRAME;
        let cfn = anchor
            .cfn
            .wrapping_add((frame_number % CONNECTION_FRAME_MODULUS) as u8);
        let position = usize::from(cfn) % self.plan.frames;
        if position == 0
            || self.first.is_some_and(|first| {
                first.index + self.collected as u64 * CHIPS_PER_FRAME != start.index
            })
        {
            self.first = None;
            self.collected = 0;
            self.control.fill(0.0);
        }
        if position != self.collected {
            return Ok(None);
        }
        let soft = frame
            .tfci_likelihoods(likelihood)?
            .ok_or(ReceiveError::Configuration)?;
        for (total, value) in self.control.iter_mut().zip(soft) {
            *total += value;
        }
        let mut offset = position * self.plan.frame_bits;
        for (channel, &width) in self
            .plan
            .widths
            .iter()
            .enumerate()
            .take(self.plan.layout.len())
        {
            frame.despread_llrs(
                self.plan.layout,
                channel,
                self.gains,
                likelihood,
                &mut self.data[offset..offset + width],
            )?;
            offset += width;
        }
        self.first.get_or_insert(start);
        self.collected += 1;
        if self.collected != self.plan.frames {
            return Ok(None);
        }
        self.collected = 0;
        let first = self.first.take().expect("complete TTI");
        let mut normalized = [0.0; UPLINK_CODE_BITS];
        normalize_soft_bits(&self.control, &mut normalized)?;
        self.control.fill(0.0);
        // TS 25.212 4.2 and 4.2.6 keep each transport block set across its TTI's radio frames.
        // Combining requires equal TTIs and identical physical layout/gains across nonempty TFCs.
        let chosen = selection
            .map_or(session.tfcs.decoder(), |value| &value.decoder)
            .decode(&normalized)?
            .filter(|decision| {
                decision.normalized_score >= acceptance.minimum_score
                    && decision
                        .normalized_margin
                        .is_none_or(|margin| margin >= acceptance.minimum_margin)
            })
            .map(|decision| decision.tfci);
        let Some(tfci) = chosen else {
            counters.rejected_tfci = counters
                .rejected_tfci
                .saturating_add(self.plan.frames as u64);
            return Ok(None);
        };
        let wire_tfci = tfci;
        let tfci = selection.map_or(Ok(tfci), |selection| selection.source(tfci))?;
        let channels = session
            .tfcs
            .allocation(tfci)
            .ok_or(ReceiveError::Configuration)?
            .layout()
            .len();
        for position in 0..self.plan.frames {
            let start = ChipInstant {
                epoch: first.epoch,
                index: first.index + position as u64 * CHIPS_PER_FRAME,
            };
            let frame_number = (start.index - anchor.time.index) / CHIPS_PER_FRAME;
            let cfn = anchor
                .cfn
                .wrapping_add((frame_number % CONNECTION_FRAME_MODULUS) as u8);
            let mut physical: [&[f64]; UplinkLayout::MAX_CHANNELS] =
                [&[]; UplinkLayout::MAX_CHANNELS];
            let mut offset = position * self.plan.frame_bits;
            for (reference, width) in physical.iter_mut().zip(self.plan.widths).take(channels) {
                *reference = &self.data[offset..offset + width];
                offset += width;
            }
            session.receive(session.identity(), start, cfn, tfci, &physical[..channels])?;
        }
        counters.accepted_frames = counters
            .accepted_frames
            .saturating_add(self.plan.frames as u64);
        Ok(Some(wire_tfci))
    }
}
