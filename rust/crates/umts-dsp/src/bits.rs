// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::DspError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Bit {
    #[default]
    Zero = 0,
    One = 1,
}

impl From<bool> for Bit {
    fn from(value: bool) -> Self {
        if value { Self::One } else { Self::Zero }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TxBit {
    Data(Bit),
    #[default]
    Dtx,
}

impl TxBit {
    // TS 25.213 5.1.1.1 distinguishes a transmitted zero from DTX.
    pub fn amplitude(self) -> f32 {
        match self {
            Self::Data(Bit::Zero) => 1.0,
            Self::Data(Bit::One) => -1.0,
            Self::Dtx => 0.0,
        }
    }
}

pub fn unpack_msb(bytes: &[u8], output: &mut [Bit]) -> Result<(), DspError> {
    if output.len().div_ceil(8) > bytes.len() {
        return Err(DspError::InvalidLength);
    }
    for (index, bit) in output.iter_mut().enumerate() {
        *bit = Bit::from(bytes[index / 8] & (1 << (7 - index % 8)) != 0);
    }
    Ok(())
}

pub fn pack_msb(bits: &[Bit], output: &mut [u8]) -> Result<(), DspError> {
    if output.len() != bits.len().div_ceil(8) {
        return Err(DspError::InvalidLength);
    }
    output.fill(0);
    for (index, &bit) in bits.iter().enumerate() {
        output[index / 8] |= (bit as u8) << (7 - index % 8);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_length_does_not_include_wire_padding() {
        let mut bits = [Bit::Zero; 11];
        unpack_msb(&[0xa5, 0xc0], &mut bits).unwrap();
        assert_eq!(&bits[..4], &[Bit::One, Bit::Zero, Bit::One, Bit::Zero]);
        let mut bytes = [0xff; 2];
        pack_msb(&bits, &mut bytes).unwrap();
        assert_eq!(bytes, [0xa5, 0xc0]);
        assert!(unpack_msb(&[], &mut bits).is_err());
        assert!(pack_msb(&bits, &mut []).is_err());
    }
}
