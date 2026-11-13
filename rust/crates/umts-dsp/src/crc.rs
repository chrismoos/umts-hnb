// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DspError, bits::Bit};

const BYTE_BITS: usize = u8::BITS as usize;
const TABLE_ENTRIES: usize = 1 << BYTE_BITS;

const fn byte_table(width: usize, polynomial: u32) -> [u32; TABLE_ENTRIES] {
    let mut table = [0; TABLE_ENTRIES];
    let mask = (1_u32 << width) - 1;
    let mut value = 0;
    while value < TABLE_ENTRIES {
        let mut register = (value as u32) << (width - BYTE_BITS);
        let mut bit = 0;
        while bit < BYTE_BITS {
            let feedback = (register >> (width - 1)) & 1;
            register = ((register << 1) & mask) ^ (polynomial & 0_u32.wrapping_sub(feedback));
            bit += 1;
        }
        table[value] = register;
        value += 1;
    }
    table
}

#[derive(Clone, Copy, Debug)]
pub enum TransportCrc {
    None,
    Crc8,
    Crc12,
    Crc16,
    Crc24,
}

impl TransportCrc {
    pub const fn width(self) -> usize {
        match self {
            Self::None => 0,
            Self::Crc8 => 8,
            Self::Crc12 => 12,
            Self::Crc16 => 16,
            Self::Crc24 => 24,
        }
    }

    const fn polynomial(self) -> u32 {
        // TS 25.212 4.2.1.1, leading coefficient omitted.
        match self {
            Self::None => 0,
            Self::Crc8 => 0x9b,
            Self::Crc12 => 0x80f,
            Self::Crc16 => 0x1021,
            Self::Crc24 => 0x80_0063,
        }
    }

