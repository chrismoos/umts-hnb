// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    CaptureError, CaptureStorage, CaptureSummary, RX_DISCONTINUITY, RX_FORMAT, TX_FORMAT,
    TX_INCOMPLETE,
};
use num_complex::Complex32;
use std::{
    io,
    num::{NonZeroU64, NonZeroUsize},
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_types::time::SampleRate;

const PREPARING: u8 = 0;
const RECORDING: u8 = 1;
const FLUSHING: u8 = 2;
const MAXIMUM_DURATION: Duration = Duration::from_secs(30);
const NO_PROGRESS_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Receive,
    Transmit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Duration,
    Stopped,
    Budget,
    RadioStopped,
    NoProgress,
    Invalid(CaptureError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Recording,
    Flushing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub direction: Direction,
    pub epoch: u64,
    pub sample_rate: SampleRate,
    pub tick_rate: NonZeroU64,
}

#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub phase: Phase,
    pub samples: u64,
    pub bytes: u64,
    pub discontinuities: u64,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    phase: AtomicU8,
    samples: AtomicU64,
    bytes: AtomicU64,
    discontinuities: AtomicU64,
}

#[derive(Clone)]
pub struct Control(Arc<Shared>);

impl Control {
    pub fn stop(&self) {
        self.0.stop.store(true, Ordering::Release);
    }

    pub fn progress(&self) -> Progress {
        let phase = match self.0.phase.load(Ordering::Acquire) {
            PREPARING => Phase::Preparing,
            RECORDING => Phase::Recording,
            _ => Phase::Flushing,
        };
        Progress {
            phase,
            samples: self.0.samples.load(Ordering::Relaxed),
            bytes: self.0.bytes.load(Ordering::Relaxed),
            discontinuities: self.0.discontinuities.load(Ordering::Relaxed),
        }
    }
}

pub struct Recording {
    geometry: Geometry,
    storage: CaptureStorage,
    control: Control,
    sample_count: u64,
    first_sample: Option<u64>,
    deadline: Instant,
    reason: Option<EndReason>,
}

impl Recording {
    /// Allocation and directory creation require a non-real-time caller.
    pub fn new(
        directory: PathBuf,
        limit_mib: u32,
        geometry: Geometry,
        duration: Duration,
    ) -> io::Result<Box<Self>> {
        let Geometry {
            sample_rate,
            epoch,
            tick_rate,
            direction,
        } = geometry;
        if duration.is_zero() || duration > MAXIMUM_DURATION || limit_mib > super::MAX_LIMIT_MB {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid live capture duration or memory budget",
            ));
        }
        let nanos = u64::try_from(duration.as_nanos()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "capture duration overflow")
        })?;
        let sample_count = sample_rate
            .ticks_to_samples_ceil(
                nanos,
                NonZeroU64::new(1_000_000_000).expect("positive tick rate"),
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "capture sample window overflow",
                )
            })?;
        let format = match direction {
            Direction::Receive => &RX_FORMAT,
            Direction::Transmit => &TX_FORMAT,
        };
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        let storage = CaptureStorage::allocate(
            directory,
            limit_mib,
            sample_rate.hz(),
            epoch,
            tick_rate.get(),
            format,
            false,
        )?;
        Ok(Box::new(Self {
            geometry,
            storage,
            control: Control(Arc::new(Shared::default())),
            sample_count,
            first_sample: None,
            deadline: Instant::now() + duration + NO_PROGRESS_GRACE,
            reason: None,
        }))
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// File writes and buffer destruction require a non-real-time caller.
    pub fn finish(self) -> (EndReason, io::Result<CaptureSummary>) {
        let reason = self.reason.unwrap_or(EndReason::RadioStopped);
        (reason, self.storage.finish())
    }
}

pub struct Port {
    geometry: Geometry,
    closed: Arc<AtomicBool>,
    publishing: Arc<AtomicBool>,
    commands: Sender<Box<Recording>>,
    returned: Receiver<Box<Recording>>,
    outstanding: bool,
}

pub struct Recorder {
    closed: Arc<AtomicBool>,
    publishing: Arc<AtomicBool>,
    commands: Receiver<Box<Recording>>,
    returned: Sender<Box<Recording>>,
    active: Option<Box<Recording>>,
    retained: Option<Box<Recording>>,
}

pub fn channel(geometry: Geometry) -> (Port, Recorder) {
    let (commands, pending) = mailbox::bounded(NonZeroUsize::MIN);
    let (returned, results) = mailbox::bounded(NonZeroUsize::MIN);
    let closed = Arc::new(AtomicBool::new(false));
    let publishing = Arc::new(AtomicBool::new(false));
    (
        Port {
            geometry,
            closed: Arc::clone(&closed),
            publishing: Arc::clone(&publishing),
            commands,
            returned: results,
            outstanding: false,
        },
        Recorder {
            closed,
            publishing,
            commands: pending,
            returned,
            active: None,
            retained: None,
        },
    )
}

