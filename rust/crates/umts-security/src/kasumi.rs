// SPDX-License-Identifier: AGPL-3.0-or-later

mod tables;
use crate::Key;
use tables::{S7, S9};
use zeroize::Zeroize;

const ROUNDS: usize = 8;
const HALF_BITS: u32 = 16;
const SEVEN_BITS: u32 = 7;
const NINE_BITS: u32 = 9;
const SEVEN_MASK: u16 = (1 << SEVEN_BITS) - 1;
const NINE_MASK: u16 = (1 << NINE_BITS) - 1;
const KEY_MODIFIERS: [u16; ROUNDS] = [
    0x0123, 0x4567, 0x89ab, 0xcdef, 0xfedc, 0xba98, 0x7654, 0x3210,
];
const KO_OFFSETS: [usize; 3] = [1, 5, 6];
const KO_ROTATIONS: [u32; 3] = [5, 8, 13];
const KI_OFFSETS: [usize; 3] = [4, 3, 7];
const KL_SECOND_OFFSET: usize = 2;

pub(crate) struct Kasumi {
    kl: [[u16; 2]; ROUNDS],
    ko: [[u16; 3]; ROUNDS],
    ki: [[u16; 3]; ROUNDS],
}
impl Kasumi {
    pub fn new(key: &Key, modifier: u8) -> Self {
        let mut words = [0u16; ROUNDS];
        let mut prime = [0u16; ROUNDS];
        for (index, bytes) in key.bytes().chunks_exact(2).enumerate() {
            words[index] = u16::from_be_bytes([bytes[0] ^ modifier, bytes[1] ^ modifier]);
            prime[index] = words[index] ^ KEY_MODIFIERS[index];
        }
        let mut cipher = Self {
            kl: [[0; 2]; ROUNDS],
            ko: [[0; 3]; ROUNDS],
            ki: [[0; 3]; ROUNDS],
        };
        // TS 35.202 4.6 gives the rotation amounts and cyclic key-word offsets.
        for round in 0..ROUNDS {
            cipher.kl[round] = [
                words[round].rotate_left(1),
                prime[(round + KL_SECOND_OFFSET) % ROUNDS],
            ];
            for part in 0..3 {
                cipher.ko[round][part] =
                    words[(round + KO_OFFSETS[part]) % ROUNDS].rotate_left(KO_ROTATIONS[part]);
                cipher.ki[round][part] = prime[(round + KI_OFFSETS[part]) % ROUNDS];
            }
        }
        words.zeroize();
        prime.zeroize();
        cipher
    }

    pub fn encrypt(&self, block: u64) -> u64 {
        let mut left = (block >> u32::BITS) as u32;
        let mut right = block as u32;
        for round in (0..ROUNDS).step_by(2) {
            right ^= self.fo(self.fl(left, round), round);
            left ^= self.fl(self.fo(right, round + 1), round + 1);
        }
        (u64::from(left) << u32::BITS) | u64::from(right)
    }

    #[inline(always)]
    fn fl(&self, input: u32, round: usize) -> u32 {
        let mut left = (input >> HALF_BITS) as u16;
        let mut right = input as u16;
        right ^= (left & self.kl[round][0]).rotate_left(1);
        left ^= (right | self.kl[round][1]).rotate_left(1);
        (u32::from(left) << HALF_BITS) | u32::from(right)
    }

    #[inline(always)]
    fn fo(&self, input: u32, round: usize) -> u32 {
        let mut left = (input >> HALF_BITS) as u16;
        let mut right = input as u16;
        left = fi(left ^ self.ko[round][0], self.ki[round][0]) ^ right;
        right = fi(right ^ self.ko[round][1], self.ki[round][1]) ^ left;
        left = fi(left ^ self.ko[round][2], self.ki[round][2]) ^ right;
        (u32::from(right) << HALF_BITS) | u32::from(left)
    }
}
impl Drop for Kasumi {
    fn drop(&mut self) {
        self.kl.zeroize();
        self.ko.zeroize();
        self.ki.zeroize();
    }
}

fn fi(input: u16, key: u16) -> u16 {
    let mut nine = input >> SEVEN_BITS;
    let mut seven = input & SEVEN_MASK;
    nine = S9[usize::from(nine)] ^ seven;
    seven = S7[usize::from(seven)] ^ (nine & SEVEN_MASK);
    nine ^= key & NINE_MASK;
    seven ^= key >> NINE_BITS;
    // The masks leave in-range values unchanged and let the table lookups skip bounds checks.
    nine = S9[usize::from(nine & NINE_MASK)] ^ seven;
    seven = S7[usize::from(seven & SEVEN_MASK)] ^ (nine & SEVEN_MASK);
    (seven << NINE_BITS) | nine
}

#[cfg(test)]
mod tests;
