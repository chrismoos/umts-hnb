// SPDX-License-Identifier: AGPL-3.0-or-later

pub const MAX_IMSI_DIGITS: usize = 15;
const MIN_IMSI_DIGITS: usize = 5;
const MAX_BCD_BYTES: usize = MAX_IMSI_DIGITS.div_ceil(2);
const NIBBLE_MASK: u8 = 0xf;
const NIBBLE_SHIFT: u32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    Length,
    Digit,
    Filler,
}
impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Length => "Invalid IMSI length",
            Self::Digit => "IMSI contains a non-decimal digit",
            Self::Filler => "IMSI filler is not terminal",
        })
    }
}
impl std::error::Error for IdentityError {}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Imsi {
    bytes: [u8; MAX_BCD_BYTES],
    digits: u8,
}
impl std::fmt::Debug for Imsi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Imsi([redacted])")
    }
}
impl Imsi {
    pub fn from_digits(digits: &str) -> Result<Self, IdentityError> {
        if !(MIN_IMSI_DIGITS..=MAX_IMSI_DIGITS).contains(&digits.len()) {
            return Err(IdentityError::Length);
        }
        let mut values = [0; MAX_IMSI_DIGITS];
        for (index, digit) in digits.bytes().enumerate() {
            if !digit.is_ascii_digit() {
                return Err(IdentityError::Digit);
            }
            values[index] = digit - b'0';
        }
        Self::from_digit_values(&values[..digits.len()])
    }
    pub fn from_digit_values(digits: &[u8]) -> Result<Self, IdentityError> {
        if !(MIN_IMSI_DIGITS..=MAX_IMSI_DIGITS).contains(&digits.len()) {
            return Err(IdentityError::Length);
        }
        if digits.iter().any(|digit| *digit > 9) {
            return Err(IdentityError::Digit);
        }
        let mut bytes = [0; MAX_BCD_BYTES];
        for (index, pair) in digits.chunks(2).enumerate() {
            bytes[index] = pair[0] | (pair.get(1).copied().unwrap_or(NIBBLE_MASK) << NIBBLE_SHIFT);
        }
        Ok(Self {
            bytes,
            digits: digits.len() as u8,
        })
    }
    pub fn from_bcd(bytes: &[u8]) -> Result<Self, IdentityError> {
        if !(MIN_IMSI_DIGITS.div_ceil(2)..=MAX_BCD_BYTES).contains(&bytes.len()) {
            return Err(IdentityError::Length);
        }
        let mut count = bytes.len() * 2;
        for (index, byte) in bytes.iter().enumerate() {
            if byte & NIBBLE_MASK > 9 {
                return Err(IdentityError::Digit);
            }
            let high = byte >> NIBBLE_SHIFT;
            if high == NIBBLE_MASK {
                if index + 1 != bytes.len() {
                    return Err(IdentityError::Filler);
                }
                count -= 1;
            } else if high > 9 {
                return Err(IdentityError::Digit);
            }
        }
        if !(MIN_IMSI_DIGITS..=MAX_IMSI_DIGITS).contains(&count) {
            return Err(IdentityError::Length);
        }
        let mut storage = [0; MAX_BCD_BYTES];
        storage[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: storage,
            digits: count as u8,
        })
    }
    // HNBAP IMSI octets put the first digit in the low nibble and permit only terminal high-nibble filler.
    pub fn bcd(&self) -> &[u8] {
        &self.bytes[..usize::from(self.digits).div_ceil(2)]
    }
    pub fn digit_count(self) -> usize {
        usize::from(self.digits)
    }
    pub fn digit(self, index: usize) -> Option<u8> {
        if index >= self.digit_count() {
            return None;
        }
        Some((self.bytes[index / 2] >> ((index % 2) as u32 * NIBBLE_SHIFT)) & NIBBLE_MASK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_lengths_have_one_canonical_bcd_and_digit_representation() {
        for count in MIN_IMSI_DIGITS..=MAX_IMSI_DIGITS {
            let digits: Vec<u8> = (0..count).map(|index| (index % 10) as u8).collect();
            let text: String = digits
                .iter()
                .map(|digit| char::from(b'0' + digit))
                .collect();
            let imsi = Imsi::from_digit_values(&digits).unwrap();
            assert_eq!(imsi, Imsi::from_digits(&text).unwrap());
            assert_eq!(imsi, Imsi::from_bcd(imsi.bcd()).unwrap());
            assert_eq!(
                (0..count)
                    .map(|index| imsi.digit(index).unwrap())
                    .collect::<Vec<_>>(),
                digits
            );
            assert_eq!(imsi.digit(count), None);
        }
        assert_eq!(
            Imsi::from_digits("001010000000099").unwrap().bcd(),
            &[0, 1, 1, 0, 0, 0, 0x90, 0xf9]
        );
    }
    #[test]
    fn malformed_digits_filler_and_lengths_are_rejected() {
        for bytes in [
            &[0x0a, 0, 0][..],
            &[0xf0, 0, 0],
            &[0, 0, 0x10, 0, 0, 0, 0, 0],
            &[0xff; 8],
            &[0; 9],
        ] {
            assert!(Imsi::from_bcd(bytes).is_err());
        }
        for text in ["", "1234", "1234567890123456", "1234x", "１２３４５"] {
            assert!(Imsi::from_digits(text).is_err());
        }
        assert!(Imsi::from_digit_values(&[1, 2, 3, 4, 10]).is_err());
        assert_eq!(
            format!("{:?}", Imsi::from_digits("001010000000099").unwrap()),
            "Imsi([redacted])"
        );
    }
}
