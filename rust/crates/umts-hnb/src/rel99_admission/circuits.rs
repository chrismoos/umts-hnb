// SPDX-License-Identifier: AGPL-3.0-or-later

use super::inventory::RabOutcomeIds;
use super::{AdmissionPhase, Rel99Admission, RetiredAdmission};
use crate::{
    core_link::CoreLink,
    dedicated_bearers::BearerId,
    dedicated_controller::DedicatedController,
    dedicated_rx::ReceiveIdentity,
    rab_handoff::SetupAssignment,
    rel99_plan::CompiledRel99,
    rel99_planner::RabPlan,
    voice_bearers::{self, InitializationStatus, VoiceKey, VoiceTraffic, VoiceTransport},
};
use std::{
    io,
    net::{IpAddr, SocketAddr},
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
    time::Instant,
};
use umts_core::{Gateway, Transport};
use umts_iuh::{
    Domain,
    ranap::rab::{Address, Association, Cause, Failed, Peer, Setup, SetupOrModify, qos},
    session::{ControlTime, RabOperation, RabOutcome, RabRequest},
};
use umts_rrc::dedicated::SetupPhase;
use umts_types::{
    CnDomain, ConnectionKey,
    time::{CHIPS_PER_FRAME, ChipInstant},
};
use umts_user_plane::{
    iu_cs::RtpEndpoint,
    iuup::{
        initialization::Versions,
        negotiation::{self, Settings},
    },
    media::RtpAnchor,
    uplink_media::{Delivery, RtpSource},
};

const MAX_SERVICE_VISITS: usize = 64;
const RNC_UNABLE_TO_ESTABLISH_ALL_RFCS: u16 = 47;

#[derive(Debug)]
pub enum InitialCircuitError {
    Phase,
    Identity,
    Capacity,
    Configuration,
    Inventory(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        super::inventory::InventoryError,
    ),
    Io {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: io::Error,
    },
    Voice {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: voice_bearers::Error,
    },
    Transport {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: voice_bearers::TransportError,
    },
    Core {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        error: umts_core::Error,
    },
    Protocol {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: umts_iuh::Error,
    },
    Radio {
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        rab: u8,
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        error: crate::dedicated_controller::DedicatedControlError,
    },
}

#[derive(Clone, Copy)]
pub struct CircuitSettings {
    pub local: SocketAddr,
    pub advertised: Option<IpAddr>,
    pub ingress: voice_bearers::IngressSettings,
    pub source: RtpSource,
    pub timestamp: u32,
    pub wait_chips: NonZeroU64,
    pub initialization: CircuitInitialization,
}

#[derive(Clone, Copy)]
pub enum CircuitInitialization {
    Initiate(Settings),
    Receive(Versions),
}

#[derive(Clone, Copy)]
pub struct CircuitTime {
    pub control: Instant,
    pub radio: ChipInstant,
    pub anchor: RtpAnchor,
}

#[derive(Clone, Copy)]
pub struct CircuitPlayoutTime {
    pub control: Instant,
    pub radio: ChipInstant,
    pub settings: voice_bearers::PlayoutSettings,
}

pub(crate) enum CircuitIngressTime {
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Anchored(CircuitTime),
    Playout(CircuitPlayoutTime),
}

pub(crate) struct CircuitPreparation<'a, C: Transport> {
    pub pending: SetupAssignment<'a>,
    pub identity: ReceiveIdentity,
    pub plan: &'a Arc<CompiledRel99>,
    pub gateway: &'a Gateway<C>,
}

