// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    ContextId, Domain, Error, NasSelector,
    hnbap::{self, Capabilities, HnbParameters, RegistrationCause, UeIdentity},
    ranap::{self, Location},
    rua,
    wire::{Limits, Protocol},
};
use std::{
    collections::HashMap,
    num::{NonZeroU64, NonZeroUsize},
    sync::atomic::{AtomicU64, Ordering},
};
use umts_types::{RadioEpoch, UeKey};

mod rab;
mod release;
mod security;
use rab::PendingAssignment;
pub use rab::{RabAuthorization, RabOperation, RabOutcome, RabRequest};
pub use release::IuReleaseRequest;
use security::PendingSecurity;
pub use security::SecurityRequest;

fn next_session_owner() -> Result<NonZeroU64, Error> {
    static NEXT: AtomicU64 = AtomicU64::new(NonZeroU64::MIN.get());
    NEXT.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    })
    .map(|value| NonZeroU64::new(value).expect("nonzero session owner"))
    .map_err(|_| Error::Capacity)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AssociationId(pub NonZeroU64);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ControlTime(pub u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HnbState {
    Idle,
    Awaiting { deadline: ControlTime },
    Registered { rnc: u16 },
    Backoff { until: ControlTime },
    Draining { backoff_until: Option<ControlTime> },
    Failed,
}
#[derive(Clone, Copy, Debug)]
pub struct SessionLimits {
    pub maximum_ues: NonZeroUsize,
    pub maximum_rab_requests_per_domain: NonZeroUsize,
    pub maximum_rab_release_requests_per_domain: NonZeroUsize,
    pub request_timeout_ms: NonZeroU64,
    pub codec: Limits,
}
// Frame scope is local ownership metadata and is never encoded into HNBAP or RUA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameScope {
    Hnb,
    Ue { key: UeKey, domain: Option<Domain> },
}
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub association: AssociationId,
    pub scope: FrameScope,
    pub protocol: Protocol,
    pub data: Vec<u8>,
}
impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("association", &self.association)
            .field("scope", &self.scope)
            .field("protocol", &self.protocol)
            .field("data_bytes", &self.data.len())
            .finish()
    }
}
pub struct UplinkNas<'a> {
    pub key: UeKey,
    pub domain: Domain,
    pub nas: &'a [u8],
    pub selector: Option<NasSelector>,
    pub cause: rua::EstablishmentCause,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    IuRelease {
        request: IuReleaseRequest,
        received_at: ControlTime,
        command: ranap::release::Command,
        notify_ies: Vec<u16>,
    },
    RabAssignment {
        request: RabRequest,
        received_at: ControlTime,
        assignment: ranap::rab::Assignment,
        notify_ies: Vec<u16>,
    },
    SecurityCommand {
        request: SecurityRequest,
        received_at: ControlTime,
        command: ranap::SecurityCommand,
        notify_ies: Vec<u16>,
    },
    HnbAccepted {
        rnc: u16,
    },
    HnbRejected {
        cause: hnbap::Cause,
        backoff_until: Option<ControlTime>,
    },
    AssociationLost {
        owners: Vec<UeKey>,
        backoff_until: Option<ControlTime>,
    },
    UeAccepted {
        key: UeKey,
        context: ContextId,
        membership: Option<rua::Membership>,
    },
    UeRejected {
        key: UeKey,
        cause: hnbap::Cause,
    },
    CancelledRegistrationAccepted {
        key: UeKey,
        context: ContextId,
    },
    UeDeregistered {
        key: UeKey,
        context: ContextId,
        cause: hnbap::Cause,
    },
    Ranap {
        key: Option<UeKey>,
        domain: Option<Domain>,
        message: ranap::Decoded,
    },
    Disconnected {
        request: IuReleaseRequest,
        key: UeKey,
        domain: Domain,
        cause: rua::Cause,
        final_ranap: Option<ranap::Decoded>,
    },
    HnbapError(hnbap::Message),
    RuaError(rua::Message),
    /// TS 25.413 8.26.2.1: a CN RESET closed every Iu connection of `domain`. Each request is
    /// released like a peer disconnect, then the acknowledgement is published.
    Reset {
        domain: Domain,
        releases: Vec<IuReleaseRequest>,
    },
    /// TS 25.413 10.3.4.2: a RANAP message on this connection could not be used. The session
    /// holds the ERROR INDICATION until the gateway takes it.
    ProtocolError {
        key: UeKey,
        domain: Domain,
        cause: u16,
    },
    Notified {
        protocol: Protocol,
        notify_ies: Vec<u16>,
        event: Box<Event>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    IuRelease,
    RabAssignment,
    SecurityCommand,
    HnbAccepted,
    HnbRejected,
    AssociationLost,
    UeAccepted,
    UeRejected,
    CancelledRegistrationAccepted,
    UeDeregistered,
    Ranap { procedure: u8 },
    Disconnected,
    HnbapError,
    RuaError,
    Reset,
    ProtocolError,
    Notified(Protocol),
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::IuRelease { .. } => EventKind::IuRelease,
            Self::RabAssignment { .. } => EventKind::RabAssignment,
            Self::SecurityCommand { .. } => EventKind::SecurityCommand,
            Self::HnbAccepted { .. } => EventKind::HnbAccepted,
            Self::HnbRejected { .. } => EventKind::HnbRejected,
            Self::AssociationLost { .. } => EventKind::AssociationLost,
            Self::UeAccepted { .. } => EventKind::UeAccepted,
            Self::UeRejected { .. } => EventKind::UeRejected,
            Self::CancelledRegistrationAccepted { .. } => EventKind::CancelledRegistrationAccepted,
            Self::UeDeregistered { .. } => EventKind::UeDeregistered,
            Self::Ranap { message, .. } => EventKind::Ranap {
                procedure: message.message.procedure_code(),
            },
            Self::Disconnected { .. } => EventKind::Disconnected,
            Self::HnbapError(_) => EventKind::HnbapError,
            Self::RuaError(_) => EventKind::RuaError,
            Self::Reset { .. } => EventKind::Reset,
            Self::ProtocolError { .. } => EventKind::ProtocolError,
            Self::Notified { protocol, .. } => EventKind::Notified(*protocol),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    Session(Error),
    Downstream(E),
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Registration {
    Pending {
        deadline: ControlTime,
        cancelled: bool,
    },
    Active {
        context: ContextId,
        cancelled: bool,
    },
    Retiring {
        context: ContextId,
    },
    Closing {
        context: ContextId,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkState {
    Closed,
    Open,
    Releasing,
}
struct Ue {
    key: UeKey,
    identity: UeIdentity,
    registration: Registration,
    domains: [LinkState; 2],
    connections: [u64; 2],
    signalling_release_requested: [bool; 2],
    // TS 25.413 8.4.2 lets CN ignore inactivity, not a subsequent UE signaling release.
    inactivity_release_requested: [bool; 2],
    membership: Option<rua::Membership>,
    security_requests: [Option<PendingSecurity>; 2],
    release_requests: [Option<IuReleaseRequest>; 2],
    peer_disconnected: [bool; 2],
    rab_requests: [Vec<PendingAssignment>; 2],
    security_algorithms: [Option<(ranap::Algorithm, Option<ranap::Algorithm>)>; 2],
}

#[derive(Clone, Copy, Debug)]
struct PendingErrorIndication {
    key: UeKey,
    context: ContextId,
    domain: Domain,
    cause: u16,
    ie: Option<u16>,
    procedure: Option<(u8, crate::wire::Criticality)>,
}

pub struct Session {
    owner: NonZeroU64,
    epoch: RadioEpoch,
    association: AssociationId,
    changed_at: ControlTime,
    parameters: HnbParameters,
    limits: SessionLimits,
    pending_error_indication: Option<PendingErrorIndication>,
    tmsi_registration: bool,
    state: HnbState,
    ues: HashMap<UeKey, Ue>,
    identities: HashMap<UeIdentity, UeKey>,
    contexts: HashMap<ContextId, UeKey>,
    next_security_request: Option<NonZeroU64>,
    next_rab_request: Option<NonZeroU64>,
    next_connection: Option<NonZeroU64>,
}
impl Session {
    pub fn epoch(&self) -> RadioEpoch {
        self.epoch
    }
    pub fn new(
        epoch: RadioEpoch,
        association: AssociationId,
        parameters: HnbParameters,
        limits: SessionLimits,
        now: ControlTime,
    ) -> Result<Self, Error> {
        parameters_message(&parameters).encode(limits.codec)?;
        let mut ues = HashMap::new();
        let mut identities = HashMap::new();
        let mut contexts = HashMap::new();
        for result in [
            ues.try_reserve(limits.maximum_ues.get()),
            identities.try_reserve(limits.maximum_ues.get()),
            contexts.try_reserve(limits.maximum_ues.get()),
        ] {
            result.map_err(|_| Error::Capacity)?;
        }
        Ok(Self {
            owner: next_session_owner()?,
            epoch,
            association,
            changed_at: now,
            parameters,
            limits,
            pending_error_indication: None,
            tmsi_registration: false,
            state: HnbState::Idle,
            ues,
            identities,
            contexts,
            next_security_request: Some(NonZeroU64::MIN),
            next_rab_request: Some(NonZeroU64::MIN),
            next_connection: Some(NonZeroU64::MIN),
        })
    }
    /// Permit TMSI/LAI and P-TMSI/RAI UE identities in UE REGISTER REQUEST. TS 25.469 reserves
    /// them for CSG UEs on closed or hybrid HNBs, but a gateway may accept them generally.
    pub fn allow_tmsi_registration(&mut self, allow: bool) {
        self.tmsi_registration = allow;
    }
    pub fn state(&self) -> HnbState {
        self.state
    }
    pub fn association(&self) -> AssociationId {
        self.association
    }
    pub fn context(&self, key: UeKey) -> Option<ContextId> {
        match self.ues.get(&key)?.registration {
            Registration::Active {
                context,
                cancelled: false,
            } => Some(context),
            _ => None,
        }
    }
    pub fn registered_ues(&self) -> usize {
        self.contexts.len()
    }
    /// A retained context can be closing and must not authorize new traffic.
    pub fn retained_context(&self, key: UeKey) -> Option<ContextId> {
        match self.ues.get(&key)?.registration {
            Registration::Active { context, .. }
            | Registration::Closing { context }
            | Registration::Retiring { context } => Some(context),
            Registration::Pending { .. } => None,
        }
    }
    pub fn context_is_closing(&self, context: ContextId) -> bool {
        self.contexts
            .get(&context)
            .and_then(|key| self.ues.get(key))
            .is_some_and(|ue| matches!(ue.registration, Registration::Closing { .. }))
    }
    pub fn maximum_ues(&self) -> NonZeroUsize {
        self.limits.maximum_ues
    }
    pub fn contains_ue(&self, key: UeKey) -> bool {
        self.ues.contains_key(&key)
    }
    pub fn codec_limits(&self) -> Limits {
        self.limits.codec
    }
    pub fn control_time(&self) -> ControlTime {
        self.changed_at
    }
    fn time(&self, now: ControlTime) -> Result<(), Error> {
        if now < self.changed_at {
            Err(Error::Time)
        } else {
            Ok(())
        }
    }
    fn ready(&self, now: ControlTime) -> Result<u16, Error> {
        self.time(now)?;
        match self.state {
            HnbState::Registered { rnc } => Ok(rnc),
            _ => Err(Error::State),
        }
    }
    pub fn register_hnb_with<T, E>(
        &mut self,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.time(now).map_err(PublishError::Session)?;
        if self.state != HnbState::Idle
            && !matches!(self.state, HnbState::Backoff { until } if now >= until)
        {
            return Err(PublishError::Session(Error::State));
        }
        let deadline = ControlTime(
            now.0
                .checked_add(self.limits.request_timeout_ms.get())
                .ok_or(PublishError::Session(Error::Time))?,
        );
        let frame = Frame {
            association: self.association,
            scope: FrameScope::Hnb,
            protocol: Protocol::Hnbap,
            data: parameters_message(&self.parameters)
                .encode(self.limits.codec)
                .map_err(PublishError::Session)?,
        };
        let result = publish(frame).map_err(PublishError::Downstream)?;
        self.state = HnbState::Awaiting { deadline };
        self.changed_at = now;
        Ok(result)
    }
    pub fn register_ue_with<T, E>(
        &mut self,
        key: UeKey,
        identity: UeIdentity,
        capabilities: Capabilities,
        cause: RegistrationCause,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        if let Err(error) = self.ready(now) {
            tracing::debug!(
                "UE registration refused, session not ready: error={error:?} state={:?}",
                self.state
            );
            return Err(PublishError::Session(error));
        }
        if key.epoch != self.epoch {
            return Err(PublishError::Session(Error::Origin));
        }
        if self.ues.contains_key(&key) || self.identities.contains_key(&identity) {
            tracing::debug!(
                "UE registration conflicts with an existing one: key_known={} identity_owner={:?} existing={:?}",
                self.ues.contains_key(&key),
                self.identities.get(&identity),
                self.ues
                    .get(&key)
                    .map(|ue| format!("{:?}", ue.registration)),
            );
            return Err(PublishError::Session(Error::State));
        }
        if self.ues.len() == self.limits.maximum_ues.get() {
            return Err(PublishError::Session(Error::Capacity));
        }
        // TS 25.469 8.4.2 requires IMSI outside CSG operation, except emergency registration.
        if cause != RegistrationCause::Emergency
            && !self.tmsi_registration
            && (self.parameters.access != hnbap::AccessMode::Closed
                || capabilities.csg == hnbap::CsgCapability::NotCapable)
            && !matches!(identity, UeIdentity::Imsi(_))
        {
            return Err(PublishError::Session(Error::Identity));
        }
        let deadline = ControlTime(
            now.0
                .checked_add(self.limits.request_timeout_ms.get())
                .ok_or(PublishError::Session(Error::Time))?,
        );
        let frame = Frame {
            association: self.association,
            scope: FrameScope::Ue { key, domain: None },
            protocol: Protocol::Hnbap,
            data: hnbap::Message::UeRegister {
                identity: identity.clone(),
                cause,
                capabilities,
            }
            .encode(self.limits.codec)
            .map_err(PublishError::Session)?,
        };
        let stored_identity = identity.clone();
        let result = publish(frame).map_err(PublishError::Downstream)?;
        self.identities.insert(identity, key);
        self.ues.insert(
            key,
            Ue {
                key,
                identity: stored_identity,
                registration: Registration::Pending {
                    deadline,
                    cancelled: false,
                },
                domains: [LinkState::Closed; 2],
                connections: [0; 2],
                signalling_release_requested: [false; 2],
                inactivity_release_requested: [false; 2],
                membership: None,
                security_requests: [None; 2],
                release_requests: [None; 2],
                peer_disconnected: [false; 2],
                rab_requests: std::array::from_fn(|_| Vec::new()),
                security_algorithms: [None; 2],
            },
        );
        self.changed_at = now;
        Ok(result)
    }
    pub fn publish_nas_with<T, E>(
        &mut self,
        request: UplinkNas<'_>,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let UplinkNas {
            key,
            domain,
            nas,
            selector,
            cause,
        } = request;
        let rnc = self.ready(now).map_err(PublishError::Session)?;
        let ue = self
            .ues
            .get_mut(&key)
            .ok_or(PublishError::Session(Error::Origin))?;
        let Registration::Active {
            context,
            cancelled: false,
        } = ue.registration
        else {
            return Err(PublishError::Session(Error::State));
        };
        let slot = domain_index(domain);
        if ue.domains[slot] == LinkState::Releasing {
            return Err(PublishError::Session(Error::State));
        }
        let opening = ue.domains[slot] == LinkState::Closed;
        let connection = if opening {
            self.next_connection
                .ok_or(PublishError::Session(Error::Capacity))?
                .get()
        } else {
            ue.connections[slot]
        };
        let location = Location {
            plmn: self.parameters.plmn,
            lac: self.parameters.lac,
            rac: self.parameters.rac,
            sac: self.parameters.sac,
        };
        let ranap = if ue.domains[slot] == LinkState::Open {
            ranap::uplink_direct(domain, location, nas, self.limits.codec)
        } else {
            ranap::initial_ue(context, domain, rnc, location, nas, self.limits.codec)
        }
        .map_err(PublishError::Session)?;
        let message = if ue.domains[slot] == LinkState::Open {
            rua::Message::Direct {
                domain,
                context,
                ranap,
            }
        } else {
            rua::Message::Connect {
                domain,
                context,
                ranap,
                cause,
                selector,
                membership: ue.membership,
            }
        };
        let frame = Frame {
            association: self.association,
            scope: FrameScope::Ue {
                key,
                domain: Some(domain),
            },
            protocol: Protocol::Rua,
            data: message
                .encode(self.limits.codec)
                .map_err(PublishError::Session)?,
        };
        let result = publish(frame).map_err(PublishError::Downstream)?;
        if opening {
            ue.peer_disconnected[slot] = false;
            ue.signalling_release_requested[slot] = false;
            ue.inactivity_release_requested[slot] = false;
            self.next_connection = NonZeroU64::new(connection)
                .expect("nonzero new connection")
                .checked_add(1);
        }
        ue.connections[slot] = connection;
        ue.domains[slot] = LinkState::Open;
        self.changed_at = now;
        Ok(result)
    }
    pub fn receive_hnbap_with<T, E>(
        &mut self,
        association: AssociationId,
        now: ControlTime,
        decoded: &hnbap::Decoded,
        publish: impl FnOnce(&Event) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        if association != self.association {
            return Err(PublishError::Session(Error::Origin));
        }
        self.time(now).map_err(PublishError::Session)?;
        let fail = PublishError::Session;
        self.changed_at = now;
        let publish = |event: &Event| {
            if decoded.notify_ies.is_empty() {
                publish(event)
            } else {
                publish(&Event::Notified {
                    protocol: Protocol::Hnbap,
                    notify_ies: decoded.notify_ies.clone(),
                    event: Box::new(event.clone()),
                })
            }
        };
        match &decoded.message {
            hnbap::Message::HnbAccepted { rnc, .. } => {
                let HnbState::Awaiting { deadline } = self.state else {
                    return Err(fail(Error::State));
                };
                if now >= deadline {
                    return Err(fail(Error::Time));
                }
                if *rnc >= 1 << 12 {
                    return Err(fail(Error::Unsupported));
                }
                let result =
                    publish(&Event::HnbAccepted { rnc: *rnc }).map_err(PublishError::Downstream)?;
                self.state = HnbState::Registered { rnc: *rnc };
                self.changed_at = now;
                Ok(result)
            }
            hnbap::Message::HnbRejected {
                cause,
                backoff_seconds,
            } => {
                if !matches!(self.state, HnbState::Awaiting { .. }) {
                    return Err(fail(Error::State));
                }
                let until = backoff_seconds
                    .map(|seconds| {
                        now.0
                            .checked_add(u64::from(seconds) * 1000)
                            .map(ControlTime)
                            .ok_or(fail(Error::Time))
                    })
                    .transpose()?;
                let result = publish(&Event::HnbRejected {
                    cause: cause.clone(),
                    backoff_until: until,
                })
                .map_err(PublishError::Downstream)?;
                self.state = until.map_or(HnbState::Failed, |until| HnbState::Backoff { until });
                self.changed_at = now;
                Ok(result)
            }
            hnbap::Message::HnbDeregistered {
                backoff_seconds, ..
            } => {
                let until = backoff_seconds
                    .map(|seconds| {
                        now.0
                            .checked_add(u64::from(seconds) * 1000)
                            .map(ControlTime)
                            .ok_or(fail(Error::Time))
                    })
                    .transpose()?;
                self.state = HnbState::Draining {
                    backoff_until: until,
                };
                self.association_lost_with(now, publish)
            }
            hnbap::Message::UeAccepted {
                identity,
                context,
                membership,
            } => {
                self.ready(now).map_err(fail)?;
                let key = *self.identities.get(identity).ok_or(fail(Error::Origin))?;
                if self
                    .contexts
                    .get(context)
                    .is_some_and(|owner| *owner != key)
                {
                    return Err(fail(Error::Identity));
                }
                let ue = self.ues.get_mut(&key).ok_or(fail(Error::Origin))?;
                let Registration::Pending {
                    cancelled,
                    deadline,
                } = ue.registration
                else {
                    return Err(fail(Error::State));
                };
                if now >= deadline {
                    return Err(fail(Error::Time));
                }
                let event = if cancelled {
                    Event::CancelledRegistrationAccepted {
                        key,
                        context: *context,
                    }
                } else {
                    Event::UeAccepted {
                        key,
                        context: *context,
                        membership: *membership,
                    }
                };
                let result = publish(&event).map_err(PublishError::Downstream)?;
                ue.registration = Registration::Active {
                    context: *context,
                    cancelled,
                };
                ue.membership = *membership;
                self.contexts.insert(*context, key);
                self.changed_at = now;
                Ok(result)
            }
            hnbap::Message::UeRejected { identity, cause } => {
                self.ready(now).map_err(fail)?;
                let key = *self.identities.get(identity).ok_or(fail(Error::Origin))?;
                if !matches!(
                    self.ues.get(&key).ok_or(fail(Error::Origin))?.registration,
                    Registration::Pending { .. }
                ) {
                    return Err(fail(Error::State));
                }
                let result = publish(&Event::UeRejected {
                    key,
                    cause: cause.clone(),
                })
                .map_err(PublishError::Downstream)?;
                self.remove(key);
                self.changed_at = now;
                Ok(result)
            }
            hnbap::Message::UeDeregister { context, cause } => {
                self.ready(now).map_err(fail)?;
                let key = *self.contexts.get(context).ok_or(fail(Error::Origin))?;
                self.ues
                    .get_mut(&key)
                    .ok_or(fail(Error::Origin))?
                    .registration = Registration::Retiring { context: *context };
                let result = publish(&Event::UeDeregistered {
                    key,
                    context: *context,
                    cause: cause.clone(),
                })
                .map_err(PublishError::Downstream)?;
                self.remove(key);
                self.changed_at = now;
                Ok(result)
            }
            hnbap::Message::ErrorIndication { .. } => {
                publish(&Event::HnbapError(decoded.message.clone()))
                    .map_err(PublishError::Downstream)
            }
            _ => Err(fail(Error::State)),
        }
    }
    pub fn receive_rua_with<T, E>(
        &mut self,
        association: AssociationId,
        now: ControlTime,
        decoded: &rua::Decoded,
        publish: impl FnOnce(&Event) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.receive_rua_received_with(association, now, now, decoded, publish)
    }
    pub fn receive_rua_received_with<T, E>(
        &mut self,
        association: AssociationId,
        now: ControlTime,
        received_at: ControlTime,
        decoded: &rua::Decoded,
        publish: impl FnOnce(&Event) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        if received_at > now {
            return Err(PublishError::Session(Error::Time));
        }
        if association != self.association {
            return Err(PublishError::Session(Error::Origin));
        }
        self.ready(now).map_err(PublishError::Session)?;
        let fail = PublishError::Session;
        self.changed_at = now;
        let publish = |event: &Event| {
            if decoded.notify_ies.is_empty() {
                publish(event)
            } else {
                publish(&Event::Notified {
                    protocol: Protocol::Rua,
                    notify_ies: decoded.notify_ies.clone(),
                    event: Box::new(event.clone()),
                })
            }
        };
        match &decoded.message {
            rua::Message::Connect {
                context,
                domain,
                ranap,
                ..
            }
            | rua::Message::Direct {
                context,
                domain,
                ranap,
            } => {
                let key = *self.contexts.get(context).ok_or(fail(Error::Origin))?;
                let ue = self.ues.get_mut(&key).ok_or(fail(Error::Origin))?;
                if !matches!(
                    ue.registration,
                    Registration::Active {
                        cancelled: false,
                        ..
                    }
                ) {
                    return Err(fail(Error::State));
                }
                let slot = domain_index(*domain);
                if matches!(decoded.message, rua::Message::Direct { .. })
                    && ue.domains[slot] != LinkState::Open
                {
                    return Err(fail(Error::State));
                }
                if matches!(decoded.message, rua::Message::Connect { .. })
                    && ue.domains[slot] != LinkState::Closed
                {
                    return Err(fail(Error::State));
                }
                let opening = ue.domains[slot] == LinkState::Closed;
                let connection = if opening {
                    self.next_connection.ok_or(fail(Error::Capacity))?
                } else {
                    NonZeroU64::new(ue.connections[slot]).ok_or(fail(Error::State))?
                };
                let message = match ranap::decode(ranap, self.limits.codec) {
                    Ok(message) => message,
                    Err(error) => {
                        let cause = ranap::protocol_cause(&error);
                        let ie = match error {
                            Error::UnknownRejectIe(id)
                            | Error::MissingIe(id)
                            | Error::DuplicateIe(id)
                            | Error::IeOrder(id)
                            | Error::Value(id) => Some(id),
                            _ => None,
                        };
                        let result = publish(&Event::ProtocolError {
                            key,
                            domain: *domain,
                            cause,
                        })
                        .map_err(PublishError::Downstream)?;
                        self.pending_error_indication = Some(PendingErrorIndication {
                            key,
                            context: *context,
                            domain: *domain,
                            cause,
                            ie,
                            procedure: None,
                        });
                        self.changed_at = now;
                        return Ok(result);
                    }
                };
                if matches!(message.message, ranap::Inbound::Paging(_)) {
                    return Err(fail(Error::State));
                }
                // TS 25.413 10.3.4.1: an initiating message whose procedure is not supported
                // here is rejected (Reject) or reported (Ignore and Notify) with ERROR
                // INDICATION specifying the procedure code, triggering message and criticality.
                if let ranap::Inbound::Procedure(packet) = &message.message
                    && packet.outcome == crate::wire::Outcome::Initiating
                    && packet.criticality != crate::wire::Criticality::Ignore
                {
                    const ABSTRACT_SYNTAX_ERROR_REJECT: u16 = 100;
                    const ABSTRACT_SYNTAX_ERROR_IGNORE_AND_NOTIFY: u16 = 101;
                    let cause = if packet.criticality == crate::wire::Criticality::Reject {
                        ABSTRACT_SYNTAX_ERROR_REJECT
                    } else {
                        ABSTRACT_SYNTAX_ERROR_IGNORE_AND_NOTIFY
                    };
                    let result = publish(&Event::ProtocolError {
                        key,
                        domain: *domain,
                        cause,
                    })
                    .map_err(PublishError::Downstream)?;
                    self.pending_error_indication = Some(PendingErrorIndication {
                        key,
                        context: *context,
                        domain: *domain,
                        cause,
                        ie: None,
                        procedure: Some((packet.procedure, packet.criticality)),
                    });
                    self.changed_at = now;
                    return Ok(result);
                }
                let result = if let ranap::Inbound::Security(command) = message.message {
                    if ue.security_requests[slot].is_some() {
                        return Err(fail(Error::State));
                    }
                    let serial = self.next_security_request.ok_or(fail(Error::Capacity))?;
                    let request =
                        SecurityRequest::new(self.association, key, *domain, *context, serial);
                    let pending = PendingSecurity::new(request, &command).map_err(fail)?;
                    let result = publish(&Event::SecurityCommand {
                        request,
                        received_at,
                        command,
                        notify_ies: message.notify_ies,
                    })
                    .map_err(PublishError::Downstream)?;
                    ue.security_requests[slot] = Some(pending);
                    self.next_security_request =
                        serial.get().checked_add(1).and_then(NonZeroU64::new);
                    result
                } else if let ranap::Inbound::IuRelease(command) = message.message {
                    if opening || ue.release_requests[slot].is_some() {
                        return Err(fail(Error::State));
                    }
                    let request = IuReleaseRequest::new(
                        self.owner,
                        self.association,
                        key,
                        *domain,
                        *context,
                        connection,
                    );
                    let result = publish(&Event::IuRelease {
                        request,
                        received_at,
                        command,
                        notify_ies: message.notify_ies,
                    })
                    .map_err(PublishError::Downstream)?;
                    ue.release_requests[slot] = Some(request);
                    // TS 25.413 8.1: Iu Release takes precedence over RAB assignment.
                    ue.rab_requests[slot].retain(|pending| pending.in_flight);
                    result
                } else if let ranap::Inbound::RabAssignment(assignment) = message.message {
                    let serial = self.next_rab_request.ok_or(fail(Error::Capacity))?;
                    let request = RabRequest::new(
                        self.owner,
                        self.association,
                        key,
                        *domain,
                        *context,
                        serial,
                        connection,
                    );
                    let pending = PendingAssignment::new(request, &assignment).map_err(fail)?;
                    let requests = &mut ue.rab_requests[slot];
                    pending
                        .admit(
                            requests,
                            self.limits.maximum_rab_requests_per_domain.get(),
                            self.limits.maximum_rab_release_requests_per_domain.get(),
                        )
                        .map_err(fail)?;
                    requests.try_reserve(1).map_err(|_| fail(Error::Capacity))?;
                    let result = publish(&Event::RabAssignment {
                        request,
                        received_at,
                        assignment,
                        notify_ies: message.notify_ies,
                    })
                    .map_err(PublishError::Downstream)?;
                    pending.supersede(requests);
                    requests.push(pending);
                    self.next_rab_request = serial.get().checked_add(1).and_then(NonZeroU64::new);
                    result
                } else {
                    publish(&Event::Ranap {
                        key: Some(key),
                        domain: Some(*domain),
                        message,
                    })
                    .map_err(PublishError::Downstream)?
                };
                if opening {
                    ue.signalling_release_requested[slot] = false;
                    ue.inactivity_release_requested[slot] = false;
                    self.next_connection = connection.checked_add(1);
                }
                ue.connections[slot] = connection.get();
                ue.domains[slot] = if ue.release_requests[slot].is_some() {
                    LinkState::Releasing
                } else {
                    LinkState::Open
                };
                self.changed_at = now;
                Ok(result)
            }
            rua::Message::Connectionless { ranap } => {
                let message = ranap::decode(ranap, self.limits.codec).map_err(fail)?;
                if matches!(
                    message.message,
                    ranap::Inbound::RabAssignment(_)
                        | ranap::Inbound::CommonIdentity(_)
                        | ranap::Inbound::IuRelease(_)
                ) {
                    return Err(fail(Error::State));
                }
                if let ranap::Inbound::Procedure(packet) = &message.message
                    && let Ok(domain) = ranap::reset_domain(packet)
                {
                    let releases = self.reset_domain(domain);
                    let result = publish(&Event::Reset { domain, releases })
                        .map_err(PublishError::Downstream)?;
                    self.changed_at = now;
                    return Ok(result);
                }
                publish(&Event::Ranap {
                    key: None,
                    domain: None,
                    message,
                })
                .map_err(PublishError::Downstream)
            }
            rua::Message::Disconnect {
                context,
                domain,
                cause,
                ranap,
            } => {
                let key = *self.contexts.get(context).ok_or(fail(Error::Origin))?;
                let ue = self.ues.get_mut(&key).ok_or(fail(Error::Origin))?;
                let slot = domain_index(*domain);
                if ue.domains[slot] == LinkState::Closed {
                    return Err(fail(Error::State));
                }
                let final_ranap = ranap
                    .as_ref()
                    .and_then(|bytes| ranap::decode(bytes, self.limits.codec).ok());
                ue.domains[slot] = LinkState::Releasing;
                ue.peer_disconnected[slot] = true;
                ue.rab_requests[slot].retain(|pending| pending.in_flight);
                let request = IuReleaseRequest::new(
                    self.owner,
                    self.association,
                    key,
                    *domain,
                    *context,
                    NonZeroU64::new(ue.connections[slot]).ok_or(fail(Error::State))?,
                );
                let result = publish(&Event::Disconnected {
                    request,
                    key,
                    domain: *domain,
                    cause: cause.clone(),
                    final_ranap,
                })
                .map_err(PublishError::Downstream)?;
                ue.release_requests[slot] = Some(request);
                self.changed_at = now;
                Ok(result)
            }
            rua::Message::ErrorIndication { .. } => {
                publish(&Event::RuaError(decoded.message.clone())).map_err(PublishError::Downstream)
            }
        }
    }
    // TS 25.413 8.26.2.1: every Iu connection of the domain is treated as disconnected by the
    // peer, so later completion sends no Iu Release Complete into the reset CN.
    fn reset_domain(&mut self, domain: Domain) -> Vec<IuReleaseRequest> {
        let slot = domain_index(domain);
        let mut releases = Vec::new();
        for ue in self.ues.values_mut() {
            if ue.domains[slot] != LinkState::Open {
                continue;
            }
            let Registration::Active { context, .. } = ue.registration else {
                continue;
            };
            let Some(connection) = NonZeroU64::new(ue.connections[slot]) else {
                continue;
            };
            let request = IuReleaseRequest::new(
                self.owner,
                self.association,
                ue.key,
                domain,
                context,
                connection,
            );
            ue.domains[slot] = LinkState::Releasing;
            ue.peer_disconnected[slot] = true;
            ue.rab_requests[slot].clear();
            ue.release_requests[slot] = Some(request);
            releases.push(request);
        }
        releases
    }

    /// The ERROR INDICATION owed for the last undecodable connection-oriented RANAP message
    /// (TS 25.413 8.27.2). Sent on the same connection as a RUA Direct Transfer.
    pub fn take_error_indication_frame(
        &mut self,
        now: ControlTime,
    ) -> Result<Option<Frame>, Error> {
        let Some(pending) = self.pending_error_indication.take() else {
            return Ok(None);
        };
        if !matches!(self.state, HnbState::Registered { .. })
            || self
                .ues
                .get(&pending.key)
                .is_none_or(|ue| ue.domains[domain_index(pending.domain)] == LinkState::Closed)
        {
            return Ok(None);
        }
        let ranap = ranap::error_indication(
            pending.cause,
            pending.procedure,
            pending.ie,
            self.limits.codec,
        )?;
        let data = rua::Message::Direct {
            context: pending.context,
            domain: pending.domain,
            ranap,
        }
        .encode(self.limits.codec)?;
        self.changed_at = now;
        Ok(Some(Frame {
            association: self.association,
            scope: FrameScope::Ue {
                key: pending.key,
                domain: Some(pending.domain),
            },
            protocol: Protocol::Rua,
            data,
        }))
    }

    /// TS 25.469 8.3.1: HNB DE-REGISTER sent by the HNB when it stops serving.
    pub fn hnb_deregister_frame(&self) -> Result<Frame, Error> {
        if !matches!(self.state, HnbState::Registered { .. }) {
            return Err(Error::State);
        }
        Ok(Frame {
            association: self.association,
            scope: FrameScope::Hnb,
            protocol: Protocol::Hnbap,
            data: hnbap::Message::HnbDeregistered {
                cause: hnbap::Cause::Radio(hnbap::RadioCause::Normal),
                backoff_seconds: None,
            }
            .encode(self.limits.codec)?,
        })
    }

    pub fn publish_reset_acknowledge_with<T, E>(
        &mut self,
        domain: Domain,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let rnc = self.ready(now).map_err(PublishError::Session)?;
        let ranap = ranap::reset_acknowledge(domain, rnc, self.parameters.plmn, self.limits.codec)
            .map_err(PublishError::Session)?;
        let frame = Frame {
            association: self.association,
            scope: FrameScope::Hnb,
            protocol: Protocol::Rua,
            data: rua::Message::Connectionless { ranap }
                .encode(self.limits.codec)
                .map_err(PublishError::Session)?,
        };
        let result = publish(frame).map_err(PublishError::Downstream)?;
        self.changed_at = now;
        Ok(result)
    }

    // Admission covers both domain disconnects and UE deregistration before any local context is removed.
    pub fn retire_with<T, E>(
        &mut self,
        key: UeKey,
        now: ControlTime,
        publish: impl FnOnce(&[Frame]) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        self.retire_cause_with(key, now, hnbap::RadioCause::ConnectionLost, publish)
    }
    pub fn retire_rrc_release_with<T, E>(
        &mut self,
        key: UeKey,
        now: ControlTime,
        publish: impl FnOnce(&[Frame]) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        if self
            .ues
            .get(&key)
            .is_some_and(|ue| ue.domains != [LinkState::Closed; 2])
        {
            return Err(PublishError::Session(Error::State));
        }
        self.retire_cause_with(key, now, hnbap::RadioCause::RrcRelease, publish)
    }
    fn retire_cause_with<T, E>(
        &mut self,
        key: UeKey,
        now: ControlTime,
        cause: hnbap::RadioCause,
        publish: impl FnOnce(&[Frame]) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        self.ready(now).map_err(PublishError::Session)?;
        let Some(ue) = self.ues.get_mut(&key) else {
            return if key.epoch == self.epoch {
                Ok(None)
            } else {
                Err(PublishError::Session(Error::Origin))
            };
        };
        let context = match &mut ue.registration {
            Registration::Pending { cancelled, .. } => {
                *cancelled = true;
                self.changed_at = now;
                return Ok(None);
            }
            Registration::Active { context, .. } => *context,
            Registration::Closing { context } => *context,
            Registration::Retiring { .. } => return Err(PublishError::Session(Error::State)),
        };
        ue.registration = Registration::Closing { context };
        self.changed_at = now;
        let mut frames = Vec::with_capacity(3);
        for domain in [Domain::Cs, Domain::Ps] {
            if matches!(
                ue.domains[domain_index(domain)],
                LinkState::Open | LinkState::Releasing
            ) && !ue.peer_disconnected[domain_index(domain)]
            {
                let message = rua::Message::Disconnect {
                    domain,
                    context,
                    cause: rua::Cause::Radio(rua::RadioCause::ConnectFailed),
                    ranap: None,
                };
                frames.push(Frame {
                    association: self.association,
                    scope: FrameScope::Ue {
                        key,
                        domain: Some(domain),
                    },
                    protocol: Protocol::Rua,
                    data: message
                        .encode(self.limits.codec)
                        .map_err(PublishError::Session)?,
                });
            }
        }
        frames.push(Frame {
            association: self.association,
            scope: FrameScope::Ue { key, domain: None },
            protocol: Protocol::Hnbap,
            data: hnbap::Message::UeDeregister {
                context,
                cause: hnbap::Cause::Radio(cause),
            }
            .encode(self.limits.codec)
            .map_err(PublishError::Session)?,
        });
        let result = publish(&frames).map_err(PublishError::Downstream)?;
        self.remove(key);
        self.changed_at = now;
        Ok(Some(result))
    }
    fn remove(&mut self, key: UeKey) {
        if let Some(ue) = self.ues.remove(&key) {
            self.identities.remove(&ue.identity);
            if let Registration::Active { context, .. }
            | Registration::Retiring { context }
            | Registration::Closing { context } = ue.registration
            {
                self.contexts.remove(&context);
            }
        }
    }
    pub fn expire_with<T, E>(
        &mut self,
        now: ControlTime,
        publish: impl FnOnce(&Event) -> Result<T, E>,
    ) -> Result<Option<T>, PublishError<E>> {
        self.time(now).map_err(PublishError::Session)?;
        if matches!(self.state, HnbState::Awaiting { deadline } if now >= deadline)
            || matches!(self.state, HnbState::Draining { .. })
        {
            return self.association_lost_with(now, publish).map(Some);
        }
        // TS 25.469 8.4: an unanswered UE REGISTER REQUEST fails that UE's registration only.
        // The HNB registration and every other UE stay as they are.
        let expired = self.ues.values().find_map(|ue| {
            matches!(ue.registration, Registration::Pending { deadline, .. } if now >= deadline)
                .then_some(ue.key)
        });
        if let Some(key) = expired {
            let result = publish(&Event::UeRejected {
                key,
                cause: hnbap::Cause::Radio(hnbap::RadioCause::Unspecified),
            })
            .map_err(PublishError::Downstream)?;
            self.remove(key);
            self.changed_at = now;
            return Ok(Some(result));
        }
        Ok(None)
    }
    pub fn association_lost_with<T, E>(
        &mut self,
        now: ControlTime,
        publish: impl FnOnce(&Event) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.time(now).map_err(PublishError::Session)?;
        let backoff_until = match self.state {
            HnbState::Draining { backoff_until } => backoff_until,
            _ => None,
        };
        self.state = HnbState::Draining { backoff_until };
        self.changed_at = now;
        let owners = self.ues.values().map(|ue| ue.key).collect();
        let result = publish(&Event::AssociationLost {
            owners,
            backoff_until,
        })
        .map_err(PublishError::Downstream)?;
        self.ues.clear();
        self.identities.clear();
        self.contexts.clear();
        self.state = backoff_until.map_or(HnbState::Failed, |until| HnbState::Backoff { until });
        self.changed_at = now;
        Ok(result)
    }
}
fn domain_index(domain: Domain) -> usize {
    match domain {
        Domain::Cs => 0,
        Domain::Ps => 1,
    }
}
fn parameters_message(parameters: &HnbParameters) -> hnbap::Message {
    hnbap::Message::HnbRegister(parameters.clone())
}

#[cfg(test)]
mod tests;
