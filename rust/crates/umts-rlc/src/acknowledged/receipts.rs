// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::atomic::{AtomicU64, Ordering};
use umts_types::time::ChipInstant;

use super::AmError;

static NEXT_ENTITY: AtomicU64 = AtomicU64::new(1);

pub(super) fn new_owner() -> Result<u64, AmError> {
    NEXT_ENTITY
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map_err(|_| AmError::CountExhausted)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiveCoverage {
    pub(super) owner: u64,
    pub(super) generation: u64,
    pub(super) first: u64,
    pub(super) last: u64,
    pub(super) first_received: ChipInstant,
    pub(super) provenance: super::ReceiveProvenance,
    pub(super) air_start: Option<ChipInstant>,
}
impl ReceiveCoverage {
    /// Earliest observed contributing transmission, not proof that no earlier attempt was lost.
    pub fn air_start(self) -> Option<ChipInstant> {
        self.air_start
    }
    pub fn provenance(self) -> super::ReceiveProvenance {
        self.provenance
    }
    /// Earliest first acceptance of any contributing PDU, not collection time or a duplicate's arrival.
    pub fn first_received(self) -> ChipInstant {
        self.first_received
    }
    pub fn first_position(self) -> u64 {
        self.first
    }
    /// Includes the PDU with a deferred end-of-SDU LI, even if it includes no SDU bytes.
    pub fn last_position(self) -> u64 {
        self.last
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusCoverage {
    pub(super) owner: u64,
    pub(super) generation: u64,
    pub(super) next: u64,
}
impl StatusCoverage {
    /// This proves what STATUS advertises, not that the radio transmitted it or the peer received it.
    pub fn covers(self, received: ReceiveCoverage) -> bool {
        self.owner == received.owner
            && self.generation == received.generation
            && self.next > received.last
    }
    pub fn next_position(self) -> u64 {
        self.next
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SduReceipt {
    pub(super) owner: u64,
    pub(super) serial: u64,
    pub(super) slot: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SduTransmission {
    Queued,
    InFlight {
        first: u64,
        last: u64,
        delimited: bool,
    },
    Acknowledged,
    Discarded,
}

#[derive(Clone, Copy)]
pub(super) struct Tracking {
    pub serial: u64,
    pub state: SduTransmission,
}
