// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_control::{ControlDecision, ControlError, ControlIngress},
    common_rx::{IngressError, RachIngress},
    common_tx::{CommonTxPort, CommonTxTarget, PlanEvent},
    connection_security::SecurityTime,
    core_link::{self, CoreLink},
    signalling::{
        CommonIngressStep, CommonSignalling, FachWork, IngressWork, SecurityAction, SignallingError,
    },
};
use std::{
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Instant,
};
use umts_core::Transport;
use umts_dsp::prach_message::DecodedPrachMessage;
use umts_iuh::session::{ControlTime, Event};
use umts_mac::{CommonHeader, CommonIdentity, MacError};
use umts_rrc::establishment::{EstablishmentPhase, TransactionId};
use umts_types::{
    UeKey,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

mod addition;
pub use addition::{AdditionalServiceError, AdditionalServiceSettings};
mod cleanup;
mod dormancy;
mod hsdpa;
mod hsupa;
pub use hsdpa::{
    HsdpaAdmissionRequest, HsdpaAdmissionStep, HsdpaControlStep, InitialCatalogAdmission,
    SecondaryHsdpaAdmission,
};
pub use hsupa::{HsupaCandidate, HsupaCleanupError, HsupaControlStep, PreparedHsupaAdmission};
mod media;
mod packet_modification;
mod release;
pub use media::{MediaServiceError, MediaServiceFailure, MediaServiceStep};
pub use release::IuCompletionError;
pub(crate) use release::IuReleaseLaunch;
pub use release::ReleaseServiceError;
pub(crate) mod diagnostics;
pub(crate) mod status;
mod uplink;

#[derive(Clone, Copy)]
pub struct ControllerTime {
    pub radio: ChipInstant,
    pub security: SecurityTime,
}

#[derive(Clone, Copy, Debug)]
pub struct ControllerSettings {
    pub maximum_ingress_age_chips: NonZeroU64,
    pub preparation_guard_chips: NonZeroU64,
    pub maximum_future_frames: NonZeroUsize,
    pub fach_scan_budget: NonZeroUsize,
    pub retirement_scan_budget: NonZeroUsize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControllerError {
    Configuration,
    Epoch,
    Time,
    Closed,
    Ingress(IngressError),
    Control(ControlError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RachRejection {
    Mac(MacError),
    Mapping,
    UnknownIdentity,
    Signalling(SignallingError),
    Control(ControlError),
    Time,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RachWork {
    CommonQueued,
    DedicatedAccepted(UeKey),
    Deferred,
    Rejected(RachRejection),
}

#[derive(Clone, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct RachStep {
    pub transport: DecodedPrachMessage,
    pub block: usize,
    pub work: RachWork,
}

pub struct UplinkFailure<E> {
    pub metadata: crate::dedicated_bearers::UpperSdu,
    pub error: E,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct ControllerStep<T, U, E = std::convert::Infallible> {
    pub hsdpa: HsdpaControlStep,
    pub hsupa: Option<anyhow::Result<HsupaControlStep>>,
    /// Only the first typed error is retained. Every failed delivery remains pending and is counted.
    pub uplink_failure: Option<UplinkFailure<E>>,
    pub uplink_failures: usize,
    pub admission: Option<
        Result<crate::rel99_admission::AdmissionStep, crate::rel99_admission::AdmissionError>,
    >,
    pub dedicated_bearers: Option<
        Result<
            crate::dedicated_controller::DedicatedStep<DedicatedProcessingError>,
            crate::dedicated_controller::DedicatedControlError,
        >,
    >,
    pub rach: Option<RachStep>,
    pub common: Option<CommonIngressStep>,
    pub collected: Result<IngressWork, SignallingError>,
    pub dedicated: Option<Result<UeKey, SignallingError>>,
    pub core: Result<core_link::Step, core_link::Error<T, U>>,
    pub security: Option<Result<SecurityAction, SignallingError>>,
    pub fach: Result<FachWork, SignallingError>,
    pub frame: Option<ChipInstant>,
    pub transmit: Option<PlanEvent>,
    pub cancelled: usize,
    pub retired: Result<u32, SignallingError>,
    pub timers: Result<(), SignallingError>,
    pub expired_transport: bool,
    pub expired_common: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DedicatedProcessingError {
    Signalling(SignallingError),
    Bearer(crate::dedicated_bearers::BearerError),
}

pub struct CommonController<T: Transport> {
    radio: CommonSignalling,
    port: CommonTxPort,
    rach: RachIngress,
    common: ControlIngress,
    core: CoreLink<T>,
    settings: ControllerSettings,
    previous: Option<ControllerTime>,
    last_frame: Option<u64>,
    dedicated: Option<crate::dedicated_controller::DedicatedController>,
    admission: Option<crate::rel99_admission::Rel99Admission>,
    hsdpa: Option<hsdpa::Sessions>,
    hsupa: Option<hsupa::Sessions>,
}

impl<T: Transport> CommonController<T> {
    pub(crate) fn request_stop(&mut self) {
        self.core.gateway_mut().shutdown();
    }
    pub fn epoch(&self) -> umts_types::RadioEpoch {
        self.radio.epoch()
    }

    pub(crate) fn paired_with(
        &self,
        preparation: &crate::radio_io::TxPreparation,
        rx: &crate::common_rx::CommonRx,
    ) -> bool {
        self.port.belongs_to(preparation.source())
            && rx.paired_with(preparation.source(), &self.rach, preparation.air_origin())
            && self
                .dedicated
                .as_ref()
                .is_none_or(|dedicated| dedicated.paired_with(rx))
            && self
                .admission
                .as_ref()
                .is_none_or(|admission| admission.paired_with(preparation))
    }
    pub fn new(
        radio: CommonSignalling,
        port: CommonTxPort,
        rach: RachIngress,
        common: ControlIngress,
        core: CoreLink<T>,
        settings: ControllerSettings,
    ) -> Result<Self, ControllerError> {
        let epoch = port.next_chip().epoch;
        if [
            radio.epoch(),
            rach.epoch(),
            common.epoch(),
            core.gateway().epoch(),
        ]
        .iter()
        .any(|item| *item != epoch)
        {
            return Err(ControllerError::Epoch);
        }
        if settings.maximum_future_frames.get() > crate::common_tx::MAX_PENDING_TRANSPORTS
            || settings.preparation_guard_chips.get()
                > settings.maximum_future_frames.get() as u64 * CHIPS_PER_FRAME
        {
            return Err(ControllerError::Configuration);
        }
        Ok(Self {
            radio,
            port,
            rach,
            common,
            core,
            settings,
            previous: None,
            last_frame: None,
            dedicated: None,
            admission: None,
            hsdpa: None,
            hsupa: None,
        })
    }

    pub fn radio(&self) -> &CommonSignalling {
        &self.radio
    }
    pub fn attach_dedicated(
        &mut self,
        dedicated: crate::dedicated_controller::DedicatedController,
    ) -> Result<
        (),
        (
            ControllerError,
            Box<crate::dedicated_controller::DedicatedController>,
        ),
    > {
        if self.previous.is_some() || self.dedicated.is_some() {
            return Err((ControllerError::Configuration, Box::new(dedicated)));
        }
        if dedicated.epoch() != self.epoch() {
            return Err((ControllerError::Epoch, Box::new(dedicated)));
        }
        self.dedicated = Some(dedicated);
        Ok(())
    }
    pub fn dedicated_mut(
        &mut self,
    ) -> Option<&mut crate::dedicated_controller::DedicatedController> {
        self.dedicated.as_mut()
    }
    pub fn admission(&self) -> Option<&crate::rel99_admission::Rel99Admission> {
        self.admission.as_ref()
    }
    pub fn attach_admission(
        &mut self,
        admission: crate::rel99_admission::Rel99Admission,
    ) -> Result<(), (ControllerError, Box<crate::rel99_admission::Rel99Admission>)> {
        if self.previous.is_some()
            || self.admission.is_some()
            || self
                .dedicated
                .as_ref()
                .is_none_or(|receive| receive.capacity() != admission.capacity())
        {
            return Err((ControllerError::Configuration, Box::new(admission)));
        }
        if admission.epoch() != self.epoch() {
            return Err((ControllerError::Epoch, Box::new(admission)));
        }
        self.admission = Some(admission);
        Ok(())
    }
    #[cfg(test)]
    pub fn begin_rab_dch(
        &mut self,
        pending: crate::rab_handoff::PendingRabAssignment,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut crate::radio_resources::RadioResources,
        settings: crate::rel99_planner::InitialOwnerSettings,
    ) -> Result<
        crate::dedicated_rx::ReceiveIdentity,
        Box<crate::rel99_admission::RejectedRabAssignment>,
    > {
        self.begin_rab_dch_with_settings(pending, policy, resources, |_| Ok(settings))
    }

    #[cfg(test)]
    pub fn begin_rab_dch_with_settings(
        &mut self,
        pending: crate::rab_handoff::PendingRabAssignment,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut crate::radio_resources::RadioResources,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedInitial,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<
        crate::dedicated_rx::ReceiveIdentity,
        Box<crate::rel99_admission::RejectedRabAssignment>,
    > {
        let domain = match pending.request().domain() {
            umts_iuh::Domain::Cs => umts_types::CnDomain::Cs,
            umts_iuh::Domain::Ps => umts_types::CnDomain::Ps,
        };
        if let Err(error) = policy.validate_rel99_packet_setup(domain) {
            return Err(Box::new(crate::rel99_admission::RejectedRabAssignment {
                error: crate::rel99_admission::RabAdmissionError::Hsdpa(error.to_string()),
                pending,
                allocation: None,
                rollback_error: None,
            }));
        }
        let prepared = self.prepare_rab_dch(pending, policy, resources)?;
        self.begin_prepared_rab_dch(prepared, policy, resources, settings)
    }

    pub fn prepare_rab_dch(
        &mut self,
        pending: crate::rab_handoff::PendingRabAssignment,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<
        Box<crate::rel99_admission::PreparedRabAssignment>,
        Box<crate::rel99_admission::RejectedRabAssignment>,
    > {
        let (Some(admission), Some(receive)) = (&mut self.admission, &self.dedicated) else {
            return Err(Box::new(crate::rel99_admission::RejectedRabAssignment {
                error: crate::rel99_admission::RabAdmissionError::Construction,
                pending,
                allocation: None,
                rollback_error: None,
            }));
        };
        crate::rel99_admission::InitialRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        }
        .prepare(pending)
    }

    pub fn begin_prepared_rab_dch(
        &mut self,
        prepared: Box<crate::rel99_admission::PreparedRabAssignment>,
        policy: &crate::rel99_planner::ConfiguredPolicy,
        resources: &mut crate::radio_resources::RadioResources,
        settings: impl FnOnce(
            &crate::rel99_planner::PreparedInitial,
        ) -> Result<
            crate::rel99_planner::InitialOwnerSettings,
            crate::rel99_planner::OwnerBuildError,
        >,
    ) -> Result<
        crate::dedicated_rx::ReceiveIdentity,
        Box<crate::rel99_admission::RejectedRabAssignment>,
    > {
        let (Some(admission), Some(receive)) = (&mut self.admission, &self.dedicated) else {
            return Err(prepared.reject(
                resources,
                crate::rel99_admission::RabAdmissionError::Construction,
            ));
        };
        crate::rel99_admission::InitialRabHost {
            gateway: self.core.gateway(),
            policy,
            radio: &mut self.radio,
            resources,
            receive,
            admission,
        }
        .begin_prepared(prepared, settings)
    }
    pub fn finish_circuit_failure_dch(
        &mut self,
        retired: &mut crate::rel99_admission::RetiredAdmission,
        voices: &mut crate::voice_bearers::VoiceTraffic,
        now: ControlTime,
        cause: umts_iuh::ranap::rab::Cause,
    ) -> Result<crate::rel99_admission::CircuitProgress, crate::rel99_admission::InitialCircuitError>
    {
        retired.finish_circuit_failure(&mut self.core, voices, now, cause)
    }
    pub fn take_retired_dch(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> Result<crate::rel99_admission::RetiredAdmission, crate::rel99_admission::AdmissionError>
    {
        self.admission
            .as_mut()
            .ok_or(crate::rel99_admission::AdmissionError::Configuration)?
            .take_retired(identity)
    }
    pub fn finish_packet_failure_dch<D: umts_user_plane::packet::BoundDatagrams>(
        &mut self,
        retired: &mut crate::rel99_admission::RetiredAdmission,
        packets: &mut crate::packet_rabs::PacketRabs<D>,
        now: ControlTime,
        cause: umts_iuh::ranap::rab::Cause,
    ) -> Result<crate::rel99_admission::PacketProgress, crate::rel99_admission::InitialPacketError>
    {
        retired.finish_packet_failure(&mut self.core, packets, now, cause)
    }
    pub fn radio_mut(&mut self) -> &mut CommonSignalling {
        &mut self.radio
    }
    pub fn dedicated_launch_cursor(&self) -> (ChipInstant, NonZeroU64) {
        (self.port.next_chip(), self.settings.preparation_guard_chips)
    }
    pub fn core(&self) -> &CoreLink<T> {
        &self.core
    }

    /// TS 25.413 8.4.2: release the PS connection when its GTP-U peer lost a tunnel.
    pub(crate) fn request_gtp_peer_failure_release(
        &mut self,
        now: ControlTime,
        route: umts_user_plane::packet::RouteKey,
    ) -> Result<bool, umts_core::Error> {
        if route.connection.domain != umts_types::CnDomain::Ps {
            return Err(umts_core::Error::State);
        }
        let key = route.connection.ue;
        let context = self
            .core
            .gateway()
            .context(key)
            .ok_or(umts_core::Error::State)?;
        self.core
            .gateway_mut()
            .request_ps_transport_release(now, key, context)
    }

    fn frame(&self, now: ChipInstant) -> Result<Option<ChipInstant>, ControllerError> {
        let cursor = self.port.next_chip();
        let first = cursor
            .index
            .max(now.index)
            .checked_add(self.settings.preparation_guard_chips.get())
            .ok_or(ControllerError::Time)?;
        let mut frame = first
            .div_ceil(CHIPS_PER_FRAME)
            .checked_mul(CHIPS_PER_FRAME)
            .ok_or(ControllerError::Time)?;
        if let Some(last) = self.last_frame {
            frame = frame.max(
                last.checked_add(CHIPS_PER_FRAME)
                    .ok_or(ControllerError::Time)?,
            );
        }
        let last = cursor
            .index
            .checked_add(self.settings.maximum_future_frames.get() as u64 * CHIPS_PER_FRAME)
            .ok_or(ControllerError::Time)?;
        Ok((frame <= last).then_some(ChipInstant {
            epoch: cursor.epoch,
            index: frame,
        }))
    }

    pub fn step_with_duplex_policy<U, P: crate::rel99_admission::DuplexFramePolicy>(
        &mut self,
        now: ControllerTime,
        other_core: impl FnMut(&mut CommonSignalling, &Event) -> Result<(), U>,
        mut control: impl FnMut(
            crate::signalling::ReceivedControl<'_>,
            crate::signalling::ControlSecurity<'_>,
        ) -> Result<(), SignallingError>,
        policy: &mut P,
    ) -> Result<ControllerStep<T::Error, U, P::UplinkError>, ControllerError> {
        if !self.port.available() {
            return Err(ControllerError::Closed);
        }
        if now.radio.epoch != self.port.next_chip().epoch {
            return Err(ControllerError::Epoch);
        }
        if self.previous.is_some_and(|previous| {
            now.radio.index < previous.radio.index
                || now.security.control < previous.security.control
                || now.security.monotonic < previous.security.monotonic
        }) {
            return Err(ControllerError::Time);
        }
        let frame = self.frame(now.radio)?;
        self.previous = Some(now);
        let first = ChipInstant {
            epoch: now.radio.epoch,
            index: now
                .radio
                .index
                .saturating_sub(self.settings.maximum_ingress_age_chips.get()),
        };
        let expired_transport = self
            .rach
            .expire_one_before(first)
            .map_err(ControllerError::Ingress)?;
        let expired_common = self
            .common
            .expire_one_before(first)
            .map_err(ControllerError::Control)?;
        let radio = &mut self.radio;
        let common = &mut self.common;
        let rach = self
            .rach
            .process_pdu_one_with(|item| {
                let outcome = route(radio, common, now.radio, item.transport, item.pdu);
                let step = RachStep {
                    transport: item.transport,
                    block: item.index,
                    work: outcome,
                };
                if step.work == RachWork::Deferred {
                    ControlDecision::Retry(step)
                } else {
                    ControlDecision::Consume(step)
                }
            })
            .map_err(ControllerError::Ingress)?
            .map(|decision| match decision {
                ControlDecision::Consume(step) | ControlDecision::Retry(step) => step,
            });
        let common = self.radio.service_common_one(
            &self.port,
            &mut self.common,
            TransactionId::new(0).expect("valid RRC transaction"),
        );
        let collected = self.radio.collect_ingress_one(now.radio);
        let admission = &mut self.admission;
        let receive = &mut self.dedicated;
        let hsdpa = &mut self.hsdpa;
        let hsupa = &mut self.hsupa;
        let dedicated = self.radio.process_one_at_with_control(
            now.radio,
            now.security,
            |received, mut security| {
                if let Some(sessions) = hsupa
                    && let Some(result) = sessions.response(&received, &mut security)
                {
                    result
                } else if let Some(sessions) = hsdpa
                    && let Some(result) = sessions.response(&received, &mut security)
                {
                    result
                } else if let Some(admission) = admission
                    && let Some(receive) = receive.as_mut()
                    && let Some(result) = admission.response(&received, &mut security, receive)
                {
                    result
                } else {
                    control(received, security)
                }
            },
        );
        let core = self
            .core
            .drive_at(now.security, &mut self.radio, other_core);
        let security = core.as_ref().ok().map(|step| {
            self.radio
                .actuate_security(step.security, now.radio, now.security)
        });
        if let Some(Ok(SecurityAction::RecoveryRequired {
            key,
            origin: Some(_),
        })) = &security
            && let Err(error) = self.radio.release_unanswered_security(
                *key,
                TransactionId::new(0).expect("valid RRC transaction"),
            )
            && !crate::signalling::scheduling::backpressure(&error)
        {
            tracing::warn!("Unanswered security release failed: key={key:?} error={error:?}");
        }
        for domain in self.radio.take_reset_acknowledges().into_iter().flatten() {
            if let Err(error) = self
                .core
                .gateway_mut()
                .acknowledge_reset(now.security.control, domain)
            {
                crate::signalling::log_radio_link_failure_text(&format!(
                    "RESET ACKNOWLEDGE for {domain:?} could not be sent: {error:?}"
                ));
                self.radio.request_reset_acknowledge(domain);
            }
        }
        // TS 25.214 4.3.3.2: a radio link set whose T_RLFAILURE expired is reported as lost.
        let mut lost: [Option<crate::dedicated_rx::ReceiveIdentity>; 4] = [None; 4];
        if let Some(dedicated) = self.dedicated.as_mut() {
            dedicated.service_receive_diagnostics(|identity, status| {
                if status.tracking.radio_link.state
                    == crate::dedicated_rx::RadioLinkSyncState::OutOfSync
                    && let Some(slot) = lost.iter_mut().find(|slot| slot.is_none())
                {
                    *slot = Some(identity);
                }
            });
        }
        for identity in lost.into_iter().flatten() {
            if let Some(key) = self.radio.key_for_route(identity) {
                crate::signalling::log_radio_link_failure(key, identity);
                let _ = self.radio.release_lost(key);
            }
        }
        let retired = self.radio.expire_some(now.radio, NonZeroU32::MIN);
        let timers = self.radio.tick_one(now.radio);
        let mut uplink = uplink::UplinkOutcomes::default();
        let dedicated_bearers = self.dedicated.as_mut().map(|dedicated| {
            dedicated.retire_orphans_some(self.settings.retirement_scan_budget, |key| {
                self.admission
                    .as_ref()
                    .is_some_and(|owner| owner.has_ue(key))
                    || self
                        .radio
                        .phase(key)
                        .is_some_and(|phase| phase != EstablishmentPhase::Closed)
            });
            dedicated.step(
                now.radio,
                |key, bank| {
                    // Admission retains the bank until RX/TX retirement completes after RRC closes.
                    if self
                        .admission
                        .as_ref()
                        .is_some_and(|owner| owner.has_ue(key))
                        && self
                            .radio
                            .phase(key)
                            .is_none_or(|phase| phase == EstablishmentPhase::Closed)
                    {
                        return Ok(crate::dedicated_bearers::BearerProgress::Idle);
                    }
                    self.radio
                        .with_bearer_access(key, |ciphers, reset, receiver| {
                            bank.service_routed(now.radio, ciphers, reset, |pdu, ciphers| {
                                if pdu.identity.ue != key {
                                    return Err(SignallingError::Origin);
                                }
                                match receiver.receive(ciphers, pdu.bearer, pdu.time, pdu.payload) {
                                    Ok(()) => Ok(true),
                                    Err(SignallingError::Capacity) => Ok(false),
                                    Err(error) => Err(error),
                                }
                            })
                        })
                        .map_err(DedicatedProcessingError::Signalling)?
                        .map_err(|error| match error {
                            crate::dedicated_bearers::ServiceError::Bearer(error) => {
                                DedicatedProcessingError::Bearer(error)
                            }
                            crate::dedicated_bearers::ServiceError::Signalling(error) => {
                                DedicatedProcessingError::Signalling(error)
                            }
                        })
                },
                |delivery| {
                    // RX can switch before the accepted RRC response commits the media routes.
                    if matches!(
                        delivery.metadata.scope,
                        umts_security::ciphering::Scope::User(_)
                    ) && self.admission.as_ref().is_some_and(|admission| {
                        admission.uplink_handoff_pending(delivery.metadata.identity)
                    }) {
                        return false;
                    }
                    uplink.deliver(policy, delivery)
                },
            )
        });
        let radio = &self.radio;
        let cancelled =
            self.port
                .cancel_orphaned_some(self.settings.retirement_scan_budget, |key| {
                    radio
                        .phase(key)
                        .is_some_and(|phase| phase != EstablishmentPhase::Closed)
                });
        let transmit = self.port.next_event();
        let admission = match (&mut self.admission, &mut self.dedicated) {
            (Some(admission), Some(receive)) => Some(admission.step_with_core(
                now.radio,
                &mut self.radio,
                receive,
                &mut self.port,
                policy,
                self.core.gateway(),
            )),
            _ => None,
        };
        let hsupa = if self.hsupa.is_some() {
            let cursor = self.port.next_chip();
            // E-HICH has a fixed air deadline, unlike advance-planned common signaling.
            // The TX owner rejects a command if preparation passes it before admission.
            let next_transmit = ChipInstant {
                epoch: cursor.epoch,
                index: cursor.index.max(now.radio.index),
            };
            Some(self.service_hsupa_sessions(now.radio, next_transmit))
        } else {
            None
        };
        let mut hsdpa = match (&mut self.hsdpa, &mut self.dedicated, &mut self.admission) {
            (Some(sessions), Some(receive), Some(admission)) => {
                sessions.service(now.radio, &mut self.radio, receive, admission)
            }
            _ => HsdpaControlStep::default(),
        };
        if let (Some(sessions), Some(admission)) = (&mut self.hsdpa, &mut self.admission) {
            hsdpa.transmit = sessions.service_transmit(admission, now.radio);
        }
        self.radio.service_paging(
            &mut self.port,
            now.radio,
            self.settings.preparation_guard_chips.get(),
        );
        let fach = match frame {
            Some(start) => self.radio.service_fach_some(
                &mut self.port,
                now.radio,
                start,
                self.settings.fach_scan_budget,
            ),
            None => Ok(FachWork::Idle),
        };
        if matches!(fach, Ok(FachWork::Published(_))) {
            self.last_frame = frame.map(|frame| frame.index);
        }
        Ok(ControllerStep {
            hsdpa,
            hsupa,
            uplink_failure: uplink.first,
            uplink_failures: uplink.failures,
            admission,
            dedicated_bearers,
            rach,
            common,
            collected,
            dedicated,
            core,
            security,
            fach,
            frame,
            transmit,
            cancelled,
            retired,
            timers,
            expired_transport,
            expired_common,
        })
    }
}

fn route(
    radio: &mut CommonSignalling,
    common: &mut ControlIngress,
    now: ChipInstant,
    transport: DecodedPrachMessage,
    pdu: Result<umts_mac::CommonPdu<'_>, MacError>,
) -> RachWork {
    let pdu = match pdu {
        Ok(pdu) => pdu,
        Err(error) => return RachWork::Rejected(RachRejection::Mac(error)),
    };
    if transport.start.epoch != now.epoch || transport.start.index > now.index {
        return RachWork::Rejected(RachRejection::Time);
    }
    match pdu.header {
        CommonHeader::CommonControl => match common.enqueue_pdu(transport, pdu) {
            Ok(()) => RachWork::CommonQueued,
            Err(ControlError::Capacity) => RachWork::Deferred,
            Err(error) => RachWork::Rejected(RachRejection::Control(error)),
        },
        CommonHeader::Dedicated {
            identity: CommonIdentity::Cell(identity),
            ..
        } => {
            let Some(key) = radio.key_for_cell_id(identity) else {
                return RachWork::Rejected(RachRejection::UnknownIdentity);
            };
            let time = umts_rlc::acknowledged::ReceiveTime::new(transport.start, now)
                .expect("validated receive time");
            match radio.receive_timed(key, time, pdu) {
                Ok(()) => RachWork::DedicatedAccepted(key),
                Err(error) if crate::signalling::scheduling::backpressure(&error) => {
                    RachWork::Deferred
                }
                Err(error) => RachWork::Rejected(RachRejection::Signalling(error)),
            }
        }
        _ => RachWork::Rejected(RachRejection::Mapping),
    }
}

impl ControllerTime {
    pub fn new(radio: ChipInstant, control: ControlTime, monotonic: Instant) -> Self {
        Self {
            radio,
            security: SecurityTime { control, monotonic },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
