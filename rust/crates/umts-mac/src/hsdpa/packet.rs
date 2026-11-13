// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::MacError;
use std::num::NonZeroUsize;
use umts_types::bits::{Bits, write_u32};

const QUEUES: u8 = 8;
const SIZE_INDICES: usize = 8;
const TSN_MODULUS: u8 = 64;
// TS 25.321 9.2.2.1 allows 127 MAC-d PDUs per SID run, but some UEs drop RLC PDUs from
// MAC-hs PDUs with more than 70 MAC-d PDUs.
const MAX_PDUS: usize = 70;
const MAX_RUNS: usize = 8;
const FIXED_HEADER_BITS: usize = 10;
const RUN_HEADER_BITS: usize = 11;
const QUEUE_OFFSET: usize = 1;
const QUEUE_BITS: usize = 3;
const TSN_OFFSET: usize = 4;
const TSN_BITS: usize = 6;
const SID_BITS: usize = 3;
const COUNT_BITS: usize = 7;

mod queue;
#[cfg(test)]
mod tests;
pub use queue::{PacketQueue, QueuePublishError};

pub struct QueueFormat {
    queue: u8,
    sizes: [Option<NonZeroUsize>; SIZE_INDICES],
    logical_channel: Option<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct MacdPdu<'a> {
    pub size_index: u8,
    pub bits: Bits<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackedPdu {
    pub queue: u8,
    pub sequence: u8,
    pub transport_bits: usize,
    pub header_bits: usize,
    pub payload_bits: usize,
    pub pdus: usize,
    pub end_fragment_octets: usize,
}

pub struct DecodedPdu<'a> {
    pub sequence: u8,
    pdus: [Option<Bits<'a>>; MAX_PDUS],
    count: usize,
}

impl<'a> DecodedPdu<'a> {
    pub fn pdus(&self) -> impl Iterator<Item = Bits<'a>> + '_ {
        self.pdus[..self.count].iter().filter_map(|value| *value)
    }
}

#[derive(Clone, Copy, Default)]
struct Run {
    sid: u8,
    count: usize,
    size: usize,
}

impl QueueFormat {
    pub fn new(queue: u8, sizes: [Option<NonZeroUsize>; SIZE_INDICES]) -> Result<Self, MacError> {
        if queue >= QUEUES || sizes.iter().all(Option::is_none) {
            return Err(MacError::Mapping);
        }
        Ok(Self {
            queue,
            sizes,
            logical_channel: None,
        })
    }

    pub fn ehs(
        queue: u8,
        logical_channel: u8,
        sizes: [Option<NonZeroUsize>; SIZE_INDICES],
    ) -> Result<Self, MacError> {
        let mut format = Self::new(queue, sizes)?;
        if !(1..=15).contains(&logical_channel)
            || sizes
                .iter()
                .flatten()
                .any(|size| !size.get().is_multiple_of(8))
        {
            return Err(MacError::Mapping);
        }
        format.logical_channel = Some(logical_channel);
        Ok(format)
    }

