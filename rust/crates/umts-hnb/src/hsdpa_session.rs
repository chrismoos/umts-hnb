// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    dedicated_bearers::{BearerError, DedicatedBearers},
    dedicated_rx::ReceiveIdentity,
    hsdpa_packet::{FeedbackError, OwnerError, PacketOwner, PacketQueues},
    hsdpa_rx::{FeedbackPort, Observation, Report, Request},
    signalling::{
        AcceptedHsdpaActivation, CommonSignalling, ControlSecurity, HspaCommitError,
        ReceivedControl, SignallingError,
    },
};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::hsdpa::HarqAck;
use umts_mac::{
    MacError,
    hsdpa::{Feedback, PacketQueue, QueueFormat},
};
use umts_rrc::{
    dedicated::{DedicatedError, ReconfigurationCounters},
    hsdpa::{ActivationPhase, ActivationProcedure, ActivationResponse},
};
use umts_security::ciphering::BearerCiphers;
use umts_types::time::ChipInstant;

mod activation;
mod adaptation;
pub(crate) mod retirement;
pub use activation::RetainedDpchPolicy;
pub use retirement::{RetirementError, RetirementProgress};
mod completion;
mod transmit;
pub use completion::{CommandFailure, CommandProgress};
mod feedback;
pub use feedback::{
    AirtimeSummary, FeedbackCounts, FeedbackServiceStep, FeedbackSnapshot, SelectionCounts,
};
mod timing;
pub use timing::FeedbackTiming;
mod dispatch;
mod power;
mod runtime;
pub(crate) use runtime::SchedulingPreparation;
pub use runtime::{
    CellSchedulingBudget, SchedulingAdmission, SchedulingBudget, SchedulingOutcome,
    SchedulingProgress, SchedulingResources, SchedulingSettings, SchedulingTime,
};
#[cfg(test)]
mod dual_tests;
mod queues;
mod radio_change;
mod rates;
mod resolution;
mod scheduling;
mod sdu_budget;
mod selection;
pub use dispatch::{DispatchError, PacketBuffers, PacketPreparation, PreparationOutcome};
pub use power::DownlinkPower;
pub use queues::QueueFeedStep;
pub use rates::{Rate, Rates};
pub use resolution::ResolutionStep;
pub use scheduling::{PacketCandidate, PacketWork};
pub use selection::{SelectedPacket, SelectionRequest};
pub use transmit::{PacketCancellationError, PacketPublication, PacketPublicationError};

#[derive(Clone, Copy)]
pub struct QueueBudget {
    pub pdus_per_queue: NonZeroUsize,
    pub total_bytes: NonZeroUsize,
}

pub struct SessionSettings<'a> {
    pub incarnation: NonZeroU64,
    pub downlink_origin: ChipInstant,
    pub queues: QueueBudget,
    pub codes: crate::radio_resources::HsdpaCodeSet,
    pub scheduling: Option<&'a SchedulingAdmission>,
}

pub struct DualCellSettings {
    pub category: umts_types::hsdpa::DualCellCategory,
    pub incarnation: NonZeroU64,
    pub codes: crate::radio_resources::HsdpaCodeSet,
    pub resources: Box<SchedulingResources>,
    pub feedback: crate::hsdpa_rx::FeedbackPolicy,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Identity,
    Phase,
    Capacity,
    Queue(MacError),
    Packet(OwnerError),
    Bearer(BearerError),
    Signalling(SignallingError),
    Procedure(DedicatedError),
    Rate(umts_dsp::DspError),
    Timing(umts_dsp::DspError),
    Allocation(crate::hsdpa_tx::Error),
}

pub struct RejectedSession {
    pub error: Error,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Owns the rejected operation until it is retried or dropped."
        )
    )]
    pub procedure: Box<ActivationProcedure>,
    pub feedback: FeedbackPort,
}

