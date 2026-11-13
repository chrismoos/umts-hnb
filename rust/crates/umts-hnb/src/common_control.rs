// SPDX-License-Identifier: AGPL-3.0-or-later

#[cfg(test)]
use crate::access_message::MessageDelivery;
use std::num::NonZeroUsize;
#[cfg(test)]
use umts_dsp::bits::pack_msb;
use umts_dsp::prach_message::DecodedPrachMessage;
use umts_mac::{CommonHeader, CommonPdu, MacError};
#[cfg(test)]
use umts_mac::{FddCommonChannel, decode_common};
use umts_rlc::transparent::{ErrorDelivery, Segmentation, TmError, TmLimits, TmReceiver, TmdPdu};
use umts_rrc_codec::{
    CodecError, DecodedUplinkCommon, MAX_UPLINK_COMMON_BITS, decode_uplink_common,
};
use umts_types::{
    RadioEpoch,
    bits::{BITS_PER_BYTE, Bits},
};

const MAX_CONTROL_PDUS: usize = 64;
const MAX_QUEUED_SDUS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlError {
    Capacity,
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Transport,
    Mac(MacError),
    Mapping,
    Rlc(TmError),
    Epoch,
}

#[cfg(test)]
pub struct RachMacBatch<'a> {
    transport: DecodedPrachMessage,
    block_bits: usize,
    blocks: usize,
    data: Bits<'a>,
}

#[cfg(test)]
impl<'a> RachMacBatch<'a> {
    #[cfg(test)]
    pub fn from_transport(
        transport: DecodedPrachMessage,
        block_bits: usize,
        blocks: usize,
        data: Bits<'a>,
    ) -> Result<Self, ControlError> {
        let length = block_bits
            .checked_mul(blocks)
            .ok_or(ControlError::Transport)?;
        if blocks == 0
            || block_bits == 0
            || length != data.len()
            || transport.payload_bits != length
        {
            return Err(ControlError::Transport);
        }
        Ok(Self {
            transport,
            block_bits,
            blocks,
            data,
        })
    }

    #[cfg(test)]
    pub fn transport(&self) -> DecodedPrachMessage {
        self.transport
    }

    #[cfg(test)]
    pub fn from_message(
        message: &MessageDelivery<'_>,
        output: &'a mut [u8],
    ) -> Result<Self, ControlError> {
        let length = message
            .block_bits
            .checked_mul(message.blocks)
            .ok_or(ControlError::Transport)?;
        if message.blocks == 0
            || message.block_bits == 0
            || message.payload.len() != length
            || message.decoded.payload_bits != length
        {
            return Err(ControlError::Transport);
        }
        let output = output
            .get_mut(..length.div_ceil(BITS_PER_BYTE))
            .ok_or(ControlError::Capacity)?;
        pack_msb(message.payload, output).map_err(|_| ControlError::Transport)?;
        Self::from_transport(
            message.decoded,
            message.block_bits,
            message.blocks,
            Bits::new(output, length).map_err(|_| ControlError::Transport)?,
        )
    }

    #[cfg(test)]
    pub fn pdus(&self) -> impl ExactSizeIterator<Item = Result<CommonPdu<'_>, MacError>> {
        (0..self.blocks).map(|index| {
            decode_common(
                FddCommonChannel::Rach,
                self.data
                    .subspan(index * self.block_bits..(index + 1) * self.block_bits)
                    .expect("validated transport blocks"),
            )
        })
    }
}

struct QueuedSdu {
    data: Vec<u8>,
    length: usize,
    pending: Option<PendingControl>,
}

struct PendingControl {
    sequence: u64,
    transport: DecodedPrachMessage,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ControlCounters {
    pub queued: u64,
    pub decoded: u64,
    pub malformed: u64,
    pub rejected_ttis: u64,
}

#[cfg(test)]
pub struct ControlEvent {
    pub transport: DecodedPrachMessage,
    pub message: Result<DecodedUplinkCommon, CodecError>,
}

pub struct ControlSdu<'a> {
    transport: DecodedPrachMessage,
    bytes: &'a [u8],
    bit_length: usize,
    message: &'a Result<DecodedUplinkCommon, CodecError>,
}

