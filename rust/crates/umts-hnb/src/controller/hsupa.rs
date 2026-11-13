// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CommonController, ControllerError};
use crate::{
    dedicated_tx::{TransmitCommandId, TransmitCommandOutcome, TransmitEvent},
    hsupa_session::{Phase, Session},
    hsupa_tx::{Command, CommandOutcome, Control},
    signalling::{ControlSecurity, ReceivedControl, SignallingError},
};
use std::num::NonZeroUsize;
use umts_core::Transport;
use umts_rrc_schema::class_definitions::ULDCCHMessageType;
use umts_types::time::ChipInstant;

const PACKET_PDUS_PER_VISIT: usize = 64;
const RECEIVE_REPORTS_PER_VISIT: usize = 4;
mod restoration;
mod visits;

pub(super) struct Sessions {
    entries: Box<[Option<Box<Session>>]>,
    budget: NonZeroUsize,
    cursor: usize,
    admission_cursor: usize,
    cleanup_cursor: usize,
    rejected_admissions: Box<[Option<crate::dedicated_rx::ReceiveIdentity>]>,
    completion: Option<TransmitCommandId>,
    unclaimed: Option<TransmitEvent>,
}

#[derive(Default, Debug)]
pub struct HsupaControlStep {
    pub visited: usize,
    pub replies: usize,
    pub packet_pdus: usize,
    pub packet_backpressure: usize,
    pub failures: usize,
    pub first_failure: Option<String>,
}

#[derive(Debug)]
pub enum HsupaCleanupError {
    Identity,
    LiveContext,
    Receiver,
    RecoveryRequired,
    Radio(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        SignallingError,
    ),
    Resources(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::radio_resources::Error,
    ),
    Cancellation(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_rrc::dedicated::DedicatedError,
    ),
}

pub struct PreparedHsupaAdmission {
    pub resources: crate::hsupa_session::ReceiveResources,
    pub lease: crate::radio_resources::HsupaUserLease,
    pub launch: crate::rel99_admission::AdditionalLaunch,
    pub response_guard: std::num::NonZeroU64,
    pub grant_value: u8,
    pub grant_ceiling: u8,
    pub grant_amplitude: f32,
    pub grant_refresh: Option<std::num::NonZeroU64>,
}

pub struct RejectedPreparedHsupa {
    pub error: anyhow::Error,
    pub request: PreparedHsupaAdmission,
}

pub struct HsupaCandidate {
    pub identity: crate::dedicated_rx::ReceiveIdentity,
    pub dedicated: std::sync::Arc<umts_rrc::dedicated::Rel99Configuration>,
    pub downlink: Option<crate::dedicated_bearers::CommittedHsdpa>,
    pub category: umts_types::hsupa::Rel6Category,
    pub source: crate::dedicated_rx::ReceiveCommandTicket,
    pub lease: crate::radio_resources::HsupaUserLease,
}

impl Sessions {
    pub(super) fn response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
    ) -> Option<Result<(), SignallingError>> {
        let transaction = match &received.message().value.message {
            ULDCCHMessageType::radioBearerReconfigurationComplete(value) => {
                value.rrc_transaction_identifier.0
            }
            ULDCCHMessageType::radioBearerReconfigurationFailure(value) => {
                value.rrc_transaction_identifier.0
            }
            _ => return None,
        };
        let session = self
            .entries
            .get_mut(received.key().slot as usize)?
            .as_deref_mut()
            .filter(|session| {
                session.identity().ue == received.key()
                    && session.can_route_response(transaction, security)
            })?;
        Some(
            session
                .response(received, security, |counters, _| {
                    if counters.transparent_cfn().is_some() {
                        return Err(SignallingError::Mapping);
                    }
                    Ok(())
                })
                .map(|_| ()),
        )
    }

    fn route(&mut self, event: TransmitEvent) -> Option<TransmitEvent> {
        if self.completion == Some(event.id) {
            match &event.outcome {
                TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(Ok(result))) => {
                    if let Some(completion) = result {
                        let accepted = self
                            .entries
                            .get_mut(completion.identity.ue.slot as usize)
                            .and_then(Option::as_deref_mut)
                            .is_some_and(|session| session.accept_completion(*completion).is_ok());
                        if !accepted {
                            return Some(event);
                        }
                    }
                    self.completion = None;
                    return None;
                }
                _ => return Some(event),
            }
        }
        let mut retained = event;
        for session in self.entries.iter_mut().flatten() {
            retained = session.route_command(retained)?;
        }
        Some(retained)
    }
}