enum Admission {
    Pending,
    Accepted(Box<AcceptedHsdpaActivation>),
    Committed(Box<AcceptedHsdpaActivation>),
    Active(Box<AcceptedHsdpaActivation>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Prepared,
    AwaitingComplete,
    Accepted,
    AwaitingCommandAck,
    Active,
    ChangingRadio,
    RecoveryRequired,
    Cancelled,
    Retiring,
}

#[must_use = "Retain the packet, feedback and activation owners until radio retirement completes"]
pub struct CellLink {
    carrier: umts_types::hsdpa::CarrierId,
    incarnation: NonZeroU64,
    recycled_blocks: Box<[Option<crate::hsdpa_packet::BlockBuffer>]>,
    resolution_cursor: usize,
    feedback_policy: Option<crate::hsdpa_rx::FeedbackPolicy>,
    feedback_snapshot: feedback::FeedbackSnapshot,
    quality: feedback::Quality,
    adaptation: adaptation::LinkAdaptation,
    staged_feedback_training: [adaptation::FeedbackTraining; 8],
    unusable_cqi_reports: u8,
    rates: Rates,
    packets: PacketOwner,
    pending: Option<transmit::PendingPacket>,
    process_cursor: usize,
    packet_queue_cursor: usize,
    queue_commands: Box<
        [Option<(
            crate::dedicated_tx::TransmitCommandId,
            umts_mac::hsdpa::TransmissionKey,
        )>],
    >,
    scheduling: Option<Box<SchedulingResources>>,
}

pub struct Session {
    links: Vec<CellLink>,
    feedback_mode: Option<crate::hsdpa_rx::FeedbackModeSchedule>,
    secondary_control: SecondaryControl,
    downlink_origin: ChipInstant,
    identity: ReceiveIdentity,
    procedure: Box<ActivationProcedure>,
    queues: PacketQueues,
    sdu_budget: sdu_budget::SduBudget,
    staged_buffer_carriers: [Option<umts_types::hsdpa::CarrierId>; 8],
    scheduling_carrier_cursor: usize,
    feedback: FeedbackPort,
    admission: Admission,
    retirement: retirement::Retirement,
    detached_route: bool,
    radio_change: Option<radio_change::RadioChange>,
    queue_cursor: usize,
    resolution_link_cursor: usize,
}

pub struct FeedbackStep {
    pub report: Report,
    pub admission: Option<Result<(), FeedbackError>>,
    /// Exact successful associations, indexed by carrier, independent of aggregate status.
    admitted_transmissions: [Option<umts_mac::hsdpa::ScheduledTransmission>; 2],
}

#[must_use = "Per-UE TX retirement does not retire the bearer mapping or release the shared carrier lease"]
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct RetiredSession {
    pub recycled_blocks: Box<[Option<crate::hsdpa_packet::BlockBuffer>]>,
    pub identity: ReceiveIdentity,
    pub procedure: Box<ActivationProcedure>,
    pub accepted: Option<Box<AcceptedHsdpaActivation>>,
    pub packets: crate::hsdpa_packet::RetiredPackets,
    pub queues: PacketQueues,
    pub additional_links: Vec<CellLink>,
    pub feedback: FeedbackPort,
    pub scheduling: Option<Box<SchedulingResources>>,
}

impl Session {
    pub fn secondary_order_feedback_pending(&self) -> bool {
        matches!(
            self.secondary_control,
            SecondaryControl::Dual {
                feedback: Some(_),
                ..
            }
        )
    }

    pub fn data_publication_allowed(&self, carrier: umts_types::hsdpa::CarrierId) -> bool {
        self.has_carrier(carrier)
            && self.phase() == Phase::Active
            && !matches!(
                self.order_progress(),
                OrderProgress::Ready | OrderProgress::Waiting | OrderProgress::Exhausted
            )
            && (carrier == umts_types::hsdpa::CarrierId::Primary
                || self.secondary_state() == Some(SecondaryState::Active))
    }

    pub(crate) fn new_prepared_dual(
        identity: ReceiveIdentity,
        procedure: Box<ActivationProcedure>,
        feedback: FeedbackPort,
        settings: SessionSettings<'_>,
        resources: Option<(Box<SchedulingResources>, crate::hsdpa_rx::FeedbackPolicy)>,
        secondary: DualCellSettings,
    ) -> Result<Self, Box<RejectedSession>> {
        let mut session = Self::new_prepared(identity, procedure, feedback, settings, resources)?;
        if let Err(error) = session.configure_dual_cell(
            secondary.category,
            secondary.incarnation,
            secondary.codes,
            secondary.resources,
            secondary.feedback,
        ) {
            return Err(Box::new(RejectedSession {
                error,
                procedure: session.procedure,
                feedback: session.feedback,
            }));
        }
        Ok(session)
    }

    pub fn carriers(&self) -> impl Iterator<Item = umts_types::hsdpa::CarrierId> + '_ {
        self.links.iter().map(|link| link.carrier)
    }

    pub fn has_carrier(&self, carrier: umts_types::hsdpa::CarrierId) -> bool {
        self.links
            .get(carrier.index())
            .is_some_and(|link| link.carrier == carrier)
    }

    pub fn retirement_pending_any(&self) -> bool {
        self.links.iter().any(|link| link.pending.is_some())
    }

    pub fn carrier_for_command(
        &self,
        result: &crate::hsdpa_packet::CommandResult,
    ) -> Option<umts_types::hsdpa::CarrierId> {
        use crate::hsdpa_packet::Operation;
        if let Operation::Queue(key) = result.operation {
            return self
                .links
                .iter()
                .find(|link| {
                    link.queue_commands
                        .get(usize::from(key.process().value()))
                        .copied()
                        .flatten()
                        == Some((result.event.id, key))
                })
                .map(|link| link.carrier);
        }
        let key = match &result.event.outcome {
            crate::dedicated_tx::TransmitCommandOutcome::HsdpaCompletion(Some(completion)) => {
                completion.transmission.transmission().key
            }
            _ => return None,
        };
        self.links
            .iter()
            .find(|link| link.packets.owns_transmission(key))
            .map(|link| link.carrier)
    }

