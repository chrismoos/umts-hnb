// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    common_tx::{CommonTxPort, CommonTxTarget},
    dedicated_bearers::{BearerError, BearerPhase, DownlinkFrame, FrameBuildError, FramePool},
    dedicated_controller::{
        DedicatedControlError, DedicatedController, DedicatedPhase, PreparedDedicated,
        RetiredDedicated,
    },
    dedicated_rx::{ReceiveIdentity, ReceiveOwner},
    dedicated_tx::{
        DedicatedTransmitPort, DedicatedTransmitSession, FrameInput, TransmitCommandId,
        TransmitCommandOutcome, TransmitError, TransmitEvent, TransmitPhase,
    },
    signalling::{CommonSignalling, ControlSecurity, ReceivedControl, SignallingError},
    tx_evidence::TransportSubmission,
};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
};
use umts_dsp::tfci::Tfci;
use umts_rlc::acknowledged::SduTransmission;
use umts_rrc::dedicated::{DedicatedError, RadioBearerSetupProcedure, SetupPhase, SetupResponse};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_FRAME, ChipInstant},
};

mod addition;
mod circuits;
mod delivery;
pub(crate) mod hsdpa;
mod hsupa;
pub use hsdpa::ActivationRequest;
mod inventory;
mod lifecycle;
use lifecycle::Lifecycle;
mod packets;
mod rab_owner_visit;
mod rabs;
mod recovery;
mod release;
pub use recovery::{
    FailedBearerChange, FailedChangeError, FailedChangeRequest, QuarantinedAdmission,
};
#[cfg(test)]
pub(crate) use release::TestReleaseServices;
mod runtime;
mod setup;
mod traffic;
mod transition_frames;
pub use addition::{
    AdditionCommitServices, AdditionMediaError, AdditionPhase, AdditionalAssignment,
    AdditionalDelivery, AdditionalLaunch, AdditionalMediaError, AdditionalMediaServices,
    AdditionalRabHost, RejectedAdditionalAssignment, RetainedCircuitServices,
    ScheduledAdditionRequest,
};
#[cfg(test)]
pub use circuits::CircuitTime;
pub use circuits::{
    CircuitAssignment, CircuitInitialization, CircuitPlayoutTime, CircuitProgress, CircuitSettings,
    InitialCircuitError,
};
pub use inventory::{ActiveRab, InventoryError, RabOutcomeIds};
pub use packets::{InitialPacketError, PacketAssignment, PacketProgress};
pub use rabs::{InitialRabHost, PreparedRabAssignment, RabAdmissionError, RejectedRabAssignment};
#[cfg(test)]
pub use release::ReleaseOutcomeProgress;
pub(crate) use release::RetainedReleaseBuild;
pub use release::{
    CoreReleaseError, MediaReleaseError, RabReleaseRequest, RejectedRabRelease,
    RejectedReleaseAssignment, ReleaseAdmissionError, ReleaseDelivery, ReleaseFinish,
    ReleaseLaunchResult, ReleaseRabHost, ReleaseRollbackError, ReleaseServices,
    ScheduledReleasePhase, ScheduledReleaseRequest, SetupAfterRelease,
};
use runtime::fail;
#[cfg(test)]
pub(crate) use traffic::{CallbackDuplexPolicy, ClosurePolicy};
pub use traffic::{DuplexFramePolicy, FramePolicy, TrafficFramePolicy};

const MAX_PREFILL_FRAMES: usize = 16;

