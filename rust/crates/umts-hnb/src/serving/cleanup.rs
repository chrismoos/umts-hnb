// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    controller::{CommonController, MediaServiceError},
    dedicated_rx::ReceiveIdentity,
    dedicated_tx::TransmitDrain,
    rab_handoff::{FailedRabAssignment, FailureDispositionError},
    radio_resources::RadioResources,
    rel99_admission::{
        AdmissionError, CircuitProgress, PacketProgress, RabAdmissionError,
        RejectedAdditionalAssignment, RejectedRabAssignment, RejectedReleaseAssignment,
        ReleaseAdmissionError, ReleaseRollbackError, RetiredAdmission,
    },
    rel99_traffic::Rel99Traffic,
};
use anyhow::{Context, Result};
use std::num::NonZeroUsize;
use umts_core::Transport;
use umts_iuh::{
    ranap::rab::Cause,
    session::{ControlTime, RabRequest},
};
use umts_types::time::ChipInstant;
use umts_user_plane::packet::BoundDatagrams;

const RADIO_INTERFACE_PROCEDURE_FAILURE: u16 = 14;

#[cfg(test)]
pub(super) mod tests;

fn outcomes_settled(
    expected: Option<usize>,
    packet: Option<PacketProgress>,
    circuit: Option<CircuitProgress>,
) -> bool {
    let packet = packet.unwrap_or_default();
    let circuit = circuit.unwrap_or_default();
    let covered = packet.items + circuit.items;
    expected.is_none_or(|items| items == covered)
        && packet.recorded + packet.superseded == packet.items
        && circuit.recorded + circuit.superseded == circuit.items
}

#[derive(Debug)]
pub enum CleanupFailure {
    Hsupa {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::controller::HsupaCleanupError,
    },
    RadioRetirement {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::signalling::SignallingError,
    },
    DedicatedRetirement {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        identity: ReceiveIdentity,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        phase: umts_rrc::dedicated::SetupPhase,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        error: AdmissionError,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        transmit: Option<Box<crate::dedicated_tx::TransmitFailureEvidence>>,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        admission_phase: Option<crate::rel99_admission::AdmissionPhase>,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        startup: Option<Box<crate::rel99_admission::StartupProgress>>,
    },
    AwaitingWorkerRetirement {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: ReceiveIdentity,
    },
    Transition {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::rel99_admission::FailedChangeError,
    },
    RejectedRelease {
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        request: RabRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: ReleaseAdmissionError,
    },
    ReleaseRollback {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: RabRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: ReleaseRollbackError,
    },
    Rejected {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: RabRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: RabAdmissionError,
    },
    Disposition {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: RabRequest,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        error: FailureDispositionError,
    },
    Core {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: RabRequest,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: umts_core::Error,
    },
    Admission {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: AdmissionError,
    },
    Media {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        identity: ReceiveIdentity,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: MediaServiceError,
    },
    Resources {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        request: Option<RabRequest>,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::radio_resources::Error,
    },
    DrainUnavailable(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        ReceiveIdentity,
    ),
    DrainLost(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        ReceiveIdentity,
    ),
}

#[derive(Default, Debug)]
pub(super) struct CleanupStep {
    pub release_transferred: Option<RabRequest>,
    pub visited: usize,
    pub completed: usize,
    pub failures: usize,
    pub first_failure: Option<CleanupFailure>,
}

impl CleanupStep {
    fn setup_retired(
        &mut self,
        identity: ReceiveIdentity,
        phase: umts_rrc::dedicated::SetupPhase,
        error: &AdmissionError,
        transmit: Option<&crate::dedicated_tx::TransmitFailureEvidence>,
        admission_phase: Option<crate::rel99_admission::AdmissionPhase>,
        startup: Option<&crate::rel99_admission::StartupProgress>,
    ) {
        if phase != umts_rrc::dedicated::SetupPhase::Complete || transmit.is_some() {
            self.failed(CleanupFailure::DedicatedRetirement {
                identity,
                phase,
                error: error.clone(),
                transmit: transmit.cloned().map(Box::new),
                admission_phase,
                startup: startup.cloned().map(Box::new),
            });
        }
    }