    pub fn configure_dual_cell(
        &mut self,
        category: umts_types::hsdpa::DualCellCategory,
        secondary_incarnation: NonZeroU64,
        secondary_codes: crate::radio_resources::HsdpaCodeSet,
        secondary_resources: Box<SchedulingResources>,
        feedback: crate::hsdpa_rx::FeedbackPolicy,
    ) -> Result<(), Error> {
        if self.phase() != Phase::Prepared
            || self.links.len() != 1
            || self.links[0].incarnation == secondary_incarnation
        {
            return Err(Error::Phase);
        }
        let parameters = self.procedure.construction().parameters();
        let configuration = self.procedure.construction().configuration();
        let Some(secondary_configuration) = configuration.secondary_cell() else {
            return Err(Error::Identity);
        };
        // Both catalogs must describe the mode and identities signaled to this UE.
        // This scheduler deliberately assigns the same H-RNTI on both cells.
        if !configuration.uses_mac_ehs()
            || parameters.harq.category() != category.per_cell_category()
            || secondary_configuration.h_rnti() != parameters.h_rnti
            || secondary_configuration.qam64() != category.per_cell_limits().qam64
            || !secondary_codes.control_codes().eq(secondary_configuration
                .control()
                .codes()
                .iter()
                .copied())
            || !secondary_resources.matches_parameters(parameters)
        {
            return Err(Error::Identity);
        }
        let memory = parameters.harq.process_memory();
        category
            .validate_process_memory([memory, memory])
            .map_err(|_| Error::Capacity)?;
        let per_cell = category.per_cell_category();
        let primary_packets = PacketOwner::new(
            self.identity.ue,
            self.links[0].incarnation,
            per_cell,
            self.downlink_origin,
            memory,
            parameters.acknowledgement.repetition(),
        )
        .map_err(Error::Packet)?;
        let secondary_packets = PacketOwner::new(
            self.identity.ue,
            secondary_incarnation,
            per_cell,
            self.downlink_origin,
            memory,
            parameters.acknowledgement.repetition(),
        )
        .map_err(Error::Packet)?;
        let primary_rates =
            Rates::new(per_cell, self.links[0].rates.codes()).map_err(Error::Rate)?;
        let rates = Rates::new(per_cell, secondary_codes).map_err(Error::Rate)?;
        self.links[0].packets = primary_packets;
        self.links[0].rates = primary_rates;
        let adaptation = secondary_resources.adaptation(per_cell);
        self.links.push(CellLink {
            carrier: umts_types::hsdpa::CarrierId::Secondary,
            incarnation: secondary_incarnation,
            recycled_blocks: (0..memory.len()).map(|_| None).collect(),
            resolution_cursor: 0,
            feedback_policy: Some(feedback),
            feedback_snapshot: FeedbackSnapshot::default(),
            quality: feedback::Quality::default(),
            adaptation,
            staged_feedback_training: [adaptation::FeedbackTraining::None; 8],
            unusable_cqi_reports: 0,
            rates,
            packets: secondary_packets,
            pending: None,
            process_cursor: 0,
            packet_queue_cursor: 0,
            queue_commands: (0..memory.len()).map(|_| None).collect(),
            scheduling: Some(secondary_resources),
        });
        Ok(())
    }

