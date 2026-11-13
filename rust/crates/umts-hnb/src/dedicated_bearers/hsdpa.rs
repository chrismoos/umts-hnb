// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rlc::acknowledged::PublicationKind;
use umts_rrc::hsdpa::InitialActivation;
use umts_security::ciphering::{BearerCiphers, CipherUse};

#[derive(Clone, Copy, Debug)]
pub struct HsdpaGrant {
    pub identity: ReceiveIdentity,
    pub bearer: BearerId,
    pub start: ChipInstant,
    pub queue: u8,
    pub size_index: u8,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct HsdpaPdu<'a> {
    pub grant: HsdpaGrant,
    pub data: Bits<'a>,
    pub cipher: CipherUse,
    pub acknowledged_kind: Option<PublicationKind>,
}

pub(super) struct Route {
    configuration: Arc<InitialActivation>,
    published: bool,
}

#[derive(Clone)]
pub struct CommittedHsdpa {
    identity: ReceiveIdentity,
    dedicated: Arc<umts_rrc::dedicated::Rel99Configuration>,
    activation: Arc<InitialActivation>,
    active_bearers: [bool; MAX_BEARERS],
}

impl CommittedHsdpa {
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }

    pub fn dedicated(&self) -> &Arc<umts_rrc::dedicated::Rel99Configuration> {
        &self.dedicated
    }

    pub fn activation(&self) -> &Arc<InitialActivation> {
        &self.activation
    }

    pub fn mapping(&self, bearer: u8) -> Option<&umts_rrc::hsdpa::MappedBearer> {
        let id = BearerId::new(bearer).ok()?;
        if !self.active_bearers[id.index()] {
            return None;
        }
        self.activation
            .mappings()
            .iter()
            .find(|mapping| mapping.bearer == bearer)
    }
}

impl transmit::DownlinkBindings {
    pub(super) fn inherit_hsdpa(&mut self, previous: &Self) -> Result<(), BearerError> {
        if self.hsdpa.is_some() || self.hsdpa_activation.iter().any(Option::is_some) {
            return Err(BearerError::Phase);
        }
        let Some(route) = &previous.hsdpa else {
            return Ok(());
        };
        let activation = route.configuration.activation().time();
        if self.anchor.time.index < activation.index || previous.next_frame.index < activation.index
        {
            return Err(BearerError::Time);
        }
        let retained: [Option<ChipInstant>; MAX_BEARERS] = std::array::from_fn(|index| {
            (self.members[index] != 0)
                .then_some(previous.hsdpa_activation[index])
                .flatten()
        });
        if retained.iter().all(Option::is_none) {
            return Ok(());
        }
        let vacated = |binding: &TransportBinding| match &binding.channels {
            TransportChannels::Single(id) => retained[id.index()].is_some(),
            TransportChannels::Multiplexed(channels) => channels
                .iter()
                .all(|(_, id)| retained[id.index()].is_some()),
        };
        if !self.formats.combinations().iter().any(|row| {
            self.bindings
                .iter()
                .zip(&row.formats)
                .all(|(binding, format)| {
                    !vacated(binding)
                        || self
                            .formats
                            .definition(binding.transport)
                            .is_some_and(|definition| definition.formats[*format].blocks == 0)
                })
        }) {
            return Err(BearerError::Transport);
        }
        self.hsdpa_activation = retained;
        self.hsdpa = Some(Route {
            configuration: route.configuration.clone(),
            published: true,
        });
        Ok(())
    }

    pub(super) fn preserves_hsdpa(&self, previous: &Self) -> bool {
        let retains = previous
            .hsdpa_activation
            .iter()
            .enumerate()
            .any(|(index, at)| at.is_some() && self.members[index] != 0);
        if !retains {
            return self.hsdpa.is_none() && self.hsdpa_activation.iter().all(Option::is_none);
        }
        match (&self.hsdpa, &previous.hsdpa) {
            (Some(next), Some(old)) if Arc::ptr_eq(&next.configuration, &old.configuration) => {
                self.hsdpa_activation.iter().enumerate().all(|(index, at)| {
                    *at == if self.members[index] != 0 {
                        previous.hsdpa_activation[index]
                    } else {
                        None
                    }
                })
            }
            _ => false,
        }
    }

