// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::NonZeroUsize;
use umts_rlc::acknowledged::{ReceiveCoverage, ReceiveTime};
use umts_rrc::nas::SignallingBearer;
use umts_rrc_codec::{CodecError, DecodedUplinkDedicated, decode_uplink_dedicated};
use umts_types::{UeKey, bits::BITS_PER_BYTE, time::ChipInstant};

use super::SignallingError;

struct Slot {
    sequence: u64,
    key: Option<UeKey>,
    bearer: SignallingBearer,
    time: Option<ReceiveTime>,
    bytes: Vec<u8>,
    length: usize,
    decoded: Option<Result<DecodedUplinkDedicated, CodecError>>,
    coverage: Option<ReceiveCoverage>,
}
impl Slot {
    fn clear(&mut self) {
        self.bytes[..self.length].fill(0);
        self.length = 0;
        self.key = None;
        self.time = None;
        self.decoded = None;
        self.coverage = None;
    }
}
struct Lane {
    slots: Vec<Slot>,
    ordered: Vec<usize>,
    free: Vec<usize>,
    waiting: Option<UeKey>,
}
impl Lane {
    fn new(capacity: NonZeroUsize, maximum: NonZeroUsize) -> Self {
        Self {
            slots: (0..capacity.get())
                .map(|_| Slot {
                    sequence: 0,
                    key: None,
                    bearer: SignallingBearer::Unacknowledged,
                    time: None,
                    bytes: vec![0; maximum.get()],
                    length: 0,
                    decoded: None,
                    coverage: None,
                })
                .collect(),
            ordered: Vec::with_capacity(capacity.get()),
            free: (0..capacity.get()).rev().collect(),
            waiting: None,
        }
    }
    fn remove(&mut self, index: usize) {
        let position = self
            .ordered
            .iter()
            .position(|slot| *slot == index)
            .expect("selected occupied slot");
        self.ordered.remove(position);
        self.slots[index].clear();
        self.free.push(index);
    }
    fn cancel(&mut self, key: UeKey) {
        let slots = &mut self.slots;
        let free = &mut self.free;
        self.ordered.retain(|index| {
            if slots[*index].key == Some(key) {
                slots[*index].clear();
                free.push(*index);
                false
            } else {
                true
            }
        });
        if self.waiting == Some(key) {
            self.waiting = None;
        }
    }
    fn first_eligible(
        &mut self,
        eligible: &mut impl FnMut(UeKey, SignallingBearer, Option<bool>) -> bool,
    ) -> Option<usize> {
        for &index in &self.ordered {
            let slot = &mut self.slots[index];
            if slot.decoded.is_none() {
                slot.decoded = Some(decode_uplink_dedicated(
                    &slot.bytes[..slot.length],
                    slot.length * BITS_PER_BYTE,
                ));
            }
            let protected = slot.decoded.as_ref().and_then(|message| {
                message
                    .as_ref()
                    .ok()
                    .map(|message| message.value.integrity_check_info.is_some())
            });
            if eligible(
                slot.key.expect("occupied lane owner"),
                slot.bearer,
                protected,
            ) {
                return Some(index);
            }
        }
        None
    }
}
#[derive(Clone, Copy)]
enum LaneId {
    Control,
    Data,
}
#[derive(Clone, Copy)]
struct Selection {
    lane: LaneId,
    index: usize,
}

