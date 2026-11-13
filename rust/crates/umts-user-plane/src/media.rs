// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    WireError,
    iuup::{
        Frame, FrameQuality, PayloadCheck,
        initialization::{DataPduType, MediaError, Rfci},
        negotiation::Negotiated,
    },
    rtp::IU_CLOCK_HZ,
};
use std::num::{NonZeroU32, NonZeroUsize};
use umts_types::{
    RadioEpoch,
    bits::Bits,
    time::{CHIPS_PER_SECOND, ChipInstant},
};

pub const MAX_MEDIA_SLOTS: usize = 256;
const RTP_HALF_RANGE: u32 = 1 << (u32::BITS - 1);
const CHIPS_PER_RTP_TICK: u64 = CHIPS_PER_SECOND / IU_CLOCK_HZ as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Capacity,
    Time,
    Duplicate,
    Subflow,
    Empty,
    Media(MediaError),
}

#[derive(Debug)]
pub enum PublishError<E> {
    Media(Error),
    Admission(E),
}

#[derive(Clone, Copy)]
pub struct RtpAnchor {
    timestamp: u32,
    at: ChipInstant,
    horizon: NonZeroU32,
}

impl RtpAnchor {
    /// Projects either side of the reference within the bounded RTP timestamp half-range.
    pub fn project(self, timestamp: u32) -> Result<ChipInstant, Error> {
        let forward = timestamp.wrapping_sub(self.timestamp);
        let index = if forward < RTP_HALF_RANGE {
            if forward > self.horizon.get() {
                return Err(Error::Time);
            }
            self.at
                .index
                .checked_add(u64::from(forward) * CHIPS_PER_RTP_TICK)
        } else {
            let backward = self.timestamp.wrapping_sub(timestamp);
            if backward > self.horizon.get() {
                return Err(Error::Time);
            }
            self.at
                .index
                .checked_sub(u64::from(backward) * CHIPS_PER_RTP_TICK)
        }
        .ok_or(Error::Time)?;
        Ok(ChipInstant {
            epoch: self.at.epoch,
            index,
        })
    }

    /// Moving the reference preserves every projected playout instant, including RTP wrap.
    pub fn advance(self, timestamp: u32) -> Result<Self, Error> {
        Ok(Self {
            timestamp,
            at: self.map(timestamp)?,
            horizon: self.horizon,
        })
    }

    /// The timestamp is mapped to an explicitly chosen radio playout instant, not arrival time.
    pub fn new(timestamp: u32, at: ChipInstant, horizon: NonZeroU32) -> Result<Self, Error> {
        if horizon.get() >= RTP_HALF_RANGE {
            return Err(Error::Time);
        }
        Ok(Self {
            timestamp,
            at,
            horizon,
        })
    }

    pub fn map(self, timestamp: u32) -> Result<ChipInstant, Error> {
        let delta = timestamp.wrapping_sub(self.timestamp);
        if delta > self.horizon.get() {
            return Err(Error::Time);
        }
        Ok(ChipInstant {
            epoch: self.at.epoch,
            index: self
                .at
                .index
                .checked_add(u64::from(delta) * CHIPS_PER_RTP_TICK)
                .ok_or(Error::Time)?,
        })
    }
}

#[derive(Clone, Copy)]
struct Metadata {
    due: ChipInstant,
    rfci: Rfci,
    number: u8,
    consumed: u8,
}