    pub(super) fn uses_dch(&self, bearer: BearerId, start: ChipInstant) -> bool {
        self.hsdpa_activation[bearer.index()].is_none_or(|at| start.index < at.index)
    }

    pub(super) fn dch_transport_available(
        &self,
        transport: TransportId,
        start: ChipInstant,
    ) -> bool {
        self.bindings
            .iter()
            .find(|binding| binding.transport == transport)
            .is_some_and(|binding| match &binding.channels {
                TransportChannels::Single(id) => self.uses_dch(*id, start),
                TransportChannels::Multiplexed(channels) => {
                    channels.iter().any(|(_, id)| self.uses_dch(*id, start))
                }
            })
    }
}

impl DedicatedBearers {
    #[cfg(test)]
    pub(crate) fn fixture_publish_hsdpa_route(&mut self) {
        self.maps[self.current]
            .as_mut()
            .unwrap()
            .downlink
            .as_mut()
            .unwrap()
            .hsdpa
            .as_mut()
            .unwrap()
            .published = true;
    }

    #[cfg(test)]
    pub(crate) fn fixture_inherit_hsdpa_route(&mut self, previous: &Self) {
        self.maps[self.current]
            .as_mut()
            .unwrap()
            .downlink
            .as_mut()
            .unwrap()
            .inherit_hsdpa(previous.current_map().downlink.as_ref().unwrap())
            .unwrap();
    }

    pub(crate) fn has_active_hsdpa_bearer(
        &self,
        identity: ReceiveIdentity,
        bearer: BearerId,
    ) -> Result<bool, BearerError> {
        let map = self.current_map();
        if self.phase != BearerPhase::Active
            || map.identity != identity
            || map.phase != mapping::MappingPhase::Active
        {
            return Err(BearerError::Phase);
        }
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        let active = downlink.hsdpa_activation[bearer.index()];
        if downlink.members[bearer.index()] == 0 {
            return if active.is_none() {
                Ok(false)
            } else {
                Err(BearerError::Mapping)
            };
        }
        let Some(activation) = active else {
            return Ok(false);
        };
        let route = downlink.hsdpa.as_ref().ok_or(BearerError::Mapping)?;
        if activation != route.configuration.activation().time()
            || !route
                .configuration
                .mappings()
                .iter()
                .any(|mapping| mapping.bearer == bearer.get())
        {
            return Err(BearerError::Mapping);
        }
        Ok(true)
    }

    /// Snapshot the published link for a subsequent same-cell RRC transaction.
    pub fn retained_hsdpa_configuration(
        &self,
        identity: ReceiveIdentity,
    ) -> Result<Option<Arc<umts_rrc::hsdpa::InitialConfiguration>>, BearerError> {
        let map = self.current_map();
        if self.phase != BearerPhase::Active
            || map.identity != identity
            || map.phase != mapping::MappingPhase::Active
        {
            return Err(BearerError::Phase);
        }
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        let Some(route) = downlink.hsdpa.as_ref().filter(|route| route.published) else {
            return Ok(None);
        };
        let original = route.configuration.configuration();
        let mut retained = [0_u8; MAX_BEARERS];
        let mut count = 0;
        for mapping in original.mappings() {
            if self.has_active_hsdpa_bearer(identity, BearerId::new(mapping.bearer)?)? {
                retained[count] = mapping.bearer;
                count += 1;
            }
        }
        if count == original.mappings().len() {
            return Ok(Some(original.clone()));
        }
        let dedicated = map
            .plan
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration()
            .clone();
        original
            .retained_for(dedicated, &retained[..count])
            .map(|configuration| configuration.map(Arc::new))
            .map_err(|_| BearerError::Mapping)
    }

