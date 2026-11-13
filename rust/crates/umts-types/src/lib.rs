// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod bits;
pub mod channel_codes;
pub mod hsdpa;
pub mod hsupa;
pub mod identity;
pub mod time;

use std::num::NonZeroU64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RadioEpoch(NonZeroU64);

impl RadioEpoch {
    pub const fn new(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CnDomain {
    Cs,
    Ps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Uplink,
    Downlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UeKey {
    pub epoch: RadioEpoch,
    pub slot: u32,
    pub generation: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    pub ue: UeKey,
    pub domain: CnDomain,
}