impl<T: Transport> CommonController<T> {
    pub fn visit_hsupa_candidates(
        &self,
        mut visit: impl FnMut(
            crate::dedicated_rx::ReceiveIdentity,
            Option<umts_rrc::capabilities::hsupa::FddHsupa>,
            bool,
            bool,
            bool,
            Option<Phase>,
            Option<crate::hsupa_rx::IngressCounters>,
            Option<crate::hsupa_session::GrantCounters>,
        ),
    ) {
        let (Some(admission), Some(sessions)) = (&self.admission, &self.hsupa) else {
            return;
        };
        for index in 0..sessions.entries.len() {
            let Some(identity) = admission.committed_identity_at(index) else {
                continue;
            };
            visit(
                identity,
                self.radio
                    .capabilities(identity.ue)
                    .map(|report| report.hsupa()),
                self.hsdpa
                    .as_ref()
                    .is_none_or(|downlink| downlink.committed_configuration(identity).is_some()),
                sessions.rejected_admissions[index] == Some(identity),
                admission.active_radio(identity).is_ok(),
                sessions.entries[index]
                    .as_ref()
                    .filter(|session| session.identity() == identity)
                    .map(|session| session.phase()),
                sessions.entries[index]
                    .as_ref()
                    .filter(|session| session.identity() == identity)
                    .and_then(|session| session.ingress_counters()),
                sessions.entries[index]
                    .as_ref()
                    .filter(|session| session.identity() == identity)
                    .and_then(|session| session.grant_counters()),
            );
        }
    }

    pub(crate) fn reject_hsupa_candidate(
        &mut self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) {
        if let Some(slot) = self.hsupa.as_mut().and_then(|sessions| {
            sessions
                .rejected_admissions
                .get_mut(identity.ue.slot as usize)
        }) {
            *slot = Some(identity);
        }
    }
    pub fn cleanup_cancelled_hsupa(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
    ) -> Result<usize, HsupaCleanupError> {
        let Some(sessions) = self.hsupa.as_mut() else {
            return Ok(0);
        };
        let mut released = 0;
        for _ in 0..sessions.budget.get().min(sessions.entries.len()) {
            let index = sessions.cleanup_cursor;
            sessions.cleanup_cursor = (index + 1) % sessions.entries.len();
            let Some(session) = sessions.entries[index].as_deref_mut() else {
                continue;
            };
            if session.phase() == Phase::Cancelled
                && session
                    .release_cancelled_resources(resources)
                    .map_err(HsupaCleanupError::Resources)?
            {
                sessions.entries[index] = None;
                released += 1;
            }
        }
        Ok(released)
    }

