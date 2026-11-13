// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IuReleaseRequest {
    owner: NonZeroU64,
    association: AssociationId,
    key: UeKey,
    domain: Domain,
    context: ContextId,
    connection: NonZeroU64,
}

impl IuReleaseRequest {
    pub(super) fn new(
        owner: NonZeroU64,
        association: AssociationId,
        key: UeKey,
        domain: Domain,
        context: ContextId,
        connection: NonZeroU64,
    ) -> Self {
        Self {
            owner,
            association,
            key,
            domain,
            context,
            connection,
        }
    }
    pub fn key(self) -> UeKey {
        self.key
    }
    pub fn domain(self) -> Domain {
        self.domain
    }
    pub fn context(self) -> ContextId {
        self.context
    }
    pub fn association(self) -> AssociationId {
        self.association
    }
    pub fn same_connection(self, rab: RabRequest) -> bool {
        rab.owned_connection(self.owner, self.connection)
            && self.association == rab.association()
            && self.key == rab.key()
            && self.domain == rab.domain()
            && self.context == rab.context()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReleaseRequestCause {
    UeSignalling,
    UserInactivity,
    RadioLost,
    UtranGenerated,
}

impl Session {
    pub fn request_signalling_release_with<T, E>(
        &mut self,
        key: UeKey,
        context: ContextId,
        domain: Domain,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<bool, PublishError<E>> {
        self.request_release_with(key, context, domain, now, false, publish)
    }
    pub fn request_inactivity_release_with<T, E>(
        &mut self,
        key: UeKey,
        context: ContextId,
        domain: Domain,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<bool, PublishError<E>> {
        self.request_release_with(key, context, domain, now, true, publish)
    }
    /// TS 25.413 8.4.2: Iu Release Request with cause "Radio Connection With UE Lost".
    pub fn request_radio_lost_release_with<T, E>(
        &mut self,
        key: UeKey,
        context: ContextId,
        domain: Domain,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<bool, PublishError<E>> {
        self.request_release_cause_with(
            key,
            context,
            domain,
            now,
            ReleaseRequestCause::RadioLost,
            publish,
        )
    }
    pub fn request_ps_transport_release_with<T, E>(
        &mut self,
        key: UeKey,
        context: ContextId,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<bool, PublishError<E>> {
        self.request_release_cause_with(
            key,
            context,
            Domain::Ps,
            now,
            ReleaseRequestCause::UtranGenerated,
            publish,
        )
    }
    fn request_release_with<T, E>(
        &mut self,
        key: UeKey,
        context: ContextId,
        domain: Domain,
        now: ControlTime,
        inactivity: bool,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<bool, PublishError<E>> {
        let cause = if inactivity {
            ReleaseRequestCause::UserInactivity
        } else {
            ReleaseRequestCause::UeSignalling
        };
        self.request_release_cause_with(key, context, domain, now, cause, publish)
    }
    fn request_release_cause_with<T, E>(
        &mut self,
        key: UeKey,
        context: ContextId,
        domain: Domain,
        now: ControlTime,
        cause: ReleaseRequestCause,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<bool, PublishError<E>> {
        let inactivity = cause == ReleaseRequestCause::UserInactivity;
        let fail = PublishError::Session;
        self.ready(now).map_err(fail)?;
        let ue = self.ues.get(&key).ok_or(fail(Error::Origin))?;
        if !matches!(ue.registration, Registration::Active { context: actual, cancelled: false } if actual == context)
        {
            return Err(fail(Error::Origin));
        }
        let slot = domain_index(domain);
        if ue.domains[slot] != LinkState::Open
            || ue.signalling_release_requested[slot]
            || (inactivity && ue.inactivity_release_requested[slot])
        {
            return Ok(false);
        }
        let data = rua::Message::Direct {
            context,
            domain,
            ranap: match cause {
                ReleaseRequestCause::UserInactivity => {
                    ranap::release::request_for_user_inactivity(self.limits.codec)
                }
                ReleaseRequestCause::RadioLost => {
                    ranap::release::request_for_radio_connection_lost(self.limits.codec)
                }
                ReleaseRequestCause::UtranGenerated => {
                    ranap::release::request_for_utran_generated_release(self.limits.codec)
                }
                ReleaseRequestCause::UeSignalling => {
                    ranap::release::request_for_ue_signalling_release(self.limits.codec)
                }
            }
            .map_err(fail)?,
        }
        .encode(self.limits.codec)
        .map_err(fail)?;
        publish(Frame {
            association: self.association,
            scope: FrameScope::Ue {
                key,
                domain: Some(domain),
            },
            protocol: Protocol::Rua,
            data,
        })
        .map_err(PublishError::Downstream)?;
        let ue = self
            .ues
            .get_mut(&key)
            .expect("validated signaling release owner");
        if inactivity {
            ue.inactivity_release_requested[slot] = true;
        } else {
            ue.signalling_release_requested[slot] = true;
        }
        self.changed_at = now;
        Ok(true)
    }

    /// TS 25.413 8.3: RAB RELEASE REQUEST on an open connection. `rabs` pairs each RAB ID
    /// with its RANAP cause.
    pub fn request_rab_release_with<T, E>(
        &mut self,
        key: UeKey,
        domain: Domain,
        rabs: &[(u8, u16)],
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let fail = PublishError::Session;
        self.ready(now).map_err(fail)?;
        let ue = self.ues.get(&key).ok_or(fail(Error::Origin))?;
        let Registration::Active {
            context,
            cancelled: false,
        } = ue.registration
        else {
            return Err(fail(Error::State));
        };
        if ue.domains[domain_index(domain)] != LinkState::Open {
            return Err(fail(Error::State));
        }
        let data = rua::Message::Direct {
            context,
            domain,
            ranap: ranap::rab_release_request(rabs, self.limits.codec).map_err(fail)?,
        }
        .encode(self.limits.codec)
        .map_err(fail)?;
        let result = publish(Frame {
            association: self.association,
            scope: FrameScope::Ue {
                key,
                domain: Some(domain),
            },
            protocol: Protocol::Rua,
            data,
        })
        .map_err(PublishError::Downstream)?;
        self.changed_at = now;
        Ok(result)
    }

    pub fn iu_release_for_rab(&self, rab: RabRequest) -> Option<IuReleaseRequest> {
        self.ues.get(&rab.key())?.release_requests[domain_index(rab.domain())]
            .filter(|request| request.same_connection(rab) && self.owns_iu_release(*request))
    }
    pub fn owns_iu_release(&self, request: IuReleaseRequest) -> bool {
        request.owner == self.owner
            && request.association == self.association
            && self.ues.get(&request.key).is_some_and(|ue| {
                let slot = domain_index(request.domain);
                matches!(ue.registration, Registration::Active { context, cancelled: false }
                    if context == request.context)
                    && ue.domains[slot] == LinkState::Releasing
                    && ue.connections[slot] == request.connection.get()
                    && ue.release_requests[slot] == Some(request)
            })
    }

    pub fn peer_disconnected_release(&self, request: IuReleaseRequest) -> bool {
        self.owns_iu_release(request)
            && self.ues[&request.key].peer_disconnected[domain_index(request.domain)]
    }

    // TS 25.468 8.4.3 closes the peer connection without a further wire response.
    pub fn complete_peer_disconnect(
        &mut self,
        request: IuReleaseRequest,
        now: ControlTime,
    ) -> Result<(), Error> {
        self.ready(now)?;
        if !self.peer_disconnected_release(request) {
            return Err(Error::Origin);
        }
        self.finish_connection_release(request, now);
        Ok(())
    }

    // TS 25.413 8.5.2 requires idle Iu user-plane resources and no outstanding reports before this call.
    pub fn complete_iu_release_without_reports_with<T, E>(
        &mut self,
        request: IuReleaseRequest,
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.complete_iu_release_with(request, &[], now, publish)
    }
    // TS 25.413 8.5.2: Iu Release Complete includes the RABs Data Volume Report List for every
    // PS RAB whose assignment asked for data volume reporting.
    pub fn complete_iu_release_with<T, E>(
        &mut self,
        request: IuReleaseRequest,
        reports: &[ranap::release::VolumeReport],
        now: ControlTime,
        publish: impl FnOnce(Frame) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        let fail = PublishError::Session;
        self.ready(now).map_err(fail)?;
        if !self.owns_iu_release(request) || self.peer_disconnected_release(request) {
            return Err(fail(Error::Origin));
        }
        let data = rua::Message::Disconnect {
            context: request.context,
            domain: request.domain,
            cause: rua::Cause::Radio(rua::RadioCause::Normal),
            ranap: Some(ranap::release::complete(reports, self.limits.codec).map_err(fail)?),
        }
        .encode(self.limits.codec)
        .map_err(fail)?;
        let result = publish(Frame {
            association: self.association,
            scope: FrameScope::Ue {
                key: request.key,
                domain: Some(request.domain),
            },
            protocol: Protocol::Rua,
            data,
        })
        .map_err(PublishError::Downstream)?;
        self.finish_connection_release(request, now);
        Ok(result)
    }

    fn finish_connection_release(&mut self, request: IuReleaseRequest, now: ControlTime) {
        let ue = self
            .ues
            .get_mut(&request.key)
            .expect("validated release owner");
        let slot = domain_index(request.domain);
        ue.domains[slot] = LinkState::Closed;
        ue.signalling_release_requested[slot] = false;
        ue.inactivity_release_requested[slot] = false;
        ue.release_requests[slot] = None;
        ue.peer_disconnected[slot] = false;
        ue.security_requests[slot] = None;
        ue.rab_requests[slot].clear();
        ue.security_algorithms[slot] = None;
        self.changed_at = now;
    }
}
