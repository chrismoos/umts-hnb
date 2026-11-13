// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rlc::{
    acknowledged::{
        PreparedReceive, PublishError, ReceiveError, ReceivePreparation, ReceiveTime,
        ResetCounters, ResetTransition, WindowPositions,
    },
    transparent::TmdPdu,
    unacknowledged::UmPreparedReceiveError,
};
use umts_security::{
    ciphering::{BearerCipherUpdateError, BearerCiphers, CipherReset, CountAnchor, Position},
    contexts::KeyOrigin,
};
use umts_types::{CnDomain, Direction};

mod tcp_evidence;

const AM_REASSEMBLY_PDUS_PER_VISIT: usize = 64;
const AM_HEADER_PREFIX_BYTES: usize = 2;
const AM_EXTENSION_PREFIX_BYTES: usize = 2;
const PACKET_VERSION_SHIFT: u8 = 4;
const IPV4_VERSION: u8 = 4;
const IPV6_VERSION: u8 = 6;

fn unsupported_packet_version_prefix(
    scope: Scope,
    data: &[u8],
) -> Option<[u8; PACKET_VERSION_PREFIX_BYTES]> {
    if scope != Scope::User(CnDomain::Ps)
        || data
            .first()
            .is_some_and(|byte| matches!(byte >> PACKET_VERSION_SHIFT, IPV4_VERSION | IPV6_VERSION))
    {
        return None;
    }
    let mut prefix = [0; PACKET_VERSION_PREFIX_BYTES];
    let count = prefix.len().min(data.len());
    prefix[..count].copy_from_slice(&data[..count]);
    Some(prefix)
}

fn am_header_failure(raw: &[u8], plaintext: Option<&[u8]>) -> BearerError {
    let extension_prefix = plaintext
        .filter(|_| {
            umts_rlc::acknowledged::parse_amd_header(raw, false)
                .is_ok_and(|header| header.indicators)
        })
        .and_then(|plain| {
            plain.get(AM_HEADER_PREFIX_BYTES..AM_HEADER_PREFIX_BYTES + AM_EXTENSION_PREFIX_BYTES)
        })
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
    BearerError::AmHeader(Box::new(AmHeaderEvidence {
        header: u16::from_be_bytes([raw[0], raw[1]]),
        pdu_bytes: raw.len(),
        prepared: plaintext.is_some(),
        extension_prefix,
    }))
}

#[cfg(test)]
mod header_evidence_tests {
    use super::*;

    #[test]
    fn packet_version_observation_is_packet_only_and_does_not_require_header_decoding() {
        let packet = Scope::User(CnDomain::Ps);
        assert_eq!(
            unsupported_packet_version_prefix(packet, &[0, 5, 0x96]),
            Some([0, 5, 0x96, 0])
        );
        assert_eq!(
            unsupported_packet_version_prefix(packet, &[]),
            Some([0; PACKET_VERSION_PREFIX_BYTES])
        );
        assert_eq!(unsupported_packet_version_prefix(packet, &[0x45]), None);
        assert_eq!(unsupported_packet_version_prefix(packet, &[0x60]), None);
        assert_eq!(
            unsupported_packet_version_prefix(Scope::User(CnDomain::Cs), &[0]),
            None
        );
        assert_eq!(
            unsupported_packet_version_prefix(Scope::Signalling, &[0]),
            None
        );
    }

    #[test]
    fn header_failure_distinguishes_preparation_and_only_records_claimed_extensions() {
        let reserved = [0x80, 0x03, 0x42, 0x43];
        assert_eq!(
            am_header_failure(&reserved, None),
            BearerError::AmHeader(Box::new(AmHeaderEvidence {
                header: 0x8003,
                pdu_bytes: reserved.len(),
                prepared: false,
                extension_prefix: None,
            }))
        );
        let indicators = [0x80, 0x01, 0x7f, 0xff];
        assert_eq!(
            am_header_failure(&indicators, Some(&indicators)),
            BearerError::AmHeader(Box::new(AmHeaderEvidence {
                header: 0x8001,
                pdu_bytes: indicators.len(),
                prepared: true,
                extension_prefix: Some(0x7fff),
            }))
        );
        assert!(matches!(
            am_header_failure(&indicators, None),
            BearerError::AmHeader(evidence) if evidence.extension_prefix.is_none()
        ));
        let data = [0x80, 0x00, 0x42, 0x43];
        assert!(matches!(
            am_header_failure(&data, Some(&data)),
            BearerError::AmHeader(evidence) if evidence.extension_prefix.is_none()
        ));
    }
}