    pub(crate) fn hsupa_candidate_current(
        &self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> bool {
        self.admission
            .as_ref()
            .is_some_and(|admission| admission.committed_radio(identity).is_ok())
    }

    pub(crate) fn hsupa_candidate_ready(
        &self,
        identity: crate::dedicated_rx::ReceiveIdentity,
    ) -> bool {
        self.admission
            .as_ref()
            .is_some_and(|admission| admission.active_radio(identity).is_ok())
    }
    pub fn next_hsupa_candidate(
        &mut self,
        resources: &mut crate::radio_resources::RadioResources,
        pool: crate::radio_resources::HsupaPoolIdentity,
    ) -> anyhow::Result<Option<HsupaCandidate>> {
        let sessions = self
            .hsupa
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HSUPA registry is unavailable"))?;
        let admission = self
            .admission
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("HSUPA radio admission is unavailable"))?;
        let receive = self
            .dedicated
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HSUPA dedicated receiver is unavailable"))?;
        for _ in 0..sessions.budget.get().min(sessions.entries.len()) {
            let index = sessions.admission_cursor;
            sessions.admission_cursor = (index + 1) % sessions.entries.len();
            if sessions.entries[index].is_some() {
                continue;
            }
            let Some(identity) = admission.committed_identity_at(index) else {
                continue;
            };
            if sessions.rejected_admissions[index] == Some(identity) {
                continue;
            }
            sessions.rejected_admissions[index] = None;
            let Some(umts_rrc::capabilities::hsupa::FddHsupa::Supported(category)) = self
                .radio
                .capabilities(identity.ue)
                .map(|capability| capability.hsupa())
            else {
                continue;
            };
            let active = match admission.committed_radio(identity) {
                Ok(active) => active,
                Err(_) => continue,
            };
            let dedicated = active.plan().configuration();
            if !dedicated
                .bearers()
                .iter()
                .any(|bearer| bearer.domain == Some(umts_types::CnDomain::Ps))
            {
                continue;
            }
            let downlink = match &self.hsdpa {
                Some(sessions) => match sessions.committed_configuration(identity) {
                    Some(configuration) => Some(
                        receive
                            .with_active_mapping(identity, |bank| {
                                bank.committed_hsdpa(identity, &configuration)
                            })
                            .map_err(|error| {
                                anyhow::anyhow!("HSUPA committed HSDPA receiver: {error:?}")
                            })?
                            .map_err(|error| {
                                anyhow::anyhow!("HSUPA committed HSDPA mapping: {error:?}")
                            })?,
                    ),
                    None => continue,
                },
                None => None,
            };
            let source = match receive.hsupa_source(identity) {
                Ok(source) => source,
                Err(crate::dedicated_controller::DedicatedControlError::Capacity) => {
                    return Ok(None);
                }
                Err(error) => anyhow::bail!("HSUPA receive source: {error:?}"),
            };
            let lease = resources
                .reserve_hsupa_user_in_pool(pool, active.lease(), None)
                .map_err(|error| anyhow::anyhow!("HSUPA UE control reservation: {error:?}"))?;
            return Ok(Some(HsupaCandidate {
                identity,
                dedicated: dedicated.clone(),
                downlink,
                category,
                source,
                lease,
            }));
        }
        Ok(None)
    }