impl Port {
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }

    pub fn publish(&mut self, recording: Box<Recording>) -> Result<(), Box<Recording>> {
        if self.outstanding || recording.geometry != self.geometry {
            return Err(recording);
        }
        self.publishing.store(true, Ordering::SeqCst);
        if self.closed.load(Ordering::SeqCst) {
            self.publishing.store(false, Ordering::SeqCst);
            return Err(recording);
        }
        let published = self.commands.try_send(recording);
        self.publishing.store(false, Ordering::SeqCst);
        published?;
        self.outstanding = true;
        Ok(())
    }

    pub fn take_finished(&mut self) -> Option<Box<Recording>> {
        let recording = self.returned.try_recv()?;
        self.outstanding = false;
        Some(recording)
    }
}

impl Recorder {
    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }
    fn return_finished(&mut self, reason: EndReason) {
        if let Some(mut recording) = self.active.take() {
            recording.reason = Some(reason);
            recording.control.0.phase.store(FLUSHING, Ordering::Release);
            self.retained = Some(recording);
        }
        self.flush_return();
    }

    fn flush_return(&mut self) {
        if let Some(recording) = self.retained.take()
            && let Err(recording) = self.returned.try_send(recording)
        {
            self.retained = Some(recording);
        }
    }

    pub fn poll(&mut self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.poll_open();
    }

    fn poll_open(&mut self) {
        self.flush_return();
        if self.retained.is_some() {
            return;
        }
        if self.active.is_none()
            && let Some(recording) = self.commands.try_recv()
        {
            recording
                .control
                .0
                .phase
                .store(RECORDING, Ordering::Release);
            self.active = Some(recording);
        }
        if let Some(recording) = &self.active {
            if recording.control.0.stop.load(Ordering::Acquire) {
                self.return_finished(EndReason::Stopped);
            } else if Instant::now() >= recording.deadline {
                self.return_finished(EndReason::NoProgress);
            }
        }
    }

    fn record(
        &mut self,
        first_sample: u64,
        first_tick: u64,
        host_us: u64,
        samples: &[Complex32],
        accepted: Option<usize>,
        flags: u32,
    ) {
        self.poll();
        let Some(recording) = &mut self.active else {
            return;
        };
        if samples.is_empty() {
            return;
        }
        if accepted.is_some_and(|accepted| accepted > samples.len()) {
            self.return_finished(EndReason::Invalid(CaptureError::Invalid(
                "accepted samples exceed attempted samples",
            )));
            return;
        }
        let first = *recording.first_sample.get_or_insert(first_sample);
        let Some(end) = first.checked_add(recording.sample_count) else {
            self.return_finished(EndReason::Invalid(CaptureError::TooManySamples));
            return;
        };
        if first_sample >= end {
            self.return_finished(EndReason::Duration);
            return;
        }
        let count = usize::try_from(end - first_sample)
            .unwrap_or(usize::MAX)
            .min(samples.len());
        let accepted = accepted.map(|count_accepted| count_accepted.min(count));
        match recording.storage.append(
            first_sample,
            first_tick,
            host_us,
            &samples[..count],
            accepted,
            flags,
        ) {
            Ok(()) => {
                recording
                    .control
                    .0
                    .samples
                    .store(recording.storage.attempted_samples, Ordering::Relaxed);
                recording.control.0.bytes.store(
                    (recording.storage.raw.len()
                        + super::METADATA_HEADER_BYTES
                        + recording.storage.metadata.len() * super::METADATA_BYTES)
                        as u64,
                    Ordering::Relaxed,
                );
                if flags & RX_DISCONTINUITY != 0 {
                    recording
                        .control
                        .0
                        .discontinuities
                        .fetch_add(1, Ordering::Relaxed);
                }
                if count < samples.len() || first_sample.saturating_add(count as u64) == end {
                    self.return_finished(EndReason::Duration);
                }
            }
            Err(CaptureError::Capacity | CaptureError::TooManyRecords) => {
                self.return_finished(EndReason::Budget)
            }
            Err(error) => self.return_finished(EndReason::Invalid(error)),
        }
    }

    pub fn record_receive(
        &mut self,
        first_sample: u64,
        first_tick: u64,
        host_us: u64,
        samples: &[Complex32],
        discontinuity: bool,
    ) {
        self.record(
            first_sample,
            first_tick,
            host_us,
            samples,
            Some(samples.len()),
            if discontinuity { RX_DISCONTINUITY } else { 0 },
        );
    }

    pub fn record_transmit(
        &mut self,
        first_sample: u64,
        first_tick: u64,
        host_us: u64,
        samples: &[Complex32],
        accepted: Option<usize>,
        failed: bool,
    ) {
        self.record(
            first_sample,
            first_tick,
            host_us,
            samples,
            accepted,
            if failed { TX_INCOMPLETE } else { 0 },
        );
    }

    pub fn invalidate_time(&mut self) {
        self.return_finished(EndReason::Invalid(CaptureError::Invalid(
            "capture tick conversion failed",
        )));
    }

    // Joined-worker retirement must close admission before draining an in-flight publication.
    pub(crate) fn stop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        while self.publishing.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_micros(50));
        }
        self.poll_open();
        if self.active.is_some() {
            self.return_finished(EndReason::RadioStopped);
        }
    }
}

#[cfg(test)]
mod tests;
