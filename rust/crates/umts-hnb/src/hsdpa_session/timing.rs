// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::dedicated_rx::FrameAnchor;
use umts_dsp::{DspError, hsdpa::feedback_start};
use umts_rrc::hsdpa::FeedbackCycle;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant};

const TTI_CHIPS: u64 = 3 * CHIPS_PER_SLOT;
const TTIS_PER_FRAME: u64 = CHIPS_PER_FRAME / TTI_CHIPS;
const CFN_MODULUS: u64 = 256;

#[derive(Clone, Copy)]
pub struct FeedbackTiming {
    downlink: FrameAnchor,
    uplink: FrameAnchor,
    pdsch_origin: ChipInstant,
}

impl FeedbackTiming {
    pub(super) fn first_acknowledgement(
        &self,
        not_before: ChipInstant,
    ) -> Result<ChipInstant, DspError> {
        // A retained HS configuration may predate this mapping's frame anchor.
        // Never evaluate its old activation against a newer DPCH clock.
        let first = self.next_pdsch(ChipInstant {
            index: not_before.index.max(self.downlink.time.index),
            ..not_before
        })?;
        self.acknowledgement(first)
    }

    pub fn next_pdsch(&self, not_before: ChipInstant) -> Result<ChipInstant, DspError> {
        if not_before.epoch != self.pdsch_origin.epoch {
            return Err(DspError::DiscontinuousTime);
        }
        let offset = not_before.index.saturating_sub(self.pdsch_origin.index);
        let index = offset
            .div_ceil(TTI_CHIPS)
            .checked_mul(TTI_CHIPS)
            .and_then(|offset| self.pdsch_origin.index.checked_add(offset))
            .ok_or(DspError::DiscontinuousTime)?;
        Ok(ChipInstant {
            index,
            ..not_before
        })
    }

    pub fn new(
        downlink: FrameAnchor,
        uplink: FrameAnchor,
        pdsch_origin: ChipInstant,
    ) -> Result<Self, DspError> {
        if downlink.cfn != uplink.cfn
            || downlink.time.epoch != uplink.time.epoch
            || downlink.time.epoch != pdsch_origin.epoch
            || pdsch_origin
                .index
                .checked_sub(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                .is_none_or(|scch| !scch.is_multiple_of(TTI_CHIPS))
        {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(Self {
            downlink,
            uplink,
            pdsch_origin,
        })
    }

    fn frames(&self, pdsch: ChipInstant) -> Result<(FrameAnchor, ChipInstant), DspError> {
        if pdsch.epoch != self.pdsch_origin.epoch
            || pdsch
                .index
                .checked_sub(self.pdsch_origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(TTI_CHIPS))
        {
            return Err(DspError::DiscontinuousTime);
        }
        let frames = pdsch
            .index
            .checked_sub(self.downlink.time.index)
            .ok_or(DspError::DiscontinuousTime)?
            / CHIPS_PER_FRAME;
        let offset = frames
            .checked_mul(CHIPS_PER_FRAME)
            .ok_or(DspError::DiscontinuousTime)?;
        let downlink = FrameAnchor {
            time: ChipInstant {
                index: self
                    .downlink
                    .time
                    .index
                    .checked_add(offset)
                    .ok_or(DspError::DiscontinuousTime)?,
                ..pdsch
            },
            cfn: ((u64::from(self.downlink.cfn) + frames % CFN_MODULUS) % CFN_MODULUS) as u8,
        };
        let uplink = ChipInstant {
            index: self
                .uplink
                .time
                .index
                .checked_add(offset)
                .ok_or(DspError::DiscontinuousTime)?,
            ..pdsch
        };
        Ok((downlink, uplink))
    }

    pub fn acknowledgement(&self, pdsch: ChipInstant) -> Result<ChipInstant, DspError> {
        let (downlink, uplink) = self.frames(pdsch)?;
        feedback_start(pdsch, downlink.time, uplink)
    }

    pub fn cqi_subframe(
        &self,
        pdsch: ChipInstant,
        cycle: FeedbackCycle,
    ) -> Result<Option<ChipInstant>, DspError> {
        let Some(period) = cycle.period_ttis() else {
            return Ok(None);
        };
        let (downlink, uplink) = self.frames(pdsch)?;
        let ack = feedback_start(pdsch, downlink.time, uplink)?;
        // TS 25.214 6A.1.2.1 uses CFN and ceil(m*256/7680), not the global HS slot index.
        let ordinal = TTIS_PER_FRAME * u64::from(downlink.cfn)
            + (ack.index - uplink.index).div_ceil(TTI_CHIPS);
        if !ordinal.is_multiple_of(u64::from(period.get())) {
            return Ok(None);
        }
        Ok(Some(ack))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;

    #[test]
    fn cqi_cycle_uses_corresponding_cfn_and_feedback_subframe_phase() {
        let timing = FeedbackTiming::new(
            FrameAnchor {
                time: at(0),
                cfn: 0,
            },
            FrameAnchor {
                time: at(1024),
                cfn: 0,
            },
            at(5120),
        )
        .unwrap();
        assert_eq!(timing.acknowledgement(at(5120)), Ok(at(32000)));
        assert_eq!(
            timing.cqi_subframe(at(5120), FeedbackCycle::Ms10),
            Ok(Some(at(32000)))
        );
        assert_eq!(timing.cqi_subframe(at(5120), FeedbackCycle::Ms4), Ok(None));
        assert_eq!(
            timing.cqi_subframe(at(12800), FeedbackCycle::Ms4),
            Ok(Some(at(39680)))
        );
        assert_eq!(
            timing.cqi_subframe(at(43520), FeedbackCycle::Ms4),
            Ok(Some(at(70400)))
        );
        assert!(timing.acknowledgement(at(0)).is_err());
        assert!(timing.acknowledgement(at(5121)).is_err());
        let wrap = FeedbackTiming::new(
            FrameAnchor {
                time: at(0),
                cfn: 255,
            },
            FrameAnchor {
                time: at(1024),
                cfn: 255,
            },
            at(5120),
        )
        .unwrap();
        assert_eq!(
            wrap.cqi_subframe(at(5120), FeedbackCycle::Ms160),
            Ok(Some(at(32000)))
        );
        assert_eq!(wrap.cqi_subframe(at(43520), FeedbackCycle::Ms160), Ok(None));
    }
}
