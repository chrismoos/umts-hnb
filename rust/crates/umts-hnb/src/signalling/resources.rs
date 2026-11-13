// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroU64;
use std::sync::Arc;

use umts_types::{RadioEpoch, UeKey};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub sample_bytes: u64,
    pub harq_bytes: u64,
    pub queue_bytes: u64,
    pub cpu_ns_per_frame: u64,
}

impl Resources {
    fn add(self, rhs: Self) -> Option<Self> {
        Some(Self {
            sample_bytes: self.sample_bytes.checked_add(rhs.sample_bytes)?,
            harq_bytes: self.harq_bytes.checked_add(rhs.harq_bytes)?,
            queue_bytes: self.queue_bytes.checked_add(rhs.queue_bytes)?,
            cpu_ns_per_frame: self.cpu_ns_per_frame.checked_add(rhs.cpu_ns_per_frame)?,
        })
    }

    fn fits(self, limit: Self) -> bool {
        self.sample_bytes <= limit.sample_bytes
            && self.harq_bytes <= limit.harq_bytes
            && self.queue_bytes <= limit.queue_bytes
            && self.cpu_ns_per_frame <= limit.cpu_ns_per_frame
    }

    fn subtract(self, rhs: Self) -> Self {
        Self {
            sample_bytes: self.sample_bytes - rhs.sample_bytes,
            harq_bytes: self.harq_bytes - rhs.harq_bytes,
            queue_bytes: self.queue_bytes - rhs.queue_bytes,
            cpu_ns_per_frame: self.cpu_ns_per_frame - rhs.cpu_ns_per_frame,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    WrongEpoch,
    Capacity,
    InvalidLease,
}

#[must_use]
#[derive(Debug)]
pub struct Lease {
    ledger: Arc<()>,
    owner: UeKey,
    allocation: NonZeroU64,
}

struct Allocation {
    owner: UeKey,
    id: NonZeroU64,
    resources: Resources,
}

pub struct ResourceLedger {
    identity: Arc<()>,
    epoch: RadioEpoch,
    capacity: Resources,
    used: Resources,
    next_id: Option<NonZeroU64>,
    allocations: Vec<Option<Allocation>>,
}

impl ResourceLedger {
    pub fn new(epoch: RadioEpoch, capacity: Resources, max_leases: usize) -> Self {
        Self {
            identity: Arc::new(()),
            epoch,
            capacity,
            used: Resources::default(),
            next_id: Some(NonZeroU64::MIN),
            allocations: (0..max_leases).map(|_| None).collect(),
        }
    }

    #[cfg(test)]
    pub fn used(&self) -> Resources {
        self.used
    }
    pub fn contains(&self, lease: &Lease) -> bool {
        Arc::ptr_eq(&self.identity, &lease.ledger)
            && self.allocations.iter().flatten().any(|allocation| {
                allocation.owner == lease.owner && allocation.id == lease.allocation
            })
    }

    pub fn reserve(&mut self, owner: UeKey, resources: Resources) -> Result<Lease, AdmissionError> {
        if owner.epoch != self.epoch {
            return Err(AdmissionError::WrongEpoch);
        }
        let used = self
            .used
            .add(resources)
            .filter(|used| used.fits(self.capacity))
            .ok_or(AdmissionError::Capacity)?;
        let id = self.next_id.ok_or(AdmissionError::Capacity)?;
        let slot = self
            .allocations
            .iter_mut()
            .find(|allocation| allocation.is_none())
            .ok_or(AdmissionError::Capacity)?;
        *slot = Some(Allocation {
            owner,
            id,
            resources,
        });
        self.used = used;
        self.next_id = id.checked_add(1);
        Ok(Lease {
            ledger: Arc::clone(&self.identity),
            owner,
            allocation: id,
        })
    }

    pub fn release(&mut self, lease: &Lease) -> Result<(), AdmissionError> {
        if !Arc::ptr_eq(&self.identity, &lease.ledger) {
            return Err(AdmissionError::InvalidLease);
        }
        let slot = self
            .allocations
            .iter_mut()
            .find(|entry| {
                entry.as_ref().is_some_and(|allocation| {
                    allocation.owner == lease.owner && allocation.id == lease.allocation
                })
            })
            .ok_or(AdmissionError::InvalidLease)?;
        let allocation = slot.take().expect("matching allocation exists");
        self.used = self.used.subtract(allocation.resources);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_membership_requires_the_actual_live_ledger_allocation() {
        let capacity = Resources {
            queue_bytes: 10,
            ..Resources::default()
        };
        let mut ledger = ResourceLedger::new(owner(0).epoch, capacity, 2);
        let other = ResourceLedger::new(owner(0).epoch, capacity, 2);
        let lease = ledger.reserve(owner(0), capacity).unwrap();
        assert!(ledger.contains(&lease));
        assert!(!other.contains(&lease));
        ledger.release(&lease).unwrap();
        assert!(!ledger.contains(&lease));
        let next = ledger.reserve(owner(0), capacity).unwrap();
        assert!(ledger.contains(&next));
        assert!(!ledger.contains(&lease));
    }

    fn owner(slot: u32) -> UeKey {
        UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot,
            generation: NonZeroU64::MIN,
        }
    }

    #[test]
    fn capacity_and_release_are_per_lease_and_all_or_nothing() {
        let cap = Resources {
            sample_bytes: 100,
            harq_bytes: 40,
            queue_bytes: 50,
            cpu_ns_per_frame: 80,
        };
        let mut ledger = ResourceLedger::new(owner(0).epoch, cap, 3);
        let demand = Resources {
            sample_bytes: 50,
            harq_bytes: 20,
            queue_bytes: 25,
            cpu_ns_per_frame: 40,
        };
        let first = ledger.reserve(owner(0), demand).unwrap();
        let second = ledger.reserve(owner(1), demand).unwrap();
        assert_eq!(ledger.used(), cap);
        assert!(matches!(
            ledger.reserve(owner(2), demand),
            Err(AdmissionError::Capacity)
        ));
        assert_eq!(ledger.used(), cap);
        ledger.release(&first).unwrap();
        assert_eq!(ledger.release(&first), Err(AdmissionError::InvalidLease));
        assert_eq!(ledger.used(), demand);
        ledger.release(&second).unwrap();
        assert_eq!(ledger.used(), Resources::default());
    }

    #[test]
    fn overflow_and_wrong_epoch_do_not_change_accounting() {
        let cap = Resources {
            queue_bytes: u64::MAX,
            ..Resources::default()
        };
        let mut ledger = ResourceLedger::new(owner(0).epoch, cap, 3);
        let lease = ledger.reserve(owner(0), cap).unwrap();
        assert!(matches!(
            ledger.reserve(
                owner(1),
                Resources {
                    queue_bytes: 1,
                    ..Resources::default()
                }
            ),
            Err(AdmissionError::Capacity)
        ));
        let wrong = UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
            ..owner(0)
        };
        assert!(matches!(
            ledger.reserve(wrong, Resources::default()),
            Err(AdmissionError::WrongEpoch)
        ));
        assert_eq!(ledger.used(), cap);
        ledger.release(&lease).unwrap();
    }

    #[test]
    fn another_ledger_cannot_consume_a_matching_owner_and_allocation_id() {
        let cap = Resources {
            queue_bytes: 100,
            ..Resources::default()
        };
        let mut first = ResourceLedger::new(owner(0).epoch, cap, 1);
        let mut second = ResourceLedger::new(owner(0).epoch, cap, 1);
        let a = first.reserve(owner(0), cap).unwrap();
        let b = second.reserve(owner(0), cap).unwrap();
        assert_eq!(second.release(&a), Err(AdmissionError::InvalidLease));
        assert_eq!(second.used(), cap);
        first.release(&a).unwrap();
        second.release(&b).unwrap();
    }
}
