// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    common_tx::{CommonTxError, MAX_PENDING_TRANSPORTS},
    tx_evidence::TransportSubmission,
};
use umts_dsp::{bits::Bit, indicators::PAGING_LEAD_CHIPS};
use umts_iuh::ranap::paging::{Cause, Request, TemporaryIdentity};
use umts_rrc::{
    common_channels::{PagingIndicators, TransportRole},
    paging as rrc,
};
use umts_types::time::CHIPS_PER_FRAME;

const DOMAINS: usize = 2;
const MAX_PAGING_BITS: usize = 512;
const MAX_INDICATORS: usize = 144;
const PCH_AFTER_OCCASION_FRAMES: u64 = 2;
const RETENTION_CYCLES: u64 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PagingCounters {
    pub queued: u64,
    pub coalesced: u64,
    pub capacity_dropped: u64,
    pub outside_area: u64,
    pub expired: u64,
    pub dedicated_queued: u64,
    pub dedicated_blocked: u64,
    pub horizon_blocked: u64,
    pub publication_blocked: u64,
    pub missed_occasion: u64,
    pub pch_planned: u64,
    pub pch_submitted: u64,
    pub pch_lost: u64,
    pub invalid: u64,
}

#[derive(Clone, Copy)]
struct Channel {
    index: usize,
    bits: usize,
    indicators: usize,
}
struct Entry {
    request: Request,
    expiry: Option<u64>,
    occasion: Option<u64>,
    receipt: Option<TransportSubmission>,
    completed_attempts: u64,
}
pub(super) struct PagingQueue {
    entries: Vec<Option<Entry>>,
    cursor: usize,
    channels: Vec<Channel>,
    payload: Vec<Bit>,
    plmn: [u8; 3],
    lac: u16,
    rac: Option<u8>,
    drx: [Option<u8>; DOMAINS],
}

impl PagingQueue {
    pub fn retained_bytes(source: &dyn CommonTxTarget, maximum_ues: usize) -> Option<usize> {
        maximum_ues
            .checked_mul(DOMAINS)?
            .checked_mul(std::mem::size_of::<Option<Entry>>())?
            .checked_add(MAX_PAGING_BITS * std::mem::size_of::<Bit>())?
            .checked_add(
                source
                    .information()
                    .sib5()
                    .secondary_channels()
                    .len()
                    .checked_mul(std::mem::size_of::<Channel>())?,
            )
    }
    pub fn new(source: &dyn CommonTxTarget, maximum_ues: usize) -> Result<Self, SignallingError> {
        let info = source.information();
        let mut channels = Vec::new();
        channels
            .try_reserve_exact(info.sib5().secondary_channels().len())
            .map_err(|_| SignallingError::Capacity)?;
        for (index, physical) in info.sib5().secondary_channels().iter().enumerate() {
            let [transport] = physical.transports() else {
                continue;
            };
            if transport.role != TransportRole::Paging {
                continue;
            }
            let [group] = transport.formats.groups.as_slice() else {
                return Err(SignallingError::Mapping);
            };
            let bits = usize::from(group.transport_block_bits);
            let pich = physical
                .settings()
                .pich
                .as_ref()
                .ok_or(SignallingError::Mapping)?;
            if bits > MAX_PAGING_BITS
                || bits == 0
                || !group.block_counts.contains(&1)
                || transport.formats.interval != umts_rrc::transport::TransportInterval::Ms10
                || physical.settings().timing_offset_chips != 0
            {
                return Err(SignallingError::Mapping);
            }
            let indicators = match pich.indicators {
                PagingIndicators::PerFrame18 => 18,
                PagingIndicators::PerFrame36 => 36,
                PagingIndicators::PerFrame72 => 72,
                PagingIndicators::PerFrame144 => 144,
            };
            channels.push(Channel {
                index,
                bits,
                indicators,
            });
        }
        if channels.is_empty() {
            return Err(SignallingError::Mapping);
        }
        let capacity = maximum_ues
            .checked_mul(DOMAINS)
            .ok_or(SignallingError::Capacity)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| SignallingError::Capacity)?;
        entries.resize_with(capacity, || None);
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(MAX_PAGING_BITS)
            .map_err(|_| SignallingError::Capacity)?;
        payload.resize(MAX_PAGING_BITS, Bit::Zero);
        Ok(Self {
            entries,
            cursor: 0,
            channels,
            payload,
            plmn: info.plmn(),
            lac: info.sib1().location_area_code,
            rac: info.sib1().ps.as_ref().map(|ps| ps.routing_area_code),
            drx: [
                info.sib1().cs.as_ref().map(|cs| cs.drx_coefficient),
                info.sib1().ps.as_ref().map(|ps| ps.drx_coefficient),
            ],
        })
    }
}