    fn table(self) -> &'static [u32; TABLE_ENTRIES] {
        const CRC8: [u32; TABLE_ENTRIES] =
            byte_table(TransportCrc::Crc8.width(), TransportCrc::Crc8.polynomial());
        const CRC12: [u32; TABLE_ENTRIES] = byte_table(
            TransportCrc::Crc12.width(),
            TransportCrc::Crc12.polynomial(),
        );
        const CRC16: [u32; TABLE_ENTRIES] = byte_table(
            TransportCrc::Crc16.width(),
            TransportCrc::Crc16.polynomial(),
        );
        const CRC24: [u32; TABLE_ENTRIES] = byte_table(
            TransportCrc::Crc24.width(),
            TransportCrc::Crc24.polynomial(),
        );
        match self {
            Self::None => unreachable!("zero-width CRC returns before lookup"),
            Self::Crc8 => &CRC8,
            Self::Crc12 => &CRC12,
            Self::Crc16 => &CRC16,
            Self::Crc24 => &CRC24,
        }
    }

    /// CRC over MSB-first bytes. Unused low bits in the final byte are ignored.
    pub fn remainder_bytes(self, bytes: &[u8], bits: usize) -> Result<u32, DspError> {
        if bytes.len() != bits.div_ceil(BYTE_BITS) {
            return Err(DspError::InvalidLength);
        }
        let width = self.width();
        if width == 0 {
            return Ok(0);
        }
        let mask = (1_u32 << width) - 1;
        let mut register = 0;
        let table = self.table();
        for &byte in &bytes[..bits / BYTE_BITS] {
            let index = ((register >> (width - BYTE_BITS)) as u8) ^ byte;
            register = ((register << BYTE_BITS) & mask) ^ table[usize::from(index)];
        }
        if !bits.is_multiple_of(BYTE_BITS) {
            let byte = bytes[bits / BYTE_BITS];
            for bit in 0..bits % BYTE_BITS {
                let value = u32::from((byte >> (BYTE_BITS - 1 - bit)) & 1);
                let feedback = ((register >> (width - 1)) ^ value) & 1;
                register = (register << 1) & mask;
                register ^= self.polynomial() & 0_u32.wrapping_sub(feedback);
            }
        }
        Ok(register)
    }

    pub fn remainder(self, payload: &[Bit]) -> u32 {
        self.extend_remainder(0, payload)
    }

    pub(crate) fn extend_remainder(self, mut register: u32, payload: &[Bit]) -> u32 {
        let width = self.width();
        if width == 0 {
            return 0;
        }
        let polynomial = self.polynomial();
        let mask = (1_u32 << width) - 1;
        let mut bytes = payload.chunks_exact(BYTE_BITS);
        let table = self.table();
        for bits in bytes.by_ref() {
            let byte = bits.iter().fold(0_u8, |byte, &bit| (byte << 1) | bit as u8);
            let index = ((register >> (width - BYTE_BITS)) as u8) ^ byte;
            register = ((register << BYTE_BITS) & mask) ^ table[usize::from(index)];
        }
        for &bit in bytes.remainder() {
            let feedback = ((register >> (width - 1)) ^ bit as u32) & 1;
            register = (register << 1) & mask;
            register ^= polynomial & 0_u32.wrapping_sub(feedback);
        }
        register
    }

    pub fn write(self, payload: &[Bit], parity: &mut [Bit]) -> Result<(), DspError> {
        if parity.len() != self.width() {
            return Err(DspError::InvalidLength);
        }
        let remainder = self.remainder(payload);
        // TS 25.212 4.2.1.2 transmits p_L first, not the highest polynomial coefficient.
        for (index, bit) in parity.iter_mut().enumerate() {
            *bit = Bit::from(remainder & (1 << index) != 0);
        }
        Ok(())
    }

    pub fn verify(self, payload: &[Bit], parity: &[Bit]) -> Result<bool, DspError> {
        if parity.len() != self.width() {
            return Err(DspError::InvalidLength);
        }
        let remainder = self.remainder(payload);
        Ok(parity
            .iter()
            .enumerate()
            .all(|(i, &bit)| bit as u32 == (remainder >> i) & 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_bytes_match_bit_crc_for_every_tail_and_all_polynomials() {
        let bytes: Vec<u8> = (0..40).map(|i| (i * 73 + 0x9d) as u8).collect();
        for crc in [
            TransportCrc::None,
            TransportCrc::Crc8,
            TransportCrc::Crc12,
            TransportCrc::Crc16,
            TransportCrc::Crc24,
        ] {
            for length in 0..=bytes.len() * 8 {
                let bits: Vec<_> = (0..length)
                    .map(|i| Bit::from(bytes[i / 8] & (1 << (7 - i % 8)) != 0))
                    .collect();
                assert_eq!(
                    crc.remainder_bytes(&bytes[..length.div_ceil(8)], length)
                        .unwrap(),
                    crc.remainder(&bits)
                );
                if !length.is_multiple_of(8) {
                    let mut changed = bytes[..length.div_ceil(8)].to_vec();
                    *changed.last_mut().unwrap() ^= (1 << (8 - length % 8)) - 1;
                    assert_eq!(
                        crc.remainder_bytes(&changed, length).unwrap(),
                        crc.remainder(&bits)
                    );
                }
            }
            assert_eq!(crc.remainder_bytes(&[], 1), Err(DspError::InvalidLength));
            assert_eq!(crc.remainder_bytes(&[0], 0), Err(DspError::InvalidLength));
        }
    }

    #[test]
    fn byte_updates_match_bit_recurrence_at_every_partial_byte_boundary() {
        for crc in [
            TransportCrc::Crc8,
            TransportCrc::Crc12,
            TransportCrc::Crc16,
            TransportCrc::Crc24,
        ] {
            for length in 0..=135 {
                let payload: Vec<_> = (0..length)
                    .map(|i| Bit::from((i * 17 + i / 3) % 5 < 2))
                    .collect();
                for initial in [0, 1, 0xdead_beef] {
                    let expected = payload.iter().fold(initial, |register, &bit| {
                        let feedback = ((register >> (crc.width() - 1)) ^ bit as u32) & 1;
                        ((register << 1) & ((1 << crc.width()) - 1))
                            ^ (crc.polynomial() & 0_u32.wrapping_sub(feedback))
                    });
                    assert_eq!(crc.extend_remainder(initial, &payload), expected);
                    for chunk in 1..=17 {
                        let actual = payload.chunks(chunk).fold(initial, |register, bits| {
                            crc.extend_remainder(register, bits)
                        });
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
        assert_eq!(
            TransportCrc::None.extend_remainder(u32::MAX, &[Bit::One; 19]),
            0
        );
    }

    fn long_division(payload: &[Bit], crc: TransportCrc) -> u32 {
        let width = crc.width();
        let full = crc.polynomial() | (1 << width);
        let mut dividend: Vec<_> = payload.iter().map(|bit| *bit as u8).collect();
        dividend.resize(payload.len() + width, 0);
        for i in 0..payload.len() {
            if dividend[i] == 0 {
                continue;
            }
            for j in 0..=width {
                dividend[i + j] ^= ((full >> (width - j)) & 1) as u8;
            }
        }
        dividend[payload.len()..]
            .iter()
            .fold(0, |value, &bit| (value << 1) | u32::from(bit))
    }

    #[test]
    fn independently_divided_polynomials_and_wire_parity_order() {
        for crc in [
            TransportCrc::Crc8,
            TransportCrc::Crc12,
            TransportCrc::Crc16,
            TransportCrc::Crc24,
        ] {
            for length in [0, 1, 8, 39, 81, 244, 5114] {
                let payload: Vec<_> = (0..length)
                    .map(|i| Bit::from((i * 17 + i / 3) % 5 < 2))
                    .collect();
                assert_eq!(crc.remainder(&payload), long_division(&payload, crc));
                let mut parity = vec![Bit::Zero; crc.width()];
                crc.write(&payload, &mut parity).unwrap();
                assert!(crc.verify(&payload, &parity).unwrap());
                parity[0] = Bit::from(parity[0] == Bit::Zero);
                assert!(!crc.verify(&payload, &parity).unwrap());
            }
        }
        let mut parity = [Bit::Zero; 8];
        TransportCrc::Crc8.write(&[Bit::One], &mut parity).unwrap();
        assert_eq!(
            parity,
            [
                Bit::One,
                Bit::One,
                Bit::Zero,
                Bit::One,
                Bit::One,
                Bit::Zero,
                Bit::Zero,
                Bit::One
            ]
        );
        assert!(TransportCrc::None.verify(&[Bit::One], &[]).unwrap());
    }

    #[test]
    fn zero_parity_is_valid_for_nonzero_generator_payloads() {
        for crc in [
            TransportCrc::Crc8,
            TransportCrc::Crc12,
            TransportCrc::Crc16,
            TransportCrc::Crc24,
        ] {
            let full = (1 << crc.width()) | crc.polynomial();
            let payload: Vec<_> = (0..=crc.width())
                .rev()
                .map(|i| Bit::from(full & (1 << i) != 0))
                .collect();
            assert_eq!(crc.remainder(&payload), 0);
            assert!(crc.verify(&payload, &vec![Bit::Zero; crc.width()]).unwrap());
        }
    }
}