pub(crate) struct CircuitService<'a, C: Transport> {
    pub identity: ReceiveIdentity,
    pub gateway: &'a Gateway<C>,
    pub receive: &'a mut DedicatedController,
    pub time: CircuitIngressTime,
    pub earliest_playout: ChipInstant,
    pub radio_active: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CircuitProgress {
    pub items: usize,
    pub prepared: usize,
    pub active: usize,
    pub ready: usize,
    pub bound: usize,
    pub recorded: usize,
    pub retired: usize,
    pub superseded: usize,
    pub failed: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CircuitCounters {
    pub visits: u64,
    pub received: u64,
    pub accepted: u64,
    pub wrong_peer: u64,
    pub wrong_payload_type: u64,
    pub wrong_source: u64,
    pub malformed: u64,
    pub rejected: u64,
    pub control_backpressure: u64,
    pub uplink_sent: u64,
    pub uplink_dropped: u64,
    pub uplink_backpressure: u64,
    pub initialization_failed: u64,
}

impl CircuitCounters {
    fn receive(&mut self, progress: voice_bearers::VoiceIngress) {
        self.received = self.received.saturating_add(progress.received as u64);
        self.accepted = self.accepted.saturating_add(progress.accepted as u64);
        self.wrong_peer = self.wrong_peer.saturating_add(progress.wrong_peer as u64);
        self.wrong_payload_type = self
            .wrong_payload_type
            .saturating_add(progress.wrong_payload_type as u64);
        self.wrong_source = self
            .wrong_source
            .saturating_add(progress.wrong_source as u64);
        self.malformed = self.malformed.saturating_add(progress.malformed as u64);
        self.rejected = self.rejected.saturating_add(progress.rejected as u64);
    }
}

struct OwnedCircuit {
    transport: VoiceTransport<RtpEndpoint>,
    setup: Setup,
    timestamp: u32,
}

#[derive(Clone, Copy)]
enum Disposition {
    Unreported,
    Recorded,
    Superseded,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
enum Stage {
    Unprepared,
    Prepared(OwnedCircuit),
    Bound(OwnedCircuit),
    Recorded(OwnedCircuit),
    Draining(OwnedCircuit),
    Failed {
        failure: negotiation::Failure,
        disposition: Disposition,
    },
    Retired(Disposition),
}

impl Stage {
    fn owned(&self) -> Option<&OwnedCircuit> {
        match self {
            Self::Prepared(value)
            | Self::Bound(value)
            | Self::Recorded(value)
            | Self::Draining(value) => Some(value),
            Self::Unprepared | Self::Retired(_) | Self::Failed { .. } => None,
        }
    }
    fn owned_mut(&mut self) -> Option<&mut OwnedCircuit> {
        match self {
            Self::Prepared(value)
            | Self::Bound(value)
            | Self::Recorded(value)
            | Self::Draining(value) => Some(value),
            Self::Unprepared | Self::Retired(_) | Self::Failed { .. } => None,
        }
    }
    fn retire(
        &mut self,
        voices: &mut VoiceTraffic,
        rab: u8,
        disposition: Disposition,
    ) -> Result<usize, InitialCircuitError> {
        if let Self::Failed {
            disposition: current,
            ..
        } = self
        {
            *current = disposition;
            return Ok(0);
        }
        let count = if let Some(owned) = self.owned_mut() {
            usize::from(
                owned
                    .transport
                    .retire(voices)
                    .map_err(|error| InitialCircuitError::Transport { rab, error })?,
            )
        } else {
            0
        };
        *self = Self::Retired(disposition);
        Ok(count)
    }
}

struct Circuit {
    metadata: RabPlan,
    stage: Stage,
}

#[must_use = "Retain CS attempts until exact media cleanup or successful canonical outcome recording"]
pub struct CircuitAssignment {
    request: RabRequest,
    circuits: Vec<Circuit>,
    cursor: usize,
    counters: CircuitCounters,
}

impl CircuitAssignment {
    pub(crate) fn needs_preparation(&self) -> bool {
        self.circuits
            .iter()
            .any(|circuit| matches!(circuit.stage, Stage::Unprepared))
    }

    pub(crate) fn prepare<C: Transport>(
        &mut self,
        context: CircuitPreparation<'_, C>,
        voices: &mut VoiceTraffic,
        mut settings: impl FnMut(
            &RabPlan,
            &SetupOrModify,
        ) -> Result<CircuitSettings, InitialCircuitError>,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        let CircuitPreparation {
            pending,
            identity,
            plan,
            gateway: core,
        } = context;
        let pending = pending.pending();
        if pending.request() != self.request
            || identity.ue != self.request.key()
            || self.request.domain() != Domain::Cs
            || self.circuits.len() != pending.assignment().setup.len()
            || self
                .circuits
                .iter()
                .zip(&pending.assignment().setup)
                .any(|(circuit, item)| {
                    circuit.metadata.domain != CnDomain::Cs
                        || circuit.metadata.rab != item.first.id.get()
                })
            || !core.rab_connection_current(self.request)
        {
            return Err(InitialCircuitError::Identity);
        }
        for (circuit, item) in self.circuits.iter_mut().zip(&pending.assignment().setup) {
            if !matches!(circuit.stage, Stage::Unprepared) {
                continue;
            }
            let rab = circuit.metadata.rab;
            core.authorize_rab(self.request, rab, RabOperation::SetupOrModify)
                .map_err(|error| InitialCircuitError::Core { rab, error })?;
            let policy = settings(&circuit.metadata, item)?;
            circuit.stage = Stage::Prepared(prepare(
                voices,
                identity,
                plan,
                self.request,
                &circuit.metadata,
                item,
                policy,
            )?);
        }
        Ok(self.progress(voices))
    }

    pub(crate) fn service<C: Transport>(
        &mut self,
        context: CircuitService<'_, C>,
        voices: &mut VoiceTraffic,
        budget: NonZeroUsize,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        let CircuitService {
            identity,
            gateway: core,
            receive,
            time: now,
            earliest_playout,
            radio_active,
        } = context;
        let (control, radio) = match &now {
            CircuitIngressTime::Anchored(now) => (now.control, now.radio),
            CircuitIngressTime::Playout(now) => (now.control, now.radio),
        };
        if budget.get() > MAX_SERVICE_VISITS {
            return Err(InitialCircuitError::Configuration);
        }

        if identity.ue != self.request.key()
            || radio.epoch != identity.ue.epoch
            || earliest_playout.epoch != identity.ue.epoch
            || !core.owns_rab_request(self.request)
        {
            return Err(InitialCircuitError::Identity);
        }
        for _ in 0..budget.get() {
            self.counters.visits = self.counters.visits.saturating_add(1);
            let index = self.cursor;
            self.cursor = (index + 1) % self.circuits.len();
            let circuit = &mut self.circuits[index];
            let rab = circuit.metadata.rab;
            if matches!(
                circuit.stage,
                Stage::Failed { .. } | Stage::Retired(_) | Stage::Draining(_)
            ) {
                continue;
            }
            let current = if matches!(circuit.stage, Stage::Recorded(_)) {
                core.rab_connection_current(self.request)
            } else {
                authorized(core, self.request, rab)?
            };
            if !current && !matches!(circuit.stage, Stage::Retired(_)) {
                if let Stage::Recorded(owned) = &circuit.stage {
                    voices
                        .begin_media_drain(owned.transport.handle())
                        .map_err(|error| InitialCircuitError::Voice { rab, error })?;
                    let Stage::Recorded(owned) =
                        std::mem::replace(&mut circuit.stage, Stage::Unprepared)
                    else {
                        unreachable!("recorded circuit owner")
                    };
                    circuit.stage = Stage::Draining(owned);
                    continue;
                }
                circuit.stage.retire(voices, rab, Disposition::Superseded)?;
                continue;
            }
            let Some(owned) = circuit.stage.owned_mut() else {
                continue;
            };
            if radio_active {
                activate(voices, receive, identity, owned, rab)?;
            }
            match owned
                .transport
                .publish_initialization(voices, control, owned.timestamp)
            {
                Ok(_) => (),
                Err(error) if error.backpressure() => {
                    self.counters.control_backpressure =
                        self.counters.control_backpressure.saturating_add(1)
                }
                Err(error) => return Err(InitialCircuitError::Transport { rab, error }),
            }
            let ingress = match &now {
                CircuitIngressTime::Anchored(now) => {
                    owned
                        .transport
                        .receive(voices, control, now.anchor, NonZeroUsize::MIN)
                }
                CircuitIngressTime::Playout(now) => owned.transport.receive_in(
                    voices,
                    control,
                    voice_bearers::PlayoutWindow {
                        observed: radio,
                        earliest: earliest_playout,
                    },
                    now.settings,
                    NonZeroUsize::MIN,
                ),
            }
            .map_err(|error| InitialCircuitError::Transport { rab, error })?;
            self.counters.receive(ingress);
            if radio_active
                && matches!(
                    voices.initialization_status(owned.transport.handle()),
                    Ok(InitializationStatus::Ready)
                )
            {
                match owned
                    .transport
                    .publish_uplink(voices, radio)
                    .map_err(|error| InitialCircuitError::Transport { rab, error })?
                {
                    umts_user_plane::uplink_media::Progress::Sent => {
                        self.counters.uplink_sent = self.counters.uplink_sent.saturating_add(1)
                    }
                    umts_user_plane::uplink_media::Progress::Dropped => {
                        self.counters.uplink_dropped =
                            self.counters.uplink_dropped.saturating_add(1)
                    }
                    umts_user_plane::uplink_media::Progress::Backpressure => {
                        self.counters.uplink_backpressure =
                            self.counters.uplink_backpressure.saturating_add(1)
                    }
                    umts_user_plane::uplink_media::Progress::Idle
                    | umts_user_plane::uplink_media::Progress::Waiting => (),
                }
            }
        }
        Ok(self.progress(voices))
    }

    pub(crate) fn complete<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &mut CoreLink<C>,
        voices: &mut VoiceTraffic,
        receive: &mut DedicatedController,
        now: ControlTime,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        if identity.ue != self.request.key() {
            return Err(InitialCircuitError::Identity);
        }
        if !core.gateway().owns_rab_request(self.request) {
            return Err(InitialCircuitError::Identity);
        }
        for circuit in &mut self.circuits {
            let rab = circuit.metadata.rab;
            if matches!(
                circuit.stage,
                Stage::Prepared(_)
                    | Stage::Bound(_)
                    | Stage::Failed {
                        disposition: Disposition::Unreported,
                        ..
                    }
            ) && !authorized(core.gateway(), self.request, rab)?
            {
                circuit.stage.retire(voices, rab, Disposition::Superseded)?;
                continue;
            }
            if let Stage::Prepared(owned) | Stage::Bound(owned) = &mut circuit.stage {
                activate(voices, receive, identity, owned, rab)?;
                match voices
                    .initialization_status(owned.transport.handle())
                    .map_err(|error| InitialCircuitError::Voice { rab, error })?
                {
                    InitializationStatus::Pending => continue,
                    InitializationStatus::Failed(failure) => {
                        owned
                            .transport
                            .retire(voices)
                            .map_err(|error| InitialCircuitError::Transport { rab, error })?;
                        circuit.stage = Stage::Failed {
                            failure,
                            disposition: Disposition::Unreported,
                        };
                        self.counters.initialization_failed =
                            self.counters.initialization_failed.saturating_add(1);
                    }
                    InitializationStatus::Ready => (),
                }
                if matches!(circuit.stage, Stage::Prepared(_)) {
                    let previous = std::mem::replace(&mut circuit.stage, Stage::Unprepared);
                    let Stage::Prepared(owned) = previous else {
                        unreachable!("validated prepared CS attempt")
                    };
                    circuit.stage = Stage::Bound(owned);
                }
            }
            if let Stage::Failed {
                disposition: disposition @ Disposition::Unreported,
                ..
            } = &mut circuit.stage
            {
                // TS 25.413 8.2.2 assigns this cause when the requested user plane cannot initialize.
                let cause = Cause::from_code(RNC_UNABLE_TO_ESTABLISH_ALL_RFCS)
                    .expect("standard RANAP radio cause");
                core.gateway_mut()
                    .record_rab_outcome(
                        now,
                        self.request,
                        RabOutcome::SetupFailed(Failed::new(rab, cause)),
                    )
                    .map_err(|error| InitialCircuitError::Core { rab, error })?;
                *disposition = Disposition::Recorded;
            }
            if let Stage::Bound(owned) = &circuit.stage {
                // TS 25.413 8.2.2 requires user-plane initialization before reporting success.
                core.gateway_mut()
                    .record_rab_outcome(now, self.request, RabOutcome::Setup(owned.setup.clone()))
                    .map_err(|error| InitialCircuitError::Core { rab, error })?;
                let previous = std::mem::replace(&mut circuit.stage, Stage::Unprepared);
                let Stage::Bound(owned) = previous else {
                    unreachable!("validated bound CS attempt")
                };
                circuit.stage = Stage::Recorded(owned);
            }
        }

        Ok(self.progress(voices))
    }

    pub(crate) fn finish_failure<C: Transport>(
        &mut self,
        core: &mut CoreLink<C>,
        voices: &mut VoiceTraffic,
        now: ControlTime,
        cause: Cause,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        if !core.gateway().owns_rab_request(self.request) {
            return Err(InitialCircuitError::Identity);
        }
        self.retire(voices)?;
        for circuit in &mut self.circuits {
            let rab = circuit.metadata.rab;
            let (disposition, cause) = match &mut circuit.stage {
                Stage::Retired(disposition) => (disposition, cause.clone()),
                Stage::Failed { disposition, .. } => (
                    disposition,
                    Cause::from_code(RNC_UNABLE_TO_ESTABLISH_ALL_RFCS)
                        .expect("standard RANAP radio cause"),
                ),
                _ => unreachable!("retired CS attempt"),
            };
            if !matches!(disposition, Disposition::Unreported) {
                continue;
            }
            if !authorized(core.gateway(), self.request, rab)? {
                *disposition = Disposition::Superseded;
                continue;
            }
            core.gateway_mut()
                .record_rab_outcome(
                    now,
                    self.request,
                    RabOutcome::SetupFailed(Failed::new(rab, cause)),
                )
                .map_err(|error| InitialCircuitError::Core { rab, error })?;
            *disposition = Disposition::Recorded;
        }
        Ok(self.progress(voices))
    }
    pub(super) fn released_transports<'a>(
        &'a mut self,
        released: &'a [BearerId],
    ) -> impl Iterator<Item = &'a mut VoiceTransport<RtpEndpoint>> {
        self.circuits.iter_mut().filter_map(move |circuit| {
            if circuit
                .metadata
                .bearers
                .iter()
                .all(|id| released.iter().any(|released| released.get() == *id))
            {
                circuit.stage.owned_mut().map(|owned| &mut owned.transport)
            } else {
                None
            }
        })
    }
    pub(super) fn finish_radio_release(&mut self, released: &[BearerId]) {
        for circuit in &mut self.circuits {
            if !circuit
                .metadata
                .bearers
                .iter()
                .all(|id| released.iter().any(|released| released.get() == *id))
            {
                continue;
            }
            let disposition = match &circuit.stage {
                Stage::Recorded(_) | Stage::Draining(_) => Disposition::Recorded,
                Stage::Retired(disposition) | Stage::Failed { disposition, .. } => *disposition,
                _ => Disposition::Superseded,
            };
            circuit.stage = Stage::Retired(disposition);
        }
    }
    pub(super) fn new(
        selection: SetupAssignment<'_>,
        metadata: Vec<RabPlan>,
    ) -> Result<Self, InitialCircuitError> {
        let pending = selection.pending();
        let assignment = pending.assignment();
        if pending.request().domain() != Domain::Cs
            || assignment.setup.is_empty()
            || metadata.len() != assignment.setup.len()
            || metadata
                .iter()
                .zip(&assignment.setup)
                .any(|(rab, item)| rab.domain != CnDomain::Cs || rab.rab != item.first.id.get())
        {
            return Err(InitialCircuitError::Identity);
        }
        let mut circuits = Vec::new();
        circuits
            .try_reserve_exact(metadata.len())
            .map_err(|_| InitialCircuitError::Capacity)?;
        circuits.extend(metadata.into_iter().map(|metadata| Circuit {
            metadata,
            stage: Stage::Unprepared,
        }));
        Ok(Self {
            request: pending.request(),
            circuits,
            cursor: 0,
            counters: CircuitCounters::default(),
        })
    }

    pub fn request(&self) -> RabRequest {
        self.request
    }
    #[cfg(test)]
    pub fn counters(&self) -> CircuitCounters {
        self.counters
    }

    #[cfg(test)]
    pub(crate) fn metadata(&self, rab: u8) -> Option<&RabPlan> {
        Some(
            &self
                .circuits
                .iter()
                .find(|circuit| circuit.metadata.rab == rab)?
                .metadata,
        )
    }

    #[cfg(test)]
    pub(crate) fn endpoint(&self, rab: u8) -> Option<SocketAddr> {
        Some(
            self.circuits
                .iter()
                .find(|circuit| circuit.metadata.rab == rab)?
                .stage
                .owned()?
                .transport
                .local_address(),
        )
    }

    pub fn progress(&self, voices: &VoiceTraffic) -> CircuitProgress {
        let mut progress = CircuitProgress {
            items: self.circuits.len(),
            ..CircuitProgress::default()
        };
        for circuit in &self.circuits {
            match &circuit.stage {
                Stage::Unprepared => (),
                Stage::Prepared(_) => progress.prepared += 1,
                Stage::Bound(_) => progress.bound += 1,
                Stage::Recorded(_) | Stage::Draining(_) => progress.recorded += 1,
                Stage::Failed { disposition, .. } => {
                    progress.retired += 1;
                    progress.failed += 1;
                    match disposition {
                        Disposition::Unreported => (),
                        Disposition::Recorded => progress.recorded += 1,
                        Disposition::Superseded => progress.superseded += 1,
                    }
                }
                Stage::Retired(disposition) => {
                    progress.retired += 1;
                    match disposition {
                        Disposition::Unreported => (),
                        Disposition::Recorded => progress.recorded += 1,
                        Disposition::Superseded => progress.superseded += 1,
                    }
                }
            }
            if let Some(owned) = circuit.stage.owned() {
                let handle = owned.transport.handle();
                progress.active += usize::from(voices.radio_active(handle).unwrap_or(false));
                match voices.initialization_status(handle) {
                    Ok(InitializationStatus::Ready) => progress.ready += 1,
                    Ok(InitializationStatus::Failed(_)) => progress.failed += 1,
                    Ok(InitializationStatus::Pending) | Err(_) => (),
                }
            }
        }
        progress
    }

    pub(crate) fn outcome_ids(&self) -> RabOutcomeIds {
        let mut outcomes = RabOutcomeIds::default();
        for circuit in &self.circuits {
            match &circuit.stage {
                Stage::Recorded(_) | Stage::Draining(_) => {
                    outcomes.record_resolved(circuit.metadata.rab, true)
                }
                Stage::Failed { disposition, .. } | Stage::Retired(disposition)
                    if matches!(disposition, Disposition::Recorded | Disposition::Superseded) =>
                {
                    outcomes.record_resolved(circuit.metadata.rab, false)
                }
                Stage::Failed { .. } | Stage::Retired(_) => (),
                Stage::Unprepared | Stage::Prepared(_) | Stage::Bound(_) => (),
            }
        }
        outcomes
    }

    pub(super) fn prepared(&self) -> bool {
        self.circuits.iter().all(|circuit| {
            matches!(
                circuit.stage,
                Stage::Prepared(_) | Stage::Bound(_) | Stage::Recorded(_)
            )
        })
    }

    pub fn retire(&mut self, voices: &mut VoiceTraffic) -> Result<usize, InitialCircuitError> {
        let mut count = 0;
        for circuit in &mut self.circuits {
            let disposition = match &circuit.stage {
                Stage::Recorded(_) | Stage::Draining(_) => Disposition::Recorded,
                Stage::Failed { disposition, .. } => *disposition,
                Stage::Retired(disposition) => *disposition,
                _ => Disposition::Unreported,
            };
            count += circuit
                .stage
                .retire(voices, circuit.metadata.rab, disposition)?;
        }
        Ok(count)
    }
}

