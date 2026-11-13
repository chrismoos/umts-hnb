// SPDX-License-Identifier: AGPL-3.0-or-later

#[cfg(test)]
use crate::common_control::RachMacBatch;
use crate::{access_message::MessageDelivery, common_control::ControlDecision};
use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use umts_dsp::{bits::pack_msb, prach_message::DecodedPrachMessage};
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_types::{
    RadioEpoch,
    bits::{BITS_PER_BYTE, Bits},
    time::ChipInstant,
};

const MAX_TRANSPORTS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct RachQueueLimits {
    pub capacity: NonZeroUsize,
    pub maximum_transport_bits: NonZeroUsize,
    pub maximum_buffer_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressError {
    Capacity,
    Closed,
    Epoch,
    Transport,
    PoolInvariant,
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    PartialTransport,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct RachPdu<'a> {
    pub transport: DecodedPrachMessage,
    pub index: usize,
    pub blocks: usize,
    pub pdu: Result<umts_mac::CommonPdu<'a>, umts_mac::MacError>,
}

struct Frame {
    bytes: Vec<u8>,
    transport: Option<DecodedPrachMessage>,
    block_bits: usize,
    blocks: usize,
}

pub(super) struct RachPublisher {
    epoch: RadioEpoch,
    maximum_bits: usize,
    alive: Arc<AtomicBool>,
    free: Receiver<Frame>,
    recycle: Sender<Frame>,
    ready: Sender<Frame>,
}

pub struct RachIngress {
    epoch: RadioEpoch,
    alive: Arc<AtomicBool>,
    ready: Receiver<Frame>,
    free: Sender<Frame>,
    pending: Option<Frame>,
    next_block: usize,
}

pub(super) fn channel(
    epoch: RadioEpoch,
    limits: RachQueueLimits,
) -> Result<(RachPublisher, RachIngress), IngressError> {
    let bytes = limits.maximum_transport_bits.get().div_ceil(BITS_PER_BYTE);
    if limits.capacity.get() > MAX_TRANSPORTS
        || bytes
            .checked_mul(limits.capacity.get())
            .is_none_or(|total| total > limits.maximum_buffer_bytes)
    {
        return Err(IngressError::Capacity);
    }
    let alive = Arc::new(AtomicBool::new(true));
    let (ready, receive) = mailbox::bounded(limits.capacity);
    let (recycle, free) = mailbox::bounded(limits.capacity);
    for _ in 0..limits.capacity.get() {
        recycle
            .try_send(Frame {
                bytes: vec![0; bytes],
                transport: None,
                block_bits: 0,
                blocks: 0,
            })
            .map_err(|_| IngressError::PoolInvariant)?;
    }
    Ok((
        RachPublisher {
            epoch,
            maximum_bits: limits.maximum_transport_bits.get(),
            alive: alive.clone(),
            free,
            recycle: recycle.clone(),
            ready,
        },
        RachIngress {
            epoch,
            alive,
            ready: receive,
            free: recycle,
            pending: None,
            next_block: 0,
        },
    ))
}

impl RachPublisher {
    pub(super) fn paired_with(&self, ingress: &RachIngress) -> bool {
        Arc::ptr_eq(&self.alive, &ingress.alive)
    }
    #[cfg(test)]
    pub(crate) fn inject_transport(
        &mut self,
        transport: DecodedPrachMessage,
        block_bits: usize,
        blocks: usize,
        bytes: &[u8],
    ) {
        RachMacBatch::from_transport(
            transport,
            block_bits,
            blocks,
            Bits::new(bytes, transport.payload_bits).unwrap(),
        )
        .unwrap();
        let mut frame = self.free.try_recv().expect("fixture transport capacity");
        frame.bytes[..bytes.len()].copy_from_slice(bytes);
        frame.transport = Some(transport);
        frame.block_bits = block_bits;
        frame.blocks = blocks;
        assert!(self.ready.try_send(frame).is_ok());
    }
    pub fn available(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    pub fn maximum_bits(&self) -> usize {
        self.maximum_bits
    }
    pub fn publish_one<'a>(
        &mut self,
        decode: impl FnOnce() -> Result<Option<MessageDelivery<'a>>, super::AccessError>,
    ) -> Result<super::DecodeProgress, super::AccessError> {
        if !self.available() {
            return Err(super::AccessError::Ingress(IngressError::Closed));
        }
        let Some(mut frame) = self.free.try_recv() else {
            return Ok(super::DecodeProgress::Backpressure);
        };
        let decoded = decode();
        let result = match decoded {
            Ok(Some(message)) => {
                if message.decoded.start.epoch != self.epoch {
                    Err(IngressError::Epoch)
                } else if message.block_bits.checked_mul(message.blocks)
                    != Some(message.payload.len())
                    || message.blocks == 0
                    || message.block_bits == 0
                    || message.decoded.payload_bits != message.payload.len()
                    || message.payload.len() > self.maximum_bits
                {
                    Err(IngressError::Transport)
                } else {
                    let bytes = message.payload.len().div_ceil(BITS_PER_BYTE);
                    if pack_msb(message.payload, &mut frame.bytes[..bytes]).is_err() {
                        Err(IngressError::Transport)
                    } else {
                        frame.transport = Some(message.decoded);
                        frame.block_bits = message.block_bits;
                        frame.blocks = message.blocks;
                        match self.ready.try_send(frame) {
                            Ok(()) => {
                                return if self.alive.fetch_and(true, Ordering::AcqRel) {
                                    Ok(super::DecodeProgress::Published)
                                } else {
                                    Err(super::AccessError::Ingress(IngressError::Closed))
                                };
                            }
                            Err(returned) => {
                                frame = returned;
                                Err(IngressError::PoolInvariant)
                            }
                        }
                    }
                }
            }
            Ok(None) => Ok(super::DecodeProgress::Idle),
            Err(error) => {
                self.recycle
                    .try_send(frame)
                    .map_err(|_| super::AccessError::Ingress(IngressError::PoolInvariant))?;
                return Err(error);
            }
        };
        self.recycle
            .try_send(frame)
            .map_err(|_| super::AccessError::Ingress(IngressError::PoolInvariant))?;
        result.map_err(super::AccessError::Ingress)
    }
}

impl RachIngress {
    pub fn epoch(&self) -> RadioEpoch {
        self.epoch
    }
    fn prepare(&mut self) -> bool {
        if self.pending.is_none() {
            self.pending = self.ready.try_recv();
        }
        self.pending.is_some()
    }

    #[cfg(test)]
    pub fn process_one_with<T>(
        &mut self,
        process: impl FnOnce(RachMacBatch<'_>) -> ControlDecision<T>,
    ) -> Result<Option<ControlDecision<T>>, IngressError> {
        if !self.prepare() {
            return Ok(None);
        }
        if self.next_block != 0 {
            return Err(IngressError::PartialTransport);
        }
        let frame = self.pending.as_ref().expect("pending transport");
        let transport = frame.transport.expect("published transport");
        let batch = RachMacBatch::from_transport(
            transport,
            frame.block_bits,
            frame.blocks,
            Bits::new(
                &frame.bytes[..transport.payload_bits.div_ceil(BITS_PER_BYTE)],
                transport.payload_bits,
            )
            .map_err(|_| IngressError::Transport)?,
        )
        .map_err(|_| IngressError::Transport)?;
        let result = process(batch);
        if matches!(result, ControlDecision::Consume(_)) {
            self.recycle_pending()?;
        }
        Ok(Some(result))
    }

    pub fn process_pdu_one_with<T>(
        &mut self,
        process: impl FnOnce(RachPdu<'_>) -> ControlDecision<T>,
    ) -> Result<Option<ControlDecision<T>>, IngressError> {
        if !self.prepare() {
            return Ok(None);
        }
        let frame = self.pending.as_ref().expect("pending transport");
        let transport = frame.transport.expect("published transport");
        let start = self
            .next_block
            .checked_mul(frame.block_bits)
            .ok_or(IngressError::Transport)?;
        let end = start
            .checked_add(frame.block_bits)
            .ok_or(IngressError::Transport)?;
        let bits = Bits::new(
            &frame.bytes[..transport.payload_bits.div_ceil(BITS_PER_BYTE)],
            transport.payload_bits,
        )
        .and_then(|bits| bits.subspan(start..end))
        .map_err(|_| IngressError::Transport)?;
        let pdu = umts_mac::decode_common(umts_mac::FddCommonChannel::Rach, bits);
        let result = process(RachPdu {
            transport,
            index: self.next_block,
            blocks: frame.blocks,
            pdu,
        });
        if matches!(result, ControlDecision::Consume(_)) {
            self.next_block += 1;
            if self.next_block == frame.blocks {
                self.recycle_pending()?;
            }
        }
        Ok(Some(result))
    }

    pub fn expire_one_before(&mut self, first: ChipInstant) -> Result<bool, IngressError> {
        if first.epoch != self.epoch {
            return Err(IngressError::Epoch);
        }
        if self.prepare()
            && self
                .pending
                .as_ref()
                .expect("pending transport")
                .transport
                .expect("published transport")
                .start
                .index
                < first.index
        {
            self.recycle_pending()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn recycle_pending(&mut self) -> Result<(), IngressError> {
        let mut frame = self.pending.take().expect("pending transport");
        frame.transport = None;
        self.next_block = 0;
        self.free
            .try_send(frame)
            .map_err(|_| IngressError::PoolInvariant)
    }
}

impl Drop for RachPublisher {
    fn drop(&mut self) {
        self.alive.swap(false, Ordering::AcqRel);
    }
}
impl Drop for RachIngress {
    fn drop(&mut self) {
        self.alive.swap(false, Ordering::AcqRel);
    }
}
