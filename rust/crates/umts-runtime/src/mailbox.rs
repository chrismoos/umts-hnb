// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;
use std::sync::Arc;

mod ring;

use ring::Ring;

#[derive(Debug)]
pub struct Sender<T>(Arc<Ring<T>>);

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

pub struct Receiver<T> {
    ring: Arc<Ring<T>>,
    head: usize,
}

pub fn retained_storage_bound<T>(capacity: NonZeroUsize) -> Option<usize> {
    use std::{alloc::Layout, sync::atomic::AtomicUsize};
    ring::lap(capacity)?;
    let slots = Layout::array::<ring::Slot<T>>(capacity.get()).ok()?;
    let header = Layout::new::<[AtomicUsize; 2]>()
        .extend(Layout::new::<Ring<T>>())
        .ok()?
        .0
        .pad_to_align();
    slots.size().checked_add(header.size())
}

pub fn bounded<T>(capacity: NonZeroUsize) -> (Sender<T>, Receiver<T>) {
    let ring = Arc::new(Ring::new(capacity));
    (Sender(Arc::clone(&ring)), Receiver { ring, head: 0 })
}

impl<T> Sender<T> {
    pub fn try_send(&self, value: T) -> Result<(), T> {
        self.0.push(value)
    }
}

impl<T> Receiver<T> {
    /// Returns `None` if the FIFO head is not published yet, even when later
    /// messages are ready. Never waits for a sender to finish publication.
    pub fn try_recv(&mut self) -> Option<T> {
        self.ring.pop(&mut self.head)
    }

    pub fn service(&mut self, budget: NonZeroUsize, mut consume: impl FnMut(T)) -> usize {
        let mut count = 0;
        for _ in 0..budget.get() {
            let Some(value) = self.try_recv() else { break };
            consume(value);
            count += 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_storage_accounts_for_alignment_and_rejects_overflow() {
        #[repr(align(128))]
        struct Aligned([u8; 128]);
        let value = Aligned([0; 128]);
        assert_eq!(value.0.len(), 128);
        let one = retained_storage_bound::<Aligned>(NonZeroUsize::MIN).unwrap();
        let two = retained_storage_bound::<Aligned>(NonZeroUsize::new(2).unwrap()).unwrap();
        assert_eq!(two - one, 256);
        assert!(retained_storage_bound::<Aligned>(NonZeroUsize::MAX).is_none());
    }

    #[test]
    fn full_ring_returns_ownership_and_preserves_order() {
        let (tx, mut rx) = bounded(NonZeroUsize::new(2).unwrap());
        tx.try_send(11).unwrap();
        tx.try_send(22).unwrap();
        assert_eq!(tx.try_send(33), Err(33));
        assert_eq!(rx.try_recv(), Some(11));
        assert_eq!(rx.try_recv(), Some(22));
        assert_eq!(rx.try_recv(), None);
    }

    #[test]
    fn continuous_production_does_not_make_one_cycle_unbounded() {
        let (tx, mut rx) = bounded(NonZeroUsize::new(2).unwrap());
        tx.try_send(0).unwrap();
        let count = rx.service(NonZeroUsize::new(7).unwrap(), |n| {
            tx.try_send(n + 1).unwrap()
        });
        assert_eq!(count, 7);
        assert_eq!(rx.try_recv(), Some(7));
    }

    #[test]
    fn concurrent_senders_preserve_each_stream_without_loss_or_duplication() {
        use std::{
            thread,
            time::{Duration, Instant},
        };
        for capacity in [1, 3, 64] {
            let (tx, mut rx) = bounded(NonZeroUsize::new(capacity).unwrap());
            let deadline = Instant::now() + Duration::from_secs(10);
            let writers: Vec<_> = (0..4)
                .map(|source| {
                    let tx = tx.clone();
                    thread::spawn(move || {
                        for sequence in 0..10_000 {
                            let mut value = (source, sequence);
                            while let Err(returned) = tx.try_send(value) {
                                value = returned;
                                assert!(Instant::now() < deadline, "sender stalled");
                                thread::yield_now();
                            }
                        }
                    })
                })
                .collect();
            let mut next = [0; 4];
            while next.iter().sum::<usize>() < 40_000 {
                if let Some((source, sequence)) = rx.try_recv() {
                    assert_eq!(sequence, next[source]);
                    next[source] += 1;
                } else {
                    assert!(Instant::now() < deadline, "receiver stalled");
                    thread::yield_now();
                }
            }
            for writer in writers {
                writer.join().unwrap();
            }
            assert_eq!(rx.try_recv(), None);
        }
    }

    #[test]
    fn owned_values_are_dropped_once_after_either_endpoint_closes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Value(usize, Arc<[AtomicUsize; 4]>);
        impl Drop for Value {
            fn drop(&mut self) {
                self.1[self.0].fetch_add(1, Ordering::Relaxed);
            }
        }
        for receiver_first in [false, true] {
            let drops = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
            let (tx, mut rx) = bounded(NonZeroUsize::new(2).unwrap());
            assert!(tx.try_send(Value(0, drops.clone())).is_ok());
            assert!(tx.try_send(Value(1, drops.clone())).is_ok());
            let returned = tx.try_send(Value(2, drops.clone())).err().unwrap();
            assert_eq!(drops[2].load(Ordering::Relaxed), 0);
            drop(returned);
            drop(rx.try_recv().unwrap());
            assert!(tx.try_send(Value(3, drops.clone())).is_ok());
            if receiver_first {
                drop(rx);
                drop(tx);
            } else {
                drop(tx);
                drop(rx);
            }
            assert!(drops.iter().all(|count| count.load(Ordering::Relaxed) == 1));
        }
    }
}
