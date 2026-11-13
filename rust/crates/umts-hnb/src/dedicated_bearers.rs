// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::dedicated_rx::{DecodedTransport, FrameAnchor, ReceiveIdentity, TransportDelivery};
use std::{num::NonZeroUsize, sync::Arc};
use umts_dsp::{
    bits::pack_msb,
    dedicated_transport::{ConfiguredTfcs, TransportId},
};
use umts_mac::{DedicatedMapping, LogicalChannelId};
use umts_rlc::{
    acknowledged::{AmEntity, AmError, ReceiveCoverage},
    transparent::{ErrorDelivery, Quality, Segmentation, TmLimits, TmReceiver},
    unacknowledged::{UmError, UmReceiver, UmTransmitter},
};
use umts_security::{
    ciphering::{BearerCipherError, Scope},
    counters::CipherMode,
};
use umts_types::{
    UeKey,
    bits::{BITS_PER_BYTE, Bits},
    time::{CHIPS_PER_FRAME, CONNECTION_FRAME_MODULUS, ChipInstant},
};

mod addition;
mod frame;
mod handoff;
mod hsdpa;
pub use hsdpa::{CommittedHsdpa, HsdpaGrant};
mod initial;
mod mapping;
mod profile;
pub use addition::{PreparedAddition, RejectedAddition, RejectedAdditionPreparation};
pub use mapping::{
    MappingActivation, MappingPhase, MappingRelease, MappingReleaseError, MappingRetirementError,
    PreparedMapping, RejectedMapping, ReleaseOutputPolicy,
};
pub use profile::{NetworkAmPolicy, UserRlcPolicy, UserRlcProfiles};
mod receive;
#[cfg(test)]
pub use frame::EmptyTransparentTraffic;
pub(crate) use frame::storage_requirements;
pub use frame::{
    ControlAmServiceFloor, DownlinkClock, DownlinkFrame, FrameBuildError, FramePool,
    FramePoolError, FramePoolLimits, RejectedFramePool, TrafficFrame, TransparentOffer,
    TransparentTraffic,
};
mod transmit;
pub use receive::{BearerProgress, ResetPermission, ServiceError, SignallingPdu};
pub use transmit::{DownlinkGrant, TransmitError};

