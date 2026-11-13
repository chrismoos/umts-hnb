// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod initialization;
mod pending_uplink;
mod radio_handoff;
use pending_uplink::PendingUplink;
mod transport;
pub use transport::{
    IngressSettings, PlayoutSettings, PlayoutWindow, TransportError, VoiceIngress, VoiceTransport,
};

use crate::{
    dedicated_bearers::{
        BearerError, BearerId, DedicatedBearers, DownlinkGrant, FrameBuildError, MAX_BLOCKS,
        SduDelivery, TransparentOffer, TransparentTraffic, TransparentUnit,
    },
    dedicated_rx::ReceiveIdentity,
    rel99_plan::CompiledRel99,
};
use std::{
    collections::HashMap,
    num::{NonZeroU64, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use umts_rrc::dedicated::Rel99Rlc;
use umts_types::{
    CnDomain, ConnectionKey,
    bits::Bits,
    time::{CHIPS_PER_FRAME, ChipInstant},
};
use umts_user_plane::{
    iuup::{
        Frame,
        initialization::{
            Formats, Initialization, MAX_RFCIS, MAX_SUBFLOWS, ModeVersion, Rfci, Versions,
        },
        negotiation::{self, AckEffect, Cause, Negotiated, Receiver, Sender, Settings},
    },
    media::{self, MediaQueue, RtpAnchor},
    uplink_media::{self, Delivery, Piece, RadioQuality, RtpSource, SourceClock, UplinkAssembler},
};

const MAX_ROUTES: usize = 4096;
const REPLACEMENT_BUFFER_SETS: usize = 2;
const IUUP_FRAME_MODULUS: u64 = 16;
static NEXT_ALLOCATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VoiceKey {
    pub connection: ConnectionKey,
    pub rab: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoiceHandle {
    key: VoiceKey,
    allocation: NonZeroU64,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub routes: NonZeroUsize,
    pub frames_per_route: NonZeroUsize,
    pub maximum_payload_bytes: NonZeroUsize,
    pub maximum_storage_bytes: usize,
}

impl Limits {
    pub fn required_route_storage_bytes(self) -> Result<usize, Error> {
        self.media_storage()?
            .checked_add(Receiver::storage_ceiling())
            .and_then(|bytes| bytes.checked_add(PendingUplink::storage(self).ok()?))
            .ok_or(Error::Capacity)
    }

    fn media_storage(self) -> Result<usize, Error> {
        let payload = self.maximum_payload_bytes.get();
        let uplink = UplinkAssembler::storage_ceiling(payload, self.frames_per_route)
            .map_err(Error::Uplink)?;
        self.frames_per_route
            .get()
            .checked_mul(payload)
            .and_then(|bytes| bytes.checked_add(uplink))
            .and_then(|bytes| bytes.checked_mul(REPLACEMENT_BUFFER_SETS))
            .ok_or(Error::Capacity)
    }
}

pub struct UplinkPolicy {
    pub delivery: Box<[Delivery]>,
    pub interval_chips: NonZeroU64,
    pub wait_chips: NonZeroU64,
    pub timestamp: u32,
    pub rtp: RtpSource,
}

#[derive(Debug)]
pub enum Error {
    Mapping,
    UplinkMapping(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        UplinkMappingFailure,
    ),
    Capacity,
    Radio,
    Negotiating,
    Attached,
    Negotiation(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        negotiation::Error,
    ),
    Proposal(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        initialization::Error,
    ),
    Media(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        media::Error,
    ),
    Ingress(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        media::IngressError,
    ),
    Uplink(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        uplink_media::Error,
    ),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UplinkMappingFailure {
    Metadata,
    Route,
    Binding,
    Origin,
    Transport,
    Format,
    Segmentation,
    Geometry,
}

#[derive(Clone, Copy, Debug, Default)]
struct Geometry {
    width: usize,
    blocks: usize,
    segmented: bool,
    period: u64,
}

struct Subflow {
    bearer: BearerId,
    transport: usize,
    segmented: bool,
    uplink_transport: usize,
    uplink_segmented: bool,
}
struct Layout {
    subflows: Box<[Subflow]>,
    rows: [Option<[Geometry; MAX_SUBFLOWS]>; MAX_RFCIS],
}

impl Layout {
    fn new(plan: &CompiledRel99, bearers: &[BearerId]) -> Result<Self, Error> {
        if bearers.is_empty() || bearers.len() > MAX_SUBFLOWS {
            return Err(Error::Mapping);
        }
        let mut subflows = Vec::with_capacity(bearers.len());
        for (index, bearer) in bearers.iter().enumerate() {
            let definition = plan
                .configuration()
                .bearer(bearer.get())
                .ok_or(Error::Mapping)?;
            let Rel99Rlc::Transparent {
                downlink_segmentation,
                uplink_segmentation,
            } = definition.rlc
            else {
                return Err(Error::Mapping);
            };
            if definition.domain != Some(CnDomain::Cs)
                || bearers[..index].contains(bearer)
                || definition.mapping.downlink.len() != 1
                || definition.mapping.uplink.len() != 1
            {
                return Err(Error::Mapping);
            }
            let mapping = definition.mapping.downlink[0];
            if mapping.logical.is_some() {
                return Err(Error::Mapping);
            }
            let transport = plan
                .downlink_configured()
                .transport_ids()
                .position(|id| id.get() == mapping.transport)
                .ok_or(Error::Mapping)?;
            let uplink_mapping = definition.mapping.uplink[0];
            if uplink_mapping.logical.is_some() {
                return Err(Error::Mapping);
            }
            let uplink_transport = plan
                .uplink_configured()
                .transport_ids()
                .position(|id| id.get() == uplink_mapping.transport)
                .ok_or(Error::Mapping)?;
            if subflows.iter().any(|old: &Subflow| {
                old.transport == transport || old.uplink_transport == uplink_transport
            }) {
                return Err(Error::Mapping);
            }
            subflows.push(Subflow {
                bearer: *bearer,
                transport,
                segmented: downlink_segmentation,
                uplink_transport,
                uplink_segmented: uplink_segmentation,
            });
        }
        Ok(Self {
            subflows: subflows.into_boxed_slice(),
            rows: [None; MAX_RFCIS],
        })
    }

    fn validate(
        &self,
        plan: &CompiledRel99,
        formats: &Formats,
    ) -> Result<[Option<[Geometry; MAX_SUBFLOWS]>; MAX_RFCIS], Cause> {
        if formats.subflows() != self.subflows.len() {
            return Err(Cause::Value);
        }
        let mut rows = [None; MAX_RFCIS];
        let configured = plan.downlink_configured();
        for row in formats.rows() {
            let uplink = plan.uplink_configured();
            if !uplink.combinations().iter().any(|combination| {
                self.subflows
                    .iter()
                    .zip(row.lengths())
                    .all(|(subflow, &length)| {
                        let Some(id) = uplink.transport_ids().nth(subflow.uplink_transport) else {
                            return false;
                        };
                        let Some(definition) = uplink.definition(id) else {
                            return false;
                        };
                        let block =
                            definition.formats[combination.formats[subflow.uplink_transport]];
                        block.blocks <= MAX_BLOCKS
                            && block.block_bits * block.blocks == usize::from(length)
                    })
            }) {
                return Err(Cause::Rfci);
            }
            let candidate = configured
                .combinations()
                .iter()
                .find_map(|combination| {
                    let mut geometry = [Geometry::default(); MAX_SUBFLOWS];
                    for (index, (subflow, length)) in
                        self.subflows.iter().zip(row.lengths()).enumerate()
                    {
                        let transport = configured.transport_ids().nth(subflow.transport)?;
                        let definition = configured.definition(transport)?;
                        let block = definition.formats[combination.formats[subflow.transport]];
                        if block.block_bits * block.blocks != usize::from(*length)
                            || block.blocks > MAX_BLOCKS
                        {
                            return None;
                        }
                        geometry[index] = Geometry {
                            width: block.block_bits,
                            blocks: block.blocks,
                            segmented: subflow.segmented,
                            period: definition.interval.frames() as u64,
                        };
                    }
                    Some(geometry)
                })
                .ok_or(Cause::Rfci)?;
            rows[usize::from(row.rfci().get())] = Some(candidate);
        }
        Ok(rows)
    }

    fn geometry(&self, rfci: Rfci, subflow: usize) -> Option<Geometry> {
        self.rows
            .get(usize::from(rfci.get()))?
            .as_ref()?
            .get(subflow)
            .copied()
    }
}

enum RadioBinding {
    Prepared,
    Active {
        origin: ChipInstant,
        first_frame: u64,
        uplink: SourceClock,
    },
}

struct Binding {
    handle: VoiceHandle,
    identity: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
    layout: Layout,
    negotiation: Negotiation,
    contract: Option<initialization::Contract>,
    contract_storage: usize,
    supported: Versions,
    receiver_started: bool,
    control_reply: Option<umts_user_plane::iuup::negotiation::ControlReply>,
    initialization_storage: usize,
    transport_capacity: Option<NonZeroUsize>,
    queue: Option<MediaQueue>,
    uplink: Option<UplinkAssembler>,
    pending_uplink: Option<PendingUplink>,
    candidate: Option<PreparedMedia>,
    uplink_policy: UplinkPolicy,
    radio: RadioBinding,
    media: MediaLifetime,
}

enum MediaLifetime {
    Active,
    Draining { discarded_uplink: u64 },
}

enum Negotiation {
    Receiver(Receiver),
    Sender(Sender),
    Reinitializing { sender: Sender, receiver: Receiver },
}

impl Negotiation {
    fn receiver_mut(&mut self) -> Option<&mut Receiver> {
        match self {
            Self::Receiver(receiver) | Self::Reinitializing { receiver, .. } => Some(receiver),
            Self::Sender(_) => None,
        }
    }
    fn negotiated(&self) -> Option<&Negotiated> {
        match self {
            Self::Receiver(receiver) => receiver.negotiated(),
            Self::Sender(sender) => sender.negotiated(),
            Self::Reinitializing { receiver, .. } => receiver.negotiated(),
        }
    }

    fn publish<E>(
        &mut self,
        now: Instant,
        output: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<bool, negotiation::PublishError<E>> {
        match self {
            Self::Receiver(receiver) | Self::Reinitializing { receiver, .. } => {
                receiver.publish(output, send)
            }
            Self::Sender(sender) => sender.publish(now, output, send),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitializationStatus {
    Pending,
    Ready,
    Failed(negotiation::Failure),
}

struct PreparedMedia {
    queue: MediaQueue,
    uplink: UplinkAssembler,
    rows: [Option<[Geometry; MAX_SUBFLOWS]>; MAX_RFCIS],
}

impl Binding {
    fn discard_draining_uplink(&mut self) -> bool {
        let MediaLifetime::Draining { discarded_uplink } = &mut self.media else {
            return false;
        };
        *discarded_uplink = discarded_uplink.saturating_add(1);
        true
    }

    fn finish_receiver_transition(&mut self) -> usize {
        let restore_sender = match &self.negotiation {
            Negotiation::Reinitializing { sender, receiver } => {
                let Some(active) = receiver.negotiated() else {
                    return 0;
                };
                sender
                    .negotiated()
                    .is_some_and(|old| old.same_session(active))
            }
            _ => return 0,
        };
        let previous = std::mem::replace(
            &mut self.negotiation,
            Negotiation::Receiver(
                Receiver::new(self.supported).expect("validated supported versions"),
            ),
        );
        let Negotiation::Reinitializing { sender, receiver } = previous else {
            unreachable!("validated receiver transition")
        };
        if restore_sender {
            self.negotiation = Negotiation::Sender(sender);
            0
        } else {
            self.negotiation = Negotiation::Receiver(receiver);
            std::mem::take(&mut self.initialization_storage)
        }
    }

    fn matches_rab(&self, rab: &crate::rel99_planner::RabPlan) -> bool {
        self.handle.key.rab == rab.rab
            && self.layout.subflows.len() == rab.bearers.len()
            && self
                .layout
                .subflows
                .iter()
                .zip(&rab.bearers)
                .all(|(subflow, id)| subflow.bearer.get() == *id)
    }

    fn prepare_media(
        &self,
        candidate: &Negotiated,
        limits: Limits,
    ) -> Result<PreparedMedia, Error> {
        if candidate.formats().maximum_payload_bytes() > limits.maximum_payload_bytes.get() {
            return Err(Error::Capacity);
        }
        let rows = self
            .layout
            .validate(&self.plan, candidate.formats())
            .map_err(|_| Error::Mapping)?;
        let queue = MediaQueue::new(
            candidate.clone(),
            self.identity.ue.epoch,
            limits.frames_per_route,
            limits.frames_per_route.get() * limits.maximum_payload_bytes.get(),
        )
        .map_err(Error::Media)?;
        let uplink = UplinkAssembler::new(
            candidate.clone(),
            self.identity.ue.epoch,
            &self.uplink_policy.delivery,
            uplink_media::Limits {
                frames: limits.frames_per_route,
                wait_chips: self.uplink_policy.wait_chips,
                maximum_storage_bytes: limits.media_storage()? / REPLACEMENT_BUFFER_SETS,
            },
        )
        .map_err(Error::Uplink)?;
        Ok(PreparedMedia {
            queue,
            uplink,
            rows,
        })
    }

    fn commit_media(&mut self) {
        let Some(active) = self.negotiation.negotiated() else {
            return;
        };
        if self
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.queue.negotiated().same_session(active))
        {
            let candidate = self.candidate.take().expect("matching candidate");
            self.queue = Some(candidate.queue);
            self.uplink = Some(candidate.uplink);
            self.layout.rows = candidate.rows;
        }
    }

    fn finish_sender(&mut self, limits: Limits) -> Result<(), Error> {
        let Some(active) = self.negotiation.negotiated() else {
            return Ok(());
        };
        if self
            .queue
            .as_ref()
            .is_none_or(|queue| !queue.negotiated().same_session(active))
        {
            self.candidate = Some(self.prepare_media(active, limits)?);
            self.commit_media();
        }
        Ok(())
    }

    fn active_queue(&self) -> Option<&MediaQueue> {
        if !matches!(self.radio, RadioBinding::Active { .. })
            || !matches!(self.media, MediaLifetime::Active)
        {
            return None;
        }
        let active = self.negotiation.negotiated()?;
        self.queue
            .as_ref()
            .filter(|queue| active.same_session(queue.negotiated()))
    }

    fn active_queue_mut(&mut self) -> Option<&mut MediaQueue> {
        if !matches!(self.radio, RadioBinding::Active { .. })
            || !matches!(self.media, MediaLifetime::Active)
        {
            return None;
        }
        let active = self.negotiation.negotiated()?;
        self.queue
            .as_mut()
            .filter(|queue| active.same_session(queue.negotiated()))
    }

    fn align_playout(&self, due: ChipInstant, rfci: Rfci) -> Option<ChipInstant> {
        let RadioBinding::Active {
            origin,
            first_frame,
            ..
        } = self.radio
        else {
            return None;
        };
        if due.epoch != origin.epoch {
            return None;
        }
        let row = self.layout.rows[usize::from(rfci.get())].as_ref()?;
        let period = row[..self.layout.subflows.len()]
            .iter()
            .filter(|geometry| geometry.width * geometry.blocks != 0)
            .map(|geometry| geometry.period)
            .max()
            .unwrap_or(1);
        let quantum = period.checked_mul(CHIPS_PER_FRAME)?;
        let phase = (first_frame % period).checked_mul(CHIPS_PER_FRAME)?;
        let relative = due.index.checked_sub(origin.index)?.checked_add(phase)?;
        // TS 25.414 §5.1.3.3.1.8 permits jitter correction. RTP ticks need not coincide with a radio TTI.
        let aligned = relative
            .checked_add(quantum / 2)?
            .checked_div(quantum)?
            .checked_mul(quantum)?
            .checked_sub(phase)?;
        Some(ChipInstant {
            epoch: due.epoch,
            index: origin.index.checked_add(aligned)?,
        })
    }

    fn legal_time(&self, due: ChipInstant, rfci: Rfci) -> bool {
        let RadioBinding::Active {
            origin,
            first_frame,
            ..
        } = self.radio
        else {
            return false;
        };
        if due.epoch != origin.epoch {
            return false;
        }
        let Some(delta) = due.index.checked_sub(origin.index) else {
            return false;
        };
        if !delta.is_multiple_of(CHIPS_PER_FRAME) {
            return false;
        }
        let Some(frame) = first_frame.checked_add(delta / CHIPS_PER_FRAME) else {
            return false;
        };
        self.layout.rows[usize::from(rfci.get())]
            .as_ref()
            .is_some_and(|row| {
                row[..self.layout.subflows.len()].iter().all(|geometry| {
                    geometry.width * geometry.blocks == 0 || frame.is_multiple_of(geometry.period)
                })
            })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RadioKey {
    ue: umts_types::UeKey,
    configuration: NonZeroU64,
    bearer: u8,
}

fn radio_key(identity: ReceiveIdentity, bearer: BearerId) -> RadioKey {
    RadioKey {
        ue: identity.ue,
        configuration: identity.configuration,
        bearer: bearer.get(),
    }
}

pub struct VoiceTraffic {
    bindings: HashMap<VoiceKey, Binding>,
    radio: HashMap<RadioKey, (VoiceKey, usize)>,
    limits: Limits,
    reserved: usize,
}

impl VoiceTraffic {
    #[cfg(test)]
    pub fn drained_uplink(&self, handle: VoiceHandle) -> Option<u64> {
        let binding = self
            .bindings
            .get(&handle.key)
            .filter(|binding| binding.handle == handle)?;
        match binding.media {
            MediaLifetime::Active => None,
            MediaLifetime::Draining { discarded_uplink } => Some(discarded_uplink),
        }
    }
    pub fn has_connection(&self, connection: ConnectionKey) -> bool {
        self.bindings.keys().any(|key| key.connection == connection)
    }
    #[cfg(test)]
    pub fn reserved_bytes(&self) -> usize {
        self.reserved
    }
    pub fn new(limits: Limits) -> Result<Self, Error> {
        if limits.routes.get() > MAX_ROUTES
            || limits.frames_per_route.get() > media::MAX_MEDIA_SLOTS
        {
            return Err(Error::Capacity);
        }
        Ok(Self {
            bindings: HashMap::with_capacity(limits.routes.get()),
            radio: HashMap::with_capacity(limits.routes.get() * MAX_SUBFLOWS),
            limits,
            reserved: 0,
        })
    }

    fn binding_mut(&mut self, handle: VoiceHandle) -> Result<&mut Binding, Error> {
        self.bindings
            .get_mut(&handle.key)
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)
    }

    pub fn prepare(
        &mut self,
        key: VoiceKey,
        identity: ReceiveIdentity,
        plan: Arc<CompiledRel99>,
        bearers: &[BearerId],
        supported: Versions,
        uplink_policy: UplinkPolicy,
    ) -> Result<VoiceHandle, Error> {
        if key.connection.ue != identity.ue
            || key.connection.domain != CnDomain::Cs
            || self.bindings.contains_key(&key)
        {
            return Err(Error::Mapping);
        }
        let layout = Layout::new(&plan, bearers)?;
        uplink_policy
            .rtp
            .validate()
            .map_err(|error| Error::Uplink(uplink_media::Error::Wire(error)))?;
        if uplink_policy.delivery.len() != bearers.len()
            || !uplink_policy
                .interval_chips
                .get()
                .is_multiple_of(CHIPS_PER_FRAME)
        {
            return Err(Error::Mapping);
        }
        if self.bindings.len() == self.limits.routes.get() {
            return Err(Error::Capacity);
        }
        let storage = self.limits.required_route_storage_bytes()?;
        let reserved = self
            .reserved
            .checked_add(storage)
            .filter(|reserved| *reserved <= self.limits.maximum_storage_bytes)
            .ok_or(Error::Capacity)?;
        if bearers
            .iter()
            .any(|bearer| self.radio.contains_key(&radio_key(identity, *bearer)))
        {
            return Err(Error::Mapping);
        }
        let receiver = Receiver::new(supported).map_err(Error::Negotiation)?;
        let allocation = NEXT_ALLOCATION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::Capacity)?;
        let handle = VoiceHandle { key, allocation };
        self.bindings.insert(
            key,
            Binding {
                handle,
                identity,
                plan,
                layout,
                negotiation: Negotiation::Receiver(receiver),
                contract: None,
                contract_storage: 0,
                supported,
                receiver_started: false,
                control_reply: None,
                initialization_storage: 0,
                transport_capacity: None,
                queue: None,
                uplink: None,
                pending_uplink: Some(PendingUplink::new(self.limits)),
                candidate: None,
                uplink_policy,
                radio: RadioBinding::Prepared,
                media: MediaLifetime::Active,
            },
        );
        for (index, bearer) in bearers.iter().enumerate() {
            self.radio
                .insert(radio_key(identity, *bearer), (key, index));
        }
        self.reserved = reserved;
        Ok(handle)
    }

    pub fn activate(&mut self, handle: VoiceHandle, bank: &DedicatedBearers) -> Result<(), Error> {
        let binding = self.binding_mut(handle)?;
        let count = binding.layout.subflows.len();
        let mut bearers = [binding.layout.subflows[0].bearer; MAX_SUBFLOWS];
        for (target, subflow) in bearers.iter_mut().zip(&binding.layout.subflows) {
            *target = subflow.bearer;
        }
        let anchors = bank
            .voice_binding_anchor(binding.identity, &bearers[..count], &binding.plan)
            .ok_or(Error::Radio)?;
        let interval_frames = binding.uplink_policy.interval_chips.get() / CHIPS_PER_FRAME;
        if !anchors.uplink.first_frame.is_multiple_of(interval_frames) {
            return Err(Error::Radio);
        }
        let uplink = SourceClock::new(
            anchors.uplink.origin,
            binding.uplink_policy.interval_chips,
            binding.uplink_policy.timestamp,
            ((anchors.uplink.first_frame / interval_frames) % IUUP_FRAME_MODULUS) as u8,
        )
        .map_err(Error::Uplink)?;
        binding.radio = RadioBinding::Active {
            origin: anchors.downlink.origin,
            first_frame: anchors.downlink.first_frame,
            uplink,
        };
        Ok(())
    }

    pub fn request(&mut self, handle: VoiceHandle, frame: Frame<'_>) -> Result<(), Error> {
        let limits = self.limits;
        let binding = self.binding_mut(handle)?;
        // TS 25.415 6.5.3 to 6.5.5: these procedures are answered without touching the
        // negotiated Iu-UP state, whichever side initialized it.
        if let Some(procedure) =
            umts_user_plane::iuup::negotiation::ControlProcedure::classify(frame)
        {
            let Some(reply) = procedure.reply() else {
                return Ok(());
            };
            if binding.control_reply.is_some() {
                return Err(Error::Negotiation(negotiation::Error::Busy));
            }
            binding.control_reply = Some(reply);
            return Ok(());
        }
        let mut seeded = match &binding.negotiation {
            Negotiation::Sender(sender) => {
                let active = sender
                    .negotiated()
                    .ok_or(Error::Negotiation(negotiation::Error::Busy))?;
                if binding
                    .queue
                    .as_ref()
                    .is_none_or(|queue| !queue.negotiated().same_session(active))
                    || binding
                        .uplink
                        .as_ref()
                        .is_none_or(|queue| !queue.negotiated().same_session(active))
                {
                    return Err(Error::Negotiating);
                }
                Some(
                    Receiver::from_negotiated(binding.supported, active.clone())
                        .map_err(Error::Negotiation)?,
                )
            }
            _ => None,
        };
        let layout = &binding.layout;
        let plan = &binding.plan;
        let transport_capacity = binding.transport_capacity;
        let contract = &binding.contract;
        let proposal = if contract.is_some() {
            match frame {
                Frame::Control { payload, .. } => Initialization::decode(payload).ok(),
                Frame::Data { .. } => None,
            }
        } else {
            None
        };
        let receiver = match &mut seeded {
            Some(receiver) => receiver,
            None => binding
                .negotiation
                .receiver_mut()
                .ok_or(Error::Negotiation(negotiation::Error::Busy))?,
        };
        receiver
            .request_with(frame, |formats| {
                if formats.maximum_payload_bytes() > limits.maximum_payload_bytes.get() {
                    return Err(Cause::Value);
                }
                if transport_capacity
                    .is_some_and(|capacity| formats.maximum_frame_bytes() >= capacity.get())
                {
                    return Err(Cause::Value);
                }
                if let Some(contract) = contract {
                    let proposal = proposal.as_ref().ok_or(Cause::Value)?;
                    contract.validate(formats, !proposal.chained(), proposal.versions())?;
                }
                layout.validate(plan, formats).map(|_| ())
            })
            .map_err(Error::Negotiation)?;
        let candidate = receiver.pending_negotiated().cloned();
        if let Some(receiver) = seeded {
            let previous = std::mem::replace(
                &mut binding.negotiation,
                Negotiation::Receiver(
                    Receiver::new(binding.supported).expect("validated supported versions"),
                ),
            );
            let Negotiation::Sender(sender) = previous else {
                unreachable!("validated original sender")
            };
            binding.negotiation = Negotiation::Reinitializing { sender, receiver };
        }
        binding.receiver_started = true;
        binding.candidate = None;
        if let Some(candidate) = candidate
            && binding
                .queue
                .as_ref()
                .is_none_or(|queue| !candidate.same_session(queue.negotiated()))
        {
            match binding.prepare_media(&candidate, limits) {
                Ok(prepared) => binding.candidate = Some(prepared),
                Err(_) => {
                    if let Some(receiver) = binding.negotiation.receiver_mut() {
                        receiver
                            .reject_pending(Cause::Value)
                            .map_err(Error::Negotiation)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn constrain_rab_initialization(
        &mut self,
        handle: VoiceHandle,
        rab: &crate::rel99_planner::RabPlan,
        request: &umts_iuh::ranap::rab::SetupOrModify,
        implemented: Versions,
    ) -> Result<(), Error> {
        let reserved = self.reserved;
        let maximum = self.limits.maximum_storage_bytes;
        let binding = self.binding_mut(handle)?;
        if !binding.matches_rab(rab)
            || implemented.intersection(binding.supported) != Some(implemented)
        {
            return Err(Error::Mapping);
        }
        if !matches!(&binding.negotiation, Negotiation::Receiver(receiver) if !receiver.response_pending() && receiver.negotiated().is_none())
            || binding.receiver_started
        {
            return Err(Error::Negotiation(negotiation::Error::Busy));
        }
        let contract = initialization::contract_from_rab(rab, request, implemented)
            .map_err(Error::Proposal)?;
        let storage = contract.storage_bytes();
        let next = reserved
            .checked_sub(binding.contract_storage)
            .and_then(|value| value.checked_add(storage))
            .filter(|value| *value <= maximum)
            .ok_or(Error::Capacity)?;
        binding.contract = Some(contract);
        binding.contract_storage = storage;
        self.reserved = next;
        Ok(())
    }

    pub fn start_rab_initialization(
        &mut self,
        handle: VoiceHandle,
        rab: &crate::rel99_planner::RabPlan,
        request: &umts_iuh::ranap::rab::SetupOrModify,
        settings: Settings,
    ) -> Result<(), Error> {
        let reserved = self.reserved;
        let maximum = self.limits.maximum_storage_bytes;
        let binding = self.binding_mut(handle)?;
        if !binding.matches_rab(rab) {
            return Err(Error::Mapping);
        }
        let proposal = initialization::from_rab(rab, request, settings.supported())
            .map_err(Error::Proposal)?;
        let contract = initialization::contract_from_rab(rab, request, settings.supported())
            .map_err(Error::Proposal)?;
        let storage = contract.storage_bytes();
        let next = reserved
            .checked_sub(binding.contract_storage)
            .and_then(|value| value.checked_add(storage))
            .filter(|value| *value <= maximum)
            .ok_or(Error::Capacity)?;
        let settings = settings
            .restrict_to(proposal.versions)
            .map_err(Error::Negotiation)?;
        self.reserved = next;
        if let Err(error) =
            self.start_initialization(handle, &proposal.frames, proposal.encoding, settings)
        {
            self.reserved = reserved;
            return Err(error);
        }
        let binding = self.binding_mut(handle)?;
        binding.contract = Some(contract);
        binding.contract_storage = storage;
        Ok(())
    }

    pub fn start_initialization(
        &mut self,
        handle: VoiceHandle,
        frames: &[Initialization],
        encoding: ModeVersion,
        settings: Settings,
    ) -> Result<(), Error> {
        let limits = self.limits;
        let reserved = self.reserved;
        let binding = self.binding_mut(handle)?;
        let Negotiation::Receiver(receiver) = &binding.negotiation else {
            return Err(Error::Negotiation(negotiation::Error::Busy));
        };
        // TS 25.415 6.5.2 forbids SRNC reinitialization without RANAP modification.
        if binding.receiver_started
            || receiver.response_pending()
            || receiver.negotiated().is_some()
            || binding.queue.is_some()
        {
            return Err(Error::Negotiation(negotiation::Error::Busy));
        }
        if settings.supported().intersection(binding.supported) != Some(settings.supported()) {
            return Err(Error::Mapping);
        }
        let sender = Sender::new(frames, encoding, settings).map_err(Error::Negotiation)?;
        if binding.transport_capacity.is_some_and(|capacity| {
            sender.maximum_frame_bytes() > capacity.get()
                || sender.proposed_formats().maximum_frame_bytes() >= capacity.get()
        }) {
            return Err(Error::Capacity);
        }
        if sender.proposed_formats().maximum_payload_bytes() > limits.maximum_payload_bytes.get() {
            return Err(Error::Capacity);
        }
        binding
            .layout
            .validate(&binding.plan, sender.proposed_formats())
            .map_err(|_| Error::Mapping)?;
        let storage = sender.storage_bytes();
        let next_reserved = reserved
            .checked_add(storage)
            .filter(|value| *value <= limits.maximum_storage_bytes)
            .ok_or(Error::Capacity)?;
        binding.negotiation = Negotiation::Sender(sender);
        binding.initialization_storage = storage;
        self.reserved = next_reserved;
        Ok(())
    }

    pub fn acknowledgment(
        &mut self,
        handle: VoiceHandle,
        frame: Frame<'_>,
        now: Instant,
    ) -> Result<AckEffect, Error> {
        let limits = self.limits;
        let binding = self.binding_mut(handle)?;
        let Negotiation::Sender(sender) = &mut binding.negotiation else {
            return Err(Error::Negotiation(negotiation::Error::NotRequest));
        };
        let effect = sender
            .acknowledgment(frame, now)
            .map_err(Error::Negotiation)?;
        binding.finish_sender(limits)?;
        Ok(effect)
    }

    pub fn tick_initialization(&mut self, handle: VoiceHandle, now: Instant) -> Result<(), Error> {
        let limits = self.limits;
        let binding = self.binding_mut(handle)?;
        if let Negotiation::Sender(sender) = &mut binding.negotiation {
            sender.tick(now).map_err(Error::Negotiation)?;
            binding.finish_sender(limits)?;
        }
        Ok(())
    }

    pub fn initialization_status(
        &self,
        handle: VoiceHandle,
    ) -> Result<InitializationStatus, Error> {
        let binding = self
            .bindings
            .get(&handle.key)
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?;
        if let Negotiation::Sender(sender) = &binding.negotiation
            && let Some(failure) = sender.failure()
        {
            return Ok(InitializationStatus::Failed(failure));
        }
        Ok(
            if binding.negotiation.negotiated().is_some_and(|active| {
                binding
                    .queue
                    .as_ref()
                    .is_some_and(|queue| queue.negotiated().same_session(active))
                    && binding
                        .uplink
                        .as_ref()
                        .is_some_and(|queue| queue.negotiated().same_session(active))
            }) {
                InitializationStatus::Ready
            } else {
                InitializationStatus::Pending
            },
        )
    }

    pub fn radio_active(&self, handle: VoiceHandle) -> Result<bool, Error> {
        let binding = self
            .bindings
            .get(&handle.key)
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?;
        Ok(matches!(binding.radio, RadioBinding::Active { .. }))
    }

    #[cfg(test)]
    pub fn publish_initialization<E>(
        &mut self,
        handle: VoiceHandle,
        output: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<bool, ControlPublishError<E>> {
        self.publish_initialization_at(handle, Instant::now(), output, send)
    }

    #[cfg(test)]
    pub fn publish_initialization_at<E>(
        &mut self,
        handle: VoiceHandle,
        now: Instant,
        output: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<(), E>,
    ) -> Result<bool, ControlPublishError<E>> {
        let binding = self
            .binding_mut(handle)
            .map_err(ControlPublishError::Binding)?;
        if let Some(reply) = binding.control_reply {
            let bytes = reply.encode(output).map_err(|error| {
                ControlPublishError::Publication(negotiation::PublishError::Wire(error))
            })?;
            send(&output[..bytes]).map_err(|error| {
                ControlPublishError::Publication(negotiation::PublishError::Send(error))
            })?;
            binding.control_reply = None;
            return Ok(true);
        }
        let published = binding
            .negotiation
            .publish(now, output, send)
            .map_err(ControlPublishError::Publication)?;
        if published {
            let refunded = binding.finish_receiver_transition();
            binding.commit_media();
            self.reserved -= refunded;
        }
        Ok(published)
    }

    #[cfg(test)]
    pub fn publish_initialization_rtp<E>(
        &mut self,
        handle: VoiceHandle,
        timestamp: u32,
        iuup: &mut [u8],
        rtp: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<bool, E>,
    ) -> Result<bool, ControlPublishError<ControlRtpError<E>>> {
        self.publish_initialization_rtp_at(handle, Instant::now(), timestamp, iuup, rtp, send)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn publish_initialization_rtp_at<E>(
        &mut self,
        handle: VoiceHandle,
        now: Instant,
        timestamp: u32,
        iuup: &mut [u8],
        rtp: &mut [u8],
        send: impl FnOnce(&[u8]) -> Result<bool, E>,
    ) -> Result<bool, ControlPublishError<ControlRtpError<E>>> {
        let binding = self
            .binding_mut(handle)
            .map_err(ControlPublishError::Binding)?;
        let source = &mut binding.uplink_policy.rtp;
        if let Some(reply) = binding.control_reply {
            let bytes = reply.encode(iuup).map_err(|error| {
                ControlPublishError::Publication(negotiation::PublishError::Wire(error))
            })?;
            let sent = source
                .publish(timestamp, &iuup[..bytes], rtp, send)
                .map_err(|error| {
                    ControlPublishError::Publication(negotiation::PublishError::Send(
                        ControlRtpError::Publication(error),
                    ))
                })?;
            if sent {
                binding.control_reply = None;
            }
            return Ok(sent);
        }
        let published = binding
            .negotiation
            .publish(now, iuup, |payload| {
                match source
                    .publish(timestamp, payload, rtp, send)
                    .map_err(ControlRtpError::Publication)?
                {
                    true => Ok(()),
                    false => Err(ControlRtpError::Backpressure),
                }
            })
            .map_err(ControlPublishError::Publication)?;
        if published {
            let refunded = binding.finish_receiver_transition();
            binding.commit_media();
            self.reserved -= refunded;
        }
        Ok(published)
    }

    pub fn downlink(
        &mut self,
        handle: VoiceHandle,
        packet: &[u8],
        anchor: RtpAnchor,
    ) -> Result<(), Error> {
        let binding = self.binding_mut(handle)?;
        let rtp = umts_user_plane::rtp::decode(packet)
            .map_err(|error| Error::Ingress(media::IngressError::Wire(error)))?;
        let frame = umts_user_plane::iuup::decode(rtp.payload)
            .map_err(|error| Error::Ingress(media::IngressError::Wire(error)))?;
        let Frame::Data { rfci, .. } = frame else {
            return Err(Error::Mapping);
        };
        let due = anchor.map(rtp.timestamp).map_err(Error::Media)?;
        let rfci = Rfci::new(rfci).map_err(|_| Error::Mapping)?;
        if !binding.legal_time(due, rfci) {
            return Err(Error::Radio);
        }
        binding
            .active_queue_mut()
            .ok_or(Error::Negotiating)?
            .enqueue(due, frame)
            .map_err(Error::Media)
    }

    pub fn retire_before(&mut self, handle: VoiceHandle, before: ChipInstant) -> Result<(), Error> {
        self.binding_mut(handle)?
            .active_queue_mut()
            .ok_or(Error::Negotiating)?
            .retire_before(before)
            .map_err(Error::Media)
    }

    pub fn queued(&self, handle: VoiceHandle) -> Option<usize> {
        self.bindings
            .get(&handle.key)
            .filter(|binding| binding.handle == handle)?
            .active_queue()
            .map(MediaQueue::queued)
    }

    pub fn release(&mut self, handle: VoiceHandle) -> Result<(), Error> {
        if self.binding_mut(handle)?.transport_capacity.is_some() {
            return Err(Error::Attached);
        }
        let binding = self.bindings.remove(&handle.key).ok_or(Error::Mapping)?;
        for subflow in &binding.layout.subflows {
            self.radio
                .remove(&radio_key(binding.identity, subflow.bearer));
        }
        self.reserved -= self.limits.required_route_storage_bytes()?
            + binding.initialization_storage
            + binding.contract_storage;
        Ok(())
    }

    pub(crate) fn begin_media_drain(&mut self, handle: VoiceHandle) -> Result<(), Error> {
        let binding = self.binding_mut(handle)?;
        if matches!(binding.media, MediaLifetime::Active) {
            binding.media = MediaLifetime::Draining {
                discarded_uplink: 0,
            };
        }
        Ok(())
    }

    pub fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Error> {
        use crate::dedicated_bearers::DeliveryMapping;
        let invalid = Error::UplinkMapping;
        if delivery.metadata.scope != umts_security::ciphering::Scope::User(CnDomain::Cs)
            || delivery.metadata.bits != delivery.data.len()
        {
            return Err(invalid(UplinkMappingFailure::Metadata));
        }
        let route = self.radio.get(&radio_key(
            delivery.metadata.identity,
            delivery.metadata.bearer,
        ));
        let Some(&(key, index)) = route else {
            let DeliveryMapping::Pending { previous } = delivery.mapping else {
                return Err(invalid(UplinkMappingFailure::Route));
            };
            if previous.ue != delivery.metadata.identity.ue
                || previous.configuration >= delivery.metadata.identity.configuration
            {
                return Err(invalid(UplinkMappingFailure::Route));
            }
            let &(key, _) = self
                .radio
                .get(&radio_key(previous, delivery.metadata.bearer))
                .ok_or(invalid(UplinkMappingFailure::Route))?;
            let binding = self
                .bindings
                .get_mut(&key)
                .filter(|binding| binding.identity == previous)
                .ok_or(invalid(UplinkMappingFailure::Binding))?;
            if !matches!(binding.radio, RadioBinding::Active { .. }) {
                return Err(Error::Radio);
            }
            if binding.discard_draining_uplink() {
                return Ok(true);
            }
            return binding
                .pending_uplink
                .as_mut()
                .expect("voice pending owner")
                .push(delivery);
        };
        let binding = self
            .bindings
            .get_mut(&key)
            .ok_or(invalid(UplinkMappingFailure::Binding))?;
        if binding.discard_draining_uplink() {
            return Ok(true);
        }
        let pending = binding
            .pending_uplink
            .as_mut()
            .expect("voice pending owner");
        if pending.has_pending_for(delivery.metadata.identity) {
            return pending.push(delivery);
        }
        Self::ingest_uplink(binding, index, delivery)
    }

    fn ingest_uplink(
        binding: &mut Binding,
        index: usize,
        delivery: SduDelivery<'_>,
    ) -> Result<bool, Error> {
        use umts_rlc::transparent::Quality;
        let invalid = Error::UplinkMapping;
        let RadioBinding::Active { uplink: clock, .. } = binding.radio else {
            return Err(Error::Radio);
        };
        let active = binding.negotiation.negotiated().ok_or(Error::Negotiating)?;
        let origin = delivery
            .metadata
            .transparent_origin
            .ok_or(invalid(UplinkMappingFailure::Origin))?;
        if origin.start.epoch != delivery.metadata.received.epoch
            || origin.start.index > delivery.metadata.received.index
        {
            return Err(invalid(UplinkMappingFailure::Origin));
        }
        let subflow = &binding.layout.subflows[index];
        let configured = binding.plan.uplink_configured();
        let transport = configured
            .transport_ids()
            .nth(subflow.uplink_transport)
            .ok_or(invalid(UplinkMappingFailure::Transport))?;
        if origin.transport != transport {
            return Err(invalid(UplinkMappingFailure::Transport));
        }
        let definition = configured
            .definition(transport)
            .ok_or(invalid(UplinkMappingFailure::Transport))?;
        let format = definition
            .formats
            .get(origin.format)
            .ok_or(invalid(UplinkMappingFailure::Format))?;
        let (part, blocks, width) = match origin.unit {
            TransparentUnit::Joined if subflow.uplink_segmented => {
                (0, 1, format.block_bits * format.blocks)
            }
            TransparentUnit::Block { index } if !subflow.uplink_segmented => {
                (index, format.blocks, format.block_bits)
            }
            _ => return Err(invalid(UplinkMappingFailure::Segmentation)),
        };
        if width != delivery.data.len() || blocks == 0 || part >= blocks {
            return Err(invalid(UplinkMappingFailure::Geometry));
        }
        clock.validate(origin.start).map_err(Error::Uplink)?;
        let queue = binding
            .uplink
            .as_mut()
            .filter(|queue| queue.negotiated().same_session(active))
            .ok_or(Error::Negotiating)?;
        let quality = match delivery.metadata.quality {
            Quality::Correct => RadioQuality::Good,
            Quality::Erroneous => RadioQuality::Bad,
            Quality::NotReported => RadioQuality::NotReported,
        };
        match queue.push(Piece {
            source: origin.start,
            subflow: index,
            index: part,
            blocks: NonZeroUsize::new(blocks).ok_or(invalid(UplinkMappingFailure::Geometry))?,
            quality,
            data: delivery.data,
        }) {
            Ok(()) => Ok(true),
            Err(uplink_media::Error::Retired) => Ok(true),
            Err(uplink_media::Error::Capacity) => Ok(false),
            Err(error) => Err(Error::Uplink(error)),
        }
    }

    pub fn publish_uplink<E>(
        &mut self,
        handle: VoiceHandle,
        now: ChipInstant,
        send: impl FnOnce(&[u8]) -> Result<bool, E>,
    ) -> Result<uplink_media::Progress, UplinkPublishError<E>> {
        let binding = self
            .binding_mut(handle)
            .map_err(UplinkPublishError::Binding)?;
        if matches!(binding.media, MediaLifetime::Draining { .. }) {
            return Ok(uplink_media::Progress::Idle);
        }
        let mut pending = binding.pending_uplink.take().expect("voice pending owner");
        let drained = pending.drain(binding);
        binding.pending_uplink = Some(pending);
        if !drained.map_err(UplinkPublishError::Binding)? {
            return Ok(uplink_media::Progress::Waiting);
        }
        let RadioBinding::Active { uplink: clock, .. } = binding.radio else {
            return Err(UplinkPublishError::Binding(Error::Radio));
        };
        let active = binding
            .negotiation
            .negotiated()
            .ok_or(UplinkPublishError::Binding(Error::Negotiating))?;
        binding
            .uplink
            .as_mut()
            .filter(|queue| queue.negotiated().same_session(active))
            .ok_or(UplinkPublishError::Binding(Error::Negotiating))?
            .publish(now, clock, &mut binding.uplink_policy.rtp, send)
            .map_err(UplinkPublishError::Publication)
    }
}

#[derive(Debug)]
pub enum UplinkPublishError<E> {
    Binding(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        Error,
    ),
    Publication(uplink_media::PublishError<E>),
}

#[derive(Debug)]
pub enum ControlRtpError<E> {
    Backpressure,
    Publication(uplink_media::RtpPublishError<E>),
}

#[derive(Debug)]
pub enum ControlPublishError<E> {
    Binding(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        Error,
    ),
    Publication(negotiation::PublishError<E>),
}

impl TransparentTraffic for VoiceTraffic {
    fn offer(
        &self,
        identity: ReceiveIdentity,
        start: ChipInstant,
        bearer: BearerId,
    ) -> Option<TransparentOffer> {
        let (key, subflow) = self.radio.get(&radio_key(identity, bearer))?;
        let binding = self.bindings.get(key)?;
        let offer = binding.active_queue()?.offer(start, *subflow)?;
        if !binding.legal_time(start, offer.rfci) {
            return None;
        }
        let geometry = binding.layout.geometry(offer.rfci, *subflow)?;
        Some(TransparentOffer {
            payload_bits_per_block: geometry.width,
            blocks: geometry.blocks,
        })
    }

    fn publish(
        &mut self,
        grant: DownlinkGrant,
        admit: impl FnOnce(&[Bits<'_>]) -> Result<(), FrameBuildError>,
    ) -> Result<(), FrameBuildError> {
        let &(key, subflow) = self
            .radio
            .get(&radio_key(grant.identity, grant.bearer))
            .ok_or(BearerError::Mapping)?;
        let binding = self.bindings.get_mut(&key).ok_or(BearerError::Mapping)?;
        let offer = binding
            .active_queue()
            .and_then(|queue| queue.offer(grant.start, subflow))
            .ok_or(BearerError::Transport)?;
        if !binding.legal_time(grant.start, offer.rfci) {
            return Err(BearerError::Time.into());
        }
        let geometry = binding
            .layout
            .geometry(offer.rfci, subflow)
            .ok_or(BearerError::Mapping)?;
        let configured = binding.plan.downlink_configured();
        let transport = configured
            .transport_ids()
            .nth(binding.layout.subflows[subflow].transport)
            .ok_or(BearerError::Mapping)?;
        let definition = configured
            .definition(transport)
            .ok_or(BearerError::Mapping)?;
        if transport != grant.transport
            || definition.formats.get(grant.format).is_none_or(|format| {
                format.block_bits != geometry.width || format.blocks != geometry.blocks
            })
        {
            return Err(BearerError::Mapping.into());
        }
        binding
            .active_queue_mut()
            .ok_or(BearerError::Phase)?
            .publish(grant.start, subflow, |bits| {
                if geometry.segmented {
                    return admit(&[bits]);
                }
                let mut blocks = [Bits::empty(); MAX_BLOCKS];
                for (index, block) in blocks[..geometry.blocks].iter_mut().enumerate() {
                    *block = bits
                        .subspan(index * geometry.width..(index + 1) * geometry.width)
                        .map_err(|_| BearerError::Transport)?;
                }
                admit(&blocks[..geometry.blocks])
            })
            .map_err(|error| match error {
                media::PublishError::Admission(error) => error,
                media::PublishError::Media(_) => BearerError::Transport.into(),
            })
    }
}