#[derive(Clone, Copy)]
pub enum ResetPermission {
    Defer,
    Settled {
        cs: Option<KeyOrigin>,
        ps: Option<KeyOrigin>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BearerProgress {
    Idle,
    Pdu,
    TransparentUnit,
    AcknowledgedSdus,
    Backpressure,
    Dropped,
}
pub struct SignallingPdu<'a> {
    pub identity: ReceiveIdentity,
    pub bearer: umts_rrc::nas::SignallingBearer,
    pub time: ReceiveTime,
    pub payload: Bits<'a>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum ServiceError<E> {
    Bearer(BearerError),
    Signalling(E),
}
impl<E> From<BearerError> for ServiceError<E> {
    fn from(error: BearerError) -> Self {
        Self::Bearer(error)
    }
}

struct LogicalPdu<'a> {
    identity: ReceiveIdentity,
    bearer: BearerId,
    received: ChipInstant,
    start: ChipInstant,
    payload: Bits<'a>,
}

struct PduReceiver<'a> {
    owner: UeKey,
    endpoint: &'a mut BearerEndpoint,
    output: &'a mut Outputs,
    counters: &'a mut BearerCounters,
}

impl PduReceiver<'_> {
    fn receive<E>(
        self,
        input: LogicalPdu<'_>,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: ResetPermission,
        mut route: impl FnMut(SignallingPdu<'_>, &mut BearerCiphers) -> Result<bool, E>,
    ) -> Result<BearerProgress, ServiceError<E>> {
        let endpoint = self.endpoint;
        let LogicalPdu {
            identity,
            bearer,
            received,
            start,
            payload,
        } = input;
        if matches!(endpoint.entity, Entity::Signalling(_)) {
            let bearer = [
                umts_rrc::nas::SignallingBearer::Unacknowledged,
                umts_rrc::nas::SignallingBearer::ControlAm,
                umts_rrc::nas::SignallingBearer::NasAm,
            ]
            .into_iter()
            .find(|candidate| candidate.identity() == bearer.get())
            .ok_or(BearerError::Mapping)?;
            let time = ReceiveTime::new(received, now)
                .map_err(BearerError::Am)?
                .with_air_start(start)
                .map_err(BearerError::Am)?
                .with_provenance(umts_rlc::acknowledged::ReceiveProvenance::Path(
                    identity.configuration,
                ));
            if !route(
                SignallingPdu {
                    identity,
                    bearer,
                    time,
                    payload,
                },
                ciphers,
            )
            .map_err(ServiceError::Signalling)?
            {
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                return Ok(BearerProgress::Backpressure);
            }
            self.counters.pdus = self.counters.pdus.saturating_add(1);
            return Ok(BearerProgress::Pdu);
        }
        if matches!(endpoint.entity, Entity::AwaitingUser { .. }) {
            self.counters.early_user_pdus = self.counters.early_user_pdus.saturating_add(1);
            return Ok(BearerProgress::Dropped);
        }
        if ciphers
            .layout(self.owner, bearer.get())
            .map_err(BearerError::Cipher)?
            != (endpoint.scope, endpoint.mode())
        {
            return Err(BearerError::Mapping.into());
        }
        let metadata = UpperSdu {
            identity,
            bearer,
            scope: endpoint.scope,
            received,
            bits: 0,
            quality: Quality::NotReported,
            coverage: None,
            transparent_origin: None,
        };
        match &mut endpoint.entity {
            Entity::AwaitingUser { .. } => {
                unreachable!("uninitialized user data rejected before cipher selection")
            }
            Entity::Signalling(_) => unreachable!("signaling routed before user RLC"),
            Entity::Acknowledged(entity) => {
                let bytes = payload.len().div_ceil(BITS_PER_BYTE);
                if payload.len() != entity.configuration().receive_pdu_bytes.get() * BITS_PER_BYTE {
                    return Err(BearerError::Transport.into());
                }
                payload
                    .copy_to(&mut endpoint.raw[..bytes])
                    .map_err(|_| BearerError::Transport)?;
                let positions = entity.window_positions();
                let time = ReceiveTime::new(received, now)
                    .map_err(BearerError::Am)?
                    .with_provenance(umts_rlc::acknowledged::ReceiveProvenance::Path(
                        identity.configuration,
                    ));
                let raw = &endpoint.raw[..bytes];
                let plain = &mut endpoint.plain[..bytes];
                let mut prepared = false;
                let received = entity.receive_prepared_timed(time, raw, |preparation| {
                    plain.copy_from_slice(raw);
                    let reset_counters = match preparation {
                        ReceivePreparation::Data { position, .. } => {
                            ciphers.apply(
                                self.owner,
                                bearer.get(),
                                Direction::Uplink,
                                Position::Sequence(position),
                                plain,
                                bytes * BITS_PER_BYTE,
                            )?;
                            None
                        }
                        ReceivePreparation::Control
                        | ReceivePreparation::Reset {
                            transition: None, ..
                        } => None,
                        ReceivePreparation::Reset {
                            transition: Some(transition),
                            ..
                        } => Some(reset_ciphers(
                            ciphers, self.owner, bearer, positions, transition, reset,
                        )?),
                    };
                    prepared = true;
                    Ok::<_, BearerError>(PreparedReceive {
                        plaintext: &*plain,
                        reset_counters,
                    })
                });
                received.map_err(|error| match error {
                    ReceiveError::Rlc(AmError::Header) => {
                        am_header_failure(raw, prepared.then_some(&*plain))
                    }
                    ReceiveError::Rlc(error) => BearerError::Am(error),
                    ReceiveError::Preparation(error) => error,
                })?;
                if endpoint.scope == Scope::User(CnDomain::Ps)
                    && raw.first().is_some_and(|byte| byte & 0x80 == 0)
                    && crate::hsdpa_packet::delivery_trace::control_enabled()
                {
                    tracing::debug!(target: "umts_hnb::delivery_trace",
                        owner = ?self.owner, identity = ?identity, bearer = bearer.get(),
                        time = ?time, before = ?positions, after = ?entity.window_positions(),
                        counters = ?entity.counters(), bytes = raw.len(),
                        hex = %crate::hsdpa_packet::delivery_trace::control_prefix(raw),
                        "Downlink delivery RLC control received");
                }
            }
            Entity::Unacknowledged { receive, .. } => {
                if !payload.len().is_multiple_of(BITS_PER_BYTE)
                    || payload.len() > endpoint.maximum_receive_bits
                {
                    return Err(BearerError::Transport.into());
                }
                let bytes = payload.len() / BITS_PER_BYTE;
                payload
                    .copy_to(&mut endpoint.raw[..bytes])
                    .map_err(|_| BearerError::Transport)?;
                let raw = &endpoint.raw[..bytes];
                let plain = &mut endpoint.plain[..bytes];
                let result = receive.receive_prepared_admitted(
                    raw,
                    |position| {
                        plain.copy_from_slice(raw);
                        ciphers.apply(
                            self.owner,
                            bearer.get(),
                            Direction::Uplink,
                            Position::Sequence(position),
                            plain,
                            bytes * BITS_PER_BYTE,
                        )?;
                        Ok::<_, BearerError>(&*plain)
                    },
                    |count, bytes| {
                        if count > self.output.slots.len() {
                            Err(BearerError::BatchCapacity)
                        } else if self.output.admit(count, bytes) {
                            Ok(())
                        } else {
                            Err(BearerError::Capacity)
                        }
                    },
                );
                let received = match result {
                    Ok(received) => received,
                    Err(UmPreparedReceiveError::Admission(BearerError::Capacity)) => {
                        self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                        return Ok(BearerProgress::Backpressure);
                    }
                    Err(UmPreparedReceiveError::Rlc(error)) => {
                        return Err(BearerError::Um(error).into());
                    }
                    // TS 25.322 10.3 requires discarding an invalid PDU, not its bearer.
                    Err(UmPreparedReceiveError::MalformedPdu(_)) => {
                        return Ok(BearerProgress::Dropped);
                    }
                    Err(
                        UmPreparedReceiveError::Preparation(error)
                        | UmPreparedReceiveError::Admission(error),
                    ) => return Err(error.into()),
                };
                for sdu in received.sdus() {
                    self.output.push(
                        UpperSdu {
                            bits: sdu.len() * BITS_PER_BYTE,
                            ..metadata
                        },
                        Bits::new(sdu, sdu.len() * BITS_PER_BYTE).expect("UM SDU geometry"),
                    );
                    self.counters.sdus = self.counters.sdus.saturating_add(1);
                }
            }
            Entity::Transparent { .. } => {
                return Err(BearerError::UnsupportedBinding.into());
            }
        }
        self.counters.pdus = self.counters.pdus.saturating_add(1);
        Ok(BearerProgress::Pdu)
    }
}

