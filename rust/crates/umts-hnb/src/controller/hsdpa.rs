// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{CommonController, ControllerError};
use crate::{
    dedicated_controller::{DedicatedControlError, DedicatedController},
    dedicated_rx::ReceiveIdentity,
    hsdpa_session::{Error, Phase, Session},
    signalling::{
        CommonSignalling, ControlSecurity, HspaCommitError, ReceivedControl, SignallingError,
    },
};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_core::Transport;
use umts_rrc_schema::class_definitions::ULDCCHMessageType;
use umts_types::time::ChipInstant;

mod admission;
mod diagnostics;
mod initial;
mod orders;
mod preparation;
pub use admission::{HsdpaAdmissionRequest, HsdpaAdmissionStep, SecondaryHsdpaAdmission};
pub use initial::InitialCatalogAdmission;
mod transmit;
pub use transmit::{HsdpaSchedulingReport, TransmitStep};

#[derive(Debug)]
pub enum HsdpaControlError {
    Session(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        Error,
    ),
    Receive(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        DedicatedControlError,
    ),
    Commit(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        HspaCommitError<SignallingError>,
    ),
    Signalling(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        SignallingError,
    ),
    Packet(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::dedicated_bearers::TransmitError<umts_mac::MacError>,
    ),
}

#[derive(Debug)]
pub struct HsdpaControlFailure {
    #[expect(
        dead_code,
        reason = "Retains typed failure context for Debug diagnostics."
    )]
    pub identity: ReceiveIdentity,
    #[expect(
        dead_code,
        reason = "Retains typed failure context for Debug diagnostics."
    )]
    pub error: HsdpaControlError,
}

#[derive(Default, Debug)]
pub struct HsdpaControlStep {
    pub radio_changes: usize,
    pub radio_change_waiting: usize,
    pub retiring: usize,
    pub retired: usize,
    pub retirement_reports: usize,
    pub retirement_waiting: usize,
    pub delivered: usize,
    pub retry: usize,
    pub not_transmitted: usize,
    pub feedback_requests: usize,
    pub feedback_reports: usize,
    pub feedback_failures: usize,
    pub feedback_backpressure: usize,
    pub visited: usize,
    pub advanced: usize,
    pub failures: usize,
    pub first_failure: Option<HsdpaControlFailure>,
    pub packet_pdus: usize,
    pub packet_backpressure: usize,
    pub transmit: TransmitStep,
}

enum RadioChangeProgress {
    Released,
    Changed,
    Waiting,
}

pub(super) struct Sessions {
    initial: Option<initial::PendingInitial>,
    entries: Box<[Option<Box<Session>>]>,
    cursor: usize,
    budget: NonZeroUsize,
    admission_cursor: usize,
    attempts: Box<[Option<admission::Attempt>]>,
    single_cell_fallback: Box<[Option<ReceiveIdentity>]>,
    next_incarnation: Option<NonZeroU64>,
    transmit: Option<transmit::Transmit>,
    orders: Option<orders::Orders>,
    preparation: preparation::PreparationWorker,
}

impl Sessions {
    pub(super) fn committed_configuration(
        &self,
        identity: ReceiveIdentity,
    ) -> Option<Arc<umts_rrc::hsdpa::InitialActivation>> {
        let session = self.entries.get(identity.ue.slot as usize)?.as_ref()?;
        (session.identity() == identity && session.phase() == Phase::Active)
            .then(|| session.procedure().construction().clone())
    }
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
            .as_mut()
            .filter(|session| {
                session.identity().ue == received.key()
                    && session.procedure().transaction().get() == transaction
            })?;
        if !security.can_route_hspa_activation(session.procedure()) {
            return None;
        }
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

