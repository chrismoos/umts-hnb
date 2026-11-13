// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_rx::{ReceiveCommandResult, ReceiveOwner},
    dedicated_tx::{TransmitCommandOutcome, TransmitPhase},
    rel99_plan::CompiledRel99,
};
use umts_rrc::dedicated::{DchDirection, Rel99Rlc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Rebind,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseOutputPolicy {
    Deliver,
    DiscardReleased,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MappingReleaseError<E> {
    Bearer(BearerError),
    Cleanup(E),
}
#[derive(Debug, PartialEq, Eq)]
pub enum MappingRetirementError<E> {
    Bearer(BearerError),
    Commit(E),
}

#[derive(Debug, PartialEq, Eq)]
pub struct MappingRelease<T> {
    pub cleanup: T,
    pub previous: ReceiveIdentity,
    pub next: ReceiveIdentity,
    pub released_bearers: usize,
    pub discarded_outputs: usize,
    pub discarded_receive_bearers: usize,
    pub released_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct MappingActivation {
    pub uplink: FrameAnchor,
    pub uplink_frame: u64,
    pub downlink: FrameAnchor,
    pub downlink_frame: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingPhase {
    Active,
    Staged {
        previous: ReceiveIdentity,
    },
    Scheduled {
        previous: ReceiveIdentity,
    },
    Draining {
        next: ReceiveIdentity,
        uplink_end: ChipInstant,
        downlink_end: ChipInstant,
    },
}
pub(super) struct BearerMapping {
    pub identity: ReceiveIdentity,
    pub anchor: FrameAnchor,
    pub first_frame: u64,
    pub uplink: Arc<ConfiguredTfcs>,
    pub bindings: Vec<TransportBinding>,
    pub last_transport: Vec<Option<ChipInstant>>,
    pub downlink: Option<transmit::DownlinkBindings>,
    pub plan: Option<Arc<CompiledRel99>>,
    pub phase: MappingPhase,
    pub additional: bool,
    pub added_bearers: Vec<BearerId>,
}
impl BearerMapping {
    pub fn storage_bytes(&self) -> Result<usize, BearerError> {
        binding_storage(&self.bindings)?
            .checked_add(std::mem::size_of::<Self>())
            .and_then(|bytes| {
                bytes.checked_add(
                    self.last_transport.capacity() * std::mem::size_of::<Option<ChipInstant>>(),
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    self.downlink
                        .as_ref()
                        .map_or(Ok(0), |map| map.storage_bytes())
                        .ok()?,
                )
            })
            .and_then(|bytes| {
                self.added_bearers
                    .capacity()
                    .checked_mul(std::mem::size_of::<BearerId>())
                    .and_then(|added_bytes| bytes.checked_add(added_bytes))
            })
            .ok_or(BearerError::Capacity)
    }
}
#[must_use]
pub struct PreparedMapping {
    owner: Arc<()>,
    previous: ReceiveIdentity,
    map: Box<BearerMapping>,
}
impl PreparedMapping {
    pub fn identity(&self) -> ReceiveIdentity {
        self.map.identity
    }
    pub fn storage_bytes(&self) -> Result<usize, BearerError> {
        self.map.storage_bytes()
    }
}
pub struct RejectedMapping {
    pub error: BearerError,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub prepared: PreparedMapping,
}
impl std::fmt::Debug for RejectedMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedMapping")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

pub(super) fn binding_storage(bindings: &Vec<TransportBinding>) -> Result<usize, BearerError> {
    bindings.iter().try_fold(
        bindings.capacity() * std::mem::size_of::<TransportBinding>(),
        |bytes, binding| {
            bytes
                .checked_add(match &binding.channels {
                    TransportChannels::Single(_) => 0,
                    TransportChannels::Multiplexed(channels) => {
                        channels.capacity() * std::mem::size_of::<(LogicalChannelId, BearerId)>()
                    }
                })
                .ok_or(BearerError::Capacity)
        },
    )
}
pub(super) fn validate_uplink(
    uplink: &ConfiguredTfcs,
    bindings: &[TransportBinding],
    layout: EndpointLayout<'_>,
    maximum_bits: usize,
) -> Result<(), BearerError> {
    if bindings.len() != uplink.transport_count() || bindings.len() > MAX_TRANSPORTS {
        return Err(BearerError::Mapping);
    }
    let mut used = [false; MAX_BEARERS];
    for (binding, transport) in bindings.iter().zip(uplink.transport_ids()) {
        if binding.transport != transport {
            return Err(BearerError::Mapping);
        }
        let channels: Vec<_> = match &binding.channels {
            TransportChannels::Single(id) => vec![(None, *id)],
            TransportChannels::Multiplexed(channels)
                if !channels.is_empty() && channels.len() <= MAX_MULTIPLEXED_CHANNELS =>
            {
                channels
                    .iter()
                    .map(|(channel, id)| (Some(*channel), *id))
                    .collect()
            }
            _ => return Err(BearerError::Mapping),
        };
        for (at, (channel, id)) in channels.iter().enumerate() {
            if channels[..at]
                .iter()
                .any(|(old, bearer)| old == channel || bearer == id)
            {
                return Err(BearerError::Mapping);
            }
            let endpoint = layout.endpoint(*id)?;
            if let Entity::AwaitingUser { profiles, .. } = &endpoint.entity
                && !std::ptr::eq(uplink, profiles.plan().uplink_configured().as_ref())
            {
                return Err(BearerError::Mapping);
            }
            if used[id.index()] && matches!(endpoint.entity, Entity::Transparent { .. }) {
                return Err(BearerError::UnsupportedBinding);
            }
            used[id.index()] = true;
            for format in &uplink
                .definition(transport)
                .expect("configured transport")
                .formats
            {
                if format.blocks > MAX_BLOCKS
                    || format
                        .block_bits
                        .checked_mul(format.blocks)
                        .is_none_or(|bits| bits > maximum_bits)
                {
                    return Err(BearerError::Capacity);
                }
                if format.blocks == 0 || format.block_bits == 0 {
                    continue;
                }
                let payload = format
                    .block_bits
                    .checked_sub(if channel.is_some() { C_T_BITS } else { 0 })
                    .filter(|bits| *bits > 0)
                    .ok_or(BearerError::Transport)?;
                match &endpoint.entity {
                    Entity::Signalling(route)
                        if !payload.is_multiple_of(BITS_PER_BYTE)
                            || payload > endpoint.maximum_receive_bits
                            || (route.mode == CipherMode::Acknowledged
                                && payload != endpoint.maximum_receive_bits) =>
                    {
                        return Err(BearerError::Transport);
                    }
                    Entity::Acknowledged(entity)
                        if payload
                            != entity.configuration().receive_pdu_bytes.get() * BITS_PER_BYTE =>
                    {
                        return Err(BearerError::Transport);
                    }
                    Entity::Unacknowledged { .. }
                        if !payload.is_multiple_of(BITS_PER_BYTE)
                            || payload > endpoint.maximum_receive_bits =>
                    {
                        return Err(BearerError::Transport);
                    }
                    Entity::Transparent { settings, .. }
                        if payload > settings.limits.maximum_pdu_bits.get()
                            || format.blocks > settings.limits.maximum_pdus.get()
                            || (settings.uplink_segmentation == Segmentation::Enabled
                                && payload * format.blocks
                                    > settings.limits.maximum_sdu_bits.get())
                            || (settings.uplink_segmentation == Segmentation::Disabled
                                && payload > settings.limits.maximum_sdu_bits.get()) =>
                    {
                        return Err(BearerError::Transport);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
pub(super) fn bindings(
    plan: &CompiledRel99,
    direction: DchDirection,
) -> Result<Vec<TransportBinding>, BearerError> {
    let formats = match direction {
        DchDirection::Uplink => plan.uplink_configured(),
        DchDirection::Downlink => plan.downlink_configured(),
    };
    formats
        .transport_ids()
        .map(|transport| {
            let channels = plan
                .configuration()
                .bearers()
                .iter()
                .flat_map(|bearer| {
                    bearer
                        .mappings(direction)
                        .filter(move |mapping| mapping.transport == transport.get())
                        .map(move |mapping| (mapping.logical, bearer.id))
                })
                .collect::<Vec<_>>();
            let channels = match channels.as_slice() {
                [(None, id)] => TransportChannels::Single(BearerId::new(*id)?),
                _ if !channels.is_empty()
                    && channels.iter().all(|(logical, _)| logical.is_some()) =>
                {
                    TransportChannels::Multiplexed(
                        channels
                            .into_iter()
                            .map(|(logical, id)| {
                                Ok((
                                    LogicalChannelId::new(logical.expect("multiplexed channel"))
                                        .map_err(BearerError::Mac)?,
                                    BearerId::new(id)?,
                                ))
                            })
                            .collect::<Result<_, BearerError>>()?,
                    )
                }
                _ => return Err(BearerError::Mapping),
            };
            Ok(TransportBinding {
                transport,
                channels,
            })
        })
        .collect()
}
pub(super) fn validate_layout(
    plan: &CompiledRel99,
    layout: EndpointLayout<'_>,
    change: Change,
) -> Result<(), BearerError> {
    if change == Change::Rebind && plan.configuration().bearers().len() != layout.len() {
        return Err(BearerError::Mapping);
    }
    for bearer in plan.configuration().bearers() {
        let endpoint = layout.endpoint(BearerId::new(bearer.id)?)?;
        if let Entity::AwaitingUser { profiles, .. } = &endpoint.entity {
            if !std::ptr::eq(plan, profiles.plan().as_ref()) {
                return Err(BearerError::Mapping);
            }
            continue;
        }
        let mode = match &bearer.rlc {
            Rel99Rlc::Transparent {
                uplink_segmentation,
                downlink_segmentation,
            } => {
                let Entity::Transparent { settings, .. } = &endpoint.entity else {
                    return Err(BearerError::Mapping);
                };
                if (settings.uplink_segmentation == Segmentation::Enabled) != *uplink_segmentation
                    || (settings.downlink_segmentation == Segmentation::Enabled)
                        != *downlink_segmentation
                {
                    return Err(BearerError::UnsupportedBinding);
                }
                CipherMode::Transparent
            }
            Rel99Rlc::Unacknowledged => CipherMode::Unacknowledged,
            Rel99Rlc::Acknowledged { uplink, downlink } => {
                if let Entity::Acknowledged(entity) = &endpoint.entity {
                    let config = entity.configuration();
                    if config.receive_window
                        != profile::uplink_window(uplink.transmission_window_size)?
                        || config.transmit_window
                            != profile::downlink_window(downlink.receiving_window_size)?
                        || config.accept_special_he
                    {
                        return Err(BearerError::UnsupportedBinding);
                    }
                }
                CipherMode::Acknowledged
            }
        };
        let scope = bearer.domain.map_or(Scope::Signalling, Scope::User);
        if endpoint.mode() != mode || endpoint.scope != scope {
            return Err(BearerError::Mapping);
        }
    }
    Ok(())
}
pub(super) fn boundary(
    old: FrameAnchor,
    first_frame: u64,
    new: FrameAnchor,
    new_frame: u64,
    longest: u64,
) -> Result<(), BearerError> {
    let delta = new
        .time
        .index
        .checked_sub(old.time.index)
        .filter(|delta| delta.is_multiple_of(CHIPS_PER_FRAME))
        .ok_or(BearerError::Time)?;
    if old.time.epoch != new.time.epoch
        || first_frame.checked_add(delta / CHIPS_PER_FRAME) != Some(new_frame)
        || new_frame % CONNECTION_FRAME_MODULUS != u64::from(new.cfn)
        || !new_frame.is_multiple_of(longest)
    {
        return Err(BearerError::Time);
    }
    Ok(())
}
pub(super) fn longest(a: &ConfiguredTfcs, b: &ConfiguredTfcs) -> u64 {
    a.transport_ids()
        .filter_map(|id| a.definition(id))
        .chain(b.transport_ids().filter_map(|id| b.definition(id)))
        .map(|definition| definition.interval.frames() as u64)
        .max()
        .unwrap_or(1)
}

impl DedicatedBearers {
    pub(super) fn current_map(&self) -> &BearerMapping {
        self.maps[self.current].as_ref().expect("current mapping")
    }
    pub(super) fn mapping_index(&self, identity: ReceiveIdentity) -> Result<usize, BearerError> {
        self.maps
            .iter()
            .position(|map| map.as_ref().is_some_and(|map| map.identity == identity))
            .ok_or(BearerError::Owner)
    }
    pub fn contains_identity(&self, identity: ReceiveIdentity) -> bool {
        self.mapping_index(identity).is_ok()
    }
    pub fn mapping_phase(&self, identity: ReceiveIdentity) -> Result<MappingPhase, BearerError> {
        Ok(self.maps[self.mapping_index(identity)?]
            .as_ref()
            .expect("registered mapping")
            .phase)
    }
    pub(crate) fn mapping_receive_configuration(
        &self,
        identity: ReceiveIdentity,
    ) -> Result<(&Arc<ConfiguredTfcs>, FrameAnchor), BearerError> {
        let map = self.maps[self.mapping_index(identity)?]
            .as_ref()
            .expect("registered mapping");
        Ok((&map.uplink, map.anchor))
    }
    pub fn from_compiled(
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<CompiledRel99>,
        endpoints: Vec<BearerEndpoint>,
        limits: BearerLimits,
    ) -> Result<Self, RejectedBearers> {
        if activation.uplink.cfn != activation.downlink.cfn
            || !activation
                .uplink_frame
                .is_multiple_of(longest(plan.uplink_configured(), plan.uplink_configured()))
            || !activation.downlink_frame.is_multiple_of(longest(
                plan.downlink_configured(),
                plan.downlink_configured(),
            ))
        {
            return Err(RejectedBearers {
                error: BearerError::Time,
                endpoints,
            });
        }
        let uplink = match bindings(&plan, DchDirection::Uplink) {
            Ok(bindings) => bindings,
            Err(error) => return Err(RejectedBearers { error, endpoints }),
        };
        let downlink = match bindings(&plan, DchDirection::Downlink) {
            Ok(bindings) => bindings,
            Err(error) => return Err(RejectedBearers { error, endpoints }),
        };
        let mut bank = Self::new(
            identity,
            activation.uplink,
            activation.uplink_frame,
            plan.uplink_configured().clone(),
            uplink,
            endpoints,
            limits,
        )?;
        let result = validate_layout(
            &plan,
            EndpointLayout {
                endpoints: &bank.endpoints,
                additional: &[],
                lookup: &bank.lookup,
            },
            Change::Rebind,
        )
        .and_then(|()| {
            bank.configure_downlink(
                activation.downlink,
                activation.downlink_frame,
                plan.downlink_configured().clone(),
                downlink,
            )
        });
        if let Err(error) = result {
            return Err(RejectedBearers {
                error,
                endpoints: bank.endpoints,
            });
        }
        bank.maps[bank.current].as_mut().expect("current map").plan = Some(plan);
        Ok(bank)
    }
    #[cfg(test)]
    pub fn prepare_mapping(
        &self,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<CompiledRel99>,
    ) -> Result<PreparedMapping, BearerError> {
        self.prepare_change(identity, activation, plan, Change::Rebind)
    }
    pub fn prepare_release_mapping(
        &self,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<CompiledRel99>,
    ) -> Result<PreparedMapping, BearerError> {
        self.prepare_change(identity, activation, plan, Change::Release)
    }
    fn prepare_change(
        &self,
        identity: ReceiveIdentity,
        activation: MappingActivation,
        plan: Arc<CompiledRel99>,
        change: Change,
    ) -> Result<PreparedMapping, BearerError> {
        if self.downlink_recovery_required {
            return Err(BearerError::RecoveryRequired);
        }
        let old = self.current_map();
        if self.phase != BearerPhase::Active
            || old.phase != MappingPhase::Active
            || self.maps.iter().flatten().count() != 1
        {
            return Err(BearerError::Phase);
        }
        if identity.ue != self.owner || identity.configuration <= old.identity.configuration {
            return Err(BearerError::Owner);
        }
        let previous = old
            .plan
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration();
        let next = plan.configuration();
        if (change == Change::Rebind && previous.bearers().len() != next.bearers().len())
            || (change == Change::Release && previous.bearers().len() <= next.bearers().len())
            || next.bearers().iter().any(|bearer| {
                previous
                    .bearer(bearer.id)
                    .is_none_or(|old| old.domain != bearer.domain || old.rlc != bearer.rlc)
            })
            || previous.bearers().iter().any(|old| {
                next.bearer(old.id).is_none()
                    && (old.id < FIRST_USER_BEARER || old.domain.is_none())
            })
        {
            return Err(BearerError::UnsupportedBinding);
        }
        if previous.physical().default_offset_512_chips != next.physical().default_offset_512_chips
            || previous.physical().frame_offset_256_chips != next.physical().frame_offset_256_chips
        {
            return Err(BearerError::UnsupportedBinding);
        }
        let downlink = old.downlink.as_ref().ok_or(BearerError::Mapping)?;
        // TS 25.212 4.2.14 includes the old and new TrCH TTIs in Fmax.
        boundary(
            old.anchor,
            old.first_frame,
            activation.uplink,
            activation.uplink_frame,
            longest(&old.uplink, plan.uplink_configured()),
        )?;
        boundary(
            downlink.anchor,
            downlink.first_frame,
            activation.downlink,
            activation.downlink_frame,
            longest(&downlink.formats, plan.downlink_configured()),
        )?;
        if activation.uplink.cfn != activation.downlink.cfn
            || activation.uplink_frame.checked_sub(old.first_frame)
                != activation.downlink_frame.checked_sub(downlink.first_frame)
        {
            return Err(BearerError::Time);
        }
        let layout = EndpointLayout {
            endpoints: &self.endpoints,
            additional: &[],
            lookup: &self.lookup,
        };
        validate_layout(&plan, layout, change)?;
        let uplink_bindings = bindings(&plan, DchDirection::Uplink)?;
        validate_uplink(
            plan.uplink_configured(),
            &uplink_bindings,
            layout,
            self.input_bits,
        )?;
        let downlink = transmit::DownlinkBindings::build(
            self.owner,
            layout,
            activation.downlink,
            activation.downlink_frame,
            plan.downlink_configured().clone(),
            bindings(&plan, DchDirection::Downlink)?,
        )?;
        let last_transport = vec![None; uplink_bindings.len()];
        let map = Box::new(BearerMapping {
            identity,
            anchor: activation.uplink,
            first_frame: activation.uplink_frame,
            uplink: plan.uplink_configured().clone(),
            bindings: uplink_bindings,
            last_transport,
            downlink: Some(downlink),
            plan: Some(plan),
            phase: MappingPhase::Staged {
                previous: old.identity,
            },
            additional: false,
            added_bearers: Vec::new(),
        });
        self.storage_bytes
            .checked_add(map.storage_bytes()?)
            .filter(|bytes| *bytes <= self.maximum_buffer_bytes)
            .ok_or(BearerError::Capacity)?;
        let prepared = PreparedMapping {
            owner: self.owner_tag.clone(),
            previous: old.identity,
            map,
        };
        self.check_boundary(&prepared.map)?;
        Ok(prepared)
    }
    pub(super) fn check_boundary(&self, next: &BearerMapping) -> Result<(), BearerError> {
        let old = self.current_map();
        let downlink_end = next
            .downlink
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .anchor
            .time
            .index;
        if old
            .downlink
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .next_frame
            .index
            > downlink_end
        {
            return Err(BearerError::Time);
        }
        for (last, binding) in old.last_transport.iter().zip(&old.bindings) {
            let frames = old
                .uplink
                .definition(binding.transport)
                .expect("registered transport")
                .interval
                .frames() as u64;
            if last.is_some_and(|start| {
                start
                    .index
                    .checked_add(frames * CHIPS_PER_FRAME)
                    .is_none_or(|end| end > next.anchor.time.index)
            }) {
                return Err(BearerError::Time);
            }
        }
        Ok(())
    }
    /// The corresponding RRC construction must retain the serving HS link and MAC-d mappings.
    pub fn retain_hsdpa_for_mapping(
        &self,
        prepared: &mut PreparedMapping,
    ) -> Result<(), BearerError> {
        if !Arc::ptr_eq(&self.owner_tag, &prepared.owner) || self.identity() != prepared.previous {
            return Err(BearerError::Owner);
        }
        prepared
            .map
            .downlink
            .as_mut()
            .ok_or(BearerError::Mapping)?
            .inherit_hsdpa(
                self.current_map()
                    .downlink
                    .as_ref()
                    .ok_or(BearerError::Mapping)?,
            )
    }

    pub fn stage_mapping(
        &mut self,
        prepared: PreparedMapping,
    ) -> Result<ReceiveIdentity, RejectedMapping> {
        let result = (|| {
            if !Arc::ptr_eq(&self.owner_tag, &prepared.owner)
                || self.identity() != prepared.previous
            {
                return Err(BearerError::Owner);
            }
            if self.phase != BearerPhase::Active
                || self.current_map().phase != MappingPhase::Active
                || self.maps.iter().flatten().count() != 1
            {
                return Err(BearerError::Phase);
            }
            self.check_boundary(&prepared.map)?;
            self.check_hsdpa_successor(&prepared.map)?;
            self.storage_bytes
                .checked_add(prepared.map.storage_bytes()?)
                .filter(|bytes| *bytes <= self.maximum_buffer_bytes)
                .ok_or(BearerError::Capacity)
        })();
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(error) => return Err(RejectedMapping { error, prepared }),
        };
        let identity = prepared.identity();
        self.maps[1 - self.current] = Some(prepared.map);
        self.storage_bytes = bytes;
        Ok(identity)
    }
    pub fn commit_mapping(&mut self, identity: ReceiveIdentity) -> Result<(), BearerError> {
        if self.downlink_recovery_required {
            return Err(BearerError::RecoveryRequired);
        }
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_ref().expect("registered mapping");
        if self.phase != BearerPhase::Active
            || map.phase
                != (MappingPhase::Staged {
                    previous: self.identity(),
                })
        {
            return Err(BearerError::Phase);
        }
        self.check_boundary(map)?;
        self.check_hsdpa_successor(map)?;
        let uplink_end = map.anchor.time;
        let downlink_end = map
            .downlink
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .anchor
            .time;
        let previous = self.identity();
        self.maps[self.current].as_mut().expect("current map").phase = MappingPhase::Draining {
            next: identity,
            uplink_end,
            downlink_end,
        };
        self.maps[index].as_mut().expect("staged mapping").phase =
            MappingPhase::Scheduled { previous };
        Ok(())
    }
    /// The caller must separately reclaim worker replacements and resolve any published RRC procedure.
    pub fn cancel_release(
        &mut self,
        previous: ReceiveIdentity,
        identity: ReceiveIdentity,
    ) -> Result<PreparedMapping, BearerError> {
        if self.phase != BearerPhase::Active || self.identity() != previous {
            return Err(BearerError::Owner);
        }
        if self.current_map().phase != MappingPhase::Active
            || self.maps.iter().flatten().count() != 2
        {
            return Err(BearerError::Phase);
        }
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_ref().ok_or(BearerError::Mapping)?;
        if map.phase != (MappingPhase::Staged { previous }) || map.additional {
            return Err(BearerError::Phase);
        }
        let original = self
            .current_map()
            .plan
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration();
        let next = map
            .plan
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration();
        if next.bearers().len() >= original.bearers().len()
            || next.bearers().iter().any(|rb| {
                original
                    .bearer(rb.id)
                    .is_none_or(|old| old.domain != rb.domain || old.rlc != rb.rlc)
            })
            || original.bearers().iter().any(|rb| {
                next.bearer(rb.id).is_none() && (rb.id < FIRST_USER_BEARER || rb.domain.is_none())
            })
        {
            return Err(BearerError::UnsupportedBinding);
        }
        let remaining = self
            .storage_bytes
            .checked_sub(map.storage_bytes()?)
            .ok_or(BearerError::Capacity)?;
        let map = self.maps[index]
            .take()
            .expect("validated staged release mapping");
        self.storage_bytes = remaining;
        Ok(PreparedMapping {
            owner: self.owner_tag.clone(),
            previous,
            map,
        })
    }
    #[cfg(test)]
    pub fn retire_mapping(
        &mut self,
        previous: ReceiveIdentity,
        receive: &ReceiveCommandResult,
        transmit: &TransmitCommandOutcome,
    ) -> Result<(), BearerError> {
        self.retire_mapping_with(previous, receive, transmit, |_| {
            Ok::<_, std::convert::Infallible>(())
        })
        .map_err(|error| match error {
            MappingRetirementError::Bearer(error) => error,
            MappingRetirementError::Commit(never) => match never {},
        })
    }
    pub fn retire_mapping_with<T, E>(
        &mut self,
        previous: ReceiveIdentity,
        receive: &ReceiveCommandResult,
        transmit: &TransmitCommandOutcome,
        commit: impl FnOnce(&Self) -> Result<T, E>,
    ) -> Result<T, MappingRetirementError<E>> {
        let fail = MappingRetirementError::Bearer;
        let new_index = self
            .validate_retirement(previous, receive, transmit)
            .map_err(fail)?;
        let next = self.maps[new_index]
            .as_ref()
            .expect("successor mapping")
            .plan
            .as_ref()
            .ok_or_else(|| fail(BearerError::Mapping))?;
        if self
            .endpoints
            .iter()
            .any(|endpoint| next.configuration().bearer(endpoint.id.get()).is_none())
        {
            return Err(fail(BearerError::UnsupportedBinding));
        }
        let bytes = self.current_map().storage_bytes().map_err(fail)?;
        let result = commit(self).map_err(MappingRetirementError::Commit)?;
        self.finish_retirement(new_index, bytes);
        Ok(result)
    }
    fn validate_retirement(
        &self,
        previous: ReceiveIdentity,
        receive: &ReceiveCommandResult,
        transmit: &TransmitCommandOutcome,
    ) -> Result<usize, BearerError> {
        if self.phase != BearerPhase::Active
            || previous != self.identity()
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.mapping == self.current)
        {
            return Err(BearerError::Phase);
        }
        let MappingPhase::Draining {
            next,
            uplink_end,
            downlink_end,
        } = self.current_map().phase
        else {
            return Err(BearerError::Phase);
        };
        let old = self.current_map();
        let plan = old.plan.as_ref().ok_or(BearerError::Mapping)?;
        let old_downlink = old.downlink.as_ref().ok_or(BearerError::Mapping)?;
        if !matches!(receive, ReceiveCommandResult::Retired { identity, receiver, retirement }
            if *identity == previous && receiver.session().identity() == previous
                && Arc::ptr_eq(receiver.session().configured(), &old.uplink)
                && receiver.session().frame_anchor().time == old.anchor.time
                && receiver.session().frame_anchor().cfn == old.anchor.cfn
                && retirement.previous() == previous && retirement.successor() == next
                && retirement.old_anchor().time == old.anchor.time && retirement.old_anchor().cfn == old.anchor.cfn
                && retirement.boundary() == uplink_end)
            || !matches!(transmit, TransmitCommandOutcome::Retired { identity, result: Ok(retired) }
                if *identity == previous && retired.session.identity() == previous
                    && Arc::ptr_eq(retired.session.plan(), plan) && retired.session.phase() == TransmitPhase::Retired
                    && retired.previous() == previous && retired.successor() == next
                    && retired.old_anchor().time == old_downlink.anchor.time
                    && retired.old_anchor().cfn == old_downlink.anchor.cfn
                    && retired.session.frame_anchor().time == old_downlink.anchor.time
                    && retired.session.frame_anchor().cfn == old_downlink.anchor.cfn
                    && retired.boundary() == downlink_end && retired.submission.resolved())
        {
            return Err(BearerError::Phase);
        }
        let new_index = self.mapping_index(next)?;
        if self.maps[new_index]
            .as_ref()
            .is_none_or(|map| map.phase != (MappingPhase::Scheduled { previous }))
        {
            return Err(BearerError::Phase);
        }
        Ok(new_index)
    }
    fn finish_retirement(&mut self, new_index: usize, bytes: usize) {
        self.maps[self.current] = None;
        self.maps[new_index].as_mut().expect("successor map").phase = MappingPhase::Active;
        self.current = new_index;
        self.storage_bytes -= bytes;
    }
    /// Cleanup must retire exact removed media owners atomically, without changing retained owners on rejection.
    #[cfg(test)]
    pub fn retire_release_mapping_with<T, E>(
        &mut self,
        accepted: &crate::signalling::AcceptedBearerRelease,
        receive: &ReceiveCommandResult,
        transmit: &TransmitCommandOutcome,
        output: ReleaseOutputPolicy,
        ciphers: &mut umts_security::ciphering::BearerCiphers,
        cleanup: impl FnOnce(&[BearerId]) -> Result<T, E>,
    ) -> Result<MappingRelease<T>, MappingReleaseError<E>> {
        self.retire_release_mapping_with_view(
            accepted,
            receive,
            transmit,
            output,
            ciphers,
            |released, _| cleanup(released),
        )
    }
    pub(crate) fn retire_release_mapping_with_view<T, E>(
        &mut self,
        accepted: &crate::signalling::AcceptedBearerRelease,
        receive: &ReceiveCommandResult,
        transmit: &TransmitCommandOutcome,
        output: ReleaseOutputPolicy,
        ciphers: &mut umts_security::ciphering::BearerCiphers,
        cleanup: impl FnOnce(&[BearerId], &DedicatedBearers) -> Result<T, E>,
    ) -> Result<MappingRelease<T>, MappingReleaseError<E>> {
        let fail = MappingReleaseError::Bearer;
        if accepted.key() != self.owner {
            return Err(fail(BearerError::Owner));
        }
        let previous = self.identity();
        let new_index = self
            .validate_retirement(previous, receive, transmit)
            .map_err(fail)?;
        let old = self.current_map();
        let old_plan = old.plan.as_ref().ok_or(fail(BearerError::Mapping))?;
        let new = self.maps[new_index].as_ref().expect("successor mapping");
        let new_plan = new.plan.as_ref().ok_or(fail(BearerError::Mapping))?;
        let downlink = new.downlink.as_ref().ok_or(fail(BearerError::Mapping))?;
        if accepted.previous_identity() != previous
            || accepted.identity() != new.identity
            || !Arc::ptr_eq(accepted.previous(), old_plan.configuration())
            || !Arc::ptr_eq(accepted.configuration(), new_plan.configuration())
            || accepted.activation().time() != downlink.anchor.time
            || accepted.activation().cfn() != new.anchor.cfn
            || accepted.activation().cfn() != downlink.anchor.cfn
        {
            return Err(fail(BearerError::Owner));
        }
        let mut released = [BearerId(FIRST_USER_BEARER); MAX_BEARERS];
        let mut count = 0;
        let mut keep = [false; MAX_BEARERS];
        let mut endpoint_bytes = 0usize;
        let mut pending_receive = 0;
        let mut cipher_ids = [FIRST_USER_BEARER; MAX_BEARERS];
        let mut cipher_count = 0;
        ciphers
            .has_user_bearers(self.owner)
            .map_err(|error| fail(BearerError::Cipher(error)))?;
        for endpoint in &self.endpoints {
            if new_plan.configuration().bearer(endpoint.id.get()).is_some() {
                keep[endpoint.id.index()] = true;
            } else {
                if endpoint.id.get() < FIRST_USER_BEARER
                    || !matches!(endpoint.scope, Scope::User(_))
                {
                    return Err(fail(BearerError::UnsupportedBinding));
                }
                released[count] = endpoint.id;
                count += 1;
                endpoint_bytes = endpoint_bytes
                    .checked_add(endpoint.storage_bytes)
                    .ok_or(fail(BearerError::Capacity))?;
                let pending = endpoint.pending_receive_data();
                pending_receive += usize::from(pending);
                let layout = ciphers.layout(self.owner, endpoint.id.get());
                if matches!(endpoint.entity, Entity::AwaitingUser { .. }) {
                    if layout
                        != Err(umts_security::ciphering::BearerCipherError::Cipher(
                            umts_security::ciphering::Error::Bearer,
                        ))
                    {
                        return Err(fail(BearerError::Mapping));
                    }
                } else {
                    if layout.map_err(|error| fail(BearerError::Cipher(error)))?
                        != (endpoint.scope, endpoint.mode())
                    {
                        return Err(fail(BearerError::Mapping));
                    }
                    cipher_ids[cipher_count] = endpoint.id.get();
                    cipher_count += 1;
                }
            }
        }
        let released = &released[..count];
        let mut expected_released = accepted
            .previous()
            .bearers()
            .iter()
            .filter(|rb| accepted.configuration().bearer(rb.id).is_none());
        if count == 0
            || expected_released.clone().count() != count
            || expected_released.any(|rb| !released.iter().any(|id| id.get() == rb.id))
        {
            return Err(fail(BearerError::Mapping));
        }
        if output == ReleaseOutputPolicy::Deliver
            && (pending_receive != 0
                || self.output.slots.iter().any(|slot| {
                    slot.metadata
                        .is_some_and(|metadata| !keep[metadata.bearer.index()])
                }))
        {
            return Err(fail(BearerError::Capacity));
        }
        let released_bytes = old
            .storage_bytes()
            .map_err(fail)?
            .checked_add(endpoint_bytes)
            .filter(|bytes| *bytes <= self.storage_bytes)
            .ok_or(fail(BearerError::Capacity))?;
        let next = new.identity;
        let cleanup = if cipher_count == 0 {
            cleanup(released, self).map_err(MappingReleaseError::Cleanup)?
        } else {
            ciphers
                .unregister_batch_with(self.owner, &cipher_ids[..cipher_count], || {
                    cleanup(released, self)
                })
                .map_err(|error| match error {
                    umts_security::ciphering::BearerCipherUpdateError::Cipher(error) => {
                        fail(BearerError::Cipher(error))
                    }
                    umts_security::ciphering::BearerCipherUpdateError::Publication(error) => {
                        MappingReleaseError::Cleanup(error)
                    }
                })?
        };
        let discarded_outputs = self.output.discard_except(&keep);
        self.endpoints.retain(|endpoint| keep[endpoint.id.index()]);
        self.lookup.fill(None);
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            self.lookup[endpoint.id.index()] = Some(index);
        }
        self.delivery_cursor = 0;
        self.finish_retirement(new_index, released_bytes);
        self.counters.retired_outputs = self
            .counters
            .retired_outputs
            .saturating_add(discarded_outputs as u64);
        self.counters.released_bearers =
            self.counters.released_bearers.saturating_add(count as u64);
        self.counters.released_receive_bearers = self
            .counters
            .released_receive_bearers
            .saturating_add(pending_receive as u64);
        Ok(MappingRelease {
            cleanup,
            previous,
            next,
            released_bearers: count,
            discarded_outputs,
            discarded_receive_bearers: pending_receive,
            released_bytes,
        })
    }
}
