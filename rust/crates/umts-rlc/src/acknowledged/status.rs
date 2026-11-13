// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AmError, MAX_PDU_BYTES, SN_BITS, SN_MASK, SN_MODULUS};
use umts_types::bits::{Bits, write_u32};

pub(super) const CONTROL_HEADER_BITS: usize = 4;
pub(super) const ACK_BITS: usize = SUFI_BITS + SN_BITS;
const LIST_HEADER_BITS: usize = SUFI_BITS + LIST_COUNT_BITS;
const LIST_PAIR_BITS: usize = SN_BITS + LIST_COUNT_BITS;
pub(super) const SINGLE_MISSING_BITS: usize = LIST_HEADER_BITS + LIST_PAIR_BITS;
const MAX_LIST_PAIRS: usize = (1 << LIST_COUNT_BITS) - 1;
pub(super) const MAX_MISSING_RUN: u8 = 1 << LIST_COUNT_BITS;
const SUFI_BITS: usize = 4;
pub(super) const MAX_SUFIS: usize = 128;
const NO_MORE: u32 = 0;
const WINDOW: u32 = 1;
const ACK: u32 = 2;
const LIST: u32 = 3;
const BITMAP: u32 = 4;
const RLIST: u32 = 5;
const MRW: u32 = 6;
const MRW_ACK: u32 = 7;
const POLL: u32 = 8;
const LIST_COUNT_BITS: usize = 4;
const CODEWORD_BITS: usize = 4;
const CODEWORD_DATA_BITS: usize = 3;
const WORD_BITS: usize = 64;
const SET_WORDS: usize = SN_MODULUS as usize / WORD_BITS;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnSet([u64; SET_WORDS]);
impl Default for SnSet {
    fn default() -> Self {
        Self([0; SET_WORDS])
    }
}
impl SnSet {
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.0.iter().enumerate().flat_map(|(index, &word)| {
            let mut word = word;
            std::iter::from_fn(move || {
                if word == 0 {
                    return None;
                }
                let bit = word.trailing_zeros();
                word &= word - 1;
                Some((index * WORD_BITS + bit as usize) as u16)
            })
        })
    }
    pub fn contains(&self, sequence: u16) -> bool {
        sequence <= SN_MASK
            && self.0[usize::from(sequence) / WORD_BITS]
                & (1 << (usize::from(sequence) % WORD_BITS))
                != 0
    }
    pub fn insert(&mut self, sequence: u16) -> Result<(), AmError> {
        if sequence > SN_MASK {
            return Err(AmError::Header);
        }
        self.0[usize::from(sequence) / WORD_BITS] |= 1 << (usize::from(sequence) % WORD_BITS);
        Ok(())
    }
    fn intersects(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0)
            .any(|(left, right)| left & right != 0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub negative: SnSet,
    pub positive: SnSet,
    pub acknowledge: Option<u16>,
    pub window: Option<u16>,
    pub poll: Option<u16>,
    /// TS 25.322 9.2.2.11.6: the last SN_MRW and NLENGTH of a Move Receiving Window request.
    pub move_window: Option<(u16, u8)>,
    /// TS 25.322 9.2.2.11.7: N and SN_ACK of a received MRW_ACK.
    pub move_window_ack: Option<(u8, u16)>,
}

struct Reader<'a> {
    input: Bits<'a>,
    position: usize,
}
impl Reader<'_> {
    fn read(&mut self, width: usize) -> Result<u32, AmError> {
        let value = self
            .input
            .read_u32(self.position, width)
            .map_err(|_| AmError::Header)?;
        self.position += width;
        Ok(value)
    }
    fn sequence(&mut self) -> Result<u16, AmError> {
        Ok(self.read(SN_BITS)? as u16)
    }
}