    // TS 25.331 8.6.6.27 retains the HS link across a same-cell bearer change without a MAC-hs reset.
    pub fn committed_hsdpa(
        &self,
        identity: ReceiveIdentity,
        expected: &Arc<InitialActivation>,
    ) -> Result<CommittedHsdpa, BearerError> {
        let map = self.current_map();
        if self.phase != BearerPhase::Active
            || map.identity != identity
            || map.phase != mapping::MappingPhase::Active
        {
            return Err(BearerError::Phase);
        }
        let dedicated = map
            .plan
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration();
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        let route = downlink.hsdpa.as_ref().ok_or(BearerError::Mapping)?;
        if !Arc::ptr_eq(&route.configuration, expected) {
            return Err(BearerError::Mapping);
        }
        let mut active_bearers = [false; MAX_BEARERS];
        for mapping in expected.mappings() {
            let bearer = BearerId::new(mapping.bearer)?;
            if !self.has_active_hsdpa_bearer(identity, bearer)? {
                continue;
            }
            active_bearers[bearer.index()] = true;
            let previous = expected
                .dedicated()
                .bearer(mapping.bearer)
                .ok_or(BearerError::Mapping)?;
            let current = dedicated
                .bearer(mapping.bearer)
                .ok_or(BearerError::Mapping)?;
            if current.domain != previous.domain || current.rlc != previous.rlc {
                return Err(BearerError::Mapping);
            }
            self.hsdpa_grant(
                identity,
                BearerId::new(mapping.bearer)?,
                downlink.anchor.time,
            )?;
        }
        Ok(CommittedHsdpa {
            identity,
            dedicated: dedicated.clone(),
            activation: route.configuration.clone(),
            active_bearers,
        })
    }

    pub fn hsdpa_grant(
        &self,
        identity: ReceiveIdentity,
        bearer: BearerId,
        start: ChipInstant,
    ) -> Result<HsdpaGrant, BearerError> {
        if self.phase != BearerPhase::Active || identity != self.identity() {
            return Err(BearerError::Phase);
        }
        let configuration = self.hsdpa_route(identity)?.ok_or(BearerError::Mapping)?;
        let mapping = configuration
            .mappings()
            .iter()
            .find(|mapping| mapping.bearer == bearer.get())
            .ok_or(BearerError::Mapping)?;
        let endpoint = &self.endpoints[self.lookup[bearer.index()].ok_or(BearerError::Mapping)?];
        if endpoint.scope != Scope::User(umts_types::CnDomain::Ps) {
            return Err(BearerError::Mapping);
        }
        let fixed_bits = match &endpoint.entity {
            Entity::Acknowledged(entity) => Some(
                entity
                    .configuration()
                    .transmit_pdu_bytes
                    .get()
                    .checked_mul(BITS_PER_BYTE)
                    .ok_or(BearerError::Capacity)?,
            ),
            Entity::Unacknowledged { .. } => None,
            Entity::AwaitingUser { .. } => return Err(BearerError::Phase),
            _ => return Err(BearerError::Mapping),
        };
        let header = if mapping.logical.is_some() && !configuration.configuration().uses_mac_ehs() {
            C_T_BITS
        } else {
            0
        };
        let (queue, size_index, _) = configuration
            .parameters()
            .queues
            .queues()
            .iter()
            .filter(|queue| queue.flow() == mapping.flow)
            .flat_map(|queue| {
                queue
                    .sizes()
                    .iter()
                    .enumerate()
                    .filter_map(move |(index, bits)| {
                        let bits = bits.as_ref()?.get();
                        let payload = bits.checked_sub(header)?;
                        (payload != 0
                            && payload.is_multiple_of(BITS_PER_BYTE)
                            && payload / BITS_PER_BYTE <= endpoint.transmit.len()
                            && bits.div_ceil(BITS_PER_BYTE) <= endpoint.mac.len()
                            && fixed_bits.is_none_or(|fixed| payload == fixed))
                        .then_some((queue.id(), index, payload))
                    })
            })
            .max_by_key(|(_, _, payload)| *payload)
            .ok_or(BearerError::Transport)?;
        Ok(HsdpaGrant {
            identity,
            bearer,
            start,
            queue,
            size_index: u8::try_from(size_index).map_err(|_| BearerError::Transport)?,
        })
    }

