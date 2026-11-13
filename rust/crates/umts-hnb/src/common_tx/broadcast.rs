// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_dsp::{
    bits::{Bit, unpack_msb},
    broadcast::{BroadcastEncoder, FRAME_BITS, PAYLOAD_BITS, TTI_BITS},
};
use umts_rrc::broadcast::{BroadcastSchedule, ScheduleError};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, ChipInstant, SYSTEM_FRAME_MODULUS},
};

use super::{BCH_FRAMES, CommonTxError};

const CYCLE_BLOCKS: usize = (SYSTEM_FRAME_MODULUS / BCH_FRAMES) as usize;

pub(super) struct BroadcastCycle {
    epoch: RadioEpoch,
    blocks: Vec<[Bit; TTI_BITS]>,
}

impl BroadcastCycle {
    pub(super) fn new(
        schedule: &BroadcastSchedule,
        origin: ChipInstant,
    ) -> Result<Self, CommonTxError> {
        schedule
            .message_at(origin)
            .map_err(CommonTxError::Schedule)?;
        let encoder = BroadcastEncoder::new()?;
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(CYCLE_BLOCKS)
            .map_err(|_| CommonTxError::Capacity)?;
        for frame in (0..SYSTEM_FRAME_MODULUS).step_by(BCH_FRAMES as usize) {
            let time = ChipInstant {
                epoch: origin.epoch,
                index: frame
                    .checked_mul(CHIPS_PER_FRAME)
                    .ok_or(CommonTxError::Time)?,
            };
            let encoded = schedule.message_at(time).map_err(CommonTxError::Schedule)?;
            let mut payload = [Bit::Zero; PAYLOAD_BITS];
            unpack_msb(encoded, &mut payload)?;
            let mut coded = [Bit::Zero; TTI_BITS];
            encoder.encode(&payload, &mut coded)?;
            blocks.push(coded);
        }
        Ok(Self {
            epoch: origin.epoch,
            blocks,
        })
    }

    pub(super) fn frame_at(&self, time: ChipInstant) -> Result<&[Bit; FRAME_BITS], CommonTxError> {
        if time.epoch != self.epoch {
            return Err(CommonTxError::Schedule(ScheduleError::WrongEpoch));
        }
        if !time.index.is_multiple_of(CHIPS_PER_FRAME) {
            return Err(CommonTxError::Schedule(ScheduleError::UnalignedTime));
        }
        let frame = usize::from(time.system_frame());
        let block = &self.blocks[frame / BCH_FRAMES as usize];
        let first = frame % BCH_FRAMES as usize * FRAME_BITS;
        Ok(block[first..first + FRAME_BITS]
            .try_into()
            .expect("fixed BCH frame"))
    }
}

#[cfg(test)]
mod tests;