    pub fn unpack<'a>(&self, input: Bits<'a>) -> Result<DecodedPdu<'a>, MacError> {
        if self.logical_channel.is_some() {
            return Err(MacError::Mapping);
        }
        if input.read_u32(0, 1)? != 0 {
            return Err(MacError::Reserved);
        }
        if input.read_u32(QUEUE_OFFSET, QUEUE_BITS)? != u32::from(self.queue) {
            return Err(MacError::Mapping);
        }
        let sequence = input.read_u32(TSN_OFFSET, TSN_BITS)? as u8;
        let mut runs = [Run::default(); MAX_RUNS];
        let mut run_count = 0;
        let mut count = 0;
        let mut header_bits = FIXED_HEADER_BITS;
        loop {
            if run_count == MAX_RUNS {
                return Err(MacError::Capacity);
            }
            let sid = input.read_u32(header_bits, SID_BITS)? as usize;
            let number = input.read_u32(header_bits + SID_BITS, COUNT_BITS)? as usize;
            let final_run = input.read_u32(header_bits + SID_BITS + COUNT_BITS, 1)? != 0;
            let size = self.sizes[sid].ok_or(MacError::Mapping)?.get();
            if number == 0
                || count + number > MAX_PDUS
                || (run_count > 0 && runs[run_count - 1].size == size)
            {
                return Err(MacError::Mapping);
            }
            runs[run_count] = Run {
                sid: sid as u8,
                count: number,
                size,
            };
            run_count += 1;
            count += number;
            header_bits += RUN_HEADER_BITS;
            if final_run {
                break;
            }
        }
        let mut result = DecodedPdu {
            sequence,
            pdus: [None; MAX_PDUS],
            count,
        };
        let mut offset = header_bits;
        let mut destination = result.pdus.iter_mut();
        for run in &runs[..run_count] {
            for _ in 0..run.count {
                let end = offset.checked_add(run.size).ok_or(MacError::Capacity)?;
                *destination.next().expect("validated MAC-hs PDU count") =
                    Some(input.subspan(offset..end)?);
                offset = end;
            }
        }
        Ok(result)
    }

    // TS 25.321 9.1.4 and 9.2.2: one queue, at most 70 SDUs and eight size runs.
    pub fn pack(
        &self,
        tsn: u8,
        pdus: &[MacdPdu<'_>],
        transport_bits: usize,
        output: &mut [u8],
    ) -> Result<PackedPdu, MacError> {
        if self.logical_channel.is_some() {
            return Err(MacError::Mapping);
        }
        if tsn >= TSN_MODULUS || pdus.is_empty() || pdus.len() > MAX_PDUS {
            return Err(MacError::Mapping);
        }
        if output.len() != transport_bits.div_ceil(8) {
            return Err(MacError::Capacity);
        }
        let mut runs = [Run::default(); MAX_RUNS];
        let mut count = 0;
        let mut payload_bits: usize = 0;
        for pdu in pdus {
            let size = self
                .sizes
                .get(usize::from(pdu.size_index))
                .copied()
                .flatten()
                .ok_or(MacError::Mapping)?
                .get();
            if pdu.bits.len() != size {
                return Err(MacError::Mapping);
            }
            payload_bits = payload_bits.checked_add(size).ok_or(MacError::Capacity)?;
            if count > 0 && runs[count - 1].size == size {
                runs[count - 1].count += 1;
            } else {
                if count == MAX_RUNS {
                    return Err(MacError::Capacity);
                }
                runs[count] = Run {
                    sid: pdu.size_index,
                    count: 1,
                    size,
                };
                count += 1;
            }
        }
        let header_bits = FIXED_HEADER_BITS + count * RUN_HEADER_BITS;
        if header_bits
            .checked_add(payload_bits)
            .is_none_or(|bits| bits > transport_bits)
        {
            return Err(MacError::Capacity);
        }
        output.fill(0);
        write_u32(output, QUEUE_OFFSET, QUEUE_BITS, u32::from(self.queue))?;
        write_u32(output, TSN_OFFSET, TSN_BITS, u32::from(tsn))?;
        for (index, run) in runs[..count].iter().enumerate() {
            let offset = FIXED_HEADER_BITS + index * RUN_HEADER_BITS;
            write_u32(output, offset, SID_BITS, u32::from(run.sid))?;
            write_u32(output, offset + SID_BITS, COUNT_BITS, run.count as u32)?;
            write_u32(
                output,
                offset + SID_BITS + COUNT_BITS,
                1,
                u32::from(index + 1 == count),
            )?;
        }
        let mut offset = header_bits;
        for pdu in pdus {
            pdu.bits.copy_into(output, offset)?;
            offset += pdu.bits.len();
        }
        Ok(PackedPdu {
            queue: self.queue,
            sequence: tsn,
            transport_bits,
            header_bits,
            payload_bits,
            pdus: pdus.len(),
            end_fragment_octets: 0,
        })
    }
}