    pub(super) fn service(
        &mut self,
        now: ChipInstant,
        radio: &mut CommonSignalling,
        receive: &mut DedicatedController,
        admission: &mut crate::rel99_admission::Rel99Admission,
    ) -> HsdpaControlStep {
        let mut report = HsdpaControlStep::default();
        let mut packet_budget = self.budget.get();
        for _ in 0..self.budget.get().min(self.entries.len()) {
            if packet_budget == 0 {
                break;
            }
            let index = self.cursor;
            self.cursor = (index + 1) % self.entries.len();
            let Some(session) = &mut self.entries[index] else {
                continue;
            };
            report.visited += 1;
            let closed = radio
                .phase(session.identity().ue)
                .is_none_or(|phase| phase == umts_rrc::establishment::EstablishmentPhase::Closed);
            if closed {
                session.begin_retirement();
            }
            if session.phase() == Phase::Retiring {
                report.retiring += 1;
                match session.drain_retirement(self.budget) {
                    Ok(step) => {
                        report.retirement_reports += step.reports;
                        report.retirement_waiting += step.waiting;
                    }
                    Err(error) => {
                        report.failures += 1;
                        if report.first_failure.is_none() {
                            report.first_failure = Some(HsdpaControlFailure {
                                identity: session.identity(),
                                error: HsdpaControlError::Session(error),
                            });
                        }
                        continue;
                    }
                }
                if closed || session.detached_route() {
                    let owner = self.entries[index].take().expect("visited session");
                    match owner.try_retire() {
                        Ok(retired) => {
                            drop(retired);
                            self.attempts[index] = None;
                            report.retired += 1;
                        }
                        Err(owner) => self.entries[index] = Some(owner),
                    }
                }
                continue;
            }
            if matches!(session.phase(), Phase::Active | Phase::ChangingRadio)
                && let Some(next) = admission
                    .committed_identity_at(index)
                    .filter(|next| next.ue == session.identity().ue && *next != session.identity())
            {
                let result = (|| {
                    if receive
                        .with_active_mapping(next, |bank| session.retire_released_route(bank))
                        .map_err(HsdpaControlError::Receive)?
                        .map_err(HsdpaControlError::Session)?
                    {
                        return Ok(RadioChangeProgress::Released);
                    }
                    session
                        .begin_radio_change(next)
                        .map_err(HsdpaControlError::Session)?;
                    let drained = session
                        .drain_radio_change(self.budget)
                        .map_err(HsdpaControlError::Session)?;
                    let command_pending = self
                        .orders
                        .as_ref()
                        .is_some_and(|orders| orders.has_flight(session.identity()));
                    let settled = session
                        .settle_radio_change_orders(now, command_pending)
                        .map_err(HsdpaControlError::Session)?;
                    if !drained || !settled {
                        return Ok(RadioChangeProgress::Waiting);
                    }
                    receive
                        .with_active_mapping(next, |bank| {
                            admission.transfer_hsdpa_feedback(next, |feedback| {
                                session.finish_radio_change(bank, feedback)
                            })
                        })
                        .map_err(HsdpaControlError::Receive)?
                        .map(|retired| {
                            if retired.is_some() {
                                RadioChangeProgress::Changed
                            } else {
                                RadioChangeProgress::Waiting
                            }
                        })
                        .map_err(HsdpaControlError::Session)
                })();
                match result {
                    Ok(RadioChangeProgress::Released) => report.retiring += 1,
                    Ok(RadioChangeProgress::Changed) => report.radio_changes += 1,
                    Ok(RadioChangeProgress::Waiting) => report.radio_change_waiting += 1,
                    Err(error) => {
                        report.failures += 1;
                        if report.first_failure.is_none() {
                            report.first_failure = Some(HsdpaControlFailure {
                                identity: session.identity(),
                                error,
                            });
                        }
                    }
                }
                continue;
            }
            let phase = session.phase();
            let feedback = if phase == Phase::Active {
                feedback_one(session, now, receive)
            } else {
                Ok(crate::hsdpa_session::FeedbackServiceStep::default())
            };
            let result = feedback.and_then(|feedback| {
                report.delivered += feedback.delivered;
                report.retry += feedback.retry;
                report.not_transmitted += feedback.not_transmitted;
                report.feedback_requests += feedback.requested;
                report.feedback_reports += feedback.reports;
                report.feedback_failures += feedback.failures;
                report.feedback_backpressure += usize::from(feedback.backpressure);
                if phase == Phase::Active {
                    let budget = NonZeroUsize::new(packet_budget).expect("remaining cycle budget");
                    match feed_one(session, now, radio, receive, budget) {
                        Ok(step) => {
                            packet_budget = packet_budget.saturating_sub(step.visited);
                            report.packet_pdus += step.enqueued;
                            report.packet_backpressure += step.backpressure;
                            Ok(())
                        }
                        Err(error) => {
                            packet_budget = 0;
                            Err(error)
                        }
                    }
                } else {
                    service_one(session, now, radio, receive)
                }
            });
            if let Err(error) = result {
                report.failures += 1;
                if report.first_failure.is_none() {
                    report.first_failure = Some(HsdpaControlFailure {
                        identity: session.identity(),
                        error,
                    });
                }
            }
            report.advanced += usize::from(session.phase() != phase);
        }
        report
    }
}

fn feedback_one(
    session: &mut Session,
    now: ChipInstant,
    receive: &mut DedicatedController,
) -> Result<crate::hsdpa_session::FeedbackServiceStep, HsdpaControlError> {
    let horizon = ChipInstant {
        index: now
            .index
            .checked_add(3 * umts_types::time::CHIPS_PER_SLOT)
            .ok_or(HsdpaControlError::Session(Error::Bearer(
                crate::dedicated_bearers::BearerError::Time,
            )))?,
        ..now
    };
    receive
        .with_active_mapping(session.identity(), |bank| {
            session.service_feedback(bank, now, horizon)
        })
        .map_err(HsdpaControlError::Receive)?
        .map_err(HsdpaControlError::Session)
}