    pub fn new(
        identity: ReceiveIdentity,
        procedure: Box<ActivationProcedure>,
        feedback: FeedbackPort,
        settings: SessionSettings<'_>,
    ) -> Result<Self, Box<RejectedSession>> {
        let SessionSettings {
            incarnation,
            downlink_origin,
            queues: budget,
            codes,
            scheduling,
        } = settings;
        let build = || {
            if procedure.key() != identity.ue
                || feedback.ue() != identity.ue
                || downlink_origin.epoch != identity.ue.epoch
            {
                return Err(Error::Identity);
            }
            if procedure.phase() != ActivationPhase::Prepared
                || !feedback.available()
                || feedback.outstanding() != 0
            {
                return Err(Error::Phase);
            }
            let parameters = procedure.construction().parameters();
            let queues = parameters.queues.queues();
            let per_queue = NonZeroUsize::new(budget.total_bytes.get() / queues.len())
                .ok_or(Error::Capacity)?;
            let packets = PacketOwner::new(
                identity.ue,
                incarnation,
                parameters.harq.category(),
                downlink_origin,
                parameters.harq.process_memory(),
                parameters.acknowledgement.repetition(),
            )
            .map_err(Error::Packet)?;
            let mut shared_queues = PacketQueues::new(identity.ue);
            for queue in queues {
                let format = if procedure.construction().configuration().uses_mac_ehs() {
                    let logical = procedure
                        .construction()
                        .mappings()
                        .iter()
                        .find(|mapping| mapping.flow == queue.flow())
                        .and_then(|mapping| mapping.logical)
                        .ok_or(Error::Identity)?;
                    QueueFormat::ehs(queue.id(), logical, *queue.sizes())
                } else {
                    QueueFormat::new(queue.id(), *queue.sizes())
                }
                .map_err(Error::Queue)?;
                let queue = PacketQueue::new(format, budget.pdus_per_queue, per_queue)
                    .map_err(Error::Queue)?;
                shared_queues
                    .add_queue(Box::new(queue))
                    .map_err(|_| Error::Capacity)?;
            }
            let rates = Rates::new(parameters.harq.category(), codes).map_err(Error::Rate)?;
            Ok((packets, rates, shared_queues))
        };
        match build() {
            Ok((packets, rates, shared_queues)) => {
                let recycled_blocks = (0..procedure
                    .construction()
                    .parameters()
                    .harq
                    .process_memory()
                    .len())
                    .map(|_| None)
                    .collect();
                let queue_commands = (0..procedure
                    .construction()
                    .parameters()
                    .harq
                    .process_memory()
                    .len())
                    .map(|_| None)
                    .collect();
                let mut session = Self {
                    feedback_mode: None,
                    secondary_control: SecondaryControl::Single,
                    links: vec![CellLink {
                        carrier: umts_types::hsdpa::CarrierId::Primary,
                        incarnation,
                        recycled_blocks,
                        resolution_cursor: 0,
                        feedback_policy: None,
                        feedback_snapshot: feedback::FeedbackSnapshot::default(),
                        quality: feedback::Quality::default(),
                        adaptation: adaptation::LinkAdaptation::disabled(),
                        staged_feedback_training: [adaptation::FeedbackTraining::None; 8],
                        unusable_cqi_reports: 0,
                        rates,
                        packets,
                        pending: None,
                        process_cursor: 0,
                        packet_queue_cursor: 0,
                        queue_commands,
                        scheduling: None,
                    }],
                    downlink_origin,
                    identity,
                    procedure,
                    queues: shared_queues,
                    sdu_budget: sdu_budget::SduBudget::new(downlink_origin),
                    staged_buffer_carriers: [None; 8],
                    scheduling_carrier_cursor: 0,
                    feedback,
                    admission: Admission::Pending,
                    retirement: retirement::Retirement::new(identity.ue),
                    detached_route: false,
                    radio_change: None,
                    queue_cursor: 0,
                    resolution_link_cursor: 0,
                };
                if let Some(admission) = scheduling {
                    let resources = match session.prepare_scheduling_resources(
                        admission.settings,
                        admission.budget,
                        &admission.cell,
                    ) {
                        Ok(resources) => resources,
                        Err(error) => {
                            return Err(Box::new(RejectedSession {
                                error,
                                procedure: session.procedure,
                                feedback: session.feedback,
                            }));
                        }
                    };
                    // No external owner can observe a partially configured new session.
                    session.links[0].scheduling = Some(resources);
                    session.links[0].feedback_policy = Some(admission.feedback);
                }
                Ok(session)
            }
            Err(error) => Err(Box::new(RejectedSession {
                error,
                procedure,
                feedback,
            })),
        }
    }

    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }

    pub(crate) fn new_prepared(
        identity: ReceiveIdentity,
        procedure: Box<ActivationProcedure>,
        feedback: FeedbackPort,
        settings: SessionSettings<'_>,
        resources: Option<(Box<SchedulingResources>, crate::hsdpa_rx::FeedbackPolicy)>,
    ) -> Result<Self, Box<RejectedSession>> {
        let mut session = Self::new(identity, procedure, feedback, settings)?;
        if let Some((resources, policy)) = resources
            && session.configure_scheduler(resources, policy).is_err()
        {
            return Err(Box::new(RejectedSession {
                error: Error::Phase,
                procedure: session.procedure,
                feedback: session.feedback,
            }));
        }
        Ok(session)
    }

    pub fn procedure(&self) -> &ActivationProcedure {
        &self.procedure
    }

    pub fn phase(&self) -> Phase {
        if self.retirement.started() {
            return Phase::Retiring;
        }
        if matches!(self.secondary_control, SecondaryControl::Recovery) {
            return Phase::RecoveryRequired;
        }
        match self.procedure.phase() {
            ActivationPhase::FailureReported | ActivationPhase::RecoveryRequired => {
                Phase::RecoveryRequired
            }
            ActivationPhase::AbortedBeforePublication => Phase::Cancelled,
            _ => match self.admission {
                Admission::Accepted(_) => Phase::Accepted,
                Admission::Committed(_) => Phase::AwaitingCommandAck,
                Admission::Active(_) if self.radio_change.is_some() => Phase::ChangingRadio,
                Admission::Active(_) => Phase::Active,
                Admission::Pending => match self.procedure.phase() {
                    ActivationPhase::Prepared => Phase::Prepared,
                    _ => Phase::AwaitingComplete,
                },
            },
        }
    }

    /// TX codes and the paired feedback receiver must already be admitted before publication.
    pub fn publish(
        &mut self,
        radio: &mut CommonSignalling,
        bank: &mut DedicatedBearers,
        now: ChipInstant,
    ) -> Result<(), Error> {
        if self.phase() != Phase::Prepared || bank.identity() != self.identity {
            return Err(Error::Phase);
        }
        match bank.hsdpa_route(self.identity).map_err(Error::Bearer)? {
            Some(configuration) if Arc::ptr_eq(configuration, self.procedure.construction()) => (),
            Some(_) => return Err(Error::Identity),
            None => bank
                .reserve_hsdpa_route(self.identity, self.procedure.construction().clone())
                .map_err(Error::Bearer)?,
        }
        radio
            .publish_hspa_activation_on(&mut self.procedure, self.identity, now)
            .map_err(Error::Signalling)?;
        Ok(())
    }

    pub fn cancel_before_publication(
        &mut self,
        bank: &mut DedicatedBearers,
        now: ChipInstant,
    ) -> Result<(), Error> {
        if self.procedure.receipt().is_some()
            || !matches!(self.phase(), Phase::Prepared | Phase::Cancelled)
        {
            return Err(Error::Phase);
        }
        let reserved =
            if let Some(configuration) = bank.hsdpa_route(self.identity).map_err(Error::Bearer)? {
                if !Arc::ptr_eq(configuration, self.procedure.construction()) {
                    return Err(Error::Identity);
                }
                true
            } else {
                false
            };
        // Reject invalid procedure time before removing the route. Failed route cleanup remains retryable.
        self.procedure
            .local_failure(now)
            .map_err(Error::Procedure)?;
        if reserved {
            bank.cancel_hsdpa_route(self.identity, now)
                .map_err(Error::Bearer)?;
        }
        self.detached_route = true;
        Ok(())
    }

    pub fn response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
        admit: impl FnOnce(ReconfigurationCounters, &mut BearerCiphers) -> Result<(), SignallingError>,
    ) -> Result<ActivationResponse, SignallingError> {
        if self.retirement.started() {
            return Err(SignallingError::Phase);
        }
        if let Admission::Active(accepted) = &self.admission {
            received.hspa_activation_duplicate(&mut self.procedure, accepted)?;
            return Ok(ActivationResponse::Duplicate);
        }
        let (response, accepted) =
            received.hspa_activation_response_with(&mut self.procedure, security, admit)?;
        if let Some((accepted, ())) = accepted {
            self.admission = Admission::Accepted(Box::new(accepted));
        }
        Ok(response)
    }

    pub fn commit<T, E>(
        &mut self,
        radio: &mut CommonSignalling,
        commit: impl FnOnce(&mut BearerCiphers) -> Result<T, E>,
    ) -> Result<T, HspaCommitError<E>> {
        let Admission::Accepted(accepted) = &self.admission else {
            return Err(HspaCommitError::Signalling(SignallingError::Phase));
        };
        if self.retirement.started() {
            return Err(HspaCommitError::Signalling(SignallingError::Phase));
        }
        let result = radio.commit_hspa_activation_with(accepted, commit)?;
        let Admission::Accepted(accepted) =
            std::mem::replace(&mut self.admission, Admission::Pending)
        else {
            unreachable!("unchanged accepted owner")
        };
        self.admission = Admission::Committed(accepted);
        Ok(result)
    }

    pub fn finish_command(&mut self, radio: &mut CommonSignalling) -> Result<(), SignallingError> {
        let Admission::Committed(accepted) = &self.admission else {
            return Err(SignallingError::Phase);
        };
        if self.retirement.started() {
            return Err(SignallingError::Phase);
        }
        radio.release_hspa_activation_receipt(accepted)?;
        let Admission::Committed(accepted) =
            std::mem::replace(&mut self.admission, Admission::Pending)
        else {
            unreachable!("unchanged committed owner")
        };
        self.admission = Admission::Active(accepted);
        Ok(())
    }

    pub fn expire(
        &mut self,
        radio: &mut CommonSignalling,
        now: ChipInstant,
    ) -> Result<bool, Error> {
        let changed = self.procedure.expire(now).map_err(Error::Procedure)?;
        if self.phase() == Phase::RecoveryRequired
            && matches!(
                self.procedure.phase(),
                ActivationPhase::FailureReported | ActivationPhase::RecoveryRequired
            )
        {
            radio
                .require_hspa_activation_recovery(&self.procedure)
                .map_err(Error::Signalling)?;
        }
        Ok(changed)
    }

    pub(crate) fn active_recovery_identity(&self) -> Option<ReceiveIdentity> {
        (!self.retirement.started()
            && matches!(self.secondary_control, SecondaryControl::Recovery)
            && matches!(self.admission, Admission::Active(_)))
        .then(|| {
            self.radio_change
                .as_ref()
                .map_or(self.identity, |change| change.identity)
        })
    }

    pub(crate) fn recover_active_route(
        &self,
        radio: &mut CommonSignalling,
        bank: &DedicatedBearers,
    ) -> Result<(), Error> {
        let identity = self.active_recovery_identity().ok_or(Error::Phase)?;
        radio
            .require_active_hsdpa_recovery(identity, self.procedure.construction(), bank)
            .map_err(Error::Signalling)
    }

    #[cfg(test)]
    pub fn active_packets(&mut self) -> Result<(&mut PacketOwner, &mut FeedbackPort), Error> {
        if self.phase() != Phase::Active {
            return Err(Error::Phase);
        }
        Ok((&mut self.links[0].packets, &mut self.feedback))
    }

    pub fn poll_feedback(
        &mut self,
        classify: impl FnOnce(&Observation<HarqAck>) -> Feedback,
    ) -> Option<FeedbackStep> {
        let mut report = self.feedback.try_report()?;
        if self.admit_secondary_order_report(&report) {
            return Some(FeedbackStep {
                report,
                admission: None,
                admitted_transmissions: [None; 2],
            });
        }
        // An intervening order can invalidate the codebook schedule captured at queue time, so
        // preserve request identity but admit no stale decoded evidence.
        if let Request::AckCells {
            start,
            repetitions,
            schedule,
        }
        | Request::CqiCells {
            start,
            repetitions,
            schedule,
        } = report.request
            && !schedule.matches_window(self.feedback_mode, start, repetitions)
        {
            report.result = Ok(match report.request {
                Request::AckCells { .. } => {
                    crate::hsdpa_rx::Measurement::AckCells(crate::hsdpa_rx::Evidence::Missing)
                }
                _ => crate::hsdpa_rx::Measurement::CqiCells(crate::hsdpa_rx::Evidence::Missing),
            });
        }
        if matches!(report.request, Request::Ack { .. } | Request::Cqi { .. }) {
            self.links[0].feedback_snapshot.record(report);
        }
        let mut admitted_transmissions = [None; 2];
        let admission = match report.request {
            Request::Ack { transmission, .. } => {
                let packet_id = self.links[0]
                    .packets
                    .payload(transmission.key.process())
                    .ok()
                    .flatten()
                    .and_then(|payload| payload.delivery_trace());
                let mut classification = None;
                let result = self.links[0]
                    .packets
                    .admit_feedback_report(&report, |observation| {
                        let value = classify(observation);
                        classification = Some(value);
                        value
                    });
                self.links[0]
                    .feedback_snapshot
                    .outcomes
                    .record(result.is_ok(), classification);
                if crate::hsdpa_packet::delivery_trace::event_enabled(packet_id) {
                    tracing::debug!(target: "umts_hnb::delivery_trace",
                        packet_id, transmission = ?transmission, classification = ?classification,
                        admitted = result.is_ok(), report = ?report,
                        "Downlink delivery feedback");
                }
                if result.is_ok() {
                    admitted_transmissions[0] = Some(transmission);
                    let link = &mut self.links[0];
                    link.feedback_snapshot.admit_with_adaptation(
                        &mut link.adaptation,
                        transmission,
                        classification,
                        report.processed_until,
                    );
                }
                Some(result)
            }
            Request::AckCells { .. } => {
                let mut admitted = None;
                for link in &mut self.links {
                    let Some(policy) = link.feedback_policy else {
                        continue;
                    };
                    let Some(result) =
                        link.packets
                            .admit_feedback_cells(&report, link.carrier, &policy)
                    else {
                        continue;
                    };
                    link.feedback_snapshot.record(report);
                    match result {
                        Ok((transmission, feedback)) => {
                            admitted_transmissions[link.carrier.index()] = Some(transmission);
                            link.feedback_snapshot.outcomes.record(true, feedback);
                            link.feedback_snapshot.admit_with_adaptation(
                                &mut link.adaptation,
                                transmission,
                                feedback,
                                report.processed_until,
                            );
                            feedback::record_admission(&mut admitted, Ok(()));
                        }
                        Err(error) => {
                            link.feedback_snapshot.outcomes.record(false, None);
                            feedback::record_admission(&mut admitted, Err(error));
                        }
                    }
                }
                admitted
            }
            Request::Cqi { .. } | Request::CqiCells { .. } | Request::OrderAck { .. } => None,
        };
        Some(FeedbackStep {
            report,
            admission,
            admitted_transmissions,
        })
    }

    pub fn begin_retirement(&mut self) {
        self.retirement.begin();
        for link in &mut self.links {
            link.packets.begin_retirement();
        }
        self.queues.begin_retirement();
    }

    pub(crate) fn detached_route(&self) -> bool {
        self.detached_route
    }

    #[cfg(test)]
    pub fn retiring_packets(&mut self) -> Result<&mut PacketOwner, Error> {
        if !self.retirement.started() {
            return Err(Error::Phase);
        }
        Ok(&mut self.links[0].packets)
    }

    pub fn service_retirement(
        &mut self,
        control: &mut crate::hsdpa_packet::ControlPort<'_>,
    ) -> Result<RetirementProgress, crate::dedicated_tx::TransmitError> {
        self.retirement.service(control)
    }

    pub fn retirement_result(
        &mut self,
        result: &crate::hsdpa_packet::CommandResult,
    ) -> Result<RetirementProgress, RetirementError> {
        self.retirement.result(result)
    }

    pub fn complete_transmit_on(
        &mut self,
        carrier: umts_types::hsdpa::CarrierId,
        completion: crate::hsdpa_tx::CompletedTransmission,
    ) -> Result<crate::hsdpa_tx::WaveformBuffer, crate::hsdpa_packet::RejectedCompletion> {
        self.links[carrier.index()]
            .packets
            .complete_transmit(completion)
    }

    pub fn try_retire(mut self: Box<Self>) -> Result<RetiredSession, Box<Self>> {
        if !self.retirement.complete()
            || self.links.iter().any(|link| {
                link.pending.is_some()
                    || link.queue_commands.iter().any(Option::is_some)
                    || !link.packets.retirement_ready()
            })
            || (self.feedback.available() && self.feedback.outstanding() != 0)
        {
            return Err(self);
        }
        if self.queues.cancel_all_staged().is_err() {
            return Err(self);
        }
        let mut owner = *self;
        let primary = owner.links.remove(0);
        let packets = match primary.packets.try_retire() {
            Ok(packets) => packets,
            Err(_) => unreachable!("checked packet retirement without intervening mutation"),
        };
        let accepted = match owner.admission {
            Admission::Pending => None,
            Admission::Accepted(token) | Admission::Committed(token) | Admission::Active(token) => {
                Some(token)
            }
        };
        Ok(RetiredSession {
            scheduling: primary.scheduling,
            recycled_blocks: primary.recycled_blocks,
            identity: owner.identity,
            procedure: owner.procedure,
            accepted,
            packets,
            queues: owner.queues,
            additional_links: owner.links,
            feedback: owner.feedback,
        })
    }
}

