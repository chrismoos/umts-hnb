// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rlc::{
    acknowledged::{PublicationKind, PublishError},
    unacknowledged::UmPublishError,
};
use umts_security::ciphering::{BearerCiphers, CipherUse, Position};
use umts_types::Direction;

pub(super) struct DownlinkBindings {
    pub(super) anchor: FrameAnchor,
    pub(super) first_frame: u64,
    pub(super) formats: Arc<ConfiguredTfcs>,
    pub(super) bindings: Vec<TransportBinding>,
    pub(super) members: [u32; MAX_BEARERS],
    pub(super) next_frame: ChipInstant,
    pub(super) active_formats: Vec<Option<usize>>,
    pub(super) recovery_required: bool,
    pub(super) hsdpa_activation: [Option<ChipInstant>; MAX_BEARERS],
    pub(super) hsdpa: Option<hsdpa::Route>,
}
#[derive(Clone, Copy, Debug)]
pub struct DownlinkGrant {
    pub identity: ReceiveIdentity,
    pub bearer: BearerId,
    pub transport: TransportId,
    pub format: usize,
    pub start: ChipInstant,
}
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct DownlinkPdu<'a> {
    pub grant: DownlinkGrant,
    pub data: Bits<'a>,
    pub block_bits: usize,
    pub blocks: usize,
    pub cipher: CipherUse,
    pub acknowledged_kind: Option<PublicationKind>,
}
#[derive(Clone, Copy)]
pub(super) struct SequencedGrant {
    pub bearer: BearerId,
    pub payload_bits: usize,
    pub mapping: DedicatedMapping,
    pub logical: Option<LogicalChannelId>,
}
pub(super) struct SequencedPdu<'a> {
    pub data: Bits<'a>,
    pub cipher: CipherUse,
    pub acknowledged_kind: Option<PublicationKind>,
}
pub(super) struct ResolvedGrant {
    pub endpoint: usize,
    pub format: umts_dsp::coding::CodedBlockFormat,
    pub mapping: DedicatedMapping,
    pub logical: Option<LogicalChannelId>,
    pub frame: u64,
    pub map_index: usize,
}
#[derive(Debug, PartialEq, Eq)]
pub enum TransmitError<E> {
    Bearer(BearerError),
    Admission(E),
}
impl<E> From<BearerError> for TransmitError<E> {
    fn from(error: BearerError) -> Self {
        Self::Bearer(error)
    }
}

