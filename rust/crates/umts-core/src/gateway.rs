// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{DriveError, Error, Limits, Transport, queue::Outbox};
use std::collections::VecDeque;
use umts_iuh::{
    ContextId, hnbap, rua,
    session::{AssociationId, ControlTime, Event, HnbState, PublishError, Session, UplinkNas},
    wire::Protocol,
};
use umts_sctp::{Notification, Receive, SendResult};
use umts_types::UeKey;
use zeroize::Zeroize;

#[derive(Debug)]
pub enum RabCommitError<E> {
    Core(Error),
    Commit(E),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    AwaitingAssociation,
    Registering,
    Serving,
    Draining,
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LossReason {
    LocalShutdown,
    TransportFailure,
    ConnectTimeout,
    RequestTimeout,
    PeerShutdown,
    HnbDeregistered,
    RegistrationRejected,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub transmitted: bool,
    pub received: bool,
    pub delivered: bool,
    pub backpressure: bool,
    pub closed: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub transmitted: u64,
    pub error_indications: u64,
    pub received: u64,
    pub rejected: u64,
    pub delivered: u64,
    pub backpressure: u64,
    pub losses: u64,
    pub decoded: u64,
    pub cleaned_cancelled: u64,
    pub cancelled_inbound: u64,
}
enum Cached {
    Hnbap(hnbap::Decoded),
    Rua {
        received_at: ControlTime,
        decoded: rua::Decoded,
    },
}
impl Cached {
    fn rua_context(&self) -> Option<ContextId> {
        match self {
            Self::Rua { decoded, .. } => match &decoded.message {
                rua::Message::Connect { context, .. }
                | rua::Message::Direct { context, .. }
                | rua::Message::Disconnect { context, .. } => Some(*context),
                _ => None,
            },
            Self::Hnbap(_) => None,
        }
    }
}
impl Drop for Cached {
    fn drop(&mut self) {
        if let Self::Rua { decoded, .. } = self {
            match &mut decoded.message {
                rua::Message::Connect { ranap, .. }
                | rua::Message::Direct { ranap, .. }
                | rua::Message::Connectionless { ranap } => ranap.zeroize(),
                rua::Message::Disconnect {
                    ranap: Some(ranap), ..
                } => ranap.zeroize(),
                _ => (),
            }
        }
    }
}
enum ReadOutcome {
    Idle,
    Fragment,
    Notification(Notification),
    Closed,
    Packet(Cached),
    Rejected(Error),
}

pub struct Gateway<T: Transport> {
    session: Session,
    transport: Option<T>,
    outbox: Outbox,
    limits: Limits,
    phase: Phase,
    connect_deadline: ControlTime,
    last_time: ControlTime,
    established: bool,
    cached: Option<Cached>,
    loss_event: Option<Event>,
    cancelled: VecDeque<UeKey>,
    loss: Option<LossReason>,
    counters: Counters,
}
impl<T: Transport> Gateway<T> {
    pub fn epoch(&self) -> umts_types::RadioEpoch {
        self.session.epoch()
    }
    pub fn new(
        session: Session,
        transport: T,
        limits: Limits,
        now: ControlTime,
    ) -> Result<Self, Error> {
        if session.state() != HnbState::Idle {
            return Err(Error::State);
        }
        if now < session.control_time() {
            return Err(Error::Time);
        }
        let deadline = now
            .0
            .checked_add(limits.connect_timeout_ms.get())
            .ok_or(Error::Time)?;
        let outbox = Outbox::new(session.association(), limits)?;
        let mut cancelled = VecDeque::new();
        cancelled
            .try_reserve_exact(session.maximum_ues().get())
            .map_err(|_| Error::Capacity)?;
        Ok(Self {
            session,
            transport: Some(transport),
            outbox,
            limits,
            phase: Phase::Connecting,
            connect_deadline: ControlTime(deadline),
            last_time: now,
            established: false,
            cached: None,
            loss_event: None,
            cancelled,
            loss: None,
            counters: Counters::default(),
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn hnb_state(&self) -> HnbState {
        self.session.state()
    }
    pub fn association(&self) -> AssociationId {
        self.session.association()
    }
    pub fn context(&self, key: UeKey) -> Option<ContextId> {
        self.session.context(key)
    }
    pub fn authorize_rab(
        &self,
        request: umts_iuh::session::RabRequest,
        rab: u8,
        operation: umts_iuh::session::RabOperation,
    ) -> Result<umts_iuh::session::RabAuthorization<'_>, Error> {
        if self.phase != Phase::Serving {
            return Err(Error::State);
        }
        self.session
            .authorize_rab(request, rab, operation)
            .map_err(Error::Session)
    }
    pub fn registered_ues(&self) -> usize {
        self.session.registered_ues()
    }
    pub fn queued_frames(&self) -> usize {
        self.outbox.len()
    }
    pub fn counters(&self) -> Counters {
        self.counters
    }
    pub fn loss_reason(&self) -> Option<LossReason> {
        self.loss
    }
    pub fn shutdown(&mut self) {
        // TS 25.469 8.3.1: a stopping HNB tells the gateway with HNB DE-REGISTER. One direct
        // write is attempted because the outbox is discarded right after.
        if self.phase == Phase::Serving
            && let Some(transport) = self.transport.as_mut()
            && let Ok(frame) = self.session.hnb_deregister_frame()
            && let Some(ppid) = Protocol::Hnbap.ppid()
        {
            let _ = transport.send(crate::queue::CONTROL_STREAM, ppid, &frame.data);
        }
        if self.phase != Phase::Closed {
            self.invalidate(LossReason::LocalShutdown);
        }
    }
    pub(crate) fn transport(&self) -> Option<&T> {
        self.transport.as_ref()
    }
    fn time(&mut self, now: ControlTime) -> Result<(), Error> {
        if now < self.last_time {
            return Err(Error::Time);
        }
        self.last_time = now;
        Ok(())
    }
    fn command(&mut self, now: ControlTime) -> Result<(), Error> {
        self.time(now)?;
        if self.phase != Phase::Serving {
            return Err(Error::State);
        }
        Ok(())
    }
    pub fn register_ue(
        &mut self,
        key: UeKey,
        identity: hnbap::UeIdentity,
        capabilities: hnbap::Capabilities,
        cause: hnbap::RegistrationCause,
        now: ControlTime,
    ) -> Result<(), Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .register_ue_with(key, identity, capabilities, cause, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn publish_nas(&mut self, now: ControlTime, request: UplinkNas<'_>) -> Result<(), Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .publish_nas_with(request, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn acknowledge_reset(
        &mut self,
        now: ControlTime,
        domain: umts_iuh::Domain,
    ) -> Result<(), Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .publish_reset_acknowledge_with(domain, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn request_signalling_release(
        &mut self,
        now: ControlTime,
        key: UeKey,
        context: umts_iuh::ContextId,
        domain: umts_iuh::Domain,
    ) -> Result<bool, Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .request_signalling_release_with(key, context, domain, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn owns_iu_release(&self, request: umts_iuh::session::IuReleaseRequest) -> bool {
        self.phase == Phase::Serving && self.session.owns_iu_release(request)
    }
    pub fn peer_disconnected_release(&self, request: umts_iuh::session::IuReleaseRequest) -> bool {
        self.phase == Phase::Serving && self.session.peer_disconnected_release(request)
    }
    pub fn request_inactivity_release(
        &mut self,
        now: ControlTime,
        key: UeKey,
        context: umts_iuh::ContextId,
        domain: umts_iuh::Domain,
    ) -> Result<bool, Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .request_inactivity_release_with(key, context, domain, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn request_rab_release(
        &mut self,
        now: ControlTime,
        key: UeKey,
        domain: umts_iuh::Domain,
        rabs: &[(u8, u16)],
    ) -> Result<(), Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .request_rab_release_with(key, domain, rabs, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn request_ps_transport_release(
        &mut self,
        now: ControlTime,
        key: UeKey,
        context: umts_iuh::ContextId,
    ) -> Result<bool, Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .request_ps_transport_release_with(key, context, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn request_radio_lost_release(
        &mut self,
        now: ControlTime,
        key: UeKey,
        context: umts_iuh::ContextId,
        domain: umts_iuh::Domain,
    ) -> Result<bool, Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .request_radio_lost_release_with(key, context, domain, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn iu_release_for_rab(
        &self,
        request: umts_iuh::session::RabRequest,
    ) -> Option<umts_iuh::session::IuReleaseRequest> {
        if self.phase != Phase::Serving {
            return None;
        }
        self.session.iu_release_for_rab(request)
    }
    pub fn security_response_cancelled_by_iu_release(
        &self,
        request: umts_iuh::session::SecurityRequest,
    ) -> bool {
        self.phase == Phase::Serving
            && self
                .session
                .security_response_cancelled_by_iu_release(request)
    }
    pub fn complete_iu_release_without_reports(
        &mut self,
        now: ControlTime,
        request: umts_iuh::session::IuReleaseRequest,
    ) -> Result<(), Error> {
        self.complete_iu_release(now, request, &[])
    }
    pub fn complete_iu_release(
        &mut self,
        now: ControlTime,
        request: umts_iuh::session::IuReleaseRequest,
        reports: &[umts_iuh::ranap::release::VolumeReport],
    ) -> Result<(), Error> {
        self.command(now)?;
        if self.session.peer_disconnected_release(request) {
            return self
                .session
                .complete_peer_disconnect(request, now)
                .map_err(Error::Session);
        }
        let outbox = &mut self.outbox;
        self.session
            .complete_iu_release_with(request, reports, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn publish_security_response(
        &mut self,
        now: ControlTime,
        request: umts_iuh::session::SecurityRequest,
        response: umts_iuh::ranap::SecurityResponse,
    ) -> Result<(), Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .publish_security_with(request, response, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn retire_radio_failure(&mut self, key: UeKey, now: ControlTime) -> Result<(), Error> {
        self.retire_ue(key, now, false)
    }
    pub fn retire_rrc_release(&mut self, key: UeKey, now: ControlTime) -> Result<(), Error> {
        self.retire_ue(key, now, true)
    }
    fn retire_ue(&mut self, key: UeKey, now: ControlTime, normal: bool) -> Result<(), Error> {
        self.command(now)?;
        let context = self.session.retained_context(key);
        let outbox = &mut self.outbox;
        if normal {
            self.session
                .retire_rrc_release_with(key, now, |frames| outbox.admit(frames))
        } else {
            self.session
                .retire_with(key, now, |frames| outbox.admit(frames))
        }
        .map_err(command_error)?;
        if !self.session.contains_ue(key)
            && context.is_some()
            && self
                .cached
                .as_ref()
                .is_some_and(|cached| cached.rua_context() == context)
        {
            self.cached = None;
            self.counters.cancelled_inbound = self.counters.cancelled_inbound.saturating_add(1);
        }
        Ok(())
    }
    pub fn publish_rab_response(
        &mut self,
        now: ControlTime,
        request: umts_iuh::session::RabRequest,
        response: &umts_iuh::ranap::rab::AssignmentResponse,
    ) -> Result<(), Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .publish_rab_with(request, response, now, |frame| {
                outbox.admit(std::slice::from_ref(&frame))
            })
            .map_err(command_error)
    }
    pub fn record_rab_outcome(
        &mut self,
        now: ControlTime,
        request: umts_iuh::session::RabRequest,
        outcome: umts_iuh::session::RabOutcome,
    ) -> Result<(), Error> {
        self.command(now)?;
        self.session
            .record_rab_outcome(request, outcome)
            .map_err(Error::Session)
    }
    pub fn record_rab_outcome_with<R, E>(
        &mut self,
        now: ControlTime,
        request: umts_iuh::session::RabRequest,
        outcome: umts_iuh::session::RabOutcome,
        commit: impl FnOnce(umts_iuh::session::RabAuthorization<'_>) -> Result<R, E>,
    ) -> Result<R, RabCommitError<E>> {
        self.command(now).map_err(RabCommitError::Core)?;
        self.session
            .record_rab_outcome_with(request, outcome, commit)
            .map_err(|error| match error {
                PublishError::Session(error) => RabCommitError::Core(Error::Session(error)),
                PublishError::Downstream(error) => RabCommitError::Commit(error),
            })
    }
    pub fn publish_ready_rab(&mut self, now: ControlTime) -> Result<bool, Error> {
        self.command(now)?;
        let outbox = &mut self.outbox;
        self.session
            .publish_ready_rab_with(now, |frame| outbox.admit(std::slice::from_ref(&frame)))
            .map(|result| result.is_some())
            .map_err(command_error)
    }
    pub fn rab_connection_current(&self, request: umts_iuh::session::RabRequest) -> bool {
        self.check_rab_connection(request).is_ok()
    }
    pub fn check_rab_connection(
        &self,
        request: umts_iuh::session::RabRequest,
    ) -> Result<(), Error> {
        if self.phase != Phase::Serving {
            return Err(Error::State);
        }
        self.session
            .check_rab_connection(request)
            .map_err(Error::Session)
    }
    pub fn owns_rab_request(&self, request: umts_iuh::session::RabRequest) -> bool {
        self.session.owns_rab_request(request)
    }
    pub fn same_rab_owner(&self, request: umts_iuh::session::RabRequest) -> bool {
        self.session.same_rab_owner(request)
    }
    pub fn rab_superseded(&self, request: umts_iuh::session::RabRequest) -> bool {
        self.session.rab_superseded(request)
    }
    pub fn reinstate_rab(
        &mut self,
        request: umts_iuh::session::RabRequest,
        assignment: &umts_iuh::ranap::rab::Assignment,
    ) -> Result<(), Error> {
        if self.phase != Phase::Serving {
            return Err(Error::State);
        }
        self.session
            .reinstate_rab(request, assignment)
            .map_err(Error::Session)
    }
    pub fn mark_rab_in_flight(&mut self, request: umts_iuh::session::RabRequest) -> bool {
        self.session.mark_rab_in_flight(request)
    }
    pub fn has_pending_rab_request(&self, request: umts_iuh::session::RabRequest) -> bool {
        self.session.has_pending_rab_request(request)
    }
    fn invalidate(&mut self, reason: LossReason) {
        if self.loss.is_none() {
            self.counters.losses = self.counters.losses.saturating_add(1);
        }
        self.loss.get_or_insert(reason);
        self.phase = Phase::Draining;
        self.transport.take();
        self.outbox.discard();
        self.cached = None;
        self.cancelled.clear();
    }
    fn drain<U>(
        &mut self,
        now: ControlTime,
        publish: impl FnOnce(&Event) -> Result<(), U>,
    ) -> Result<Step, DriveError<T::Error, U>> {
        let loss_event = &mut self.loss_event;
        match self.session.association_lost_with(now, |event| {
            publish(loss_event.get_or_insert_with(|| event.clone()))
        }) {
            Ok(()) => {
                self.phase = Phase::Closed;
                self.loss_event = None;
                self.counters.delivered = self.counters.delivered.saturating_add(1);
                Ok(Step {
                    delivered: true,
                    closed: true,
                    ..Step::default()
                })
            }
            Err(PublishError::Downstream(error)) => {
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                Err(DriveError::Upper(error))
            }
            Err(PublishError::Session(error)) => Err(DriveError::Owner(Error::Session(error))),
        }
    }
    fn read_once(&mut self, now: ControlTime) -> Result<ReadOutcome, T::Error> {
        let transport = self.transport.as_mut().expect("open transport");
        let outcome = match transport.receive_once()? {
            Receive::Idle => return Ok(ReadOutcome::Idle),
            Receive::Fragment => return Ok(ReadOutcome::Fragment),
            Receive::Closed => return Ok(ReadOutcome::Closed),
            Receive::Notification(notification) => ReadOutcome::Notification(notification),
            Receive::Record { metadata, bytes } => {
                let decoded = if bytes.len() > self.limits.maximum_packet_bytes.get() {
                    Err(Error::Capacity)
                } else if Some(metadata.ppid) == Protocol::Hnbap.ppid() {
                    hnbap::Message::decode(bytes, self.session.codec_limits())
                        .map(Cached::Hnbap)
                        .map_err(Error::Session)
                } else if Some(metadata.ppid) == Protocol::Rua.ppid() {
                    rua::Message::decode(bytes, self.session.codec_limits())
                        .map(|decoded| Cached::Rua {
                            received_at: now,
                            decoded,
                        })
                        .map_err(Error::Session)
                } else {
                    Err(Error::Protocol)
                };
                // TS 25.469 10 and TS 25.468 10: a HNBAP or RUA message that cannot be decoded
                // is answered with ERROR INDICATION specifying the protocol cause.
                if let Err(Error::Session(error)) = &decoded
                    && self.phase == Phase::Serving
                {
                    let protocol = if Some(metadata.ppid) == Protocol::Hnbap.ppid() {
                        Protocol::Hnbap
                    } else {
                        Protocol::Rua
                    };
                    if let Ok(data) = error_indication(protocol, error, self.session.codec_limits())
                    {
                        let frame = umts_iuh::session::Frame {
                            association: self.session.association(),
                            scope: umts_iuh::session::FrameScope::Hnb,
                            protocol,
                            data,
                        };
                        if self.outbox.admit(std::slice::from_ref(&frame)).is_ok() {
                            self.counters.error_indications =
                                self.counters.error_indications.saturating_add(1);
                        }
                    }
                }
                match decoded {
                    Ok(packet) => ReadOutcome::Packet(packet),
                    Err(error) => ReadOutcome::Rejected(error),
                }
            }
        };
        transport.acknowledge_receive()?;
        Ok(outcome)
    }
    fn apply_cached<U>(
        &mut self,
        now: ControlTime,
        publish: &mut impl FnMut(&Event) -> Result<(), U>,
    ) -> Result<(), DriveError<T::Error, U>> {
        let cached = self.cached.take().expect("cached packet");
        if self.cancelled.len() == self.session.maximum_ues().get() {
            let session = &self.session;
            self.cancelled.retain(|key| session.contains_ue(*key));
        }
        let phase = &mut self.phase;
        let transport = &mut self.transport;
        let outbox = &mut self.outbox;
        let cancelled = &mut self.cancelled;
        let loss = &mut self.loss;
        let loss_event = &mut self.loss_event;
        let mut cancelled_accepted = None;
        let mut observed_loss = false;
        let mut callback = |event: &Event| {
            let base = base_event(event);
            match base {
                Event::IuRelease { request, .. } => {
                    outbox.cancel(request.key(), Some(request.domain()))
                }
                Event::Disconnected { key, domain, .. } => outbox.cancel(*key, Some(*domain)),
                Event::UeDeregistered { key, .. } | Event::UeRejected { key, .. } => {
                    outbox.cancel(*key, None)
                }
                _ => {}
            }
            if matches!(base, Event::AssociationLost { .. }) {
                *phase = Phase::Draining;
                transport.take();
                outbox.discard();
                cancelled.clear();
                loss.get_or_insert(LossReason::HnbDeregistered);
                observed_loss = true;
                loss_event.get_or_insert_with(|| event.clone());
            }
            publish(event)?;
            if let Event::CancelledRegistrationAccepted { key, .. } = base {
                cancelled_accepted = Some(*key);
            }
            Ok(())
        };
        let result = match &cached {
            Cached::Hnbap(decoded) => self.session.receive_hnbap_with(
                self.session.association(),
                now,
                decoded,
                &mut callback,
            ),
            Cached::Rua {
                received_at,
                decoded,
            } => self.session.receive_rua_received_with(
                self.session.association(),
                now,
                *received_at,
                decoded,
                &mut callback,
            ),
        };
        if observed_loss {
            self.counters.losses = self.counters.losses.saturating_add(1);
        }
        match result {
            Ok(()) => {
                if let Some(key) = cancelled_accepted {
                    // Each canceled accept owns a distinct registered UE until its deregistration batch is admitted.
                    assert!(self.cancelled.len() < self.session.maximum_ues().get());
                    self.cancelled.push_back(key);
                }
                self.counters.delivered = self.counters.delivered.saturating_add(1);
                match self.session.state() {
                    HnbState::Registered { .. } => self.phase = Phase::Serving,
                    HnbState::Backoff { .. } | HnbState::Failed => {
                        if !observed_loss {
                            self.loss.get_or_insert(LossReason::RegistrationRejected);
                        }
                        self.transport.take();
                        self.outbox.discard();
                        self.phase = Phase::Closed;
                        self.loss_event = None;
                    }
                    _ => {}
                }
                Ok(())
            }
            Err(PublishError::Downstream(error)) => {
                if self.phase != Phase::Draining {
                    self.cached = Some(cached);
                }
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                Err(DriveError::Upper(error))
            }
            Err(PublishError::Session(error)) => {
                self.counters.rejected = self.counters.rejected.saturating_add(1);
                Err(DriveError::Owner(Error::Session(error)))
            }
        }
    }
    fn expire<U>(
        &mut self,
        now: ControlTime,
        publish: &mut impl FnMut(&Event) -> Result<(), U>,
    ) -> Result<bool, DriveError<T::Error, U>> {
        let transport = &mut self.transport;
        let phase = &mut self.phase;
        let outbox = &mut self.outbox;
        let cached = &mut self.cached;
        let cancelled = &mut self.cancelled;
        let loss = &mut self.loss;
        let loss_event = &mut self.loss_event;
        let mut lost = false;
        let result = self.session.expire_with(now, |event| {
            if matches!(event, Event::AssociationLost { .. }) {
                lost = true;
                *phase = Phase::Draining;
                transport.take();
                outbox.discard();
                *cached = None;
                cancelled.clear();
                loss.get_or_insert(LossReason::RequestTimeout);
                loss_event.get_or_insert_with(|| event.clone());
            }
            publish(event)
        });
        match result {
            Ok(Some(())) if lost => {
                self.phase = Phase::Closed;
                self.loss_event = None;
                self.counters.losses = self.counters.losses.saturating_add(1);
                self.counters.delivered = self.counters.delivered.saturating_add(1);
                Ok(true)
            }
            Ok(Some(())) => {
                self.counters.delivered = self.counters.delivered.saturating_add(1);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(PublishError::Downstream(error)) => {
                self.counters.losses = self.counters.losses.saturating_add(1);
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
                Err(DriveError::Upper(error))
            }
            Err(PublishError::Session(error)) => Err(DriveError::Owner(Error::Session(error))),
        }
    }
    // One drive performs at most one send, one read, one decoded publication and one cancellation cleanup.
    pub fn drive<U>(
        &mut self,
        now: ControlTime,
        mut publish: impl FnMut(&Event) -> Result<(), U>,
    ) -> Result<Step, DriveError<T::Error, U>> {
        self.time(now).map_err(DriveError::Owner)?;
        if self.phase == Phase::Closed {
            return Ok(Step {
                closed: true,
                ..Step::default()
            });
        }
        if self.phase == Phase::Draining {
            return self.drain(now, publish);
        }
        if matches!(self.phase, Phase::Connecting | Phase::AwaitingAssociation)
            && now >= self.connect_deadline
        {
            self.invalidate(LossReason::ConnectTimeout);
            return self.drain(now, publish);
        }
        if self.expire(now, &mut publish)? {
            return Ok(Step {
                closed: true,
                delivered: true,
                ..Step::default()
            });
        }
        let mut step = Step::default();
        if self.phase == Phase::Connecting {
            match self
                .transport
                .as_mut()
                .expect("connecting transport")
                .complete_connect()
            {
                Ok(false) => return Ok(step),
                Ok(true) => self.phase = Phase::AwaitingAssociation,
                Err(error) => {
                    self.invalidate(LossReason::TransportFailure);
                    return Err(DriveError::Transport(error));
                }
            }
        }
        if self.phase == Phase::AwaitingAssociation && self.established {
            let outbox = &mut self.outbox;
            self.session
                .register_hnb_with(now, |frame| outbox.admit(std::slice::from_ref(&frame)))
                .map_err(|error| DriveError::Owner(command_error(error)))?;
            self.phase = Phase::Registering;
        }
        if self.phase == Phase::Serving
            && let Ok(Some(frame)) = self.session.take_error_indication_frame(now)
        {
            let _ = self.outbox.admit(std::slice::from_ref(&frame));
        }
        match self
            .outbox
            .transmit_one(self.transport.as_mut().expect("open transport"))
        {
            Ok(Some(SendResult::Sent)) => {
                step.transmitted = true;
                self.counters.transmitted = self.counters.transmitted.saturating_add(1);
            }
            Ok(Some(SendResult::Retry)) => {
                step.backpressure = true;
                self.counters.backpressure = self.counters.backpressure.saturating_add(1);
            }
            Ok(None) => {}
            Err(error) => {
                self.invalidate(LossReason::TransportFailure);
                return Err(DriveError::Transport(error));
            }
        }
        if let Some(key) = self.cancelled.front().copied() {
            let outbox = &mut self.outbox;
            match self
                .session
                .retire_with(key, now, |frames| outbox.admit(frames))
            {
                Ok(_) | Err(PublishError::Session(umts_iuh::Error::Origin)) => {
                    self.cancelled.pop_front();
                    self.counters.cleaned_cancelled =
                        self.counters.cleaned_cancelled.saturating_add(1);
                }
                Err(PublishError::Downstream(Error::Capacity)) => step.backpressure = true,
                Err(PublishError::Session(umts_iuh::Error::State)) if self.cached.is_some() => {
                    step.backpressure = true
                }
                Err(error) => return Err(DriveError::Owner(command_error(error))),
            }
        }
        if self.cached.is_none() {
            match self.read_once(now) {
                Ok(ReadOutcome::Idle | ReadOutcome::Fragment) => return Ok(step),
                Ok(ReadOutcome::Notification(notification)) => {
                    step.received = true;
                    match notification {
                        Notification::Established { .. } => {
                            if self.established {
                                self.invalidate(LossReason::PeerShutdown);
                                return self.drain(now, publish);
                            }
                            self.established = true;
                        }
                        notification if notification.invalidates_association() => {
                            self.invalidate(LossReason::PeerShutdown);
                            return self.drain(now, publish);
                        }
                        _ => {}
                    }
                    return Ok(step);
                }
                Ok(ReadOutcome::Closed) => {
                    self.invalidate(LossReason::PeerShutdown);
                    return self.drain(now, publish);
                }
                Ok(ReadOutcome::Rejected(error)) => {
                    self.counters.received = self.counters.received.saturating_add(1);
                    self.counters.rejected = self.counters.rejected.saturating_add(1);
                    return Err(DriveError::Owner(error));
                }
                Ok(ReadOutcome::Packet(packet)) => {
                    self.counters.received = self.counters.received.saturating_add(1);
                    self.counters.decoded = self.counters.decoded.saturating_add(1);
                    step.received = true;
                    self.cached = Some(packet);
                }
                Err(error) => {
                    self.invalidate(LossReason::TransportFailure);
                    return Err(DriveError::Transport(error));
                }
            }
        }
        if self
            .cached
            .as_ref()
            .and_then(Cached::rua_context)
            .is_some_and(|context| self.session.context_is_closing(context))
        {
            step.backpressure = true;
            self.counters.backpressure = self.counters.backpressure.saturating_add(1);
            return Ok(step);
        }
        self.apply_cached(now, &mut publish)?;
        step.delivered = true;
        step.closed = self.phase == Phase::Closed;
        Ok(step)
    }
}
fn command_error(error: PublishError<Error>) -> Error {
    match error {
        PublishError::Session(error) => Error::Session(error),
        PublishError::Downstream(error) => error,
    }
}
fn base_event(event: &Event) -> &Event {
    match event {
        Event::Notified { event, .. } => event,
        event => event,
    }
}

fn error_indication(
    protocol: Protocol,
    error: &umts_iuh::Error,
    limits: umts_iuh::wire::Limits,
) -> Result<Vec<u8>, umts_iuh::Error> {
    use umts_iuh::{Error as E, ProtocolCause as C};
    let cause = match error {
        E::Decode | E::Length | E::Trailing | E::Capacity => C::TransferSyntax,
        E::UnknownRejectIe(_) | E::MissingIe(_) | E::DuplicateIe(_) | E::IeOrder(_) => {
            C::AbstractSyntaxReject
        }
        E::Value(_) => C::Semantic,
        E::State => C::IncompatibleState,
        _ => C::Unspecified,
    };
    match protocol {
        Protocol::Hnbap => hnbap::Message::ErrorIndication {
            cause: Some(hnbap::Cause::Protocol(cause)),
            diagnostics: None,
        }
        .encode(limits),
        _ => rua::Message::ErrorIndication {
            cause: Some(rua::Cause::Protocol(cause)),
            diagnostics: None,
        }
        .encode(limits),
    }
}
