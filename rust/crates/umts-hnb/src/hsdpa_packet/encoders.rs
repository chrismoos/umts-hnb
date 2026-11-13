// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub struct EncoderBank {
    entries: Box<[PacketEncoder]>,
    maximum_coded_bits: NonZeroUsize,
    retained_bytes: usize,
}

impl EncoderBank {
    pub fn new(
        formats: &[NonZeroUsize],
        maximum_formats: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
        method: CrcMethod,
        maximum_bytes: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if formats.is_empty() || formats.len() > maximum_formats.get() {
            return Err(DspError::Capacity);
        }
        let mut sizes = formats.to_vec();
        sizes.sort_unstable();
        sizes.dedup();
        let mut retained_bytes = std::mem::size_of::<Self>();
        for &bits in &sizes {
            retained_bytes = retained_bytes
                .checked_add(PacketEncoder::required_storage_bytes(
                    bits,
                    maximum_coded_bits,
                )?)
                .ok_or(DspError::Capacity)?;
            if retained_bytes > maximum_bytes.get() {
                return Err(DspError::Capacity);
            }
        }
        let entries = sizes
            .into_iter()
            .map(|bits| PacketEncoder::new(bits, maximum_coded_bits, method))
            .collect::<Result<Box<[_]>, _>>()?;
        let maximum_coded_bits = entries
            .iter()
            .map(PacketEncoder::coded_bits)
            .max()
            .and_then(NonZeroUsize::new)
            .ok_or(DspError::InvalidLength)?;
        Ok(Self {
            entries,
            maximum_coded_bits,
            retained_bytes,
        })
    }

    pub fn get_mut(&mut self, bits: NonZeroUsize) -> Option<&mut PacketEncoder> {
        let index = self
            .entries
            .binary_search_by_key(&bits, PacketEncoder::transport_bits)
            .ok()?;
        self.entries.get_mut(index)
    }

    pub fn buffer(&self) -> BlockBuffer {
        BlockBuffer::new(self.maximum_coded_bits)
    }

    pub fn maximum_coded_bits(&self) -> NonZeroUsize {
        self.maximum_coded_bits
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::count;

    #[test]
    fn admission_deduplicates_sizes_and_reuses_encoder_scratch() {
        let mut bank = EncoderBank::new(
            &[count(461), count(137), count(461)],
            count(3),
            count(100_000),
            CrcMethod::Method1,
            count(1_000_000),
        )
        .unwrap();
        assert_eq!(bank.entries.len(), 2);
        let bytes = bank.retained_bytes();
        assert!(
            EncoderBank::new(
                &[count(137), count(461)],
                count(2),
                count(100_000),
                CrcMethod::Method1,
                count(bytes - 1),
            )
            .is_err()
        );
        assert_eq!(
            EncoderBank::new(
                &[count(137), count(461)],
                count(2),
                count(100_000),
                CrcMethod::Method1,
                count(bytes),
            )
            .unwrap()
            .retained_bytes(),
            bytes
        );
        let scratch = bank.get_mut(count(461)).unwrap().packed.as_ptr();
        let capacity = bank.maximum_coded_bits();
        assert_eq!(bank.buffer().capacity(), capacity.get());
        assert!(bank.get_mut(count(317)).is_none());
        assert!(bank.get_mut(count(137)).unwrap().coded_bits() < capacity.get());
        assert_eq!(bank.get_mut(count(461)).unwrap().packed.as_ptr(), scratch);
        assert!(
            EncoderBank::new(
                &[count(137), count(461)],
                count(1),
                count(100_000),
                CrcMethod::Method1,
                count(1_000_000),
            )
            .is_err()
        );
    }
}
