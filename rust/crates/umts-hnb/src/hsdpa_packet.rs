// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;
use umts_dsp::{
    DspError,
    bits::Bit,
    hsdpa::{CrcMethod, TransportEncoder},
};
use umts_mac::{
    MacError,
    hsdpa::{EntityError, HarqEntity, PackedPdu, PacketQueue, ProcessId, QueuePublishError},
};

pub(crate) mod delivery_trace;
mod flight;
pub use flight::{FeedbackError, Flight, FlightError, RejectedCompletion, Resolution};
mod encoders;
mod owner;
pub use encoders::EncoderBank;
mod plans;
pub use owner::{OwnerError, PacketOwner, PacketQueues, RetiredPackets};
pub use plans::{PlanBank, PlanFormat};
mod preparation;
pub use preparation::{PrepareError, RejectedPreparation, TransmitRequest};
pub(crate) mod control;
pub use control::{CommandResult, ControlPort, Operation};

pub struct BlockBuffer {
    coded: Box<[Bit]>,
}

impl BlockBuffer {
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            coded: vec![Bit::Zero; capacity.get()].into_boxed_slice(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.coded.len()
    }
}

pub struct CodedBlock {
    buffer: BlockBuffer,
    coded_bits: usize,
    transport_bits: NonZeroUsize,
    packet: PackedPdu,
    delivery_trace: Option<u16>,
}