impl DownlinkBindings {
    pub(super) fn build(
        owner: UeKey,
        layout: EndpointLayout<'_>,
        anchor: FrameAnchor,
        first_frame: u64,
        formats: Arc<ConfiguredTfcs>,
        bindings: Vec<TransportBinding>,
    ) -> Result<Self, BearerError> {
        if anchor.time.epoch != owner.epoch
            || first_frame % CONNECTION_FRAME_MODULUS != u64::from(anchor.cfn)
        {
            return Err(BearerError::Time);
        }
        if formats.transport_count() != bindings.len() || bindings.len() > MAX_TRANSPORTS {
            return Err(BearerError::Mapping);
        }
        let mut members = [0_u32; MAX_BEARERS];
        for (index, (binding, transport)) in
            bindings.iter().zip(formats.transport_ids()).enumerate()
        {
            if binding.transport != transport {
                return Err(BearerError::Mapping);
            }
            let channels: Vec<_> = match &binding.channels {
                TransportChannels::Single(bearer) => vec![(None, *bearer)],
                TransportChannels::Multiplexed(channels)
                    if !channels.is_empty() && channels.len() <= MAX_MULTIPLEXED_CHANNELS =>
                {
                    channels
                        .iter()
                        .map(|(channel, bearer)| (Some(*channel), *bearer))
                        .collect()
                }
                _ => return Err(BearerError::Mapping),
            };
            for (at, (channel, bearer)) in channels.iter().enumerate() {
                if channels[..at]
                    .iter()
                    .any(|(old, id)| old == channel || id == bearer)
                {
                    return Err(BearerError::UnsupportedBinding);
                }
                let endpoint = layout.endpoint(*bearer)?;
                if members[bearer.index()] != 0
                    && matches!(endpoint.entity, Entity::Transparent { .. })
                {
                    return Err(BearerError::UnsupportedBinding);
                }
                for format in &formats
                    .definition(transport)
                    .expect("configured downlink transport")
                    .formats
                {
                    if format.blocks > MAX_BLOCKS {
                        return Err(BearerError::Capacity);
                    }
                    if format.blocks == 0 || format.block_bits == 0 {
                        continue;
                    }
                    let bits = format
                        .block_bits
                        .checked_sub(if channel.is_some() { C_T_BITS } else { 0 })
                        .filter(|v| *v != 0)
                        .ok_or(BearerError::Transport)?;
                    match &endpoint.entity {
                        Entity::AwaitingUser { profiles, index }
                            if !Arc::ptr_eq(&formats, profiles.plan().downlink_configured())
                                || bits.div_ceil(BITS_PER_BYTE)
                                    > profiles.transmit_bytes(*index) =>
                        {
                            return Err(BearerError::Mapping);
                        }
                        Entity::Signalling(route)
                            if !bits.is_multiple_of(BITS_PER_BYTE)
                                || !(route.transmit_minimum..=route.transmit_maximum)
                                    .contains(&(bits / BITS_PER_BYTE)) =>
                        {
                            return Err(BearerError::Transport);
                        }
                        Entity::Acknowledged(entity)
                            if bits
                                != entity.configuration().transmit_pdu_bytes.get()
                                    * BITS_PER_BYTE =>
                        {
                            return Err(BearerError::Transport);
                        }
                        Entity::Unacknowledged { transmit, .. }
                            if !bits.is_multiple_of(BITS_PER_BYTE)
                                || !(transmit.minimum_pdu_bytes()
                                    ..=transmit.limits().maximum_pdu_bytes.get())
                                    .contains(&(bits / BITS_PER_BYTE)) =>
                        {
                            return Err(BearerError::Transport);
                        }
                        Entity::Transparent { settings, .. }
                            if bits > settings.limits.maximum_pdu_bits.get()
                                || format.blocks > settings.limits.maximum_pdus.get()
                                || bits.checked_mul(format.blocks).is_none_or(|v| {
                                    v.div_ceil(BITS_PER_BYTE) > endpoint.transmit.len()
                                }) =>
                        {
                            return Err(BearerError::Transport);
                        }
                        _ => {}
                    }
                }
                members[bearer.index()] |= 1 << index;
            }
        }
        let active_formats = vec![None; formats.transport_count()];
        Ok(Self {
            anchor,
            first_frame,
            formats,
            bindings,
            members,
            next_frame: anchor.time,
            active_formats,
            recovery_required: false,
            hsdpa_activation: [None; MAX_BEARERS],
            hsdpa: None,
        })
    }
    pub(super) fn storage_bytes(&self) -> Result<usize, BearerError> {
        mapping::binding_storage(&self.bindings)?
            .checked_add(std::mem::size_of::<Self>())
            .and_then(|bytes| {
                bytes.checked_add(
                    self.active_formats.capacity() * std::mem::size_of::<Option<usize>>(),
                )
            })
            .ok_or(BearerError::Capacity)
    }
}
impl DedicatedBearers {
    pub fn configure_downlink(
        &mut self,
        anchor: FrameAnchor,
        first_frame: u64,
        formats: Arc<ConfiguredTfcs>,
        bindings: Vec<TransportBinding>,
    ) -> Result<(), BearerError> {
        if self.phase != BearerPhase::Prepared || self.current_map().downlink.is_some() {
            return Err(BearerError::Phase);
        }
        let plan = DownlinkBindings::build(
            self.owner,
            EndpointLayout {
                endpoints: &self.endpoints,
                additional: &[],
                lookup: &self.lookup,
            },
            anchor,
            first_frame,
            formats,
            bindings,
        )?;
        let bytes = self
            .storage_bytes
            .checked_add(plan.storage_bytes()?)
            .filter(|bytes| *bytes <= self.maximum_buffer_bytes)
            .ok_or(BearerError::Capacity)?;
        self.maps[self.current]
            .as_mut()
            .expect("current map")
            .downlink = Some(plan);
        self.storage_bytes = bytes;
        Ok(())
    }