impl Rel99Admission {
    #[cfg(test)]
    pub fn circuit_metadata(&self, identity: ReceiveIdentity, rab: u8) -> Option<&RabPlan> {
        self.entry(identity)?
            .circuit_assignment
            .as_ref()?
            .metadata(rab)
    }
    #[cfg(test)]
    pub fn circuit_endpoint(&self, identity: ReceiveIdentity, rab: u8) -> Option<SocketAddr> {
        self.entry(identity)?
            .circuit_assignment
            .as_ref()?
            .endpoint(rab)
    }
    #[cfg(test)]
    pub fn circuit_counters(&self, identity: ReceiveIdentity) -> Option<CircuitCounters> {
        Some(
            self.entry(identity)?
                .circuit_assignment
                .as_ref()?
                .counters(),
        )
    }
    pub fn circuit_progress(
        &self,
        identity: ReceiveIdentity,
        voices: &VoiceTraffic,
    ) -> Option<CircuitProgress> {
        Some(
            self.entry(identity)?
                .circuit_assignment
                .as_ref()?
                .progress(voices),
        )
    }

    pub fn prepare_circuits<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &Gateway<C>,
        voices: &mut VoiceTraffic,
        settings: impl FnMut(&RabPlan, &SetupOrModify) -> Result<CircuitSettings, InitialCircuitError>,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(InitialCircuitError::Identity)?;
        if matches!(
            entry.lifecycle.phase(),
            AdmissionPhase::Active | AdmissionPhase::Stopping | AdmissionPhase::Retired
        ) || entry.procedure.phase() != SetupPhase::Prepared
        {
            return Err(InitialCircuitError::Phase);
        }
        let source = entry
            .rab_assignment
            .as_ref()
            .ok_or(InitialCircuitError::Identity)?;
        let state = entry
            .circuit_assignment
            .as_mut()
            .ok_or(InitialCircuitError::Phase)?;
        let plan = entry
            .pool
            .as_ref()
            .ok_or(InitialCircuitError::Phase)?
            .plan();
        state.prepare(
            CircuitPreparation {
                pending: source.selection().ok_or(InitialCircuitError::Identity)?,
                identity,
                plan,
                gateway: core,
            },
            voices,
            settings,
        )
    }

    #[cfg(test)]
    pub fn service_circuits<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &Gateway<C>,
        voices: &mut VoiceTraffic,
        receive: &mut DedicatedController,
        now: CircuitTime,
        budget: NonZeroUsize,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        self.service_circuits_with(
            identity,
            core,
            voices,
            receive,
            CircuitIngressTime::Anchored(now),
            budget,
        )
    }

    pub fn service_circuits_at<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &Gateway<C>,
        voices: &mut VoiceTraffic,
        receive: &mut DedicatedController,
        now: CircuitPlayoutTime,
        budget: NonZeroUsize,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        self.service_circuits_with(
            identity,
            core,
            voices,
            receive,
            CircuitIngressTime::Playout(now),
            budget,
        )
    }

    fn service_circuits_with<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &Gateway<C>,
        voices: &mut VoiceTraffic,
        receive: &mut DedicatedController,
        now: CircuitIngressTime,
        budget: NonZeroUsize,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(InitialCircuitError::Identity)?;
        if matches!(
            entry.lifecycle.phase(),
            AdmissionPhase::Stopping | AdmissionPhase::Retired
        ) {
            return Err(InitialCircuitError::Phase);
        }
        // A retained frame is already filled even when its worker command is backpressured.
        let earliest_playout = match entry.frame.as_ref() {
            Some(frame) => ChipInstant {
                epoch: frame.start.epoch,
                index: frame
                    .start
                    .index
                    .checked_add(CHIPS_PER_FRAME)
                    .ok_or(InitialCircuitError::Configuration)?,
            },
            None => entry.next_frame,
        };
        let radio_active = entry.lifecycle.phase() == AdmissionPhase::Active;
        let state = entry
            .circuit_assignment
            .as_mut()
            .ok_or(InitialCircuitError::Phase)?;
        state.service(
            CircuitService {
                identity,
                gateway: core,
                receive,
                time: now,
                earliest_playout,
                radio_active,
            },
            voices,
            budget,
        )
    }

    pub fn complete_circuits<C: Transport>(
        &mut self,
        identity: ReceiveIdentity,
        core: &mut CoreLink<C>,
        voices: &mut VoiceTraffic,
        receive: &mut DedicatedController,
        now: ControlTime,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        let entry = self
            .entry_mut(identity)
            .ok_or(InitialCircuitError::Identity)?;
        if entry.lifecycle.phase() != AdmissionPhase::Active {
            return Err(InitialCircuitError::Phase);
        }
        let state = entry
            .circuit_assignment
            .as_mut()
            .ok_or(InitialCircuitError::Phase)?;
        let progress = state.complete(identity, core, voices, receive, now)?;
        let outcomes = state.outcome_ids();
        let request = state.request;
        if outcomes.resolved().len() == progress.items
            && entry
                .inventory
                .as_ref()
                .is_some_and(|inventory| inventory.initial_pending())
        {
            entry
                .inventory
                .as_mut()
                .expect("checked initial inventory")
                .record_initial_outcomes(request, &outcomes)
                .map_err(InitialCircuitError::Inventory)?;
        }
        Ok(progress)
    }
}