impl DedicatedBearers {
    #[cfg(test)]
    pub fn service_hsupa(
        &mut self,
        configuration: &umts_rrc::hsupa::UplinkActivation,
        packets: &mut crate::hsupa_rx::PacketDelivery,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: ResetPermission,
    ) -> Result<BearerProgress, BearerError> {
        let Some(pdu) = packets.peek(now.index).map_err(BearerError::HsupaPacket)? else {
            return Ok(BearerProgress::Idle);
        };
        let progress = self.receive_hsupa(configuration, pdu, now, ciphers, reset)?;
        if matches!(progress, BearerProgress::Pdu | BearerProgress::Dropped) {
            assert!(
                packets.release(),
                "consumed HSUPA PDU remains retained until RLC acceptance"
            );
        }
        Ok(progress)
    }

    // The serving activation owner must authorize this configuration before routing E-DCH PDUs.
    #[cfg(test)]
    pub fn receive_hsupa(
        &mut self,
        configuration: &umts_rrc::hsupa::UplinkActivation,
        pdu: crate::hsupa_rx::PacketPdu<'_>,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: ResetPermission,
    ) -> Result<BearerProgress, BearerError> {
        self.receive_hsupa_routed(configuration, pdu, now, ciphers, reset, |_, _| {
            Err(BearerError::UnsupportedBinding)
        })
        .map_err(|error| match error {
            ServiceError::Bearer(error) | ServiceError::Signalling(error) => error,
        })
    }