impl<'a> ControlSdu<'a> {
    pub fn transport(&self) -> DecodedPrachMessage {
        self.transport
    }
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
    pub fn bit_length(&self) -> usize {
        self.bit_length
    }
    pub fn message(&self) -> &'a Result<DecodedUplinkCommon, CodecError> {
        self.message
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ControlDecision<T> {
    Consume(T),
    Retry(T),
}

pub struct ControlIngress {
    epoch: RadioEpoch,
    maximum_pdus: usize,
    receiver: TmReceiver,
    queue: Vec<QueuedSdu>,
    next_sequence: u64,
    decoded: Option<(u64, Result<DecodedUplinkCommon, CodecError>)>,
    counters: ControlCounters,
}

impl ControlIngress {
    pub fn new(
        epoch: RadioEpoch,
        limits: TmLimits,
        queue_capacity: NonZeroUsize,
    ) -> Result<Self, ControlError> {
        if limits.maximum_pdus.get() > MAX_CONTROL_PDUS
            || queue_capacity.get() > MAX_QUEUED_SDUS
            || limits.maximum_sdu_bits.get() > MAX_UPLINK_COMMON_BITS
            || limits.maximum_pdu_bits.get() > MAX_UPLINK_COMMON_BITS
        {
            return Err(ControlError::Capacity);
        }
        Ok(Self {
            epoch,
            maximum_pdus: limits.maximum_pdus.get(),
            // TS 25.331 13.6/13.6e fix RB0 uplink TM segmentation to FALSE.
            receiver: TmReceiver::new(Segmentation::Disabled, ErrorDelivery::Discard, limits),
            queue: (0..queue_capacity.get())
                .map(|_| QueuedSdu {
                    data: vec![0; limits.maximum_sdu_bits.get().div_ceil(BITS_PER_BYTE)],
                    length: 0,
                    pending: None,
                })
                .collect(),
            next_sequence: 0,
            decoded: None,
            counters: ControlCounters::default(),
        })
    }

    #[cfg(test)]
    pub fn counters(&self) -> ControlCounters {
        self.counters
    }
    pub fn pending(&self) -> usize {
        self.queue
            .iter()
            .filter(|entry| entry.pending.is_some())
            .count()
    }

    #[cfg(test)]
    pub fn enqueue(&mut self, batch: &RachMacBatch<'_>) -> Result<(), ControlError> {
        let result = self.enqueue_pdus(batch.transport, batch.pdus());
        if result.is_err() {
            self.counters.rejected_ttis = self.counters.rejected_ttis.saturating_add(1);
        }
        result
    }

    pub fn enqueue_pdu(
        &mut self,
        transport: DecodedPrachMessage,
        pdu: CommonPdu<'_>,
    ) -> Result<(), ControlError> {
        let result = self.enqueue_pdus(transport, std::iter::once(Ok(pdu)));
        if result.is_err() {
            self.counters.rejected_ttis = self.counters.rejected_ttis.saturating_add(1);
        }
        result
    }

    fn enqueue_pdus<'a>(
        &mut self,
        transport: DecodedPrachMessage,
        input: impl ExactSizeIterator<Item = Result<CommonPdu<'a>, MacError>>,
    ) -> Result<(), ControlError> {
        if transport.start.epoch != self.epoch {
            return Err(ControlError::Epoch);
        }
        let count = input.len();
        if count == 0 || count > self.maximum_pdus {
            return Err(ControlError::Capacity);
        }
        if self.queue.len() - self.pending() < count {
            return Err(ControlError::Capacity);
        }
        let next_sequence = self
            .next_sequence
            .checked_add(count as u64)
            .ok_or(ControlError::Capacity)?;
        let mut pdus = [TmdPdu {
            data: Bits::empty(),
            erroneous: false,
        }; MAX_CONTROL_PDUS];
        for (entry, pdu) in pdus.iter_mut().zip(input) {
            let pdu = pdu.map_err(ControlError::Mac)?;
            if pdu.header != CommonHeader::CommonControl {
                return Err(ControlError::Mapping);
            }
            *entry = TmdPdu {
                data: pdu.payload,
                erroneous: false,
            };
        }
        let sdus = self
            .receiver
            .receive(&pdus[..count])
            .map_err(ControlError::Rlc)?;
        // The whole TTI and every output buffer are admitted before any SDU becomes visible.
        let mut slots = self
            .queue
            .iter_mut()
            .filter(|entry| entry.pending.is_none());
        for (index, sdu) in sdus.enumerate() {
            let slot = slots.next().expect("admitted SDU capacity");
            slot.length = sdu.data.len();
            sdu.data
                .copy_to(&mut slot.data[..slot.length.div_ceil(BITS_PER_BYTE)])
                .expect("admitted SDU buffer");
            slot.pending = Some(PendingControl {
                sequence: self.next_sequence + index as u64,
                transport,
            });
            self.counters.queued = self.counters.queued.saturating_add(1);
        }
        self.next_sequence = next_sequence;
        Ok(())
    }