impl RetiredAdmission {
    pub fn finish_circuit_failure<C: Transport>(
        &mut self,
        core: &mut CoreLink<C>,
        voices: &mut VoiceTraffic,
        now: ControlTime,
        cause: Cause,
    ) -> Result<CircuitProgress, InitialCircuitError> {
        let state = self
            .circuit_assignment
            .as_mut()
            .ok_or(InitialCircuitError::Phase)?;
        state.finish_failure(core, voices, now, cause)
    }
}

fn authorized<C: Transport>(
    core: &Gateway<C>,
    request: RabRequest,
    rab: u8,
) -> Result<bool, InitialCircuitError> {
    if !core.owns_rab_request(request) {
        return Err(InitialCircuitError::Identity);
    }
    super::rabs::authorization(core, request, rab)
        .map(|proof| proof.is_some())
        .map_err(|error| InitialCircuitError::Core { rab, error })
}

fn activate(
    voices: &mut VoiceTraffic,
    receive: &mut DedicatedController,
    identity: ReceiveIdentity,
    owned: &OwnedCircuit,
    rab: u8,
) -> Result<(), InitialCircuitError> {
    receive
        .with_active_bank(identity, |bank| {
            voices.activate(owned.transport.handle(), bank)
        })
        .map_err(|error| InitialCircuitError::Radio { rab, error })?
        .map_err(|error| InitialCircuitError::Voice { rab, error })
}

