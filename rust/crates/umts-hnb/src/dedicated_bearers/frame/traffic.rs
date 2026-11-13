// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_bearers::transmit::SequencedGrant;
use crate::signalling::CommonSignalling;
use umts_dsp::tfci::Tfci;
use umts_rrc::nas::SignallingBearer;

#[cfg(test)]
mod control_capacity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlAmServiceFloor {
    pub period_frames: NonZeroUsize,
    pub pdus_per_period: NonZeroUsize,
    pub pdu_bytes: NonZeroUsize,
}

fn transport_payload_bits(
    block: umts_dsp::coding::CodedBlockFormat,
    binding: &TransportBinding,
) -> Result<usize, BearerError> {
    if block.block_bits == 0 {
        return Ok(0);
    }
    let header = if matches!(binding.channels, TransportChannels::Multiplexed(_)) {
        C_T_BITS
    } else {
        0
    };
    block
        .block_bits
        .checked_sub(header)
        .ok_or(BearerError::Transport)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransparentOffer {
    pub payload_bits_per_block: usize,
    pub blocks: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct TrafficFrame {
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
}

pub trait TransparentTraffic {
    /// Offers must retain the same SDUs until successful publication or explicit retirement.
    fn offer(
        &self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        bearer: BearerId,
    ) -> Option<TransparentOffer>;
    /// Consume source SDUs only if the complete admission callback succeeds.
    fn publish(
        &mut self,
        grant: DownlinkGrant,
        admit: impl FnOnce(&[Bits<'_>]) -> Result<(), FrameBuildError>,
    ) -> Result<(), FrameBuildError>;
}
#[cfg(test)]
pub struct EmptyTransparentTraffic;
#[cfg(test)]
impl TransparentTraffic for EmptyTransparentTraffic {
    fn offer(&self, _: ReceiveIdentity, _: ChipInstant, _: BearerId) -> Option<TransparentOffer> {
        None
    }
    fn publish(
        &mut self,
        _: DownlinkGrant,
        _: impl FnOnce(&[Bits<'_>]) -> Result<(), FrameBuildError>,
    ) -> Result<(), FrameBuildError> {
        Err(BearerError::Transport.into())
    }
}

#[derive(Clone, Copy, Default)]
struct Demand {
    pending: bool,
    transparent: Option<TransparentOffer>,
}
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    control: usize,
    other_signalling: usize,
    user_bits: usize,
    reverse_cost: std::cmp::Reverse<usize>,
}

fn srb(id: BearerId) -> Option<SignallingBearer> {
    [
        SignallingBearer::ControlAm,
        SignallingBearer::Unacknowledged,
        SignallingBearer::NasAm,
    ]
    .into_iter()
    .find(|bearer| bearer.identity() == id.get())
}

impl DedicatedBearers {
    /// Capacity at joint TrCH boundaries, assuming ControlAm remains publication-ready.
    pub fn control_am_service_floor(
        &self,
        identity: ReceiveIdentity,
        maximum_combinations: NonZeroUsize,
    ) -> Result<ControlAmServiceFloor, BearerError> {
        if !matches!(self.phase, BearerPhase::Prepared | BearerPhase::Active) {
            return Err(BearerError::Phase);
        }
        let map = self.maps[self.mapping_index(identity)?]
            .as_ref()
            .expect("registered map");
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        match map.phase {
            MappingPhase::Active | MappingPhase::Draining { .. } if identity == self.identity() => {
            }
            MappingPhase::Staged { previous } | MappingPhase::Scheduled { previous }
                if previous == self.identity() && identity != previous => {}
            _ => return Err(BearerError::Phase),
        }
        if self.downlink_recovery_required || downlink.recovery_required {
            return Err(BearerError::RecoveryRequired);
        }
        let rows = downlink.formats.combinations();
        if rows.len() > maximum_combinations.get() {
            return Err(BearerError::Capacity);
        }
        let control = BearerId::new(SignallingBearer::ControlAm.identity())?;
        let endpoint = &self.endpoints[self.lookup[control.index()].ok_or(BearerError::Mapping)?];
        if endpoint.scope != Scope::Signalling {
            return Err(BearerError::Mapping);
        }
        let pdu_bytes = match &endpoint.entity {
            Entity::Signalling(route)
                if route.mode == CipherMode::Acknowledged
                    && route.transmit_minimum == route.transmit_maximum =>
            {
                route.transmit_minimum
            }
            Entity::Acknowledged(entity) => entity.configuration().transmit_pdu_bytes.get(),
            _ => return Err(BearerError::UnsupportedBinding),
        };
        let pdu_bytes = NonZeroUsize::new(pdu_bytes).ok_or(BearerError::Mapping)?;
        let pdu_bits = pdu_bytes
            .get()
            .checked_mul(BITS_PER_BYTE)
            .ok_or(BearerError::Capacity)?;
        let membership = downlink.members[control.index()];
        if membership == 0 {
            return Err(BearerError::Mapping);
        }
        if membership.count_ones() != 1 {
            return Err(BearerError::UnsupportedBinding);
        }
        let control_index = membership.trailing_zeros() as usize;
        let binding = downlink
            .bindings
            .get(control_index)
            .ok_or(BearerError::Mapping)?;
        let sequenced = |id: BearerId| -> Result<(), BearerError> {
            let endpoint = &self.endpoints[self.lookup[id.index()].ok_or(BearerError::Mapping)?];
            if endpoint.mode() == CipherMode::Transparent {
                return Err(BearerError::UnsupportedBinding);
            }
            Ok(())
        };
        match &binding.channels {
            TransportChannels::Single(id) => sequenced(*id)?,
            TransportChannels::Multiplexed(channels) => {
                for (_, id) in channels {
                    sequenced(*id)?;
                }
            }
        }
        let definition = downlink
            .formats
            .definition(binding.transport)
            .ok_or(BearerError::Mapping)?;
        let mut period = 1;
        for transport in downlink.formats.transport_ids() {
            period = period.max(
                downlink
                    .formats
                    .definition(transport)
                    .ok_or(BearerError::Mapping)?
                    .interval
                    .frames(),
            );
        }
        for block in &definition.formats {
            if block.blocks != 0 && transport_payload_bits(*block, binding)? != pdu_bits {
                return Err(BearerError::Transport);
            }
        }
        let mut rigid = Vec::with_capacity(downlink.bindings.len());
        for binding in &downlink.bindings {
            let transparent = |id: BearerId| -> Result<bool, BearerError> {
                let endpoint =
                    &self.endpoints[self.lookup[id.index()].ok_or(BearerError::Mapping)?];
                Ok(endpoint.mode() == CipherMode::Transparent)
            };
            rigid.push(match &binding.channels {
                TransportChannels::Single(id) => transparent(*id)?,
                TransportChannels::Multiplexed(channels) => {
                    let mut fixed = false;
                    for (_, id) in channels {
                        fixed |= transparent(*id)?;
                    }
                    fixed
                }
            });
        }
        // At the joint TTI boundary packet grants may idle, but transparent offers must still fit.
        let same_group = |left: &[usize], right: &[usize]| {
            left.iter()
                .zip(right)
                .enumerate()
                .all(|(index, (left, right))| !rigid[index] || left == right)
        };
        let optional_idle =
            |row: &[usize]| {
                downlink.formats.transport_ids().zip(row).enumerate().all(
                    |(index, (id, format))| {
                        index == control_index
                            || rigid[index]
                            || downlink
                                .formats
                                .definition(id)
                                .is_some_and(|definition| definition.formats[*format].blocks == 0)
                    },
                )
            };
        let mut floor = usize::MAX;
        for (index, row) in rows.iter().enumerate() {
            if rows[..index]
                .iter()
                .any(|previous| same_group(&previous.formats, &row.formats))
            {
                continue;
            }
            let capacity = rows
                .iter()
                .filter(|candidate| {
                    same_group(&candidate.formats, &row.formats)
                        && optional_idle(&candidate.formats)
                })
                .map(|candidate| definition.formats[candidate.formats[control_index]].blocks)
                .max()
                .unwrap_or(0);
            if capacity == 0 {
                return Err(BearerError::UnsupportedBinding);
            }
            floor = floor.min(capacity);
        }
        Ok(ControlAmServiceFloor {
            period_frames: NonZeroUsize::new(period).ok_or(BearerError::Mapping)?,
            pdus_per_period: NonZeroUsize::new(floor).ok_or(BearerError::UnsupportedBinding)?,
            pdu_bytes,
        })
    }
    #[cfg(test)]
    pub fn select_traffic(
        &mut self,
        start: ChipInstant,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        transparent: &impl TransparentTraffic,
        maximum_combinations: NonZeroUsize,
    ) -> Result<Tfci, FrameBuildError> {
        self.select_traffic_on(
            TrafficFrame {
                identity: self.identity(),
                start,
            },
            now,
            radio,
            transparent,
            maximum_combinations,
        )
    }
    pub fn select_traffic_on(
        &mut self,
        frame: TrafficFrame,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        transparent: &impl TransparentTraffic,
        maximum_combinations: NonZeroUsize,
    ) -> Result<Tfci, FrameBuildError> {
        let TrafficFrame { identity, start } = frame;
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase.into());
        }
        let mapping = self.mapping_index(identity)?;
        let map = self.maps[mapping].as_ref().expect("registered mapping");
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        if start != downlink.next_frame
            || start.epoch != self.owner.epoch
            || now.epoch != self.owner.epoch
        {
            return Err(BearerError::Time.into());
        }
        match map.phase {
            MappingPhase::Active | MappingPhase::Draining { .. } if identity == self.identity() => {
            }
            MappingPhase::Staged { previous } | MappingPhase::Scheduled { previous }
                if previous == self.identity() && identity != previous => {}
            _ => return Err(BearerError::Phase.into()),
        }
        if matches!(map.phase, MappingPhase::Draining { downlink_end, .. }
            if start.index.checked_add(CHIPS_PER_FRAME).is_none_or(|end| end > downlink_end.index))
        {
            return Err(BearerError::Time.into());
        }
        if self.downlink_recovery_required || downlink.recovery_required {
            return Err(BearerError::RecoveryRequired.into());
        }
        if downlink.formats.combinations().len() > maximum_combinations.get() {
            return Err(BearerError::Capacity.into());
        }
        let delta = start
            .index
            .checked_sub(downlink.anchor.time.index)
            .ok_or(BearerError::Time)?;
        if !delta.is_multiple_of(CHIPS_PER_FRAME) {
            return Err(BearerError::Time.into());
        }
        let frame = downlink
            .first_frame
            .checked_add(delta / CHIPS_PER_FRAME)
            .ok_or(BearerError::Time)?;
        let members = downlink.members;
        let reservation = radio.reserved_signalling(identity);
        let mut demand = [Demand::default(); MAX_BEARERS];
        for endpoint in &mut self.endpoints {
            if members[endpoint.id.index()] == 0 || !downlink.uses_dch(endpoint.id, start) {
                continue;
            }
            let target = &mut demand[endpoint.id.index()];
            match &mut endpoint.entity {
                Entity::Signalling(_) => {
                    target.pending = reservation
                        .is_none_or(|reserved| srb(endpoint.id) == Some(reserved))
                        && radio
                            .srb_publication_ready(
                                self.owner,
                                srb(endpoint.id).ok_or(BearerError::Mapping)?,
                                now,
                            )
                            .map_err(FrameBuildError::Signalling)?;
                }
                Entity::Acknowledged(entity) => {
                    target.pending = reservation.is_none()
                        && entity.publication_ready(now).map_err(|error| {
                            FrameBuildError::Acknowledged {
                                bearer: endpoint.id,
                                error,
                                diagnostics: Box::new(entity.diagnostics()),
                            }
                        })?
                }
                Entity::Unacknowledged { transmit, .. } => {
                    target.pending = reservation.is_none() && transmit.has_pending()
                }
                Entity::Transparent { .. } => {
                    target.transparent = transparent.offer(identity, start, endpoint.id)
                }
                Entity::AwaitingUser { .. } => (),
            }
        }
        let map = self.maps[mapping].as_ref().expect("registered mapping");
        let downlink = map.downlink.as_ref().expect("validated downlink");
        let mut best = None;
        for row in downlink.formats.combinations() {
            let mut score = Score::default();
            let mut valid = true;
            let mut assigned = [false; MAX_BEARERS];
            let mut longest = 1;
            for (((transport, &format), previous), binding) in downlink
                .formats
                .transport_ids()
                .zip(&row.formats)
                .zip(&downlink.active_formats)
                .zip(&downlink.bindings)
            {
                let definition = downlink
                    .formats
                    .definition(transport)
                    .expect("canonical transport");
                if !frame.is_multiple_of(definition.interval.frames() as u64) {
                    if *previous != Some(format) {
                        valid = false;
                        break;
                    }
                    continue;
                }
                let block = definition.formats[format];
                if block.blocks != 0 && !downlink.dch_transport_available(transport, start) {
                    valid = false;
                    break;
                }
                if downlink
                    .formats
                    .block_plan(transport, format)
                    .ok_or(BearerError::Mapping)?
                    .coded_bits()
                    != 0
                {
                    longest = longest.max(definition.interval.frames());
                }
                let payload_bits = transport_payload_bits(block, binding)?;
                let mut selected = None;
                let mut candidate = |id: BearerId| -> Result<(), FrameBuildError> {
                    if !downlink.uses_dch(id, start) {
                        return Ok(());
                    }
                    let index = self.lookup[id.index()].ok_or(BearerError::Mapping)?;
                    let endpoint = &self.endpoints[index];
                    let wanted = demand[id.index()];
                    match &endpoint.entity {
                        Entity::AwaitingUser { .. } => (),
                        Entity::Transparent { .. } => {
                            if wanted.transparent
                                == Some(TransparentOffer {
                                    payload_bits_per_block: payload_bits,
                                    blocks: block.blocks,
                                })
                                && payload_bits != 0
                            {
                                if assigned[id.index()] {
                                    return Err(BearerError::UnsupportedBinding.into());
                                }
                                selected = Some(id);
                            }
                        }
                        _ if wanted.pending
                            && payload_bits != 0
                            && block.blocks != 0
                            && selected
                                .is_none_or(|old: BearerId| priority(id) < priority(old)) =>
                        {
                            selected = Some(id);
                        }
                        _ => (),
                    }
                    Ok(())
                };
                match &binding.channels {
                    TransportChannels::Single(id) => candidate(*id)?,
                    TransportChannels::Multiplexed(channels) => {
                        for (_, id) in channels {
                            candidate(*id)?;
                        }
                    }
                }
                if let Some(id) = selected {
                    assigned[id.index()] = true;
                    match srb(id).filter(|_| {
                        self.endpoints[self.lookup[id.index()].unwrap()].scope == Scope::Signalling
                    }) {
                        Some(SignallingBearer::ControlAm) => score.control += block.blocks,
                        Some(_) => score.other_signalling += block.blocks,
                        None => score.user_bits += payload_bits * block.blocks,
                    }
                } else if payload_bits != 0 && block.blocks != 0 {
                    valid = false;
                    break;
                }
                score.reverse_cost.0 += block.block_bits * block.blocks;
            }
            let end = start
                .index
                .checked_add(longest as u64 * CHIPS_PER_FRAME)
                .ok_or(BearerError::Time)?;
            if matches!(map.phase, MappingPhase::Draining { downlink_end, .. } if end > downlink_end.index)
            {
                valid = false;
            }
            if valid
                && self.endpoints.iter().all(|endpoint| {
                    demand[endpoint.id.index()].transparent.is_none()
                        || assigned[endpoint.id.index()]
                })
                && best.as_ref().is_none_or(|(old, _)| score > *old)
            {
                best = Some((score, row.tfci));
            }
        }
        best.map(|(_, tfci)| tfci)
            .ok_or(BearerError::Transport.into())
    }
}

fn priority(id: BearerId) -> (u8, u8) {
    (
        match srb(id) {
            Some(SignallingBearer::ControlAm) => 0,
            Some(_) => 1,
            None => 2,
        },
        id.get(),
    )
}

impl DownlinkFrame<'_> {
    pub fn fill_traffic(
        &mut self,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        source: &mut impl TransparentTraffic,
    ) -> Result<(), FrameBuildError> {
        let mut order = [None; MAX_BEARERS];
        let count = self.bank.endpoints.len();
        for (slot, endpoint) in order.iter_mut().zip(&self.bank.endpoints) {
            *slot = Some(endpoint.id);
        }
        order[..count].sort_unstable_by_key(|id| priority(id.expect("registered bearer")));
        for index in 0..self.slot_count {
            let transport =
                self.input.as_ref().expect("reserved frame").transports[index].transport;
            let members = {
                let mapping = self.bank.maps[self.mapping]
                    .as_ref()
                    .expect("registered map")
                    .downlink
                    .as_ref()
                    .expect("downlink");
                let transport_index = mapping
                    .formats
                    .transport_ids()
                    .position(|candidate| candidate == transport)
                    .ok_or(BearerError::Mapping)?;
                std::array::from_fn::<_, MAX_BEARERS, _>(|index| {
                    mapping.members[index] & (1 << transport_index) != 0
                })
            };
            for id in order[..count].iter().flatten().copied() {
                if self.slots[index].filled == self.slots[index].blocks {
                    break;
                }
                if !members[id.index()] {
                    continue;
                }
                let endpoint = self.bank.lookup[id.index()].ok_or(BearerError::Mapping)?;
                let input = self.input.as_ref().expect("reserved frame");
                if !self.bank.maps[self.mapping]
                    .as_ref()
                    .expect("registered map")
                    .downlink
                    .as_ref()
                    .expect("downlink")
                    .uses_dch(id, input.start)
                {
                    continue;
                }
                let grant = DownlinkGrant {
                    identity: self.identity(),
                    bearer: id,
                    transport,
                    format: self.slots[index].format,
                    start: self.input.as_ref().expect("reserved frame").start,
                };
                let mode = self.bank.endpoints[endpoint].mode();
                let signalling = self.bank.endpoints[endpoint].scope == Scope::Signalling;
                if let Some(reserved) = radio.reserved_signalling(grant.identity)
                    && mode != CipherMode::Transparent
                    && (!signalling || srb(id) != Some(reserved))
                {
                    continue;
                }
                if matches!(
                    self.bank.endpoints[endpoint].entity,
                    Entity::AwaitingUser { .. }
                ) {
                    continue;
                }
                if mode == CipherMode::Transparent {
                    if source.offer(grant.identity, grant.start, id).is_some() {
                        radio
                            .with_bearer_access(grant.identity.ue, |ciphers, _, _| {
                                source.publish(grant, |sdus| {
                                    self.publish_transparent(grant, sdus, ciphers)
                                })
                            })
                            .map_err(FrameBuildError::Signalling)??;
                    }
                    continue;
                }
                while self.slots[index].filled < self.slots[index].blocks {
                    let published = if signalling {
                        self.publish_signalling(grant, now, radio)?
                    } else {
                        radio
                            .with_bearer_access(grant.identity.ue, |ciphers, _, _| {
                                self.publish_sequenced(grant, now, ciphers)
                            })
                            .map_err(FrameBuildError::Signalling)??
                    };
                    if !published {
                        break;
                    }
                }
            }
            if self.slots[index].filled < self.slots[index].blocks {
                self.pad_transport(index, now, radio)?;
            }
        }
        Ok(())
    }
    fn pad_transport(
        &mut self,
        slot_index: usize,
        now: ChipInstant,
        radio: &mut CommonSignalling,
    ) -> Result<(), FrameBuildError> {
        let input = self.input.as_ref().expect("reserved frame");
        let transport = input.transports[slot_index].transport;
        let downlink = self.bank.maps[self.mapping]
            .as_ref()
            .expect("registered map")
            .downlink
            .as_ref()
            .expect("downlink");
        let index = downlink
            .formats
            .transport_ids()
            .position(|id| id == transport)
            .ok_or(BearerError::Mapping)?;
        let endpoint = self
            .bank
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, endpoint)| {
                (endpoint.mode() == CipherMode::Acknowledged
                    || matches!(endpoint.entity, Entity::Unacknowledged { .. }))
                    && downlink.uses_dch(endpoint.id, input.start)
                    && !matches!(endpoint.entity, Entity::AwaitingUser { .. })
                    && downlink.members[endpoint.id.index()] & (1 << index) != 0
            })
            .min_by_key(|(_, endpoint)| u8::from(endpoint.mode() == CipherMode::Unacknowledged))
            .map(|(index, _)| index)
            .ok_or(BearerError::UnsupportedBinding)?;
        let grant = DownlinkGrant {
            identity: input.identity,
            bearer: self.bank.endpoints[endpoint].id,
            transport,
            format: self.slots[slot_index].format,
            start: input.start,
        };
        let resolved = self.bank.grant(grant)?;
        if self.bank.endpoints[endpoint].mode() == CipherMode::Unacknowledged {
            let sequenced = SequencedGrant {
                bearer: grant.bearer,
                payload_bits: resolved.format.block_bits
                    - if resolved.logical.is_some() {
                        C_T_BITS
                    } else {
                        0
                    },
                mapping: resolved.mapping,
                logical: resolved.logical,
            };
            let input = self.input.as_mut().expect("reserved frame");
            let slot = &mut self.slots[slot_index];
            let bank = &mut self.bank;
            radio
                .with_bearer_access(grant.identity.ue, |ciphers, _, _| {
                    while slot.filled < slot.blocks {
                        bank.publish_um_padding(sequenced, now, ciphers, |pdu| {
                            store(
                                &mut input.transports[slot_index],
                                slot,
                                pdu.data,
                                1,
                                pdu.data.len(),
                            )
                        })
                        .map_err(|error| match error {
                            TransmitError::Bearer(error) | TransmitError::Admission(error) => {
                                bank.frame_error(grant.bearer, error)
                            }
                        })?
                        .ok_or(BearerError::Transport)?;
                        self.admitted = true;
                    }
                    Ok::<_, FrameBuildError>(())
                })
                .map_err(FrameBuildError::Signalling)??;
            return Ok(());
        }
        let payload_bits = resolved.format.block_bits
            - if resolved.logical.is_some() {
                C_T_BITS
            } else {
                0
            };
        if !payload_bits.is_multiple_of(BITS_PER_BYTE) {
            return Err(BearerError::Transport.into());
        }
        let endpoint = &mut self.bank.endpoints[endpoint];
        let bytes = payload_bits / BITS_PER_BYTE;
        umts_rlc::acknowledged::write_padding(&mut endpoint.transmit[..bytes])
            .map_err(BearerError::Am)?;
        let bits = umts_mac::encode_dedicated(
            resolved.mapping,
            resolved.logical,
            Bits::new(&endpoint.transmit[..bytes], payload_bits)
                .map_err(|_| BearerError::Transport)?,
            &mut endpoint.mac,
        )
        .map_err(BearerError::Mac)?;
        let payload = Bits::new(&endpoint.mac, bits).expect("encoded padding MAC");
        let input = self.input.as_mut().expect("reserved frame");
        while self.slots[slot_index].filled < self.slots[slot_index].blocks {
            store(
                &mut input.transports[slot_index],
                &mut self.slots[slot_index],
                payload,
                1,
                bits,
            )?;
        }
        Ok(())
    }
}
