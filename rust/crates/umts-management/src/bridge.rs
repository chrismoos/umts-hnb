// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::proto::{self, OperationPhase};
use arc_swap::ArcSwap;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, mpsc};
use tonic::{Code, Status};
use uuid::Uuid;

pub const MAXIMUM_PAGE_SIZE: usize = 128;
pub const MAXIMUM_UES: usize = 512;
pub const MAXIMUM_EVENTS: usize = 256;
pub const MAXIMUM_OPERATIONS: usize = 128;
pub const MAXIMUM_CAPTURES: usize = 32;
pub const COMMAND_CAPACITY: usize = 8;
pub const MAXIMUM_CAPTURE_MIB: u32 = 512;
pub const MAXIMUM_CAPTURE_DURATION_MS: u32 = 30_000;
pub const OPERATION_RETENTION: Duration = Duration::from_secs(300);
pub const COMMAND_LIFETIME: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub struct Snapshot {
    pub status: proto::StatusSnapshot,
    pub configuration: proto::Configuration,
    pub ues: Vec<proto::Ue>,
    pub radio_counters: proto::Counters,
}

impl Snapshot {
    pub fn metrics(&self) -> proto::RadioMetrics {
        proto::RadioMetrics {
            identity: self.status.identity.clone(),
            counters: Some(self.radio_counters.clone()),
            ues: self.ues.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    StartCapture {
        direction: proto::CaptureDirection,
        duration_ms: u32,
        maximum_mib: u32,
    },
    StopCapture {
        capture_id: String,
    },
    Shutdown,
}

impl Action {
    fn kind(&self) -> &'static str {
        match self {
            Self::StartCapture { .. } => "capture.start",
            Self::StopCapture { .. } => "capture.stop",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Debug)]
pub struct Command {
    pub operation_id: String,
    pub expected_epoch: u64,
    pub expires: Instant,
    pub action: Action,
}

pub enum Completion {
    Running {
        operation_id: String,
        capture_id: String,
    },
    Finished {
        operation_id: String,
        result: Result<(), (Code, String)>,
    },
    Capture(Box<proto::Capture>),
}

pub struct Controller {
    state: Arc<State>,
    commands: mpsc::Receiver<Command>,
    completions: mpsc::Sender<Completion>,
    sequence: u64,
}

#[derive(Clone)]
pub struct CompletionSink(mpsc::Sender<Completion>);

impl CompletionSink {
    pub fn send_blocking(&self, completion: Completion) -> Result<(), Completion> {
        self.0.blocking_send(completion).map_err(|error| error.0)
    }
}

pub struct Driver {
    state: Arc<State>,
    completions: mpsc::Receiver<Completion>,
    previous: Arc<Snapshot>,
    controller_closed: bool,
}

struct Entry {
    command: proto::CommandIdentity,
    action: Action,
    operation: proto::Operation,
    finished: Option<Instant>,
}

#[derive(Default)]
struct Registry {
    operations: VecDeque<Entry>,
    captures: VecDeque<proto::Capture>,
    events: VecDeque<proto::Event>,
    event_sequence: u64,
}

pub struct State {
    instance_id: String,
    began: Instant,
    snapshot: ArcSwap<Snapshot>,
    registry: Mutex<Registry>,
    commands: mpsc::Sender<Command>,
    pub(crate) event_notify: Notify,
}

pub fn channel(initial: Snapshot) -> (Controller, Driver, Arc<State>) {
    let (sender, commands) = mpsc::channel(COMMAND_CAPACITY);
    let (completions, completed) = mpsc::channel(MAXIMUM_OPERATIONS);
    let previous = Arc::new(initial);
    let state = Arc::new(State {
        instance_id: Uuid::new_v4().to_string(),
        began: Instant::now(),
        snapshot: ArcSwap::from(Arc::clone(&previous)),
        registry: Mutex::new(Registry::default()),
        commands: sender,
        event_notify: Notify::new(),
    });
    let mut controller = Controller {
        state: Arc::clone(&state),
        commands,
        completions,
        sequence: 0,
    };
    controller
        .publish((*previous).clone())
        .expect("initial management snapshot fits");
    let driver = Driver {
        state: Arc::clone(&state),
        completions: completed,
        previous,
        controller_closed: false,
    };
    (controller, driver, state)
}

impl Controller {
    pub fn publish(&mut self, mut snapshot: Snapshot) -> Result<(), &'static str> {
        if snapshot.ues.len() > MAXIMUM_UES {
            return Err("management UE snapshot exceeds its bound");
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("snapshot sequence exhausted")?;
        snapshot.status.identity = Some(proto::SnapshotIdentity {
            instance_id: self.state.instance_id.clone(),
            sequence: self.sequence,
            epoch: snapshot.status.identity.as_ref().map_or(0, |id| id.epoch),
            uptime_ms: u64::try_from(self.state.began.elapsed().as_millis()).unwrap_or(u64::MAX),
            observed_unix_ms: unix_ms(),
        });
        self.state.snapshot.store(Arc::new(snapshot));
        Ok(())
    }

    pub fn try_next(&mut self) -> Option<Command> {
        self.commands.try_recv().ok()
    }

    pub fn close_commands(&mut self) {
        self.commands.close();
    }

    // A full completion queue returns custody rather than losing an operation outcome.
    pub fn try_complete(&self, completion: Completion) -> Result<(), Completion> {
        self.completions
            .try_send(completion)
            .map_err(|error| error.into_inner())
    }

    pub fn instance_id(&self) -> &str {
        &self.state.instance_id
    }

    pub fn completion_sink(&self) -> CompletionSink {
        CompletionSink(self.completions.clone())
    }
}

impl State {
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.load_full()
    }

    pub fn status(&self) -> Result<proto::StatusSnapshot, Status> {
        let mut status = self.snapshot().status.clone();
        if self.commands.is_closed() && status.phase != i32::from(proto::Phase::Stopped) {
            status.phase = proto::Phase::Failed.into();
            status.radio_ready = false;
            status.failure_reason = "controller is no longer available".into();
        }
        status.pending_operations = self
            .registry
            .lock()
            .map_err(poisoned)?
            .operations
            .iter()
            .filter(|entry| entry.finished.is_none())
            .count() as u32;
        Ok(status)
    }

    pub fn submit(
        &self,
        command: proto::CommandIdentity,
        action: Action,
        lifetime: Duration,
    ) -> Result<proto::Operation, Status> {
        Uuid::parse_str(&command.request_id)
            .map_err(|_| Status::invalid_argument("request_id must be a UUID"))?;
        if command.expected_instance_id != self.instance_id {
            return Err(Status::failed_precondition(
                "daemon instance changed, refresh status",
            ));
        }
        if lifetime.is_zero() {
            return Err(Status::deadline_exceeded("command deadline elapsed"));
        }
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry.prune();
        if let Some(previous) = registry
            .operations
            .iter()
            .find(|entry| entry.command.request_id == command.request_id)
        {
            return if previous.command == command && previous.action == action {
                Ok(previous.operation.clone())
            } else {
                Err(Status::already_exists(
                    "request_id was used with different parameters",
                ))
            };
        }
        let snapshot = self.snapshot();
        let epoch = snapshot
            .status
            .identity
            .as_ref()
            .map_or(0, |identity| identity.epoch);
        if command.expected_epoch != epoch {
            return Err(Status::failed_precondition(
                "radio epoch changed, refresh status",
            ));
        }
        if registry.operations.len() == MAXIMUM_OPERATIONS {
            return Err(Status::resource_exhausted(
                "operation retention budget exhausted",
            ));
        }
        if !matches!(action, Action::Shutdown) && !snapshot.status.radio_ready {
            return Err(Status::failed_precondition("radio is not ready"));
        }
        let id = Uuid::new_v4().to_string();
        let now = unix_ms();
        let operation = proto::Operation {
            id: id.clone(),
            request_id: command.request_id.clone(),
            phase: OperationPhase::Queued.into(),
            kind: action.kind().into(),
            created_unix_ms: now,
            updated_unix_ms: now,
            ..Default::default()
        };
        let queued = Command {
            operation_id: id,
            expected_epoch: epoch,
            expires: Instant::now() + lifetime.min(COMMAND_LIFETIME),
            action: action.clone(),
        };
        self.commands
            .try_send(queued)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    Status::resource_exhausted("controller command queue full")
                }
                mpsc::error::TrySendError::Closed(_) => Status::unavailable("controller stopped"),
            })?;
        registry.operations.push_back(Entry {
            command,
            action,
            operation: operation.clone(),
            finished: None,
        });
        registry.event("operation.queued", &operation.kind, &operation.id, epoch)?;
        self.event_notify.notify_waiters();
        Ok(operation)
    }

    pub fn operation(
        &self,
        selector: &proto::get_operation_request::Selector,
    ) -> Result<proto::Operation, Status> {
        let mut registry = self.registry.lock().map_err(poisoned)?;
        registry.prune();
        registry
            .operations
            .iter()
            .find(|entry| match selector {
                proto::get_operation_request::Selector::OperationId(id) => {
                    entry.operation.id == *id
                }
                proto::get_operation_request::Selector::RequestId(id) => {
                    entry.command.request_id == *id
                }
            })
            .map(|entry| entry.operation.clone())
            .ok_or_else(|| Status::not_found("operation not retained"))
    }

    pub fn captures(&self) -> Result<Vec<proto::Capture>, Status> {
        Ok(self
            .registry
            .lock()
            .map_err(poisoned)?
            .captures
            .iter()
            .cloned()
            .collect())
    }

    pub fn event_tail(&self) -> Result<u64, Status> {
        Ok(self.registry.lock().map_err(poisoned)?.event_sequence)
    }

    pub fn events_after(&self, sequence: u64) -> Result<Vec<proto::Event>, Status> {
        let registry = self.registry.lock().map_err(poisoned)?;
        if sequence > registry.event_sequence {
            return Err(Status::out_of_range(
                "event cursor belongs to a future or different instance",
            ));
        }
        let first = registry
            .events
            .front()
            .map_or(registry.event_sequence, |event| event.sequence);
        let mut output = Vec::with_capacity(MAXIMUM_EVENTS + 1);
        if sequence.saturating_add(1) < first {
            output.push(proto::Event {
                sequence: first - 1,
                observed_unix_ms: unix_ms(),
                kind: "stream.gap".into(),
                message: "event history expired, refresh status".into(),
                lost_events: first - sequence - 1,
                ..Default::default()
            });
        }
        output.extend(
            registry
                .events
                .iter()
                .filter(|event| event.sequence > sequence)
                .cloned(),
        );
        for event in &mut output {
            event.instance_id = self.instance_id.clone();
        }
        Ok(output)
    }
}

