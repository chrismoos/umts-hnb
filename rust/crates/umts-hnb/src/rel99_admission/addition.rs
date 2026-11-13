// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    dedicated_bearers::{BearerEndpoint, MappingActivation},
    dedicated_rx::{DedicatedSampleReceiver, ReceiveCommandResult, ReceiveCommandTicket},
    radio_resources::{PreparedRadioChange, RadioResources, RetiredRadioCodes},
    signalling::AcceptedAdditionalSetup,
};

mod assignment;
mod delivery;
mod host;
mod media;
mod runtime;
mod workers;
pub use assignment::{
    AdditionalAssignment, AdditionalMediaError, AdditionalMediaServices, RetainedCircuitServices,
};
pub use delivery::{AdditionalDelivery, AdditionalLaunch};
pub use host::{AdditionalRabHost, RejectedAdditionalAssignment};
pub use media::{AdditionCommitServices, AdditionMediaError};
pub(super) use runtime::step_addition;
pub(super) use workers::poll_transmit;
use workers::{ReceiveProgress, TransmitProgress};

pub(super) struct CompletedAddition {
    procedure: Box<RadioBearerSetupProcedure>,
    accepted: AcceptedAdditionalSetup,
}

impl ScheduledAddition {
    pub(super) fn uplink_handoff_pending(&self, identity: ReceiveIdentity) -> bool {
        self.identity == identity
            && matches!(
                self.phase,
                AdditionPhase::Switching
                    | AdditionPhase::AwaitingComplete
                    | AdditionPhase::ReadyToCommit
            )
    }
}

pub struct ScheduledAdditionRequest {
    pub receiver: Box<DedicatedSampleReceiver>,
    pub transmit: Box<DedicatedTransmitSession>,
    pub procedure: Box<RadioBearerSetupProcedure>,
    pub pool: FramePool,
    pub activation: MappingActivation,
    pub endpoints: Vec<BearerEndpoint>,
}

#[must_use = "Recover the candidate owners and original radio change"]
pub struct RejectedScheduledAddition {
    pub error: Box<AdmissionError>,
    pub request: Box<ScheduledAdditionRequest>,
    pub change: PreparedRadioChange,
}
impl std::fmt::Debug for RejectedScheduledAddition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedScheduledAddition")
            .field("error", &self.error)
            .field("change", &self.change)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdditionPhase {
    Preparing,
    Ready,
    Switching,
    AwaitingComplete,
    ReadyToCommit,
    Committed,
    RecoveryRequired,
}

enum ResourceHandoff {
    Prepared(PreparedRadioChange),
    Retiring(RetiredRadioCodes),
    Retired,
}

pub(super) struct ScheduledAddition {
    delivery: Option<AdditionalDelivery>,
    pub(super) assignment: Option<AdditionalAssignment>,
    identity: ReceiveIdentity,
    pub(super) phase: AdditionPhase,
    procedure: Box<RadioBearerSetupProcedure>,
    activation: MappingActivation,
    receive: ReceiveProgress,
    transmit: TransmitProgress,
    pool: Option<FramePool>,
    frame: Option<FrameInput>,
    next_frame: ChipInstant,
    idle: Tfci,
    prepared_frames: usize,
    accepted: Option<AcceptedAdditionalSetup>,
    resources: ResourceHandoff,
    previous_pool: Option<FramePool>,
    receipt_released: bool,
    failure: Option<AdmissionError>,
    unclaimed_receive: Option<ReceiveCommandResult>,
}

impl ScheduledAddition {
    pub(super) fn fail(&mut self, error: AdmissionError) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        self.phase = AdditionPhase::RecoveryRequired;
    }
}

