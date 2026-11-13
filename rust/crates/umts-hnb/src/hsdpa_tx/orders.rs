// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;
use umts_dsp::hsdpa::{ScchEncoder, SecondaryCellOrder};

const ORDER_CHIPS: usize = TTI_SLOTS * CHIPS_PER_SLOT as usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderIdentity {
    pub identity: ReceiveIdentity,
    pub generation: u64,
    pub ordinal: u8,
    pub scch_start: ChipInstant,
    pub expected_ack: ChipInstant,
}

struct OrderStorage {
    metadata: Option<OrderMetadata>,
    samples: Box<[Complex32]>,
}

#[derive(Clone, Copy)]
struct OrderMetadata {
    key: OrderIdentity,
    primary: PrimaryScramblingCode,
    pool: HsdpaCodeSet,
    control_code: u16,
    end: ChipInstant,
}

pub struct OrderBuffer {
    storage: Box<OrderStorage>,
}
impl Default for OrderBuffer {
    fn default() -> Self {
        // Allocate metadata and samples with the bounded startup pool. Preparing,
        // rejecting and recycling an order transfer this same allocation.
        Self {
            storage: Box::new(OrderStorage {
                metadata: None,
                samples: vec![Complex32::default(); ORDER_CHIPS].into_boxed_slice(),
            }),
        }
    }
}

pub struct PreparedOrder {
    storage: Box<OrderStorage>,
}

impl PreparedOrder {
    fn metadata(&self) -> &OrderMetadata {
        self.storage
            .metadata
            .as_ref()
            .expect("prepared order metadata")
    }
    pub fn identity(&self) -> OrderIdentity {
        self.metadata().key
    }
    pub(crate) fn primary_code(&self) -> PrimaryScramblingCode {
        self.metadata().primary
    }
    pub fn recycle(mut self) -> OrderBuffer {
        self.storage.metadata = None;
        OrderBuffer {
            storage: self.storage,
        }
    }
    pub(crate) fn retained_bits(&self) -> usize {
        retained_order_bits()
    }
}

pub struct RejectedOrder {
    pub error: Error,
    pub order: PreparedOrder,
}
impl std::fmt::Debug for RejectedOrder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedOrder")
            .field("error", &self.error)
            .field("identity", &self.order.identity())
            .finish_non_exhaustive()
    }
}

pub struct CompletedOrder {
    pub order: PreparedOrder,
    pub submitted: bool,
}
pub struct OrderPreparationError {
    pub error: Error,
    pub buffer: OrderBuffer,
}

pub(crate) const fn retained_order_bits() -> usize {
    (ORDER_CHIPS * size_of::<Complex32>() + size_of::<OrderStorage>() + size_of::<PreparedOrder>())
        * u8::BITS as usize
}

pub struct OrderPreparation {
    pub key: OrderIdentity,
    pub h_rnti: u16,
    pub order: SecondaryCellOrder,
    pub control_code: u16,
    pub amplitude: f32,
}

impl WaveformPreparer {
    pub fn prepare_order(
        &self,
        request: OrderPreparation,
        mut buffer: OrderBuffer,
    ) -> Result<PreparedOrder, OrderPreparationError> {
        let result = (|| {
            let key = request.key;
            if key.identity.ue.epoch != key.scch_start.epoch
                || key.expected_ack.epoch != key.scch_start.epoch
                || key.expected_ack.index <= key.scch_start.index
                || key.generation == 0
                || key.ordinal == 0
            {
                return Err(Error::Identity);
            }
            if !key.scch_start.index.is_multiple_of(ORDER_CHIPS as u64) {
                return Err(Error::Time);
            }
            let end = ChipInstant {
                index: key
                    .scch_start
                    .index
                    .checked_add(ORDER_CHIPS as u64)
                    .ok_or(Error::Time)?,
                ..key.scch_start
            };
            let control = self
                .control
                .iter()
                .find(|(code, _)| *code == request.control_code)
                .ok_or(Error::Codes)?;
            let bits = ScchEncoder::new(request.h_rnti)
                .and_then(|encoder| encoder.encode_order(request.order))
                .map_err(Error::Dsp)?;
            buffer.storage.samples.fill(Complex32::default());
            for (slot, output) in buffer
                .storage
                .samples
                .chunks_exact_mut(CHIPS_PER_SLOT as usize)
                .enumerate()
            {
                control
                    .1
                    .add_slot(&bits, key.scch_start, slot, request.amplitude, output)
                    .map_err(Error::Dsp)?;
            }
            Ok(end)
        })();
        match result {
            Ok(end) => {
                buffer.storage.metadata = Some(OrderMetadata {
                    key: request.key,
                    primary: self.primary,
                    pool: self.pool,
                    control_code: request.control_code,
                    end,
                });
                Ok(PreparedOrder {
                    storage: buffer.storage,
                })
            }
            Err(error) => Err(OrderPreparationError { error, buffer }),
        }
    }
}

