// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{Error, Limits, Transport};
use umts_iuh::{
    Domain,
    session::{AssociationId, Frame, FrameScope},
};
use umts_sctp::SendResult;
use umts_types::UeKey;
// Iuh transmits HNBAP and RUA on SCTP stream 0 (TS 25.467 and TS 25.468).
pub(crate) const CONTROL_STREAM: u16 = 0;

struct Slot {
    scope: FrameScope,
    cancelled: bool,
    ppid: u32,
    length: usize,
    bytes: Vec<u8>,
}
pub(crate) struct Outbox {
    association: AssociationId,
    slots: Vec<Slot>,
    head: usize,
    length: usize,
}
impl Outbox {
    pub(crate) fn new(association: AssociationId, limits: Limits) -> Result<Self, Error> {
        const MAX_RELEASE_FRAMES: usize = 3;
        if limits.outbound_entries.get() < MAX_RELEASE_FRAMES {
            return Err(Error::Capacity);
        }
        let retained = limits
            .outbound_entries
            .get()
            .checked_mul(limits.maximum_packet_bytes.get())
            .ok_or(Error::Capacity)?;
        if retained > limits.outbound_byte_budget.get() {
            return Err(Error::Capacity);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(limits.outbound_entries.get())
            .map_err(|_| Error::Capacity)?;
        for _ in 0..limits.outbound_entries.get() {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(limits.maximum_packet_bytes.get())
                .map_err(|_| Error::Capacity)?;
            bytes.resize(limits.maximum_packet_bytes.get(), 0);
            slots.push(Slot {
                scope: FrameScope::Hnb,
                cancelled: false,
                ppid: 0,
                length: 0,
                bytes,
            });
        }
        Ok(Self {
            association,
            slots,
            head: 0,
            length: 0,
        })
    }
    pub(crate) fn len(&self) -> usize {
        self.length
    }
    pub(crate) fn discard(&mut self) {
        self.head = 0;
        self.length = 0;
    }
    pub(crate) fn admit(&mut self, frames: &[Frame]) -> Result<(), Error> {
        if frames.is_empty() || frames.len() > self.slots.len() - self.length {
            return Err(Error::Capacity);
        }
        for frame in frames {
            if frame.association != self.association {
                return Err(Error::Association);
            }
            if frame.protocol.ppid().is_none() {
                return Err(Error::Protocol);
            }
            if frame.data.is_empty() || frame.data.len() > self.slots[0].bytes.len() {
                return Err(Error::Capacity);
            }
        }
        let count = self.slots.len();
        for frame in frames {
            let slot = &mut self.slots[(self.head + self.length) % count];
            slot.ppid = frame.protocol.ppid().expect("preflighted protocol");
            slot.scope = frame.scope;
            slot.cancelled = false;
            slot.length = frame.data.len();
            slot.bytes[..slot.length].copy_from_slice(&frame.data);
            self.length += 1;
        }
        Ok(())
    }
    pub(crate) fn cancel(&mut self, key: UeKey, domain: Option<Domain>) {
        for offset in 0..self.length {
            let index = (self.head + offset) % self.slots.len();
            let slot = &mut self.slots[index];
            if matches!(slot.scope, FrameScope::Ue { key: owner, domain: target } if owner == key && (domain.is_none() || target == domain))
            {
                slot.cancelled = true;
            }
        }
    }
    pub(crate) fn transmit_one<T: Transport>(
        &mut self,
        transport: &mut T,
    ) -> Result<Option<SendResult>, T::Error> {
        if self.length == 0 {
            return Ok(None);
        }
        let slot = &mut self.slots[self.head];
        if slot.cancelled {
            slot.bytes[..slot.length].fill(0);
            slot.length = 0;
            self.head = (self.head + 1) % self.slots.len();
            self.length -= 1;
            return Ok(None);
        }
        // Both protocols require ordered delivery. Stream zero exists in every established association.
        let result = transport.send(CONTROL_STREAM, slot.ppid, &slot.bytes[..slot.length])?;
        if result == SendResult::Sent {
            slot.bytes[..slot.length].fill(0);
            slot.length = 0;
            self.head = (self.head + 1) % self.slots.len();
            self.length -= 1;
        }
        Ok(Some(result))
    }
}
