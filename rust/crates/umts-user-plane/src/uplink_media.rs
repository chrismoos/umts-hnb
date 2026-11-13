// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    WireError,
    iuup::{
        self, Frame, FrameQuality, PayloadCheck,
        initialization::{DataPduType, MAX_SUBFLOWS},
        negotiation::Negotiated,
    },
    media::MAX_MEDIA_SLOTS,
    rtp,
};
use std::num::{NonZeroU64, NonZeroUsize};
use umts_types::{
    RadioEpoch,
    bits::{BITS_PER_BYTE, Bits},
    time::{CHIPS_PER_SECOND, ChipInstant},
};

const MAX_FRAGMENTS: usize = u64::BITS as usize;
const DATA_HEADER_BYTES: usize = 4;
const FRAME_MODULUS: u64 = 16;
const CHIPS_PER_RTP_TICK: u64 = CHIPS_PER_SECOND / rtp::IU_CLOCK_HZ as u64;
const DYNAMIC_PAYLOAD_FIRST: u8 = 96;
const DYNAMIC_PAYLOAD_LAST: u8 = 127;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    No,
    Yes,
    NoDetect,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioQuality {
    Good,
    Bad,
    NotReported,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Capacity,
    Time,
    Retired,
    Geometry,
    Duplicate,
    Ambiguous,
    Wire(WireError),
}
impl From<WireError> for Error {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}

#[derive(Clone, Copy)]
pub struct SourceClock {
    origin: ChipInstant,
    interval: NonZeroU64,
    timestamp: u32,
    frame: u8,
}
impl SourceClock {
    pub fn new(
        origin: ChipInstant,
        interval: NonZeroU64,
        timestamp: u32,
        frame: u8,
    ) -> Result<Self, Error> {
        if frame as u64 >= FRAME_MODULUS || !interval.get().is_multiple_of(CHIPS_PER_RTP_TICK) {
            return Err(Error::Configuration);
        }
        Ok(Self {
            origin,
            interval,
            timestamp,
            frame,
        })
    }
    fn at(self, source: ChipInstant) -> Result<(u8, u32), Error> {
        if source.epoch != self.origin.epoch {
            return Err(Error::Time);
        }
        let delta = source
            .index
            .checked_sub(self.origin.index)
            .ok_or(Error::Time)?;
        if !delta.is_multiple_of(self.interval.get()) {
            return Err(Error::Time);
        }
        Ok((
            ((u64::from(self.frame) + delta / self.interval.get()) % FRAME_MODULUS) as u8,
            self.timestamp
                .wrapping_add((delta / CHIPS_PER_RTP_TICK) as u32),
        ))
    }
    pub fn validate(self, source: ChipInstant) -> Result<(), Error> {
        self.at(source).map(|_| ())
    }
}

#[derive(Clone, Copy)]
pub struct RtpSource {
    pub sequence: u16,
    pub ssrc: u32,
    pub payload_type: u8,
}
#[derive(Debug)]
pub enum RtpPublishError<E> {
    Wire(WireError),
    Transport(E),
}
impl RtpSource {
    pub fn validate(self) -> Result<(), WireError> {
        if (DYNAMIC_PAYLOAD_FIRST..=DYNAMIC_PAYLOAD_LAST).contains(&self.payload_type) {
            Ok(())
        } else {
            Err(WireError::Header)
        }
    }
    pub fn publish<E>(
        &mut self,
        timestamp: u32,
        payload: &[u8],
        output: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<bool, E>,
    ) -> Result<bool, RtpPublishError<E>> {
        self.validate().map_err(RtpPublishError::Wire)?;
        let bytes = rtp::encode(
            rtp::Packet {
                marker: false,
                payload_type: self.payload_type,
                sequence: self.sequence,
                timestamp,
                ssrc: self.ssrc,
                payload,
            },
            output,
        )
        .map_err(RtpPublishError::Wire)?;
        if !send(&output[..bytes]).map_err(RtpPublishError::Transport)? {
            return Ok(false);
        }
        self.sequence = self.sequence.wrapping_add(1);
        Ok(true)
    }
}
#[derive(Clone, Copy)]
pub struct Limits {
    pub frames: NonZeroUsize,
    pub wait_chips: NonZeroU64,
    pub maximum_storage_bytes: usize,
}
pub struct Piece<'a> {
    pub source: ChipInstant,
    pub subflow: usize,
    pub index: usize,
    pub blocks: NonZeroUsize,
    pub quality: RadioQuality,
    pub data: Bits<'a>,
}

