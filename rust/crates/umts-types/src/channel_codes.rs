// SPDX-License-Identifier: AGPL-3.0-or-later

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelCodeError {
    InvalidCode,
    InvalidLength,
}

const MAX_SPREADING_FACTOR: u16 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OvsfCode {
    spreading_factor: u16,
    index: u16,
    reversed_index: u16,
}

impl OvsfCode {
    pub fn new(spreading_factor: u16, index: u16) -> Result<Self, ChannelCodeError> {
        if !spreading_factor.is_power_of_two()
            || spreading_factor > MAX_SPREADING_FACTOR
            || index >= spreading_factor
        {
            return Err(ChannelCodeError::InvalidCode);
        }
        let reversed_index = if spreading_factor == 1 {
            0
        } else {
            index.reverse_bits() >> (u16::BITS - spreading_factor.ilog2())
        };
        Ok(Self {
            spreading_factor,
            index,
            reversed_index,
        })
    }

    pub fn spreading_factor(self) -> u16 {
        self.spreading_factor
    }

    pub fn index(self) -> u16 {
        self.index
    }

    pub fn write(self, output: &mut [f32]) -> Result<(), ChannelCodeError> {
        if output.len() != usize::from(self.spreading_factor) {
            return Err(ChannelCodeError::InvalidLength);
        }
        // TS 25.213 4.3.1.1 recursive code-tree ordering.
        for (chip, value) in output.iter_mut().enumerate() {
            *value = if ((chip as u16) & self.reversed_index)
                .count_ones()
                .is_multiple_of(2)
            {
                1.0
            } else {
                -1.0
            };
        }
        Ok(())
    }

    pub fn overlaps(self, other: Self) -> bool {
        let (coarse, fine) = if self.spreading_factor <= other.spreading_factor {
            (self, other)
        } else {
            (other, self)
        };
        fine.index >> (fine.spreading_factor.ilog2() - coarse.spreading_factor.ilog2())
            == coarse.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_tree_vectors_and_all_codes_at_one_level_are_orthogonal() {
        let vectors = [
            [1.0, 1.0, 1.0, 1.0],
            [1.0, 1.0, -1.0, -1.0],
            [1.0, -1.0, 1.0, -1.0],
            [1.0, -1.0, -1.0, 1.0],
        ];
        for (index, expected) in vectors.iter().enumerate() {
            let mut actual = [0.0; 4];
            OvsfCode::new(4, index as u16)
                .unwrap()
                .write(&mut actual)
                .unwrap();
            assert_eq!(&actual, expected);
        }
        for a in 0..32 {
            for b in 0..32 {
                let mut x = [0.0; 32];
                let mut y = [0.0; 32];
                OvsfCode::new(32, a).unwrap().write(&mut x).unwrap();
                OvsfCode::new(32, b).unwrap().write(&mut y).unwrap();
                let dot: f32 = x.iter().zip(y).map(|(&x, y)| x * y).sum();
                assert_eq!(dot, if a == b { 32.0 } else { 0.0 });
            }
        }
    }

    #[test]
    fn ancestor_claims_conflict_and_invalid_codes_are_rejected() {
        let root = OvsfCode::new(4, 1).unwrap();
        assert!(root.overlaps(OvsfCode::new(16, 4).unwrap()));
        assert!(root.overlaps(OvsfCode::new(16, 7).unwrap()));
        assert!(!root.overlaps(OvsfCode::new(16, 8).unwrap()));
        assert!(OvsfCode::new(0, 0).is_err());
        assert!(OvsfCode::new(3, 0).is_err());
        assert!(OvsfCode::new(512, 512).is_err());
    }
}
