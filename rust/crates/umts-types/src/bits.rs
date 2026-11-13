// SPDX-License-Identifier: AGPL-3.0-or-later

use std::ops::Range;

pub const BITS_PER_BYTE: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitError {
    Bounds,
    Width,
}

#[derive(Clone, Copy, Debug)]
pub struct Bits<'a> {
    bytes: &'a [u8],
    offset: usize,
    length: usize,
}

impl<'a> Bits<'a> {
    pub const fn empty() -> Self {
        Self {
            bytes: &[],
            offset: 0,
            length: 0,
        }
    }

    pub fn new(bytes: &'a [u8], length: usize) -> Result<Self, BitError> {
        if length.div_ceil(BITS_PER_BYTE) > bytes.len() {
            return Err(BitError::Bounds);
        }
        Ok(Self {
            bytes,
            offset: 0,
            length,
        })
    }

    pub fn len(self) -> usize {
        self.length
    }
    pub fn is_empty(self) -> bool {
        self.length == 0
    }

    pub fn subspan(self, range: Range<usize>) -> Result<Self, BitError> {
        if range.start > range.end || range.end > self.length {
            return Err(BitError::Bounds);
        }
        Ok(Self {
            offset: self.offset + range.start,
            length: range.len(),
            ..self
        })
    }

    pub fn bit(self, index: usize) -> Option<bool> {
        if index >= self.length {
            return None;
        }
        let index = self.offset + index;
        Some(
            self.bytes[index / BITS_PER_BYTE] & (1 << (BITS_PER_BYTE - 1 - index % BITS_PER_BYTE))
                != 0,
        )
    }

    pub fn read_u32(self, offset: usize, width: usize) -> Result<u32, BitError> {
        if width > u32::BITS as usize {
            return Err(BitError::Width);
        }
        if offset
            .checked_add(width)
            .is_none_or(|end| end > self.length)
        {
            return Err(BitError::Bounds);
        }
        Ok((offset..offset + width).fold(0, |word, bit| {
            (word << 1) | u32::from(self.bit(bit).expect("checked bit range"))
        }))
    }

    pub fn copy_into(self, output: &mut [u8], offset: usize) -> Result<(), BitError> {
        let end = offset.checked_add(self.length).ok_or(BitError::Bounds)?;
        if end.div_ceil(BITS_PER_BYTE) > output.len() {
            return Err(BitError::Bounds);
        }
        let prefix = ((BITS_PER_BYTE - offset % BITS_PER_BYTE) % BITS_PER_BYTE).min(self.length);
        for bit in 0..prefix {
            set_bit(
                output,
                offset + bit,
                self.bit(bit).expect("checked bit range"),
            );
        }
        let bytes = (self.length - prefix) / BITS_PER_BYTE;
        let source = self.offset + prefix;
        let destination = (offset + prefix) / BITS_PER_BYTE;
        let shift = source % BITS_PER_BYTE;
        let source_byte = source / BITS_PER_BYTE;
        let body = &mut output[destination..destination + bytes];
        if shift == 0 {
            body.copy_from_slice(&self.bytes[source_byte..source_byte + bytes]);
        } else {
            for (index, byte) in body.iter_mut().enumerate() {
                *byte = (self.bytes[source_byte + index] << shift)
                    | (self.bytes[source_byte + index + 1] >> (BITS_PER_BYTE - shift));
            }
        }
        for bit in prefix + bytes * BITS_PER_BYTE..self.length {
            set_bit(
                output,
                offset + bit,
                self.bit(bit).expect("checked bit range"),
            );
        }
        Ok(())
    }

    pub fn copy_to(self, output: &mut [u8]) -> Result<(), BitError> {
        if output.len() != self.length.div_ceil(BITS_PER_BYTE) {
            return Err(BitError::Bounds);
        }
        output.fill(0);
        self.copy_into(output, 0)
    }
}