    fn prepare_one(&mut self) -> Option<usize> {
        let index = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.pending.is_some())
            .min_by_key(|(_, entry)| entry.pending.as_ref().map(|pending| pending.sequence))?
            .0;
        let slot = &self.queue[index];
        let sequence = slot
            .pending
            .as_ref()
            .expect("occupied control SDU")
            .sequence;
        if let Some((cached, _)) = self.decoded.as_ref() {
            assert_eq!(*cached, sequence, "cached decode belongs to FIFO head");
        } else {
            let message = decode_uplink_common(
                &slot.data[..slot.length.div_ceil(BITS_PER_BYTE)],
                slot.length,
            );
            if message.is_ok() {
                self.counters.decoded = self.counters.decoded.saturating_add(1);
            } else {
                self.counters.malformed = self.counters.malformed.saturating_add(1);
            }
            self.decoded = Some((sequence, message));
        }
        Some(index)
    }

    pub fn epoch(&self) -> RadioEpoch {
        self.epoch
    }

    pub fn expire_one_before(
        &mut self,
        first: umts_types::time::ChipInstant,
    ) -> Result<bool, ControlError> {
        if first.epoch != self.epoch {
            return Err(ControlError::Epoch);
        }
        let index = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.pending.is_some())
            .min_by_key(|(_, entry)| entry.pending.as_ref().map(|pending| pending.sequence))
            .map(|(index, _)| index);
        let Some(index) = index else {
            return Ok(false);
        };
        if self.queue[index]
            .pending
            .as_ref()
            .expect("pending SDU")
            .transport
            .start
            .index
            >= first.index
        {
            return Ok(false);
        }
        self.queue[index].pending = None;
        self.decoded = None;
        Ok(true)
    }

    /// Retry retains the original SDU, receive metadata and one cached decode. The callback runs once per call.
    pub fn process_one_with<T>(
        &mut self,
        process: impl FnOnce(ControlSdu<'_>) -> ControlDecision<T>,
    ) -> Option<ControlDecision<T>> {
        let index = self.prepare_one()?;
        let slot = &self.queue[index];
        let outcome = process(ControlSdu {
            transport: slot
                .pending
                .as_ref()
                .expect("occupied control SDU")
                .transport,
            bytes: &slot.data[..slot.length.div_ceil(BITS_PER_BYTE)],
            bit_length: slot.length,
            message: &self.decoded.as_ref().expect("prepared control SDU").1,
        });
        if matches!(outcome, ControlDecision::Consume(_)) {
            self.queue[index].pending = None;
            self.decoded = None;
        }
        Some(outcome)
    }

    // Call on the control owner. This consumes the original bytes and is only suitable when authentication/retry is unnecessary.
    #[cfg(test)]
    pub fn decode_one(&mut self) -> Option<ControlEvent> {
        let index = self.prepare_one()?;
        let pending = self.queue[index]
            .pending
            .take()
            .expect("occupied control SDU");
        let (_, message) = self.decoded.take().expect("prepared control SDU");
        Some(ControlEvent {
            transport: pending.transport,
            message,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests;