#[cfg(test)]
pub(super) fn quarantined_custody(entry: &Entry) -> (usize, usize, bool) {
    entry
        .lifecycle
        .addition()
        .map_or((0, 0, false), |addition| {
            (
                usize::from(addition.unclaimed_receive.is_some())
                    + usize::from(matches!(addition.receive, ReceiveProgress::Retired(_))),
                usize::from(matches!(addition.receive, ReceiveProgress::Waiting { .. })),
                matches!(addition.resources, ResourceHandoff::Prepared(_)),
            )
        })
}
pub(super) fn extract_failed(
    entry: &mut Entry,
) -> Result<Option<super::recovery::JoinedRecovery>, AdmissionError> {
    use super::recovery::{Candidate, FailedProcedure, JoinedRecovery, Kind, Mode};
    let Some(addition) = entry.lifecycle.addition() else {
        return Ok(None);
    };
    if (addition.phase != AdditionPhase::RecoveryRequired && entry.failure.is_none())
        || entry.pending.is_some()
        || matches!(addition.receive, ReceiveProgress::Waiting { .. })
    {
        return Ok(None);
    }
    if addition.unclaimed_receive.is_some() {
        return Err(AdmissionError::Protocol);
    }
    let promoted = entry.identity == addition.identity && addition.accepted.is_some();
    if promoted
        && (!matches!(addition.receive, ReceiveProgress::Retired(_))
            || !matches!(addition.transmit, TransmitProgress::Retired(_)))
    {
        return Err(AdmissionError::Phase);
    }
    if promoted
        && !matches!(
            addition.resources,
            ResourceHandoff::Retiring(_) | ResourceHandoff::Retired
        )
        || !promoted && !matches!(addition.resources, ResourceHandoff::Prepared(_))
    {
        return Err(AdmissionError::Phase);
    }
    let mut addition = entry.lifecycle.take_addition().expect("failed addition");
    if promoted {
        let ReceiveProgress::Retired(received) = addition.receive else {
            unreachable!("validated retired receiver")
        };
        let TransmitProgress::Retired(transmit) = addition.transmit else {
            unreachable!("validated retired transmitter")
        };
        let TransmitCommandOutcome::Retired {
            identity: retired_identity,
            result: Ok(transmit),
        } = *transmit
        else {
            unreachable!("successful retirement")
        };
        if let Some(assignment) = addition.assignment.take() {
            entry.additional_assignments.push(assignment)
        }
        let retired_codes = match addition.resources {
            ResourceHandoff::Retiring(proof) => Some(proof),
            ResourceHandoff::Retired => None,
            _ => unreachable!("promoted resource owner"),
        };
        return Ok(Some(JoinedRecovery::new(Candidate {
            previous: entry.identity,
            identity: retired_identity,
            mode: Mode::CurrentBankTeardown,
            receiver: None,
            returned_receiver: Some(*received),
            original_receiver: None,
            transmitter: Some(transmit.session),
            pool: addition.previous_pool.take(),
            frame: None,
            change: None,
            retired_codes,
            release: None,
            pending: None,
            procedure: FailedProcedure::Addition(addition.procedure),
            kind: Kind::Addition,
            error: addition
                .failure
                .or_else(|| entry.failure.take())
                .unwrap_or(AdmissionError::Protocol),
        })));
    }
    let mode = if entry.failure.is_none()
        && addition.procedure.receipt().is_none()
        && !addition.receive_scheduled()
        && !addition.transmit_scheduled()
    {
        Mode::Unpublished
    } else {
        Mode::Published
    };
    let (receiver, original_receiver) = match addition.receive {
        ReceiveProgress::Local(receiver) => (Some(receiver), None),
        ReceiveProgress::Retired(result) => (None, Some(*result)),
        _ => (None, None),
    };
    let transmitter = match addition.transmit {
        TransmitProgress::Local(session) => Some(session),
        TransmitProgress::Retired(result) => {
            if let TransmitCommandOutcome::Retired {
                result: Ok(retired),
                ..
            } = *result
            {
                entry.transmit = TransmitOwnership::Returned(retired.session);
            } else {
                unreachable!("only successful retirement enters returned state")
            }
            None
        }
        _ => None,
    };
    let ResourceHandoff::Prepared(change) = addition.resources else {
        unreachable!("validated prepared resource owner")
    };
    let pending = addition
        .assignment
        .take()
        .map(AdditionalAssignment::into_pending);
    Ok(Some(JoinedRecovery::new(Candidate {
        previous: entry.identity,
        identity: addition.identity,
        mode,
        receiver,
        returned_receiver: None,
        original_receiver,
        transmitter,
        pool: addition.pool.take(),
        frame: addition.frame.take(),
        change: Some(change),
        retired_codes: None,
        release: None,
        pending,
        procedure: FailedProcedure::Addition(addition.procedure),
        kind: Kind::Addition,
        error: addition.failure.unwrap_or(AdmissionError::Protocol),
    })))
}

impl Rel99Admission {
    #[cfg(test)]
    pub fn addition_failure(&self, previous: ReceiveIdentity) -> Option<&AdmissionError> {
        self.entry(previous)?.lifecycle.addition()?.failure.as_ref()
    }
    pub fn addition_phase(&self, previous: ReceiveIdentity) -> Option<AdditionPhase> {
        Some(self.entry(previous)?.lifecycle.addition()?.phase)
    }