fn set_bit(output: &mut [u8], index: usize, value: bool) {
    let mask = 1 << (BITS_PER_BYTE - 1 - index % BITS_PER_BYTE);
    let byte = &mut output[index / BITS_PER_BYTE];
    *byte = (*byte & !mask) | if value { mask } else { 0 };
}

pub fn write_u32(
    output: &mut [u8],
    offset: usize,
    width: usize,
    value: u32,
) -> Result<(), BitError> {
    if width > u32::BITS as usize || (width < u32::BITS as usize && value >> width != 0) {
        return Err(BitError::Width);
    }
    if offset
        .checked_add(width)
        .is_none_or(|end| end.div_ceil(BITS_PER_BYTE) > output.len())
    {
        return Err(BitError::Bounds);
    }
    for bit in 0..width {
        set_bit(output, offset + bit, value & (1 << (width - 1 - bit)) != 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_copy_matches_bit_reference_for_all_alignments_and_tails() {
        let input = std::array::from_fn::<_, 40, _>(|index| (index * 73 + 19) as u8);
        for source in 0..16 {
            for destination in 0..16 {
                for length in 0..257 {
                    let bits = Bits::new(&input, input.len() * BITS_PER_BYTE)
                        .unwrap()
                        .subspan(source..source + length)
                        .unwrap();
                    let mut actual = [0xa5; 40];
                    let mut expected = actual;
                    for bit in 0..length {
                        set_bit(&mut expected, destination + bit, bits.bit(bit).unwrap());
                    }
                    bits.copy_into(&mut actual, destination).unwrap();
                    assert_eq!(
                        actual, expected,
                        "source={source} destination={destination} length={length}"
                    );
                }
            }
        }
    }

    #[test]
    fn nested_non_octet_views_preserve_exact_bit_order_and_length() {
        let input = [0xb3, 0x5a, 0xe0];
        let bits = Bits::new(&input, 19)
            .unwrap()
            .subspan(3..18)
            .unwrap()
            .subspan(2..13)
            .unwrap();
        assert_eq!(bits.len(), 11);
        assert_eq!(bits.read_u32(0, 11), Ok(0b01101011010));
        let mut output = [0xff; 2];
        bits.copy_to(&mut output).unwrap();
        assert_eq!(output, [0x6b, 0x40]);
        assert_eq!(bits.bit(11), None);
        assert!(Bits::new(&input, 25).is_err());
        assert!(bits.subspan(0..12).is_err());
    }

    #[test]
    fn copies_and_words_preserve_surrounding_bits_at_every_bit_offset() {
        for offset in 0..16 {
            let mut output = [0xa5; 8];
            let before = output;
            write_u32(&mut output, offset, 32, 0x8123_4567).unwrap();
            let bits = Bits::new(&output, 64).unwrap();
            assert_eq!(bits.read_u32(offset, 32), Ok(0x8123_4567));
            for bit in (0..offset).chain(offset + 32..64) {
                assert_eq!(bits.bit(bit), Bits::new(&before, 64).unwrap().bit(bit));
            }
            let mut copy = [0xff; 5];
            bits.subspan(offset..offset + 32)
                .unwrap()
                .copy_into(&mut copy, 3)
                .unwrap();
            assert_eq!(
                Bits::new(&copy, 40).unwrap().read_u32(3, 32),
                Ok(0x8123_4567)
            );
        }
    }

    #[test]
    fn invalid_writes_are_atomic_and_zero_width_is_defined() {
        let mut bytes = [0xa5; 2];
        for (offset, width, value) in [(0, 33, 0), (0, 3, 8), (15, 2, 0), (usize::MAX, 2, 0)] {
            assert!(write_u32(&mut bytes, offset, width, value).is_err());
            assert_eq!(bytes, [0xa5; 2]);
        }
        write_u32(&mut bytes, 16, 0, 0).unwrap();
        Bits::empty().copy_into(&mut bytes, 16).unwrap();
        assert_eq!(bytes, [0xa5; 2]);
    }
}