#[derive(Clone, Copy)]
pub struct AdmissionLimits {
    pub visits: NonZeroUsize,
    pub command_results: NonZeroUsize,
    pub prefill_frames: NonZeroUsize,
    pub maximum_lead_frames: NonZeroUsize,
    pub health_interval_chips: NonZeroU64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionPhase {
    Preparing,
    ArmingReceive,
    Prefilling,
    ArmingTransmit,
    Starting,
    AwaitingComplete,
    AwaitingCommandAck,
    Active,
    Stopping,
    Retired,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparationDeadline {
    pub first_frame: ChipInstant,
    pub transmit_cursor: ChipInstant,
    pub observed_at: ChipInstant,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Configuration,
    Identity,
    Phase,
    Time,
    PreparationDeadline(Box<PreparationDeadline>),
    Capacity,
    Closed,
    Parked,
    Protocol,
    Cancelled,
    Receive(DedicatedControlError),
    Transmit(TransmitError),
    Frame(FrameBuildError),
    Signalling(SignallingError),
    Procedure(DedicatedError),
    Resources(crate::radio_resources::Error),
}
pub struct InitialAdmission {
    pub receive: Box<PreparedDedicated>,
    pub transmit: Box<DedicatedTransmitSession>,
    pub procedure: Box<RadioBearerSetupProcedure>,
    pub pool: FramePool,
}
pub struct RejectedAdmission {
    pub error: AdmissionError,
    pub request: Box<InitialAdmission>,
}
#[derive(Debug)]
pub struct RejectedAllocatedAdmission {
    pub admission: RejectedAdmission,
    pub allocation: crate::radio_resources::AllocatedRadio,
}
impl std::fmt::Debug for RejectedAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RejectedAdmission")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete typed result and resource custody."
    )
)]
pub struct RetiredAdmission {
    pub failed_phase: Option<AdmissionPhase>,
    pub startup: Option<StartupProgress>,
    pub failed_change: Option<FailedBearerChange>,
    pub circuit_assignment: Option<CircuitAssignment>,
    pub packet_assignment: Option<PacketAssignment>,
    pub additional_assignments: Vec<AdditionalAssignment>,
    pub rab_assignment: Option<crate::rab_handoff::PendingRabAssignment>,
    pub resources: Option<crate::radio_resources::RadioLease>,
    pub identity: ReceiveIdentity,
    pub procedure: Box<RadioBearerSetupProcedure>,
    pub receive: Option<RetiredDedicated>,
    pub transmit: Option<Box<DedicatedTransmitSession>>,
    pub unadmitted_receive: Option<Box<PreparedDedicated>>,
    pub error: AdmissionError,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupProgress {
    pub observed_at: ChipInstant,
    pub packets_prepared: bool,
    pub circuits_prepared: bool,
    pub bootstrap_submitted: bool,
    pub bootstrap: Option<crate::tx_evidence::TransportSubmissionSnapshot>,
    pub publication_attempts: u64,
    pub publication_readiness: Option<crate::signalling::SetupPublicationReadiness>,
    pub security_delivery: Option<crate::signalling::SecurityDeliveryStatus>,
    pub last_backpressure: Option<(ChipInstant, SignallingError)>,
}

pub struct ActiveRadio<'a> {
    identity: ReceiveIdentity,
    plan: &'a Arc<crate::rel99_plan::CompiledRel99>,
    lease: &'a crate::radio_resources::RadioLease,
}

impl ActiveRadio<'_> {
    pub fn identity(&self) -> ReceiveIdentity {
        self.identity
    }
    pub fn plan(&self) -> &Arc<crate::rel99_plan::CompiledRel99> {
        self.plan
    }
    pub fn lease(&self) -> &crate::radio_resources::RadioLease {
        self.lease
    }
}
#[derive(Clone, Copy)]
#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub struct FrameOpportunity {
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub dedicated_signalling: bool,
    pub user_bearers_ready: bool,
    pub idle: Tfci,
}
#[derive(Default, Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Retains the complete observation for diagnostic snapshots and replay."
    )
)]
pub struct AdmissionStep {
    pub command_results: usize,
    pub visits: usize,
    pub frames: usize,
    pub published: usize,
    pub retired: usize,
    pub failure: Option<(ReceiveIdentity, AdmissionError)>,
}
#[derive(Clone, Copy)]
enum CommandKind {
    Prepare,
    Frame,
    Arm,
    Observe,
    Remove,
    ReplacementPrepare,
    ReplacementCommit,
    SuccessorFrame,
    RetirePrevious,
    RecoveryRemove,
}
struct PendingCommand {
    id: TransmitCommandId,
    kind: CommandKind,
}
enum ReceiveOwnership {
    Local(Box<PreparedDedicated>),
    Worker,
    Returned(RetiredDedicated),
}
enum TransmitOwnership {
    Local(Box<DedicatedTransmitSession>),
    Preparing,
    Prepared,
    Armed,
    Returned(Box<DedicatedTransmitSession>),
    AwaitingJoinedOwner,
}
#[derive(Default)]
enum Transition {
    #[default]
    Idle,
    Addition(Box<addition::ScheduledAddition>),
    Release(Box<release::ScheduledRelease>),
    Recovery(Box<recovery::JoinedRecovery>),
}

