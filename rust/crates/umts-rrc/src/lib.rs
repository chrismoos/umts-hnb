// SPDX-License-Identifier: AGPL-3.0-or-later

mod bits;
#[cfg(test)]
extern crate self as umts_rrc;
pub mod broadcast;
pub mod capabilities;
pub mod cell_selection;
pub mod common_channels;
pub mod dedicated;
pub mod establishment;
mod generated_information;
pub mod hsdpa;
pub mod hsupa;
pub mod integrity;
pub mod master_information;
pub mod measurement;
pub mod mobility;
pub mod nas;
pub mod paging;
pub mod random_access;
pub mod reports;
pub mod security;
pub mod system_information;
#[cfg(test)]
mod test_support;
pub mod transport;
pub mod ue_timers;

pub struct EncodedInformation {
    pub bytes: Vec<u8>,
    pub bit_length: usize,
}
