// SPDX-License-Identifier: AGPL-3.0-or-later

#[derive(Default)]
pub(crate) struct BitWriter {
    pub(crate) bytes: Vec<u8>,
    pub(crate) length: usize,
}

impl BitWriter {
    pub(crate) fn flag(&mut self, value: bool) {
        self.write(u64::from(value), 1);
    }

    pub(crate) fn write(&mut self, value: u64, width: usize) {
        for position in (0..width).rev() {
            if self.length.is_multiple_of(8) {
                self.bytes.push(0);
            }
            self.bytes[self.length / 8] |=
                (((value >> position) & 1) as u8) << (7 - self.length % 8);
            self.length += 1;
        }
    }
}