mod secondary_order;
pub use secondary_order::{OrderProgress, OrderTransmission, SecondaryState};

const SECONDARY_ORDER_ATTEMPTS: u8 = 3;
const SECONDARY_ORDER_TIMEOUT_CHIPS: u64 = 30 * umts_types::time::CHIPS_PER_SLOT;
const SECONDARY_UNUSABLE_CQI_REPORTS: u8 = 5;
const SECONDARY_CQI_AGE_CHIPS: u64 = 300 * umts_types::time::CHIPS_PER_SLOT;

enum SecondaryControl {
    Single,
    Recovery,
    Dual {
        origin: ChipInstant,
        procedure: Box<secondary_order::SecondaryOrderProcedure>,
        feedback: Option<(crate::hsdpa_rx::RequestId, OrderTransmission)>,
    },
}

impl Session {
    pub fn configure_secondary_control(
        &mut self,
        feedback_origin: ChipInstant,
    ) -> Result<(), Error> {
        if !self.has_carrier(umts_types::hsdpa::CarrierId::Secondary) {
            return Err(Error::Phase);
        }
        if matches!(self.secondary_control, SecondaryControl::Dual { .. }) {
            return Ok(());
        }
        if matches!(self.secondary_control, SecondaryControl::Recovery) {
            return Err(Error::Phase);
        }
        let activation = self.procedure.construction().activation().time();
        let procedure = secondary_order::SecondaryOrderProcedure::configured_by_rrc(
            activation,
            feedback_origin,
            SECONDARY_ORDER_ATTEMPTS,
            SECONDARY_ORDER_TIMEOUT_CHIPS,
        )
        .map_err(Error::Timing)?;
        self.feedback_mode = Some(crate::hsdpa_rx::FeedbackModeSchedule::fixed(
            feedback_origin,
            crate::hsdpa_rx::FeedbackMode::Dual,
        ));
        self.secondary_control = SecondaryControl::Dual {
            origin: feedback_origin,
            procedure: Box::new(procedure),
            feedback: None,
        };
        Ok(())
    }

