// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{DedicatedReceiveSession, ReceiveError, ReceiveIdentity, TransportDelivery};
use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use umts_dsp::bits::Bit;
use umts_runtime::mailbox::{self, Receiver, Sender};

const RESERVATION_ATTEMPTS: usize = 8;

pub(super) struct OutputBudget {
    maximum: usize,
    used: AtomicUsize,
}
impl OutputBudget {
    pub fn new(maximum: usize) -> Arc<Self> {
        Arc::new(Self {
            maximum,
            used: AtomicUsize::new(0),
        })
    }
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation, ReceiveError> {
        let mut used = self.used();
        for _ in 0..RESERVATION_ATTEMPTS {
            let next = used
                .checked_add(bytes)
                .filter(|next| *next <= self.maximum)
                .ok_or(ReceiveError::Capacity)?;
            match self
                .used
                .compare_exchange(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Ok(Reservation {
                        budget: self.clone(),
                        bytes,
                    });
                }
                Err(current) => used = current,
            }
        }
        Err(ReceiveError::Capacity)
    }
}
struct Reservation {
    budget: Arc<OutputBudget>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct LaneState {
    identity: ReceiveIdentity,
    previous: Option<ReceiveIdentity>,
    producer: AtomicBool,
    consumer: AtomicBool,
    reservation: Reservation,
}
struct Packet {
    decoded: Option<super::DecodedTransport>,
    payload: Vec<Bit>,
}
pub(super) struct LanePublisher {
    free: Receiver<Packet>,
    recycle: Sender<Packet>,
    ready: Sender<Packet>,
    state: Arc<LaneState>,
}
pub(super) struct LaneConsumer {
    ready: Receiver<Packet>,
    free: Sender<Packet>,
    pending: Option<Packet>,
    capacity: usize,
    state: Arc<LaneState>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Publication {
    Idle,
    Published,
    Backpressure,
    Closed,
}

pub(super) fn channel(
    identity: ReceiveIdentity,
    previous: Option<ReceiveIdentity>,
    capacity: NonZeroUsize,
    bits: NonZeroUsize,
    budget: &Arc<OutputBudget>,
) -> Result<(LanePublisher, LaneConsumer), ReceiveError> {
    let bytes = capacity
        .get()
        .checked_mul(bits.get())
        .ok_or(ReceiveError::Capacity)?;
    let reservation = budget.reserve(bytes)?;
    let (ready, receive) = mailbox::bounded(capacity);
    let (recycle, free) = mailbox::bounded(capacity);
    for _ in 0..capacity.get() {
        assert!(
            recycle
                .try_send(Packet {
                    decoded: None,
                    payload: vec![Bit::Zero; bits.get()]
                })
                .is_ok(),
            "new UE output pool"
        );
    }
    let state = Arc::new(LaneState {
        identity,
        previous,
        producer: AtomicBool::new(true),
        consumer: AtomicBool::new(true),
        reservation,
    });
    Ok((
        LanePublisher {
            free,
            recycle: recycle.clone(),
            ready,
            state: state.clone(),
        },
        LaneConsumer {
            ready: receive,
            free: recycle,
            pending: None,
            capacity: capacity.get(),
            state,
        },
    ))
}
impl LanePublisher {
    pub fn identity(&self) -> ReceiveIdentity {
        self.state.identity
    }
    pub fn bytes(&self) -> usize {
        self.state.reservation.bytes
    }
    pub fn publish_one(&mut self, session: &mut DedicatedReceiveSession) -> Publication {
        if !self.state.consumer.load(Ordering::Acquire) {
            return Publication::Closed;
        }
        let Some(mut packet) = self.free.try_recv() else {
            return Publication::Backpressure;
        };
        let published = session.process_one(|delivery| {
            assert_eq!(
                delivery.decoded.identity, self.state.identity,
                "UE output identity"
            );
            packet.decoded = Some(delivery.decoded);
            packet.payload[..delivery.payload.len()].copy_from_slice(delivery.payload);
            true
        });
        if published {
            assert!(
                self.ready.try_send(packet).is_ok(),
                "reserved UE output slot"
            );
            Publication::Published
        } else {
            assert!(
                self.recycle.try_send(packet).is_ok(),
                "unused UE output slot"
            );
            Publication::Idle
        }
    }
}
impl Drop for LanePublisher {
    fn drop(&mut self) {
        self.state.producer.store(false, Ordering::Release);
    }
}
impl LaneConsumer {
    pub fn identity(&self) -> ReceiveIdentity {
        self.state.identity
    }
    pub fn previous(&self) -> Option<ReceiveIdentity> {
        self.state.previous
    }
    pub fn closed(&self) -> bool {
        !self.state.producer.load(Ordering::Acquire)
    }
    fn prepare(&mut self) -> bool {
        if self.pending.is_none() {
            self.pending = self.ready.try_recv();
        }
        self.pending.is_some()
    }
    pub fn closed_and_empty(&mut self) -> bool {
        self.closed() && !self.prepare()
    }
    pub fn process_one(
        &mut self,
        accept: impl FnOnce(TransportDelivery<'_>) -> bool,
    ) -> Option<bool> {
        if !self.prepare() {
            return None;
        }
        let packet = self.pending.as_ref().expect("pending UE transport");
        let decoded = packet.decoded.expect("published UE transport");
        if !accept(TransportDelivery {
            decoded,
            payload: &packet.payload[..decoded.block_bits * decoded.blocks],
        }) {
            return Some(false);
        }
        let mut packet = self.pending.take().expect("accepted UE transport");
        packet.decoded = None;
        assert!(
            self.free.try_send(packet).is_ok(),
            "UE output pool invariant"
        );
        Some(true)
    }
    pub fn discard(mut self) -> usize {
        let mut count = usize::from(self.pending.take().is_some());
        for _ in 0..self.capacity {
            if self.ready.try_recv().is_some() {
                count += 1;
            } else {
                break;
            }
        }
        count
    }
}
impl Drop for LaneConsumer {
    fn drop(&mut self) {
        self.state.consumer.store(false, Ordering::Release);
    }
}
