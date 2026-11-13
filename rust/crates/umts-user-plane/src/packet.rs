// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    WireError,
    gtpu::{self, MessageType, PeerTeid, Teid},
};
use std::{
    collections::BTreeMap,
    io,
    net::{SocketAddr, UdpSocket},
    num::{NonZeroU64, NonZeroUsize},
    sync::atomic::{AtomicU64, Ordering},
};
use umts_types::{CnDomain, ConnectionKey, UeKey};

// TS 29.060 7.2.1: an Echo Request on a path in use at most every 60 s. T3-RESPONSE and
// N3-REQUESTS are implementation choices (TS 29.060 11.2).
const ECHO_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const ECHO_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
const ECHO_ATTEMPTS: u8 = 5;
const MAX_UDP_BYTES: usize = 65_507;
const MAX_ROUTES: usize = 4096;
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

pub trait Datagrams {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>>;
    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool>;
}
pub trait BoundDatagrams: Datagrams {
    fn local_address(&self) -> io::Result<SocketAddr>;
}
pub struct UdpEndpoint(UdpSocket);
impl UdpEndpoint {
    pub fn bind(local: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(local)?;
        socket.set_nonblocking(true)?;
        Ok(Self(socket))
    }
    pub fn local_address(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }
}
impl Datagrams for UdpEndpoint {
    fn receive(&mut self, output: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        match self.0.recv_from(output) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn send(&mut self, bytes: &[u8], peer: SocketAddr) -> io::Result<bool> {
        match self.0.send_to(bytes, peer) {
            Ok(size) if size == bytes.len() => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "partial UDP datagram",
            )),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error),
        }
    }
}
impl BoundDatagrams for UdpEndpoint {
    fn local_address(&self) -> io::Result<SocketAddr> {
        self.local_address()
    }
}
#[derive(Clone, Copy)]
pub struct Limits {
    pub routes: NonZeroUsize,
    pub packets_per_route: NonZeroUsize,
    pub maximum_payload_bytes: NonZeroUsize,
    pub maximum_storage_bytes: usize,
    pub receive_budget: NonZeroUsize,
    pub route_visits: NonZeroUsize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    InvalidPayload,
    InvalidRadioBinding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownlinkDisposition {
    Accepted,
    Blocked,
    Discarded(DiscardReason),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RouteKey {
    pub connection: ConnectionKey,
    pub rab: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteHandle {
    key: RouteKey,
    local_teid: Teid,
    owner: NonZeroU64,
}
impl RouteHandle {
    pub fn key(self) -> RouteKey {
        self.key
    }
    pub fn local_teid(self) -> Teid {
        self.local_teid
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Prepared,
    Active,
}
#[derive(Debug)]
pub enum Error {
    Entropy,
    Capacity,
    Identity,
    Phase,
    Wire(WireError),
    Io(io::Error),
}
impl From<WireError> for Error {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}
impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

struct PacketSlot {
    bytes: Vec<u8>,
    length: usize,
    sequence: Option<u16>,
}
struct Queue {
    slots: Vec<PacketSlot>,
    head: usize,
    count: usize,
}
impl Queue {
    fn new(count: usize, bytes: usize) -> Self {
        Self {
            slots: (0..count)
                .map(|_| PacketSlot {
                    bytes: vec![0; bytes],
                    length: 0,
                    sequence: None,
                })
                .collect(),
            head: 0,
            count: 0,
        }
    }
    fn push(&mut self, bytes: &[u8]) -> bool {
        self.push_sequence(bytes, None)
    }
    fn push_sequence(&mut self, bytes: &[u8], sequence: Option<u16>) -> bool {
        if self.count == self.slots.len() {
            return false;
        }
        let tail = (self.head + self.count) % self.slots.len();
        let slot = &mut self.slots[tail];
        if bytes.len() > slot.bytes.len() {
            return false;
        }
        slot.bytes[..bytes.len()].copy_from_slice(bytes);
        slot.length = bytes.len();
        slot.sequence = sequence;
        self.count += 1;
        true
    }
    fn first(&self) -> Option<&[u8]> {
        (self.count != 0).then(|| &self.slots[self.head].bytes[..self.slots[self.head].length])
    }
    fn pop(&mut self) {
        self.slots[self.head].length = 0;
        self.head = (self.head + 1) % self.slots.len();
        self.count -= 1;
    }
}
/// TS 29.060 7.2.1 and 11.2 path supervision towards one GTP-U peer.
struct Path {
    peer: SocketAddr,
    last_request: Option<std::time::Duration>,
    outstanding: Option<(u16, std::time::Duration, u8)>,
}
struct Route {
    handle: RouteHandle,
    peer: SocketAddr,
    peer_teid: PeerTeid,
    phase: Phase,
    receive: Queue,
    transmit: Queue,
    next_sequence: u16,
    next_downlink: Option<u16>,
    // TS 29.060 7.3.7: the peer reported that no PDP context or RAB exists for this tunnel.
    peer_failed: bool,
    peer_failure_reported: bool,
    // TS 25.413 9.2.3.12: downlink octets accepted from the peer but never delivered.
    undelivered_bytes: u64,
}
impl Route {
    fn deliver_one(
        &mut self,
        downlink: &mut impl FnMut(RouteHandle, &[u8]) -> DownlinkDisposition,
        counters: &mut Counters,
        report: &mut Step,
    ) {
        let Some(payload) = self.receive.first() else {
            return;
        };
        match downlink(self.handle, payload) {
            DownlinkDisposition::Blocked => {
                counters.blocked = counters.blocked.saturating_add(1);
                return;
            }
            DownlinkDisposition::Accepted => {
                report.delivered += 1;
                counters.delivered = counters.delivered.saturating_add(1);
            }
            DownlinkDisposition::Discarded(reason) => {
                report.discarded += 1;
                self.undelivered_bytes = self
                    .undelivered_bytes
                    .saturating_add(self.receive.slots[self.receive.head].length as u64);
                match reason {
                    DiscardReason::InvalidPayload => {
                        counters.discarded_payload = counters.discarded_payload.saturating_add(1);
                    }
                    DiscardReason::InvalidRadioBinding => {
                        counters.discarded_binding = counters.discarded_binding.saturating_add(1);
                    }
                }
            }
        }
        if let Some(sequence) = self.receive.slots[self.receive.head].sequence {
            self.next_downlink = Some(sequence.wrapping_add(1));
        }
        self.receive.pop();
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SequenceState {
    pub next_uplink: u16,
    pub next_downlink: Option<u16>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Update {
    pub peer: Option<(SocketAddr, PeerTeid)>,
    pub next_uplink: Option<u16>,
    pub next_downlink: Option<u16>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub malformed: u64,
    pub unsupported: u64,
    pub unknown_tunnel: u64,
    pub wrong_peer: u64,
    pub before_activation: u64,
    pub receive_overflow: u64,
    pub echo_overflow: u64,
    pub error_indications_sent: u64,
    pub error_indications_received: u64,
    pub echo_requests_sent: u64,
    pub path_failures: u64,
    pub received: u64,
    pub sent: u64,
    pub delivered: u64,
    pub blocked: u64,
    pub discarded_payload: u64,
    pub discarded_binding: u64,
    pub retired_packets: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub received: usize,
    pub visited: usize,
    pub delivered: usize,
    pub sent: usize,
    pub discarded: usize,
}

pub struct PacketPlane<T: Datagrams> {
    owner: NonZeroU64,
    io: T,
    limits: Limits,
    routes: Vec<Option<Route>>,
    index: BTreeMap<Teid, usize>,
    next_teid: u32,
    cursor: usize,
    input: Vec<u8>,
    output: Vec<u8>,
    echoes: Queue,
    echo_peers: Vec<SocketAddr>,
    counters: Counters,
    storage_bytes: usize,
    local: std::net::IpAddr,
    signalling_sequence: u16,
    paths: Vec<Path>,
    released_volumes: Vec<(RouteKey, u64)>,
    started: std::time::Instant,
    test_time: Option<std::time::Duration>,
}
pub struct PreparedRouteRelease<'a, T: Datagrams> {
    plane: &'a mut PacketPlane<T>,
    handles: Vec<RouteHandle>,
}
impl<T: Datagrams> PreparedRouteRelease<'_, T> {
    pub fn handles(&self) -> &[RouteHandle] {
        &self.handles
    }
    pub fn commit(self) {
        for handle in self.handles {
            self.plane.release_validated(handle);
        }
    }
}
impl<T: Datagrams> PacketPlane<T> {
    pub fn owns_handle(&self, handle: RouteHandle) -> bool {
        handle.owner == self.owner
    }
    pub fn required_storage_bytes(limits: Limits) -> Result<usize, Error> {
        let packet_bytes = limits
            .maximum_payload_bytes
            .get()
            .checked_add(gtpu::HEADER_BYTES + gtpu::OPTIONAL_BYTES)
            .ok_or(Error::Capacity)?;
        if limits.routes.get() > MAX_ROUTES
            || !(gtpu::HEADER_BYTES + gtpu::OPTIONAL_BYTES + 2..=MAX_UDP_BYTES)
                .contains(&packet_bytes)
        {
            return Err(Error::Capacity);
        }
        let per_route = limits
            .packets_per_route
            .get()
            .checked_mul(2)
            .and_then(|slots| slots.checked_mul(packet_bytes + std::mem::size_of::<PacketSlot>()))
            .ok_or(Error::Capacity)?;
        let route_bytes = per_route
            .checked_add(std::mem::size_of::<Option<Route>>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<(Teid, usize)>()))
            .ok_or(Error::Capacity)?;
        let storage_bytes = limits
            .routes
            .get()
            .checked_mul(route_bytes)
            .and_then(|bytes| bytes.checked_add(packet_bytes + MAX_UDP_BYTES + 1))
            .and_then(|bytes| {
                bytes.checked_add(limits.receive_budget.get().checked_mul(
                    packet_bytes
                        + std::mem::size_of::<PacketSlot>()
                        + std::mem::size_of::<SocketAddr>(),
                )?)
            })
            .filter(|bytes| *bytes <= limits.maximum_storage_bytes)
            .ok_or(Error::Capacity)?;
        Ok(storage_bytes)
    }
    pub fn new(io: T, limits: Limits) -> Result<Self, Error> {
        let storage_bytes = Self::required_storage_bytes(limits)?;
        let packet_bytes = limits
            .maximum_payload_bytes
            .get()
            .checked_add(gtpu::HEADER_BYTES + gtpu::OPTIONAL_BYTES)
            .ok_or(Error::Capacity)?;
        let echoes = Queue::new(limits.receive_budget.get(), packet_bytes);
        let echo_peers = vec![SocketAddr::from(([0, 0, 0, 0], 0)); limits.receive_budget.get()];
        let owner = NEXT_OWNER
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or(Error::Capacity)?;
        let mut seed = [0; std::mem::size_of::<u32>()];
        getrandom::fill(&mut seed).map_err(|_| Error::Entropy)?;
        let next_teid = (u32::from_ne_bytes(seed) & (u32::MAX >> 1)).max(1);
        Ok(Self {
            owner,
            io,
            limits,
            routes: (0..limits.routes.get()).map(|_| None).collect(),
            index: BTreeMap::new(),
            next_teid,
            cursor: 0,
            input: vec![0; MAX_UDP_BYTES + 1],
            output: vec![0; packet_bytes],
            echoes,
            echo_peers,
            counters: Counters::default(),
            storage_bytes,
            local: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            signalling_sequence: 0,
            paths: Vec::with_capacity(limits.routes.get()),
            released_volumes: Vec::with_capacity(limits.routes.get()),
            started: std::time::Instant::now(),
            test_time: None,
        })
    }
    pub fn storage_bytes(&self) -> usize {
        self.storage_bytes
    }
    pub fn capacity(&self) -> usize {
        self.routes.len()
    }
    pub fn maximum_payload_bytes(&self) -> usize {
        self.limits.maximum_payload_bytes.get()
    }
    /// The address GTP-U peers send to, reported in Error Indications (TS 29.060 7.3.7).
    pub fn set_local_address(&mut self, local: std::net::IpAddr) {
        self.local = local;
    }
    fn now(&self) -> std::time::Duration {
        self.test_time.unwrap_or_else(|| self.started.elapsed())
    }
    #[cfg(test)]
    pub(crate) fn set_test_time(&mut self, now: std::time::Duration) {
        self.test_time = Some(now);
    }
    fn queue_signalling(&mut self, size: usize, peer: SocketAddr) -> bool {
        let tail = (self.echoes.head + self.echoes.count) % self.echoes.slots.len();
        if self.echoes.push(&self.output[..size]) {
            self.echo_peers[tail] = peer;
            true
        } else {
            self.counters.echo_overflow = self.counters.echo_overflow.saturating_add(1);
            false
        }
    }
    // TS 29.060 7.2.1: a path is in use while an active route uses it. An Echo Request goes
    // out at most every ECHO_INTERVAL. ECHO_ATTEMPTS unanswered requests fail the path and
    // every route on it (TS 29.060 11.2, TS 23.007).
    fn supervise_paths(&mut self) -> Result<(), Error> {
        let now = self.now();
        self.paths.retain(|path| {
            self.routes
                .iter()
                .flatten()
                .any(|route| route.peer == path.peer && route.phase == Phase::Active)
        });
        for route in self.routes.iter().flatten() {
            if route.phase == Phase::Active
                && !self.paths.iter().any(|path| path.peer == route.peer)
                && self.paths.len() < self.paths.capacity()
            {
                self.paths.push(Path {
                    peer: route.peer,
                    last_request: Some(now),
                    outstanding: None,
                });
            }
        }
        for index in 0..self.paths.len() {
            let path = &self.paths[index];
            let peer = path.peer;
            let (send, attempts) = match path.outstanding {
                Some((_, sent, attempts)) if now.saturating_sub(sent) >= ECHO_RESPONSE_TIMEOUT => {
                    if attempts >= ECHO_ATTEMPTS {
                        self.counters.path_failures = self.counters.path_failures.saturating_add(1);
                        for route in self.routes.iter_mut().flatten() {
                            if route.peer == peer {
                                route.peer_failed = true;
                            }
                        }
                        self.paths[index].outstanding = None;
                        self.paths[index].last_request = Some(now);
                        continue;
                    }
                    (true, attempts)
                }
                Some(_) => (false, 0),
                None => (
                    path.last_request
                        .is_none_or(|last| now.saturating_sub(last) >= ECHO_INTERVAL),
                    0,
                ),
            };
            if !send {
                continue;
            }
            let sequence = self.signalling_sequence;
            let size = gtpu::encode(
                gtpu::Packet {
                    kind: MessageType::EchoRequest,
                    teid: None,
                    sequence: Some(sequence),
                    n_pdu: None,
                    payload: &[],
                },
                &mut self.output,
            )?;
            if self.queue_signalling(size, peer) {
                self.signalling_sequence = self.signalling_sequence.wrapping_add(1);
                self.counters.echo_requests_sent =
                    self.counters.echo_requests_sent.saturating_add(1);
                let path = &mut self.paths[index];
                path.last_request = Some(now);
                path.outstanding = Some((sequence, now, attempts + 1));
            }
        }
        Ok(())
    }
    /// A failed route remains pending until the visitor accepts its release notification.
    pub fn take_peer_failures(&mut self, mut visit: impl FnMut(RouteHandle) -> bool) {
        for route in self.routes.iter_mut().flatten() {
            if route.peer_failed && !route.peer_failure_reported {
                route.peer_failure_reported = visit(route.handle);
            }
        }
    }
    /// Routes whose peer reported, by Error Indication, that no context exists for them.
    pub fn peer_failed_routes(&self, mut visit: impl FnMut(RouteHandle)) {
        for route in self.routes.iter().flatten() {
            if route.peer_failed {
                visit(route.handle);
            }
        }
    }
    pub fn counters(&self) -> Counters {
        self.counters
    }
    pub fn io(&self) -> &T {
        &self.io
    }
    pub fn io_mut(&mut self) -> &mut T {
        &mut self.io
    }
    pub fn prepare(
        &mut self,
        key: RouteKey,
        peer: SocketAddr,
        peer_teid: impl Into<PeerTeid>,
    ) -> Result<RouteHandle, Error> {
        self.prepare_with_state(key, peer, peer_teid, SequenceState::default())
    }
    pub fn prepare_with_state(
        &mut self,
        key: RouteKey,
        peer: SocketAddr,
        peer_teid: impl Into<PeerTeid>,
        state: SequenceState,
    ) -> Result<RouteHandle, Error> {
        if key.connection.domain != CnDomain::Ps
            || peer.port() == 0
            || peer.ip().is_unspecified()
            || self
                .routes
                .iter()
                .flatten()
                .any(|route| route.handle.key == key)
        {
            return Err(Error::Identity);
        }
        let slot = self
            .routes
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Capacity)?;
        let teid = Teid::new(self.next_teid).map_err(|_| Error::Capacity)?;
        let next = self.next_teid.checked_add(1).ok_or(Error::Capacity)?;
        let handle = RouteHandle {
            key,
            local_teid: teid,
            owner: self.owner,
        };
        let packet_bytes = self.output.len();
        let route = Route {
            handle,
            peer,
            peer_teid: peer_teid.into(),
            phase: Phase::Prepared,
            receive: Queue::new(self.limits.packets_per_route.get(), packet_bytes),
            transmit: Queue::new(self.limits.packets_per_route.get(), packet_bytes),
            next_sequence: state.next_uplink,
            next_downlink: state.next_downlink,
            peer_failed: false,
            peer_failure_reported: false,
            undelivered_bytes: 0,
        };
        self.routes[slot] = Some(route);
        self.index.insert(teid, slot);
        self.next_teid = next;
        Ok(handle)
    }
    fn route_mut(&mut self, handle: RouteHandle) -> Result<&mut Route, Error> {
        let index = *self.index.get(&handle.local_teid).ok_or(Error::Identity)?;
        self.routes[index]
            .as_mut()
            .filter(|route| route.handle == handle)
            .ok_or(Error::Identity)
    }
    pub fn phase(&mut self, handle: RouteHandle) -> Result<Phase, Error> {
        Ok(self.route_mut(handle)?.phase)
    }
    /// Downlink octets received from the peer and still queued, so not yet delivered.
    pub fn pending_downlink_bytes(&mut self, handle: RouteHandle) -> Result<usize, Error> {
        let route = self.route_mut(handle)?;
        Ok((0..route.receive.count)
            .map(|offset| {
                route.receive.slots[(route.receive.head + offset) % route.receive.slots.len()]
                    .length
            })
            .sum())
    }
    pub fn sequence_state(&mut self, handle: RouteHandle) -> Result<SequenceState, Error> {
        let route = self.route_mut(handle)?;
        Ok(SequenceState {
            next_uplink: route.next_sequence,
            next_downlink: route.next_downlink,
        })
    }
    pub fn update(&mut self, handle: RouteHandle, update: Update) -> Result<(), Error> {
        if update
            .peer
            .is_some_and(|(peer, _)| peer.port() == 0 || peer.ip().is_unspecified())
        {
            return Err(Error::Identity);
        }
        let route = self.route_mut(handle)?;
        if route.phase != Phase::Active {
            return Err(Error::Phase);
        }
        if let Some((peer, teid)) = update.peer {
            route.peer = peer;
            route.peer_teid = teid;
        }
        if let Some(next) = update.next_uplink {
            route.next_sequence = next;
        }
        if let Some(next) = update.next_downlink {
            route.next_downlink = Some(next);
        }
        Ok(())
    }
    pub fn activate(&mut self, handle: RouteHandle) -> Result<(), Error> {
        let route = self.route_mut(handle)?;
        if route.phase != Phase::Prepared {
            return Err(Error::Phase);
        }
        route.phase = Phase::Active;
        Ok(())
    }
    pub fn release(&mut self, handle: RouteHandle) -> Result<(), Error> {
        self.route_mut(handle)?;
        self.release_validated(handle);
        Ok(())
    }
    pub fn prepare_release_batch(
        &mut self,
        handles: Vec<RouteHandle>,
    ) -> Result<PreparedRouteRelease<'_, T>, Error> {
        let mut seen = [false; MAX_ROUTES];
        for handle in &handles {
            self.route_mut(*handle)?;
            let index = self.index[&handle.local_teid];
            if std::mem::replace(&mut seen[index], true) {
                return Err(Error::Identity);
            }
        }
        Ok(PreparedRouteRelease {
            plane: self,
            handles,
        })
    }
    fn release_validated(&mut self, handle: RouteHandle) {
        let index = self
            .index
            .remove(&handle.local_teid)
            .expect("validated handle");
        let route = self.routes[index].take().expect("registered route");
        self.counters.retired_packets = self
            .counters
            .retired_packets
            .saturating_add((route.receive.count + route.transmit.count) as u64);
        let queued: u64 = (0..route.receive.count)
            .map(|offset| {
                route.receive.slots[(route.receive.head + offset) % route.receive.slots.len()]
                    .length as u64
            })
            .sum();
        if self.released_volumes.len() == self.released_volumes.capacity() {
            self.released_volumes.remove(0);
        }
        self.released_volumes
            .push((handle.key, route.undelivered_bytes.saturating_add(queued)));
    }
    /// Undelivered downlink octets of each route released since the last call.
    pub fn drain_released_volumes(&mut self, mut visit: impl FnMut(RouteKey, u64)) {
        for (key, volume) in self.released_volumes.drain(..) {
            visit(key, volume);
        }
    }
    pub fn release_ue(&mut self, key: UeKey) -> usize {
        let mut released = 0;
        for index in 0..self.routes.len() {
            if let Some(handle) = self.routes[index]
                .as_ref()
                .filter(|route| route.handle.key.connection.ue == key)
                .map(|route| route.handle)
            {
                self.release(handle).expect("retained handle");
                released += 1;
            }
        }
        released
    }
    pub fn enqueue_uplink(&mut self, handle: RouteHandle, payload: &[u8]) -> Result<(), Error> {
        if payload.is_empty() || payload.len() > self.limits.maximum_payload_bytes.get() {
            return Err(Error::Capacity);
        }
        let route = self.route_mut(handle)?;
        if route.phase != Phase::Active {
            return Err(Error::Phase);
        }
        if !route.transmit.push(payload) {
            return Err(Error::Capacity);
        }
        Ok(())
    }
    pub fn step(
        &mut self,
        mut downlink: impl FnMut(RouteHandle, &[u8]) -> DownlinkDisposition,
    ) -> Result<Step, Error> {
        let mut report = Step::default();
        for _ in 0..self.limits.receive_budget.get() {
            let Some((length, peer)) = self.io.receive(&mut self.input)? else {
                break;
            };
            report.received += 1;
            if length > MAX_UDP_BYTES {
                self.counters.malformed = self.counters.malformed.saturating_add(1);
                continue;
            }
            let packet = match gtpu::decode(&self.input[..length]) {
                Ok(packet) => packet,
                // TS 29.060 11.1.1: an unsupported GTP version is answered with Version Not
                // Supported naming the version in use here, then the packet is discarded.
                Err(WireError::Version(_)) => {
                    self.counters.unsupported = self.counters.unsupported.saturating_add(1);
                    let size =
                        gtpu::version_not_supported(self.signalling_sequence, &mut self.output)?;
                    if self.queue_signalling(size, peer) {
                        self.signalling_sequence = self.signalling_sequence.wrapping_add(1);
                    }
                    continue;
                }
                // TS 29.060 6: a comprehension-required extension header that is not understood
                // is answered with Supported Extension Headers Notification. No extension
                // header is interpreted here, so the list is empty.
                Err(WireError::RequiredExtension(_)) => {
                    self.counters.unsupported = self.counters.unsupported.saturating_add(1);
                    let size = gtpu::supported_extension_headers(
                        &[],
                        self.signalling_sequence,
                        &mut self.output,
                    )?;
                    if self.queue_signalling(size, peer) {
                        self.signalling_sequence = self.signalling_sequence.wrapping_add(1);
                    }
                    continue;
                }
                Err(WireError::Unsupported(_)) => {
                    self.counters.unsupported = self.counters.unsupported.saturating_add(1);
                    continue;
                }
                Err(_) => {
                    self.counters.malformed = self.counters.malformed.saturating_add(1);
                    continue;
                }
            };
            match packet.kind {
                MessageType::EchoRequest => {
                    let size = gtpu::echo_response(
                        packet.sequence.expect("validated echo"),
                        &mut self.output,
                    )?;
                    let tail = (self.echoes.head + self.echoes.count) % self.echoes.slots.len();
                    if self.echoes.push(&self.output[..size]) {
                        self.echo_peers[tail] = peer;
                    } else {
                        self.counters.echo_overflow = self.counters.echo_overflow.saturating_add(1);
                    }
                }
                MessageType::Data => {
                    let local = Teid::new(packet.teid.expect("validated data").get()).ok();
                    let Some(index) = local.and_then(|id| self.index.get(&id)) else {
                        self.counters.unknown_tunnel =
                            self.counters.unknown_tunnel.saturating_add(1);
                        // TS 29.060 7.3.7: discard and return an Error Indication with the
                        // G-PDU's TEID and the address it was sent to.
                        let size = gtpu::error_indication(
                            packet.teid.expect("validated data").get(),
                            self.local,
                            self.signalling_sequence,
                            &mut self.output,
                        )?;
                        if self.queue_signalling(size, peer) {
                            self.signalling_sequence = self.signalling_sequence.wrapping_add(1);
                            self.counters.error_indications_sent =
                                self.counters.error_indications_sent.saturating_add(1);
                        }
                        continue;
                    };
                    let route = self.routes[*index].as_mut().expect("registered tunnel");
                    if peer != route.peer {
                        self.counters.wrong_peer = self.counters.wrong_peer.saturating_add(1);
                        continue;
                    }
                    if route.phase != Phase::Active {
                        self.counters.before_activation =
                            self.counters.before_activation.saturating_add(1);
                        continue;
                    }
                    if packet.payload.is_empty()
                        || packet.payload.len() > self.limits.maximum_payload_bytes.get()
                    {
                        self.counters.malformed = self.counters.malformed.saturating_add(1);
                        continue;
                    }
                    if route.receive.count == route.receive.slots.len() {
                        route.deliver_one(&mut downlink, &mut self.counters, &mut report);
                    }
                    if route.receive.push_sequence(packet.payload, packet.sequence) {
                        self.counters.received = self.counters.received.saturating_add(1);
                    } else {
                        self.counters.receive_overflow =
                            self.counters.receive_overflow.saturating_add(1);
                        route.undelivered_bytes = route
                            .undelivered_bytes
                            .saturating_add(packet.payload.len() as u64);
                    }
                }
                MessageType::EchoResponse => {
                    if let Some(path) = self.paths.iter_mut().find(|path| path.peer == peer)
                        && path
                            .outstanding
                            .is_some_and(|(sequence, _, _)| Some(sequence) == packet.sequence)
                    {
                        path.outstanding = None;
                    }
                }
                // TS 29.060 7.3.7 and TS 23.007: the peer has no context for the tunnel named
                // by TEID Data I, so the matching route is marked failed for release.
                MessageType::ErrorIndication => {
                    let Ok(indication) = gtpu::parse_error_indication(packet.payload) else {
                        self.counters.malformed = self.counters.malformed.saturating_add(1);
                        continue;
                    };
                    self.counters.error_indications_received =
                        self.counters.error_indications_received.saturating_add(1);
                    for route in self.routes.iter_mut().flatten() {
                        if route.peer == peer && route.peer_teid.get() == indication.teid {
                            route.peer_failed = true;
                        }
                    }
                }
                _ => self.counters.unsupported = self.counters.unsupported.saturating_add(1),
            }
        }
        self.supervise_paths()?;
        if let Some(bytes) = self.echoes.first()
            && self.io.send(bytes, self.echo_peers[self.echoes.head])?
        {
            self.echoes.pop();
            report.sent += 1;
        }
        for _ in 0..self.limits.route_visits.get().min(self.routes.len()) {
            let index = self.cursor;
            self.cursor = (self.cursor + 1) % self.routes.len();
            report.visited += 1;
            let Some(route) = self.routes[index].as_mut() else {
                continue;
            };
            if route.phase != Phase::Active {
                continue;
            }
            route.deliver_one(&mut downlink, &mut self.counters, &mut report);
            if let Some(payload) = route.transmit.first() {
                let size = gtpu::encode(
                    gtpu::Packet {
                        kind: MessageType::Data,
                        teid: Some(route.peer_teid),
                        sequence: Some(route.next_sequence),
                        n_pdu: None,
                        payload,
                    },
                    &mut self.output,
                )?;
                if self.io.send(&self.output[..size], route.peer)? {
                    route.transmit.pop();
                    route.next_sequence = route.next_sequence.wrapping_add(1);
                    report.sent += 1;
                    self.counters.sent = self.counters.sent.saturating_add(1);
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod binding_tests;

#[cfg(test)]
mod storage_sizing_tests {
    use super::*;
    struct Unused;
    impl Datagrams for Unused {
        fn receive(&mut self, _: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
            panic!("sizing cannot receive")
        }
        fn send(&mut self, _: &[u8], _: SocketAddr) -> io::Result<bool> {
            panic!("sizing cannot send")
        }
    }
    fn limits() -> Limits {
        Limits {
            routes: NonZeroUsize::new(2).unwrap(),
            packets_per_route: NonZeroUsize::new(3).unwrap(),
            maximum_payload_bytes: NonZeroUsize::new(1500).unwrap(),
            maximum_storage_bytes: usize::MAX,
            receive_budget: NonZeroUsize::MIN,
            route_visits: NonZeroUsize::MIN,
        }
    }
    #[test]
    fn required_storage_matches_actual_allocation_and_exact_budget_boundary() {
        let mut limits = limits();
        let bytes = PacketPlane::<Unused>::required_storage_bytes(limits).unwrap();
        assert!(bytes > MAX_UDP_BYTES);
        limits.maximum_storage_bytes = bytes;
        assert_eq!(
            PacketPlane::new(Unused, limits).unwrap().storage_bytes(),
            bytes
        );
        limits.maximum_storage_bytes -= 1;
        assert!(matches!(
            PacketPlane::<Unused>::required_storage_bytes(limits),
            Err(Error::Capacity)
        ));
        assert!(matches!(
            PacketPlane::new(Unused, limits),
            Err(Error::Capacity)
        ));
    }
    #[test]
    fn sizing_rejects_overflow_and_payloads_that_exceed_udp() {
        let mut limits = limits();
        limits.packets_per_route = NonZeroUsize::new(usize::MAX).unwrap();
        assert!(matches!(
            PacketPlane::<Unused>::required_storage_bytes(limits),
            Err(Error::Capacity)
        ));
        limits.packets_per_route = NonZeroUsize::MIN;
        limits.maximum_payload_bytes = NonZeroUsize::new(MAX_UDP_BYTES).unwrap();
        assert!(matches!(
            PacketPlane::<Unused>::required_storage_bytes(limits),
            Err(Error::Capacity)
        ));
    }
}
#[cfg(test)]
mod tests;