    pub fn secondary_state(&self) -> Option<SecondaryState> {
        match &self.secondary_control {
            SecondaryControl::Single | SecondaryControl::Recovery => None,
            SecondaryControl::Dual { procedure, .. } => Some(procedure.state()),
        }
    }

    pub fn request_secondary_order(
        &mut self,
        order: umts_dsp::hsdpa::SecondaryCellOrder,
    ) -> Result<(), Error> {
        let SecondaryControl::Dual { procedure, .. } = &mut self.secondary_control else {
            return Err(Error::Phase);
        };
        procedure.request(order).map_err(Error::Timing)
    }

    pub fn prepare_secondary_order(
        &self,
        scch_start: ChipInstant,
        feedback_start: ChipInstant,
    ) -> Result<OrderTransmission, Error> {
        let SecondaryControl::Dual { procedure, .. } = &self.secondary_control else {
            return Err(Error::Phase);
        };
        procedure
            .prepare(scch_start, feedback_start)
            .map_err(Error::Timing)
    }

    pub fn accept_secondary_order(&mut self, tx: OrderTransmission) -> Result<(), Error> {
        let SecondaryControl::Dual {
            origin, procedure, ..
        } = &mut self.secondary_control
        else {
            return Err(Error::Phase);
        };
        let initial = match tx.order {
            umts_dsp::hsdpa::SecondaryCellOrder::Activate => crate::hsdpa_rx::FeedbackMode::Single,
            umts_dsp::hsdpa::SecondaryCellOrder::Deactivate => crate::hsdpa_rx::FeedbackMode::Dual,
        };
        let initial = if tx.ordinal > 1 {
            crate::hsdpa_rx::FeedbackMode::Ambiguous
        } else {
            initial
        };
        let schedule = crate::hsdpa_rx::FeedbackModeSchedule::change_at(
            *origin,
            initial,
            tx.codebook_effective,
            crate::hsdpa_rx::FeedbackMode::Ambiguous,
        )
        .map_err(Error::Timing)?;
        procedure.accepted(tx).map_err(Error::Timing)?;
        if tx.order == umts_dsp::hsdpa::SecondaryCellOrder::Activate {
            // Start a new quality trial only after radio acceptance. Keep a
            // timestamp floor so delayed CQI from the previous trial cannot revive it.
            let secondary = &mut self.links[umts_types::hsdpa::CarrierId::Secondary.index()];
            secondary.unusable_cqi_reports = 0;
            secondary.quality.begin_trial(tx.codebook_effective);
        }
        self.feedback_mode = Some(schedule);
        Ok(())
    }