    fn failed(&mut self, error: CleanupFailure) {
        self.failures += 1;
        if self.first_failure.is_none() {
            self.first_failure = Some(error);
        }
    }
}

enum State {
    Quarantined(crate::rel99_admission::QuarantinedAdmission),
    Rejected(Box<RejectedRabAssignment>),
    RejectedAdditional(Box<RejectedAdditionalAssignment>),
    RejectedRelease(Box<RejectedReleaseAssignment>),
    Reporting(FailedRabAssignment),
    Retired {
        owner: Box<RetiredAdmission>,
        reporting: Option<FailedRabAssignment>,
    },
}

pub(super) struct Cleanup {
    slots: Vec<Option<State>>,
    retired: Vec<ReceiveIdentity>,
    cursor: usize,
    budget: NonZeroUsize,
    recovery: Option<ReceiveIdentity>,
}

fn cause() -> Cause {
    // TS 25.413 CauseRadioNetwork identifies failure in the radio interface procedure as 14.
    Cause::from_code(RADIO_INTERFACE_PROCEDURE_FAILURE).expect("standard radio cause")
}

impl Cleanup {
    pub(super) fn new(capacity: NonZeroUsize, budget: NonZeroUsize) -> Result<Self> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity.get())
            .context("reserve bearer cleanup slots")?;
        slots.resize_with(capacity.get(), || None);
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(capacity.get())
            .context("reserve retired bearer visits")?;
        Ok(Self {
            slots,
            retired,
            cursor: 0,
            budget,
            recovery: None,
        })
    }

    pub(super) fn recovery_required(&self) -> Option<ReceiveIdentity> {
        self.recovery
    }

    pub(super) fn recovery_error(&self) -> Option<&AdmissionError> {
        let identity = self.recovery?;
        self.slots.iter().flatten().find_map(|state| match state {
            State::Quarantined(owner) if owner.identity() == identity => Some(owner.error()),
            _ => None,
        })
    }

    pub(super) fn quarantine(
        &mut self,
        slot: usize,
        owner: crate::rel99_admission::QuarantinedAdmission,
    ) {
        assert!(
            self.slots[slot].is_none(),
            "cleanup slot reserved before quarantine transfer"
        );
        let identity = owner.identity();
        self.slots[slot] = Some(State::Quarantined(owner));
        self.recovery.get_or_insert(identity);
    }

    pub(super) fn vacant(&self) -> Option<usize> {
        self.slots.iter().position(Option::is_none)
    }

    pub(super) fn pending(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    pub(super) fn reject(&mut self, slot: usize, rejected: Box<RejectedRabAssignment>) {
        assert!(
            self.slots[slot].is_none(),
            "cleanup slot reserved before request transfer"
        );
        self.slots[slot] = Some(State::Rejected(rejected));
    }

    pub(super) fn reject_additional(
        &mut self,
        slot: usize,
        rejected: Box<RejectedAdditionalAssignment>,
    ) {
        assert!(
            self.slots[slot].is_none(),
            "cleanup slot reserved before request transfer"
        );
        self.slots[slot] = Some(State::RejectedAdditional(rejected));
    }

    pub(super) fn reject_release(&mut self, slot: usize, rejected: Box<RejectedReleaseAssignment>) {
        assert!(
            self.slots[slot].is_none(),
            "cleanup slot reserved before request transfer"
        );
        self.slots[slot] = Some(State::RejectedRelease(rejected));
    }

    pub(super) fn step<T: Transport, D: BoundDatagrams>(
        &mut self,
        controller: &mut CommonController<T>,
        resources: &mut RadioResources,
        traffic: &mut Rel99Traffic<D>,
        now: ChipInstant,
        control: ControlTime,
    ) -> CleanupStep {
        let mut report = CleanupStep::default();
        self.retired.clear();
        controller.visit_failed_changes(NonZeroUsize::MIN, |identity| self.retired.push(identity));
        if let Some(identity) = self.retired.pop()
            && let Some(slot) = self.vacant()
        {
            match controller.take_failed_change(identity, resources) {
                Ok(Some(crate::rel99_admission::FailedChangeRequest::Quarantined(owner))) => {
                    report.release_transferred = owner.release_request();
                    report.failed(CleanupFailure::AwaitingWorkerRetirement {
                        identity: owner.identity(),
                    });
                    self.quarantine(slot, owner);
                }
                Ok(Some(crate::rel99_admission::FailedChangeRequest::Addition(rejected))) => {
                    self.reject_additional(slot, rejected)
                }
                Ok(Some(crate::rel99_admission::FailedChangeRequest::Release(rejected))) => {
                    report.release_transferred = Some(rejected.pending.request());
                    self.reject_release(slot, rejected);
                }
                Ok(None) => (),
                Err(error) => report.failed(CleanupFailure::Admission { identity, error }),
            }
        }
        let retired = &mut self.retired;
        controller.visit_retired_initial_rabs(self.budget, |identity| {
            if retired.len() < retired.capacity() {
                retired.push(identity);
            }
        });
        for index in 0..self.retired.len() {
            if report.release_transferred.is_some() {
                break;
            }
            let Some(slot) = self.vacant() else { break };
            let identity = self.retired[index];
            match controller.take_retired_dch(identity) {
                Ok(owner) => {
                    report.setup_retired(
                        identity,
                        owner.procedure.phase(),
                        &owner.error,
                        owner
                            .transmit
                            .as_ref()
                            .and_then(|session| session.failure_evidence()),
                        owner.failed_phase,
                        owner.startup.as_ref(),
                    );
                    report.release_transferred = owner
                        .failed_change
                        .as_ref()
                        .and_then(|change| change.release_request());
                    self.slots[slot] = Some(State::Retired {
                        owner: Box::new(owner),
                        reporting: None,
                    })
                }
                Err(error) => report.failed(CleanupFailure::Admission { identity, error }),
            }
        }
        self.retired.clear();
        for _ in 0..self.budget.get().min(self.slots.len()) {
            let slot = self.cursor;
            self.cursor = (slot + 1) % self.slots.len();
            let Some(state) = self.slots[slot].take() else {
                continue;
            };
            report.visited += 1;
            self.slots[slot] = advance(
                state,
                controller,
                resources,
                traffic,
                now,
                control,
                &mut report,
            );
            report.completed += usize::from(self.slots[slot].is_none());
        }
        report
    }
}

