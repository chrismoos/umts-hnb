// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_rrc::nas::{
    build_downlink_nas,
    identification::{Identity, Observation, Phase, Poll, PublishError},
    inspect_uplink_nas, pending,
};
use umts_rrc_codec::DecodedUplinkDedicated;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationFailure {
    IdentityUnavailable,
    IdentityTimeout,
    CoreUnavailable,
    CoreRejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegistrationRequest {
    pub identity: RegistrationIdentity,
    pub access_release: AccessRelease,
    pub supports_csg: bool,
    pub emergency: bool,
}

/// The UE identity in HNBAP UE REGISTER REQUEST.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationIdentity {
    Imsi(Imsi),
    Tmsi {
        tmsi: [u8; 4],
        plmn: [u8; 3],
        lac: u16,
    },
    Ptmsi {
        ptmsi: [u8; 4],
        plmn: [u8; 3],
        lac: u16,
        rac: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionReleaseProgress {
    PendingSecurity,
    PendingNasDelivery,
    Complete,
}

impl Context {
    fn registration_request(&self, identity: RegistrationIdentity) -> RegistrationRequest {
        RegistrationRequest {
            identity,
            access_release: self.access_release,
            supports_csg: self.supports_csg,
            emergency: self.emergency,
        }
    }
}

impl CommonSignalling {
    pub(crate) fn begin_core_connection_release(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
    ) -> Result<(), SignallingError> {
        self.core.begin_connection_release(request)
    }
    pub(crate) fn finish_core_connection_release(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
    ) {
        self.core.finish_connection_release(request);
    }
    pub(crate) fn notify_owned_connection_release(
        &mut self,
        request: umts_iuh::session::IuReleaseRequest,
    ) -> Result<ConnectionReleaseProgress, SignallingError> {
        match self.core.connection_release_notified(request) {
            Some(true) => return Ok(ConnectionReleaseProgress::Complete),
            None if self.registry.get(request.key()).is_none() => {
                return Ok(ConnectionReleaseProgress::Complete);
            }
            None => return Err(SignallingError::Phase),
            Some(false) => (),
        }
        let progress =
            self.notify_core_connection_release(request.key(), request.domain().into())?;
        if progress == ConnectionReleaseProgress::Complete {
            self.core.mark_connection_release_notified(request);
        }
        Ok(progress)
    }
    pub fn notify_core_connection_release(
        &mut self,
        key: UeKey,
        domain: umts_types::CnDomain,
    ) -> Result<ConnectionReleaseProgress, SignallingError> {
        let Some(context) = self.registry.get_mut(key) else {
            return Ok(ConnectionReleaseProgress::Complete);
        };
        if context.security.phase(domain.into()) != crate::connection_security::Phase::Idle {
            return Ok(ConnectionReleaseProgress::PendingSecurity);
        }
        let router = context.router.as_mut().ok_or(SignallingError::Phase)?;
        let Some(connection) = router.connection(domain) else {
            return Ok(ConnectionReleaseProgress::Complete);
        };
        if context
            .nas_deliveries
            .pending(&mut context.nas_bearer, connection)?
        {
            return Ok(ConnectionReleaseProgress::PendingNasDelivery);
        }
        let bearer = &mut context.control;
        router
            .release_scheduled_with(
                connection,
                &mut context.protection.integrity,
                |selected, message| {
                    if selected != SignallingBearer::ControlAm {
                        return Err(SignallingError::Mapping);
                    }
                    bearer
                        .enqueue(
                            key,
                            Bits::new(&message.bytes, message.bit_length)
                                .map_err(|_| SignallingError::Message)?,
                        )
                        .map_err(SignallingError::Am)
                },
            )
            .map_err(|error| match error {
                umts_rrc::nas::NasScheduledError::Nas(error) => SignallingError::Nas(error),
                umts_rrc::nas::NasScheduledError::Schedule(error) => {
                    super::protection::schedule_error(error, |error| error)
                }
            })?;
        context
            .security
            .release_signalling(domain.into())
            .map_err(super::security_error)?;
        context.release_after_core_disconnect = router.connections().iter().all(Option::is_none);
        Ok(ConnectionReleaseProgress::Complete)
    }
    #[cfg(test)]
    pub fn permanent_identity(&self, key: UeKey) -> Option<Imsi> {
        self.registry.get(key)?.core_imsi
    }

    // TS 25.413 8.16.2 retains the permanent identity on the RRC owner, not only its Iu domain.
    pub fn associate_core_identity(
        &mut self,
        connection: CoreConnection,
        imsi: Imsi,
    ) -> Result<(), SignallingError> {
        let context = self
            .registry
            .get_mut(connection.key.ue)
            .ok_or(SignallingError::Origin)?;
        if context
            .router
            .as_ref()
            .and_then(|router| router.connection(connection.key.domain))
            != Some(connection)
        {
            return Err(SignallingError::Origin);
        }
        if context.core_imsi.is_some_and(|known| known != imsi) {
            self.counters.conflicting_core_identities =
                self.counters.conflicting_core_identities.saturating_add(1);
            return Err(SignallingError::IdentityConflict);
        }
        context.core_imsi = Some(imsi);
        self.counters.confirmed_core_identities =
            self.counters.confirmed_core_identities.saturating_add(1);
        Ok(())
    }

    pub fn accept_rnc(&mut self, rnc: u16) -> Result<(), SignallingError> {
        AssignedRntis::new(rnc, 0, 0).map_err(|_| SignallingError::Mapping)?;
        if rnc != self.rnc && !self.identities.is_empty() {
            return Err(SignallingError::Phase);
        }
        self.rnc = rnc;
        Ok(())
    }

    pub fn core_registration_lost(&mut self, key: UeKey, reason: RegistrationFailure) {
        self.core.cancel_for(key);
        if let Some(context) = self.registry.get_mut(key) {
            context.security.retire();
            if let Registration::Active(id) = context.registration {
                self.gateway_contexts.remove(&id);
            }
            context.router = None;
            context.registration = Registration::ReleaseRequired(reason);
        }
    }

    pub fn core_association_lost(&mut self) {
        for slot in 0..self.registry.capacity() {
            let Some((key, context)) = self.registry.at_slot(slot) else {
                continue;
            };
            if matches!(
                context.registration,
                Registration::Pending | Registration::Active(_)
            ) {
                let context = self.registry.get_mut(key).expect("occupied core owner");
                context.security.retire();
                context.router = None;
                context.registration =
                    Registration::ReleaseRequired(RegistrationFailure::CoreUnavailable);
            }
        }
        self.gateway_contexts.clear();
        self.core.clear();
    }

    #[cfg(test)]
    pub fn registration_failure(&self, key: UeKey) -> Option<RegistrationFailure> {
        match self.registry.get(key)?.registration {
            Registration::ReleaseRequired(reason) => Some(reason),
            _ => None,
        }
    }

    fn registered_owner(&self, imsi: Imsi, except: UeKey) -> Option<UeKey> {
        (0..self.registry.capacity()).find_map(|slot| {
            let (key, context) = self.registry.at_slot(slot)?;
            (key != except && context.registration_imsi == Some(imsi)).then_some(key)
        })
    }

    pub(super) fn service_registration_one(&mut self) -> Option<Result<UeKey, SignallingError>> {
        for _ in 0..self.registry.capacity() {
            let slot = self.registration_cursor;
            self.registration_cursor = (slot + 1) % self.registry.capacity();
            let Some((key, context)) = self.registry.at_slot(slot) else {
                continue;
            };
            let ready = context
                .identification
                .as_ref()
                .is_some_and(|procedure| matches!(procedure.phase(), Phase::Ready(_)));
            let replay = matches!(context.registration, Registration::Active(_))
                && !context.deferred.is_empty();
            if !ready && !replay {
                continue;
            }
            // A UE holds one RRC connection, so an IMSI already registered by another context
            // means that connection is gone. Retire it first: Iuh refuses a duplicate identity
            // on every retry.
            let identified = match context
                .identification
                .as_ref()
                .map(|procedure| procedure.phase())
            {
                Some(Phase::Ready(response)) => match response.identity {
                    Identity::Imsi(imsi) => Some(imsi),
                    Identity::Unavailable => None,
                },
                _ => None,
            };
            if ready
                && let Some(imsi) = identified
                && let Some(stale) = self.registered_owner(imsi, key)
            {
                tracing::info!(
                    "Retiring stale context {stale:?} whose IMSI re-registers as {key:?}"
                );
                return Some(self.retire(stale).map(|()| stale));
            }
            let context = self
                .registry
                .get_mut(key)
                .expect("occupied registration owner");
            if ready {
                return Some(publish_identity(context, &mut self.core, key).map(|()| key));
            }
            let router = context.router.as_mut().expect("registered NAS router");
            let emergency = context.emergency;
            return Some(
                context
                    .deferred
                    .forward_one_with(router, |event| self.core.push_nas(event, emergency))
                    .map(|_| key)
                    .map_err(deferred_error),
            );
        }
        None
    }
}

fn enqueue_request(
    context: &mut CommonAmBearer,
    key: UeKey,
    domain: umts_types::CnDomain,
    nas: &[u8],
) -> Result<(), SignallingError> {
    let message = build_downlink_nas(
        TransactionId::new(0).map_err(|_| SignallingError::Message)?,
        domain,
        nas,
    )
    .map_err(SignallingError::Nas)?;
    context
        .enqueue(
            key,
            Bits::new(&message.bytes, message.bit_length).map_err(|_| SignallingError::Message)?,
        )
        .map_err(|error| match error {
            CommonAmError::Rlc(umts_rlc::acknowledged::AmError::Capacity) => {
                SignallingError::Capacity
            }
            error => SignallingError::Am(error),
        })
}

fn publish_identity(
    context: &mut Context,
    core: &mut CoreOutbox,
    key: UeKey,
) -> Result<(), SignallingError> {
    let procedure = context
        .identification
        .as_mut()
        .ok_or(SignallingError::Phase)?;
    procedure
        .publish_with(|response| {
            match response.identity {
                Identity::Imsi(imsi) => {
                    core.push_registration(
                        key,
                        RegistrationRequest {
                            identity: RegistrationIdentity::Imsi(imsi),
                            access_release: context.access_release,
                            supports_csg: context.supports_csg,
                            emergency: context.emergency,
                        },
                    )?;
                    context.registration_imsi = Some(imsi);
                    context.registration = Registration::Pending;
                }
                Identity::Unavailable => {
                    context.registration =
                        Registration::ReleaseRequired(RegistrationFailure::IdentityUnavailable);
                }
            }
            Ok(())
        })
        .map_err(identity_error)
}

pub(super) fn receive_before_registration(
    context: &mut Context,
    core: &mut CoreOutbox,
    key: UeKey,
    bearer: SignallingBearer,
    now: ChipInstant,
    message: &DecodedUplinkDedicated,
    tmsi_registration: Option<[u8; 3]>,
) -> Result<UeKey, SignallingError> {
    if let Registration::ReleaseRequired(reason) = context.registration {
        return Err(SignallingError::IdentityReleaseRequired(reason));
    }
    let inspected = inspect_uplink_nas(bearer, message, false).map_err(SignallingError::Nas)?;
    if let Some(procedure) = context.identification.as_mut() {
        let mut candidate = procedure.clone();
        let observation = candidate
            .observe_response(key, inspected.domain(), now, inspected.nas())
            .map_err(SignallingError::Identification)?;
        if observation != Observation::Unrelated {
            // The locally terminated IDENTITY RESPONSE consumes one MM send sequence number and no
            // MM NULL fills it.
            // On Iu the core ignores N(SD) for duplicate detection, and a NULL on the initial
            // connection makes the MSC reject the location update.
            // Queue custody must be established before consuming the locally terminated response.
            *procedure = candidate;
            return Ok(key);
        }
    }
    if context.registration == Registration::Unregistered && inspected.initial().is_none() {
        return Err(SignallingError::Message);
    }
    if context.registration == Registration::Unregistered
        && context.registration_imsi.is_none()
        && !matches!(
            context.identity,
            InitialUEIdentity::tmsi_and_LAI(_)
                | InitialUEIdentity::p_TMSI_and_RAI(_)
                | InitialUEIdentity::imei(_)
        )
    {
        return Err(SignallingError::Nas(NasError::Unsupported));
    }
    let domain = inspected.domain();
    let reported = inspected
        .initial()
        .map(|metadata| (metadata.supports_csg, metadata.establishment_cause));
    context
        .deferred
        .hold(key, inspected)
        .map_err(|error| match error {
            pending::Error::Capacity => SignallingError::Capacity,
            _ => SignallingError::Phase,
        })?;
    if context.registration != Registration::Unregistered {
        return Ok(key);
    }
    if let Some((supports_csg, cause)) = reported {
        context.supports_csg = supports_csg;
        if let Some(cause) = cause {
            context.emergency =
                cause == umts_rrc_schema::information_elements::EstablishmentCause::emergencyCall;
        }
    }
    if let Some(imsi) = context.registration_imsi {
        core.push_registration(
            key,
            context.registration_request(RegistrationIdentity::Imsi(imsi)),
        )?;
        context.registration = Registration::Pending;
        return Err(SignallingError::RegistrationPending);
    }
    // A UE that presented a TMSI/LAI or P-TMSI/RAI can be registered with that identity when
    // the gateway accepts it. That avoids a local identity request, whose IDENTITY RESPONSE
    // would consume a NAS send sequence number that the core never sees.
    if let Some(cell_plmn) = tmsi_registration
        && let Some(identity) = temporary_identity(&context.identity, cell_plmn)
    {
        core.push_registration(key, context.registration_request(identity))?;
        context.registration = Registration::Pending;
        return Err(SignallingError::RegistrationPending);
    }
    let mut procedure =
        Identification::new(key, domain, now).map_err(SignallingError::Identification)?;
    procedure
        .begin_with(now, |nas| {
            enqueue_request(&mut context.nas_bearer, key, domain, nas)
        })
        .map_err(identity_error)?;
    context.identification = Some(procedure);
    context.registration = Registration::Identifying;
    Ok(key)
}

pub(super) fn poll_identification(
    context: &mut Context,
    now: ChipInstant,
) -> Result<(), SignallingError> {
    let Some(procedure) = context.identification.as_mut() else {
        return Ok(());
    };
    let domain = procedure.domain();
    let key = procedure.key();
    let poll = procedure
        .poll_with(now, |nas| {
            enqueue_request(&mut context.nas_bearer, key, domain, nas)
        })
        .map_err(identity_error)?;
    if poll == Poll::Expired {
        context.registration = Registration::ReleaseRequired(RegistrationFailure::IdentityTimeout);
    }
    Ok(())
}

fn identity_error(error: PublishError<SignallingError>) -> SignallingError {
    match error {
        PublishError::Procedure(error) => SignallingError::Identification(error),
        PublishError::Downstream(error) => error,
    }
}
fn deferred_error(error: pending::PublishError<SignallingError>) -> SignallingError {
    match error {
        pending::PublishError::Owner(pending::Error::Capacity) => SignallingError::Capacity,
        pending::PublishError::Owner(_) => SignallingError::Phase,
        pending::PublishError::Nas(error) => SignallingError::Nas(error),
        pending::PublishError::Downstream(error) => error,
    }
}

#[cfg(test)]
mod tests;

fn plmn_octets(
    plmn: &umts_rrc_schema::information_elements::PLMNIdentity,
    cell_plmn: [u8; 3],
) -> [u8; 3] {
    // TS 24.008 10.5.1.3 BCD layout, as umts_config::parse_plmn encodes the cell PLMN.
    let mcc: Vec<u8> = plmn.mcc.0.iter().map(|digit| digit.0).collect();
    let mnc: Vec<u8> = plmn.mnc.0.iter().map(|digit| digit.0).collect();
    if mcc.len() != 3 || !(2..=3).contains(&mnc.len()) {
        return cell_plmn;
    }
    let mnc3 = if mnc.len() == 3 { mnc[2] } else { 0xf };
    [
        (mcc[1] << 4) | mcc[0],
        (mnc3 << 4) | mcc[2],
        (mnc[1] << 4) | mnc[0],
    ]
}

fn bit_word(bits: &rasn::types::BitStr, width: usize) -> u32 {
    bits.iter()
        .take(width)
        .fold(0, |value, bit| (value << 1) | u32::from(*bit))
}

fn temporary_identity(
    identity: &InitialUEIdentity,
    cell_plmn: [u8; 3],
) -> Option<RegistrationIdentity> {
    match identity {
        InitialUEIdentity::tmsi_and_LAI(value) => Some(RegistrationIdentity::Tmsi {
            tmsi: bit_word(&value.tmsi.0, 32).to_be_bytes(),
            plmn: plmn_octets(&value.lai.plmn_identity, cell_plmn),
            lac: bit_word(&value.lai.lac, 16) as u16,
        }),
        InitialUEIdentity::p_TMSI_and_RAI(value) => Some(RegistrationIdentity::Ptmsi {
            ptmsi: bit_word(&value.p_tmsi.0, 32).to_be_bytes(),
            plmn: plmn_octets(&value.rai.lai.plmn_identity, cell_plmn),
            lac: bit_word(&value.rai.lai.lac, 16) as u16,
            rac: bit_word(&value.rai.rac.0, 8) as u8,
        }),
        _ => None,
    }
}
