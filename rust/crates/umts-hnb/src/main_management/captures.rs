// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::radio_io::capture::live::{self, Control, EndReason, Port, Recording};
use anyhow::{Context, Result, anyhow, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tonic::Code;
use umts_management::{
    bridge::{
        Action, Command, Completion, CompletionSink, MAXIMUM_CAPTURES, MAXIMUM_OPERATIONS, Snapshot,
    },
    proto,
};

const QUEUE_CAPACITY: usize = 8;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const MAXIMUM_DIRECTORY_ENTRIES: usize = 4096;
const MIB: u64 = 1024 * 1024;
const DISK_RESERVE_BYTES: u64 = 1024 * MIB;
const MAXIMUM_MANIFEST_BYTES: u64 = MIB;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

enum Request {
    Attach([Port; 2]),
    Command(Command, Box<Snapshot>),
    Finish,
}

pub struct Captures {
    sender: SyncSender<Request>,
    worker: Option<JoinHandle<Result<()>>>,
}

struct Active {
    row: proto::Capture,
    control: Control,
    start_operation: String,
    stop_operations: Vec<String>,
    last_report: Instant,
    manifest_bytes: u64,
}

struct Owner {
    root: PathBuf,
    disk_budget: u64,
    cell_context: serde_json::Value,
    ports: Option<[Port; 2]>,
    pending_ports: Option<[Port; 2]>,
    active: [Option<Active>; 2],
    history: VecDeque<proto::Capture>,
    sink: CompletionSink,
}

impl Captures {
    pub fn start(
        root: PathBuf,
        maximum_disk_mib: u32,
        sink: CompletionSink,
        cell_context: serde_json::Value,
    ) -> Result<Self> {
        private_root(&root)?;
        let disk_budget = u64::from(maximum_disk_mib)
            .checked_mul(MIB)
            .context("capture disk budget overflow")?;
        let _ = retained_bytes(&root)?;
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let (ready, started) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("hnb-capture-io".into())
            .spawn(move || {
                umts_management::server::normal_priority()?;
                ready
                    .send(())
                    .map_err(|_| anyhow!("capture I/O startup owner disappeared"))?;
                Owner {
                    root,
                    disk_budget,
                    cell_context,
                    ports: None,
                    pending_ports: None,
                    active: [None, None],
                    history: VecDeque::with_capacity(MAXIMUM_CAPTURES),
                    sink,
                }
                .run(receiver)
            })
            .context("start management capture I/O worker")?;
        if started.recv_timeout(STARTUP_TIMEOUT).is_err() {
            drop(sender);
            worker
                .join()
                .map_err(|_| anyhow!("capture I/O startup panicked"))??;
            return Err(anyhow!("capture I/O startup did not become ready"));
        }
        Ok(Self {
            sender,
            worker: Some(worker),
        })
    }

    pub fn attach(&self, ports: [Port; 2]) -> Result<(), Box<[Port; 2]>> {
        self.sender
            .try_send(Request::Attach(ports))
            .map_err(|error| {
                match match error {
                    TrySendError::Full(request) | TrySendError::Disconnected(request) => request,
                } {
                    Request::Attach(ports) => Box::new(ports),
                    _ => unreachable!("matching request"),
                }
            })
    }

    pub fn dispatch(
        &self,
        command: Command,
        snapshot: Box<Snapshot>,
    ) -> Result<(), (Code, Box<Command>)> {
        self.sender
            .try_send(Request::Command(command, snapshot))
            .map_err(|error| {
                let code = match &error {
                    TrySendError::Full(_) => Code::ResourceExhausted,
                    TrySendError::Disconnected(_) => Code::Unavailable,
                };
                match match error {
                    TrySendError::Full(request) | TrySendError::Disconnected(request) => request,
                } {
                    Request::Command(command, _) => (code, Box::new(command)),
                    _ => unreachable!("matching request"),
                }
            })
    }

    pub fn finish(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            let sent = self.sender.send(Request::Finish);
            let result = worker
                .join()
                .map_err(|_| anyhow!("capture I/O worker panicked"))?;
            result?;
            sent.context("capture I/O worker stopped before shutdown")?;
        }
        Ok(())
    }
}

impl Drop for Captures {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

impl Owner {
    fn emit(&self, completion: Completion) -> Result<()> {
        self.sink
            .send_blocking(completion)
            .map_err(|_| anyhow!("management result consumer closed"))
    }