    pub fn request_secondary_order_feedback(
        &mut self,
        tx: OrderTransmission,
    ) -> Result<crate::hsdpa_rx::RequestId, Error> {
        let SecondaryControl::Dual { feedback, .. } = &mut self.secondary_control else {
            return Err(Error::Phase);
        };
        if let Some((id, previous)) = feedback {
            return if *previous == tx {
                Ok(*id)
            } else {
                Err(Error::Phase)
            };
        }
        let schedule = self.feedback_mode.ok_or(Error::Phase)?;
        if !self.feedback.available() {
            return Err(Error::Phase);
        }
        let id = self
            .feedback
            .request(Request::OrderAck {
                start: tx.feedback_start,
                schedule,
            })
            .map_err(|_| Error::Capacity)?;
        *feedback = Some((id, tx));
        Ok(id)
    }

    pub fn advance_secondary_order(&mut self, now: ChipInstant) -> Result<OrderProgress, Error> {
        let progress = match &mut self.secondary_control {
            SecondaryControl::Single => OrderProgress::Idle,
            SecondaryControl::Recovery => OrderProgress::Exhausted,
            SecondaryControl::Dual { procedure, .. } => {
                procedure.advance(now).map_err(Error::Timing)?
            }
        };
        if progress == OrderProgress::Exhausted {
            self.secondary_control = SecondaryControl::Recovery;
        }
        Ok(progress)
    }

