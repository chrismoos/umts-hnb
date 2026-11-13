// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Report, ReportRef};
use crate::dedicated_rx::ReceiveIdentity;
use std::num::{NonZeroU64, NonZeroUsize};
use umts_dsp::{bits::pack_msb, transport_blocks::BlockOutcome};
use umts_mac::hsupa::{
    Reception,
    payload::{Mapping, Mappings, Payload},
    reorder::{Admission, Disposition, REORDER_WINDOW, Reorder},
};
use umts_types::bits::Bits;

const LOGICAL_CHANNELS: usize = 16;
const MAX_RETAINED_GROUPS: usize = LOGICAL_CHANNELS * REORDER_WINDOW + 1;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    Identity,
    Time,
    Configuration,
    Payload,
    Budget,
    Backpressure,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketAdmission {
    pub stored: usize,
    pub duplicates: usize,
    pub old: usize,
    pub scheduling_information: Option<u32>,
}

pub struct PacketPdu<'a> {
    pub report: Report,
    pub logical_channel: u8,
    pub flow: u8,
    pub bits: Bits<'a>,
}

struct GroupMetadata {
    report: Report,
    mapping: Mapping,
    count: u8,
}
struct StoredGroup {
    bytes: Vec<u8>,
    metadata: Option<GroupMetadata>,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct PacketDelivery {
    identity: ReceiveIdentity,
    mappings: Mappings,
    channels: [Option<Reorder<usize>>; LOGICAL_CHANNELS],
    groups: Vec<StoredGroup>,
    free: Vec<usize>,
    packed: Vec<u8>,
    maximum_bits: usize,
    pending: Option<(usize, u8)>,
    next_channel: usize,
    now: u64,
    retained_bytes: usize,
}

impl PacketDelivery {
    pub fn required_capacity(entries: &[(u8, Mapping)]) -> Result<usize, PacketError> {
        Mappings::new(entries).map_err(|_| PacketError::Configuration)?;
        let mut channels = [false; LOGICAL_CHANNELS];
        for (_, mapping) in entries {
            channels[usize::from(mapping.logical_channel)] = true;
        }
        // Each reorder window must admit its missing head while one popped group remains in RLC delivery.
        Ok(channels.into_iter().filter(|present| *present).count() * REORDER_WINDOW + 1)
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub fn new(
        identity: ReceiveIdentity,
        entries: &[(u8, Mapping)],
        maximum_bits: NonZeroUsize,
        capacity: NonZeroUsize,
        hold: NonZeroU64,
        now: u64,
        budget: NonZeroUsize,
    ) -> Result<Self, PacketError> {
        if entries.is_empty() || capacity.get() > MAX_RETAINED_GROUPS {
            return Err(PacketError::Configuration);
        }
        let mappings = Mappings::new(entries).map_err(|_| PacketError::Configuration)?;
        let bytes = maximum_bits.get().div_ceil(8);
        let retained_bytes = bytes
            .checked_add(std::mem::size_of::<StoredGroup>() + std::mem::size_of::<usize>())
            .and_then(|per_group| per_group.checked_mul(capacity.get()))
            .and_then(|total| total.checked_add(bytes))
            .and_then(|total| total.checked_add(std::mem::size_of::<Self>()))
            .ok_or(PacketError::Budget)?;
        if retained_bytes > budget.get() {
            return Err(PacketError::Budget);
        }
        let mut channels = std::array::from_fn(|_| None);
        let mut flows = [None; LOGICAL_CHANNELS];
        for &(_, mapping) in entries {
            let logical = usize::from(mapping.logical_channel);
            if mapping.pdu_bits.get() > maximum_bits.get()
                || flows[logical].is_some_and(|flow| flow != mapping.flow)
            {
                return Err(PacketError::Configuration);
            }
            flows[logical] = Some(mapping.flow);
            channels[logical] = Some(Reorder::new(hold, now));
        }
        Ok(Self {
            identity,
            mappings,
            channels,
            groups: (0..capacity.get())
                .map(|_| StoredGroup {
                    bytes: vec![0; bytes],
                    metadata: None,
                })
                .collect(),
            free: (0..capacity.get()).rev().collect(),
            packed: vec![0; bytes],
            maximum_bits: maximum_bits.get(),
            pending: None,
            next_channel: 0,
            now,
            retained_bytes,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.free.len() == self.groups.len()
    }

    // Feedback acknowledges the transport CRC, independently of MAC parsing or RLC delivery.
    // Backpressure leaves every reordering process unchanged so the report can be retried.
    pub fn admit(
        &mut self,
        input: ReportRef<'_>,
        now: u64,
    ) -> Result<PacketAdmission, PacketError> {
        if input.report.identity != self.identity {
            return Err(PacketError::Identity);
        }
        if now < self.now {
            return Err(PacketError::Time);
        }
        if input.report.feedback != (Reception::Ack { deliver: true }) {
            return if input.payload.is_empty() && input.report.payload_bits == 0 {
                self.now = now;
                Ok(PacketAdmission::default())
            } else {
                Err(PacketError::Payload)
            };
        }
        if input.report.outcome != BlockOutcome::Decoded
            || input.payload.len() != input.report.payload_bits
            || input.payload.is_empty()
            || input.payload.len() > self.maximum_bits
        {
            return Err(PacketError::Payload);
        }
        let packed = &mut self.packed[..input.payload.len().div_ceil(8)];
        pack_msb(input.payload, packed).map_err(|_| PacketError::Payload)?;
        let payload = Payload::parse(
            Bits::new(packed, input.payload.len()).map_err(|_| PacketError::Payload)?,
            &self.mappings,
            input.report.control.information.format.value() == 0,
        )
        .map_err(|_| PacketError::Payload)?;
        let mut admission = PacketAdmission::default();
        let mut dispositions = [Disposition::Old; LOGICAL_CHANNELS];
        for (index, group) in payload.groups.iter().enumerate() {
            let Some(group) = group else {
                continue;
            };
            let queue = self.channels[usize::from(group.mapping.logical_channel)]
                .as_ref()
                .expect("validated DDI mapping");
            let disposition = queue
                .classify(group.sequence, now)
                .map_err(|error| match error {
                    umts_mac::hsupa::reorder::Error::Capacity => PacketError::Backpressure,
                    _ => PacketError::Time,
                })?;
            match disposition {
                Disposition::Store => admission.stored += 1,
                Disposition::Duplicate => admission.duplicates += 1,
                Disposition::Old => admission.old += 1,
            }
            dispositions[index] = disposition;
        }
        if admission.stored > self.free.len() {
            return Err(PacketError::Backpressure);
        }
        admission.scheduling_information = payload.scheduling_information.map(|bits| {
            bits.read_u32(0, bits.len())
                .expect("validated scheduling information")
        });
        for (index, group) in payload.groups.iter().enumerate() {
            let Some(group) = group else {
                continue;
            };
            if dispositions[index] != Disposition::Store {
                continue;
            }
            let slot = self.free.pop().expect("reserved group capacity");
            let stored = &mut self.groups[slot];
            group
                .data
                .copy_to(&mut stored.bytes[..group.data.len().div_ceil(8)])
                .expect("transport-bounded MAC-es group");
            stored.metadata = Some(GroupMetadata {
                report: input.report,
                mapping: group.mapping,
                count: group.count,
            });
            let queue = self.channels[usize::from(group.mapping.logical_channel)]
                .as_mut()
                .expect("validated DDI mapping");
            assert!(
                matches!(
                    queue.insert(group.sequence, now, slot),
                    Ok(Admission::Stored)
                ),
                "one prevalidated group per logical channel"
            );
        }
        self.now = now;
        Ok(admission)
    }

    #[cfg(test)]
    pub fn peek(&mut self, now: u64) -> Result<Option<PacketPdu<'_>>, PacketError> {
        self.peek_matching(now, |_| true)
    }

    pub fn peek_matching(
        &mut self,
        now: u64,
        allowed: impl Fn(u8) -> bool,
    ) -> Result<Option<PacketPdu<'_>>, PacketError> {
        if now < self.now {
            return Err(PacketError::Time);
        }
        self.now = now;
        if self.pending.is_none() {
            for distance in 0..LOGICAL_CHANNELS {
                let logical = (self.next_channel + distance) % LOGICAL_CHANNELS;
                if !allowed(logical as u8) {
                    continue;
                }
                let Some(queue) = &mut self.channels[logical] else {
                    continue;
                };
                if let Some(slot) = queue.pop(now).map_err(|_| PacketError::Time)? {
                    self.pending = Some((slot, 0));
                    self.next_channel = (logical + 1) % LOGICAL_CHANNELS;
                    break;
                }
            }
        }
        let Some((slot, index)) = self.pending else {
            return Ok(None);
        };
        let stored = &self.groups[slot];
        let metadata = stored.metadata.as_ref().expect("retained MAC-es group");
        if !allowed(metadata.mapping.logical_channel) {
            return Ok(None);
        }
        let width = metadata.mapping.pdu_bits.get();
        let offset = usize::from(index) * width;
        let bits = Bits::new(&stored.bytes, usize::from(metadata.count) * width)
            .expect("bounded stored group")
            .subspan(offset..offset + width)
            .expect("bounded PDU index");
        Ok(Some(PacketPdu {
            report: metadata.report,
            logical_channel: metadata.mapping.logical_channel,
            flow: metadata.mapping.flow,
            bits,
        }))
    }

    pub fn release(&mut self) -> bool {
        let Some((slot, index)) = self.pending else {
            return false;
        };
        let stored = &mut self.groups[slot];
        let count = stored
            .metadata
            .as_ref()
            .expect("retained MAC-es group")
            .count;
        if index + 1 == count {
            stored.metadata = None;
            self.pending = None;
            self.free.push(slot);
        } else {
            self.pending = Some((slot, index + 1));
        }
        true
    }
}
