// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    AMD_HEADER_BYTES, AmError, AmLimits, Indicator, LiWidth, MAX_LIS, MAX_QUEUED_SDUS, storage,
};

pub(super) struct Sdu {
    bytes: Vec<u8>,
    length: usize,
    consumed: usize,
    tracking: Option<usize>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Boundary {
    None,
    Exact,
    OneShort,
}

pub(super) struct Prepared {
    finished: usize,
    partial: Option<usize>,
    boundary: Boundary,
    boundary_tracking: Option<usize>,
    pub contributions: [Option<Contribution>; MAX_QUEUED_SDUS + 1],
}
#[derive(Clone, Copy)]
pub(super) struct Contribution {
    pub slot: usize,
    pub delimited: bool,
}
pub(super) struct Queue {
    sdus: Vec<Sdu>,
    head: usize,
    count: usize,
    boundary: Boundary,
    boundary_tracking: Option<usize>,
    width: LiWidth,
    revision: u64,
    pub packet: Vec<u8>,
    data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ForecastCounts {
    pub queued_pdus: usize,
    pub total_pdus: usize,
    pub queue_revision: u64,
}

impl Queue {
    pub fn new(bytes: usize, limits: AmLimits) -> Result<Self, AmError> {
        let sdus = (0..limits.queued_sdus.get())
            .map(|_| {
                Ok(Sdu {
                    bytes: storage(limits.maximum_sdu_bytes.get())?,
                    length: 0,
                    consumed: 0,
                    tracking: None,
                })
            })
            .collect::<Result<_, AmError>>()?;
        Ok(Self {
            sdus,
            head: 0,
            count: 0,
            boundary: Boundary::None,
            boundary_tracking: None,
            width: LiWidth::for_fixed_size(bytes),
            revision: 0,
            packet: storage(bytes)?,
            data: storage(bytes)?,
        })
    }
    pub fn pending(&self) -> bool {
        self.count != 0 || self.boundary != Boundary::None
    }
    pub fn pending_sdus(&self) -> usize {
        self.count
    }
    pub fn isolated_pdus(&self, bytes: usize) -> Result<usize, AmError> {
        if self.pending() {
            return Err(AmError::State);
        }
        if bytes == 0 {
            return Err(AmError::Length);
        }
        if bytes > self.sdus[0].bytes.len() {
            return Err(AmError::Capacity);
        }
        let payload = self.packet.len() - AMD_HEADER_BYTES;
        let full = bytes / payload;
        let remainder = bytes % payload;
        let trailing = if self.width == LiWidth::Fifteen && remainder == payload - 1 {
            2
        } else {
            1
        };
        full.checked_add(trailing).ok_or(AmError::CountExhausted)
    }
    pub fn forecast(
        &self,
        appended_bytes: usize,
        maximum_pdus: usize,
    ) -> Result<ForecastCounts, AmError> {
        if appended_bytes == 0 {
            return Err(AmError::Length);
        }
        if maximum_pdus == 0
            || appended_bytes > self.sdus[0].bytes.len()
            || self.count == self.sdus.len()
        {
            return Err(AmError::Capacity);
        }
        let mut existing = [0usize; MAX_QUEUED_SDUS + 1];
        for (index, remaining) in existing.iter_mut().take(self.count).enumerate() {
            let sdu = &self.sdus[(self.head + index) % self.sdus.len()];
            *remaining = sdu.length - sdu.consumed;
        }
        let mut with_appended = existing;
        let queued_pdus = forecast_sequence(
            &mut existing,
            self.count,
            self.boundary,
            self.packet.len(),
            self.width,
            maximum_pdus,
        )?;

        with_appended[self.count] = appended_bytes;
        let total_pdus = forecast_sequence(
            &mut with_appended,
            self.count + 1,
            self.boundary,
            self.packet.len(),
            self.width,
            maximum_pdus,
        )?;
        if total_pdus < queued_pdus {
            return Err(AmError::State);
        }
        Ok(ForecastCounts {
            queued_pdus,
            total_pdus,
            queue_revision: self.revision,
        })
    }
    pub fn enqueue(&mut self, bytes: &[u8]) -> Result<(), AmError> {
        self.enqueue_tracked(bytes, None)
    }
    pub fn enqueue_tracked(
        &mut self,
        bytes: &[u8],
        tracking: Option<usize>,
    ) -> Result<(), AmError> {
        if bytes.is_empty() {
            return Err(AmError::Length);
        }
        if self.count == self.sdus.len() {
            return Err(AmError::Capacity);
        }
        let index = (self.head + self.count) % self.sdus.len();
        let slot = &mut self.sdus[index];
        if bytes.len() > slot.bytes.len() {
            return Err(AmError::Capacity);
        }
        slot.bytes[..bytes.len()].copy_from_slice(bytes);
        slot.length = bytes.len();
        slot.consumed = 0;
        slot.tracking = tracking;
        self.count += 1;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }
    pub fn prepare(&mut self) -> Result<Prepared, AmError> {
        let mut indicators = [Indicator::Padding; MAX_LIS];
        let mut count = 0;
        match self.boundary {
            Boundary::Exact => {
                indicators[0] = Indicator::PreviousExact;
                count = 1;
            }
            Boundary::OneShort => {
                indicators[0] = Indicator::PreviousOneShort;
                count = 1;
            }
            Boundary::None => {}
        }
        let size = self.packet.len();
        let width = self.width.octets();
        let mut copied = 0;
        let mut finished = 0;
        let mut partial = None;
        let mut boundary = Boundary::None;
        let mut boundary_tracking = None;
        let mut contributions = [None; MAX_QUEUED_SDUS + 1];
        let mut contribution_count = 0;
        if let Some(slot) = self.boundary_tracking {
            contributions[contribution_count] = Some(Contribution {
                slot,
                delimited: true,
            });
            contribution_count += 1;
        }
        while finished < self.count && count + 1 < MAX_LIS {
            let sdu = &self.sdus[(self.head + finished) % self.sdus.len()];
            let remaining = sdu.length - sdu.consumed;
            let free = size - AMD_HEADER_BYTES - count * width - copied;
            if let Some(slot) = sdu.tracking {
                contributions[contribution_count] = Some(Contribution {
                    slot,
                    delimited: remaining < free && !(width == 2 && remaining + 1 == free),
                });
                contribution_count += 1;
            }
            if remaining >= free {
                self.data[copied..copied + free]
                    .copy_from_slice(&sdu.bytes[sdu.consumed..sdu.consumed + free]);
                copied += free;
                if remaining == free {
                    finished += 1;
                    boundary = Boundary::Exact;
                    boundary_tracking = sdu.tracking;
                } else {
                    partial = Some(sdu.consumed + free);
                }
                break;
            }
            if width == 2 && remaining + 1 == free {
                self.data[copied..copied + remaining]
                    .copy_from_slice(&sdu.bytes[sdu.consumed..sdu.length]);
                copied += remaining;
                self.data[copied] = 0;
                copied += 1;
                finished += 1;
                boundary = Boundary::OneShort;
                boundary_tracking = sdu.tracking;
                break;
            }
            if count == MAX_LIS {
                return Err(AmError::Capacity);
            }
            indicators[count] = Indicator::End((copied + remaining) as u16);
            count += 1;
            self.data[copied..copied + remaining]
                .copy_from_slice(&sdu.bytes[sdu.consumed..sdu.length]);
            copied += remaining;
            finished += 1;
            let free = size - AMD_HEADER_BYTES - count * width - copied;
            if free < width {
                self.data[copied..copied + free].fill(0);
                copied += free;
                break;
            }
        }
        if boundary == Boundary::None && size - AMD_HEADER_BYTES - count * width - copied >= width {
            if count == MAX_LIS {
                return Err(AmError::Capacity);
            }
            indicators[count] = Indicator::Padding;
            count += 1;
        }
        self.packet.fill(0);
        super::wire::data_header(&mut self.packet, 0, false, count != 0);
        for (index, &indicator) in indicators[..count].iter().enumerate() {
            let value = (indicator.word(self.width) << 1) | u16::from(index + 1 != count);
            let cursor = AMD_HEADER_BYTES + index * width;
            if width == 1 {
                self.packet[cursor] = value as u8;
            } else {
                self.packet[cursor..cursor + width].copy_from_slice(&value.to_be_bytes());
            }
        }
        let header = AMD_HEADER_BYTES + count * width;
        self.packet[header..header + copied].copy_from_slice(&self.data[..copied]);
        Ok(Prepared {
            finished,
            partial,
            boundary,
            boundary_tracking,
            contributions,
        })
    }
    pub fn last(&self, prepared: &Prepared) -> bool {
        prepared.finished == self.count && prepared.boundary == Boundary::None
    }
    pub fn commit(&mut self, prepared: Prepared) {
        for index in 0..prepared.finished {
            let index = (self.head + index) % self.sdus.len();
            self.sdus[index].length = 0;
            self.sdus[index].consumed = 0;
            self.sdus[index].tracking = None;
        }
        self.head = (self.head + prepared.finished) % self.sdus.len();
        self.count -= prepared.finished;
        if let Some(consumed) = prepared.partial {
            self.sdus[self.head].consumed = consumed;
        }
        self.boundary = prepared.boundary;
        self.boundary_tracking = prepared.boundary_tracking;
        self.revision = self.revision.saturating_add(1);
    }
    pub fn reset_transmitted(&mut self) {
        let before = (self.count, self.boundary);
        if self.count > 0 && self.sdus[self.head].consumed != 0 {
            self.sdus[self.head].consumed = 0;
            self.sdus[self.head].length = 0;
            self.sdus[self.head].tracking = None;
            self.head = (self.head + 1) % self.sdus.len();
            self.count -= 1;
        }
        self.boundary = Boundary::None;
        self.boundary_tracking = None;
        if before != (self.count, self.boundary) {
            self.revision = self.revision.saturating_add(1);
        }
    }
}

fn forecast_sequence(
    remaining: &mut [usize; MAX_QUEUED_SDUS + 1],
    entries: usize,
    initial_boundary: Boundary,
    packet_bytes: usize,
    width: LiWidth,
    maximum_pdus: usize,
) -> Result<usize, AmError> {
    let payload = packet_bytes
        .checked_sub(AMD_HEADER_BYTES)
        .ok_or(AmError::Capacity)?;
    let li_bytes = width.octets();
    let mut head = 0usize;
    let mut boundary = initial_boundary;
    let mut pdus = 0usize;
    while head < entries || boundary != Boundary::None {
        if pdus >= maximum_pdus {
            return Err(AmError::Capacity);
        }
        if head == entries {
            boundary = Boundary::None;
            pdus += 1;
            continue;
        }
        let mut free = payload
            .checked_sub(if boundary != Boundary::None {
                li_bytes
            } else {
                0
            })
            .ok_or(AmError::Capacity)?;
        let mut finished = head;
        let mut indicators = usize::from(boundary != Boundary::None);
        let mut progressed = false;
        let mut next_boundary = Boundary::None;
        while finished < entries && indicators + 1 < MAX_LIS {
            let left = remaining[finished];
            if left >= free {
                remaining[finished] = left - free;
                progressed = remaining[finished] != left;
                if left == free {
                    finished += 1;
                    next_boundary = Boundary::Exact;
                }
                break;
            }
            if width == LiWidth::Fifteen && left.checked_add(1) == Some(free) {
                remaining[finished] = 0;
                finished += 1;
                next_boundary = Boundary::OneShort;
                break;
            }
            if free < li_bytes {
                break;
            }
            if indicators == MAX_LIS {
                return Err(AmError::Capacity);
            }
            indicators += 1;
            free -= li_bytes + left;
            remaining[finished] = 0;
            finished += 1;
            if free < li_bytes {
                break;
            }
        }
        if finished == head && !progressed {
            return Err(AmError::Capacity);
        }
        if finished == entries
            && next_boundary == Boundary::None
            && free >= li_bytes
            && indicators == MAX_LIS
        {
            return Err(AmError::Capacity);
        }
        head = finished;
        boundary = next_boundary;
        pdus += 1;
    }
    Ok(pdus)
}