    pub fn admit_prepared_hsupa(
        &mut self,
        mut request: PreparedHsupaAdmission,
    ) -> Result<(), Box<RejectedPreparedHsupa>> {
        let identity = request.resources.identity();
        let prepare = (|| -> anyhow::Result<_> {
            let slot = self
                .hsupa
                .as_ref()
                .and_then(|sessions| sessions.entries.get(identity.ue.slot as usize))
                .ok_or_else(|| anyhow::anyhow!("HSUPA session registry is unavailable"))?;
            anyhow::ensure!(
                slot.is_none(),
                "HSUPA session already occupies this UE slot"
            );
            let admission = self
                .admission
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("HSUPA radio admission is unavailable"))?;
            let receive = self
                .dedicated
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("HSUPA dedicated receiver is unavailable"))?;
            admission.reschedule_prepared_hsupa_activation(
                &request.resources,
                request.launch,
                request.response_guard,
                &self.radio,
                receive,
            )
        })();
        let procedure = match prepare {
            Ok(procedure) => procedure,
            Err(error) => return Err(Box::new(RejectedPreparedHsupa { error, request })),
        };
        if let Err(error) = request
            .resources
            .reschedule(procedure.construction().clone())
        {
            return Err(Box::new(RejectedPreparedHsupa { error, request }));
        }
        let PreparedHsupaAdmission {
            resources,
            lease,
            launch,
            response_guard,
            grant_value,
            grant_ceiling,
            grant_amplitude,
            grant_refresh,
        } = request;
        match resources.into_session(
            lease,
            procedure,
            grant_value,
            grant_ceiling,
            grant_amplitude,
            grant_refresh,
        ) {
            Ok(session) => {
                self.hsupa
                    .as_mut()
                    .expect("validated HSUPA registry")
                    .entries[identity.ue.slot as usize] = Some(session);
                Ok(())
            }
            Err(rejected) => {
                let crate::hsupa_session::RejectedReceiveSession {
                    error,
                    resources,
                    lease,
                    procedure: _,
                } = *rejected;
                Err(Box::new(RejectedPreparedHsupa {
                    error: anyhow::anyhow!("HSUPA prepared session assembly: {error:?}"),
                    request: PreparedHsupaAdmission {
                        resources,
                        lease,
                        launch,
                        response_guard,
                        grant_value,
                        grant_ceiling,
                        grant_amplitude,
                        grant_refresh,
                    },
                }))
            }
        }
    }

    pub fn configure_hsupa_sessions(
        &mut self,
        budget: NonZeroUsize,
        commands: NonZeroUsize,
    ) -> Result<(), ControllerError> {
        if self.hsupa.is_some() {
            return Err(ControllerError::Configuration);
        }
        let admission = self
            .admission
            .as_mut()
            .ok_or(ControllerError::Configuration)?;
        admission
            .configure_hsupa_control(commands)
            .map_err(|_| ControllerError::Configuration)?;
        self.hsupa = Some(Sessions {
            entries: (0..admission.capacity()).map(|_| None).collect(),
            budget,
            cursor: 0,
            admission_cursor: 0,
            cleanup_cursor: 0,
            rejected_admissions: (0..admission.capacity()).map(|_| None).collect(),
            completion: None,
            unclaimed: None,
        });
        Ok(())
    }

    pub fn service_hsupa_sessions(
        &mut self,
        now: ChipInstant,
        next_transmit: ChipInstant,
    ) -> anyhow::Result<HsupaControlStep> {
        anyhow::ensure!(
            now.epoch == self.epoch() && next_transmit.epoch == now.epoch,
            "HSUPA service uses another radio epoch"
        );
        let sessions = self
            .hsupa
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HSUPA sessions are not configured"))?;
        anyhow::ensure!(
            sessions.unclaimed.is_none(),
            "HSUPA retains an unclaimed TX result"
        );
        let admission = self
            .admission
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HSUPA requires radio admission"))?;
        let receive = self
            .dedicated
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("HSUPA requires dedicated RX"))?;
        let mut control = admission
            .hsupa_control()
            .ok_or_else(|| anyhow::anyhow!("HSUPA command routing is absent"))?;
        let mut report = HsupaControlStep::default();
        for _ in 0..sessions.budget.get() {
            let Some(event) = control.take_result() else {
                break;
            };
            report.replies += 1;
            if let Some(event) = sessions.route(event) {
                let completion = match &event.outcome {
                    TransmitCommandOutcome::Hsupa(CommandOutcome::Completion(result)) => {
                        Some(result)
                    }
                    _ => None,
                };
                let error = anyhow::anyhow!(
                    "HSUPA TX result has no matching owner: command={:?} completion={completion:?}",
                    event.id
                );
                sessions.unclaimed = Some(event);
                return Err(error);
            }
        }
        for session in
            visits::occupied(&mut sessions.entries, &mut sessions.cursor, sessions.budget)
        {
            report.visited += 1;
            let result: anyhow::Result<()> = (|| {
                if matches!(
                    session.phase(),
                    Phase::Retiring | Phase::Retired | Phase::Restored
                ) {
                    session.service_ue_release(now, &mut control);
                    return Ok(());
                }
                if session.phase() == Phase::Restoring {
                    receive.service_hsupa_restoration(session, &mut self.radio, now)?;
                    if session.phase() == Phase::Retiring {
                        return Ok(());
                    }
                }
                session
                    .expire(&mut self.radio, now)
                    .map_err(|error| anyhow::anyhow!("HSUPA expiry: {error:?}"))?;
                receive
                    .service_hsupa_resources(session, &mut control, now)
                    .map_err(|error| anyhow::anyhow!("HSUPA resource service: {error:?}"))?;
                match session.phase() {
                    Phase::Prepared => session
                        .publish(&mut self.radio, now)
                        .map_err(|error| anyhow::anyhow!("HSUPA publication: {error:?}"))?,
                    Phase::Accepted => {
                        session
                            .commit(&mut self.radio, |_| Ok::<_, SignallingError>(()))
                            .map_err(|error| anyhow::anyhow!("HSUPA commit: {error:?}"))?;
                    }
                    Phase::AwaitingCommandAck => match session.finish_command(&mut self.radio) {
                        Ok(()) | Err(SignallingError::Capacity) => (),
                        Err(error) => anyhow::bail!("HSUPA receipt: {error:?}"),
                    },
                    Phase::RecoveryRequired => {
                        let procedure = session.procedure().phase();
                        let preparation = session.preparation();
                        let receive = preparation.rejected_receiver().map(|(_, error)| error);
                        let transmit = preparation.rejected_transmitter().map(|(_, error)| error);
                        anyhow::bail!(
                            "HSUPA session requires recovery: procedure={procedure:?} preparation={:?} receive={receive:?} transmit={transmit:?}",
                            preparation.phase()
                        )
                    }
                    _ => (),
                }
                if session.uplink_service_ready(now) {
                    session
                        .service_grants(now, next_transmit, &mut control)
                        .map_err(|error| anyhow::anyhow!("HSUPA grant service: {error:?}"))?;
                    for _ in 0..RECEIVE_REPORTS_PER_VISIT {
                        let ingress = session
                            .service_ingress(now, next_transmit, &mut control)
                            .map_err(|error| anyhow::anyhow!("HSUPA ingress: {error:?}"))?;
                        if matches!(
                            ingress.progress,
                            crate::hsupa_rx::IngressProgress::Idle
                                | crate::hsupa_rx::IngressProgress::Backpressure
                        ) {
                            break;
                        }
                    }
                    let identity = session.identity();
                    self.radio
                        .with_bearer_access(identity.ue, |ciphers, reset, receiver| {
                            receive.with_active_bank(identity, |bank| {
                                for _ in 0..PACKET_PDUS_PER_VISIT {
                                    match session.service_packets_routed(
                                        bank,
                                        now,
                                        ciphers,
                                        reset,
                                        |pdu, ciphers| {
                                            if pdu.identity.ue != identity.ue {
                                                return Err(SignallingError::Origin);
                                            }
                                            match receiver.receive(
                                                ciphers,
                                                pdu.bearer,
                                                pdu.time,
                                                pdu.payload,
                                            ) {
                                                Ok(()) => Ok(true),
                                                Err(SignallingError::Capacity) => Ok(false),
                                                Err(error) => Err(error),
                                            }
                                        },
                                    )? {
                                        crate::dedicated_bearers::BearerProgress::Pdu
                                        | crate::dedicated_bearers::BearerProgress::Dropped => {
                                            report.packet_pdus += 1;
                                        }
                                        crate::dedicated_bearers::BearerProgress::Backpressure => {
                                            report.packet_backpressure += 1;
                                            break;
                                        }
                                        _ => break,
                                    }
                                }
                                Ok::<_, crate::dedicated_bearers::ServiceError<SignallingError>>(())
                            })
                        })
                        .map_err(|error| anyhow::anyhow!("HSUPA bearer access: {error:?}"))?
                        .map_err(|error| anyhow::anyhow!("HSUPA receive bank: {error:?}"))?
                        .map_err(|error| anyhow::anyhow!("HSUPA packet delivery: {error:?}"))?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                report.failures += 1;
                if report.first_failure.is_none() {
                    report.first_failure = Some(format!("{error:#}"));
                }
            }
        }
        // A restoring session still owns feedback flights whose completions must be consumed
        // before its E-DCH ingress can drain, so it keeps the completion poll alive too.
        if sessions.completion.is_none()
            && sessions.entries.iter().flatten().any(|session| {
                matches!(
                    session.phase(),
                    Phase::AwaitingComplete
                        | Phase::Accepted
                        | Phase::AwaitingCommandAck
                        | Phase::Active
                        | Phase::Retiring
                        | Phase::Restoring
                )
            })
        {
            sessions.completion = control.queue_hsupa(Command::TakeCompletion).ok();
        }
        Ok(report)
    }

    pub(crate) fn cleanup_retired_hsupa(
        &mut self,
        owner: &mut crate::rel99_admission::RetiredAdmission,
        resources: &mut crate::radio_resources::RadioResources,
        now: ChipInstant,
    ) -> Result<bool, HsupaCleanupError> {
        let identity = owner.identity;
        let attached = owner
            .receive
            .as_ref()
            .and_then(|received| received.receiver.as_ref())
            .and_then(|receiver| receiver.hsupa_attachment());
        let session = self
            .hsupa
            .as_mut()
            .and_then(|sessions| sessions.entries.get_mut(identity.ue.slot as usize))
            .and_then(Option::as_deref_mut);
        let Some(session) = session else {
            return if attached.is_some() {
                Err(HsupaCleanupError::Receiver)
            } else {
                Ok(true)
            };
        };
        if session.identity() != identity {
            return Err(HsupaCleanupError::Identity);
        }
        if self
            .radio
            .phase(identity.ue)
            .is_some_and(|phase| phase != umts_rrc::establishment::EstablishmentPhase::Closed)
        {
            // A promoted bearer keeps its completed initial procedure, not setup authority.
            if owner.procedure.receipt().is_none() {
                return Err(HsupaCleanupError::LiveContext);
            }
            self.radio
                .retire_failed_radio(identity)
                .map_err(HsupaCleanupError::Radio)?;
        }
        match session.phase() {
            Phase::Cancelled => {
                if !session
                    .release_cancelled_resources(resources)
                    .map_err(HsupaCleanupError::Resources)?
                {
                    return Ok(false);
                }
                self.hsupa
                    .as_mut()
                    .expect("retained session registry")
                    .entries[identity.ue.slot as usize] = None;
                Ok(true)
            }
            Phase::Cancelling if attached.is_none() => Ok(false),
            Phase::Retired | Phase::Restored => {
                if !session
                    .release_retired_resources(resources)
                    .map_err(HsupaCleanupError::Resources)?
                {
                    return Ok(false);
                }
                self.hsupa
                    .as_mut()
                    .expect("retained session registry")
                    .entries[identity.ue.slot as usize] = None;
                Ok(true)
            }
            Phase::Retiring => Ok(false),
            _ if session.retirement_failure().is_some() => Err(HsupaCleanupError::RecoveryRequired),
            _ => {
                if attached.is_none() && session.procedure().receipt().is_none() {
                    session
                        .abort_unpublished(now)
                        .map_err(HsupaCleanupError::Cancellation)?;
                    return Ok(false);
                }
                let Some(receiver) = owner
                    .receive
                    .as_mut()
                    .and_then(|received| received.receiver.take())
                else {
                    // The receiver was retired elsewhere (a failed release or a closed radio
                    // context). Retrying every tick cannot bring it back, so retire the session
                    // without it instead of stalling the slot.
                    if attached.is_none() && session.retire_orphaned() {
                        tracing::warn!(
                            "HSUPA session retired without its receiver: identity={identity:?}"
                        );
                    }
                    return Ok(false);
                };
                if let Err(receiver) = session.begin_ue_release(receiver) {
                    owner
                        .receive
                        .as_mut()
                        .expect("retired receive owner")
                        .receiver = Some(receiver);
                    return Err(HsupaCleanupError::Receiver);
                }
                Ok(false)
            }
        }
    }
}