pub(super) struct DedicatedIngress {
    control: Lane,
    data: Lane,
    next_sequence: u64,
    selected: Option<Selection>,
    maximum_sdu_bytes: usize,
}
pub(super) struct IngressView<'a> {
    pub key: UeKey,
    pub bearer: SignallingBearer,
    pub time: ReceiveTime,
    pub message: &'a Result<DecodedUplinkDedicated, CodecError>,
    pub bytes: &'a [u8],
    pub coverage: Option<ReceiveCoverage>,
}
impl DedicatedIngress {
    pub fn buffer_bytes(
        data: NonZeroUsize,
        control: NonZeroUsize,
        maximum: NonZeroUsize,
    ) -> Result<usize, SignallingError> {
        let slots = data
            .get()
            .checked_add(control.get())
            .ok_or(SignallingError::Capacity)?;
        let each = maximum
            .get()
            .checked_add(std::mem::size_of::<Slot>())
            .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
            .ok_or(SignallingError::Capacity)?;
        slots
            .checked_mul(each)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or(SignallingError::Capacity)
    }
    pub fn new(
        data: NonZeroUsize,
        control: NonZeroUsize,
        maximum: NonZeroUsize,
    ) -> Result<Self, SignallingError> {
        Self::buffer_bytes(data, control, maximum)?;
        Ok(Self {
            control: Lane::new(control, maximum),
            data: Lane::new(data, maximum),
            next_sequence: 0,
            selected: None,
            maximum_sdu_bytes: maximum.get(),
        })
    }
    fn lane(&self, bearer: SignallingBearer) -> &Lane {
        if bearer == SignallingBearer::ControlAm {
            &self.control
        } else {
            &self.data
        }
    }
    fn lane_mut(&mut self, bearer: SignallingBearer) -> &mut Lane {
        if bearer == SignallingBearer::ControlAm {
            &mut self.control
        } else {
            &mut self.data
        }
    }
    fn selected_lane_mut(&mut self, id: LaneId) -> &mut Lane {
        match id {
            LaneId::Control => &mut self.control,
            LaneId::Data => &mut self.data,
        }
    }
    pub fn pending(&self, key: UeKey, bearer: SignallingBearer) -> bool {
        let lane = self.lane(bearer);
        lane.ordered.iter().any(|index| {
            let slot = &lane.slots[*index];
            slot.key == Some(key) && slot.bearer == bearer
        })
    }
    pub fn admit(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        count: usize,
        bytes: usize,
    ) -> Result<(), SignallingError> {
        if self
            .next_sequence
            .checked_add(u64::try_from(count).map_err(|_| SignallingError::Capacity)?)
            .is_none()
            || count
                .checked_mul(self.maximum_sdu_bytes)
                .is_none_or(|limit| bytes > limit)
        {
            return Err(SignallingError::Capacity);
        }
        if count == 0 {
            return Ok(());
        }
        let lane = self.lane_mut(bearer);
        if count > lane.slots.len() {
            return Err(SignallingError::Capacity);
        }
        if bearer == SignallingBearer::ControlAm
            && lane.waiting.is_some_and(|waiting| waiting != key)
        {
            return Err(SignallingError::Capacity);
        }
        if count > lane.free.len() {
            if bearer == SignallingBearer::ControlAm && lane.waiting.is_none() {
                lane.waiting = Some(key);
            }
            return Err(SignallingError::Capacity);
        }
        if lane.waiting == Some(key) {
            lane.waiting = None;
        }
        Ok(())
    }
    // Only call after whole-batch admission and validation of every SDU against maximum_sdu_bytes.
    #[cfg(test)]
    pub fn push_admitted(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        now: ChipInstant,
        data: &[u8],
    ) {
        self.push_covered_admitted(key, bearer, now, data, None)
    }
    pub fn push_covered_admitted(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        now: ChipInstant,
        data: &[u8],
        coverage: Option<ReceiveCoverage>,
    ) {
        let received = coverage.map_or(now, ReceiveCoverage::first_received);
        let mut time = ReceiveTime::new(received, now)
            .expect("admitted receive clock")
            .with_provenance(coverage.map_or(
                umts_rlc::acknowledged::ReceiveProvenance::Unspecified,
                ReceiveCoverage::provenance,
            ));
        if let Some(start) = coverage.and_then(ReceiveCoverage::air_start) {
            time = time.with_air_start(start).expect("retained air timestamp");
        }
        self.push_timed_admitted(key, bearer, time, data, coverage)
    }

    pub fn push_timed_admitted(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        time: ReceiveTime,
        data: &[u8],
        coverage: Option<ReceiveCoverage>,
    ) {
        assert!(data.len() <= self.maximum_sdu_bytes);
        let sequence = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("admitted sequence capacity");
        let lane = self.lane_mut(bearer);
        let index = lane.free.pop().expect("admitted ingress slot");
        let slot = &mut lane.slots[index];
        slot.sequence = sequence;
        slot.key = Some(key);
        slot.bearer = bearer;
        slot.time = Some(time);
        slot.length = data.len();
        slot.bytes[..data.len()].copy_from_slice(data);
        slot.coverage = coverage;
        slot.decoded = None;
        lane.ordered.push(index);
    }
    #[cfg(test)]
    pub fn front_eligible(
        &mut self,
        mut eligible: impl FnMut(UeKey, SignallingBearer) -> bool,
    ) -> Option<IngressView<'_>> {
        self.front_eligible_with_protection(|key, bearer, _| eligible(key, bearer))
    }
    pub fn front_eligible_with_protection(
        &mut self,
        mut eligible: impl FnMut(UeKey, SignallingBearer, Option<bool>) -> bool,
    ) -> Option<IngressView<'_>> {
        let control = self.control.first_eligible(&mut eligible);
        let data = self.data.first_eligible(&mut eligible);
        let selected = match (control, data) {
            (Some(control), Some(data))
                if self.control.slots[control].sequence < self.data.slots[data].sequence =>
            {
                Selection {
                    lane: LaneId::Control,
                    index: control,
                }
            }
            (_, Some(data)) => Selection {
                lane: LaneId::Data,
                index: data,
            },
            (Some(control), None) => Selection {
                lane: LaneId::Control,
                index: control,
            },
            (None, None) => return None,
        };
        self.selected = Some(selected);
        let lane = self.selected_lane_mut(selected.lane);
        let slot = &mut lane.slots[selected.index];
        Some(IngressView {
            key: slot.key.expect("occupied ingress owner"),
            bearer: slot.bearer,
            time: slot.time.expect("occupied ingress time"),
            message: slot.decoded.as_ref().expect("decoded ingress"),
            bytes: &slot.bytes[..slot.length],
            coverage: slot.coverage,
        })
    }
    pub fn pop(&mut self) {
        let selected = self.selected.take().expect("selected ingress slot");
        self.selected_lane_mut(selected.lane).remove(selected.index);
    }
    pub fn cancel_for(&mut self, key: UeKey) {
        if self.selected.is_some_and(|selected| {
            self.selected_lane_mut(selected.lane).slots[selected.index].key == Some(key)
        }) {
            self.selected = None;
        }
        self.control.cancel(key);
        self.data.cancel(key);
    }
}

#[cfg(test)]
mod tests;