const MAX_BEARERS: usize = 32;
const MAX_TRANSPORTS: usize = 32;
const MAX_MULTIPLEXED_CHANNELS: usize = 15;
pub(crate) const MAX_BLOCKS: usize = 64;
const MAX_OUTPUTS: usize = 256;
const FIRST_USER_BEARER: u8 = 5;
const C_T_BITS: usize = 4;
const PACKET_VERSION_PREFIX_BYTES: usize = 4;
const MAX_TRANSPORT_BITS: usize = 1 << 20;
const MAX_SDU_BYTES: usize = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BearerId(u8);
impl BearerId {
    pub fn new(value: u8) -> Result<Self, BearerError> {
        if !(1..=MAX_BEARERS as u8).contains(&value) {
            return Err(BearerError::Mapping);
        }
        Ok(Self(value))
    }
    pub fn get(self) -> u8 {
        self.0
    }
    fn index(self) -> usize {
        usize::from(self.0 - 1)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BearerError {
    HsupaPacket(crate::hsupa_rx::PacketError),
    Owner,
    Phase,
    Time,
    Capacity,
    BatchCapacity,
    SduSize,
    Mapping,
    Transport,
    RecoveryRequired,
    ResetRecoveryRequired(ResetRecoveryReason),
    UnsupportedBinding,
    MissingStart(umts_types::CnDomain),
    Mac(umts_mac::MacError),
    Cipher(BearerCipherError),
    Am(AmError),
    AmHeader(Box<AmHeaderEvidence>),
    AmDelivery(Box<umts_rlc::acknowledged::AmDeliveryEvidence>),
    Um(UmError),
    Tm(umts_rlc::transparent::TmError),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmHeaderEvidence {
    pub header: u16,
    pub pdu_bytes: usize,
    pub prepared: bool,
    pub extension_prefix: Option<u16>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetRecoveryReason {
    AuthorizationDeferred,
    DirectionOriginsDiffer,
    PendingOriginNotActive,
}
impl From<BearerCipherError> for BearerError {
    fn from(error: BearerCipherError) -> Self {
        Self::Cipher(error)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BearerPhase {
    Prepared,
    Active,
    Retired,
}
#[derive(Clone, Copy)]
pub struct TransparentSettings {
    pub uplink_segmentation: Segmentation,
    pub downlink_segmentation: Segmentation,
    pub errors: ErrorDelivery,
    pub limits: TmLimits,
}
enum Entity {
    AwaitingUser {
        profiles: Arc<UserRlcProfiles>,
        index: usize,
    },
    Signalling(SignallingRoute),
    Acknowledged(Box<AmEntity>),
    Unacknowledged {
        transmit: Box<UmTransmitter>,
        receive: Box<UmReceiver>,
    },
    Transparent {
        receive: Box<TmReceiver>,
        settings: TransparentSettings,
    },
}
#[derive(Clone, Copy)]
pub(crate) struct SignallingRoute {
    pub mode: CipherMode,
    pub receive_bytes: usize,
    pub transmit_minimum: usize,
    pub transmit_maximum: usize,
}
pub struct RlcBuildError<T> {
    pub error: BearerError,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )
    )]
    pub entity: Box<T>,
}
impl<T> std::fmt::Debug for RlcBuildError<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RlcBuildError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
pub struct RejectedBearers {
    pub error: BearerError,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub endpoints: Vec<BearerEndpoint>,
}
impl std::fmt::Debug for RejectedBearers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedBearers")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
pub struct BearerEndpoint {
    ue: UeKey,
    id: BearerId,
    scope: Scope,
    entity: Entity,
    raw: Vec<u8>,
    plain: Vec<u8>,
    transmit: Vec<u8>,
    mac: Vec<u8>,
    maximum_receive_bits: usize,
    maximum_sdu_bytes: usize,
    storage_bytes: usize,
    next_tm_transmit: Option<u64>,
    next_tm_receive: Option<u64>,
}
impl BearerEndpoint {
    pub(crate) fn signalling(
        ue: UeKey,
        id: BearerId,
        route: SignallingRoute,
        maximum_sdu_bytes: usize,
    ) -> Result<Self, BearerError> {
        Self::validate_scope(id, Scope::Signalling)?;
        if id.get() > umts_rrc::nas::SignallingBearer::NasAm.identity()
            || !matches!(
                route.mode,
                CipherMode::Acknowledged | CipherMode::Unacknowledged
            )
            || route.receive_bytes == 0
            || route.transmit_minimum == 0
            || route.transmit_minimum > route.transmit_maximum
            || route.receive_bytes > MAX_TRANSPORT_BITS / BITS_PER_BYTE
            || route.transmit_maximum > MAX_TRANSPORT_BITS / BITS_PER_BYTE
        {
            return Err(BearerError::Mapping);
        }
        Self::new(
            ue,
            id,
            Scope::Signalling,
            Entity::Signalling(route),
            route.receive_bytes * BITS_PER_BYTE,
            route.transmit_maximum,
            maximum_sdu_bytes,
            0,
        )
        .map_err(|(error, _)| error)
    }
    fn validate_scope(id: BearerId, scope: Scope) -> Result<(), BearerError> {
        if matches!(scope, Scope::Signalling) != (id.get() < FIRST_USER_BEARER) {
            return Err(BearerError::Mapping);
        }
        Ok(())
    }
    pub fn acknowledged(
        ue: UeKey,
        id: BearerId,
        scope: Scope,
        entity: AmEntity,
    ) -> Result<Self, RlcBuildError<AmEntity>> {
        if let Err(error) = Self::validate_scope(id, scope) {
            return Err(RlcBuildError {
                error,
                entity: Box::new(entity),
            });
        }
        let configuration = entity.configuration();
        let limits = entity.limits();
        let rx = configuration.receive_pdu_bytes.get();
        let tx = configuration.transmit_pdu_bytes.get();
        let bytes = match configuration.buffer_bytes(limits) {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(RlcBuildError {
                    error: BearerError::Am(error),
                    entity: Box::new(entity),
                });
            }
        };
        Self::new(
            ue,
            id,
            scope,
            Entity::Acknowledged(Box::new(entity)),
            rx * BITS_PER_BYTE,
            tx,
            limits.maximum_sdu_bytes.get(),
            bytes,
        )
        .map_err(|(error, entity)| {
            let Entity::Acknowledged(entity) = entity else {
                unreachable!("AM construction")
            };
            RlcBuildError { error, entity }
        })
    }
    pub fn unacknowledged(
        ue: UeKey,
        id: BearerId,
        scope: Scope,
        transmit: UmTransmitter,
        receive: UmReceiver,
    ) -> Result<Self, RlcBuildError<(UmTransmitter, UmReceiver)>> {
        if let Err(error) = Self::validate_scope(id, scope) {
            return Err(RlcBuildError {
                error,
                entity: Box::new((transmit, receive)),
            });
        }
        let rx = receive.limits();
        let tx = transmit.limits();
        let bytes = (|| {
            rx.receive_buffer_bytes()
                .map_err(BearerError::Um)?
                .checked_add(tx.transmit_buffer_bytes().map_err(BearerError::Um)?)
                .ok_or(BearerError::Capacity)
        })();
        let bytes = match bytes {
            Ok(bytes) => bytes,
            Err(error) => {
                return Err(RlcBuildError {
                    error,
                    entity: Box::new((transmit, receive)),
                });
            }
        };
        Self::new(
            ue,
            id,
            scope,
            Entity::Unacknowledged {
                transmit: Box::new(transmit),
                receive: Box::new(receive),
            },
            rx.maximum_pdu_bytes.get() * BITS_PER_BYTE,
            tx.maximum_pdu_bytes.get(),
            rx.maximum_sdu_bytes.get(),
            bytes,
        )
        .map_err(|(error, entity)| {
            let Entity::Unacknowledged { transmit, receive } = entity else {
                unreachable!("UM construction")
            };
            RlcBuildError {
                error,
                entity: Box::new((*transmit, *receive)),
            }
        })
    }
    pub fn transparent(
        ue: UeKey,
        id: BearerId,
        scope: Scope,
        settings: TransparentSettings,
    ) -> Result<Self, BearerError> {
        Self::validate_scope(id, scope)?;
        if scope == Scope::Signalling || settings.limits.maximum_pdus.get() > MAX_BLOCKS {
            return Err(BearerError::Mapping);
        }
        if settings.limits.maximum_sdu_bits.get() > umts_security::algorithms::MAX_F8_BITS {
            return Err(BearerError::Capacity);
        }
        let bits = settings
            .limits
            .maximum_pdu_bits
            .get()
            .checked_mul(settings.limits.maximum_pdus.get())
            .ok_or(BearerError::Capacity)?;
        if bits > umts_security::algorithms::MAX_F8_BITS {
            return Err(BearerError::Capacity);
        }
        let bytes = settings
            .limits
            .maximum_sdu_bits
            .get()
            .div_ceil(BITS_PER_BYTE);
        let receive = TmReceiver::new(
            settings.uplink_segmentation,
            settings.errors,
            settings.limits,
        );
        Self::new(
            ue,
            id,
            scope,
            Entity::Transparent {
                receive: Box::new(receive),
                settings,
            },
            bits,
            bits.div_ceil(BITS_PER_BYTE),
            bytes,
            bytes,
        )
        .map_err(|(error, _)| error)
    }
    #[allow(clippy::too_many_arguments)]
    fn new(
        ue: UeKey,
        id: BearerId,
        scope: Scope,
        entity: Entity,
        maximum_receive_bits: usize,
        transmit_bytes: usize,
        maximum_sdu_bytes: usize,
        entity_bytes: usize,
    ) -> Result<Self, (BearerError, Entity)> {
        let receive_bytes = maximum_receive_bits.div_ceil(BITS_PER_BYTE);
        let sizes = (|| {
            let mac_bytes = transmit_bytes
                .checked_add(MAX_BLOCKS)
                .ok_or(BearerError::Capacity)?;
            let storage_bytes =
                Self::storage_requirement(maximum_receive_bits, transmit_bytes, entity_bytes)?;
            Ok::<_, BearerError>((mac_bytes, storage_bytes))
        })();
        let (mac_bytes, storage_bytes) = match sizes {
            Ok(sizes) => sizes,
            Err(error) => return Err((error, entity)),
        };
        Ok(Self {
            ue,
            id,
            scope,
            entity,
            raw: vec![0; receive_bytes],
            plain: vec![0; receive_bytes],
            transmit: vec![0; transmit_bytes],
            mac: vec![0; mac_bytes],
            maximum_receive_bits,
            maximum_sdu_bytes,
            storage_bytes,
            next_tm_transmit: None,
            next_tm_receive: None,
        })
    }
    fn storage_requirement(
        receive_bits: usize,
        transmit_bytes: usize,
        entity_bytes: usize,
    ) -> Result<usize, BearerError> {
        entity_bytes
            .checked_add(
                receive_bits
                    .div_ceil(BITS_PER_BYTE)
                    .checked_mul(2)
                    .ok_or(BearerError::Capacity)?,
            )
            .and_then(|bytes| bytes.checked_add(transmit_bytes.checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add(MAX_BLOCKS))
            .ok_or(BearerError::Capacity)
    }
    pub fn id(&self) -> BearerId {
        self.id
    }
    fn pending_receive_data(&self) -> bool {
        match &self.entity {
            Entity::Acknowledged(entity) => entity.pending_receive_data(),
            Entity::Unacknowledged { receive, .. } => receive.pending_receive_data(),
            _ => false,
        }
    }
    pub fn mode(&self) -> CipherMode {
        match self.entity {
            Entity::AwaitingUser {
                ref profiles,
                index,
            } => profiles.mode(index),
            Entity::Signalling(route) => route.mode,
            Entity::Acknowledged(_) => CipherMode::Acknowledged,
            Entity::Unacknowledged { .. } => CipherMode::Unacknowledged,
            Entity::Transparent { .. } => CipherMode::Transparent,
        }
    }
    pub fn enqueue(&mut self, sdu: &[u8]) -> Result<(), BearerError> {
        let maximum = match &self.entity {
            Entity::AwaitingUser { .. } => return Err(BearerError::Phase),
            Entity::Transparent { .. } | Entity::Signalling(_) => return Err(BearerError::Mapping),
            Entity::Acknowledged(entity) => entity.limits().maximum_sdu_bytes.get(),
            Entity::Unacknowledged { transmit, .. } => transmit.limits().maximum_sdu_bytes.get(),
        };
        if sdu.is_empty() || sdu.len() > maximum {
            return Err(BearerError::SduSize);
        }
        match &mut self.entity {
            Entity::Acknowledged(entity) => entity.enqueue(sdu).map_err(BearerError::Am),
            Entity::Unacknowledged { transmit, .. } => {
                transmit.enqueue(sdu).map_err(BearerError::Um)
            }
            Entity::AwaitingUser { .. } => Err(BearerError::Phase),
            Entity::Transparent { .. } | Entity::Signalling(_) => Err(BearerError::Mapping),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportChannels {
    Single(BearerId),
    Multiplexed(Vec<(LogicalChannelId, BearerId)>),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportBinding {
    pub transport: TransportId,
    pub channels: TransportChannels,
}
impl TransportBinding {
    fn mapping(&self) -> DedicatedMapping {
        match self.channels {
            TransportChannels::Single(_) => DedicatedMapping::Single,
            TransportChannels::Multiplexed(_) => DedicatedMapping::Multiplexed,
        }
    }
    fn resolve(&self, channel: Option<LogicalChannelId>) -> Result<BearerId, BearerError> {
        match (&self.channels, channel) {
            (TransportChannels::Single(id), None) => Ok(*id),
            (TransportChannels::Multiplexed(channels), Some(channel)) => channels
                .iter()
                .find(|(id, _)| *id == channel)
                .map(|(_, bearer)| *bearer)
                .ok_or(BearerError::Mapping),
            _ => Err(BearerError::Mapping),
        }
    }
    fn logical(&self, bearer: BearerId) -> Result<Option<LogicalChannelId>, BearerError> {
        match &self.channels {
            TransportChannels::Single(id) if *id == bearer => Ok(None),
            TransportChannels::Multiplexed(channels) => channels
                .iter()
                .find(|(_, id)| *id == bearer)
                .map(|(channel, _)| Some(*channel))
                .ok_or(BearerError::Mapping),
            _ => Err(BearerError::Mapping),
        }
    }
}
#[derive(Clone, Copy)]
pub struct BearerLimits {
    pub maximum_transport_bits: NonZeroUsize,
    pub output_slots: NonZeroUsize,
    pub maximum_sdu_bytes: NonZeroUsize,
    pub maximum_buffer_bytes: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransparentUnit {
    Joined,
    Block { index: usize },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransparentOrigin {
    pub start: ChipInstant,
    pub transport: TransportId,
    pub format: usize,
    pub unit: TransparentUnit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpperSdu {
    pub identity: ReceiveIdentity,
    pub bearer: BearerId,
    pub scope: Scope,
    pub received: ChipInstant,
    pub bits: usize,
    pub quality: Quality,
    pub coverage: Option<ReceiveCoverage>,
    pub transparent_origin: Option<TransparentOrigin>,
}
pub struct SduDelivery<'a> {
    pub metadata: UpperSdu,
    pub data: Bits<'a>,
    pub mapping: DeliveryMapping,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryMapping {
    Committed,
    Pending { previous: ReceiveIdentity },
}
struct Output {
    metadata: Option<UpperSdu>,
    bytes: Vec<u8>,
}
struct Outputs {
    slots: Vec<Output>,
    head: usize,
    queued: usize,
}
impl Outputs {
    fn discard_except(&mut self, keep: &[bool; MAX_BEARERS]) -> usize {
        let original = self.queued;
        let mut retained = 0;
        for offset in 0..original {
            let source = (self.head + offset) % self.slots.len();
            let metadata = self.slots[source].metadata.expect("queued bearer SDU");
            if keep[metadata.bearer.index()] {
                let target = (self.head + retained) % self.slots.len();
                self.slots.swap(target, source);
                retained += 1;
            } else {
                self.slots[source].metadata = None;
            }
        }
        self.queued = retained;
        original - retained
    }
    fn admit(&self, count: usize, bytes: usize) -> bool {
        count <= self.slots.len() - self.queued
            && bytes <= count.saturating_mul(self.slots[0].bytes.len())
    }
    fn push(&mut self, metadata: UpperSdu, bytes: Bits<'_>) {
        let index = (self.head + self.queued) % self.slots.len();
        bytes
            .copy_to(&mut self.slots[index].bytes[..metadata.bits.div_ceil(BITS_PER_BYTE)])
            .expect("reserved output geometry");
        self.slots[index].metadata = Some(metadata);
        self.queued += 1;
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BearerCounters {
    /// HSUPA PDUs for a bearer the configuration no longer binds, discarded on arrival.
    pub unbound_pdus: u64,
    pub am_reassembled_pdus: u64,
    pub am_reassembly_budget_exhausted: u64,
    pub maximum_am_delivery_age_chips: u64,
    pub early_user_pdus: u64,
    pub transports: u64,
    pub pdus: u64,
    pub tm_units: u64,
    pub sdus: u64,
    pub malformed: u64,
    pub backpressure: u64,
    pub retired_outputs: u64,
    pub retired_blocks: u64,
    pub released_bearers: u64,
    pub released_receive_bearers: u64,
    pub first_unsupported_packet_version: Option<PacketVersionEvidence>,
    pub last_tcp_packet: Option<TcpPacketEvidence>,
    pub first_tcp_gap: Option<TcpGapEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpPacketEvidence {
    pub identity: ReceiveIdentity,
    pub bearer: BearerId,
    pub resets: u64,
    pub addresses: [u8; 8],
    pub ports: [u8; 4],
    pub ip_id: u16,
    pub sequence: u32,
    pub payload_bytes: u16,
    pub first_position: u64,
    pub last_position: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpGapEvidence {
    pub previous: TcpPacketEvidence,
    pub current: TcpPacketEvidence,
    pub sequence_gap: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketVersionEvidence {
    pub identity: ReceiveIdentity,
    pub bearer: BearerId,
    pub bytes: usize,
    pub prefix: [u8; PACKET_VERSION_PREFIX_BYTES],
    pub first_position: u64,
    pub last_position: u64,
    pub preceding: Option<umts_rlc::acknowledged::AmRetainedPduEvidence>,
    pub first: Option<umts_rlc::acknowledged::AmRetainedPduEvidence>,
    pub last: Option<umts_rlc::acknowledged::AmRetainedPduEvidence>,
}
struct Pending {
    decoded: DecodedTransport,
    received: ChipInstant,
    binding: usize,
    block: usize,
    processed: u64,
    mapping: usize,
}
pub struct DedicatedBearers {
    owner: UeKey,
    owner_tag: Arc<()>,
    maps: [Option<Box<mapping::BearerMapping>>; 2],
    current: usize,
    endpoints: Vec<BearerEndpoint>,
    lookup: [Option<usize>; MAX_BEARERS],
    phase: BearerPhase,
    input: Vec<u8>,
    input_bits: usize,
    pending: Option<Pending>,
    output: Outputs,
    delivery_cursor: usize,
    delivery_turn: bool,
    last_service: Option<ChipInstant>,
    counters: BearerCounters,
    storage_bytes: usize,
    maximum_buffer_bytes: usize,
    downlink_recovery_required: bool,
}
#[derive(Clone, Copy)]
struct EndpointLayout<'a> {
    endpoints: &'a [BearerEndpoint],
    additional: &'a [BearerEndpoint],
    lookup: &'a [Option<usize>; MAX_BEARERS],
}
impl EndpointLayout<'_> {
    fn endpoint(&self, id: BearerId) -> Result<&BearerEndpoint, BearerError> {
        self.lookup[id.index()]
            .and_then(|index| self.endpoints.get(index))
            .or_else(|| self.additional.iter().find(|endpoint| endpoint.id == id))
            .ok_or(BearerError::Mapping)
    }

    fn len(&self) -> usize {
        self.endpoints.len() + self.additional.len()
    }
}
impl DedicatedBearers {
    pub(crate) fn owns_signalling_rlc(&self) -> bool {
        self.endpoints.iter().any(|endpoint| {
            endpoint.scope == Scope::Signalling && !matches!(endpoint.entity, Entity::Signalling(_))
        })
    }
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: ReceiveIdentity,
        anchor: FrameAnchor,
        first_frame: u64,
        uplink: Arc<ConfiguredTfcs>,
        bindings: Vec<TransportBinding>,
        endpoints: Vec<BearerEndpoint>,
        limits: BearerLimits,
    ) -> Result<Self, RejectedBearers> {
        let prepared = (|| {
            if identity.ue.epoch != anchor.time.epoch
                || first_frame % CONNECTION_FRAME_MODULUS != u64::from(anchor.cfn)
            {
                return Err(BearerError::Owner);
            }
            if endpoints.is_empty()
                || endpoints.len() > MAX_BEARERS
                || limits.output_slots.get() > MAX_OUTPUTS
                || limits.maximum_transport_bits.get() > MAX_TRANSPORT_BITS
                || limits.maximum_sdu_bytes.get() > MAX_SDU_BYTES
                || bindings.len() != uplink.transport_count()
            {
                return Err(BearerError::Capacity);
            }
            let mut lookup = [None; MAX_BEARERS];
            for (index, endpoint) in endpoints.iter().enumerate() {
                if endpoint.ue != identity.ue || lookup[endpoint.id.index()].is_some() {
                    return Err(BearerError::Owner);
                }
                if endpoint.maximum_sdu_bytes > limits.maximum_sdu_bytes.get() {
                    return Err(BearerError::Capacity);
                }
                if endpoint
                    .next_tm_receive
                    .is_some_and(|next| first_frame < next)
                {
                    return Err(BearerError::Time);
                }
                lookup[endpoint.id.index()] = Some(index);
            }
            mapping::validate_uplink(
                &uplink,
                &bindings,
                EndpointLayout {
                    endpoints: &endpoints,
                    additional: &[],
                    lookup: &lookup,
                },
                limits.maximum_transport_bits.get(),
            )?;
            let input_bytes = limits.maximum_transport_bits.get().div_ceil(BITS_PER_BYTE);
            let bytes = endpoints
                .iter()
                .try_fold(input_bytes, |sum, endpoint| {
                    sum.checked_add(endpoint.storage_bytes)
                        .ok_or(BearerError::Capacity)
                })?
                .checked_add(
                    limits
                        .output_slots
                        .get()
                        .checked_mul(limits.maximum_sdu_bytes.get())
                        .ok_or(BearerError::Capacity)?,
                )
                .ok_or(BearerError::Capacity)?;
            if bytes > limits.maximum_buffer_bytes {
                return Err(BearerError::Capacity);
            }
            Ok((lookup, input_bytes, bytes))
        })();
        let (lookup, input_bytes, storage_bytes) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Err(RejectedBearers { error, endpoints }),
        };
        let last_transport = vec![None; bindings.len()];
        let map = Box::new(mapping::BearerMapping {
            identity,
            anchor,
            first_frame,
            uplink,
            bindings,
            last_transport,
            downlink: None,
            plan: None,
            phase: MappingPhase::Active,
            additional: false,
            added_bearers: Vec::new(),
        });
        let map_bytes = match map.storage_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return Err(RejectedBearers { error, endpoints }),
        };
        let Some(storage_bytes) = storage_bytes
            .checked_add(map_bytes)
            .filter(|bytes| *bytes <= limits.maximum_buffer_bytes)
        else {
            return Err(RejectedBearers {
                error: BearerError::Capacity,
                endpoints,
            });
        };
        Ok(Self {
            owner: identity.ue,
            owner_tag: Arc::new(()),
            maps: [Some(map), None],
            current: 0,
            endpoints,
            lookup,
            phase: BearerPhase::Prepared,
            input: vec![0; input_bytes],
            input_bits: limits.maximum_transport_bits.get(),
            pending: None,
            output: Outputs {
                slots: (0..limits.output_slots.get())
                    .map(|_| Output {
                        metadata: None,
                        bytes: vec![0; limits.maximum_sdu_bytes.get()],
                    })
                    .collect(),
                head: 0,
                queued: 0,
            },
            delivery_cursor: 0,
            delivery_turn: false,
            last_service: None,
            counters: BearerCounters::default(),
            storage_bytes,
            maximum_buffer_bytes: limits.maximum_buffer_bytes,
            downlink_recovery_required: false,
        })
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.current_map().identity
    }
    pub fn frame_anchor(&self) -> FrameAnchor {
        self.current_map().anchor
    }
    pub fn configured(&self) -> &Arc<ConfiguredTfcs> {
        &self.current_map().uplink
    }
    pub fn storage_bytes(&self) -> usize {
        self.storage_bytes
    }
    pub fn phase(&self) -> BearerPhase {
        self.phase
    }
    #[cfg(test)]
    pub fn counters(&self) -> BearerCounters {
        self.counters
    }
    pub(crate) fn user_data_pending(&self) -> bool {
        self.pending.is_some()
            || self.output.queued != 0
            || self.endpoints.iter().any(|endpoint| {
                if endpoint.scope == Scope::Signalling {
                    return false;
                }
                match &endpoint.entity {
                    Entity::Acknowledged(entity) => {
                        let window = entity.window_positions();
                        entity.pending_new_data()
                            || entity.pending_receive_data()
                            || window.oldest_transmit != window.next_transmit
                    }
                    Entity::Unacknowledged { transmit, receive } => {
                        transmit.has_pending() || receive.pending_receive_data()
                    }
                    Entity::Transparent { .. } | Entity::AwaitingUser { .. } => true,
                    Entity::Signalling(_) => false,
                }
            })
    }
    pub fn activate(&mut self, identity: ReceiveIdentity) -> Result<(), BearerError> {
        if identity != self.identity() {
            return Err(BearerError::Owner);
        }
        if self.phase != BearerPhase::Prepared {
            return Err(BearerError::Phase);
        }
        self.phase = BearerPhase::Active;
        Ok(())
    }
    pub fn enqueue_transport(
        &mut self,
        delivery: TransportDelivery<'_>,
    ) -> Result<(), BearerError> {
        let map_index = self.mapping_index(delivery.decoded.identity)?;
        let map = self.maps[map_index].as_ref().expect("registered mapping");
        if matches!(map.phase, MappingPhase::Staged { .. }) {
            return Err(BearerError::Phase);
        }
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase);
        }
        if self.pending.is_some() {
            return Err(BearerError::Capacity);
        }
        let decoded = delivery.decoded;
        let binding = map
            .bindings
            .iter()
            .position(|binding| binding.transport == decoded.transport)
            .ok_or(BearerError::Mapping)?;
        let definition = map
            .uplink
            .definition(decoded.transport)
            .ok_or(BearerError::Mapping)?;
        let format = definition
            .formats
            .get(decoded.format)
            .ok_or(BearerError::Transport)?;
        let delta = decoded
            .start
            .index
            .checked_sub(map.anchor.time.index)
            .ok_or(BearerError::Time)?;
        let frame = map
            .first_frame
            .checked_add(delta / CHIPS_PER_FRAME)
            .ok_or(BearerError::Time)?;
        let received = ChipInstant {
            index: decoded
                .start
                .index
                .checked_add(CHIPS_PER_FRAME * definition.interval.frames() as u64)
                .ok_or(BearerError::Time)?,
            ..decoded.start
        };
        if decoded.start.epoch != map.anchor.time.epoch
            || !delta.is_multiple_of(CHIPS_PER_FRAME)
            || !frame.is_multiple_of(definition.interval.frames() as u64)
            || map.last_transport[binding].is_some_and(|old| old.index >= decoded.start.index)
            || matches!(map.phase, MappingPhase::Draining { uplink_end, .. } if received.index > uplink_end.index)
        {
            return Err(BearerError::Time);
        }
        let bits = decoded
            .block_bits
            .checked_mul(decoded.blocks)
            .filter(|v| *v <= self.input_bits)
            .ok_or(BearerError::Capacity)?;
        if decoded.blocks == 0
            || format.blocks != decoded.blocks
            || format.block_bits != decoded.block_bits
            || bits != delivery.payload.len()
            || decoded.crc_checked != (format.crc.width() != 0)
        {
            return Err(BearerError::Transport);
        }
        pack_msb(
            delivery.payload,
            &mut self.input[..bits.div_ceil(BITS_PER_BYTE)],
        )
        .map_err(|_| BearerError::Transport)?;
        self.pending = Some(Pending {
            decoded,
            received,
            binding,
            block: 0,
            processed: 0,
            mapping: map_index,
        });
        self.maps[map_index]
            .as_mut()
            .expect("registered mapping")
            .last_transport[binding] = Some(decoded.start);
        self.counters.transports = self.counters.transports.saturating_add(1);
        Ok(())
    }
    pub fn process_one(&mut self, accept: impl FnOnce(SduDelivery<'_>) -> bool) -> bool {
        if self.output.queued == 0 {
            return false;
        }
        let slot = &self.output.slots[self.output.head];
        let metadata = slot.metadata.expect("queued bearer SDU");
        let mapping = self
            .maps
            .iter()
            .flatten()
            .find(|map| map.identity == metadata.identity)
            .and_then(|map| match map.phase {
                MappingPhase::Scheduled { previous } => Some(DeliveryMapping::Pending { previous }),
                _ => None,
            })
            .unwrap_or(DeliveryMapping::Committed);
        if !accept(SduDelivery {
            metadata,
            data: Bits::new(&slot.bytes, metadata.bits).expect("published SDU length"),
            mapping,
        }) {
            return false;
        }
        self.output.slots[self.output.head].metadata = None;
        self.output.head = (self.output.head + 1) % self.output.slots.len();
        self.output.queued -= 1;
        true
    }
    pub fn enqueue_sdu(&mut self, bearer: BearerId, sdu: &[u8]) -> Result<(), BearerError> {
        if self.phase != BearerPhase::Active {
            return Err(BearerError::Phase);
        }
        if let MappingPhase::Draining { next, .. } = self.current_map().phase {
            let successor = self.maps[self.mapping_index(next)?]
                .as_ref()
                .expect("scheduled mapping");
            if successor
                .plan
                .as_ref()
                .is_none_or(|plan| plan.configuration().bearer(bearer.get()).is_none())
            {
                return Err(BearerError::Phase);
            }
        }
        self.endpoints[self.lookup[bearer.index()].ok_or(BearerError::Mapping)?].enqueue(sdu)
    }
    pub fn retire(&mut self) {
        self.phase = BearerPhase::Retired;
        self.counters.retired_outputs = self
            .counters
            .retired_outputs
            .saturating_add(self.output.queued as u64);
        if let Some(pending) = self.pending.take() {
            self.counters.retired_blocks = self.counters.retired_blocks.saturating_add(
                pending.decoded.blocks as u64 - u64::from(pending.processed.count_ones()),
            );
        }
        for output in &mut self.output.slots {
            output.metadata = None;
        }
        self.output.queued = 0;
    }
    #[cfg(test)]
    pub fn into_endpoints(self) -> Result<Vec<BearerEndpoint>, Box<Self>> {
        if self.phase != BearerPhase::Retired {
            return Err(Box::new(self));
        }
        Ok(self.endpoints)
    }
}

#[cfg(test)]
mod tests;
