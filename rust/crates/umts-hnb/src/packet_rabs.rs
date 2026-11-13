// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::{BearerId, SduDelivery},
    dedicated_controller::DedicatedController,
    dedicated_rx::ReceiveIdentity,
    packet_bearers::{self, NetworkBinding, PacketBearers},
    rel99_plan::CompiledRel99,
    signalling::AcceptedAdditionalSetup,
};
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    num::NonZeroU64,
    sync::Arc,
};
use umts_iuh::{
    Domain,
    ranap::rab::{Address, Association, Mode, Peer, Setup, SetupOrModify},
    session::{RabAuthorization, RabOperation, RabRequest},
};
use umts_rrc::dedicated::{RadioBearerSetupProcedure, SetupPhase};
use umts_types::{CnDomain, ConnectionKey};
use umts_user_plane::{
    gtpu::PeerTeid,
    packet::{BoundDatagrams, RouteHandle, RouteKey, SequenceState, Update},
};

const GTP_USER_PORT: u16 = 2152;

mod handoff;
mod modification;
pub use handoff::PreparedPacketRabHandoff;
pub(crate) use modification::{ModificationError, PacketModification};

#[derive(Debug)]
pub enum Error {
    Capacity,
    Identity,
    Conflict,
    Mapping,
    Unsupported,
    Protocol(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_iuh::Error,
    ),
    Binding(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        packet_bearers::Error,
    ),
    Io(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        io::Error,
    ),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    pub pending_cancelled: usize,
    pub active_retired: usize,
}

pub struct RadioBinding {
    identity: ReceiveIdentity,
    plan: Arc<CompiledRel99>,
    rab: u8,
    bearer: BearerId,
    nas: Option<u8>,
    evidence: RadioEvidence,
}
#[derive(Clone, Copy)]
enum RadioEvidence {
    Prepared,
    Completed,
    Additional,
}
impl RadioBinding {
    pub fn from_procedure(
        identity: ReceiveIdentity,
        plan: Arc<CompiledRel99>,
        procedure: &RadioBearerSetupProcedure,
        rab: u8,
    ) -> Result<Self, Error> {
        let evidence = match procedure.phase() {
            SetupPhase::Prepared => RadioEvidence::Prepared,
            SetupPhase::Complete => RadioEvidence::Completed,
            _ => return Err(Error::Mapping),
        };
        if identity.ue != procedure.key()
            || !Arc::ptr_eq(plan.configuration(), procedure.configuration())
        {
            return Err(Error::Mapping);
        }
        let mapping = procedure
            .rabs()
            .iter()
            .find(|item| item.identity == rab && item.domain == CnDomain::Ps)
            .ok_or(Error::Mapping)?;
        let [bearer] = mapping.bearers.as_slice() else {
            return Err(Error::Unsupported);
        };
        let bearer = BearerId::new(*bearer).map_err(|_| Error::Mapping)?;
        if plan
            .configuration()
            .bearer(bearer.get())
            .is_none_or(|rb| rb.domain != Some(CnDomain::Ps))
        {
            return Err(Error::Mapping);
        }
        Ok(Self {
            identity,
            plan,
            rab,
            bearer,
            nas: mapping.nas_synchronization,
            evidence,
        })
    }