    pub fn enqueue_hsdpa(
        &mut self,
        grant: HsdpaGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        queue: &mut umts_mac::hsdpa::PacketQueue,
    ) -> Result<Option<()>, TransmitError<umts_mac::MacError>> {
        if queue.id() != grant.queue {
            return Err(BearerError::Mapping.into());
        }
        self.publish_hsdpa(grant, now, ciphers, |pdu| {
            queue.enqueue(umts_mac::hsdpa::MacdPdu {
                size_index: grant.size_index,
                bits: pdu.data,
            })
        })
    }

    pub(super) fn check_hsdpa_successor(
        &self,
        next: &mapping::BearerMapping,
    ) -> Result<(), BearerError> {
        let old = self
            .current_map()
            .downlink
            .as_ref()
            .ok_or(BearerError::Mapping)?;
        let next = next.downlink.as_ref().ok_or(BearerError::Mapping)?;
        if next.preserves_hsdpa(old) {
            Ok(())
        } else {
            Err(BearerError::Mapping)
        }
    }

    pub fn hsdpa_route(
        &self,
        identity: ReceiveIdentity,
    ) -> Result<Option<&Arc<InitialActivation>>, BearerError> {
        let index = self.mapping_index(identity)?;
        let downlink = self.maps[index]
            .as_ref()
            .and_then(|map| map.downlink.as_ref())
            .ok_or(BearerError::Mapping)?;
        Ok(downlink.hsdpa.as_ref().map(|route| &route.configuration))
    }

    /// Radio and protected RRC publication must reserve this same activation before cutover.
    pub fn reserve_hsdpa_route(
        &mut self,
        identity: ReceiveIdentity,
        configuration: Arc<InitialActivation>,
    ) -> Result<(), BearerError> {
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_ref().ok_or(BearerError::Mapping)?;
        if self.maps.iter().flatten().count() != 1
            || map
                .plan
                .as_ref()
                .is_none_or(|plan| !Arc::ptr_eq(plan.configuration(), configuration.dedicated()))
            || map
                .downlink
                .as_ref()
                .is_none_or(|downlink| downlink.hsdpa.is_some())
        {
            return Err(BearerError::Mapping);
        }
        let mappings = configuration.mappings();
        if mappings.len() > MAX_BEARERS {
            return Err(BearerError::Capacity);
        }
        let ids = mappings
            .iter()
            .map(|mapping| BearerId::new(mapping.bearer))
            .collect::<Result<Vec<_>, _>>()?;
        self.schedule_hsdpa_downlink(identity, configuration.activation().time(), &ids)?;
        self.maps[index]
            .as_mut()
            .expect("reserved mapping")
            .downlink
            .as_mut()
            .expect("reserved downlink")
            .hsdpa = Some(Route {
            configuration,
            published: false,
        });
        Ok(())
    }

