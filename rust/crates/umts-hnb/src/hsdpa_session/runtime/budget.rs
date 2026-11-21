// SPDX-License-Identifier: AGPL-3.0-or-later

use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Clone)]
pub struct CellSchedulingBudget {
    remaining: Arc<AtomicUsize>,
    tables: crate::hsdpa_packet::SharedPlanTables,
}

impl CellSchedulingBudget {
    pub fn new(bytes: NonZeroUsize) -> Self {
        let remaining = Arc::new(AtomicUsize::new(bytes.get()));
        Self {
            tables: crate::hsdpa_packet::SharedPlanTables::new(Arc::clone(&remaining)),
            remaining,
        }
    }

    pub fn tables(&self) -> &crate::hsdpa_packet::SharedPlanTables {
        &self.tables
    }

    pub fn available_bytes(&self) -> usize {
        self.remaining.load(Ordering::Relaxed)
    }

    pub(super) fn reserve(&self, bytes: NonZeroUsize) -> Option<Reservation> {
        self.remaining
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(bytes.get())
            })
            .ok()?;
        Some(Reservation {
            remaining: Arc::clone(&self.remaining),
            bytes: bytes.get(),
        })
    }
}

pub(super) struct Reservation {
    remaining: Arc<AtomicUsize>,
    bytes: usize,
}

impl Reservation {
    pub(super) fn retain(&mut self, bytes: usize) -> bool {
        let Some(unused) = self.bytes.checked_sub(bytes) else {
            return false;
        };
        self.bytes = bytes;
        self.remaining.fetch_add(unused, Ordering::Relaxed);
        true
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.remaining.fetch_add(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_share_capacity_and_failed_admission_rolls_back() {
        let pool = CellSchedulingBudget::new(NonZeroUsize::new(100).unwrap());
        let peer = pool.clone();
        let mut first = pool.reserve(NonZeroUsize::new(80).unwrap()).unwrap();
        assert!(peer.reserve(NonZeroUsize::new(21).unwrap()).is_none());
        assert!(!first.retain(81));
        assert_eq!(pool.available_bytes(), 20);
        assert!(first.retain(60));
        let second = peer.reserve(NonZeroUsize::new(40).unwrap()).unwrap();
        assert_eq!(pool.available_bytes(), 0);
        drop(first);
        assert_eq!(peer.available_bytes(), 60);
        drop(second);
        assert_eq!(pool.available_bytes(), 100);
    }
}