    #[cfg(test)]
    pub fn addition_procedure(
        &self,
        previous: ReceiveIdentity,
    ) -> Option<&RadioBearerSetupProcedure> {
        Some(&self.entry(previous)?.lifecycle.addition()?.procedure)
    }

    pub fn begin_addition(
        &mut self,
        previous: ReceiveIdentity,
        mut request: Box<ScheduledAdditionRequest>,
        change: PreparedRadioChange,
        radio: &CommonSignalling,
        receive: &mut DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedScheduledAddition> {
        let check = (|| {
            let active = self.active_radio(previous)?;
            let entry = self.entry(previous).ok_or(AdmissionError::Identity)?;
            let identity = request.pool.identity();
            let plan = request.pool.plan();
            if !matches!(entry.transmit, TransmitOwnership::Armed)
                || request.procedure.phase() != SetupPhase::Prepared
                || request.transmit.phase() != TransmitPhase::Prepared
                || request.transmit.queued_frames() != 0
            {
                return Err(AdmissionError::Phase);
            }
            if change.receive_identity() != identity
                || identity.ue != previous.ue
                || identity.configuration <= previous.configuration
                || request.receiver.session().identity() != identity
                || request.transmit.identity() != identity
                || request.procedure.key() != identity.ue
                || radio.dedicated_route(previous.ue) != Some(previous)
                || !radio.owns_frame_pool(&request.pool)
                || !request.pool.bound_to(&self.port)
                || !Arc::ptr_eq(change.plan(), plan)
                || request
                    .procedure
                    .previous()
                    .is_none_or(|old| !Arc::ptr_eq(old, active.plan().configuration()))
                || !Arc::ptr_eq(request.procedure.configuration(), plan.configuration())
                || !Arc::ptr_eq(request.transmit.plan(), plan)
                || !Arc::ptr_eq(
                    request.receiver.session().configured(),
                    plan.uplink_configured(),
                )
            {
                return Err(AdmissionError::Identity);
            }
            let tx = request.transmit.frame_anchor();
            let rx = request.receiver.session().frame_anchor();
            let activation = request
                .procedure
                .activation()
                .ok_or(AdmissionError::Configuration)?;
            if request.activation.downlink.time != tx.time
                || request.activation.downlink.cfn != tx.cfn
                || request.activation.uplink.time != rx.time
                || request.activation.uplink.cfn != rx.cfn
                || activation.time() != tx.time
                || activation.cfn() != tx.cfn
                || entry.next_frame.epoch != tx.time.epoch
                || entry.next_frame.index > tx.time.index
                || entry.frame.as_ref().is_some_and(|frame| {
                    frame
                        .start
                        .index
                        .checked_add(CHIPS_PER_FRAME)
                        .is_none_or(|end| end > tx.time.index)
                })
            {
                return Err(AdmissionError::Time);
            }
            let longest = plan
                .downlink_configured()
                .transport_ids()
                .map(|id| {
                    plan.downlink_configured()
                        .definition(id)
                        .expect("configured transport")
                        .interval
                        .frames()
                })
                .max()
                .ok_or(AdmissionError::Configuration)?;
            if self.limits.prefill_frames.get() < longest {
                return Err(AdmissionError::Configuration);
            }
            let idle = plan
                .downlink_configured()
                .combinations()
                .iter()
                .find(|row| {
                    plan.downlink_configured()
                        .transport_ids()
                        .zip(&row.formats)
                        .all(|(id, &format)| {
                            let block = plan
                                .downlink_configured()
                                .definition(id)
                                .expect("configured transport")
                                .formats[format];
                            block.blocks == 0 || block.block_bits == 0
                        })
                })
                .map(|row| row.tfci)
                .ok_or(AdmissionError::Configuration)?;
            Ok((identity, idle))
        })();
        let (identity, idle) = match check {
            Ok(value) => value,
            Err(error) => {
                return Err(RejectedScheduledAddition {
                    error: Box::new(error),
                    request,
                    change,
                });
            }
        };
        let prepared = match receive.prepare_addition(
            previous,
            identity,
            request.activation,
            request.pool.plan().clone(),
            std::mem::take(&mut request.endpoints),
        ) {
            Ok(prepared) => prepared,
            Err(rejected) => {
                request.endpoints = rejected.endpoints;
                return Err(RejectedScheduledAddition {
                    error: Box::new(AdmissionError::Receive(rejected.error)),
                    request,
                    change,
                });
            }
        };
        if let Err(rejected) = receive.stage_addition(previous, prepared) {
            request.endpoints = rejected.prepared.into_endpoints();
            return Err(RejectedScheduledAddition {
                error: Box::new(AdmissionError::Receive(DedicatedControlError::Bearer(
                    rejected.error,
                ))),
                request,
                change,
            });
        }
        let ScheduledAdditionRequest {
            receiver,
            transmit,
            procedure,
            pool,
            activation,
            ..
        } = *request;
        let physical = pool.plan().configuration().physical();
        let uplink = pool.plan().configuration().uplink();
        tracing::debug!(
            "Dedicated addition prepared: identity={identity:?} activation={activation:?} uplink_scrambling={} uplink_transports={:?} uplink_ctfc={:?} uplink_combinations={:?} downlink_transports={:?} downlink_ctfc={:?}",
            physical.uplink_scrambling,
            uplink.transports(),
            uplink.ctfc_values(),
            uplink.combinations(),
            pool.plan().configuration().downlink().transports(),
            pool.plan().configuration().downlink().ctfc_values(),
        );
        tracing::debug!(
            "Dedicated addition setup encoding: identity={identity:?} bits={} bytes={:02x?}",
            procedure.encoded().bit_length,
            procedure.encoded().bytes,
        );
        *self
            .entry_mut(previous)
            .expect("validated active owner")
            .lifecycle
            .transition_mut()
            .expect("active bearer change owner") =
            Transition::Addition(Box::new(ScheduledAddition {
                delivery: None,
                assignment: None,
                identity,
                phase: AdditionPhase::Preparing,
                procedure,
                activation,
                receive: ReceiveProgress::Local(receiver),
                transmit: TransmitProgress::Local(transmit),
                pool: Some(pool),
                frame: None,
                next_frame: activation.downlink.time,
                idle,
                prepared_frames: 0,
                accepted: None,
                resources: ResourceHandoff::Prepared(change),
                previous_pool: None,
                receipt_released: false,
                failure: None,
                unclaimed_receive: None,
            }));
        Ok(identity)
    }

    pub(super) fn addition_response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
        receive: &mut DedicatedController,
    ) -> Option<Result<(), SignallingError>> {
        let entry = self
            .entries
            .get_mut(received.key().slot as usize)?
            .as_mut()
            .filter(|entry| entry.identity.ue == received.key())?;
        if let Some(history) = entry.addition_history.as_mut()
            && received
                .additional_setup_duplicate(&mut history.procedure, &history.accepted)
                .is_ok()
        {
            return Some(Ok(()));
        }
        let addition = entry.lifecycle.addition_mut()?;
        if matches!(
            addition.phase,
            AdditionPhase::ReadyToCommit | AdditionPhase::Committed
        ) {
            return Some(
                received.additional_setup_duplicate(
                    &mut addition.procedure,
                    addition
                        .accepted
                        .as_ref()
                        .expect("accepted additional setup"),
                ),
            );
        }
        if !matches!(
            addition.phase,
            AdditionPhase::Switching | AdditionPhase::AwaitingComplete
        ) {
            return Some(Err(SignallingError::Phase));
        }
        // TS 25.331 8.2.2.9 permits rejection on the prior configuration before activation.
        if addition.phase == AdditionPhase::Switching
            && matches!(
                received.message().value.message,
                umts_rrc_schema::class_definitions::ULDCCHMessageType::radioBearerSetupComplete(_)
            )
        {
            return Some(Err(SignallingError::Capacity));
        }
        Some((|| {
            let contexts = [
                security.user_cipher(umts_types::CnDomain::Cs)?,
                security.user_cipher(umts_types::CnDomain::Ps)?,
            ];
            let (response, accepted) = received.additional_setup_response_with(
                &mut addition.procedure,
                security,
                |accepted, ciphers| {
                    receive
                        .with_active_mapping(addition.identity, |bank| {
                            bank.install_additional_users(
                                accepted,
                                contexts,
                                ciphers,
                                received.time().serviced(),
                            )
                        })
                        .map_err(SignallingError::DedicatedControl)?
                        .map_err(SignallingError::UserBearer)
                },
            )?;
            match response {
                SetupResponse::Complete => {
                    addition.accepted = accepted.map(|(accepted, ())| accepted)
                }
                SetupResponse::Failure => addition.fail(AdmissionError::Protocol),
                SetupResponse::Duplicate => (),
            }
            Ok(())
        })())
    }
}
