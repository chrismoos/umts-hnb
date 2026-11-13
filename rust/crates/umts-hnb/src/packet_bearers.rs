// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::{BearerError, BearerId, SduDelivery},
    dedicated_controller::DedicatedController,
    dedicated_rx::ReceiveIdentity,
    rel99_plan::CompiledRel99,
};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    num::NonZeroU64,
    sync::Arc,
};
use umts_security::ciphering::Scope;
use umts_types::{CnDomain, UeKey, bits::BITS_PER_BYTE};
use umts_user_plane::{
    gtpu::{PeerTeid, Teid},
    packet::{
        self, Datagrams, DiscardReason, DownlinkDisposition, PacketPlane, RouteHandle, RouteKey,
        SequenceState, Update,
    },
};

mod handoff;
pub use handoff::{PacketHandoff, PreparedPacketHandoff};

struct Binding {
    handle: RouteHandle,
    identity: ReceiveIdentity,
    bearer: BearerId,
    plan: Arc<CompiledRel99>,
    previous: Option<RadioAlias>,
    activity: u64,
}
struct RadioAlias {
    identity: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RadioKey {
    ue: UeKey,
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
#[derive(Debug)]
pub enum Error {
    Mapping,
    PendingAlias {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        current: ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        previous: ReceiveIdentity,
    },
    Capacity,
    Radio,
    Transport(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        packet::Error,
    ),
}
pub struct PacketBearers<T: Datagrams> {
    plane: PacketPlane<T>,
    bindings: BTreeMap<Teid, Binding>,
    uplink: HashMap<RadioKey, RouteHandle>,
    scratch: Vec<u8>,
    alias_cursor: Option<Teid>,
    activity_sequence: u64,
}
#[derive(Clone, Copy, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct Step {
    pub transport: packet::Step,
    pub radio_deferred: usize,
    pub aliases_retired: usize,
}
pub struct NetworkBinding {
    pub peer: SocketAddr,
    pub peer_teid: PeerTeid,
    pub sequence: SequenceState,
}
impl<T: Datagrams> PacketBearers<T> {
    pub fn new(plane: PacketPlane<T>) -> Self {
        let bindings = BTreeMap::new();
        let uplink = HashMap::with_capacity(plane.capacity());
        let scratch = vec![0; plane.maximum_payload_bytes()];
        Self {
            plane,
            bindings,
            uplink,
            scratch,
            alias_cursor: None,
            activity_sequence: 0,
        }
    }
    pub fn plane(&self) -> &PacketPlane<T> {
        &self.plane
    }
    #[cfg(test)]
    pub fn io_mut(&mut self) -> &mut T {
        self.plane.io_mut()
    }
    #[cfg(test)]
    pub fn prepare(
        &mut self,
        key: RouteKey,
        identity: ReceiveIdentity,
        bearer: BearerId,
        plan: Arc<CompiledRel99>,
        peer: SocketAddr,
        peer_teid: impl Into<PeerTeid>,
    ) -> Result<RouteHandle, Error> {
        self.prepare_bound(
            key,
            identity,
            bearer,
            plan,
            NetworkBinding {
                peer,
                peer_teid: peer_teid.into(),
                sequence: SequenceState::default(),
            },
        )
    }
    pub fn prepare_bound(
        &mut self,
        key: RouteKey,
        identity: ReceiveIdentity,
        bearer: BearerId,
        plan: Arc<CompiledRel99>,
        network: NetworkBinding,
    ) -> Result<RouteHandle, Error> {
        if key.connection.ue != identity.ue
            || key.connection.domain != CnDomain::Ps
            || plan.configuration().bearer(bearer.get()).is_none_or(|rb| {
                rb.domain != Some(CnDomain::Ps)
                    || !matches!(
                        rb.rlc,
                        umts_rrc::dedicated::Rel99Rlc::Acknowledged { .. }
                            | umts_rrc::dedicated::Rel99Rlc::Unacknowledged
                    )
            })
        {
            return Err(Error::Mapping);
        }
        let radio = radio_key(identity, bearer);
        if self.uplink.contains_key(&radio) {
            return Err(Error::Mapping);
        }
        if self.bindings.len() == self.plane.capacity() {
            return Err(Error::Capacity);
        }
        let handle = self
            .plane
            .prepare_with_state(key, network.peer, network.peer_teid, network.sequence)
            .map_err(Error::Transport)?;
        self.bindings.insert(
            handle.local_teid(),
            Binding {
                handle,
                identity,
                bearer,
                plan,
                previous: None,
                activity: 0,
            },
        );
        self.uplink.insert(radio, handle);
        Ok(handle)
    }
    pub fn activate(
        &mut self,
        handle: RouteHandle,
        receive: &mut DedicatedController,
    ) -> Result<(), Error> {
        let identity = self
            .bindings
            .get(&handle.local_teid())
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?
            .identity;
        receive
            .with_active_bank(identity, |bank| self.activate_on_bank(handle, bank))
            .map_err(|_| Error::Radio)?
    }
    pub(crate) fn activate_on_bank(
        &mut self,
        handle: RouteHandle,
        bank: &crate::dedicated_bearers::DedicatedBearers,
    ) -> Result<(), Error> {
        let binding = self
            .bindings
            .get(&handle.local_teid())
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?;
        let valid = bank.packet_binding_ready(binding.identity, binding.bearer, &binding.plan);
        if !valid {
            return Err(Error::Radio);
        }
        self.plane.activate(handle).map_err(Error::Transport)
    }
    pub fn rebind(
        &mut self,
        handle: RouteHandle,
        identity: ReceiveIdentity,
        bearer: BearerId,
        plan: Arc<CompiledRel99>,
        update: Update,
        receive: &mut DedicatedController,
    ) -> Result<(), Error> {
        let binding = self
            .bindings
            .get(&handle.local_teid())
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?;
        if binding.previous.is_some() {
            return Err(Error::Mapping);
        }
        if handle.key().connection.ue != identity.ue
            || plan.configuration().bearer(bearer.get()).is_none_or(|rb| {
                rb.domain != Some(CnDomain::Ps)
                    || !matches!(
                        rb.rlc,
                        umts_rrc::dedicated::Rel99Rlc::Acknowledged { .. }
                            | umts_rrc::dedicated::Rel99Rlc::Unacknowledged
                    )
            })
        {
            return Err(Error::Mapping);
        }
        let old_radio = radio_key(binding.identity, binding.bearer);
        let new_radio = radio_key(identity, bearer);
        if old_radio != new_radio && self.uplink.contains_key(&new_radio) {
            return Err(Error::Mapping);
        }
        let ready = receive
            .with_active_bank(identity, |bank| {
                bank.packet_binding_ready(identity, bearer, &plan)
            })
            .map_err(|_| Error::Radio)?;
        if !ready {
            return Err(Error::Radio);
        }
        self.uplink.try_reserve(1).map_err(|_| Error::Capacity)?;
        self.plane
            .update(handle, update)
            .map_err(Error::Transport)?;
        self.uplink.remove(&old_radio);
        self.uplink.insert(new_radio, handle);
        let binding = self
            .bindings
            .get_mut(&handle.local_teid())
            .expect("validated binding");
        binding.identity = identity;
        binding.bearer = bearer;
        binding.plan = plan;
        Ok(())
    }
    pub fn take_peer_failures(&mut self, mut visit: impl FnMut(RouteKey) -> bool) {
        self.plane.take_peer_failures(|handle| visit(handle.key()));
    }
    pub fn drain_released_volumes(&mut self, visit: impl FnMut(RouteKey, u64)) {
        self.plane.drain_released_volumes(visit);
    }
    pub fn sequence_state(&mut self, handle: RouteHandle) -> Result<SequenceState, Error> {
        self.plane.sequence_state(handle).map_err(Error::Transport)
    }
    /// Downlink octets queued for `handle` that have not been delivered over the radio.
    #[cfg(test)]
    pub fn pending_downlink_bytes(&mut self, handle: RouteHandle) -> Result<usize, Error> {
        self.plane
            .pending_downlink_bytes(handle)
            .map_err(Error::Transport)
    }
    pub(crate) fn retained_bearer(
        &self,
        handle: RouteHandle,
        identity: ReceiveIdentity,
        plan: &Arc<CompiledRel99>,
    ) -> Result<BearerId, Error> {
        let binding = self
            .bindings
            .get(&handle.local_teid())
            .filter(|binding| {
                binding.handle == handle
                    && binding.identity == identity
                    && Arc::ptr_eq(&binding.plan, plan)
            })
            .ok_or(Error::Mapping)?;
        Ok(binding.bearer)
    }
    pub fn release(&mut self, handle: RouteHandle) -> Result<(), Error> {
        let binding = self
            .bindings
            .get(&handle.local_teid())
            .filter(|binding| binding.handle == handle)
            .ok_or(Error::Mapping)?;
        let radio = radio_key(binding.identity, binding.bearer);
        let previous = binding
            .previous
            .as_ref()
            .map(|alias| radio_key(alias.identity, binding.bearer));
        self.plane.release(handle).map_err(Error::Transport)?;
        self.bindings.remove(&handle.local_teid());
        self.uplink.remove(&radio);
        if let Some(previous) = previous {
            self.uplink.remove(&previous);
        }
        Ok(())
    }
    pub fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Error> {
        if delivery.metadata.scope != Scope::User(CnDomain::Ps)
            || delivery.metadata.bits != delivery.data.len()
            || delivery.metadata.quality == umts_rlc::transparent::Quality::Erroneous
            || !delivery.data.len().is_multiple_of(BITS_PER_BYTE)
        {
            return Err(Error::Mapping);
        }
        let handle = *self
            .uplink
            .get(&radio_key(
                delivery.metadata.identity,
                delivery.metadata.bearer,
            ))
            .ok_or(Error::Mapping)?;
        let bytes = delivery.data.len() / BITS_PER_BYTE;
        if bytes > self.scratch.len() {
            return Err(Error::Capacity);
        }
        if self.plane.phase(handle).map_err(Error::Transport)? == packet::Phase::Prepared {
            return Ok(false);
        }
        delivery
            .data
            .copy_to(&mut self.scratch[..bytes])
            .map_err(|_| Error::Mapping)?;
        match self.plane.enqueue_uplink(handle, &self.scratch[..bytes]) {
            Ok(()) => {
                if let Some(binding) = self.bindings.get_mut(&handle.local_teid()) {
                    self.activity_sequence = self.activity_sequence.saturating_add(1);
                    binding.activity = self.activity_sequence;
                }
                Ok(true)
            }
            Err(packet::Error::Capacity) => Ok(false),
            Err(error) => Err(Error::Transport(error)),
        }
    }
    pub fn step(&mut self, receive: &mut DedicatedController) -> Result<Step, Error> {
        let mut radio_deferred = 0;
        let bindings = &mut self.bindings;
        let activity_sequence = &mut self.activity_sequence;
        let transport = self
            .plane
            .step(|handle, payload| {
                let Some(binding) = bindings
                    .get_mut(&handle.local_teid())
                    .filter(|binding| binding.handle == handle)
                else {
                    return DownlinkDisposition::Discarded(DiscardReason::InvalidRadioBinding);
                };
                let disposition = receive
                    .with_active_bank(binding.identity, |bank| {
                        if !bank.packet_binding_ready(
                            binding.identity,
                            binding.bearer,
                            &binding.plan,
                        ) {
                            return DownlinkDisposition::Blocked;
                        }
                        match bank.enqueue_sdu(binding.bearer, payload) {
                            Ok(()) => DownlinkDisposition::Accepted,
                            Err(BearerError::SduSize) => {
                                DownlinkDisposition::Discarded(DiscardReason::InvalidPayload)
                            }
                            Err(
                                BearerError::Am(umts_rlc::acknowledged::AmError::Capacity)
                                | BearerError::Um(umts_rlc::unacknowledged::UmError::Capacity)
                                | BearerError::Phase,
                            ) => DownlinkDisposition::Blocked,
                            Err(_) => {
                                DownlinkDisposition::Discarded(DiscardReason::InvalidRadioBinding)
                            }
                        }
                    })
                    .unwrap_or(DownlinkDisposition::Blocked);
                if disposition == DownlinkDisposition::Blocked {
                    radio_deferred += 1;
                }
                if matches!(
                    disposition,
                    DownlinkDisposition::Accepted | DownlinkDisposition::Blocked
                ) {
                    *activity_sequence = activity_sequence.saturating_add(1);
                    binding.activity = *activity_sequence;
                }
                disposition
            })
            .map_err(Error::Transport)?;
        let aliases_retired = usize::from(self.retire_next_alias(receive)?);
        Ok(Step {
            transport,
            radio_deferred,
            aliases_retired,
        })
    }
    pub(crate) fn activity(&self, key: UeKey) -> u64 {
        self.bindings
            .values()
            .filter(|binding| binding.identity.ue == key)
            .map(|binding| binding.activity)
            .max()
            .unwrap_or_default()
    }
}
