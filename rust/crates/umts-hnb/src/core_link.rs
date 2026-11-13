// SPDX-License-Identifier: AGPL-3.0-or-later

mod reconnect;
mod selector;

pub use reconnect::Reconnect;

const DISPATCH_FAILURE_REPORT_LIMIT: u8 = 8;

use crate::{
    connection_security::SecurityTime,
    signalling::{
        CommonSignalling, CoreEventKind, RegistrationFailure, SecurityWork, SignallingError,
    },
};
use umts_core::{Gateway, Transport};
#[cfg(test)]
use umts_iuh::session::ControlTime;
use umts_iuh::{
    hnbap::{Capabilities, CsgCapability, RegistrationCause, Release, UeIdentity},
    ranap::{Inbound, Priority},
    rua,
    session::{Event, UplinkNas},
};
use umts_rrc::{
    capabilities::AccessRelease,
    nas::{CoreContextId, DownlinkPriority},
};

#[derive(Debug)]
pub enum DispatchError<E> {
    Radio(SignallingError),
    Other(E),
}
#[derive(Debug)]
pub enum Error<T, U> {
    Security(SignallingError),
    Command(umts_core::Error),
    Drive(umts_core::DriveError<T, DispatchError<U>>),
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub rab_published: bool,
    pub rab_blocked: bool,
    pub packet_cleanup: crate::packet_rabs::Reconciled,
    pub security: SecurityWork,
    pub uplink_published: bool,
    pub uplink_blocked: bool,
    pub uplink_rejected: bool,
    pub gateway: umts_core::Step,
}

pub struct CoreLink<T: Transport> {
    gateway: Gateway<T>,
    reconnect: Option<Reconnect<T>>,
    dispatch_failure_reports: u8,
}
impl<T: Transport> CoreLink<T> {
    pub fn new(gateway: Gateway<T>) -> Self {
        Self {
            gateway,
            reconnect: None,
            dispatch_failure_reports: DISPATCH_FAILURE_REPORT_LIMIT,
        }
    }
    pub fn with_reconnect(gateway: Gateway<T>, reconnect: Reconnect<T>) -> Self {
        Self {
            reconnect: Some(reconnect),
            ..Self::new(gateway)
        }
    }
    pub fn gateway(&self) -> &Gateway<T> {
        &self.gateway
    }
    pub fn reconnect_exhausted(&self) -> bool {
        self.reconnect.as_ref().is_some_and(Reconnect::exhausted)
    }
    pub(crate) fn gateway_mut(&mut self) -> &mut Gateway<T> {
        &mut self.gateway
    }

    // A rejecting procedure consumer must leave radio state unchanged so the retained event can be retried.
    #[cfg(test)]
    pub fn drive<U>(
        &mut self,
        now: ControlTime,
        radio: &mut CommonSignalling,
        other: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
    ) -> Result<Step, Error<T::Error, U>> {
        self.drive_at(
            SecurityTime {
                control: now,
                monotonic: std::time::Instant::now(),
            },
            radio,
            other,
        )
    }

