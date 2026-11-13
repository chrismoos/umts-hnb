// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_types::{
    hsupa::Tti,
    time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant},
};

use crate::{DspError, dedicated::DpchTiming};

const OFFSET_QUANTUM: i128 = 256;
const SUBFRAME_CHIPS: i128 = 3 * CHIPS_PER_SLOT as i128;
const HICH_BASE_CHIPS: i128 = 2 * CHIPS_PER_SLOT as i128;
const TEN_MS_OFFSET_UNITS: i128 = -70;
const TWO_MS_OFFSET_UNITS: i128 = 50;
const SUBFRAME_OFFSET_UNITS: i128 = 30;
const FEEDBACK_DELAY: i128 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarqFeedbackTiming {
    pub start: ChipInstant,
    pub active_chips: u64,
}

impl HarqFeedbackTiming {
    // Frame is the P-CCPCH frame associated with the UL transmission, not its raw RX timestamp.
    // TS 25.211 7.10 allows a negative E-HICH frame offset for a 10 ms TTI.
    pub fn for_transmission(
        frame: ChipInstant,
        subframe: u8,
        interval: Tti,
        dpch: DpchTiming,
    ) -> Result<Self, DspError> {
        if !frame.index.is_multiple_of(CHIPS_PER_FRAME) {
            return Err(DspError::DiscontinuousTime);
        }
        interval
            .number(0, subframe)
            .map_err(|_| DspError::InvalidLength)?;
        let offset_units = i128::from(dpch.chips()) / OFFSET_QUANTUM
            + match interval {
                Tti::TwoMilliseconds => TWO_MS_OFFSET_UNITS,
                Tti::TenMilliseconds => TEN_MS_OFFSET_UNITS,
            };
        let offset =
            HICH_BASE_CHIPS + SUBFRAME_CHIPS * offset_units.div_euclid(SUBFRAME_OFFSET_UNITS);
        // TS 25.214 6B.3 associates feedback with UL three TTIs earlier for either TTI length.
        let index = i128::from(frame.index)
            + offset
            + (i128::from(subframe) + FEEDBACK_DELAY) * i128::from(interval.chips());
        let index = u64::try_from(index).map_err(|_| DspError::DiscontinuousTime)?;
        Ok(Self {
            start: ChipInstant { index, ..frame },
            active_chips: match interval {
                Tti::TwoMilliseconds => 3 * CHIPS_PER_SLOT,
                Tti::TenMilliseconds => 12 * CHIPS_PER_SLOT,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;
    use umts_types::RadioEpoch;

    #[test]
    fn negative_frame_offset_and_two_ms_wrap_preserve_feedback_association() {
        let frame = ChipInstant {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            index: 0,
        };
        let dpch = DpchTiming::new(0).unwrap();
        let long =
            HarqFeedbackTiming::for_transmission(frame, 0, Tti::TenMilliseconds, dpch).unwrap();
        assert_eq!(long.start.index, 97_280);
        assert_eq!(long.active_chips, 30_720);
        for subframe in 0..5 {
            let short =
                HarqFeedbackTiming::for_transmission(frame, subframe, Tti::TwoMilliseconds, dpch)
                    .unwrap();
            assert_eq!(short.start.index, 35_840 + u64::from(subframe) * 7_680);
            assert_eq!(short.active_chips, 7_680);
        }
        assert!(
            HarqFeedbackTiming::for_transmission(frame, 1, Tti::TenMilliseconds, dpch).is_err()
        );
        assert!(
            HarqFeedbackTiming::for_transmission(
                ChipInstant { index: 1, ..frame },
                0,
                Tti::TwoMilliseconds,
                dpch
            )
            .is_err()
        );
    }
}