    pub fn receive_hsupa_routed<E>(
        &mut self,
        configuration: &umts_rrc::hsupa::UplinkActivation,
        pdu: crate::hsupa_rx::PacketPdu<'_>,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: ResetPermission,
        route: impl FnMut(SignallingPdu<'_>, &mut BearerCiphers) -> Result<bool, E>,
    ) -> Result<BearerProgress, ServiceError<E>> {
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase.into());
        }
        let identity = pdu.report.identity;
        let map_index = self.mapping_index(identity)?;
        let map = self.maps[map_index].as_ref().expect("registered mapping");
        if matches!(map.phase, MappingPhase::Staged { .. }) {
            return Err(BearerError::Phase.into());
        }
        let activation = configuration.activation().time();
        let interval = configuration.parameters().transport.interval();
        let start = pdu.report.start;
        let received = ChipInstant {
            index: start
                .index
                .checked_add(interval.chips())
                .ok_or(BearerError::Time)?,
            ..start
        };
        let delta = start
            .index
            .checked_sub(map.anchor.time.index)
            .ok_or(BearerError::Time)?;
        if now.epoch != self.owner.epoch
            || start.epoch != now.epoch
            || activation.epoch != now.epoch
            || start.index < activation.index
            || received.index > now.index
            || !delta.is_multiple_of(interval.chips())
            || self.last_service.is_some_and(|old| now.index < old.index)
            || matches!(map.phase, MappingPhase::Draining { uplink_end, .. } if received.index > uplink_end.index)
        {
            return Err(BearerError::Time.into());
        }
        let mapping = configuration
            .mappings()
            .iter()
            .find(|mapping| {
                mapping.uplink.logical() == pdu.logical_channel && mapping.uplink.flow() == pdu.flow
            })
            .ok_or(BearerError::Mapping)?;
        if usize::from(mapping.uplink.pdu_bits()) != pdu.bits.len()
            || pdu.report.feedback != (umts_mac::hsupa::Reception::Ack { deliver: true })
            || pdu.report.outcome != umts_dsp::transport_blocks::BlockOutcome::Decoded
        {
            return Err(BearerError::Transport.into());
        }
        let bearer = BearerId::new(mapping.bearer)?;
        let bound = map.bindings.iter().any(|binding| match &binding.channels {
            TransportChannels::Single(id) => *id == bearer,
            TransportChannels::Multiplexed(channels) => {
                channels.iter().any(|(_, id)| *id == bearer)
            }
        });
        // TS 25.322 discards data for a released entity. Dropping it also lets the E-DCH ingress
        // drain.
        let Some(index) = bound.then(|| self.lookup[bearer.index()]).flatten() else {
            self.counters.unbound_pdus = self.counters.unbound_pdus.saturating_add(1);
            return Ok(BearerProgress::Dropped);
        };
        let endpoint = &mut self.endpoints[index];
        let packet = endpoint.scope == Scope::User(CnDomain::Ps)
            && matches!(
                endpoint.entity,
                Entity::Acknowledged(_) | Entity::Unacknowledged { .. }
            );
        let signalling =
            configuration.maximum_dpdch() == 0 && matches!(endpoint.entity, Entity::Signalling(_));
        if !packet && !signalling {
            return Err(BearerError::UnsupportedBinding.into());
        }
        self.last_service = Some(now);
        PduReceiver {
            owner: self.owner,
            endpoint,
            output: &mut self.output,
            counters: &mut self.counters,
        }
        .receive(
            LogicalPdu {
                identity,
                bearer,
                received,
                start,
                payload: pdu.bits,
            },
            now,
            ciphers,
            reset,
            route,
        )
    }

    #[cfg(test)]
    pub fn service(
        &mut self,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: ResetPermission,
    ) -> Result<BearerProgress, BearerError> {
        self.service_routed(now, ciphers, reset, |_, _| {
            Err(BearerError::UnsupportedBinding)
        })
        .map_err(|error| match error {
            ServiceError::Bearer(error) | ServiceError::Signalling(error) => error,
        })
    }
    pub fn service_routed<E>(
        &mut self,
        now: ChipInstant,
        ciphers: &mut BearerCiphers,
        reset: ResetPermission,
        route: impl FnMut(SignallingPdu<'_>, &mut BearerCiphers) -> Result<bool, E>,
    ) -> Result<BearerProgress, ServiceError<E>> {
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase.into());
        }
        if now.epoch != self.owner.epoch
            || self.last_service.is_some_and(|old| now.index < old.index)
        {
            return Err(BearerError::Time.into());
        }
        self.last_service = Some(now);
        self.delivery_turn = !self.delivery_turn;
        if self.delivery_turn && self.deliver_am()? {
            return Ok(BearerProgress::AcknowledgedSdus);
        }
        if self.pending.is_none() {
            return Ok(if !self.delivery_turn && self.deliver_am()? {
                BearerProgress::AcknowledgedSdus
            } else {
                BearerProgress::Idle
            });
        }
        let pending = self.pending.as_ref().expect("pending transport");
        let identity = pending.decoded.identity;
        let mapping = self.maps[pending.mapping]
            .as_ref()
            .expect("pending mapping");
        if now.index < pending.received.index {
            return Ok(BearerProgress::Idle);
        }
        if pending.decoded.block_bits == 0 {
            self.pending = None;
            return Ok(BearerProgress::Pdu);
        }
        let block = pending.block;
        let input = Bits::new(
            &self.input,
            pending.decoded.block_bits * pending.decoded.blocks,
        )
        .map_err(|_| BearerError::Transport)?;
        let mac = match umts_mac::decode_dedicated(
            mapping.bindings[pending.binding].mapping(),
            input
                .subspan(
                    block * pending.decoded.block_bits..(block + 1) * pending.decoded.block_bits,
                )
                .map_err(|_| BearerError::Transport)?,
        ) {
            Ok(pdu) => pdu,
            Err(_) => {
                self.advance_block();
                self.counters.malformed = self.counters.malformed.saturating_add(1);
                return Ok(BearerProgress::Dropped);
            }
        };
        let bearer = match mapping.bindings[pending.binding].resolve(mac.channel) {
            Ok(bearer) => bearer,
            Err(_) => {
                self.advance_block();
                self.counters.malformed = self.counters.malformed.saturating_add(1);
                return Ok(BearerProgress::Dropped);
            }
        };
        let index = self.lookup[bearer.index()].ok_or(BearerError::Mapping)?;
        let endpoint = &mut self.endpoints[index];
        if matches!(endpoint.entity, Entity::Transparent { .. }) {
            if ciphers
                .layout(self.owner, bearer.get())
                .map_err(BearerError::Cipher)?
                != (endpoint.scope, endpoint.mode())
            {
                return Err(BearerError::Mapping.into());
            }
            return self
                .receive_tm(index, ciphers)
                .map_err(ServiceError::Bearer);
        }
        let result = PduReceiver {
            owner: self.owner,
            endpoint,
            output: &mut self.output,
            counters: &mut self.counters,
        }
        .receive(
            LogicalPdu {
                identity,
                bearer,
                received: pending.received,
                start: pending.decoded.start,
                payload: mac.payload,
            },
            now,
            ciphers,
            reset,
            route,
        )?;
        if matches!(result, BearerProgress::Pdu | BearerProgress::Dropped) {
            self.advance_block();
        }
        Ok(result)
    }
    fn advance_block(&mut self) {
        let pending = self.pending.as_mut().expect("pending transport");
        pending.processed |= 1 << pending.block;
        while pending.block < pending.decoded.blocks
            && pending.processed & (1 << pending.block) != 0
        {
            pending.block += 1;
        }
        if pending.block == pending.decoded.blocks {
            self.pending = None;
        }
    }
    fn deliver_am(&mut self) -> Result<bool, BearerError> {
        let mut consumed = 0;
        let mut idle = 0;
        while consumed < AM_REASSEMBLY_PDUS_PER_VISIT && idle < self.endpoints.len() {
            if self.deliver_am_one()? {
                consumed += 1;
                idle = 0;
            } else {
                idle += 1;
            }
        }
        self.counters.am_reassembled_pdus = self
            .counters
            .am_reassembled_pdus
            .saturating_add(consumed as u64);
        if consumed == AM_REASSEMBLY_PDUS_PER_VISIT {
            self.counters.am_reassembly_budget_exhausted = self
                .counters
                .am_reassembly_budget_exhausted
                .saturating_add(1);
        }
        Ok(consumed != 0)
    }

    fn deliver_am_one(&mut self) -> Result<bool, BearerError> {
        let default_identity = self.identity();
        let now = self
            .last_service
            .expect("reassembly follows bearer service");
        let index = self.delivery_cursor;
        self.delivery_cursor = (self.delivery_cursor + 1) % self.endpoints.len();
        let endpoint = &mut self.endpoints[index];
        let Entity::Acknowledged(entity) = &mut endpoint.entity else {
            return Ok(false);
        };
        // TS 25.322 11.4.4a requires terminal reset failure to reach the upper layer even without queued data.
        if entity.state() == umts_rlc::acknowledged::AmState::Failed {
            return Err(BearerError::Am(AmError::State));
        }
        let mut sdus = 0;
        let mut maximum_age = 0;
        let inspect_packet = endpoint.scope == Scope::User(CnDomain::Ps)
            && entity.status_history_enabled()
            && self.counters.first_unsupported_packet_version.is_none();
        let mut unsupported_packet = None;
        let inspect_tcp = endpoint.scope == Scope::User(CnDomain::Ps)
            && entity.status_history_enabled()
            && self.counters.first_tcp_gap.is_none();
        let resets = entity.counters().resets;
        let result = entity.deliver_with(|delivery| {
            let bytes = delivery.sdus().map(|sdu| sdu.len()).sum();
            if delivery.count() > self.output.slots.len() {
                return Err(BearerError::BatchCapacity);
            }
            if !self.output.admit(delivery.count(), bytes) {
                return Err(BearerError::Capacity);
            }
            for (sdu, coverage) in delivery.covered_sdus() {
                maximum_age =
                    maximum_age.max(now.index.saturating_sub(coverage.first_received().index));
                let bits = sdu.len() * BITS_PER_BYTE;
                let identity = match coverage.provenance() {
                    umts_rlc::acknowledged::ReceiveProvenance::Path(configuration) => {
                        ReceiveIdentity {
                            configuration,
                            ..default_identity
                        }
                    }
                    _ => default_identity,
                };
                if inspect_packet
                    && unsupported_packet.is_none()
                    && let Some(prefix) = unsupported_packet_version_prefix(endpoint.scope, sdu)
                {
                    unsupported_packet = Some((identity, sdu.len(), prefix, coverage));
                }
                self.output.push(
                    UpperSdu {
                        identity,
                        bearer: endpoint.id,
                        scope: endpoint.scope,
                        received: coverage.first_received(),
                        bits,
                        quality: Quality::NotReported,
                        coverage: Some(coverage),
                        transparent_origin: None,
                    },
                    Bits::new(sdu, bits).expect("AM SDU geometry"),
                );
                if inspect_tcp {
                    tcp_evidence::observe(
                        &mut self.counters,
                        identity,
                        endpoint.id,
                        resets,
                        sdu,
                        coverage,
                    );
                }
                sdus += 1;
            }
            Ok(())
        });
        match result {
            Ok(result) => {
                if let Some((identity, bytes, prefix, coverage)) = unsupported_packet {
                    let first_position = coverage.first_position();
                    let last_position = coverage.last_position();
                    self.counters.first_unsupported_packet_version = Some(PacketVersionEvidence {
                        identity,
                        bearer: endpoint.id,
                        bytes,
                        prefix,
                        first_position,
                        last_position,
                        preceding: first_position
                            .checked_sub(1)
                            .and_then(|position| entity.retained_pdu_evidence(position)),
                        first: entity.retained_pdu_evidence(first_position),
                        last: entity.retained_pdu_evidence(last_position),
                    });
                }
                self.counters.sdus = self.counters.sdus.saturating_add(sdus);
                self.counters.maximum_am_delivery_age_chips =
                    self.counters.maximum_am_delivery_age_chips.max(maximum_age);
                Ok(result.is_some())
            }
            Err(PublishError::Downstream(BearerError::Capacity)) => {
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                Ok(false)
            }
            Err(PublishError::Rlc(AmError::Header)) => Err(entity
                .delivery_evidence()
                .map(|evidence| BearerError::AmDelivery(Box::new(evidence)))
                .unwrap_or(BearerError::Am(AmError::Header))),
            Err(PublishError::Rlc(error)) => Err(BearerError::Am(error)),
            Err(PublishError::Downstream(error)) => Err(error),
        }
    }
    fn receive_tm(
        &mut self,
        index: usize,
        ciphers: &BearerCiphers,
    ) -> Result<BearerProgress, BearerError> {
        let pending = self.pending.as_ref().expect("pending TM transport");
        let mapping = self.maps[pending.mapping]
            .as_ref()
            .expect("pending mapping");
        let endpoint = &mut self.endpoints[index];
        let Entity::Transparent { receive, settings } = &mut endpoint.entity else {
            return Err(BearerError::Mapping);
        };
        let binding = &mapping.bindings[pending.binding];
        let input = Bits::new(
            &self.input,
            pending.decoded.blocks * pending.decoded.block_bits,
        )
        .map_err(|_| BearerError::Transport)?;
        let mut count = 0;
        let mut bits = 0;
        let mut width = None;
        let mut processed = 0_u64;
        endpoint.plain.fill(0);
        for block in pending.block..pending.decoded.blocks {
            let Ok(pdu) = umts_mac::decode_dedicated(
                binding.mapping(),
                input
                    .subspan(
                        block * pending.decoded.block_bits
                            ..(block + 1) * pending.decoded.block_bits,
                    )
                    .map_err(|_| BearerError::Transport)?,
            ) else {
                continue;
            };
            if binding.resolve(pdu.channel).ok() != Some(endpoint.id) {
                continue;
            }
            if width.is_some_and(|old| old != pdu.payload.len())
                || count == settings.limits.maximum_pdus.get()
            {
                return Err(BearerError::Transport);
            }
            width = Some(pdu.payload.len());
            pdu.payload
                .copy_into(&mut endpoint.plain, bits)
                .map_err(|_| BearerError::Capacity)?;
            bits += pdu.payload.len();
            count += 1;
            processed |= 1 << block;
        }
        let width = width.ok_or(BearerError::Transport)?;
        let outputs = if settings.uplink_segmentation == Segmentation::Enabled {
            1
        } else {
            count
        };
        let bytes = if settings.uplink_segmentation == Segmentation::Enabled {
            bits.div_ceil(BITS_PER_BYTE)
        } else {
            count * width.div_ceil(BITS_PER_BYTE)
        };
        if outputs > self.output.slots.len() {
            return Err(BearerError::BatchCapacity);
        }
        if !self.output.admit(outputs, bytes) {
            self.counters.backpressure = self.counters.backpressure.saturating_add(1);
            return Ok(BearerProgress::Backpressure);
        }
        let frame = mapping
            .first_frame
            .checked_add(
                (pending.decoded.start.index - mapping.anchor.time.index) / CHIPS_PER_FRAME,
            )
            .ok_or(BearerError::Time)?;
        if endpoint.next_tm_receive.is_some_and(|next| frame < next) {
            return Err(BearerError::Time);
        }
        let next_frame = frame
            .checked_add(
                mapping
                    .uplink
                    .definition(pending.decoded.transport)
                    .expect("configured TM transport")
                    .interval
                    .frames() as u64,
            )
            .ok_or(BearerError::Time)?;
        // TS 25.321 11.5 ciphers the concatenation of this RB's MAC SDUs once, in transport-block order.
        ciphers.apply(
            self.owner,
            endpoint.id.get(),
            umts_types::Direction::Uplink,
            Position::Frame(frame),
            &mut endpoint.plain[..bits.div_ceil(BITS_PER_BYTE)],
            bits,
        )?;
        let plain = Bits::new(&endpoint.plain, bits).map_err(|_| BearerError::Transport)?;
        let mut pdus = [TmdPdu {
            data: Bits::empty(),
            erroneous: false,
        }; MAX_BLOCKS];
        for (index, pdu) in pdus.iter_mut().enumerate().take(count) {
            pdu.data = plain
                .subspan(index * width..(index + 1) * width)
                .map_err(|_| BearerError::Transport)?;
        }
        let batch = receive.receive(&pdus[..count]).map_err(BearerError::Tm)?;
        for (index, sdu) in batch.enumerate() {
            let quality = if pending.decoded.crc_checked {
                sdu.quality
            } else {
                Quality::NotReported
            };
            self.output.push(
                UpperSdu {
                    identity: pending.decoded.identity,
                    bearer: endpoint.id,
                    scope: endpoint.scope,
                    received: pending.received,
                    bits: sdu.data.len(),
                    quality,
                    coverage: None,
                    transparent_origin: Some(TransparentOrigin {
                        start: pending.decoded.start,
                        transport: pending.decoded.transport,
                        format: pending.decoded.format,
                        unit: if settings.uplink_segmentation == Segmentation::Enabled {
                            TransparentUnit::Joined
                        } else {
                            TransparentUnit::Block { index }
                        },
                    }),
                },
                sdu.data,
            );
            self.counters.sdus = self.counters.sdus.saturating_add(1);
        }
        self.pending
            .as_mut()
            .expect("pending TM transport")
            .processed |= processed;
        endpoint.next_tm_receive = Some(next_frame);
        self.advance_block();
        self.counters.tm_units = self.counters.tm_units.saturating_add(1);
        self.counters.pdus = self.counters.pdus.saturating_add(count as u64);
        Ok(BearerProgress::TransparentUnit)
    }
}

