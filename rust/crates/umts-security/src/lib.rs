// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod algorithms;
pub mod ciphering;
pub mod contexts;
pub mod counters;
mod kasumi;
pub mod negotiation;
mod snow3g;
mod snow_algorithms;

use zeroize::Zeroize;

pub const KEY_BYTES: usize = 16;
pub struct Key([u8; KEY_BYTES]);
impl Key {
    pub fn new(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }
    pub(crate) fn bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }
}
impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key([redacted])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Length,
    Bearer,
    Count,
    Origin,
    Exhausted,
    UnsupportedAlgorithm,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Length => "Security input bit length is invalid",
            Self::Bearer => "Security bearer identity is invalid",
            Self::Count => "Security counter or START is invalid",
            Self::Origin => "Security owner generation or direction does not match",
            Self::Exhausted => "Security counter is exhausted",
            Self::UnsupportedAlgorithm => "Security algorithm is not implemented",
        })
    }
}
impl std::error::Error for Error {}