impl CodedBlock {
    pub fn coded(&self) -> &[Bit] {
        &self.buffer.coded[..self.coded_bits]
    }
    pub fn transport_bits(&self) -> NonZeroUsize {
        self.transport_bits
    }
    pub fn packet(&self) -> PackedPdu {
        self.packet
    }
    pub(crate) fn delivery_trace(&self) -> Option<u16> {
        self.delivery_trace
    }
    pub fn recycle(self) -> BlockBuffer {
        self.buffer
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Phase,
    Dsp(DspError),
    Mac(MacError),
    Harq(EntityError),
}

pub struct RejectedBlock {
    pub error: Error,
    pub buffer: BlockBuffer,
}

#[must_use = "Retain the encoded payload or return its unused buffer"]
pub enum PayloadPreparation {
    Prepared(CodedBlock),
    Empty(BlockBuffer),
}

pub struct PacketEncoder {
    transport_bits: NonZeroUsize,
    encoder: TransportEncoder,
    packed: Box<[u8]>,
    method: CrcMethod,
}

impl PacketEncoder {
    pub fn required_storage_bytes(
        transport_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<usize, DspError> {
        TransportEncoder::required_storage_bytes(transport_bits, maximum_coded_bits)?
            .checked_add(std::mem::size_of::<Self>() - std::mem::size_of::<TransportEncoder>())
            .and_then(|bytes| bytes.checked_add(transport_bits.get().div_ceil(u8::BITS as usize)))
            .ok_or(DspError::Capacity)
    }

    pub fn new(
        transport_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
        method: CrcMethod,
    ) -> Result<Self, DspError> {
        Ok(Self {
            transport_bits,
            encoder: TransportEncoder::new(transport_bits, maximum_coded_bits)?,
            packed: vec![0; transport_bits.get().div_ceil(u8::BITS as usize)].into_boxed_slice(),
            method,
        })
    }

    #[cfg(test)]
    pub fn buffer(&self) -> BlockBuffer {
        BlockBuffer::new(
            NonZeroUsize::new(self.encoder.coded_bits()).expect("coded transport block"),
        )
    }

    pub fn transport_bits(&self) -> NonZeroUsize {
        self.transport_bits
    }

    pub fn coded_bits(&self) -> usize {
        self.encoder.coded_bits()
    }

    /// None transfers the buffer into HARQ. Some returns it because no queued prefix fits.
    pub fn admit(
        &mut self,
        queue: &mut PacketQueue,
        harq: &mut HarqEntity<CodedBlock>,
        process: ProcessId,
        buffer: BlockBuffer,
    ) -> Result<Option<BlockBuffer>, RejectedBlock> {
        if buffer.capacity() < self.encoder.coded_bits() {
            return Err(RejectedBlock {
                error: Error::Dsp(DspError::InvalidLength),
                buffer,
            });
        }
        if self.transport_bits.get() > harq.category().limits().maximum_transport_bits {
            return Err(RejectedBlock {
                error: Error::Dsp(DspError::Capacity),
                buffer,
            });
        }
        match harq.payload(process) {
            Ok(None) => (),
            Ok(Some(_)) => {
                return Err(RejectedBlock {
                    error: Error::Harq(EntityError::Harq(umts_mac::hsdpa::HarqError::State)),
                    buffer,
                });
            }
            Err(error) => {
                return Err(RejectedBlock {
                    error: Error::Harq(error),
                    buffer,
                });
            }
        }
        self.publish_encoded(queue, buffer, false, |block| {
            harq.insert(process, block)
                .map_err(|rejected| (Error::Harq(rejected.error), rejected.payload))
        })
    }

    pub fn prepare_payload(
        &mut self,
        queue: &mut PacketQueue,
        category: umts_types::hsdpa::Category,
        buffer: BlockBuffer,
    ) -> Result<PayloadPreparation, RejectedBlock> {
        if self.transport_bits.get() > category.limits().maximum_transport_bits {
            return Err(RejectedBlock {
                error: Error::Dsp(DspError::Capacity),
                buffer,
            });
        }
        let mut prepared = None;
        let unused = self.publish_encoded(queue, buffer, true, |block| {
            prepared = Some(block);
            Ok(())
        })?;
        Ok(match unused {
            Some(buffer) => PayloadPreparation::Empty(buffer),
            None => PayloadPreparation::Prepared(prepared.expect("committed encoded payload")),
        })
    }

    fn publish_encoded(
        &mut self,
        queue: &mut PacketQueue,
        buffer: BlockBuffer,
        staged: bool,
        admit: impl FnOnce(CodedBlock) -> Result<(), (Error, CodedBlock)>,
    ) -> Result<Option<BlockBuffer>, RejectedBlock> {
        if buffer.capacity() < self.encoder.coded_bits() {
            return Err(RejectedBlock {
                error: Error::Dsp(DspError::InvalidLength),
                buffer,
            });
        }
        let coded_bits = self.encoder.coded_bits();
        let mut buffer = Some(buffer);
        let encode = |packet, bytes: umts_types::bits::Bits<'_>| {
            self.encoder
                .encode_packed(
                    bytes,
                    self.method,
                    // TS 25.212 4.5.1a requires bit scrambling after CRC attachment.
                    true,
                    &mut buffer.as_mut().expect("owned buffer").coded[..coded_bits],
                )
                .map_err(Error::Dsp)?;
            let block = CodedBlock {
                buffer: buffer.take().expect("encoded buffer"),
                coded_bits,
                transport_bits: self.transport_bits,
                packet,
                delivery_trace: delivery_trace::encoded(packet, bytes),
            };
            admit(block).map_err(|(error, rejected)| {
                buffer = Some(rejected.recycle());
                error
            })
        };
        let result = if staged {
            queue.stage(self.transport_bits.get(), &mut self.packed, encode)
        } else {
            queue.publish(self.transport_bits.get(), &mut self.packed, encode)
        };
        match result {
            Ok(Some(())) => Ok(None),
            Ok(None) => Ok(buffer),
            Err(error) => Err(RejectedBlock {
                error: match error {
                    QueuePublishError::Mac(error) => Error::Mac(error),
                    QueuePublishError::Admission(error) => error,
                },
                buffer: buffer.expect("failed admission retains buffer"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{at, count};
    use std::num::{NonZeroU8, NonZeroU64};
    use umts_mac::hsdpa::{Feedback, MacdPdu, QueueFormat};
    use umts_types::{UeKey, bits::Bits, hsdpa::Category};

    #[test]
    fn staged_encoding_matches_immediate_encoding_through_sequence_wrap() {
        let mut sizes = [None; 8];
        sizes[0] = Some(count(8));
        let format = QueueFormat::new(0, sizes).unwrap();
        let mut direct =
            PacketQueue::new(QueueFormat::new(0, sizes).unwrap(), count(2), count(4096)).unwrap();
        let mut staged = PacketQueue::new(format, count(2), count(4096)).unwrap();
        let category = Category::new(10).unwrap();
        let mut encoder =
            PacketEncoder::new(count(137), count(100_000), CrcMethod::Method1).unwrap();
        for value in 0..130 {
            let bytes = [value as u8];
            for queue in [&mut direct, &mut staged] {
                queue
                    .enqueue(MacdPdu {
                        size_index: 0,
                        bits: Bits::new(&bytes, 8).unwrap(),
                    })
                    .unwrap();
            }
            let buffer = encoder.buffer();
            let prepared = encoder
                .prepare_payload(&mut staged, category, buffer)
                .unwrap_or_else(|_| panic!("stage"));
            let PayloadPreparation::Prepared(block) = prepared else {
                panic!("payload")
            };
            let mut expected = None;
            let buffer = encoder.buffer();
            encoder
                .publish_encoded(&mut direct, buffer, false, |block| {
                    expected = Some(block);
                    Ok(())
                })
                .unwrap_or_else(|_| panic!("immediate encode"));
            let expected = expected.unwrap();
            assert_eq!(block.packet(), expected.packet());
            assert_eq!(block.coded(), expected.coded());
            staged
                .commit_staged(block.packet(), || Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(staged.len(), direct.len());
        }
    }

    #[test]
    fn queued_bytes_commit_once_and_coded_block_survives_nack_until_ack() {
        let mut sizes = [None; 8];
        sizes[0] = Some(count(8));
        let mut queue =
            PacketQueue::new(QueueFormat::new(0, sizes).unwrap(), count(2), count(4096)).unwrap();
        let ue = UeKey {
            epoch: at(0).epoch,
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let mut harq = HarqEntity::new(
            ue,
            NonZeroU64::MIN,
            Category::new(10).unwrap(),
            at(0),
            &[count(28_800); 6],
            NonZeroU8::MIN,
        )
        .unwrap();
        let id = ProcessId::new(0).unwrap();
        let mut encoder =
            PacketEncoder::new(count(137), count(100_000), CrcMethod::Method1).unwrap();
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x5a], 8).unwrap(),
            })
            .unwrap();
        let buffer = BlockBuffer::new(count(100_000));
        assert!(
            encoder
                .admit(&mut queue, &mut harq, id, buffer)
                .unwrap_or_else(|_| panic!("admission"))
                .is_none()
        );
        assert!(queue.is_empty());
        let coded = harq.payload(id).unwrap().unwrap().coded().to_vec();
        assert_eq!(coded.len(), encoder.coded_bits());
        let mut reference = TransportEncoder::new(count(137), count(100_000)).unwrap();
        let mut expected = vec![Bit::Zero; reference.coded_bits()];
        let payload = Bits::new(&encoder.packed, encoder.transport_bits.get()).unwrap();
        let payload: Vec<_> = (0..payload.len())
            .map(|index| Bit::from(payload.bit(index).unwrap()))
            .collect();
        reference
            .encode(&payload, CrcMethod::Method1, true, &mut expected)
            .unwrap();
        assert_eq!(coded, expected);
        reference
            .encode(&payload, CrcMethod::Method1, false, &mut expected)
            .unwrap();
        assert_ne!(coded, expected);
        assert_eq!(harq.payload(id).unwrap().unwrap().packet().pdus, 1);
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0xa5], 8).unwrap(),
            })
            .unwrap();
        let buffer = encoder.buffer();
        let rejected = match encoder.admit(&mut queue, &mut harq, id, buffer) {
            Err(rejected) => rejected,
            _ => panic!("occupied process must retain queued packet"),
        };
        assert_eq!(queue.len(), 1);
        let first = harq.schedule(id, at(0), at(30_000)).unwrap();
        harq.transmitted(first.key).unwrap();
        assert!(
            harq.feedback(first.key, first.expected_ack, Feedback::Nack)
                .unwrap()
                .is_none()
        );
        assert_eq!(harq.payload(id).unwrap().unwrap().coded(), coded);
        let retry = harq.schedule(id, at(38_400), at(68_400)).unwrap();
        assert_eq!(first.new_data, retry.new_data);
        harq.transmitted(retry.key).unwrap();
        let acknowledged = harq
            .feedback(retry.key, retry.expected_ack, Feedback::Ack)
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.coded(), coded);
        assert!(
            encoder
                .admit(&mut queue, &mut harq, id, rejected.buffer)
                .unwrap_or_else(|_| panic!("freed process"))
                .is_none()
        );
        assert!(queue.is_empty());
        let buffer = acknowledged.recycle();
        let next_id = ProcessId::new(1).unwrap();
        let buffer = encoder
            .admit(&mut queue, &mut harq, next_id, buffer)
            .unwrap_or_else(|_| panic!("empty queue"))
            .expect("empty queue returns original storage");
        assert_eq!(buffer.capacity(), 100_000);
        let storage = buffer.coded.as_ptr();
        let mut larger =
            PacketEncoder::new(count(461), count(100_000), CrcMethod::Method1).unwrap();
        queue
            .enqueue(MacdPdu {
                size_index: 0,
                bits: Bits::new(&[0x39], 8).unwrap(),
            })
            .unwrap();
        assert!(
            larger
                .admit(&mut queue, &mut harq, next_id, buffer)
                .unwrap_or_else(|_| panic!("larger transport reuses storage"))
                .is_none()
        );
        let block = harq.payload(next_id).unwrap().unwrap();
        assert_eq!(block.buffer.coded.as_ptr(), storage);
        assert_eq!(block.coded().len(), larger.coded_bits());
        assert_eq!(block.transport_bits().get(), 461);
    }
}