pub(super) struct PendingOrder {
    order: PreparedOrder,
    submission: TransportSubmission,
    resolution: TransportResolution,
    cancelled: bool,
}

impl TransmitBank {
    pub fn enqueue_order(&mut self, order: PreparedOrder) -> Result<(), RejectedOrder> {
        let error = if self.isolated || self.retiring.contains(&Some(order.identity().identity.ue))
        {
            Some(Error::Unavailable)
        } else if order.metadata().primary != self.primary || order.metadata().pool != self.codes()
        {
            Some(Error::Codes)
        } else if !self.progress.available()
            || order.metadata().key.scch_start.epoch != self.next.epoch
            || order.metadata().key.scch_start.index < self.next.index
        {
            Some(Error::Time)
        } else if self
            .orders
            .iter()
            .flatten()
            .any(|pending| !pending.cancelled && order.conflicts_order(&pending.order))
            || self.pending.iter().flatten().any(|pending| {
                !pending.cancelled && order.conflicts_data(pending.transmission.metadata())
            })
        {
            Some(Error::Conflict)
        } else if self.orders.iter().all(Option::is_some) {
            Some(Error::Capacity)
        } else {
            None
        };
        if let Some(error) = error {
            return Err(RejectedOrder { error, order });
        }
        let submission = self.progress.interval_receipt(
            order.metadata().key.scch_start,
            order.metadata().end,
            self.rate,
            self.group_delay,
        );
        let resolution = self.progress.interval_resolution(
            order.metadata().key.scch_start,
            order.metadata().end,
            self.rate,
            self.group_delay,
        );
        let (Some(submission), Some(resolution)) = (submission, resolution) else {
            return Err(RejectedOrder {
                error: Error::Time,
                order,
            });
        };
        *self
            .orders
            .iter_mut()
            .find(|pending| pending.is_none())
            .expect("checked order capacity") = Some(PendingOrder {
            order,
            submission,
            resolution,
            cancelled: false,
        });
        Ok(())
    }

    pub fn take_completed_order(&mut self) -> Option<CompletedOrder> {
        let slot = self.orders.iter_mut().find(|slot| {
            slot.as_ref().is_some_and(|pending| {
                pending.cancelled
                    || (self.next.index >= pending.order.metadata().end.index
                        && (pending.resolution.resolved() || !pending.resolution.viable()))
            })
        })?;
        let pending = slot.take().expect("completed order");
        Some(CompletedOrder {
            submitted: !pending.cancelled && pending.submission.submitted(),
            order: pending.order,
        })
    }

    pub(super) fn order_conflicts(&self, metadata: &TransmissionMetadata) -> bool {
        self.orders
            .iter()
            .flatten()
            .any(|pending| !pending.cancelled && pending.order.conflicts_data(metadata))
    }

    pub(super) fn isolate_orders(&mut self) {
        for pending in self.orders.iter_mut().flatten() {
            pending.cancelled = true;
        }
    }

    pub(super) fn retire_orders(&mut self, ue: UeKey) {
        for pending in self.orders.iter_mut().flatten() {
            if pending.order.metadata().key.identity.ue == ue
                && pending.order.metadata().key.scch_start.index >= self.next.index
            {
                pending.cancelled = true;
            }
        }
    }

    pub(super) fn orders_pending(&self, ue: UeKey) -> bool {
        self.orders
            .iter()
            .flatten()
            .any(|pending| pending.order.metadata().key.identity.ue == ue)
    }

