// SPDX-License-Identifier: AGPL-3.0-or-later

pub use crate::snow_algorithms::{Uea2Key, Uia2Key};
use crate::{Error, Key, kasumi::Kasumi};
use umts_types::Direction;

const BLOCK_BITS: usize = u64::BITS as usize;
const BLOCK_BYTES: usize = BLOCK_BITS / u8::BITS as usize;
const KEY_UNMODIFIED: u8 = 0;
const CONFIDENTIALITY_MODIFIER: u8 = 0x55;
const INTEGRITY_MODIFIER: u8 = 0xaa;
const BEARER_BITS: u32 = 5;
const BEARER_SHIFT: u32 = 27;
const DIRECTION_SHIFT: u32 = 26;
pub const MAX_F8_BITS: usize = 20_000;

pub struct Uea1Key {
    cipher: Kasumi,
    initialization: Kasumi,
}
pub struct Uia1Key {
    cipher: Kasumi,
    finalization: Kasumi,
}

impl Uea1Key {
    pub fn new(key: &Key) -> Self {
        Self {
            cipher: Kasumi::new(key, KEY_UNMODIFIED),
            initialization: Kasumi::new(key, CONFIDENTIALITY_MODIFIER),
        }
    }
    pub fn apply(
        &self,
        count: u32,
        bearer: u8,
        direction: Direction,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<(), Error> {
        if bits == 0 || bits > MAX_F8_BITS || bytes.len() != bits.div_ceil(u8::BITS as usize) {
            return Err(Error::Length);
        }
        if u32::from(bearer) >= 1 << BEARER_BITS {
            return Err(Error::Bearer);
        }
        // TS 35.201 3.4/3.5 puts COUNT, bearer and direction at the most-significant end.
        let initial = (u64::from(count) << u32::BITS)
            | (u64::from(bearer) << BEARER_SHIFT)
            | (direction_bit(direction) << DIRECTION_SHIFT);
        let initial = self.initialization.encrypt(initial);
        let whole_blocks = bits / BLOCK_BITS;
        let (whole, tail) = bytes.split_at_mut(whole_blocks * BLOCK_BYTES);
        let mut previous = 0;
        let mut block = 0_u64;
        for bytes in whole.chunks_exact_mut(BLOCK_BYTES) {
            previous = self.cipher.encrypt(initial ^ block ^ previous);
            let mut word = [0; BLOCK_BYTES];
            word.copy_from_slice(bytes);
            bytes.copy_from_slice(&(u64::from_be_bytes(word) ^ previous).to_be_bytes());
            block += 1;
        }
        if !tail.is_empty() {
            let stream = self
                .cipher
                .encrypt(initial ^ block ^ previous)
                .to_be_bytes();
            let tail_bits = bits - whole_blocks * BLOCK_BITS;
            for (offset, byte) in tail.iter_mut().enumerate() {
                let used = (tail_bits - offset * u8::BITS as usize).min(u8::BITS as usize);
                let mask = u8::MAX << (u8::BITS as usize - used);
                *byte ^= stream[offset] & mask;
            }
        }
        Ok(())
    }
}

impl Uia1Key {
    pub fn new(key: &Key) -> Self {
        Self {
            cipher: Kasumi::new(key, KEY_UNMODIFIED),
            finalization: Kasumi::new(key, INTEGRITY_MODIFIER),
        }
    }
    pub fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error> {
        if bytes.len() != bits.div_ceil(u8::BITS as usize) {
            return Err(Error::Length);
        }
        let total = bits.checked_add(2).ok_or(Error::Length)?;
        let mut chain = self
            .cipher
            .encrypt((u64::from(count) << u32::BITS) | u64::from(fresh));
        let mut digest = chain;
        let mut input = 0u64;
        let mut filled = 0;
        for index in 0..total {
            let bit = if index < bits {
                u64::from(
                    (bytes[index / u8::BITS as usize]
                        >> (u8::BITS as usize - 1 - index % u8::BITS as usize))
                        & 1,
                )
            } else if index == bits {
                direction_bit(direction)
            } else {
                1
            };
            input = (input << 1) | bit;
            filled += 1;
            if filled == BLOCK_BITS {
                chain = self.cipher.encrypt(chain ^ input);
                digest ^= chain;
                input = 0;
                filled = 0;
            }
        }
        if filled != 0 {
            chain = self
                .cipher
                .encrypt(chain ^ (input << (BLOCK_BITS - filled)));
            digest ^= chain;
        }
        Ok(((self.finalization.encrypt(digest) >> u32::BITS) as u32).to_be_bytes())
    }
}

fn direction_bit(direction: Direction) -> u64 {
    match direction {
        Direction::Uplink => 0,
        Direction::Downlink => 1,
    }
}

#[cfg(test)]
mod tests;
