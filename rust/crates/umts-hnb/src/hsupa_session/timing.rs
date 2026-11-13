// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::dedicated_rx::FrameAnchor;
use umts_rrc::dedicated::{DedicatedError, ScheduledActivation};
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant};

pub(super) fn receive_activation(
    activation: ScheduledActivation,
    uplink: FrameAnchor,
) -> Result<ChipInstant, DedicatedError> {
    let target = activation.time();
    if target.epoch != uplink.time.epoch {
        return Err(DedicatedError::Time);
    }
    // TS 25.211 7.6.3 separates UL/DL frame timing while the activation CFN is shared.
    let frames = target
        .index
        .checked_sub(uplink.time.index)
        .ok_or(DedicatedError::Time)?
        .div_ceil(CHIPS_PER_FRAME);
    if uplink.cfn.wrapping_add(frames as u8) != activation.cfn() {
        return Err(DedicatedError::Time);
    }
    let index = frames
        .checked_mul(CHIPS_PER_FRAME)
        .and_then(|offset| uplink.time.index.checked_add(offset))
        .ok_or(DedicatedError::Time)?;
    Ok(ChipInstant {
        epoch: target.epoch,
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;
    use std::num::NonZeroU8;

    #[test]
    fn uplink_activation_preserves_frame_offset_across_cfn_wrap() {
        let offset = 1024;
        for cfn in [0_u8, 254] {
            let activation =
                ScheduledActivation::new(at(0), cfn, NonZeroU8::new(4).unwrap()).unwrap();
            let uplink = FrameAnchor {
                time: at(offset),
                cfn,
            };
            assert_eq!(
                receive_activation(activation, uplink).unwrap(),
                at(4 * CHIPS_PER_FRAME + offset)
            );
        }
    }

    #[test]
    fn uplink_activation_rejects_inconsistent_cfn() {
        let activation = ScheduledActivation::new(at(0), 0, NonZeroU8::new(4).unwrap()).unwrap();
        let uplink = FrameAnchor {
            time: at(1024),
            cfn: 1,
        };
        assert_eq!(
            receive_activation(activation, uplink),
            Err(DedicatedError::Time)
        );
    }
}