    /// Cancel radio/RRC reservations first. Published MAC-d bytes require release, not rollback.
    pub fn cancel_hsdpa_route(
        &mut self,
        identity: ReceiveIdentity,
        now: ChipInstant,
    ) -> Result<Arc<InitialActivation>, BearerError> {
        if self.phase != BearerPhase::Active || identity != self.identity() {
            return Err(BearerError::Phase);
        }
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_mut().ok_or(BearerError::Mapping)?;
        if map.phase != MappingPhase::Active {
            return Err(BearerError::Phase);
        }
        let downlink = map.downlink.as_mut().ok_or(BearerError::Mapping)?;
        let route = downlink.hsdpa.as_ref().ok_or(BearerError::Mapping)?;
        let activation = route.configuration.activation().time();
        if now.epoch != activation.epoch
            || now.index >= activation.index
            || downlink.next_frame.index > activation.index
            || route.published
        {
            return Err(BearerError::Time);
        }
        let route = downlink.hsdpa.take().expect("validated route");
        for mapping in route.configuration.mappings() {
            downlink.hsdpa_activation[BearerId::new(mapping.bearer)
                .expect("admitted bearer")
                .index()] = None;
        }
        Ok(route.configuration)
    }

    /// The admission callback must retain these MAC-d bytes for the whole HARQ lifetime.
    pub fn publish_hsdpa<T, E>(
        &mut self,
        grant: HsdpaGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        accept: impl FnOnce(HsdpaPdu<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, TransmitError<E>> {
        if self.phase != BearerPhase::Active || grant.identity != self.identity() {
            return Err(BearerError::Phase.into());
        }
        let index = self.mapping_index(grant.identity)?;
        let map = self.maps[index].as_ref().ok_or(BearerError::Mapping)?;
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        let configuration = &downlink
            .hsdpa
            .as_ref()
            .ok_or(BearerError::Mapping)?
            .configuration;
        let activation = configuration.activation().time();
        if map.phase != MappingPhase::Active
            || downlink.hsdpa_activation[grant.bearer.index()] != Some(activation)
            || grant.start.epoch != self.owner.epoch
            || now.epoch != self.owner.epoch
            || grant.start.index < activation.index
        {
            return Err(BearerError::Time.into());
        }
        if self.downlink_recovery_required || downlink.recovery_required {
            return Err(BearerError::RecoveryRequired.into());
        }
        let mapping = configuration
            .mappings()
            .iter()
            .find(|mapping| mapping.bearer == grant.bearer.get())
            .ok_or(BearerError::Mapping)?;
        let queue = configuration
            .parameters()
            .queues
            .queues()
            .iter()
            .find(|queue| queue.id() == grant.queue && queue.flow() == mapping.flow)
            .ok_or(BearerError::Mapping)?;
        let bits = queue
            .sizes()
            .get(usize::from(grant.size_index))
            .copied()
            .flatten()
            .ok_or(BearerError::Transport)?
            .get();
        let logical = mapping
            .logical
            .filter(|_| !configuration.configuration().uses_mac_ehs())
            .map(LogicalChannelId::new)
            .transpose()
            .map_err(BearerError::Mac)?;
        let payload_bits = bits
            .checked_sub(if logical.is_some() { C_T_BITS } else { 0 })
            .filter(|bits| *bits != 0 && bits.is_multiple_of(BITS_PER_BYTE))
            .ok_or(BearerError::Transport)?;
        let endpoint =
            &self.endpoints[self.lookup[grant.bearer.index()].ok_or(BearerError::Mapping)?];
        if endpoint.scope != Scope::User(umts_types::CnDomain::Ps)
            || payload_bits / BITS_PER_BYTE > endpoint.transmit.len()
            || bits.div_ceil(BITS_PER_BYTE) > endpoint.mac.len()
        {
            return Err(BearerError::Capacity.into());
        }
        let result = self.publish_sequenced_payload(
            transmit::SequencedGrant {
                bearer: grant.bearer,
                payload_bits,
                mapping: if logical.is_some() {
                    DedicatedMapping::Multiplexed
                } else {
                    DedicatedMapping::Single
                },
                logical,
            },
            now,
            ciphers,
            |pdu| {
                accept(HsdpaPdu {
                    grant,
                    data: pdu.data,
                    cipher: pdu.cipher,
                    acknowledged_kind: pdu.acknowledged_kind,
                })
            },
        );
        if matches!(&result, Ok(Some(_))) {
            self.maps[index]
                .as_mut()
                .expect("published mapping")
                .downlink
                .as_mut()
                .expect("published downlink")
                .hsdpa
                .as_mut()
                .expect("published route")
                .published = true;
        }
        result
    }

    /// Install only after the same activation has been reserved by the RRC and radio owners.
    pub(super) fn schedule_hsdpa_downlink(
        &mut self,
        identity: ReceiveIdentity,
        activation: ChipInstant,
        bearers: &[BearerId],
    ) -> Result<(), BearerError> {
        if self.phase != BearerPhase::Active || identity != self.identity() {
            return Err(BearerError::Phase);
        }
        if bearers.is_empty() || bearers.len() > MAX_BEARERS {
            return Err(BearerError::Mapping);
        }
        let index = self.mapping_index(identity)?;
        let map = self.maps[index].as_ref().ok_or(BearerError::Mapping)?;
        if map.phase != MappingPhase::Active {
            return Err(BearerError::Phase);
        }
        let downlink = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        if activation.epoch != self.owner.epoch || activation.index < downlink.next_frame.index {
            return Err(BearerError::Time);
        }
        let delta = activation
            .index
            .checked_sub(downlink.anchor.time.index)
            .filter(|delta| delta.is_multiple_of(CHIPS_PER_FRAME))
            .ok_or(BearerError::Time)?;
        let frame = downlink
            .first_frame
            .checked_add(delta / CHIPS_PER_FRAME)
            .ok_or(BearerError::Time)?;
        for transport in downlink.formats.transport_ids() {
            let definition = downlink
                .formats
                .definition(transport)
                .ok_or(BearerError::Mapping)?;
            if !frame.is_multiple_of(definition.interval.frames() as u64) {
                return Err(BearerError::Time);
            }
        }
        for (position, bearer) in bearers.iter().enumerate() {
            let endpoint =
                &self.endpoints[self.lookup[bearer.index()].ok_or(BearerError::Mapping)?];
            if bearers[..position].contains(bearer)
                || endpoint.scope != Scope::User(umts_types::CnDomain::Ps)
                || endpoint.mode() == CipherMode::Transparent
                || downlink.members[bearer.index()] == 0
                || downlink.hsdpa_activation[bearer.index()].is_some()
            {
                return Err(BearerError::Mapping);
            }
        }
        let mut vacated_transports = 0_u32;
        // Every vacated TrCH must admit an empty TF in the retained DPCH configuration.
        for (index, binding) in downlink.bindings.iter().enumerate() {
            let vacated = match &binding.channels {
                TransportChannels::Single(id) => {
                    bearers.contains(id) || !downlink.uses_dch(*id, activation)
                }
                TransportChannels::Multiplexed(channels) => channels
                    .iter()
                    .all(|(_, id)| bearers.contains(id) || !downlink.uses_dch(*id, activation)),
            };
            if vacated
                && !downlink
                    .formats
                    .definition(binding.transport)
                    .is_some_and(|definition| {
                        definition.formats.iter().any(|format| format.blocks == 0)
                    })
            {
                return Err(BearerError::Transport);
            }
            if vacated {
                vacated_transports |= 1 << index;
            }
        }
        if !downlink.formats.combinations().iter().any(|row| {
            downlink
                .formats
                .transport_ids()
                .zip(&row.formats)
                .enumerate()
                .all(|(index, (id, format))| {
                    vacated_transports & (1 << index) == 0
                        || downlink
                            .formats
                            .definition(id)
                            .is_some_and(|definition| definition.formats[*format].blocks == 0)
                })
        }) {
            return Err(BearerError::Transport);
        }
        let downlink = self.maps[index]
            .as_mut()
            .expect("validated mapping")
            .downlink
            .as_mut()
            .expect("validated downlink");
        for bearer in bearers {
            downlink.hsdpa_activation[bearer.index()] = Some(activation);
        }
        Ok(())
    }
}