fn cause(value: Option<Cause>) -> rrc::Cause {
    match value {
        Some(Cause::Conversational) => rrc::Cause::terminatingConversationalCall,
        Some(Cause::Streaming) => rrc::Cause::terminatingStreamingCall,
        Some(Cause::Interactive) => rrc::Cause::terminatingInteractiveCall,
        Some(Cause::Background) => rrc::Cause::terminatingBackgroundCall,
        Some(Cause::LowPrioritySignalling) => rrc::Cause::terminatingLowPrioritySignalling,
        Some(Cause::HighPrioritySignalling) => rrc::Cause::terminatingHighPrioritySignalling,
        _ => rrc::Cause::terminatingCauseUnknown,
    }
}

fn record(request: &Request) -> rrc::Record {
    rrc::Record {
        identity: match request.temporary {
            Some(TemporaryIdentity::Tmsi(value)) => rrc::Identity::Tmsi(value),
            Some(TemporaryIdentity::Ptmsi(value)) => rrc::Identity::Ptmsi(value),
            None => rrc::Identity::Imsi(request.imsi),
        },
        domain: request.domain.into(),
        cause: cause(request.cause),
    }
}

impl CommonSignalling {
    pub(crate) fn receive_paging(&mut self, request: &Request) {
        let counters = &mut self.counters.paging;
        if self
            .paging
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.request == *request)
        {
            counters.coalesced = counters.coalesced.saturating_add(1);
            return;
        }
        let Some(slot) = self.paging.entries.iter_mut().find(|entry| entry.is_none()) else {
            counters.capacity_dropped = counters.capacity_dropped.saturating_add(1);
            return;
        };
        *slot = Some(Entry {
            request: request.clone(),
            expiry: None,
            occasion: None,
            receipt: None,
            completed_attempts: 0,
        });
        counters.queued = counters.queued.saturating_add(1);
    }

    pub(crate) fn service_paging(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        guard: u64,
    ) {
        if self.paging.entries.is_empty() {
            return;
        }
        let index = self.paging.cursor;
        self.paging.cursor = (index + 1) % self.paging.entries.len();
        let Some(mut entry) = self.paging.entries[index].take() else {
            return;
        };
        let result = self.service_page(source, now, guard, &mut entry);
        if result {
            self.paging.entries[index] = Some(entry);
        }
    }

    fn service_page(
        &mut self,
        source: &mut dyn CommonTxTarget,
        now: ChipInstant,
        guard: u64,
        entry: &mut Entry,
    ) -> bool {
        if now.epoch != self.epoch() || source.next_chip().epoch != now.epoch {
            self.counters.paging.invalid = self.counters.paging.invalid.saturating_add(1);
            return false;
        }
        let count = &mut self.counters.paging;
        if let Some(receipt) = &entry.receipt {
            if !receipt.viable() {
                count.pch_lost = count.pch_lost.saturating_add(1);
            } else if receipt.submitted() {
                count.pch_submitted = count.pch_submitted.saturating_add(1);
            } else {
                return true;
            }
            entry.completed_attempts += 1;
            // TS 25.331 8.1.2.2 permits repetition at subsequent paging occasions.
            entry.receipt = None;
            entry.occasion = None;
        }
        let domain = if entry.request.domain == umts_iuh::Domain::Cs {
            0
        } else {
            1
        };
        let coefficient = entry
            .request
            .drx_coefficient
            .or(self.paging.drx[domain])
            .unwrap_or(9);
        if !(6..=9).contains(&coefficient) {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        }
        let cycle = 1u64 << coefficient;
        let Some(expiry) = entry.expiry.or_else(|| {
            now.index.checked_add(
                (RETENTION_CYCLES * cycle + PCH_AFTER_OCCASION_FRAMES) * CHIPS_PER_FRAME,
            )
        }) else {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        };
        entry.expiry = Some(expiry);
        if now.index >= expiry {
            count.expired = count.expired.saturating_add(1);
            return false;
        }

        let known = (0..self.registry.capacity())
            .filter_map(|slot| self.registry.at_slot(slot))
            .find(|(_, context)| {
                context.setup.phase() == EstablishmentPhase::Established
                    && context.idle_release.is_none()
                    && (context.core_imsi == Some(entry.request.imsi)
                        || context.registration_imsi == Some(entry.request.imsi))
            })
            .map(|(key, _)| key);
        if let Some(key) = known {
            if entry.completed_attempts != 0
                && self.registry.get(key).is_some_and(|context| {
                    context.router.as_ref().is_some_and(|router| {
                        router.connection(entry.request.domain.into()).is_some()
                    })
                })
            {
                return false;
            }
            let result = self.page_connected(key, &entry.request);
            match result {
                Ok(()) => {
                    self.counters.paging.dedicated_queued =
                        self.counters.paging.dedicated_queued.saturating_add(1);
                    return false;
                }
                Err(error) if super::scheduling::backpressure(&error) => {
                    self.counters.paging.dedicated_blocked =
                        self.counters.paging.dedicated_blocked.saturating_add(1);
                    return true;
                }
                Err(_) => {
                    self.counters.paging.invalid = self.counters.paging.invalid.saturating_add(1);
                    return false;
                }
            }
        }

        // Identity resolution can enable TS 25.331 8.1.11 paging before expiry.
        if entry.completed_attempts >= RETENTION_CYCLES {
            return true;
        }

        let count = &mut self.counters.paging;
        if entry.request.area.is_some_and(|area| {
            area.plmn != self.paging.plmn
                || area.lac != self.paging.lac
                || area.rac.is_some_and(|rac| Some(rac) != self.paging.rac)
        }) {
            count.outside_area = count.outside_area.saturating_add(1);
            return false;
        }
        let Some(earliest) = now
            .index
            .checked_add(guard)
            .map(|time| time.max(source.next_chip().index))
        else {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        };
        let earliest_frame = earliest.div_ceil(CHIPS_PER_FRAME);
        let Some((channel, occasion)) = rrc::next_occasion(
            entry.request.imsi,
            cycle,
            self.paging.channels.len(),
            earliest_frame,
        ) else {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        };
        let occasion = *entry.occasion.get_or_insert(occasion);
        let Some(start) = occasion
            .checked_add(PCH_AFTER_OCCASION_FRAMES)
            .and_then(|frame| frame.checked_mul(CHIPS_PER_FRAME))
        else {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        };
        if start
            .checked_sub(PAGING_LEAD_CHIPS)
            .is_none_or(|pich| pich < earliest)
        {
            count.missed_occasion = count.missed_occasion.saturating_add(1);
            entry.occasion = None;
            return true;
        }
        if start - source.next_chip().index > MAX_PENDING_TRANSPORTS as u64 * CHIPS_PER_FRAME {
            count.horizon_blocked = count.horizon_blocked.saturating_add(1);
            return true;
        }
        let channel_number = channel;
        let channel = self.paging.channels[channel_number];
        let mut records = [record(&entry.request); rrc::MAX_RECORDS];
        let mut selected = [None; rrc::MAX_RECORDS];
        let mut record_count = 1;
        let Ok(mut encoded) = rrc::idle_records(&records[..record_count]) else {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        };
        if encoded.bit_length > channel.bits {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        }
        let mut indicators = [false; MAX_INDICATORS];
        let Some(indicator) = rrc::indicator(entry.request.imsi, channel.indicators) else {
            count.invalid = count.invalid.saturating_add(1);
            return false;
        };
        indicators[indicator] = true;
        for (index, candidate) in self.paging.entries.iter().enumerate() {
            let Some(candidate) = candidate else {
                continue;
            };
            if record_count == rrc::MAX_RECORDS {
                break;
            }
            if candidate.receipt.is_some()
                || candidate.completed_attempts >= RETENTION_CYCLES
                || candidate.expiry.is_some_and(|expiry| now.index >= expiry)
                || candidate.occasion.is_some_and(|frame| frame != occasion)
                || candidate.request.area.is_some_and(|area| {
                    area.plmn != self.paging.plmn
                        || area.lac != self.paging.lac
                        || area.rac.is_some_and(|rac| Some(rac) != self.paging.rac)
                })
            {
                continue;
            }
            if (0..self.registry.capacity())
                .filter_map(|slot| self.registry.at_slot(slot))
                .any(|(_, context)| {
                    context.setup.phase() == EstablishmentPhase::Established
                        && (context.core_imsi == Some(candidate.request.imsi)
                            || context.registration_imsi == Some(candidate.request.imsi))
                })
            {
                continue;
            }
            let domain = if candidate.request.domain == umts_iuh::Domain::Cs {
                0
            } else {
                1
            };
            let Some(coefficient) = candidate
                .request
                .drx_coefficient
                .or(self.paging.drx[domain])
            else {
                continue;
            };
            if !(6..=9).contains(&coefficient)
                || rrc::next_occasion(
                    candidate.request.imsi,
                    1u64 << coefficient,
                    self.paging.channels.len(),
                    occasion,
                ) != Some((channel_number, occasion))
            {
                continue;
            }
            records[record_count] = record(&candidate.request);
            let Ok(combined) = rrc::idle_records(&records[..record_count + 1]) else {
                continue;
            };
            if combined.bit_length > channel.bits {
                continue;
            }
            let Some(indicator) = rrc::indicator(candidate.request.imsi, channel.indicators) else {
                continue;
            };
            indicators[indicator] = true;
            let Some(expiry) = candidate.expiry.or_else(|| {
                now.index.checked_add(
                    (RETENTION_CYCLES * (1u64 << coefficient) + PCH_AFTER_OCCASION_FRAMES)
                        * CHIPS_PER_FRAME,
                )
            }) else {
                continue;
            };
            selected[record_count] = Some((index, expiry));
            record_count += 1;
            encoded = combined;
        }
        self.paging.payload.fill(Bit::Zero);
        for bit in 0..encoded.bit_length {
            self.paging.payload[bit] = Bit::from(
                encoded.bytes[bit / BITS_PER_BYTE]
                    & (1 << (BITS_PER_BYTE - 1 - bit % BITS_PER_BYTE))
                    != 0,
            );
        }
        // TS 25.321 9.2.1.3 gives PCCH no MAC header. PCCH uses transparent RLC.
        match source.publish_transport(
            ChipInstant {
                index: start,
                ..now
            },
            channel.index,
            &self.paging.payload[..channel.bits],
            &indicators[..channel.indicators],
            None,
        ) {
            Ok(receipt) => {
                for &(index, expiry) in selected[1..record_count].iter().flatten() {
                    let candidate = self.paging.entries[index]
                        .as_mut()
                        .expect("selected paging owner");
                    candidate.receipt = Some(receipt.clone());
                    candidate.expiry = Some(expiry);
                }
                entry.receipt = Some(receipt);
                count.pch_planned = count.pch_planned.saturating_add(record_count as u64);
                true
            }
            Err(CommonTxError::Capacity | CommonTxError::Conflict) => {
                count.publication_blocked = count.publication_blocked.saturating_add(1);
                true
            }
            Err(CommonTxError::Late) => {
                entry.occasion = None;
                true
            }
            Err(_) => {
                count.invalid = count.invalid.saturating_add(1);
                false
            }
        }
    }

    fn page_connected(&mut self, key: UeKey, request: &Request) -> Result<(), SignallingError> {
        // The previous domain's release must precede paging for its next connection.
        if self.core.connection_releasing(key, request.domain) {
            return Err(SignallingError::Capacity);
        }
        let context = self.registry.get_mut(key).ok_or(SignallingError::Origin)?;
        // TS 25.331 8.1.11.2 and 8.1.12.5 allow paging after authenticated security completion.
        if context.protection.bearer_reconfiguration_block().is_some()
            || context.bearer_change.is_some()
        {
            return Err(SignallingError::Capacity);
        }
        if [umts_iuh::Domain::Cs, umts_iuh::Domain::Ps]
            .into_iter()
            .any(|domain| context.security.active_origin(domain).is_some())
            && context
                .protection
                .integrity
                .downlink_origin(key, SignallingBearer::ControlAm.identity())
                .map_err(SignallingError::Integrity)?
                .is_none()
        {
            return Err(SignallingError::Capacity);
        }
        let kind = super::transactions::MessageType::Paging;
        let transaction = context.transactions.next(kind);
        let message = rrc::dedicated(
            transaction,
            request.domain.into(),
            cause(request.cause),
            request.temporary.is_some(),
        );
        let control = &mut context.control;
        context
            .protection
            .integrity
            .publish_with(
                key,
                SignallingBearer::ControlAm.identity(),
                &message,
                |encoded| {
                    control.enqueue(
                        key,
                        Bits::new(&encoded.bytes, encoded.bit_length)
                            .map_err(|_| CommonAmError::Payload)?,
                    )
                },
            )
            .map_err(|error| super::protection::schedule_error(error, am_admission_error))?;
        context.transactions.admitted(kind, transaction);
        context.activity = context.activity.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
