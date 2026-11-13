// SPDX-License-Identifier: AGPL-3.0-or-later

use super::Report;
use crate::dedicated_rx::{FrameAnchor, ReceiveIdentity};
use umts_dsp::{
    dedicated::DpchTiming,
    hsupa::{HarqFeedbackTiming, Indicator},
    transport_blocks::BlockOutcome,
};
use umts_mac::hsupa::Reception;
use umts_types::{
    hsupa::Tti,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackError {
    Identity,
    Time,
    Result,
    Late {
        expected: ChipInstant,
        next_transmit: ChipInstant,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct FeedbackRequest {
    pub identity: ReceiveIdentity,
    pub absolute_tti: u64,
    pub timing: HarqFeedbackTiming,
    pub indicator: Indicator,
}

pub struct FeedbackAssociation {
    identity: ReceiveIdentity,
    uplink: FrameAnchor,
    common_frame: ChipInstant,
    interval: Tti,
    dpch: DpchTiming,
}

impl FeedbackAssociation {
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub fn interval(&self) -> Tti {
        self.interval
    }
    // Both anchors must refer to the same radio frame association, not RX arrival time.
    pub fn new(
        identity: ReceiveIdentity,
        uplink: FrameAnchor,
        common_frame: ChipInstant,
        interval: Tti,
        dpch: DpchTiming,
    ) -> Result<Self, FeedbackError> {
        if common_frame.epoch != identity.ue.epoch
            || uplink.time.epoch != identity.ue.epoch
            || !common_frame.index.is_multiple_of(CHIPS_PER_FRAME)
        {
            return Err(FeedbackError::Time);
        }
        Ok(Self {
            identity,
            uplink,
            common_frame,
            interval,
            dpch,
        })
    }

    pub fn request(
        &self,
        report: Report,
        next_transmit: ChipInstant,
    ) -> Result<FeedbackRequest, FeedbackError> {
        if report.identity != self.identity {
            return Err(FeedbackError::Identity);
        }
        if report.start.epoch != self.identity.ue.epoch
            || next_transmit.epoch != self.identity.ue.epoch
        {
            return Err(FeedbackError::Time);
        }
        let delta = report
            .start
            .index
            .checked_sub(self.uplink.time.index)
            .ok_or(FeedbackError::Time)?;
        if !delta.is_multiple_of(self.interval.chips()) {
            return Err(FeedbackError::Time);
        }
        let expected = u64::from(
            self.interval
                .number(self.uplink.cfn, 0)
                .map_err(|_| FeedbackError::Time)?,
        )
        .checked_add(delta / self.interval.chips())
        .ok_or(FeedbackError::Time)?;
        if report.absolute_tti != expected {
            return Err(FeedbackError::Time);
        }
        let frame = ChipInstant {
            index: self
                .common_frame
                .index
                .checked_add(delta / CHIPS_PER_FRAME * CHIPS_PER_FRAME)
                .ok_or(FeedbackError::Time)?,
            ..self.common_frame
        };
        let subframe = ((delta % CHIPS_PER_FRAME) / self.interval.chips()) as u8;
        let timing =
            HarqFeedbackTiming::for_transmission(frame, subframe, self.interval, self.dpch)
                .map_err(|_| FeedbackError::Time)?;
        if timing.start.index < next_transmit.index {
            return Err(FeedbackError::Late {
                expected: timing.start,
                next_transmit,
            });
        }
        let indicator = match report.feedback {
            Reception::Ack { .. } if report.outcome == BlockOutcome::Decoded => Indicator::Positive,
            Reception::Nack if report.outcome != BlockOutcome::Decoded => Indicator::Negative,
            _ => return Err(FeedbackError::Result),
        };
        Ok(FeedbackRequest {
            identity: self.identity,
            absolute_tti: report.absolute_tti,
            timing,
            indicator,
        })
    }
}