fn reset_ciphers(
    ciphers: &mut BearerCiphers,
    key: UeKey,
    bearer: BearerId,
    positions: WindowPositions,
    reset: ResetTransition,
    permission: ResetPermission,
) -> Result<ResetCounters, BearerError> {
    let ResetPermission::Settled { cs, ps } = permission else {
        return Err(BearerError::ResetRecoveryRequired(
            ResetRecoveryReason::AuthorizationDeferred,
        ));
    };
    let tx = ciphers.reset_configuration(
        key,
        bearer.get(),
        Direction::Downlink,
        Position::Sequence(positions.next_transmit),
    )?;
    let rx = ciphers.reset_configuration(
        key,
        bearer.get(),
        Direction::Uplink,
        Position::Sequence(positions.next_receive),
    )?;
    if tx.origin != rx.origin {
        return Err(BearerError::ResetRecoveryRequired(
            ResetRecoveryReason::DirectionOriginsDiffer,
        ));
    }
    for selection in [tx, rx] {
        if selection.initial_count.is_some()
            && selection.origin.is_none_or(|origin| match origin.domain {
                CnDomain::Cs => cs != Some(origin),
                CnDomain::Ps => ps != Some(origin),
            })
        {
            return Err(BearerError::ResetRecoveryRequired(
                ResetRecoveryReason::PendingOriginNotActive,
            ));
        }
    }
    let mode = CipherMode::Acknowledged;
    ciphers
        .reset_with(
            key,
            CipherReset {
                bearer: bearer.get(),
                origin: tx.origin,
                transmit: CountAnchor::new(
                    mode,
                    Position::Sequence(reset.tx_position),
                    tx.initial_count
                        .unwrap_or(reset.tx_hfn << mode.sequence_bits()),
                )
                .map_err(|error| BearerError::Cipher(error.into()))?,
                receive: CountAnchor::new(
                    mode,
                    Position::Sequence(reset.rx_position),
                    rx.initial_count
                        .unwrap_or(reset.rx_hfn << mode.sequence_bits()),
                )
                .map_err(|error| BearerError::Cipher(error.into()))?,
            },
            || Ok::<_, std::convert::Infallible>(()),
        )
        .map_err(|error| match error {
            BearerCipherUpdateError::Cipher(error) => BearerError::Cipher(error),
            BearerCipherUpdateError::Publication(never) => match never {},
        })?;
    Ok(ResetCounters {
        transmit_hfn: tx.initial_count.map(|count| count >> mode.sequence_bits()),
        receive_hfn: rx.initial_count.map(|count| count >> mode.sequence_bits()),
    })
}
