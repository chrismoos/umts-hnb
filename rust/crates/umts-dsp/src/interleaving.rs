// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::DspError;

#[derive(Clone, Copy, Debug)]
pub enum Rel99Tti {
    Ms10,
    Ms20,
    Ms40,
    Ms80,
}

impl Rel99Tti {
    pub fn frames(self) -> usize {
        match self {
            Self::Ms10 => 1,
            Self::Ms20 => 2,
            Self::Ms40 => 4,
            Self::Ms80 => 8,
        }
    }

    pub(crate) fn permutation(self) -> &'static [usize] {
        // TS 25.212 4.2.5.2 table 4.
        match self {
            Self::Ms10 => &[0],
            Self::Ms20 => &[0, 1],
            Self::Ms40 => &[0, 2, 1, 3],
            Self::Ms80 => &[0, 4, 2, 6, 1, 5, 3, 7],
        }
    }
}

// TS 25.212 4.2.11 table 7.
const SECOND_PERMUTATION: [usize; 30] = [
    0, 20, 10, 5, 15, 25, 3, 13, 23, 8, 18, 28, 1, 11, 21, 6, 16, 26, 4, 14, 24, 19, 9, 29, 12, 2,
    7, 22, 27, 17,
];

pub struct Interleaver {
    source: Vec<usize>,
}

impl Interleaver {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.source.capacity() * std::mem::size_of::<usize>()
    }
    pub fn first(tti: Rel99Tti, length: usize) -> Result<Self, DspError> {
        if !length.is_multiple_of(tti.frames()) {
            return Err(DspError::InvalidLength);
        }
        Self::rectangular(length, tti.permutation())
    }

    pub fn second(length: usize) -> Result<Self, DspError> {
        Self::rectangular(length, &SECOND_PERMUTATION)
    }

    fn rectangular(length: usize, permutation: &[usize]) -> Result<Self, DspError> {
        let columns = permutation.len();
        let rows = length.div_ceil(columns);
        rows.checked_mul(columns).ok_or(DspError::InvalidLength)?;
        let mut source = Vec::new();
        source
            .try_reserve_exact(length)
            .map_err(|_| DspError::Capacity)?;
        for &column in permutation {
            for row in 0..rows {
                let index = row * columns + column;
                if index < length {
                    source.push(index);
                }
            }
        }
        Ok(Self { source })
    }

    pub fn interleave<T: Copy>(&self, input: &[T], output: &mut [T]) -> Result<(), DspError> {
        if input.len() != self.source.len() || output.len() != input.len() {
            return Err(DspError::InvalidLength);
        }
        for (destination, &source) in output.iter_mut().zip(&self.source) {
            *destination = input[source];
        }
        Ok(())
    }

    pub fn deinterleave<T: Copy>(&self, input: &[T], output: &mut [T]) -> Result<(), DspError> {
        if input.len() != self.source.len() || output.len() != input.len() {
            return Err(DspError::InvalidLength);
        }
        for (&value, &destination) in input.iter().zip(&self.source) {
            output[destination] = value;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_interleaver_matches_the_spec_column_order() {
        let input: Vec<_> = (0..16).collect();
        let mut output = vec![0; 16];
        Interleaver::first(Rel99Tti::Ms40, 16)
            .unwrap()
            .interleave(&input, &mut output)
            .unwrap();
        assert_eq!(
            output,
            [0, 4, 8, 12, 2, 6, 10, 14, 1, 5, 9, 13, 3, 7, 11, 15]
        );
        assert!(Interleaver::first(Rel99Tti::Ms40, 15).is_err());
    }

    #[test]
    fn second_interleaver_prunes_dummy_positions_after_column_permutation() {
        let input: Vec<_> = (0..31).collect();
        let mut output = vec![0; 31];
        Interleaver::second(31)
            .unwrap()
            .interleave(&input, &mut output)
            .unwrap();
        assert_eq!(
            output,
            [
                0, 30, 20, 10, 5, 15, 25, 3, 13, 23, 8, 18, 28, 1, 11, 21, 6, 16, 26, 4, 14, 24,
                19, 9, 29, 12, 2, 7, 22, 27, 17
            ]
        );
    }

    #[test]
    fn all_legal_ttis_and_uneven_second_matrices_preserve_order_on_inverse() {
        for length in [0, 1, 2, 29, 30, 31, 81, 244, 512, 7680] {
            let input: Vec<_> = (0..length).collect();
            let mut output = vec![0; length];
            let mut recovered = vec![0; length];
            let second = Interleaver::second(length).unwrap();
            second.interleave(&input, &mut output).unwrap();
            second.deinterleave(&output, &mut recovered).unwrap();
            assert_eq!(recovered, input);
        }
        for tti in [
            Rel99Tti::Ms10,
            Rel99Tti::Ms20,
            Rel99Tti::Ms40,
            Rel99Tti::Ms80,
        ] {
            let length = 81 * tti.frames();
            let input: Vec<_> = (0..length).collect();
            let mut output = vec![0; length];
            let mut recovered = vec![0; length];
            let first = Interleaver::first(tti, length).unwrap();
            first.interleave(&input, &mut output).unwrap();
            first.deinterleave(&output, &mut recovered).unwrap();
            assert_eq!(recovered, input);
        }
    }
}