#[derive(Clone, Copy, Default)]
struct Part {
    blocks: usize,
    width: usize,
    seen: u64,
    bad: bool,
}
impl Part {
    fn complete(self) -> bool {
        self.blocks != 0 && self.seen == full_mask(self.blocks)
    }
}
fn full_mask(bits: usize) -> u64 {
    if bits == MAX_FRAGMENTS {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

struct Assembly {
    source: ChipInstant,
    candidates: u64,
    parts: [Part; MAX_SUBFLOWS],
}
struct Slot {
    assembly: Option<Assembly>,
    payload: Box<[u8]>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub retired_pieces: u64,
    pub sent: u64,
    pub suppressed: u64,
    pub ambiguous: u64,
    pub missing: u64,
    pub bad_radio: u64,
    pub backpressure: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Idle,
    Waiting,
    Dropped,
    Sent,
    Backpressure,
}
#[derive(Debug)]
pub enum PublishError<E> {
    Media(Error),
    Transport(E),
}

pub struct UplinkAssembler {
    negotiated: Negotiated,
    delivery: Box<[Delivery]>,
    epoch: RadioEpoch,
    offsets: [usize; MAX_SUBFLOWS],
    slots: Box<[Slot]>,
    scratch: Box<[u8]>,
    iuup: Box<[u8]>,
    packet: Box<[u8]>,
    wait: NonZeroU64,
    retired: Option<u64>,
    last_poll: Option<u64>,
    counters: Counters,
}

impl UplinkAssembler {
    pub fn storage_ceiling(payload_bytes: usize, frames: NonZeroUsize) -> Result<usize, Error> {
        payload_bytes
            .checked_mul(MAX_SUBFLOWS)
            .and_then(|bytes| bytes.checked_mul(frames.get()))
            .and_then(|bytes| bytes.checked_add(payload_bytes.checked_mul(3)?))
            .and_then(|bytes| bytes.checked_add(DATA_HEADER_BYTES * 2 + rtp::HEADER_BYTES))
            .ok_or(Error::Capacity)
    }
    pub fn new(
        negotiated: Negotiated,
        epoch: RadioEpoch,
        delivery: &[Delivery],
        limits: Limits,
    ) -> Result<Self, Error> {
        let formats = negotiated.formats();
        if delivery.len() != formats.subflows() || limits.frames.get() > MAX_MEDIA_SLOTS {
            return Err(Error::Configuration);
        }
        let mut offsets = [0; MAX_SUBFLOWS];
        let mut storage = 0;
        for (flow, offset) in offsets.iter_mut().enumerate().take(formats.subflows()) {
            *offset = storage;
            storage += formats
                .rows()
                .map(|row| usize::from(row.lengths()[flow]).div_ceil(BITS_PER_BYTE))
                .max()
                .unwrap_or(0);
        }
        let bytes = formats.maximum_payload_bytes();
        let iuup_bytes = bytes
            .checked_add(DATA_HEADER_BYTES)
            .ok_or(Error::Capacity)?;
        let packet_bytes = iuup_bytes
            .checked_add(rtp::HEADER_BYTES)
            .ok_or(Error::Capacity)?;
        let required = storage
            .checked_mul(limits.frames.get())
            .and_then(|value| value.checked_add(bytes))
            .and_then(|value| value.checked_add(iuup_bytes))
            .and_then(|value| value.checked_add(packet_bytes))
            .ok_or(Error::Capacity)?;
        if required > limits.maximum_storage_bytes {
            return Err(Error::Capacity);
        }
        Ok(Self {
            negotiated,
            epoch,
            delivery: delivery.into(),
            offsets,
            slots: (0..limits.frames.get())
                .map(|_| Slot {
                    assembly: None,
                    payload: vec![0; storage].into_boxed_slice(),
                })
                .collect(),
            scratch: vec![0; bytes].into_boxed_slice(),
            iuup: vec![0; iuup_bytes].into_boxed_slice(),
            packet: vec![0; packet_bytes].into_boxed_slice(),
            wait: limits.wait_chips,
            retired: None,
            last_poll: None,
            counters: Counters::default(),
        })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }
    pub fn counters(&self) -> Counters {
        self.counters
    }

    pub fn push(&mut self, piece: Piece<'_>) -> Result<(), Error> {
        if piece.source.epoch != self.epoch
            || piece.source.index.checked_add(self.wait.get()).is_none()
        {
            return Err(Error::Time);
        }
        if piece.subflow >= self.delivery.len()
            || piece.blocks.get() > MAX_FRAGMENTS
            || piece.index >= piece.blocks.get()
            || piece.data.is_empty()
        {
            return Err(Error::Geometry);
        }
        let total = piece
            .data
            .len()
            .checked_mul(piece.blocks.get())
            .ok_or(Error::Capacity)?;
        let candidates = self
            .negotiated
            .formats()
            .rows()
            .filter(|row| usize::from(row.lengths()[piece.subflow]) == total)
            .fold(0, |mask, row| mask | (1 << row.rfci().get()));
        if candidates == 0 {
            return Err(Error::Geometry);
        }
        if self
            .retired
            .is_some_and(|retired| piece.source.index <= retired)
        {
            self.counters.retired_pieces = self.counters.retired_pieces.saturating_add(1);
            return Err(Error::Retired);
        }
        let index = if let Some(index) = self.slots.iter().position(|slot| {
            slot.assembly
                .as_ref()
                .is_some_and(|assembly| assembly.source == piece.source)
        }) {
            index
        } else {
            self.slots
                .iter()
                .position(|slot| slot.assembly.is_none())
                .ok_or(Error::Capacity)?
        };
        if let Some(assembly) = &self.slots[index].assembly {
            let part = assembly.parts[piece.subflow];
            if assembly.candidates & candidates == 0
                || (part.blocks != 0
                    && (part.blocks != piece.blocks.get() || part.width != piece.data.len()))
            {
                return Err(Error::Geometry);
            }
            if part.seen & (1 << piece.index) != 0 {
                return Err(Error::Duplicate);
            }
        } else {
            self.slots[index].payload.fill(0);
            self.slots[index].assembly = Some(Assembly {
                source: piece.source,
                candidates,
                parts: [Part::default(); MAX_SUBFLOWS],
            });
        }
        let slot = &mut self.slots[index];
        piece
            .data
            .copy_into(
                &mut slot.payload,
                self.offsets[piece.subflow] * BITS_PER_BYTE + piece.index * piece.data.len(),
            )
            .map_err(|_| Error::Capacity)?;
        let assembly = slot.assembly.as_mut().ok_or(Error::Geometry)?;
        assembly.candidates &= candidates;
        let part = &mut assembly.parts[piece.subflow];
        part.blocks = piece.blocks.get();
        part.width = piece.data.len();
        part.seen |= 1 << piece.index;
        part.bad |= piece.quality == RadioQuality::Bad;
        Ok(())
    }

    pub fn publish<E>(
        &mut self,
        now: ChipInstant,
        clock: SourceClock,
        rtp: &mut RtpSource,
        send: impl FnOnce(&[u8]) -> Result<bool, E>,
    ) -> Result<Progress, PublishError<E>> {
        if now.epoch != self.epoch || self.last_poll.is_some_and(|last| now.index < last) {
            return Err(PublishError::Media(Error::Time));
        }
        self.last_poll = Some(now.index);
        let Some((slot_index, assembly)) = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.assembly.as_ref().map(|assembly| (index, assembly)))
            .min_by_key(|(_, assembly)| assembly.source.index)
        else {
            return Ok(Progress::Idle);
        };
        if now.index < assembly.source.index {
            return Ok(Progress::Waiting);
        }
        let expired = now.index - assembly.source.index >= self.wait.get();
        let mut candidates = self
            .negotiated
            .formats()
            .rows()
            .filter(|row| assembly.candidates & (1 << row.rfci().get()) != 0);
        let row = candidates
            .next()
            .ok_or(PublishError::Media(Error::Ambiguous))?;
        if candidates.any(|other| other.lengths() != row.lengths()) {
            if !expired {
                return Ok(Progress::Waiting);
            }
            self.counters.ambiguous = self.counters.ambiguous.saturating_add(1);
            self.retired = Some(assembly.source.index);
            self.slots[slot_index].assembly = None;
            return Ok(Progress::Dropped);
        }
        let mut quality = FrameQuality::Good;
        let mut missing = false;
        let mut suppress = false;
        for (subflow, (&length, policy)) in row.lengths().iter().zip(&self.delivery).enumerate() {
            if length == 0 {
                continue;
            }
            let part = assembly.parts[subflow];
            if !part.complete() {
                if !expired {
                    return Ok(Progress::Waiting);
                }
                missing = true;
            }
            let bad = part.bad || !part.complete();
            match policy {
                Delivery::No if bad => suppress = true,
                Delivery::Yes if bad => quality = FrameQuality::BadRadio,
                Delivery::NoDetect if !part.complete() => suppress = true,
                _ => (),
            }
        }
        let source = assembly.source;
        if suppress {
            self.counters.suppressed = self.counters.suppressed.saturating_add(1);
            self.counters.missing = self.counters.missing.saturating_add(u64::from(missing));
            self.retired = Some(source.index);
            self.slots[slot_index].assembly = None;
            return Ok(Progress::Dropped);
        }
        let mut subflows = [Bits::empty(); MAX_SUBFLOWS];
        for (subflow, &length) in row.lengths().iter().enumerate() {
            subflows[subflow] = Bits::new(
                &self.slots[slot_index].payload[self.offsets[subflow]..],
                usize::from(length),
            )
            .map_err(|_| PublishError::Media(Error::Geometry))?;
        }
        let payload_bytes = self
            .negotiated
            .formats()
            .pack(
                row.rfci(),
                &subflows[..self.delivery.len()],
                &mut self.scratch,
            )
            .map_err(|error| PublishError::Media(error.into()))?;
        let (number, timestamp) = clock.at(source).map_err(PublishError::Media)?;
        let payload_crc = match self.negotiated.formats().data_type() {
            DataPduType::Crc => PayloadCheck::Valid,
            DataPduType::NoCrc => PayloadCheck::Absent,
        };
        let bytes = iuup::encode(
            Frame::Data {
                number,
                quality,
                rfci: row.rfci().get(),
                payload_crc,
                payload: &self.scratch[..payload_bytes],
            },
            &mut self.iuup,
        )
        .map_err(|error| PublishError::Media(error.into()))?;
        if !rtp
            .publish(timestamp, &self.iuup[..bytes], &mut self.packet, send)
            .map_err(|error| match error {
                RtpPublishError::Wire(error) => PublishError::Media(error.into()),
                RtpPublishError::Transport(error) => PublishError::Transport(error),
            })?
        {
            self.counters.backpressure = self.counters.backpressure.saturating_add(1);
            return Ok(Progress::Backpressure);
        }
        self.retired = Some(source.index);
        self.slots[slot_index].assembly = None;
        self.counters.sent = self.counters.sent.saturating_add(1);
        self.counters.missing = self.counters.missing.saturating_add(u64::from(missing));
        self.counters.bad_radio = self
            .counters
            .bad_radio
            .saturating_add(u64::from(quality == FrameQuality::BadRadio));
        Ok(Progress::Sent)
    }
}

#[cfg(test)]
mod tests;