    pub fn drive_at<U>(
        &mut self,
        time: SecurityTime,
        radio: &mut CommonSignalling,
        mut other: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
    ) -> Result<Step, Error<T::Error, U>> {
        let now = time.control;
        let mut result = Step::default();
        let mut command_failure = None;
        if self.gateway.phase() == umts_core::Phase::Serving {
            match self.gateway.publish_ready_rab(now) {
                Ok(published) => result.rab_published = published,
                Err(
                    umts_core::Error::Capacity
                    | umts_core::Error::Session(umts_iuh::Error::Capacity),
                ) => result.rab_blocked = true,
                Err(error) => {
                    trace_core_failure("rab", None, &error);
                    command_failure = Some(error);
                }
            }
            let publication = radio.core().publish_one(|event| {
                let kind = match &event.kind {
                    CoreEventKind::RrcRelease => "rrc_release",
                    CoreEventKind::InactivityRelease { .. } => "inactivity_release",
                    CoreEventKind::RadioLostRelease { .. } => "radio_lost_release",
                    CoreEventKind::SignallingRelease { .. } => "signalling_release",
                    CoreEventKind::SecurityResponse { .. } => "security_response",
                    CoreEventKind::Register { .. } => "register",
                    CoreEventKind::Nas { .. } => "nas",
                    CoreEventKind::RadioFailure { .. } => "radio_failure",
                };
                let key = event.key;
                let outcome = match event.kind {
                    CoreEventKind::RrcRelease => self.gateway.retire_rrc_release(event.key, now),
                    CoreEventKind::InactivityRelease { connection } => {
                        if self.gateway.context(event.key).map(|id| id.get())
                            != Some(connection.context.get())
                        {
                            return Ok(());
                        }
                        self.gateway
                            .request_inactivity_release(
                                now,
                                event.key,
                                umts_iuh::ContextId::new(connection.context.get())
                                    .map_err(umts_core::Error::Session)?,
                                connection.key.domain.into(),
                            )
                            .map(|_| ())
                    }
                    CoreEventKind::RadioLostRelease { connection } => {
                        if self.gateway.context(event.key).map(|id| id.get())
                            != Some(connection.context.get())
                        {
                            return Ok(());
                        }
                        self.gateway
                            .request_radio_lost_release(
                                now,
                                event.key,
                                umts_iuh::ContextId::new(connection.context.get())
                                    .map_err(umts_core::Error::Session)?,
                                connection.key.domain.into(),
                            )
                            .map(|_| ())
                    }
                    CoreEventKind::SignallingRelease { connection } => {
                        if self.gateway.context(event.key).map(|id| id.get())
                            != Some(connection.context.get())
                        {
                            return Ok(());
                        }
                        self.gateway
                            .request_signalling_release(
                                now,
                                event.key,
                                umts_iuh::ContextId::new(connection.context.get())
                                    .map_err(umts_core::Error::Session)?,
                                connection.key.domain.into(),
                            )
                            .map(|_| ())
                    }
                    CoreEventKind::SecurityResponse { request, response } => {
                        if self
                            .gateway
                            .security_response_cancelled_by_iu_release(request)
                        {
                            Ok(())
                        } else {
                            self.gateway
                                .publish_security_response(now, request, response)
                        }
                    }
                    CoreEventKind::Register { request } => self.gateway.register_ue(
                        event.key,
                        match request.identity {
                            crate::signalling::RegistrationIdentity::Imsi(imsi) => {
                                UeIdentity::Imsi(imsi)
                            }
                            crate::signalling::RegistrationIdentity::Tmsi { tmsi, plmn, lac } => {
                                UeIdentity::Tmsi { tmsi, plmn, lac }
                            }
                            crate::signalling::RegistrationIdentity::Ptmsi {
                                ptmsi,
                                plmn,
                                lac,
                                rac,
                            } => UeIdentity::Ptmsi {
                                ptmsi,
                                plmn,
                                lac,
                                rac,
                            },
                        },
                        Capabilities {
                            release: release(request.access_release),
                            csg: if request.supports_csg {
                                CsgCapability::Capable
                            } else {
                                CsgCapability::NotCapable
                            },
                        },
                        if request.emergency {
                            RegistrationCause::Emergency
                        } else {
                            RegistrationCause::Normal
                        },
                        now,
                    ),
                    CoreEventKind::Nas {
                        connection,
                        initial,
                        nas,
                        emergency,
                        ..
                    } => {
                        if self.gateway.context(event.key).map(|id| id.get())
                            != Some(connection.context.get())
                        {
                            result.uplink_rejected = true;
                            trace_core_failure(
                                "stale_nas",
                                Some(event.key),
                                &umts_core::Error::Session(umts_iuh::Error::Origin),
                            );
                            return Ok(());
                        }
                        let selector = match initial
                            .map(|initial| selector::convert(&initial.selector))
                            .transpose()
                        {
                            Ok(selector) => selector,
                            Err(_) => {
                                result.uplink_rejected = true;
                                trace_core_failure(
                                    "nas_selector",
                                    Some(key),
                                    &umts_core::Error::Session(umts_iuh::Error::Identity),
                                );
                                return Ok(());
                            }
                        };
                        self.gateway.publish_nas(
                            now,
                            UplinkNas {
                                key: event.key,
                                domain: connection.key.domain.into(),
                                nas,
                                selector,
                                cause: if emergency {
                                    rua::EstablishmentCause::Emergency
                                } else {
                                    rua::EstablishmentCause::Normal
                                },
                            },
                        )
                    }
                    CoreEventKind::RadioFailure { .. } => {
                        self.gateway.retire_radio_failure(event.key, now)
                    }
                };
                if let Err(error) = &outcome {
                    trace_core_failure(kind, Some(key), error);
                }
                match outcome {
                    Err(umts_core::Error::Session(
                        umts_iuh::Error::Origin
                        | umts_iuh::Error::State
                        | umts_iuh::Error::Identity,
                    )) => {
                        result.uplink_rejected = true;
                        Ok(())
                    }
                    other => other,
                }
            });
            match publication {
                Ok(value) => result.uplink_published = value.is_some(),
                Err(
                    umts_core::Error::Capacity
                    | umts_core::Error::Session(umts_iuh::Error::Capacity),
                ) => result.uplink_blocked = true,
                Err(error) => command_failure = Some(error),
            }
        }
        let reports = &mut self.dispatch_failure_reports;
        result.gateway = self
            .gateway
            .drive(now, |event| {
                let result = dispatch_at(radio, event, time, &mut other);
                if let Err(DispatchError::Radio(error)) = &result
                    && *reports != 0
                {
                    *reports -= 1;
                    let (key, domain) = match event {
                        Event::Ranap { key, domain, .. } => (*key, *domain),
                        Event::SecurityCommand { request, .. } => {
                            (Some(request.key()), Some(request.domain()))
                        }
                        Event::UeAccepted { key, .. } => (Some(*key), None),
                        _ => (None, None),
                    };
                    tracing::warn!(
                        "Core dispatch failure: kind={:?} key={key:?} domain={domain:?} phase={:?} connection={:?} error={error:?}",
                        event.kind(),
                        key.and_then(|key| radio.phase(key)),
                        key.zip(domain).and_then(|(key, domain)| radio.connection(key, domain.into())),
                    );
                }
                result
            })
            .map_err(Error::Drive)?;
        if let Some(reconnect) = self.reconnect.as_mut()
            && let Some(gateway) =
                reconnect.service(self.gateway.phase(), self.gateway.loss_reason(), now)
        {
            self.gateway = gateway;
        }
        if let Some(error) = command_failure {
            return Err(Error::Command(error));
        }
        if self.gateway.phase() == umts_core::Phase::Serving {
            result.security = radio.service_security_one(time).map_err(Error::Security)?;
        }
        Ok(result)
    }
}