    fn outcome(&self, operation_id: String, result: Result<(), (Code, String)>) -> Result<()> {
        self.emit(Completion::Finished {
            operation_id,
            result,
        })
    }

    fn run(mut self, receiver: Receiver<Request>) -> Result<()> {
        loop {
            self.collect()?;
            match receiver.recv_timeout(POLL_INTERVAL) {
                Ok(Request::Attach(ports)) => {
                    if self.active.iter().all(Option::is_none) {
                        self.ports = Some(ports);
                    } else {
                        self.pending_ports = Some(ports);
                    }
                }
                Ok(Request::Command(command, snapshot)) => self.command(command, *snapshot)?,
                Ok(Request::Finish) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.collect()?;
                    ensure!(
                        self.active.iter().all(Option::is_none),
                        "capture owners are still retained by radio workers"
                    );
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            }
        }
    }

    fn command(&mut self, command: Command, snapshot: Snapshot) -> Result<()> {
        if Instant::now() >= command.expires {
            return self.outcome(
                command.operation_id,
                Err((
                    Code::DeadlineExceeded,
                    "capture command expired before execution".into(),
                )),
            );
        }
        if self.pending_ports.is_some()
            || self
                .ports
                .as_ref()
                .is_none_or(|ports| ports[0].geometry().epoch != command.expected_epoch)
        {
            return self.outcome(
                command.operation_id,
                Err((
                    Code::FailedPrecondition,
                    "capture radio epoch is not attached".into(),
                )),
            );
        }
        let operation = command.operation_id;
        match command.action {
            Action::StartCapture {
                direction,
                duration_ms,
                maximum_mib,
            } => self.start(operation, direction, duration_ms, maximum_mib, snapshot),
            Action::StopCapture { capture_id } => self.stop(operation, &capture_id),
            Action::Shutdown => self.outcome(
                operation,
                Err((Code::Internal, "shutdown is not a capture command".into())),
            ),
        }
    }

    fn start(
        &mut self,
        operation: String,
        direction: proto::CaptureDirection,
        duration_ms: u32,
        maximum_mib: u32,
        snapshot: Snapshot,
    ) -> Result<()> {
        let index = match direction {
            proto::CaptureDirection::Rx => 0,
            proto::CaptureDirection::Tx => 1,
            proto::CaptureDirection::Unspecified => {
                return self.outcome(
                    operation,
                    Err((Code::InvalidArgument, "capture direction missing".into())),
                );
            }
        };
        if self.active[index].is_some() {
            return self.outcome(
                operation,
                Err((
                    Code::ResourceExhausted,
                    "a capture is already active for this direction".into(),
                )),
            );
        }
        let reserved: u64 = self
            .active
            .iter()
            .flatten()
            .map(|capture| u64::from(capture.row.maximum_mib) * MIB + MAXIMUM_MANIFEST_BYTES)
            .sum();
        let required = u64::from(maximum_mib) * MIB + MAXIMUM_MANIFEST_BYTES;
        let admission = (|| -> Result<()> {
            let used = retained_bytes(&self.root)?;
            ensure!(
                used.checked_add(reserved)
                    .and_then(|total| total.checked_add(required))
                    .is_some_and(|total| total <= self.disk_budget),
                "retained capture disk budget exhausted"
            );
            ensure!(
                available_bytes(&self.root)? >= DISK_RESERVE_BYTES + reserved + required,
                "capture would consume reserved free disk space"
            );
            Ok(())
        })();
        if let Err(error) = admission {
            return self.outcome(
                operation,
                Err((Code::ResourceExhausted, format!("{error:#}"))),
            );
        }
        let port = &mut self.ports.as_mut().expect("attached capture ports")[index];
        let geometry = port.geometry();
        let id = uuid::Uuid::new_v4().to_string();
        let path = self.root.join(&id);
        let row = proto::Capture {
            id: id.clone(),
            epoch: geometry.epoch,
            direction: direction.into(),
            phase: proto::CapturePhase::Preparing.into(),
            maximum_mib,
            duration_ms,
            directory: path.display().to_string(),
            sample_rate_hz: geometry.sample_rate.hz(),
            tick_rate_hz: geometry.tick_rate.get(),
            sample_format: "fc32-le".into(),
            ..Default::default()
        };
        self.emit(Completion::Running {
            operation_id: operation.clone(),
            capture_id: id,
        })?;
        self.emit(Completion::Capture(Box::new(row.clone())))?;
        let recording = Recording::new(
            path,
            maximum_mib,
            geometry,
            Duration::from_millis(u64::from(duration_ms)),
        );
        match recording {
            Ok(recording) => {
                let manifest_bytes = match write_manifest(
                    Path::new(&row.directory),
                    &snapshot,
                    &self.cell_context,
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return self.start_failed(
                            operation,
                            row,
                            format!("write capture manifest: {error:#}"),
                        );
                    }
                };
                let control = recording.control();
                if self.ports.as_mut().expect("capture ports")[index]
                    .publish(recording)
                    .is_err()
                {
                    return self.start_failed(
                        operation,
                        row,
                        "radio recorder is no longer available".into(),
                    );
                }
                self.active[index] = Some(Active {
                    row,
                    control,
                    start_operation: operation,
                    stop_operations: Vec::with_capacity(MAXIMUM_OPERATIONS),
                    last_report: Instant::now(),
                    manifest_bytes,
                });
                Ok(())
            }
            Err(error) => self.start_failed(operation, row, format!("allocate capture: {error}")),
        }
    }