fn feed_one(
    session: &mut Session,
    now: ChipInstant,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
    budget: NonZeroUsize,
) -> Result<crate::hsdpa_session::QueueFeedStep, HsdpaControlError> {
    let identity = session.identity();
    radio
        .with_bearer_access(identity.ue, |ciphers, reset, _| {
            if matches!(reset, crate::dedicated_bearers::ResetPermission::Defer) {
                return Ok(crate::hsdpa_session::QueueFeedStep::default());
            }
            receive
                .with_active_mapping(identity, |bank| {
                    session.feed_queues(bank, now, ciphers, budget)
                })
                .map_err(HsdpaControlError::Receive)?
                .map_err(HsdpaControlError::Packet)
        })
        .map_err(HsdpaControlError::Signalling)?
}

fn service_one(
    session: &mut Session,
    now: ChipInstant,
    radio: &mut CommonSignalling,
    receive: &mut DedicatedController,
) -> Result<(), HsdpaControlError> {
    if let Some(identity) = session.active_recovery_identity() {
        return receive
            .with_active_mapping(identity, |bank| session.recover_active_route(radio, bank))
            .map_err(HsdpaControlError::Receive)?
            .map_err(HsdpaControlError::Session);
    }
    if matches!(
        session.phase(),
        Phase::Prepared | Phase::AwaitingComplete | Phase::RecoveryRequired
    ) {
        session
            .expire(radio, now)
            .map_err(HsdpaControlError::Session)?;
    }
    let identity = session.identity();
    match session.phase() {
        Phase::Prepared => {
            let published = receive
                .with_active_mapping(identity, |bank| session.publish(radio, bank, now))
                .map_err(HsdpaControlError::Receive)?;
            match published {
                Ok(()) | Err(Error::Signalling(SignallingError::Capacity)) => Ok(()),
                Err(error) => Err(HsdpaControlError::Session(error)),
            }
        }
        Phase::Accepted => {
            let construction = session.procedure().construction().clone();
            session
                .commit(radio, |_| {
                    receive
                        .with_active_mapping(identity, |bank| {
                            let route = bank
                                .hsdpa_route(identity)
                                .map_err(|_| SignallingError::Mapping)?;
                            if !route.is_some_and(|route| Arc::ptr_eq(route, &construction)) {
                                return Err(SignallingError::Mapping);
                            }
                            Ok(())
                        })
                        .map_err(SignallingError::DedicatedControl)?
                })
                .map_err(HsdpaControlError::Commit)
        }
        Phase::AwaitingCommandAck => match session.finish_command(radio) {
            Ok(()) | Err(SignallingError::Capacity) => Ok(()),
            Err(error) => Err(HsdpaControlError::Signalling(error)),
        },
        Phase::Cancelled => {
            receive
                .with_active_mapping(identity, |bank| {
                    session.cancel_before_publication(bank, now)
                })
                .map_err(HsdpaControlError::Receive)?
                .map_err(HsdpaControlError::Session)?;
            session.begin_retirement();
            Ok(())
        }
        Phase::RecoveryRequired => Err(HsdpaControlError::Session(Error::Phase)),
        Phase::AwaitingComplete | Phase::Active | Phase::ChangingRadio | Phase::Retiring => Ok(()),
    }
}

impl<T: Transport> CommonController<T> {
    pub fn visit_hsdpa_diagnostics(
        &self,
        mut visit: impl FnMut(ReceiveIdentity, Phase, crate::hsdpa_session::FeedbackSnapshot),
    ) {
        if let Some(sessions) = &self.hsdpa {
            for session in sessions.entries.iter().flatten() {
                visit(
                    session.identity(),
                    session.phase(),
                    session.feedback_snapshot(),
                );
            }
        }
    }

    pub fn configure_hsdpa_sessions(
        &mut self,
        budget: NonZeroUsize,
    ) -> Result<(), ControllerError> {
        if self.hsdpa.is_some() {
            return Err(ControllerError::Configuration);
        }
        let capacity = self
            .admission
            .as_ref()
            .ok_or(ControllerError::Configuration)?
            .capacity();
        self.hsdpa = Some(Sessions {
            initial: None,
            entries: (0..capacity).map(|_| None).collect(),
            cursor: 0,
            budget,
            admission_cursor: 0,
            attempts: (0..capacity).map(|_| None).collect(),
            single_cell_fallback: vec![None; capacity].into_boxed_slice(),
            next_incarnation: Some(NonZeroU64::MIN),
            transmit: None,
            orders: None,
            preparation: preparation::PreparationWorker::new()
                .map_err(|_| ControllerError::Configuration)?,
        });
        Ok(())
    }
}