    pub fn from_additional_setup(
        plan: &Arc<CompiledRel99>,
        accepted: &AcceptedAdditionalSetup,
        rab: u8,
    ) -> Result<Self, Error> {
        if accepted.key() != accepted.previous_identity().ue
            || accepted.identity().ue != accepted.key()
            || accepted.identity().configuration <= accepted.previous_identity().configuration
            || !Arc::ptr_eq(accepted.configuration(), plan.configuration())
        {
            return Err(Error::Mapping);
        }
        let mut matches = accepted
            .rabs()
            .iter()
            .filter(|item| item.identity == rab && item.domain == CnDomain::Ps);
        let mapping = matches.next().ok_or(Error::Mapping)?;
        if matches.next().is_some() {
            return Err(Error::Mapping);
        }
        let [bearer] = mapping.bearers.as_slice() else {
            return Err(Error::Unsupported);
        };
        let bearer = BearerId::new(*bearer).map_err(|_| Error::Mapping)?;
        if plan
            .configuration()
            .bearer(bearer.get())
            .is_none_or(|rb| rb.domain != Some(CnDomain::Ps))
        {
            return Err(Error::Mapping);
        }
        Ok(Self {
            identity: accepted.identity(),
            plan: plan.clone(),
            rab,
            bearer,
            nas: mapping.nas_synchronization,
            evidence: RadioEvidence::Additional,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ticket {
    attempt: NonZeroU64,
    request: RabRequest,
    route: RouteHandle,
}
impl Ticket {
    #[cfg(test)]
    pub fn request(self) -> RabRequest {
        self.request
    }
    pub fn key(self) -> RouteKey {
        self.route.key()
    }
}
struct Active {
    ticket: Ticket,
    requested: SetupOrModify,
    plan: Arc<CompiledRel99>,
}
enum Operation {
    New,
    Modify(Update),
}
struct Pending {
    ticket: Ticket,
    radio: RadioBinding,
    effective: SetupOrModify,
    operation: Operation,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct PacketRabs<T: BoundDatagrams> {
    bearers: PacketBearers<T>,
    active: HashMap<RouteKey, Active>,
    pending: HashMap<RouteKey, Pending>,
    address: Address,
    local: SocketAddr,
    versions: std::num::NonZeroU16,
    capacity: usize,
    next_attempt: Option<NonZeroU64>,
    cleanup_keys: Vec<RouteKey>,
    required_iu_reports: Vec<(RabRequest, u8, u32)>,
    // TS 25.413 8.2.2: released RABs that requested data volume reporting, with their volume
    // once the transport is gone.
    release_reports: Vec<RouteKey>,
    release_volumes: Vec<(RouteKey, u32)>,
    release_on_error_indication: bool,
}
impl<T: BoundDatagrams> PacketRabs<T> {
    pub fn new(
        bearers: PacketBearers<T>,
        advertised: Option<IpAddr>,
        versions: std::num::NonZeroU16,
    ) -> Result<Self, Error> {
        let local = bearers.plane().io().local_address().map_err(Error::Io)?;
        let advertised = advertised.unwrap_or(local.ip());
        if local.port() != GTP_USER_PORT
            || advertised.is_unspecified()
            || advertised.is_ipv4() != local.is_ipv4()
        {
            return Err(Error::Mapping);
        }
        let capacity = bearers.plane().capacity();
        let mut active = HashMap::new();
        let mut pending = HashMap::new();
        let mut cleanup_keys = Vec::new();
        active.try_reserve(capacity).map_err(|_| Error::Capacity)?;
        pending.try_reserve(capacity).map_err(|_| Error::Capacity)?;
        cleanup_keys
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Capacity)?;
        let mut required_iu_reports = Vec::new();
        required_iu_reports
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            bearers,
            active,
            pending,
            address: Address::from_ip(Domain::Ps, advertised).map_err(Error::Protocol)?,
            local,
            versions,
            capacity,
            next_attempt: Some(NonZeroU64::MIN),
            cleanup_keys,
            required_iu_reports,
            release_reports: Vec::with_capacity(capacity),
            release_volumes: Vec::with_capacity(capacity),
            release_on_error_indication: false,
        })
    }
    /// TS 25.413 8.2.2: the unsuccessfully transmitted DL data volume of a RAB released by
    /// RAB assignment that requested data volume reporting. Taken once.
    pub fn take_release_volume(&mut self, key: RouteKey) -> Option<u32> {
        let reports = &mut self.release_reports;
        let volumes = &mut self.release_volumes;
        self.bearers.drain_released_volumes(|route, volume| {
            if let Some(index) = reports.iter().position(|pending| *pending == route) {
                reports.swap_remove(index);
                if volumes.len() == volumes.capacity() {
                    volumes.remove(0);
                }
                volumes.push((route, u32::try_from(volume).unwrap_or(u32::MAX)));
            }
        });
        let index = self
            .release_volumes
            .iter()
            .position(|(route, _)| *route == key)?;
        Some(self.release_volumes.swap_remove(index).1)
    }
    #[cfg(test)]
    pub fn iu_volume_reports_pending(&self, request: umts_iuh::session::IuReleaseRequest) -> bool {
        self.required_iu_reports
            .iter()
            .any(|(rab, _, _)| request.same_connection(*rab))
    }
    /// Moves the data volume reports owed on `request` into `out` for Iu Release Complete.
    pub fn take_iu_volume_reports(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
        out: &mut Vec<umts_iuh::ranap::release::VolumeReport>,
    ) {
        self.required_iu_reports.retain(|(rab, id, volume)| {
            if request.same_connection(*rab) && out.len() < out.capacity() {
                out.push(umts_iuh::ranap::release::VolumeReport {
                    rab: *id,
                    unsuccessfully_transmitted: *volume,
                });
                false
            } else {
                true
            }
        });
    }
    pub fn has_connection(&self, connection: ConnectionKey) -> bool {
        self.pending.keys().any(|key| key.connection == connection)
            || self.active.keys().any(|key| key.connection == connection)
    }
    #[cfg(test)]
    pub fn counters(&self) -> umts_user_plane::packet::Counters {
        self.bearers.plane().counters()
    }
    #[cfg(test)]
    pub fn local_address(&self) -> SocketAddr {
        self.local
    }
    pub(crate) fn activity(&self, key: umts_types::UeKey) -> u64 {
        self.bearers.activity(key)
    }
    #[cfg(test)]
    pub fn io_mut(&mut self) -> &mut T {
        self.bearers.io_mut()
    }
    #[cfg(test)]
    pub fn requested(&self, key: RouteKey) -> Option<&SetupOrModify> {
        self.active.get(&key).map(|entry| &entry.requested)
    }
    #[cfg(test)]
    pub fn sequence_state(&mut self, key: RouteKey) -> Result<SequenceState, Error> {
        let route = self.active.get(&key).ok_or(Error::Identity)?.ticket.route;
        self.bearers.sequence_state(route).map_err(Error::Binding)
    }
    pub fn prepare(
        &mut self,
        authorization: RabAuthorization<'_>,
        item: &SetupOrModify,
        radio: RadioBinding,
    ) -> Result<Ticket, Error> {
        let request = authorization.request();
        if authorization.rab() != item.first.id.get()
            || authorization.operation() != RabOperation::SetupOrModify
        {
            return Err(Error::Identity);
        }
        if request.domain() != Domain::Ps
            || request.key() != radio.identity.ue
            || radio.rab != item.first.id.get()
        {
            return Err(Error::Identity);
        }
        let key = RouteKey {
            connection: ConnectionKey {
                ue: request.key(),
                domain: CnDomain::Ps,
            },
            rab: item.first.id.get(),
        };
        if self.active.iter().any(|(route, entry)| {
            route.connection == key.connection && !entry.ticket.request.same_connection(request)
        }) || self.pending.iter().any(|(route, entry)| {
            route.connection == key.connection && !entry.ticket.request.same_connection(request)
        }) {
            return Err(Error::Identity);
        }
        if self.pending.contains_key(&key) {
            return Err(Error::Conflict);
        }
        if self.pending.len() == self.capacity {
            return Err(Error::Capacity);
        }
        let effective = match self.active.get(&key) {
            Some(previous) => item.merged(&previous.requested).map_err(Error::Protocol)?,
            None => item.clone(),
        };
        effective
            .validate_new(Domain::Ps)
            .map_err(Error::Protocol)?;
        match radio.evidence {
            RadioEvidence::Completed => {
                let active = self.active.get(&key).ok_or(Error::Mapping)?;
                if !Arc::ptr_eq(&active.plan, &radio.plan)
                    || effective.first.parameters != active.requested.first.parameters
                    || effective.first.nas_synchronization
                        != active.requested.first.nas_synchronization
                    || effective.first.user_plane != active.requested.first.user_plane
                    || effective.first.handover != active.requested.first.handover
                {
                    return Err(Error::Mapping);
                }
            }
            RadioEvidence::Additional => {
                if self.active.contains_key(&key) {
                    return Err(Error::Mapping);
                }
            }
            RadioEvidence::Prepared => {}
        }
        let nas = effective.first.nas_synchronization_value();
        if nas != radio.nas {
            return Err(Error::Mapping);
        }
        let user_plane = effective
            .first
            .user_plane
            .as_ref()
            .expect("validated user plane");
        if user_plane.mode != Mode::Transparent
            || user_plane.version_mask() & self.versions.get() == 0
        {
            return Err(Error::Unsupported);
        }
        let Peer::Ps { address, teid } = effective
            .first
            .transport
            .as_ref()
            .expect("validated transport")
            .ip_peer(Domain::Ps)
            .map_err(Error::Protocol)?
        else {
            return Err(Error::Identity);
        };
        if address.is_unspecified() || address.is_ipv4() != self.local.is_ipv4() {
            return Err(Error::Mapping);
        }
        let network = NetworkBinding {
            peer: SocketAddr::new(address, GTP_USER_PORT),
            peer_teid: PeerTeid::new(teid),
            sequence: SequenceState {
                next_uplink: effective.second.uplink_gtp_sequence.unwrap_or(0),
                next_downlink: effective.second.downlink_gtp_sequence,
            },
        };
        let attempt = self.next_attempt.ok_or(Error::Capacity)?;
        self.pending.try_reserve(1).map_err(|_| Error::Capacity)?;
        let (route, operation) = match self.active.get(&key) {
            Some(previous) => (
                previous.ticket.route,
                Operation::Modify(Update {
                    peer: item
                        .first
                        .transport
                        .as_ref()
                        .map(|_| (network.peer, network.peer_teid)),
                    // An absent modification field must not rewind the live sequence to its setup seed.
                    next_uplink: item.second.uplink_gtp_sequence,
                    next_downlink: item.second.downlink_gtp_sequence,
                }),
            ),
            None => {
                let route = self
                    .bearers
                    .prepare_bound(
                        key,
                        radio.identity,
                        radio.bearer,
                        radio.plan.clone(),
                        network,
                    )
                    .map_err(Error::Binding)?;
                (route, Operation::New)
            }
        };
        let ticket = Ticket {
            attempt,
            request,
            route,
        };
        self.next_attempt = attempt.checked_add(1);
        self.pending.insert(
            key,
            Pending {
                ticket,
                radio,
                effective,
                operation,
            },
        );
        Ok(ticket)
    }
    pub fn complete(
        &mut self,
        ticket: Ticket,
        authorization: RabAuthorization<'_>,
        receive: &mut DedicatedController,
    ) -> Result<Setup, Error> {
        let key = ticket.key();
        if authorization.request() != ticket.request
            || authorization.rab() != key.rab
            || authorization.operation() != RabOperation::SetupOrModify
        {
            return Err(Error::Identity);
        }
        let setup = self.setup_outcome(ticket);
        self.active.try_reserve(1).map_err(|_| Error::Capacity)?;
        let pending = self
            .pending
            .get(&key)
            .filter(|pending| pending.ticket == ticket)
            .ok_or(Error::Identity)?;
        match pending.operation {
            Operation::New => self
                .bearers
                .activate(ticket.route, receive)
                .map_err(Error::Binding)?,
            Operation::Modify(update) => self
                .bearers
                .rebind(
                    ticket.route,
                    pending.radio.identity,
                    pending.radio.bearer,
                    pending.radio.plan.clone(),
                    update,
                    receive,
                )
                .map_err(Error::Binding)?,
        }
        let pending = self
            .pending
            .remove(&key)
            .expect("validated pending binding");
        self.active.insert(
            key,
            Active {
                ticket,
                requested: pending.effective,
                plan: pending.radio.plan,
            },
        );
        Ok(setup)
    }
    fn setup_outcome(&self, ticket: Ticket) -> Setup {
        let mut setup = Setup::new(ticket.key().rab);
        setup.address = Some(self.address.clone());
        setup.association = Some(Association::gtp(ticket.route.local_teid().get()));
        setup
    }
    pub fn cancel(&mut self, ticket: Ticket) -> Result<(), Error> {
        let pending = self
            .pending
            .get(&ticket.key())
            .filter(|pending| pending.ticket == ticket)
            .ok_or(Error::Identity)?;
        if matches!(pending.operation, Operation::New) {
            self.bearers.release(ticket.route).map_err(Error::Binding)?;
        }
        self.pending.remove(&ticket.key());
        Ok(())
    }
    pub fn retire_attempt(&mut self, ticket: Ticket) -> Result<bool, Error> {
        if !self.bearers.plane().owns_handle(ticket.route) {
            return Err(Error::Identity);
        }
        let key = ticket.key();
        // Old active cleanup must not cancel a newer pending modification of the same route.
        if let Some(pending) = self.pending.get(&key) {
            if pending.ticket != ticket {
                if self
                    .active
                    .get(&key)
                    .is_some_and(|active| active.ticket == ticket)
                {
                    return Err(Error::Conflict);
                }
                return Ok(false);
            }
            self.cancel(ticket)?;
            return Ok(true);
        }
        if self
            .active
            .get(&key)
            .is_none_or(|active| active.ticket != ticket)
        {
            return Ok(false);
        }
        self.release_transport(key)?;
        Ok(true)
    }
    // Removing transport is not evidence that the UE's radio bearer has been released.
    pub fn release_transport(&mut self, key: RouteKey) -> Result<Option<SequenceState>, Error> {
        if let Some(ticket) = self.pending.get(&key).map(|pending| pending.ticket) {
            self.cancel(ticket)?;
        }
        let Some(active) = self.active.get(&key) else {
            return Ok(None);
        };
        let sequence = self
            .bearers
            .sequence_state(active.ticket.route)
            .map_err(Error::Binding)?;
        self.bearers
            .release(active.ticket.route)
            .map_err(Error::Binding)?;
        self.active.remove(&key);
        Ok(Some(sequence))
    }
    #[cfg(test)]
    pub fn reconcile_core<C: umts_core::Transport>(
        &mut self,
        core: &umts_core::Gateway<C>,
    ) -> Result<Reconciled, Error> {
        let mut result = Reconciled::default();
        self.required_iu_reports.retain(|(rab, _, _)| {
            core.iu_release_for_rab(*rab)
                .is_none_or(|request| !core.peer_disconnected_release(request))
        });
        self.cleanup_keys.clear();
        for (key, pending) in &self.pending {
            if core.owns_rab_request(pending.ticket.request)
                && core
                    .authorize_rab(pending.ticket.request, key.rab, RabOperation::SetupOrModify)
                    .is_err()
            {
                self.cleanup_keys.push(*key);
            }
        }
        for index in 0..self.cleanup_keys.len() {
            let key = self.cleanup_keys[index];
            let ticket = self
                .pending
                .get(&key)
                .expect("collected pending RAB")
                .ticket;
            self.cancel(ticket)?;
            result.pending_cancelled += 1;
        }
        self.cleanup_keys.clear();
        for (key, active) in &self.active {
            if core.owns_rab_request(active.ticket.request)
                && !core.rab_connection_current(active.ticket.request)
            {
                self.cleanup_keys.push(*key);
            }
        }
        for index in 0..self.cleanup_keys.len() {
            let key = self.cleanup_keys[index];
            let active = self.active.get(&key).expect("collected active Iu route");
            if core
                .iu_release_for_rab(active.ticket.request)
                .is_some_and(|request| !core.peer_disconnected_release(request))
                && active.requested.second.reporting
                    == Some(umts_iuh::ranap::rab::Reporting::Report)
                && !self
                    .required_iu_reports
                    .iter()
                    .any(|&(request, rab, _)| request == active.ticket.request && rab == key.rab)
            {
                if self.required_iu_reports.len() == self.capacity {
                    return Err(Error::Capacity);
                }
                // TS 25.413 9.2.1.49: downlink data still queued here was never delivered.
                let volume = self
                    .bearers
                    .pending_downlink_bytes(active.ticket.route)
                    .map_err(Error::Binding)?;
                self.required_iu_reports.push((
                    active.ticket.request,
                    key.rab,
                    u32::try_from(volume).unwrap_or(u32::MAX),
                ));
            }
            self.release_transport(key)?;
            result.active_retired += 1;
        }
        self.cleanup_keys.clear();
        Ok(result)
    }
    pub fn with_error_indication_release(mut self, enabled: bool) -> Self {
        self.release_on_error_indication = enabled;
        self
    }
    /// Visits each active RAB whose GTP-U peer reported, by Error Indication, that it has no
    /// context for the tunnel. A notification remains pending until accepted.
    pub fn take_peer_failures(&mut self, mut visit: impl FnMut(RouteKey) -> bool) {
        if !self.release_on_error_indication {
            return;
        }
        let active = &self.active;
        self.bearers
            .take_peer_failures(|key| active.contains_key(&key) && visit(key));
    }
    pub fn uplink(&mut self, delivery: SduDelivery<'_>) -> Result<bool, Error> {
        self.bearers.uplink(delivery).map_err(Error::Binding)
    }
    pub fn step(
        &mut self,
        receive: &mut DedicatedController,
    ) -> Result<packet_bearers::Step, Error> {
        self.bearers.step(receive).map_err(Error::Binding)
    }
}