    pub(super) fn mix_orders(&self, start: ChipInstant, output: &mut [Complex32]) {
        for pending in self.orders.iter().flatten() {
            if pending.cancelled
                || start.index < pending.order.metadata().key.scch_start.index
                || start.index >= pending.order.metadata().end.index
            {
                continue;
            }
            let offset = (start.index - pending.order.metadata().key.scch_start.index) as usize;
            for (out, sample) in output
                .iter_mut()
                .zip(&pending.order.storage.samples[offset..offset + CHIPS_PER_SLOT as usize])
            {
                *out += *sample;
            }
        }
    }
}

impl PreparedOrder {
    fn conflicts_order(&self, other: &Self) -> bool {
        self.metadata().key == other.metadata().key
            || ((self.metadata().control_code == other.metadata().control_code
                || self.metadata().key.identity.ue == other.metadata().key.identity.ue)
                && overlaps(
                    self.metadata().key.scch_start.index,
                    self.metadata().end.index,
                    other.metadata().key.scch_start.index,
                    other.metadata().end.index,
                ))
    }
    fn conflicts_data(&self, other: &TransmissionMetadata) -> bool {
        (self.metadata().control_code == other.control_code
            || self.metadata().key.identity.ue == other.transmission.key.ue())
            && overlaps(
                self.metadata().key.scch_start.index,
                self.metadata().end.index,
                other.start.index,
                other.start.index + ORDER_CHIPS as u64,
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        radio_resources::RadioResources,
        test_support::{at, count, source},
    };
    use std::num::{NonZeroU8, NonZeroU64};

    #[test]
    fn order_prepare_reject_and_recycle_retain_storage_and_samples() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(2).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let codes = *lease.codes();
        let preparer = WaveformPreparer::new(codes, source.information().primary_code()).unwrap();
        let control_code = codes.control_codes().next().unwrap().index();
        let mut buffer = OrderBuffer::default();
        let storage_address = std::ptr::from_ref(buffer.storage.as_ref());
        let samples_address = buffer.storage.samples.as_ptr();
        assert_eq!(size_of::<PreparedOrder>(), size_of::<Box<OrderStorage>>());
        assert_eq!(
            retained_order_bits() / u8::BITS as usize,
            buffer.storage.samples.len() * size_of::<Complex32>()
                + size_of::<OrderStorage>()
                + size_of::<PreparedOrder>()
        );
        for ordinal in 1..=3 {
            let start = (u64::from(ordinal) - 1) * ORDER_CHIPS as u64;
            let identity = OrderIdentity {
                identity: ReceiveIdentity {
                    ue: UeKey {
                        epoch: at(0).epoch,
                        slot: 0,
                        generation: NonZeroU64::MIN,
                    },
                    configuration: NonZeroU64::MIN,
                },
                generation: 1,
                ordinal,
                scch_start: at(start),
                expected_ack: at(start + 30_000),
            };
            let request = |control_code| OrderPreparation {
                key: identity,
                h_rnti: 77,
                order: SecondaryCellOrder::Deactivate,
                control_code,
                amplitude: 0.1,
            };
            let failure = match preparer.prepare_order(request(u16::MAX), buffer) {
                Ok(_) => panic!("unallocated control code"),
                Err(failure) => failure,
            };
            assert_eq!(failure.error, Error::Codes);
            buffer = failure.buffer;
            assert_eq!(std::ptr::from_ref(buffer.storage.as_ref()), storage_address);
            assert_eq!(buffer.storage.samples.as_ptr(), samples_address);
            assert!(buffer.storage.metadata.is_none());
            let order = preparer
                .prepare_order(request(control_code), buffer)
                .unwrap_or_else(|failure| panic!("{:?}", failure.error));
            assert_eq!(order.identity(), identity);
            assert_eq!(std::ptr::from_ref(order.storage.as_ref()), storage_address);
            assert_eq!(order.storage.samples.as_ptr(), samples_address);
            assert!(
                order
                    .storage
                    .samples
                    .iter()
                    .any(|sample| sample.norm_sqr() > 0.0)
            );
            buffer = order.recycle();
            assert_eq!(std::ptr::from_ref(buffer.storage.as_ref()), storage_address);
            assert_eq!(buffer.storage.samples.as_ptr(), samples_address);
            assert!(buffer.storage.metadata.is_none());
        }
    }
}