fn advance<T: Transport, D: BoundDatagrams>(
    state: State,
    controller: &mut CommonController<T>,
    resources: &mut RadioResources,
    traffic: &mut Rel99Traffic<D>,
    now: ChipInstant,
    control: ControlTime,
    report: &mut CleanupStep,
) -> Option<State> {
    match state {
        State::Quarantined(owner) => Some(State::Quarantined(owner)),
        State::RejectedRelease(mut rejected) => {
            let request = rejected.pending.request();
            if let Err(error) = rejected.rollback(controller.radio_mut(), resources) {
                report.failed(CleanupFailure::ReleaseRollback { request, error });
                return Some(State::RejectedRelease(rejected));
            }
            report_rejected_release(rejected, controller.core().gateway(), report)
        }
        State::Rejected(mut rejected) => {
            let request = rejected.pending.request();
            if let Some(allocation) = rejected.allocation.take()
                && let Err(failed) = resources.cancel(allocation)
            {
                rejected.allocation = Some(failed.allocation);
                rejected.rollback_error = Some(failed.error.clone());
                report.failed(CleanupFailure::Resources {
                    request: Some(request),
                    error: failed.error,
                });
                return Some(State::Rejected(rejected));
            }
            let RejectedRabAssignment {
                error,
                pending,
                allocation: _,
                rollback_error,
            } = *rejected;
            match FailedRabAssignment::new(pending, cause(), controller.core().gateway()) {
                Ok(reporting) => {
                    report.failed(CleanupFailure::Rejected { request, error });
                    Some(State::Reporting(reporting))
                }
                Err(failed) => {
                    report.failed(CleanupFailure::Disposition {
                        request,
                        error: failed.error,
                    });
                    Some(State::Rejected(Box::new(RejectedRabAssignment {
                        error,
                        pending: failed.pending,
                        allocation: None,
                        rollback_error,
                    })))
                }
            }
        }
        State::RejectedAdditional(mut rejected) => {
            let request = rejected.pending.request();
            if let Some(change) = rejected.change.take()
                && let Err(failed) = resources.cancel_reconfiguration(change)
            {
                rejected.change = Some(failed.change);
                rejected.rollback_error = Some(failed.error.clone());
                report.failed(CleanupFailure::Resources {
                    request: Some(request),
                    error: failed.error,
                });
                return Some(State::RejectedAdditional(rejected));
            }
            let RejectedAdditionalAssignment {
                error,
                pending,
                change: _,
                rollback_error,
            } = *rejected;
            match FailedRabAssignment::new(pending, cause(), controller.core().gateway()) {
                Ok(reporting) => {
                    report.failed(CleanupFailure::Rejected { request, error });
                    Some(State::Reporting(reporting))
                }
                Err(failed) => {
                    report.failed(CleanupFailure::Disposition {
                        request,
                        error: failed.error,
                    });
                    Some(State::RejectedAdditional(Box::new(
                        RejectedAdditionalAssignment {
                            error,
                            pending: failed.pending,
                            change: None,
                            rollback_error,
                        },
                    )))
                }
            }
        }
        State::Reporting(mut reporting) => {
            if let Err(error) =
                controller.record_failed_assignment(&mut reporting, control, NonZeroUsize::MIN)
            {
                report.failed(CleanupFailure::Core {
                    request: reporting.request(),
                    error,
                });
            }
            (!reporting.is_complete()).then_some(State::Reporting(reporting))
        }
        State::Retired {
            mut owner,
            mut reporting,
        } => {
            let keep = |owner, reporting| Some(State::Retired { owner, reporting });
            let identity = owner.identity;
            match controller.finish_failed_change(&mut owner, resources, now, control, cause()) {
                Ok(true) => (),
                Ok(false) => return keep(owner, reporting),
                Err(error) => {
                    report.failed(CleanupFailure::Transition { identity, error });
                    return keep(owner, reporting);
                }
            }
            let (voices, packets) = traffic.services_mut();
            if let Some(state) = owner.packet_assignment.as_mut()
                && let Err(error) = state.retire(packets)
            {
                report.failed(CleanupFailure::Media {
                    identity,
                    error: MediaServiceError::Packet(error),
                });
                return keep(owner, reporting);
            }
            if let Some(state) = owner.circuit_assignment.as_mut()
                && let Err(error) = state.retire(voices)
            {
                report.failed(CleanupFailure::Media {
                    identity,
                    error: MediaServiceError::Circuit(error),
                });
                return keep(owner, reporting);
            }
            if owner.packet_assignment.as_ref().is_some_and(|state| {
                controller
                    .core()
                    .gateway()
                    .owns_rab_request(state.request())
            }) && let Err(error) =
                controller.finish_packet_failure_dch(&mut owner, packets, control, cause())
            {
                report.failed(CleanupFailure::Media {
                    identity,
                    error: MediaServiceError::Packet(error),
                });
                return keep(owner, reporting);
            }
            if owner.circuit_assignment.as_ref().is_some_and(|state| {
                controller
                    .core()
                    .gateway()
                    .owns_rab_request(state.request())
            }) && let Err(error) =
                controller.finish_circuit_failure_dch(&mut owner, voices, control, cause())
            {
                report.failed(CleanupFailure::Media {
                    identity,
                    error: MediaServiceError::Circuit(error),
                });
                return keep(owner, reporting);
            }
            if let Err(error) = controller.finish_additional_media_failure_dch(
                &mut owner,
                packets,
                voices,
                control,
                cause(),
            ) {
                report.failed(CleanupFailure::Media {
                    identity,
                    error: MediaServiceError::Additional(error),
                });
                return keep(owner, reporting);
            }
            let settled = outcomes_settled(
                owner.rab_assignment.as_ref().map(|pending| {
                    pending.assignment().setup.len() + pending.assignment().release.len()
                }),
                owner
                    .packet_assignment
                    .as_ref()
                    .map(|state| state.progress()),
                owner
                    .circuit_assignment
                    .as_ref()
                    .map(|state| state.progress(voices)),
            );
            if reporting.is_none()
                && !settled
                && let Some(pending) = owner.rab_assignment.take()
            {
                let request = pending.request();
                match FailedRabAssignment::new(pending, cause(), controller.core().gateway()) {
                    Ok(failed) => reporting = Some(failed),
                    Err(failed) => {
                        owner.rab_assignment = Some(failed.pending);
                        report.failed(CleanupFailure::Disposition {
                            request,
                            error: failed.error,
                        });
                        return keep(owner, reporting);
                    }
                }
            }
            if let Some(failed) = reporting.as_mut() {
                if let Err(error) =
                    controller.record_failed_assignment(failed, control, NonZeroUsize::MIN)
                {
                    report.failed(CleanupFailure::Core {
                        request: failed.request(),
                        error,
                    });
                }
                if !failed.is_complete() {
                    return keep(owner, reporting);
                }
            } else if !settled {
                report.failed(CleanupFailure::DrainUnavailable(identity));
                return keep(owner, reporting);
            }
            let Some(transmit) = owner.transmit.as_ref() else {
                report.failed(CleanupFailure::DrainUnavailable(identity));
                return keep(owner, reporting);
            };
            match transmit.drain() {
                TransmitDrain::LocalUnsubmitted | TransmitDrain::NoContribution => (),
                TransmitDrain::Unknown => {
                    report.failed(CleanupFailure::DrainUnavailable(identity));
                    return keep(owner, reporting);
                }
                TransmitDrain::ResolvedTail(receipt) => {
                    if !receipt.viable() {
                        report.failed(CleanupFailure::DrainLost(identity));
                        return keep(owner, reporting);
                    }
                    if !receipt.resolved()
                        || receipt.air_end().epoch != now.epoch
                        || now.index < receipt.air_end().index
                    {
                        return keep(owner, reporting);
                    }
                }
            }
            match controller.cleanup_retired_hsupa(&mut owner, resources, now) {
                Ok(true) => (),
                Ok(false) => return keep(owner, reporting),
                Err(error) => {
                    report.failed(CleanupFailure::Hsupa { identity, error });
                    return keep(owner, reporting);
                }
            }
            if let Some(lease) = owner.resources.take()
                && let Err(failed) = resources.release(lease)
            {
                owner.resources = Some(failed.lease);
                report.failed(CleanupFailure::Resources {
                    request: owner
                        .rab_assignment
                        .as_ref()
                        .map(|pending| pending.request()),
                    error: failed.error,
                });
                return keep(owner, reporting);
            }
            if owner.procedure.receipt().is_some()
                && let Err(error) = controller.radio_mut().retire_failed_radio(identity)
            {
                report.failed(CleanupFailure::RadioRetirement { identity, error });
                return keep(owner, reporting);
            }
            None
        }
    }
}

fn report_rejected_release<T: Transport>(
    rejected: Box<RejectedReleaseAssignment>,
    gateway: &umts_core::Gateway<T>,
    report: &mut CleanupStep,
) -> Option<State> {
    let request = rejected.pending.request();
    let RejectedReleaseAssignment {
        error,
        pending,
        pool,
        change,
        rollback_error,
    } = *rejected;
    match FailedRabAssignment::new(pending, cause(), gateway) {
        Ok(reporting) => {
            report.failed(CleanupFailure::RejectedRelease { request, error });
            Some(State::Reporting(reporting))
        }
        Err(failed) => {
            report.failed(CleanupFailure::Disposition {
                request,
                error: failed.error,
            });
            Some(State::RejectedRelease(Box::new(
                RejectedReleaseAssignment {
                    error,
                    pending: failed.pending,
                    pool,
                    change,
                    rollback_error,
                },
            )))
        }
    }
}