fn trace_core_failure(kind: &str, key: Option<umts_types::UeKey>, error: &umts_core::Error) {
    tracing::debug!("Core command failed: kind={kind} key={key:?} error={error:?}");
}

fn release(value: AccessRelease) -> Release {
    match value {
        AccessRelease::R99 => Release::R99,
        AccessRelease::R4 => Release::R4,
        AccessRelease::R5 => Release::R5,
        AccessRelease::R6 => Release::R6,
        AccessRelease::R7 => Release::R7,
        AccessRelease::R8
        | AccessRelease::R9
        | AccessRelease::R10
        | AccessRelease::R11
        | AccessRelease::R12
        | AccessRelease::R13
        | AccessRelease::R14
        | AccessRelease::R15
        | AccessRelease::R16 => Release::R8AndLater,
    }
}

#[cfg(test)]
fn dispatch<U>(
    radio: &mut CommonSignalling,
    event: &Event,
    now: ControlTime,
    other: &mut impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
) -> Result<(), DispatchError<U>> {
    dispatch_at(
        radio,
        event,
        SecurityTime {
            control: now,
            monotonic: std::time::Instant::now(),
        },
        other,
    )
}

fn dispatch_at<U>(
    radio: &mut CommonSignalling,
    event: &Event,
    now: SecurityTime,
    other: &mut impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
) -> Result<(), DispatchError<U>> {
    let map = DispatchError::Radio;
    match event {
        Event::Ranap {
            key: None,
            domain: None,
            message,
        } if matches!(message.message, Inbound::Paging(_)) => {
            let Inbound::Paging(request) = &message.message else {
                unreachable!()
            };
            radio.receive_paging(request);
            Ok(())
        }
        Event::IuRelease { request, .. } | Event::Disconnected { request, .. } => {
            radio.begin_core_connection_release(*request).map_err(map)?;
            other(radio, event).map_err(DispatchError::Other)
        }
        Event::SecurityCommand {
            request,
            received_at,
            command,
            ..
        } => radio
            .receive_security_command(now, *received_at, *request, command)
            .map_err(map),
        Event::HnbAccepted { rnc } => radio.accept_rnc(*rnc).map_err(map),
        Event::UeAccepted { key, context, .. } => {
            if radio.phase(*key).is_none() {
                return Ok(());
            }
            radio
                .registration_accepted(
                    *key,
                    CoreContextId::new(context.get())
                        .map_err(|error| map(SignallingError::Nas(error)))?,
                )
                .map_err(map)
        }
        Event::UeRejected { key, .. } => {
            radio.core_registration_lost(*key, RegistrationFailure::CoreRejected);
            Ok(())
        }
        Event::CancelledRegistrationAccepted { .. } => Ok(()),
        Event::UeDeregistered { key, .. } => {
            radio.core_registration_lost(*key, RegistrationFailure::CoreUnavailable);
            Ok(())
        }
        Event::AssociationLost { .. } => {
            radio.core_association_lost();
            Ok(())
        }
        Event::Ranap {
            key: Some(key),
            domain: Some(domain),
            message,
        } if matches!(message.message, Inbound::CommonIdentity(_)) => {
            if radio.phase(*key).is_none() {
                return Ok(());
            }
            let Inbound::CommonIdentity(imsi) = message.message else {
                unreachable!()
            };
            let Some(connection) = radio.connection(*key, (*domain).into()) else {
                return Ok(());
            };
            match radio.associate_core_identity(connection, imsi) {
                Ok(()) => Ok(()),
                // A contradictory notification cannot replace a known identity or block other connections.
                Err(SignallingError::IdentityConflict) => Ok(()),
                Err(error) => Err(map(error)),
            }
        }
        Event::Ranap {
            key: Some(key),
            domain: Some(domain),
            message,
        } if matches!(message.message, Inbound::Direct { .. }) => {
            if radio.phase(*key).is_none() {
                return Ok(());
            }
            let Inbound::Direct { nas, priority } = &message.message else {
                unreachable!()
            };
            let connection = radio
                .connection(*key, (*domain).into())
                .ok_or_else(|| map(SignallingError::Phase))?;
            radio
                .downlink_nas(
                    connection,
                    match priority {
                        Priority::Signalling => DownlinkPriority::Signalling,
                        Priority::Sms => DownlinkPriority::Sms,
                        _ => return Err(map(SignallingError::Message)),
                    },
                    nas,
                )
                .map_err(map)
        }
        // TS 25.413 10.3.4.3: an ignore-and-notify IE is reported and the procedure continues.
        // RAB assignments keep the wrapper so their response can report it in Criticality
        // Diagnostics.
        Event::Notified {
            protocol,
            notify_ies,
            event: inner,
        } if !matches!(**inner, Event::RabAssignment { .. }) => {
            log_core_event(&format!(
                "Core {protocol:?} message included ignored IEs {notify_ies:?}, procedure continues"
            ));
            dispatch_at(radio, inner, now, other)
        }
        // TS 25.413 8.27.3 and TS 25.469 8.6: a received Error Indication ends no procedure and
        // needs no reply. It is logged and consumed so it cannot block later core messages.
        Event::HnbapError(message) => {
            log_core_event(&format!(
                "HNBAP error indication from the gateway: {message:?}"
            ));
            Ok(())
        }
        Event::ProtocolError { key, domain, cause } => {
            log_core_event(&format!(
                "RANAP message for ue={key:?} domain={domain:?} rejected with protocol cause {cause}, ERROR INDICATION queued"
            ));
            Ok(())
        }
        Event::RuaError(message) => {
            log_core_event(&format!(
                "RUA error indication from the gateway: {message:?}"
            ));
            Ok(())
        }
        Event::Ranap {
            message:
                umts_iuh::ranap::Decoded {
                    message: Inbound::Procedure(packet),
                    ..
                },
            key,
            domain,
        } => {
            // TS 25.413 8.25, 8.27, 8.17 and 8.21 need no reply. Unsupported procedures are
            // consumed so they cannot stall the core queue.
            log_core_event(&format!(
                "Unsupported RANAP procedure {} ({:?}) from the core for ue={key:?} domain={domain:?} consumed",
                packet.procedure, packet.outcome
            ));
            Ok(())
        }
        // Unimplemented procedures retain gateway ownership unless their actual consumer accepts them.
        _ => other(radio, event).map_err(DispatchError::Other),
    }
}

#[cfg(test)]
pub(crate) mod tests;

fn log_core_event(message: &str) {
    tracing::info!("{message}");
}
