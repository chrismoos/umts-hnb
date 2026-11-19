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
        let edch = sessions
            .entries
            .iter()
            .flatten()
            .filter(|session| {
                matches!(
                    session.phase(),
                    Phase::Accepted | Phase::AwaitingCommandAck | Phase::Active | Phase::Cancelling
                )
            })
            .filter(|session| !session.carries_voice())
            .map(|session| session.identity().ue.slot)
            .filter(|slot| *slot < 64)
            .fold(0u64, |slots, slot| slots | (1 << slot));
        crate::dedicated_rx::set_edch_slots(edch);
        // A handset no longer on HSUPA must not be sent a process allocation left from before.
        crate::signalling::retain_process_allocations(|key| {
            sessions
                .entries
                .iter()
                .flatten()
                .any(|session| session.phase() == Phase::Active && session.identity().ue == key)
        });
        let voice = sessions
            .entries
            .iter()
            .flatten()
            .any(|session| session.phase() == Phase::Active && session.carries_voice());
        let mut wanting: Vec<u32> = sessions
            .entries
            .iter()
            .flatten()
            .filter(|session| {
                session.phase() == Phase::Active && (voice || session.uplink_demand(now))
            })
            .map(|session| session.identity().ue.slot)
            .collect();
        wanting.sort_unstable();
        let settling = {
            let mut seen = RECENTLY_ACTIVE
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            for session in sessions.entries.iter().flatten() {
                let slot = session.identity().ue.slot as usize;
                if session.phase() == Phase::Active && slot < seen.len() {
                    seen[slot] = Some(now);
                }
            }
            seen.iter().flatten().any(|last| {
                last.epoch == now.epoch
                    && last.index != now.index
                    && now.index.saturating_sub(last.index) < SETTLE_CHIPS
            })
        };
        let allocations = {
            let mut shares = SHARES.lock().unwrap_or_else(|error| error.into_inner());
            let sharing: &[u32] = if wanting.len() < 2 { &[] } else { &wanting };
            let due = shares.1.is_none_or(|at| {
                at.epoch != now.epoch || now.index.saturating_sub(at.index) >= SHARE_STEP_CHIPS
            });
            if !shares.0.iter().map(|(slot, _)| slot).eq(sharing) {
                *shares = (process_shares(sharing), Some(now));
            } else if due && !settling {
                let uses: Vec<ShareUse> = sessions
                    .entries
                    .iter()
                    .flatten()
                    .filter(|session| session.phase() == Phase::Active)
                    .filter_map(|session| {
                        let slot = session.identity().ue.slot;
                        let (backlog, used, most, applied) = session.uplink_use()?;
                        let (_, share) = shares.0.iter().find(|(other, _)| *other == slot)?;
                        (*share == applied).then_some(ShareUse {
                            slot,
                            backlog,
                            used: used?,
                            most,
                        })
                    })
                    .collect();
                if uses.len() == shares.0.len() && shift_share(&mut shares.0, &uses) {
                    shares.1 = Some(now);
                }
            }
            shares.0.clone()
        };
        for session in
            visits::occupied(&mut sessions.entries, &mut sessions.cursor, sessions.budget)
        {
            let slot = session.identity().ue.slot;
            let processes = allocations
                .iter()
                .find(|(wanting, _)| *wanting == slot)
                .map_or(0xff, |(_, processes)| *processes);
            if session.phase() == Phase::Active && !settling {
                session.allocate_uplink(now, processes);
            }
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

pub(crate) fn process_shares(wanting: &[u32]) -> Vec<(u32, u8)> {
    if wanting.len() < 2 {
        return Vec::new();
    }
    let mut shares: Vec<(u32, u8)> = wanting.iter().map(|slot| (*slot, 0u8)).collect();
    let count = shares.len();
    for place in 0..count.max(8) {
        shares[place % count].1 |= 1 << (place % 8);
    }
    shares
}

pub(crate) struct ShareUse {
    pub slot: u32,
    pub backlog: bool,
    pub used: f64,
    pub most: f64,
}

const FULL_USE: f64 = 0.8;
const LIGHT_USE: f64 = 0.4;

const TARGET_USE: f64 = 0.2;
// One process starves the acknowledgements of a handset that is downloading.
const LIGHT_PROCESSES: u32 = 2;

// A handset leaving its share idle keeps the processes that hold its traffic at the target use
// and the rest go to one filling its own. One process moves back per call once it fills what it has.
pub(crate) fn shift_share(shares: &mut [(u32, u8)], uses: &[ShareUse]) -> bool {
    let densest = uses.iter().map(|entry| entry.most).fold(0.0, f64::max);
    if densest <= 0.0 {
        return false;
    }
    let count = |slot: u32| {
        shares
            .iter()
            .find(|(other, _)| *other == slot)
            .map_or(0, |(_, processes)| processes.count_ones())
    };
    let full = |entry: &&ShareUse| entry.backlog || entry.used >= FULL_USE * densest;
    let light = |entry: &&ShareUse| !entry.backlog && entry.used < LIGHT_USE * densest;
    let Some(receiver) = uses
        .iter()
        .filter(full)
        .min_by_key(|entry| count(entry.slot))
    else {
        return false;
    };
    let idle = uses
        .iter()
        .filter(light)
        .filter(|entry| count(entry.slot) > LIGHT_PROCESSES)
        .max_by_key(|entry| count(entry.slot));
    let largest = uses
        .iter()
        .max_by_key(|entry| count(entry.slot))
        .filter(|entry| count(entry.slot) >= count(receiver.slot) + 2);
    let Some(donor) = idle.or(largest).filter(|entry| entry.slot != receiver.slot) else {
        return false;
    };
    let held = count(donor.slot);
    let keep = if idle.is_some() {
        ((f64::from(held) * donor.used / (TARGET_USE * densest)).ceil() as u32)
            .clamp(LIGHT_PROCESSES, held)
    } else {
        held - 1
    };
    let mut process = 0u8;
    if let Some((_, processes)) = shares.iter().find(|(slot, _)| *slot == donor.slot) {
        let mut remaining = *processes;
        for _ in keep..held {
            process |= remaining & remaining.wrapping_neg();
            remaining &= remaining - 1;
        }
    }
    if process == 0 {
        return false;
    }
    let (donor, receiver) = (donor.slot, receiver.slot);
    for (slot, processes) in shares.iter_mut() {
        if *slot == donor {
            *processes &= !process;
        } else if *slot == receiver {
            *processes |= process;
        }
    }
    true
}

type Shares = (Vec<(u32, u8)>, Option<ChipInstant>);
static SHARES: std::sync::Mutex<Shares> = std::sync::Mutex::new((Vec::new(), None));
const SHARE_STEP_CHIPS: u64 = 3 * umts_types::time::CHIPS_PER_SECOND;

static RECENTLY_ACTIVE: std::sync::Mutex<[Option<ChipInstant>; 64]> =
    std::sync::Mutex::new([None; 64]);
const SETTLE_CHIPS: u64 = 6 * umts_types::time::CHIPS_PER_SECOND;

#[cfg(test)]
mod share_tests {
    use super::{ShareUse, process_shares, shift_share};

    fn entry(slot: u32, backlog: bool, used: f64) -> ShareUse {
        ShareUse {
            slot,
            backlog,
            used,
            most: used.max(if slot == 0 { 9000.0 } else { 0.0 }),
        }
    }

    #[test]
    fn an_idle_share_moves_to_the_handset_filling_its_own_and_back() {
        let mut shares = process_shares(&[0, 1]);
        let lopsided = [entry(0, true, 9000.0), entry(1, false, 900.0)];
        assert!(shift_share(&mut shares, &lopsided));
        assert_eq!((shares[0].1.count_ones(), shares[1].1.count_ones()), (6, 2));
        assert_eq!(
            (shares[0].1 | shares[1].1, shares[0].1 & shares[1].1),
            (0xff, 0)
        );
        let mut partial = process_shares(&[0, 1]);
        assert!(shift_share(
            &mut partial,
            &[entry(0, true, 9000.0), entry(1, false, 1200.0)]
        ));
        assert_eq!(partial[1].1.count_ones(), 3);
        assert!(!shift_share(&mut shares, &lopsided));
        let mut depressed = process_shares(&[0, 1]);
        assert!(shift_share(
            &mut depressed,
            &[entry(0, true, 3000.0), entry(1, false, 1200.0)]
        ));
        assert_eq!(depressed[1].1.count_ones(), 3);
        let steady = [entry(0, true, 9000.0), entry(1, false, 5000.0)];
        assert!(!shift_share(&mut shares, &steady));
        let both = [entry(0, true, 9000.0), entry(1, true, 9000.0)];
        for expected in [(5, 3), (4, 4)] {
            assert!(shift_share(&mut shares, &both));
            assert_eq!(
                (shares[0].1.count_ones(), shares[1].1.count_ones()),
                expected
            );
        }
        assert!(!shift_share(&mut shares, &both));
        assert!(!shift_share(
            &mut shares,
            &[entry(0, false, 0.0), entry(1, false, 0.0)]
        ));
    }

    #[test]
    fn processes_divide_among_any_number_of_handsets() {
        assert!(process_shares(&[]).is_empty());
        assert!(process_shares(&[3]).is_empty());
        assert_eq!(process_shares(&[0, 1]), [(0, 0x55), (1, 0xaa)]);
        assert_eq!(
            process_shares(&[2, 5, 9]),
            [(2, 0x49), (5, 0x92), (9, 0x24)]
        );
        for count in 2..=8u32 {
            let wanting: Vec<u32> = (0..count).collect();
            let shares = process_shares(&wanting);
            let mut used = 0u8;
            for (_, processes) in &shares {
                assert_eq!(used & processes, 0);
                used |= processes;
            }
            assert_eq!(used, 0xff);
            let sizes: Vec<u32> = shares.iter().map(|share| share.1.count_ones()).collect();
            assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() <= 1);
        }
        let many: Vec<u32> = (0..11).collect();
        let shares = process_shares(&many);
        assert!(shares.iter().all(|share| share.1.count_ones() == 1));
    }
}