    fn start_failed(
        &mut self,
        operation: String,
        mut row: proto::Capture,
        message: String,
    ) -> Result<()> {
        row.phase = proto::CapturePhase::Failed.into();
        row.error = message.clone();
        self.emit(Completion::Capture(Box::new(row.clone())))?;
        self.remember(row);
        self.outcome(operation, Err((Code::Unavailable, message)))
    }

    fn stop(&mut self, operation: String, id: &str) -> Result<()> {
        if let Some(capture) = self
            .active
            .iter_mut()
            .flatten()
            .find(|capture| capture.row.id == id)
        {
            capture.control.stop();
            capture.stop_operations.push(operation.clone());
            let capture_id = capture.row.id.clone();
            return self.emit(Completion::Running {
                operation_id: operation,
                capture_id,
            });
        }
        if self.history.iter().any(|capture| capture.id == id) {
            return self.outcome(operation, Ok(()));
        }
        self.outcome(
            operation,
            Err((Code::NotFound, "capture is not retained".into())),
        )
    }

    fn collect(&mut self) -> Result<()> {
        for index in 0..self.active.len() {
            let finished = self
                .ports
                .as_mut()
                .and_then(|ports| ports[index].take_finished());
            let Some(mut capture) = self.active[index].take() else {
                ensure!(finished.is_none(), "capture result has no operation owner");
                continue;
            };
            let progress = capture.control.progress();
            capture.row.samples = progress.samples;
            capture.row.bytes = progress.bytes + capture.manifest_bytes;
            capture.row.discontinuities = progress.discontinuities;
            let phase = match progress.phase {
                live::Phase::Preparing => proto::CapturePhase::Preparing,
                live::Phase::Recording => proto::CapturePhase::Recording,
                live::Phase::Flushing => proto::CapturePhase::Flushing,
            };
            if capture.row.phase != i32::from(phase)
                || capture.last_report.elapsed() >= PROGRESS_INTERVAL
            {
                capture.row.phase = phase.into();
                self.emit(Completion::Capture(Box::new(capture.row.clone())))?;
                capture.last_report = Instant::now();
            }
            if let Some(recording) = finished {
                capture.row.phase = proto::CapturePhase::Flushing.into();
                self.emit(Completion::Capture(Box::new(capture.row.clone())))?;
                let (reason, result) = recording.finish();
                let outcome = (|| -> Result<(), (Code, String)> {
                    let summary = result
                        .map_err(|error| (Code::DataLoss, format!("finalize capture: {error}")))?;
                    capture.row.samples = summary.attempted_samples;
                    capture.row.bytes = (summary.raw_bytes + summary.metadata_bytes) as u64
                        + capture.manifest_bytes;
                    capture.row.stopped_early = reason != EndReason::Duration;
                    capture.row.bytes += write_checksums(&summary.directory, capture.row.direction)
                        .map_err(|error| {
                            (
                                Code::DataLoss,
                                format!("finalize capture checksums: {error:#}"),
                            )
                        })?;
                    match reason {
                        EndReason::Duration | EndReason::Stopped | EndReason::Budget => Ok(()),
                        _ => Err((
                            match reason {
                                EndReason::Invalid(_) => Code::DataLoss,
                                EndReason::NoProgress => Code::DeadlineExceeded,
                                _ => Code::Cancelled,
                            },
                            format!("capture ended before its window: {reason:?}"),
                        )),
                    }
                })();
                match &outcome {
                    Ok(()) => capture.row.phase = proto::CapturePhase::Complete.into(),
                    Err((_, error)) => {
                        capture.row.phase = proto::CapturePhase::Failed.into();
                        capture.row.error = error.clone();
                    }
                }
                self.emit(Completion::Capture(Box::new(capture.row.clone())))?;
                self.outcome(capture.start_operation, outcome.clone())?;
                for operation in capture.stop_operations {
                    self.outcome(operation, outcome.clone())?;
                }
                self.remember(capture.row);
            } else {
                self.active[index] = Some(capture);
            }
        }
        if self.active.iter().all(Option::is_none)
            && let Some(ports) = self.pending_ports.take()
        {
            self.ports = Some(ports);
        }
        Ok(())
    }