    fn admit_secondary_order_report(&mut self, report: &Report) -> bool {
        use crate::hsdpa_rx::{Evidence, FeedbackMode, FeedbackModeSchedule, Measurement};
        let SecondaryControl::Dual {
            origin,
            procedure,
            feedback,
        } = &mut self.secondary_control
        else {
            return false;
        };
        let Some((id, tx)) = *feedback else {
            return false;
        };
        if id != report.id {
            return false;
        }
        *feedback = None;
        let Request::OrderAck { start, .. } = report.request else {
            return true;
        };
        if start != tx.feedback_start {
            return true;
        }
        let Ok(Measurement::AckCells(Evidence::Measured(observation))) = report.result else {
            return true;
        };
        let Some(policy) = self.links[0].feedback_policy else {
            return true;
        };
        if policy.classify_ack_cells(&observation)[0] != Feedback::Ack {
            return true;
        }
        if !procedure.acknowledge(tx, start) {
            return true;
        }
        let (initial, next) = match tx.order {
            umts_dsp::hsdpa::SecondaryCellOrder::Activate => {
                (FeedbackMode::Single, FeedbackMode::Dual)
            }
            umts_dsp::hsdpa::SecondaryCellOrder::Deactivate => {
                (FeedbackMode::Dual, FeedbackMode::Single)
            }
        };
        let initial = if tx.ordinal > 1 {
            FeedbackMode::Ambiguous
        } else {
            initial
        };
        if let Ok(schedule) =
            FeedbackModeSchedule::change_at(*origin, initial, tx.codebook_effective, next)
        {
            self.feedback_mode = Some(schedule);
        }
        true
    }
}

impl Session {
    pub fn order_progress(&self) -> OrderProgress {
        match &self.secondary_control {
            SecondaryControl::Single => OrderProgress::Idle,
            SecondaryControl::Recovery => OrderProgress::Exhausted,
            SecondaryControl::Dual { procedure, .. } => procedure.progress(),
        }
    }

    pub fn h_rnti(&self) -> u16 {
        self.procedure.construction().parameters().h_rnti
    }

    pub fn order_control_amplitude(&self) -> Option<f32> {
        self.links[0]
            .scheduling
            .as_ref()
            .map(|resources| resources.order_control_amplitude())
    }

    pub fn order_feedback_time(
        &self,
        scch_start: ChipInstant,
        timing: FeedbackTiming,
    ) -> Result<ChipInstant, Error> {
        timing
            .acknowledgement(ChipInstant {
                index: scch_start
                    .index
                    .checked_add(crate::hsdpa_tx::CONTROL_LEAD_CHIPS)
                    .ok_or(Error::Identity)?,
                ..scch_start
            })
            .map_err(Error::Timing)
    }

    pub fn next_order_target(
        &self,
        now: ChipInstant,
    ) -> Option<umts_dsp::hsdpa::SecondaryCellOrder> {
        if self.phase() != Phase::Active {
            return None;
        }
        self.secondary_quality_target(now)
    }

    fn secondary_quality_target(
        &self,
        now: ChipInstant,
    ) -> Option<umts_dsp::hsdpa::SecondaryCellOrder> {
        use umts_dsp::hsdpa::SecondaryCellOrder;
        if self.order_progress() != OrderProgress::Idle {
            return None;
        }
        let link = self
            .links
            .get(umts_types::hsdpa::CarrierId::Secondary.index())?;
        let retained = link.pending.is_some()
            || (0..link.queue_commands.len()).any(|index| {
                link.packets
                    .payload(
                        umts_mac::hsdpa::ProcessId::new(index as u8).expect("admitted process"),
                    )
                    .is_ok_and(|payload| payload.is_some())
            });
        match self.secondary_state()? {
            SecondaryState::Configured
                if matches!(&self.secondary_control, SecondaryControl::Dual { procedure, .. }
                    if procedure.automatic_reactivation_allowed(now))
                    && (retained
                        || self
                            .procedure
                            .construction()
                            .parameters()
                            .queues
                            .queues()
                            .iter()
                            .any(|queue| self.queues.queued(queue.id()) != 0)) =>
            {
                Some(SecondaryCellOrder::Activate)
            }
            SecondaryState::Active
                if !retained
                    && link.unusable_cqi_reports >= SECONDARY_UNUSABLE_CQI_REPORTS
                    && link
                        .quality
                        .value(
                            now,
                            NonZeroU64::new(SECONDARY_CQI_AGE_CHIPS).expect("bounded CQI age"),
                        )
                        .is_some_and(|cqi| cqi.value() == 0) =>
            {
                Some(SecondaryCellOrder::Deactivate)
            }
            _ => None,
        }
    }

    pub fn secondary_order_not_transmitted(&mut self, _tx: OrderTransmission) {
        if matches!(
            self.order_progress(),
            OrderProgress::Ready | OrderProgress::Waiting | OrderProgress::Exhausted
        ) {
            self.secondary_control = SecondaryControl::Recovery;
        }
    }
}
