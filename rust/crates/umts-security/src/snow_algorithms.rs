// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{Error, Key, algorithms::MAX_F8_BITS, snow3g::Snow3g};
use umts_types::Direction;
use zeroize::Zeroizing;

const BEARER_LIMIT: u8 = 32;
const BEARER_SHIFT: u32 = 27;
const CIPHER_DIRECTION_SHIFT: u32 = 26;
const INTEGRITY_COUNT_DIRECTION_SHIFT: u32 = 31;
const INTEGRITY_FRESH_DIRECTION_SHIFT: u32 = 15;
const INTEGRITY_POLYNOMIAL: u64 = 0x1b;

pub struct Uea2Key(Key);
pub struct Uia2Key(Key);

impl Uea2Key {
    pub fn new(key: &Key) -> Self {
        Self(Key::new(*key.bytes()))
    }

    pub fn apply(
        &self,
        count: u32,
        bearer: u8,
        direction: Direction,
        bytes: &mut [u8],
        bits: usize,
    ) -> Result<(), Error> {
        if bits == 0 || bits > MAX_F8_BITS || bytes.len() != bits.div_ceil(8) {
            return Err(Error::Length);
        }
        if bearer >= BEARER_LIMIT {
            return Err(Error::Bearer);
        }
        let control = (u32::from(bearer) << BEARER_SHIFT)
            | (direction_bit(direction) << CIPHER_DIRECTION_SHIFT);
        let mut stream = Snow3g::new(&self.0, [count, control, count, control]);
        for (index, chunk) in bytes.chunks_mut(4).enumerate() {
            let word = Zeroizing::new(stream.next_word().to_be_bytes());
            for (offset, byte) in chunk.iter_mut().enumerate() {
                let used = (bits - (index * 4 + offset) * 8).min(8);
                *byte ^= word[offset] & (u8::MAX << (8 - used));
            }
        }
        Ok(())
    }
}

impl Uia2Key {
    pub fn new(key: &Key) -> Self {
        Self(Key::new(*key.bytes()))
    }

    pub fn authenticate(
        &self,
        count: u32,
        fresh: u32,
        direction: Direction,
        bytes: &[u8],
        bits: usize,
    ) -> Result<[u8; 4], Error> {
        if bits == 0 || bytes.len() != bits.div_ceil(8) {
            return Err(Error::Length);
        }
        let length = u64::try_from(bits).map_err(|_| Error::Length)?;
        let direction = direction_bit(direction);
        let mut stream = Snow3g::new(
            &self.0,
            [
                count,
                fresh,
                count ^ (direction << INTEGRITY_COUNT_DIRECTION_SHIFT),
                fresh ^ (direction << INTEGRITY_FRESH_DIRECTION_SHIFT),
            ],
        );
        let p =
            Zeroizing::new((u64::from(stream.next_word()) << 32) | u64::from(stream.next_word()));
        let q =
            Zeroizing::new((u64::from(stream.next_word()) << 32) | u64::from(stream.next_word()));
        let mask = Zeroizing::new(stream.next_word());
        let mut evaluation = Zeroizing::new(0u64);
        for (index, chunk) in bytes.chunks(8).enumerate() {
            let mut block = [0; 8];
            block[..chunk.len()].copy_from_slice(chunk);
            let used = (bits - index * 64).min(64);
            let message = u64::from_be_bytes(block) & (u64::MAX << (64 - used));
            *evaluation = multiply(*evaluation ^ message, *p);
        }
        *evaluation = multiply(*evaluation ^ length, *q);
        Ok(((*evaluation >> 32) as u32 ^ *mask).to_be_bytes())
    }
}

fn direction_bit(direction: Direction) -> u32 {
    match direction {
        Direction::Uplink => 0,
        Direction::Downlink => 1,
    }
}

// TS 35.215 f9 multiplication in GF(2^64), with fixed work per block.
fn multiply(mut value: u64, mut factor: u64) -> u64 {
    let mut result = 0;
    for _ in 0..u64::BITS {
        result ^= value & 0u64.wrapping_sub(factor & 1);
        value = (value << 1) ^ (INTEGRITY_POLYNOMIAL & 0u64.wrapping_sub(value >> 63));
        factor >>= 1;
    }
    result
}

#[cfg(test)]
mod tests;