// TS 25.322 9.2.2.11 terminates STATUS with ACK or NO_MORE and ignores all following padding.
pub fn parse_status(input: &[u8]) -> Result<Status, AmError> {
    if input.is_empty() || input.len() > MAX_PDU_BYTES {
        return Err(AmError::Length);
    }
    let mut reader = Reader {
        input: Bits::new(input, input.len() * 8).map_err(|_| AmError::Length)?,
        position: 0,
    };
    if reader.read(CONTROL_HEADER_BITS)? != 0 {
        return Err(AmError::Header);
    }
    let mut status = Status::default();
    let mut terminated = false;
    for _ in 0..MAX_SUFIS {
        match reader.read(SUFI_BITS)? {
            NO_MORE => {
                terminated = true;
                break;
            }
            ACK => {
                status.acknowledge = Some(reader.sequence()?);
                terminated = true;
                break;
            }
            WINDOW => {
                let window = reader.sequence()?;
                if window != 0 {
                    status.window = Some(window);
                }
            }
            LIST => {
                let pairs = reader.read(LIST_COUNT_BITS)?;
                if pairs == 0 {
                    return Err(AmError::Header);
                }
                for _ in 0..pairs {
                    let first = reader.sequence()?;
                    let following = reader.read(LIST_COUNT_BITS)? as u16;
                    for offset in 0..=following {
                        status.negative.insert((first + offset) & SN_MASK)?;
                    }
                }
            }
            BITMAP => {
                let count = (reader.read(LIST_COUNT_BITS)? as usize + 1) * 8;
                let first = reader.sequence()?;
                for offset in 0..count {
                    let set = if reader.read(1)? == 0 {
                        &mut status.negative
                    } else {
                        &mut status.positive
                    };
                    set.insert((first + offset as u16) & SN_MASK)?;
                }
            }
            RLIST => {
                let count = reader.read(LIST_COUNT_BITS)?;
                let mut previous = reader.sequence()?;
                status.negative.insert(previous)?;
                let mut number = 0u32;
                let mut shift = 0;
                let mut burst = false;
                for index in 0..count {
                    let word = reader.read(CODEWORD_BITS)?;
                    number |= (word >> 1).checked_shl(shift).ok_or(AmError::Header)?;
                    if number > u32::from(SN_MASK) {
                        return Err(AmError::Header);
                    }
                    if word & 1 == 0 {
                        shift += CODEWORD_DATA_BITS as u32;
                        if index + 1 == count {
                            return Err(AmError::Header);
                        }
                        continue;
                    }
                    if number == 0 {
                        if burst || shift != 0 || index + 1 == count {
                            return Err(AmError::Header);
                        }
                        burst = true;
                    } else {
                        if burst {
                            for offset in 1..=number {
                                status
                                    .negative
                                    .insert((previous + offset as u16) & SN_MASK)?;
                            }
                        }
                        previous = (previous + number as u16) & SN_MASK;
                        status.negative.insert(previous)?;
                        burst = false;
                    }
                    number = 0;
                    shift = 0;
                }
            }
            POLL => {
                status.poll = Some(reader.sequence()?);
            }
            MRW => {
                let length = reader.read(LIST_COUNT_BITS)?;
                if length == 0 {
                    return Err(AmError::Header);
                }
                let mut last = 0;
                for _ in 0..length {
                    last = reader.sequence()?;
                }
                let n_length = reader.read(LIST_COUNT_BITS)?;
                status.move_window = Some((last, n_length as u8));
            }
            MRW_ACK => {
                let n = reader.read(LIST_COUNT_BITS)?;
                let sequence = reader.sequence()?;
                status.move_window_ack = Some((n as u8, sequence));
            }
            _ => return Err(AmError::Header),
        }
    }
    if !terminated {
        return Err(AmError::Capacity);
    }
    if status.negative.intersects(&status.positive) {
        return Err(AmError::InconsistentStatus);
    }
    Ok(status)
}

pub(super) struct Writer<'a> {
    bytes: &'a mut [u8],
    position: usize,
    list: Option<(usize, usize)>,
    fields: usize,
}
impl<'a> Writer<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self {
            bytes,
            position: CONTROL_HEADER_BITS,
            list: None,
            fields: 0,
        }
    }

    pub fn can_missing(&self) -> bool {
        let new_list = self.list.is_none_or(|(_, pairs)| pairs == MAX_LIST_PAIRS);
        self.fields + usize::from(new_list) < MAX_SUFIS
            && self.position + LIST_PAIR_BITS + usize::from(new_list) * LIST_HEADER_BITS + ACK_BITS
                <= self.bytes.len() * 8
    }
    pub fn write(&mut self, width: usize, value: u32) -> Result<(), AmError> {
        write_u32(self.bytes, self.position, width, value).map_err(|_| AmError::Capacity)?;
        self.position += width;
        Ok(())
    }
    pub fn ack(&mut self, sequence: u16) -> Result<(), AmError> {
        self.write(SUFI_BITS, ACK)?;
        self.write(SN_BITS, u32::from(sequence))
    }
    // TS 25.322 9.2.2.11.7: MRW_ACK precedes the terminating ACK.
    pub fn mrw_ack(&mut self, n: u8, sequence: u16) -> Result<(), AmError> {
        if sequence > SN_MASK || n > 0xf || self.fields + 1 >= MAX_SUFIS {
            return Err(AmError::Header);
        }
        let position = self.position;
        let result = self
            .write(SUFI_BITS, MRW_ACK)
            .and_then(|()| self.write(LIST_COUNT_BITS, u32::from(n)))
            .and_then(|()| self.write(SN_BITS, u32::from(sequence)));
        if result.is_err() {
            self.position = position;
            return result;
        }
        self.list = None;
        self.fields += 1;
        Ok(())
    }
    // TS 25.322 9.2.2.11.4 counts following missing PDUs, not the entire run.
    pub fn missing_run(&mut self, sequence: u16, count: u8) -> Result<(), AmError> {
        if sequence > SN_MASK || !(1..=MAX_MISSING_RUN).contains(&count) {
            return Err(AmError::Header);
        }
        if !self.can_missing() {
            return Err(AmError::Capacity);
        }
        if self.list.is_none_or(|(_, pairs)| pairs == MAX_LIST_PAIRS) {
            self.write(SUFI_BITS, LIST)?;
            let length_position = self.position;
            self.write(LIST_COUNT_BITS, 0)?;
            self.list = Some((length_position, 0));
            self.fields += 1;
        }
        self.write(SN_BITS, u32::from(sequence))?;
        self.write(LIST_COUNT_BITS, u32::from(count - 1))?;
        let (position, pairs) = self.list.expect("open LIST");
        write_u32(self.bytes, position, LIST_COUNT_BITS, (pairs + 1) as u32)
            .map_err(|_| AmError::Capacity)?;
        self.list = Some((position, pairs + 1));
        Ok(())
    }
}