    pub fn publish_sequenced<T, E>(
        &mut self,
        grant: DownlinkGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        accept: impl FnOnce(DownlinkPdu<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, TransmitError<E>> {
        let ResolvedGrant {
            format,
            mapping,
            logical,
            ..
        } = self.grant(grant)?;
        self.publish_sequenced_payload(
            SequencedGrant {
                bearer: grant.bearer,
                payload_bits: format.block_bits - if logical.is_some() { C_T_BITS } else { 0 },
                mapping,
                logical,
            },
            now,
            ciphers,
            |pdu| {
                accept(DownlinkPdu {
                    grant,
                    data: pdu.data,
                    blocks: 1,
                    block_bits: pdu.data.len(),
                    cipher: pdu.cipher,
                    acknowledged_kind: pdu.acknowledged_kind,
                })
            },
        )
    }

    pub(super) fn publish_sequenced_payload<T, E>(
        &mut self,
        grant: SequencedGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        accept: impl FnOnce(SequencedPdu<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, TransmitError<E>> {
        self.publish_sequenced_payload_inner(grant, now, ciphers, false, accept)
    }

    pub(super) fn publish_um_padding<T, E>(
        &mut self,
        grant: SequencedGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        accept: impl FnOnce(SequencedPdu<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, TransmitError<E>> {
        self.publish_sequenced_payload_inner(grant, now, ciphers, true, accept)
    }

    fn publish_sequenced_payload_inner<T, E>(
        &mut self,
        grant: SequencedGrant,
        now: ChipInstant,
        ciphers: &BearerCiphers,
        allow_padding: bool,
        accept: impl FnOnce(SequencedPdu<'_>) -> Result<T, E>,
    ) -> Result<Option<T>, TransmitError<E>> {
        let SequencedGrant {
            bearer,
            payload_bits,
            mapping,
            logical,
        } = grant;
        let index = self.lookup[bearer.index()].ok_or(BearerError::Mapping)?;
        let endpoint = &mut self.endpoints[index];
        if matches!(endpoint.entity, Entity::AwaitingUser { .. }) {
            return Err(BearerError::Phase.into());
        }
        if ciphers
            .layout(self.owner, bearer.get())
            .map_err(BearerError::Cipher)?
            != (endpoint.scope, endpoint.mode())
        {
            return Err(BearerError::Mapping.into());
        }
        let key = self.owner;
        let transmit = &mut endpoint.transmit;
        let mac = &mut endpoint.mac;
        let publish = |input: &[u8], position: Option<u64>, kind: Option<PublicationKind>| {
            if input.len().checked_mul(BITS_PER_BYTE) != Some(payload_bits)
                || input.len() > transmit.len()
            {
                return Err(TransmitError::Bearer(BearerError::Transport));
            }
            transmit[..input.len()].copy_from_slice(input);
            let cipher = if let Some(position) = position {
                ciphers
                    .apply(
                        key,
                        bearer.get(),
                        Direction::Downlink,
                        Position::Sequence(position),
                        &mut transmit[..input.len()],
                        payload_bits,
                    )
                    .map_err(|error| TransmitError::Bearer(BearerError::Cipher(error)))?
            } else {
                CipherUse::Control
            };
            let bits = umts_mac::encode_dedicated(
                mapping,
                logical,
                Bits::new(&transmit[..input.len()], payload_bits)
                    .map_err(|_| TransmitError::Bearer(BearerError::Transport))?,
                mac,
            )
            .map_err(|error| TransmitError::Bearer(BearerError::Mac(error)))?;
            accept(SequencedPdu {
                data: Bits::new(mac, bits).expect("encoded MAC PDU"),
                cipher,
                acknowledged_kind: kind,
            })
            .map_err(TransmitError::Admission)
        };
        match &mut endpoint.entity {
            Entity::Acknowledged(entity) => entity
                .publish_next(now, |publication| {
                    publish(
                        publication.bytes,
                        publication.position,
                        Some(publication.kind),
                    )
                })
                .map(|value| value.map(|(_, value)| value))
                .map_err(|error| match error {
                    PublishError::Rlc(error) => TransmitError::Bearer(BearerError::Am(error)),
                    PublishError::Downstream(error) => error,
                }),
            Entity::Unacknowledged { transmit, .. } => {
                let publish = |publication: umts_rlc::unacknowledged::UmPublication<'_>| {
                    publish(publication.bytes, Some(publication.position), None)
                };
                if allow_padding {
                    transmit
                        .transmit_or_padding_positioned_with(payload_bits / BITS_PER_BYTE, publish)
                } else {
                    transmit.transmit_positioned_with(payload_bits / BITS_PER_BYTE, publish)
                }
                .map_err(|error| match error {
                    UmPublishError::Rlc(error) => TransmitError::Bearer(BearerError::Um(error)),
                    UmPublishError::Downstream(error) => error,
                })
            }
            Entity::AwaitingUser { .. } => Err(BearerError::Phase.into()),
            Entity::Transparent { .. } | Entity::Signalling(_) => Err(BearerError::Mapping.into()),
        }
    }

    // The admission callback owns the whole TM TTI. Failure must not consume the source SDUs.
    pub fn publish_transparent<T, E>(
        &mut self,
        grant: DownlinkGrant,
        sdus: &[Bits<'_>],
        ciphers: &BearerCiphers,
        accept: impl FnOnce(DownlinkPdu<'_>) -> Result<T, E>,
    ) -> Result<T, TransmitError<E>> {
        let ResolvedGrant {
            endpoint: index,
            format,
            logical,
            frame,
            map_index,
            ..
        } = self.grant(grant)?;
        let endpoint = &mut self.endpoints[index];
        if ciphers
            .layout(self.owner, grant.bearer.get())
            .map_err(BearerError::Cipher)?
            != (endpoint.scope, CipherMode::Transparent)
        {
            return Err(BearerError::Mapping.into());
        }
        let Entity::Transparent { settings, .. } = &endpoint.entity else {
            return Err(BearerError::Mapping.into());
        };
        if endpoint.next_tm_transmit.is_some_and(|next| frame < next) {
            return Err(BearerError::Time.into());
        }
        let next_frame = frame
            .checked_add(
                self.maps[map_index]
                    .as_ref()
                    .expect("grant map")
                    .downlink
                    .as_ref()
                    .expect("checked downlink")
                    .formats
                    .definition(grant.transport)
                    .expect("checked transport")
                    .interval
                    .frames() as u64,
            )
            .ok_or(BearerError::Time)?;
        let header = if logical.is_some() { C_T_BITS } else { 0 };
        let width = format.block_bits - header;
        if (settings.downlink_segmentation == Segmentation::Enabled && sdus.len() != 1)
            || (settings.downlink_segmentation == Segmentation::Disabled
                && sdus.len() != format.blocks)
        {
            return Err(BearerError::Transport.into());
        }
        let bits = format
            .blocks
            .checked_mul(width)
            .ok_or(BearerError::Capacity)?;
        endpoint.transmit.fill(0);
        let mut offset = 0;
        for sdu in sdus {
            if sdu.is_empty()
                || sdu.len() > settings.limits.maximum_sdu_bits.get()
                || (settings.downlink_segmentation == Segmentation::Disabled && sdu.len() != width)
            {
                return Err(BearerError::Transport.into());
            }
            sdu.copy_into(&mut endpoint.transmit, offset)
                .map_err(|_| BearerError::Capacity)?;
            offset += sdu.len();
        }
        if offset != bits {
            return Err(BearerError::Transport.into());
        }
        let bytes = bits.div_ceil(BITS_PER_BYTE);
        let cipher = ciphers
            .apply(
                self.owner,
                grant.bearer.get(),
                Direction::Downlink,
                Position::Frame(frame),
                &mut endpoint.transmit[..bytes],
                bits,
            )
            .map_err(BearerError::Cipher)?;
        let payload = Bits::new(&endpoint.transmit[..bytes], bits).expect("TM cipher geometry");
        let total = format.block_bits * format.blocks;
        endpoint.mac[..total.div_ceil(BITS_PER_BYTE)].fill(0);
        for block in 0..format.blocks {
            if let Some(logical) = logical {
                umts_types::bits::write_u32(
                    &mut endpoint.mac,
                    block * format.block_bits,
                    C_T_BITS,
                    u32::from(logical.get() - 1),
                )
                .map_err(|_| BearerError::Transport)?;
            }
            payload
                .subspan(block * width..(block + 1) * width)
                .map_err(|_| BearerError::Transport)?
                .copy_into(&mut endpoint.mac, block * format.block_bits + header)
                .map_err(|_| BearerError::Transport)?;
        }
        let result = accept(DownlinkPdu {
            grant,
            data: Bits::new(&endpoint.mac, total).expect("TM transport geometry"),
            block_bits: format.block_bits,
            blocks: format.blocks,
            cipher,
            acknowledged_kind: None,
        })
        .map_err(TransmitError::Admission)?;
        endpoint.next_tm_transmit = Some(next_frame);
        Ok(result)
    }

    pub(super) fn grant(&self, grant: DownlinkGrant) -> Result<ResolvedGrant, BearerError> {
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase);
        }
        let map_index = self.mapping_index(grant.identity)?;
        let map = self.maps[map_index].as_ref().expect("grant map");
        let plan = map.downlink.as_ref().ok_or(BearerError::Mapping)?;
        if !plan.uses_dch(grant.bearer, grant.start) {
            return Err(BearerError::Mapping);
        }
        if self.downlink_recovery_required || plan.recovery_required {
            return Err(BearerError::RecoveryRequired);
        }
        let endpoint = self.lookup[grant.bearer.index()].ok_or(BearerError::Mapping)?;
        let transport_index = plan
            .bindings
            .iter()
            .position(|binding| binding.transport == grant.transport)
            .ok_or(BearerError::Mapping)?;
        if plan.members[grant.bearer.index()] & (1 << transport_index) == 0 {
            return Err(BearerError::Mapping);
        }
        let binding = &plan.bindings[transport_index];
        let definition = plan
            .formats
            .definition(grant.transport)
            .ok_or(BearerError::Mapping)?;
        let format = *definition
            .formats
            .get(grant.format)
            .ok_or(BearerError::Transport)?;
        if format.blocks == 0 || format.block_bits == 0 {
            return Err(BearerError::Transport);
        }
        let delta = grant
            .start
            .index
            .checked_sub(plan.anchor.time.index)
            .filter(|delta| delta.is_multiple_of(CHIPS_PER_FRAME))
            .ok_or(BearerError::Time)?;
        let frame = plan
            .first_frame
            .checked_add(delta / CHIPS_PER_FRAME)
            .ok_or(BearerError::Time)?;
        if grant.start.epoch != plan.anchor.time.epoch
            || !frame.is_multiple_of(definition.interval.frames() as u64)
            || matches!(map.phase, MappingPhase::Draining { downlink_end, .. } if grant.start.index.checked_add(definition.interval.frames() as u64 * CHIPS_PER_FRAME).is_none_or(|end| end > downlink_end.index))
        {
            return Err(BearerError::Time);
        }
        Ok(ResolvedGrant {
            endpoint,
            format,
            mapping: binding.mapping(),
            logical: binding.logical(grant.bearer)?,
            frame,
            map_index,
        })
    }
}
