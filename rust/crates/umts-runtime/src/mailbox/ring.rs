// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(unsafe_code)]

use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
pub(super) struct Slot<T> {
    stamp: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

#[derive(Debug)]
pub(super) struct Ring<T> {
    slots: Box<[Slot<T>]>,
    tail: AtomicUsize,
    one_lap: usize,
}

// SAFETY: tail reservations give each producer exclusive write access to one slot.
// Only Receiver owns the read cursor. Slot release/acquire pairs transfer T between
// these owners, including reuse. Shared access never exposes a reference to T.
unsafe impl<T: Send> Sync for Ring<T> {}

pub(super) fn lap(capacity: NonZeroUsize) -> Option<usize> {
    capacity.get().checked_add(1)?.checked_next_power_of_two()
}

impl<T> Ring<T> {
    pub(super) fn new(capacity: NonZeroUsize) -> Self {
        let one_lap = lap(capacity).expect("mailbox capacity fits a lap counter");
        Self {
            slots: (0..capacity.get())
                .map(|index| Slot {
                    stamp: AtomicUsize::new(index),
                    value: UnsafeCell::new(MaybeUninit::uninit()),
                })
                .collect(),
            tail: AtomicUsize::new(0),
            one_lap,
        }
    }

    fn next(&self, cursor: usize, index: usize) -> usize {
        if index + 1 == self.slots.len() {
            (cursor & !(self.one_lap - 1)).wrapping_add(self.one_lap)
        } else {
            cursor.wrapping_add(1)
        }
    }

    pub(super) fn push(&self, value: T) -> Result<(), T> {
        self.push_with(value, || {})
    }

    fn push_with(&self, value: T, reserved: impl FnOnce()) -> Result<(), T> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let index = tail & (self.one_lap - 1);
            let slot = &self.slots[index];
            if slot.stamp.load(Ordering::Acquire) == tail {
                match self.tail.compare_exchange(
                    tail,
                    self.next(tail, index),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        reserved();
                        // SAFETY: this producer reserved the free slot. Acquire above
                        // observes the consumer's completed read before reuse.
                        unsafe { (*slot.value.get()).write(value) };
                        slot.stamp.store(tail.wrapping_add(1), Ordering::Release);
                        return Ok(());
                    }
                    Err(current) => tail = current,
                }
            } else {
                let current = self.tail.load(Ordering::Relaxed);
                if current == tail {
                    // The next slot remains occupied or reserved. Never wait for a
                    // preempted consumer to release it or a producer to publish it.
                    return Err(value);
                }
                tail = current;
            }
        }
    }

    pub(super) fn pop(&self, head: &mut usize) -> Option<T> {
        self.pop_with(head, || {})
    }

    fn pop_with(&self, head: &mut usize, read: impl FnOnce()) -> Option<T> {
        let index = *head & (self.one_lap - 1);
        let slot = &self.slots[index];
        if slot.stamp.load(Ordering::Acquire) != head.wrapping_add(1) {
            // A later producer may have finished first. Preserve FIFO without
            // waiting for the producer that owns this slot.
            return None;
        }
        read();
        // SAFETY: the single consumer owns head and Acquire observes a published T.
        // No producer can reuse the slot until the Release below.
        let value = unsafe { (*slot.value.get()).assume_init_read() };
        slot.stamp
            .store(head.wrapping_add(self.one_lap), Ordering::Release);
        *head = self.next(*head, index);
        Some(value)
    }
}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if *slot.stamp.get_mut() & (self.one_lap - 1) == index + 1 {
                // SAFETY: the final Arc owns all slots exclusively. Published stamps
                // identify initialized values not yet taken by the consumer.
                unsafe { slot.value.get_mut().assume_init_drop() };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn paused_publication_returns_empty_without_skipping_later_messages() {
        let ring = Ring::new(NonZeroUsize::new(2).unwrap());
        let (reserved, reservation) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        thread::scope(|scope| {
            let shared = &ring;
            let writer = scope.spawn(move || {
                shared.push_with(11, || {
                    reserved.send(()).unwrap();
                    resumed.recv_timeout(Duration::from_secs(5)).unwrap();
                })
            });
            reservation.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(ring.push(22), Ok(()));
            assert_eq!(ring.push(33), Err(33));
            let mut head = 0;
            assert_eq!(ring.pop(&mut head), None);
            resume.send(()).unwrap();
            assert_eq!(writer.join().unwrap(), Ok(()));
            assert_eq!(ring.pop(&mut head), Some(11));
            assert_eq!(ring.pop(&mut head), Some(22));
            assert_eq!(ring.pop(&mut head), None);
        });
    }

    #[test]
    fn paused_consumer_returns_full_until_the_slot_is_released() {
        let ring = Ring::new(NonZeroUsize::MIN);
        ring.push(11).unwrap();
        let (read, reading) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        thread::scope(|scope| {
            let shared = &ring;
            let reader = scope.spawn(move || {
                let mut head = 0;
                let value = shared.pop_with(&mut head, || {
                    read.send(()).unwrap();
                    resumed.recv_timeout(Duration::from_secs(5)).unwrap();
                });
                (head, value)
            });
            reading.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(ring.push(22), Err(22));
            resume.send(()).unwrap();
            let (mut head, value) = reader.join().unwrap();
            assert_eq!(value, Some(11));
            ring.push(22).unwrap();
            assert_eq!(ring.pop(&mut head), Some(22));
        });
    }

    #[test]
    fn exact_capacity_and_counter_wrap_preserve_order() {
        for capacity in [1, 2, 3, 7, 8, 17] {
            let mut ring = Ring::new(NonZeroUsize::new(capacity).unwrap());
            let start = usize::MAX & !(ring.one_lap - 1);
            *ring.tail.get_mut() = start;
            for (index, slot) in ring.slots.iter_mut().enumerate() {
                *slot.stamp.get_mut() = start + index;
            }
            let mut head = start;
            for round in 0..10 {
                for value in 0..capacity {
                    ring.push((round, value)).unwrap();
                }
                assert_eq!(ring.push((round, capacity)), Err((round, capacity)));
                for value in 0..capacity {
                    assert_eq!(ring.pop(&mut head), Some((round, value)));
                }
                assert_eq!(ring.pop(&mut head), None);
            }
        }
    }
}
