// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::atomic::{AtomicU64, Ordering};

const CAPACITY: usize = 256;

#[derive(Debug, Default)]
struct Interval {
    first: AtomicU64,
    next: AtomicU64,
}

#[derive(Debug)]
pub(super) struct History {
    version: AtomicU64,
    count: AtomicU64,
    retired_before: AtomicU64,
    intervals: [Interval; CAPACITY],
}

impl Default for History {
    fn default() -> Self {
        Self {
            version: AtomicU64::new(0),
            count: AtomicU64::new(0),
            retired_before: AtomicU64::new(0),
            intervals: std::array::from_fn(|_| Interval::default()),
        }
    }
}

impl History {
    pub(super) fn record(&self, first: u64, next: u64, publish: impl FnOnce()) -> bool {
        let version = self.version.load(Ordering::SeqCst);
        let Some(finished) = version.checked_add(2) else {
            return false;
        };
        let count = self.count.load(Ordering::SeqCst);
        let Some(incremented) = count.checked_add(1) else {
            return false;
        };
        self.version.store(version + 1, Ordering::SeqCst);
        let previous = &self.intervals[count.saturating_sub(1) as usize % CAPACITY];
        if count != 0 && previous.next.load(Ordering::SeqCst) == first {
            previous.next.store(next, Ordering::SeqCst);
        } else {
            let entry = &self.intervals[count as usize % CAPACITY];
            if count >= CAPACITY as u64 {
                self.retired_before
                    .store(entry.next.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            entry.first.store(first, Ordering::SeqCst);
            entry.next.store(next, Ordering::SeqCst);
            self.count.store(incremented, Ordering::SeqCst);
        }
        publish();
        self.version.store(finished, Ordering::SeqCst);
        true
    }

    // Retired history cannot establish acceptance. Concurrent publication yields pending, never success.
    pub(super) fn intersects(&self, first: u64, next: u64) -> Option<bool> {
        let version = self.version.load(Ordering::SeqCst);
        if !version.is_multiple_of(2) {
            return None;
        }
        let mut lost = first < self.retired_before.load(Ordering::SeqCst);
        let count = self.count.load(Ordering::SeqCst).min(CAPACITY as u64) as usize;
        for entry in &self.intervals[..count] {
            let start = entry.first.load(Ordering::SeqCst);
            let end = entry.next.load(Ordering::SeqCst);
            lost |= start < next && first < end;
        }
        (self.version.load(Ordering::SeqCst) == version).then_some(lost)
    }
}
