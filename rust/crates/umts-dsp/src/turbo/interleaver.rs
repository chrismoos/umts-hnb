// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{MAX_BLOCK_BITS, MIN_BLOCK_BITS};
use crate::DspError;

// TS 25.212 Table 2, sorted prime/root pairs.
pub(super) const PRIMES: [(usize, usize); 52] = [
    (7, 3),
    (11, 2),
    (13, 2),
    (17, 3),
    (19, 2),
    (23, 5),
    (29, 2),
    (31, 3),
    (37, 2),
    (41, 6),
    (43, 3),
    (47, 5),
    (53, 2),
    (59, 2),
    (61, 2),
    (67, 2),
    (71, 7),
    (73, 5),
    (79, 3),
    (83, 2),
    (89, 3),
    (97, 5),
    (101, 2),
    (103, 5),
    (107, 2),
    (109, 6),
    (113, 3),
    (127, 3),
    (131, 2),
    (137, 3),
    (139, 2),
    (149, 2),
    (151, 6),
    (157, 5),
    (163, 2),
    (167, 5),
    (173, 2),
    (179, 2),
    (181, 2),
    (191, 19),
    (193, 5),
    (197, 2),
    (199, 3),
    (211, 2),
    (223, 3),
    (227, 2),
    (229, 6),
    (233, 3),
    (239, 7),
    (241, 7),
    (251, 6),
    (257, 3),
];
const ROWS_5: [usize; 5] = [4, 3, 2, 1, 0];
const ROWS_10: [usize; 10] = [9, 8, 7, 6, 5, 4, 3, 2, 1, 0];
const ROWS_20: [usize; 20] = [
    19, 9, 14, 4, 0, 2, 5, 7, 12, 18, 10, 8, 13, 17, 3, 1, 16, 6, 15, 11,
];
const ROWS_20_SPECIAL: [usize; 20] = [
    19, 9, 14, 4, 0, 2, 5, 7, 12, 18, 16, 13, 17, 15, 3, 1, 6, 11, 8, 10,
];

pub struct TurboInterleaver {
    source: Box<[usize]>,
}

impl TurboInterleaver {
    pub fn new(length: usize) -> Result<Self, DspError> {
        if !(MIN_BLOCK_BITS..=MAX_BLOCK_BITS).contains(&length) {
            return Err(DspError::InvalidLength);
        }
        // TS 25.212 4.2.3.2.3, including the exceptional 481..530 construction.
        let rows: &[usize] = match length {
            40..=159 => &ROWS_5,
            160..=200 | 481..=530 => &ROWS_10,
            2281..=2480 | 3161..=3210 => &ROWS_20_SPECIAL,
            _ => &ROWS_20,
        };
        let (prime, root) = if (481..=530).contains(&length) {
            (53, 2)
        } else {
            PRIMES
                .iter()
                .copied()
                .find(|(prime, _)| length <= rows.len() * (prime + 1))
                .ok_or(DspError::InvalidLength)?
        };
        let columns = if (481..=530).contains(&length) {
            prime
        } else if length <= rows.len() * (prime - 1) {
            prime - 1
        } else if length <= rows.len() * prime {
            prime
        } else {
            prime + 1
        };
        let mut base = vec![1; prime - 1];
        for index in 1..base.len() {
            base[index] = root * base[index - 1] % prime;
        }
        let mut multipliers = [0; ROWS_20.len()];
        let mut q = 1;
        for (index, &row) in rows.iter().enumerate() {
            if index != 0 {
                q = PRIMES
                    .iter()
                    .map(|&(candidate, _)| candidate)
                    .find(|&candidate| candidate > q && gcd(candidate, prime - 1) == 1)
                    .ok_or(DspError::InvalidCode)?;
            }
            multipliers[row] = q;
        }
        let mut source = Vec::with_capacity(length);
        for column in 0..columns {
            for &row in rows {
                let mut column = column;
                if columns == prime + 1 && length == rows.len() * columns && row == rows.len() - 1 {
                    if column == 0 {
                        column = prime;
                    } else if column == prime {
                        column = 0;
                    }
                }
                let original_column = if column < prime - 1 {
                    base[column * multipliers[row] % (prime - 1)]
                        - usize::from(columns == prime - 1)
                } else if column == prime - 1 {
                    0
                } else {
                    prime
                };
                let original = row * columns + original_column;
                if original < length {
                    source.push(original);
                }
            }
        }
        if source.len() != length {
            return Err(DspError::InvalidCode);
        }
        Ok(Self {
            source: source.into_boxed_slice(),
        })
    }

    pub fn len(&self) -> usize {
        self.source.len()
    }
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// Each output position names its original, zero-based input position.
    pub fn source(&self) -> &[usize] {
        &self.source
    }
}

fn gcd(mut first: usize, mut second: usize) -> usize {
    while second != 0 {
        (first, second) = (second, first % second);
    }
    first
}