    fn remember(&mut self, row: proto::Capture) {
        if self.history.len() == MAXIMUM_CAPTURES {
            self.history.pop_front();
        }
        self.history.push_back(row);
    }
}

fn write_manifest(
    directory: &Path,
    snapshot: &Snapshot,
    cell_context: &serde_json::Value,
) -> Result<u64> {
    let mut ues = snapshot.ues.clone();
    for ue in &mut ues {
        ue.imsi = None;
    }
    let ues = ues
        .iter()
        .map(umts_management::json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "umts-management-capture-v1",
        "status": umts_management::json::to_value(&snapshot.status)?,
        "configuration": umts_management::json::to_value(&snapshot.configuration)?,
        "cell_context": cell_context,
        "ues": ues,
    }))?;
    ensure!(
        bytes.len() as u64 <= MAXIMUM_MANIFEST_BYTES / 2,
        "capture manifest exceeds its size limit"
    );
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("capture.json"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(bytes.len() as u64)
}

fn write_checksums(directory: &Path, direction: i32) -> Result<u64> {
    let names = if direction == i32::from(proto::CaptureDirection::Rx) {
        ["raw-rx-0.bin", "raw-rx-0.meta", "capture.json"]
    } else {
        ["raw-tx-0.bin", "raw-tx-0.meta", "capture.json"]
    };
    let mut hashes = BTreeMap::new();
    let mut buffer = [0; 64 * 1024];
    for name in names {
        let mut file = fs::File::open(directory.join(name))?;
        let mut hash = Sha256::new();
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        hashes.insert(name, format!("{:x}", hash.finalize()));
    }
    let bytes = serde_json::to_vec_pretty(&hashes)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("checksums.json"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::File::open(directory)?.sync_all()?;
    Ok(bytes.len() as u64)
}

fn private_root(root: &Path) -> Result<()> {
    ensure!(
        root.is_absolute() && root.parent().is_some(),
        "capture root must be an absolute non-root directory"
    );
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .context("create private capture root")?;
    let metadata = fs::symlink_metadata(root)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.mode() & 0o077 == 0,
        "capture root must be a private real directory"
    );
    #[cfg(target_os = "linux")]
    ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw(),
        "capture root must belong to the daemon owner"
    );
    Ok(())
}

fn retained_bytes(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    let mut count = 0_usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        count += 1;
        ensure!(
            count <= MAXIMUM_DIRECTORY_ENTRIES,
            "capture directory entry limit exceeded"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unexpected entry in private capture root"
        );
        for file in fs::read_dir(entry.path())? {
            let file = file?;
            count += 1;
            ensure!(
                count <= MAXIMUM_DIRECTORY_ENTRIES,
                "capture directory entry limit exceeded"
            );
            let metadata = fs::symlink_metadata(file.path())?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "unexpected capture file type"
            );
            total = total
                .checked_add(metadata.len())
                .context("capture disk usage overflow")?;
        }
    }
    Ok(total)
}

#[cfg(target_os = "linux")]
fn available_bytes(root: &Path) -> Result<u64> {
    let stat = rustix::fs::statvfs(root)?;
    stat.f_bavail
        .checked_mul(stat.f_frsize)
        .context("free disk byte count overflow")
}

#[cfg(not(target_os = "linux"))]
fn available_bytes(_root: &Path) -> Result<u64> {
    anyhow::bail!("management capture requires Linux disk accounting")
}

#[cfg(test)]
mod tests;