fn prepare(
    voices: &mut VoiceTraffic,
    identity: ReceiveIdentity,
    plan: &Arc<CompiledRel99>,
    request: RabRequest,
    metadata: &RabPlan,
    item: &SetupOrModify,
    settings: CircuitSettings,
) -> Result<OwnedCircuit, InitialCircuitError> {
    let rab = metadata.rab;
    let Peer::Cs(peer) = item
        .first
        .transport
        .as_ref()
        .ok_or(InitialCircuitError::Identity)?
        .ip_peer(Domain::Cs)
        .map_err(|error| InitialCircuitError::Protocol { rab, error })?
    else {
        return Err(InitialCircuitError::Identity);
    };
    let endpoint = RtpEndpoint::bind(settings.local)
        .map_err(|error| InitialCircuitError::Io { rab, error })?;
    let local = endpoint
        .local_address()
        .map_err(|error| InitialCircuitError::Io { rab, error })?;
    let address = settings.advertised.unwrap_or(local.ip());
    if address.is_unspecified() || address.is_multicast() || address.is_ipv4() != local.is_ipv4() {
        return Err(InitialCircuitError::Configuration);
    }
    let mut setup = Setup::new(rab);
    setup.address = Some(
        Address::from_ip(Domain::Cs, address)
            .map_err(|error| InitialCircuitError::Protocol { rab, error })?,
    );
    let port = local.port().to_be_bytes();
    setup.association = Some(Association::binding([port[0], port[1], 0, 0]));
    let bearers = metadata
        .bearers
        .iter()
        .map(|id| BearerId::new(*id).map_err(|_| InitialCircuitError::Identity))
        .collect::<Result<Vec<_>, _>>()?;
    let implemented = match settings.initialization {
        CircuitInitialization::Initiate(settings) => settings.supported(),
        CircuitInitialization::Receive(versions) => versions,
    };
    let versions = Versions::new(
        metadata
            .circuit_versions
            .ok_or(InitialCircuitError::Identity)?,
    )
    .map_err(|_| InitialCircuitError::Configuration)?
    .intersection(implemented)
    .ok_or(InitialCircuitError::Configuration)?;
    let interval = metadata
        .circuit_interval
        .ok_or(InitialCircuitError::Identity)?;
    let frames = match interval {
        umts_rrc::transport::TransportInterval::Ms10 => 1,
        umts_rrc::transport::TransportInterval::Ms20 => 2,
        umts_rrc::transport::TransportInterval::Ms40 => 4,
        umts_rrc::transport::TransportInterval::Ms80 => 8,
    };
    let interval_chips =
        NonZeroU64::new(frames * CHIPS_PER_FRAME).ok_or(InitialCircuitError::Configuration)?;
    let parameters = item
        .first
        .parameters
        .as_ref()
        .ok_or(InitialCircuitError::Identity)?;
    let delivery = parameters
        .subflows
        .as_slice()
        .iter()
        .map(|flow| match flow.delivery {
            qos::Delivery::No => Delivery::No,
            qos::Delivery::Yes => Delivery::Yes,
            qos::Delivery::NoDetection => Delivery::NoDetect,
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let handle = voices
        .prepare(
            VoiceKey {
                connection: ConnectionKey {
                    ue: request.key(),
                    domain: CnDomain::Cs,
                },
                rab,
            },
            identity,
            plan.clone(),
            &bearers,
            versions,
            voice_bearers::UplinkPolicy {
                delivery,
                interval_chips,
                wait_chips: settings.wait_chips,
                timestamp: settings.timestamp,
                rtp: settings.source,
            },
        )
        .map_err(|error| InitialCircuitError::Voice { rab, error })?;
    let initialized = match settings.initialization {
        CircuitInitialization::Initiate(settings) => {
            voices.start_rab_initialization(handle, metadata, item, settings)
        }
        CircuitInitialization::Receive(_) => {
            voices.constrain_rab_initialization(handle, metadata, item, versions)
        }
    };
    if let Err(error) = initialized {
        voices.release(handle).expect("fresh unattached CS route");
        return Err(InitialCircuitError::Voice { rab, error });
    }
    match voices.attach_transport(handle, endpoint, peer, settings.ingress) {
        Ok(transport) => Ok(OwnedCircuit {
            transport,
            setup,
            timestamp: settings.timestamp,
        }),
        Err(error) => {
            voices.release(handle).expect("fresh unattached CS route");
            Err(InitialCircuitError::Transport { rab, error })
        }
    }
}