impl Driver {
    pub fn poll(&mut self) -> Result<(), Status> {
        let mut registry = self.state.registry.lock().map_err(poisoned)?;
        let previous_event_sequence = registry.event_sequence;
        for _ in 0..MAXIMUM_OPERATIONS {
            let Ok(completion) = self.completions.try_recv() else {
                break;
            };
            match completion {
                Completion::Capture(capture) => {
                    let capture = *capture;
                    let phase_changed = registry
                        .captures
                        .iter()
                        .find(|value| value.id == capture.id)
                        .is_none_or(|value| value.phase != capture.phase);
                    if phase_changed {
                        registry.event("capture.state", &capture.id, "", capture.epoch)?;
                    }
                    if let Some(existing) = registry
                        .captures
                        .iter_mut()
                        .find(|value| value.id == capture.id)
                    {
                        *existing = capture;
                    } else {
                        if registry.captures.len() == MAXIMUM_CAPTURES {
                            let terminal = registry.captures.iter().position(|value| {
                                matches!(
                                    proto::CapturePhase::try_from(value.phase),
                                    Ok(proto::CapturePhase::Complete | proto::CapturePhase::Failed)
                                )
                            });
                            if let Some(index) = terminal {
                                registry.captures.remove(index);
                            } else {
                                return Err(Status::resource_exhausted("capture history full"));
                            }
                        }
                        registry.captures.push_back(capture);
                    }
                }
                Completion::Running {
                    operation_id,
                    capture_id,
                } => {
                    let Some(entry) = registry
                        .operations
                        .iter_mut()
                        .find(|entry| entry.operation.id == operation_id)
                    else {
                        return Err(Status::internal("controller returned unknown operation"));
                    };
                    if entry.finished.is_none() {
                        entry.operation.phase = OperationPhase::Running.into();
                        entry.operation.capture_id = capture_id;
                        entry.operation.updated_unix_ms = unix_ms();
                    }
                }
                Completion::Finished {
                    operation_id,
                    result,
                } => {
                    let Some(entry) = registry
                        .operations
                        .iter_mut()
                        .find(|entry| entry.operation.id == operation_id)
                    else {
                        return Err(Status::internal("controller returned unknown operation"));
                    };
                    if entry.finished.is_none() {
                        match result {
                            Ok(()) => entry.operation.phase = OperationPhase::Succeeded.into(),
                            Err((code, message)) => {
                                entry.operation.phase = OperationPhase::Failed.into();
                                entry.operation.error_code = code.into();
                                entry.operation.error_message = message;
                            }
                        }
                        entry.operation.updated_unix_ms = unix_ms();
                        entry.finished = Some(Instant::now());
                        let kind = entry.operation.kind.clone();
                        registry.event("operation.finished", &kind, &operation_id, 0)?;
                    }
                }
            }
        }
        let current = self.state.snapshot();
        if self.completions.is_closed() && !self.controller_closed {
            self.controller_closed = true;
            for entry in &mut registry.operations {
                if entry.finished.is_none() {
                    entry.operation.phase = OperationPhase::Failed.into();
                    entry.operation.error_code = Code::Unavailable.into();
                    entry.operation.error_message =
                        "controller closed without a terminal operation result".into();
                    entry.operation.updated_unix_ms = unix_ms();
                    entry.finished = Some(Instant::now());
                }
            }
            registry.event(
                "controller.closed",
                "controller stopped without further updates",
                "",
                current.status.identity.as_ref().map_or(0, |id| id.epoch),
            )?;
        }
        if current.status.identity == self.previous.status.identity {
            if registry.event_sequence != previous_event_sequence {
                self.state.event_notify.notify_waiters();
            }
            return Ok(());
        }
        if current.status.phase != self.previous.status.phase
            || current.status.radio_ready != self.previous.status.radio_ready
        {
            registry.event(
                "runtime.state",
                proto::Phase::try_from(current.status.phase)
                    .map_or("UNSPECIFIED", |phase| phase.as_str_name()),
                "",
                current
                    .status
                    .identity
                    .as_ref()
                    .map_or(0, |value| value.epoch),
            )?;
        }
        let old_keys = self
            .previous
            .ues
            .iter()
            .filter_map(|ue| ue.reference.as_ref())
            .map(|key| (key.epoch, key.slot, key.generation))
            .collect::<HashSet<_>>();
        let new_keys = current
            .ues
            .iter()
            .filter_map(|ue| ue.reference.as_ref())
            .map(|key| (key.epoch, key.slot, key.generation))
            .collect::<HashSet<_>>();
        for ue in &current.ues {
            if ue
                .reference
                .as_ref()
                .is_some_and(|key| !old_keys.contains(&(key.epoch, key.slot, key.generation)))
            {
                registry.event(
                    "ue.registered",
                    "UE context published",
                    "",
                    ue.reference.as_ref().map_or(0, |value| value.epoch),
                )?;
                if let Some(event) = registry.events.back_mut() {
                    event.ue = ue.reference
                }
            }
        }
        let old_ues: HashMap<_, _> = self
            .previous
            .ues
            .iter()
            .filter_map(|ue| {
                ue.reference
                    .map(|key| ((key.epoch, key.slot, key.generation), ue))
            })
            .collect();
        for ue in &current.ues {
            let Some(key) = ue.reference else {
                continue;
            };
            let Some(previous) = old_ues.get(&(key.epoch, key.slot, key.generation)) else {
                continue;
            };
            if ue.rrc_state != previous.rrc_state
                || ue.cs_state != previous.cs_state
                || ue.ps_state != previous.ps_state
                || ue.configuration != previous.configuration
                || ue.pending_procedure != previous.pending_procedure
                || ue.bearers != previous.bearers
            {
                registry.event(
                    "ue.state",
                    "UE procedure, domain or bearer state changed",
                    "",
                    key.epoch,
                )?;
                registry.events.back_mut().expect("event appended").ue = Some(key);
            }
        }
        for ue in &self.previous.ues {
            if ue
                .reference
                .as_ref()
                .is_some_and(|key| !new_keys.contains(&(key.epoch, key.slot, key.generation)))
            {
                registry.event(
                    "ue.released",
                    "UE context retired",
                    "",
                    ue.reference.as_ref().map_or(0, |value| value.epoch),
                )?;
                if let Some(event) = registry.events.back_mut() {
                    event.ue = ue.reference
                }
            }
        }
        self.previous = current;
        if registry.event_sequence != previous_event_sequence {
            self.state.event_notify.notify_waiters();
        }
        Ok(())
    }

    pub fn controller_closed(&self) -> bool {
        self.completions.is_closed() && self.completions.is_empty()
    }
}

impl Registry {
    fn prune(&mut self) {
        self.operations.retain(|entry| {
            entry
                .finished
                .is_none_or(|finished| finished.elapsed() < OPERATION_RETENTION)
        });
    }

    fn event(
        &mut self,
        kind: &str,
        message: &str,
        operation: &str,
        epoch: u64,
    ) -> Result<(), Status> {
        self.event_sequence = self
            .event_sequence
            .checked_add(1)
            .ok_or_else(|| Status::internal("event sequence exhausted"))?;
        if self.events.len() == MAXIMUM_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(proto::Event {
            sequence: self.event_sequence,
            observed_unix_ms: unix_ms(),
            kind: kind.into(),
            message: message.into(),
            operation_id: operation.into(),
            epoch,
            ..Default::default()
        });
        Ok(())
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Status {
    Status::internal("management registry unavailable")
}

pub fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
