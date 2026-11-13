// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;

const MODULUS: usize = 64;
pub const REORDER_WINDOW: usize = MODULUS / 2;
const HALF_WINDOW: usize = REORDER_WINDOW;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Sequence,
    Time,
    Capacity,
}

pub enum Admission<T> {
    Stored,
    Duplicate(T),
    Old(T),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Store,
    Duplicate,
    Old,
}

struct Entry<T> {
    arrival: u64,
    value: T,
}

// TS 25.321 11.8.3.1 leaves the reordering policy to the implementation.
// One owner is required per logical channel, with TSN initialized to zero.
pub struct Reorder<T> {
    entries: [Option<Entry<T>>; MODULUS],
    next: usize,
    count: usize,
    hold: NonZeroU64,
    now: u64,
}

impl<T> Reorder<T> {
    pub fn new(hold: NonZeroU64, now: u64) -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            next: 0,
            count: 0,
            hold,
            now,
        }
    }

    // Time and hold share the caller's monotonic unit. Rejection returns ownership unchanged.
    pub fn insert(&mut self, sequence: u8, now: u64, value: T) -> Result<Admission<T>, (Error, T)> {
        let disposition = match self.classify(sequence, now) {
            Ok(disposition) => disposition,
            Err(error) => return Err((error, value)),
        };
        self.now = now;
        match disposition {
            Disposition::Duplicate => return Ok(Admission::Duplicate(value)),
            Disposition::Old => return Ok(Admission::Old(value)),
            Disposition::Store => (),
        }
        let index = usize::from(sequence);
        self.entries[index] = Some(Entry {
            arrival: now,
            value,
        });
        self.count += 1;
        Ok(Admission::Stored)
    }

    pub fn classify(&self, sequence: u8, now: u64) -> Result<Disposition, Error> {
        let index = usize::from(sequence);
        if index >= MODULUS {
            return Err(Error::Sequence);
        }
        if now < self.now {
            return Err(Error::Time);
        }
        let distance = (index + MODULUS - self.next) % MODULUS;
        if distance >= HALF_WINDOW {
            return Ok(Disposition::Old);
        }
        if self.entries[index].is_some() {
            return Ok(Disposition::Duplicate);
        }
        if self.count >= HALF_WINDOW - 1 && distance != 0 {
            return Err(Error::Capacity);
        }
        Ok(Disposition::Store)
    }

    pub fn pop(&mut self, now: u64) -> Result<Option<T>, Error> {
        if now < self.now {
            return Err(Error::Time);
        }
        self.now = now;
        if self.count == 0 {
            return Ok(None);
        }
        if self.entries[self.next].is_none() {
            let oldest = self
                .entries
                .iter()
                .filter_map(|entry| entry.as_ref().map(|entry| entry.arrival))
                .min()
                .expect("nonempty reorder queue");
            if self.count < HALF_WINDOW - 1 && now - oldest < self.hold.get() {
                return Ok(None);
            }
            self.next = (1..HALF_WINDOW)
                .map(|distance| (self.next + distance) % MODULUS)
                .find(|index| self.entries[*index].is_some())
                .expect("entries remain inside half window");
        }
        let entry = self.entries[self.next]
            .take()
            .expect("selected reorder entry");
        self.count -= 1;
        self.next = (self.next + 1) % MODULUS;
        Ok(Some(entry.value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_parallel_process_is_not_lost_until_hold_expires() {
        let mut queue = Reorder::new(NonZeroU64::new(100).unwrap(), 0);
        assert!(matches!(queue.insert(1, 1, 11), Ok(Admission::Stored)));
        assert_eq!(queue.pop(99), Ok(None));
        assert!(matches!(queue.insert(0, 99, 10), Ok(Admission::Stored)));
        assert_eq!(queue.pop(99), Ok(Some(10)));
        assert_eq!(queue.pop(99), Ok(Some(11)));
        assert!(matches!(queue.insert(4, 100, 14), Ok(Admission::Stored)));
        assert_eq!(queue.pop(199), Ok(None));
        assert_eq!(queue.pop(200), Ok(Some(14)));
        assert!(matches!(queue.insert(3, 200, 13), Ok(Admission::Old(13))));
    }

    #[test]
    fn missing_head_can_enter_a_full_forward_window() {
        let mut queue = Reorder::new(NonZeroU64::new(100).unwrap(), 0);
        for sequence in 1..HALF_WINDOW as u8 {
            assert!(matches!(
                queue.insert(sequence, 1, sequence),
                Ok(Admission::Stored)
            ));
        }
        assert!(matches!(queue.insert(0, 2, 0), Ok(Admission::Stored)));
        for sequence in 0..HALF_WINDOW as u8 {
            assert_eq!(queue.pop(2), Ok(Some(sequence)));
        }
        assert_eq!(queue.pop(2), Ok(None));
    }

    #[test]
    fn duplicate_observation_advances_time_and_wrap_preserves_order() {
        let mut queue = Reorder::new(NonZeroU64::new(100).unwrap(), 0);
        assert!(matches!(queue.insert(0, 1, 0), Ok(Admission::Stored)));
        assert!(matches!(
            queue.insert(0, 3, 10),
            Ok(Admission::Duplicate(10))
        ));
        assert!(matches!(queue.insert(1, 2, 1), Err((Error::Time, 1))));
        assert_eq!(queue.pop(3), Ok(Some(0)));
        for absolute in 1..=128 {
            let sequence = (absolute % MODULUS) as u8;
            assert!(matches!(
                queue.insert(sequence, 3, absolute),
                Ok(Admission::Stored)
            ));
            assert_eq!(queue.pop(3), Ok(Some(absolute)));
        }
    }
}