struct Entry {
    prepared_hsdpa: Option<hsdpa::PreparedCatalog>,
    failed_phase: Option<AdmissionPhase>,
    startup: Option<StartupProgress>,
    quarantine_error: Option<AdmissionError>,
    quarantine_transmit: Option<Box<TransmitEvent>>,
    lifecycle: Lifecycle,
    additional_assignments: Vec<AdditionalAssignment>,
    additional_media_cursor: usize,
    addition_history: Option<addition::CompletedAddition>,
    inventory: Option<inventory::RabInventory>,
    release_history: Option<release::CompletedRelease>,
    setup_authority: bool,
    circuit_assignment: Option<CircuitAssignment>,
    packet_assignment: Option<PacketAssignment>,
    rab_assignment: Option<setup::SetupSource>,
    resources: Option<crate::radio_resources::RadioLease>,
    identity: ReceiveIdentity,
    procedure: Box<RadioBearerSetupProcedure>,
    pool: Option<FramePool>,
    receive: ReceiveOwnership,
    transmit: TransmitOwnership,
    pending: Option<PendingCommand>,
    frame: Option<FrameInput>,
    first: Option<TransportSubmission>,
    next_frame: ChipInstant,
    prepared_frames: usize,
    idle: Tfci,
    next_health: ChipInstant,
    last_downlink_power: Option<(ReceiveIdentity, crate::dedicated_tx::DownlinkPowerCounters)>,
    recovery_marked: bool,
    failure: Option<AdmissionError>,
}
pub struct Rel99Admission {
    hsupa_commands: Option<crate::hsupa_tx::control::Commands>,
    hsdpa_commands: Option<crate::hsdpa_packet::control::Commands>,
    hsdpa_feedback: Option<hsdpa::FeedbackRegistry>,
    recovery_owner_cursor: usize,
    frame_backpressure: u64,
    epoch: RadioEpoch,
    port: DedicatedTransmitPort,
    entries: Vec<Option<Entry>>,
    limits: AdmissionLimits,
    cursor: usize,
    rab_owner_cursor: usize,
    addition_owner_cursor: usize,
    release_owner_cursor: usize,
    retirement_owner_cursor: usize,
    previous: Option<ChipInstant>,
    unclaimed: Option<TransmitEvent>,
}
impl Rel99Admission {
    #[cfg(test)]
    pub fn startup_progress(&self, identity: ReceiveIdentity) -> Option<&StartupProgress> {
        self.entry(identity)?.startup.as_ref()
    }
    pub fn new(
        port: DedicatedTransmitPort,
        capacity: NonZeroUsize,
        limits: AdmissionLimits,
    ) -> Result<Self, (AdmissionError, Box<DedicatedTransmitPort>)> {
        if !port.available() {
            return Err((AdmissionError::Closed, Box::new(port)));
        }
        if port.outstanding() != 0 {
            return Err((AdmissionError::Phase, Box::new(port)));
        }
        if limits.maximum_lead_frames.get() > MAX_PREFILL_FRAMES
            || limits.prefill_frames > limits.maximum_lead_frames
        {
            return Err((AdmissionError::Configuration, Box::new(port)));
        }
        Ok(Self {
            hsupa_commands: None,
            hsdpa_commands: None,
            hsdpa_feedback: None,
            recovery_owner_cursor: 0,
            frame_backpressure: 0,
            addition_owner_cursor: 0,
            release_owner_cursor: 0,
            epoch: port.epoch(),
            port,
            entries: (0..capacity.get()).map(|_| None).collect(),
            limits,
            cursor: 0,
            rab_owner_cursor: 0,
            retirement_owner_cursor: 0,
            previous: None,
            unclaimed: None,
        })
    }
    pub fn epoch(&self) -> RadioEpoch {
        self.epoch
    }
    pub fn configure_hsdpa_control(&mut self, capacity: NonZeroUsize) -> Result<(), TransmitError> {
        if self.hsdpa_commands.is_some() {
            return Err(TransmitError::Phase);
        }
        self.hsdpa_commands = Some(crate::hsdpa_packet::control::Commands::new(capacity)?);
        Ok(())
    }
    pub fn hsdpa_control(&mut self) -> Option<crate::hsdpa_packet::ControlPort<'_>> {
        Some(self.hsdpa_commands.as_mut()?.port(&mut self.port))
    }
    pub fn configure_hsupa_control(&mut self, capacity: NonZeroUsize) -> Result<(), TransmitError> {
        if self.hsupa_commands.is_some() {
            return Err(TransmitError::Phase);
        }
        self.hsupa_commands = Some(crate::hsupa_tx::control::Commands::new(capacity)?);
        Ok(())
    }
    pub fn hsupa_control(&mut self) -> Option<crate::hsupa_tx::ControlPort<'_>> {
        Some(self.hsupa_commands.as_mut()?.port(&mut self.port))
    }
    #[cfg(test)]
    pub fn frame_backpressure(&self) -> u64 {
        self.frame_backpressure
    }
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }
    pub(crate) fn paired_with(&self, preparation: &crate::radio_io::TxPreparation) -> bool {
        self.port.belongs_to(preparation.source()) && preparation.dedicated_paired_with(&self.port)
    }
    pub fn phase(&self, identity: ReceiveIdentity) -> Option<AdmissionPhase> {
        self.entry(identity).map(|entry| entry.lifecycle.phase())
    }

    pub(crate) fn uplink_handoff_pending(&self, identity: ReceiveIdentity) -> bool {
        self.entries
            .get(identity.ue.slot as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.identity.ue == identity.ue && entry.identity != identity)
            .is_some_and(|entry| {
                entry
                    .lifecycle
                    .release()
                    .is_some_and(|release| release.uplink_handoff_pending(identity))
                    || entry
                        .lifecycle
                        .addition()
                        .is_some_and(|addition| addition.uplink_handoff_pending(identity))
            })
    }

    pub fn visit_downlink_power(
        &self,
        mut visit: impl FnMut(ReceiveIdentity, crate::dedicated_tx::DownlinkPowerCounters),
    ) {
        for entry in self.entries.iter().flatten() {
            if let Some((identity, counters)) = entry.last_downlink_power
                && identity == entry.identity
            {
                visit(identity, counters);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn initial_frame_submission(
        &self,
        identity: ReceiveIdentity,
    ) -> Option<TransportSubmission> {
        self.entry(identity)?.first.clone()
    }

    pub fn active_radio(
        &self,
        identity: ReceiveIdentity,
    ) -> Result<ActiveRadio<'_>, AdmissionError> {
        if self
            .entry(identity)
            .ok_or(AdmissionError::Identity)?
            .pending
            .is_some()
        {
            return Err(AdmissionError::Phase);
        }
        self.committed_radio(identity)
    }

    pub(crate) fn committed_radio(
        &self,
        identity: ReceiveIdentity,
    ) -> Result<ActiveRadio<'_>, AdmissionError> {
        let entry = self.entry(identity).ok_or(AdmissionError::Identity)?;
        // Frame/health replies do not change the committed configuration or its ownership.
        let transition_pending = entry.pending.as_ref().is_some_and(|pending| {
            !matches!(pending.kind, CommandKind::Frame | CommandKind::Observe)
        });
        if entry.lifecycle.phase() != AdmissionPhase::Active
            || !entry.lifecycle.is_idle()
            || entry.quarantine_error.is_some()
            || entry.failure.is_some()
            || transition_pending
        {
            return Err(AdmissionError::Phase);
        }
        let pool = entry.pool.as_ref().ok_or(AdmissionError::Phase)?;
        let lease = entry.resources.as_ref().ok_or(AdmissionError::Identity)?;
        if lease.receive_identity() != identity || pool.identity() != identity {
            return Err(AdmissionError::Identity);
        }
        Ok(ActiveRadio {
            identity,
            plan: pool.plan(),
            lease,
        })
    }
    pub fn build_release_owners(
        &self,
        previous: ReceiveIdentity,
        prepared: crate::rel99_planner::PreparedRelease,
        radio: &mut CommonSignalling,
        settings: crate::rel99_planner::InitialOwnerSettings,
    ) -> Result<crate::rel99_planner::BuiltRelease, Box<crate::rel99_planner::RejectedReleaseOwners>>
    {
        let active = match self.active_radio(previous) {
            Ok(active) => active,
            Err(_) => {
                return Err(Box::new(crate::rel99_planner::RejectedReleaseOwners {
                    error: crate::rel99_planner::OwnerBuildError::Phase,
                    prepared: Box::new(prepared),
                }));
            }
        };
        prepared.build_owners(&active, radio, &self.port, settings)
    }

    pub fn build_existing_owners<'a>(
        &self,
        previous: ReceiveIdentity,
        prepared: &crate::rel99_planner::PreparedExisting,
        radio: &'a mut CommonSignalling,
        settings: crate::rel99_planner::InitialOwnerSettings,
    ) -> Result<crate::rel99_planner::BuiltExisting<'a>, crate::rel99_planner::OwnerBuildError>
    {
        let active = self
            .active_radio(previous)
            .map_err(|_| crate::rel99_planner::OwnerBuildError::Phase)?;
        prepared.build_owners(&active, radio, &self.port, settings)
    }
    #[cfg(test)]
    pub fn failure(&self, identity: ReceiveIdentity) -> Option<&AdmissionError> {
        self.entry(identity)?.failure.as_ref()
    }
    #[cfg(test)]
    pub fn procedure(&self, identity: ReceiveIdentity) -> Option<&RadioBearerSetupProcedure> {
        let entry = self.entry(identity)?;
        entry.setup_authority.then_some(&entry.procedure)
    }
    #[cfg(test)]
    pub fn take_unclaimed(&mut self) -> Option<TransmitEvent> {
        self.unclaimed.take()
    }
    fn entry(&self, identity: ReceiveIdentity) -> Option<&Entry> {
        self.entries
            .get(identity.ue.slot as usize)?
            .as_ref()
            .filter(|entry| entry.identity == identity)
    }
    fn entry_mut(&mut self, identity: ReceiveIdentity) -> Option<&mut Entry> {
        self.entries
            .get_mut(identity.ue.slot as usize)?
            .as_mut()
            .filter(|entry| entry.identity == identity)
    }
    pub fn begin_allocated(
        &mut self,
        request: Box<InitialAdmission>,
        allocation: crate::radio_resources::AllocatedRadio,
        radio: &CommonSignalling,
        receive: &DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedAllocatedAdmission> {
        if allocation.key() != request.pool.identity().ue
            || !Arc::ptr_eq(allocation.plan(), request.pool.plan())
        {
            return Err(RejectedAllocatedAdmission {
                admission: RejectedAdmission {
                    error: AdmissionError::Configuration,
                    request,
                },
                allocation,
            });
        }
        match self.begin_initial(request, radio, receive) {
            Ok(identity) => {
                self.entry_mut(identity)
                    .expect("accepted admission")
                    .resources = Some(allocation.into_lease());
                Ok(identity)
            }
            Err(admission) => Err(RejectedAllocatedAdmission {
                admission,
                allocation,
            }),
        }
    }
    pub fn begin_initial(
        &mut self,
        request: Box<InitialAdmission>,
        radio: &CommonSignalling,
        receive: &DedicatedController,
    ) -> Result<ReceiveIdentity, RejectedAdmission> {
        let identity = request.pool.identity();
        let check = (|| {
            if identity.ue.epoch != self.epoch
                || receive.epoch() != self.epoch
                || radio.epoch() != self.epoch
                || self.entries.len() != receive.capacity()
                || identity.ue.slot as usize >= self.entries.len()
            {
                return Err(AdmissionError::Identity);
            }
            if self.entries[identity.ue.slot as usize].is_some()
                || receive.phase(identity).is_some()
                || request.procedure.phase() != SetupPhase::Prepared
                || request.procedure.activation().is_some()
                || request.receive.bearers.phase() != BearerPhase::Prepared
                || request.transmit.phase() != TransmitPhase::Prepared
                || request.transmit.queued_frames() != 0
                || radio.dedicated_route(identity.ue).is_some()
            {
                return Err(AdmissionError::Phase);
            }
            if identity != request.transmit.identity()
                || identity != request.receive.bearers.identity()
                || identity != request.receive.receiver.session().identity()
                || identity.ue != request.procedure.key()
                || !radio.owns_frame_pool(&request.pool)
                || !request.pool.bound_to(&self.port)
            {
                return Err(AdmissionError::Identity);
            }
            let plan = request.pool.plan();
            if request.receive.bearers.user_bearers_ready()
                || request
                    .receive
                    .bearers
                    .awaiting_users()
                    .map_err(|error| AdmissionError::Receive(DedicatedControlError::Bearer(error)))?
                    .is_none()
            {
                return Err(AdmissionError::Configuration);
            }
            let tx = request.transmit.frame_anchor();
            let rx = request.receive.bearers.frame_anchor();
            // TS 25.331 8.5.15.1 initializes CELL_DCH CFN from the advertised DOFF and cell SFN.
            if plan
                .configuration()
                .physical()
                .cfn_for_sfn(tx.time.system_frame())
                .map_err(AdmissionError::Procedure)?
                != tx.cfn
            {
                return Err(AdmissionError::Time);
            }
            if !Arc::ptr_eq(plan, request.transmit.plan())
                || !Arc::ptr_eq(plan.configuration(), request.procedure.configuration())
                || !Arc::ptr_eq(
                    plan.uplink_configured(),
                    request.receive.bearers.configured(),
                )
                || tx.cfn != rx.cfn
            {
                return Err(AdmissionError::Configuration);
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
            plan.downlink_configured()
                .combinations()
                .iter()
                .find(|combination| {
                    plan.downlink_configured()
                        .transport_ids()
                        .zip(&combination.formats)
                        .all(|(id, format)| {
                            let block = plan
                                .downlink_configured()
                                .definition(id)
                                .expect("configured transport")
                                .formats[*format];
                            block.blocks == 0 || block.block_bits == 0
                        })
                })
                .map(|combination| combination.tfci)
                .ok_or(AdmissionError::Configuration)
        })();
        let idle = match check {
            Ok(idle) => idle,
            Err(error) => return Err(RejectedAdmission { error, request }),
        };
        let initial_frame = request.transmit.frame_anchor().time;
        let InitialAdmission {
            receive,
            transmit,
            procedure,
            pool,
        } = *request;
        self.entries[identity.ue.slot as usize] = Some(Entry {
            prepared_hsdpa: None,
            failed_phase: None,
            startup: None,
            lifecycle: Lifecycle::initial(),
            quarantine_error: None,
            quarantine_transmit: None,
            additional_assignments: Vec::new(),
            additional_media_cursor: 0,
            addition_history: None,
            inventory: None,
            release_history: None,
            setup_authority: true,
            circuit_assignment: None,
            packet_assignment: None,
            rab_assignment: None,
            resources: None,
            identity,
            procedure,
            pool: Some(pool),
            receive: ReceiveOwnership::Local(receive),
            transmit: TransmitOwnership::Local(transmit),
            pending: None,
            frame: None,
            first: None,
            next_frame: initial_frame,
            prepared_frames: 0,
            idle,
            next_health: initial_frame,
            last_downlink_power: None,
            recovery_marked: false,
            failure: None,
        });
        Ok(identity)
    }
    #[cfg(test)]
    pub fn cancel(&mut self, identity: ReceiveIdentity) -> Result<(), AdmissionError> {
        let entry = self.entry_mut(identity).ok_or(AdmissionError::Identity)?;
        if entry.lifecycle.phase() == AdmissionPhase::Retired
            || entry.lifecycle.release().is_some()
            || entry.lifecycle.addition().is_some()
        {
            return Err(AdmissionError::Phase);
        }
        fail(entry, AdmissionError::Cancelled);
        Ok(())
    }
    /// `Some(true)` with packet RABs of the original assignment to set up again, `Some(false)`
    /// with no RAB, `None` when the session cannot be stopped for a paging state.
    pub fn parkable(&self, identity: ReceiveIdentity) -> Option<bool> {
        let entry = self.entry(identity)?;
        if entry.lifecycle.phase() != AdmissionPhase::Active
            || !entry.lifecycle.is_idle()
            || entry.failure.is_some()
        {
            return None;
        }
        let inventory = entry.inventory.as_ref()?;
        if inventory.active().is_empty() {
            return Some(false);
        }
        if !entry.additional_assignments.is_empty() || entry.circuit_assignment.is_some() {
            return None;
        }
        let pending = entry.rab_assignment.as_ref()?.pending();
        (pending.request().domain() == umts_iuh::Domain::Ps
            && pending.setup_selection().is_some()
            && inventory
                .active()
                .iter()
                .all(|active| active.request() == pending.request()))
        .then_some(true)
    }
    pub fn park(&mut self, identity: ReceiveIdentity) -> Result<(), AdmissionError> {
        if self.parkable(identity).is_none() {
            return Err(AdmissionError::Phase);
        }
        let entry = self.entry_mut(identity).ok_or(AdmissionError::Identity)?;
        fail(entry, AdmissionError::Parked);
        Ok(())
    }
    pub fn take_retired(
        &mut self,
        identity: ReceiveIdentity,
    ) -> Result<RetiredAdmission, AdmissionError> {
        if self
            .entry(identity)
            .is_some_and(|entry| entry.lifecycle.addition().is_some())
        {
            return Err(AdmissionError::Phase);
        }
        if self.phase(identity) != Some(AdmissionPhase::Retired) {
            return Err(AdmissionError::Phase);
        }
        let mut entry = self.entries[identity.ue.slot as usize]
            .take()
            .expect("retired admission");
        let (receive, unadmitted_receive) = match entry.receive {
            ReceiveOwnership::Returned(receive) => (Some(receive), None),
            ReceiveOwnership::Local(receive) => (None, Some(receive)),
            ReceiveOwnership::Worker => {
                unreachable!("retirement requires the returned receiver owner")
            }
        };
        let transmit = match entry.transmit {
            TransmitOwnership::Returned(transmit) | TransmitOwnership::Local(transmit) => {
                Some(transmit)
            }
            _ => unreachable!("retirement requires the returned transmitter owner"),
        };
        Ok(RetiredAdmission {
            failed_phase: entry.failed_phase,
            startup: entry.startup,
            failed_change: entry
                .lifecycle
                .take_recovery()
                .map(|value| value.into_failed()),
            additional_assignments: entry.additional_assignments,
            circuit_assignment: entry.circuit_assignment,
            packet_assignment: entry.packet_assignment,
            rab_assignment: entry.rab_assignment.map(setup::SetupSource::into_pending),
            resources: entry.resources,
            identity,
            procedure: entry.procedure,
            receive,
            transmit,
            unadmitted_receive,
            error: entry.failure.expect("terminal admission cause"),
        })
    }
    pub fn response(
        &mut self,
        received: &ReceivedControl<'_>,
        security: &mut ControlSecurity<'_>,
        receive: &mut DedicatedController,
    ) -> Option<Result<(), SignallingError>> {
        use umts_rrc_schema::class_definitions::ULDCCHMessageType;
        if matches!(
            received.message().value.message,
            ULDCCHMessageType::radioBearerReleaseComplete(_)
                | ULDCCHMessageType::radioBearerReleaseFailure(_)
        ) {
            return self.release_response(received, security);
        }
        let resume = self
            .entries
            .get(received.key().slot as usize)
            .and_then(Option::as_ref)
            .is_some_and(|entry| {
                entry.identity.ue == received.key()
                    && entry.setup_authority
                    && entry.procedure.mode() == umts_rrc::dedicated::SetupMode::Resume
                    && entry.procedure.phase() != umts_rrc::dedicated::SetupPhase::Complete
            });
        // The answer to a resume is a reconfiguration response. Later set-up responses on the
        // same session belong to bearer additions.
        if !matches!(
            (&received.message().value.message, resume),
            (
                ULDCCHMessageType::radioBearerSetupComplete(_)
                    | ULDCCHMessageType::radioBearerSetupFailure(_),
                _
            ) | (
                ULDCCHMessageType::radioBearerReconfigurationComplete(_)
                    | ULDCCHMessageType::radioBearerReconfigurationFailure(_),
                true
            )
        ) {
            return None;
        }
        if let Some(response) = self.addition_response(received, security, receive) {
            return Some(response);
        }
        let entry = self
            .entries
            .get_mut(received.key().slot as usize)?
            .as_mut()
            .filter(|entry| entry.identity.ue == received.key())?;
        if !entry.setup_authority || entry.lifecycle.release().is_some() {
            return Some(Err(SignallingError::Phase));
        }
        if !matches!(
            entry.lifecycle.phase(),
            AdmissionPhase::AwaitingComplete
                | AdmissionPhase::AwaitingCommandAck
                | AdmissionPhase::Active
        ) {
            return Some(Err(SignallingError::Phase));
        }
        let response = if entry.lifecycle.phase() == AdmissionPhase::Active {
            entry
                .procedure
                .response(received.key(), received.time(), received.message())
                .map_err(SignallingError::Dedicated)
        } else {
            let pending =
                receive.with_active_bank(entry.identity, |bank| !bank.user_bearers_ready());
            match pending {
                Err(error) => Err(SignallingError::DedicatedControl(error)),
                Ok(false) => received.bearer_setup_response(&mut entry.procedure, security),
                Ok(true) => (|| {
                    let contexts = [
                        security.user_cipher(umts_types::CnDomain::Cs)?,
                        security.user_cipher(umts_types::CnDomain::Ps)?,
                    ];
                    received
                        .bearer_setup_response_with(
                            &mut entry.procedure,
                            security,
                            |accepted, ciphers| {
                                receive
                                    .with_active_bank(entry.identity, |bank| {
                                        bank.install_initial_users(
                                            &accepted,
                                            contexts,
                                            ciphers,
                                            received.time().serviced(),
                                        )
                                    })
                                    .map_err(SignallingError::DedicatedControl)?
                                    .map_err(SignallingError::UserBearer)
                            },
                        )
                        .map(|(response, _)| response)
                })(),
            }
        };
        Some(response.and_then(|response| {
            match response {
                SetupResponse::Complete => entry
                    .lifecycle
                    .advance(AdmissionPhase::AwaitingCommandAck)
                    .map_err(|_| SignallingError::Message),
                SetupResponse::Failure => {
                    fail(entry, AdmissionError::Procedure(DedicatedError::Phase));
                    Ok(())
                }
                SetupResponse::Duplicate => Ok(()),
            }
        }))
    }
    fn poll(&mut self) -> usize {
        let mut count = 0;
        for _ in 0..self.limits.command_results.get() {
            if self.unclaimed.is_some() {
                break;
            }
            let Some(event) = self.port.try_event() else {
                break;
            };
            let event = if let Some(commands) = &mut self.hsupa_commands {
                let Some(event) = commands.route(event) else {
                    count += 1;
                    continue;
                };
                event
            } else {
                event
            };
            let event = if let Some(commands) = &mut self.hsdpa_commands {
                let Some(event) = commands.route(event) else {
                    count += 1;
                    continue;
                };
                event
            } else {
                event
            };
            let slot = match &event.outcome {
                TransmitCommandOutcome::Prepared(identity)
                | TransmitCommandOutcome::Armed { identity, .. }
                | TransmitCommandOutcome::Removed { identity, .. }
                | TransmitCommandOutcome::Observed { identity, .. } => {
                    Some(identity.ue.slot as usize)
                }
                TransmitCommandOutcome::ReplacementPrepared { identity, .. }
                | TransmitCommandOutcome::ReplacementCommitted { identity, .. }
                | TransmitCommandOutcome::Retired { identity, .. } => {
                    Some(identity.ue.slot as usize)
                }
                TransmitCommandOutcome::Frame { input, .. } => {
                    Some(input.identity.ue.slot as usize)
                }
                TransmitCommandOutcome::PrepareRejected { session, .. } => {
                    Some(session.identity().ue.slot as usize)
                }
                TransmitCommandOutcome::Closed => self.entries.iter().position(|entry| {
                    entry.as_ref().is_some_and(|entry| {
                        entry
                            .pending
                            .as_ref()
                            .is_some_and(|pending| pending.id == event.id)
                    })
                }),
                _ => None,
            };
            let slot = slot
                .filter(|slot| {
                    self.entries
                        .get(*slot)
                        .and_then(Option::as_ref)
                        .is_some_and(|entry| {
                            entry
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.id == event.id)
                        })
                })
                .or_else(|| {
                    self.entries.iter().position(|entry| {
                        entry.as_ref().is_some_and(|entry| {
                            entry
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.id == event.id)
                        })
                    })
                });
            let Some(entry) = slot.and_then(|slot| self.entries[slot].as_mut()) else {
                self.unclaimed = Some(event);
                break;
            };
            let pending = entry.pending.take().expect("matched command");
            if entry.quarantine_error.is_some()
                || entry
                    .lifecycle
                    .recovery()
                    .is_some_and(recovery::JoinedRecovery::is_quarantined)
            {
                entry.quarantine_transmit = Some(Box::new(event));
                count += 1;
                continue;
            }
            if matches!(pending.kind, CommandKind::RecoveryRemove) {
                entry
                    .lifecycle
                    .recovery_mut()
                    .expect("recovery removal owner")
                    .poll_transmit(event.outcome);
                count += 1;
                continue;
            }
            if matches!(
                pending.kind,
                CommandKind::ReplacementPrepare
                    | CommandKind::ReplacementCommit
                    | CommandKind::SuccessorFrame
                    | CommandKind::RetirePrevious
            ) {
                let outcome = if entry.lifecycle.addition().is_some() {
                    addition::poll_transmit(
                        entry,
                        pending.kind,
                        event.outcome,
                        &mut self.frame_backpressure,
                    )
                } else {
                    release::poll_transmit(
                        entry,
                        pending.kind,
                        event.outcome,
                        &mut self.frame_backpressure,
                    )
                };
                if let Some(outcome) = outcome {
                    entry.quarantine_transmit = Some(Box::new(TransmitEvent {
                        id: event.id,
                        outcome,
                    }));
                    entry.quarantine_error = Some(AdmissionError::Protocol);
                    entry.setup_authority = false;
                }
                count += 1;
                continue;
            }
            match (pending.kind, event.outcome) {
                (_, TransmitCommandOutcome::Closed) => {
                    entry.transmit = TransmitOwnership::AwaitingJoinedOwner;
                    fail(entry, AdmissionError::Closed);
                }
                (CommandKind::Prepare, TransmitCommandOutcome::Prepared(identity))
                    if identity == entry.identity =>
                {
                    entry.transmit = TransmitOwnership::Prepared;
                }
                (
                    CommandKind::Prepare,
                    TransmitCommandOutcome::PrepareRejected { error, session },
                ) if session.identity() == entry.identity => {
                    entry.transmit = TransmitOwnership::Local(session);
                    fail(entry, AdmissionError::Transmit(error));
                }
                (CommandKind::Frame, TransmitCommandOutcome::Frame { mut input, result })
                    if input.identity == entry.identity =>
                {
                    if result == Err(TransmitError::Capacity)
                        && entry.lifecycle.phase() != AdmissionPhase::Stopping
                    {
                        // Capacity rejects before TX mutates the frame or advances its cursor.
                        entry.next_frame = input.start;
                        entry.frame = Some(input);
                        self.frame_backpressure = self.frame_backpressure.saturating_add(1);
                        count += 1;
                        continue;
                    }
                    if result.is_err()
                        && let Some(emission) = &mut input.emission
                    {
                        emission.fail();
                    }
                    if let Err(input) = entry.pool.as_mut().expect("live pool").recycle(input) {
                        entry.frame = Some(input);
                        fail(entry, AdmissionError::Protocol);
                    }
                    match result {
                        Ok(_) => entry.prepared_frames += 1,
                        Err(error) => fail(entry, AdmissionError::Transmit(error)),
                    }
                }
                (CommandKind::Arm, TransmitCommandOutcome::Armed { identity, result })
                    if identity == entry.identity =>
                {
                    match result {
                        Ok(()) => entry.transmit = TransmitOwnership::Armed,
                        Err(error) => fail(entry, AdmissionError::Transmit(error)),
                    }
                }
                (
                    CommandKind::Observe,
                    TransmitCommandOutcome::Observed {
                        identity,
                        state,
                        tpc: _,
                    },
                ) if identity == entry.identity => {
                    entry.last_downlink_power = state
                        .as_ref()
                        .map(|(_, counters, _)| (identity, counters.downlink_power));
                    if let Some((TransmitPhase::Failed, _, error)) = state {
                        fail(
                            entry,
                            AdmissionError::Transmit(error.unwrap_or(TransmitError::Closed)),
                        );
                    } else if state.is_none() {
                        fail(entry, AdmissionError::Protocol);
                    }
                }
                (CommandKind::Remove, TransmitCommandOutcome::Removed { identity, result })
                    if identity == entry.identity =>
                {
                    match result {
                        Ok(session) => {
                            entry.transmit = TransmitOwnership::Returned(session);
                        }
                        Err(error) => {
                            if let Some(recovery) = entry.lifecycle.recovery_mut() {
                                if error != TransmitError::Capacity {
                                    recovery.quarantine(AdmissionError::Transmit(error));
                                }
                            } else {
                                fail(entry, AdmissionError::Transmit(error));
                            }
                        }
                    }
                }
                (_, outcome) => {
                    let retained = TransmitEvent {
                        id: event.id,
                        outcome,
                    };
                    if let Some(recovery) = entry.lifecycle.recovery_mut() {
                        entry.quarantine_transmit = Some(Box::new(retained));
                        recovery.quarantine(AdmissionError::Protocol);
                    } else {
                        entry.quarantine_transmit = Some(Box::new(retained));
                        entry.quarantine_error = Some(AdmissionError::Protocol);
                        entry.setup_authority = false;
                    }
                }
            }
            count += 1;
        }
        count
    }
}