struct Slot {
    frame: Option<Metadata>,
    payload: Box<[u8]>,
    bytes: usize,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Counters {
    pub accepted: u64,
    pub expired: u64,
    pub overflow: u64,
    pub duplicate: u64,
    pub no_data: u64,
    pub published_subflows: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubflowOffer {
    pub rfci: Rfci,
    pub bits: usize,
}

pub struct MediaQueue {
    negotiated: Negotiated,
    epoch: RadioEpoch,
    minimum: Option<u64>,
    last_completed: Option<u64>,
    slots: Box<[Slot]>,
    counters: Counters,
}

impl MediaQueue {
    pub fn new(
        negotiated: Negotiated,
        epoch: RadioEpoch,
        slots: NonZeroUsize,
        maximum_storage_bytes: usize,
    ) -> Result<Self, Error> {
        let bytes = negotiated.formats().maximum_payload_bytes();
        if slots.get() > MAX_MEDIA_SLOTS
            || slots
                .get()
                .checked_mul(bytes)
                .is_none_or(|storage| storage > maximum_storage_bytes)
        {
            return Err(Error::Capacity);
        }
        Ok(Self {
            negotiated,
            epoch,
            minimum: None,
            last_completed: None,
            slots: (0..slots.get())
                .map(|_| Slot {
                    frame: None,
                    payload: vec![0; bytes].into_boxed_slice(),
                    bytes: 0,
                })
                .collect(),
            counters: Counters::default(),
        })
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }
    pub fn queued(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.frame.is_some())
            .count()
    }
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    pub fn enqueue(&mut self, due: ChipInstant, frame: Frame<'_>) -> Result<(), Error> {
        if !self.eligible(due) {
            return Err(Error::Time);
        }
        let media = self
            .negotiated
            .formats()
            .downlink(frame)
            .map_err(Error::Media)?;
        if self
            .slots
            .iter()
            .any(|slot| slot.frame.is_some_and(|frame| frame.due == due))
        {
            self.counters.duplicate = self.counters.duplicate.saturating_add(1);
            return Err(Error::Duplicate);
        }
        let row = self
            .negotiated
            .formats()
            .lookup(media.rfci)
            .ok_or(Error::Media(MediaError::Rfci))?;
        if row.payload_bits() == 0 {
            self.counters.no_data = self.counters.no_data.saturating_add(1);
            return Ok(());
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.frame.is_none()) else {
            self.counters.overflow = self.counters.overflow.saturating_add(1);
            return Err(Error::Capacity);
        };
        let Frame::Data { payload, .. } = frame else {
            return Err(Error::Media(MediaError::Pdu));
        };
        slot.bytes = row.payload_bits().div_ceil(u8::BITS as usize);
        slot.payload[..slot.bytes].copy_from_slice(&payload[..slot.bytes]);
        slot.frame = Some(Metadata {
            due,
            rfci: media.rfci,
            number: media.number,
            consumed: 0,
        });
        self.counters.accepted = self.counters.accepted.saturating_add(1);
        Ok(())
    }

    pub fn retire_before(&mut self, before: ChipInstant) -> Result<(), Error> {
        if before.epoch != self.epoch || self.minimum.is_some_and(|minimum| before.index < minimum)
        {
            return Err(Error::Time);
        }
        self.minimum = Some(before.index);
        for slot in &mut self.slots {
            if slot
                .frame
                .is_some_and(|frame| frame.due.index < before.index)
            {
                slot.frame = None;
                self.counters.expired = self.counters.expired.saturating_add(1);
            }
        }
        Ok(())
    }

    pub fn offer(&self, due: ChipInstant, subflow: usize) -> Option<SubflowOffer> {
        if !self.eligible(due) || subflow >= self.negotiated.formats().subflows() {
            return None;
        }
        let frame = self
            .slots
            .iter()
            .filter_map(|slot| slot.frame)
            .find(|frame| frame.due == due)?;
        if frame.consumed & (1 << subflow) != 0 {
            return None;
        }
        let length = usize::from(
            *self
                .negotiated
                .formats()
                .lookup(frame.rfci)?
                .lengths()
                .get(subflow)?,
        );
        (length != 0).then_some(SubflowOffer {
            rfci: frame.rfci,
            bits: length,
        })
    }

    /// A subflow is consumed only after its original radio admission succeeds.
    pub fn publish<E>(
        &mut self,
        due: ChipInstant,
        subflow: usize,
        admit: impl FnOnce(Bits<'_>) -> Result<(), E>,
    ) -> Result<(), PublishError<E>> {
        if self.offer(due, subflow).is_none() {
            return Err(PublishError::Media(Error::Subflow));
        }
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.frame.is_some_and(|frame| frame.due == due))
            .ok_or(PublishError::Media(Error::Empty))?;
        let mut metadata = slot.frame.ok_or(PublishError::Media(Error::Empty))?;
        let payload_crc = match self.negotiated.formats().data_type() {
            DataPduType::Crc => PayloadCheck::Valid,
            DataPduType::NoCrc => PayloadCheck::Absent,
        };
        let media = self
            .negotiated
            .formats()
            .downlink(Frame::Data {
                number: metadata.number,
                quality: FrameQuality::Good,
                rfci: metadata.rfci.get(),
                payload_crc,
                payload: &slot.payload[..slot.bytes],
            })
            .map_err(|error| PublishError::Media(Error::Media(error)))?;
        admit(media.subflows()[subflow]).map_err(PublishError::Admission)?;
        metadata.consumed |= 1 << subflow;
        let row = self
            .negotiated
            .formats()
            .lookup(metadata.rfci)
            .ok_or(PublishError::Media(Error::Media(MediaError::Rfci)))?;
        slot.frame = row
            .lengths()
            .iter()
            .enumerate()
            .any(|(index, length)| *length != 0 && metadata.consumed & (1 << index) == 0)
            .then_some(metadata);
        self.counters.published_subflows = self.counters.published_subflows.saturating_add(1);
        if slot.frame.is_none() {
            self.last_completed = Some(due.index);
            self.retire_before(due).map_err(PublishError::Media)?;
        }
        Ok(())
    }

    fn eligible(&self, due: ChipInstant) -> bool {
        due.epoch == self.epoch
            && self.minimum.is_none_or(|minimum| due.index >= minimum)
            && self.last_completed.is_none_or(|last| due.index > last)
    }

    pub fn enqueue_rtp(&mut self, packet: &[u8], anchor: RtpAnchor) -> Result<(), IngressError> {
        let packet = crate::rtp::decode(packet).map_err(IngressError::Wire)?;
        let frame = super::iuup::decode(packet.payload).map_err(IngressError::Wire)?;
        let due = anchor.map(packet.timestamp).map_err(IngressError::Media)?;
        self.enqueue(due, frame).map_err(IngressError::Media)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressError {
    Wire(WireError),
    Media(Error),
}

#[cfg(test)]
mod tests;
