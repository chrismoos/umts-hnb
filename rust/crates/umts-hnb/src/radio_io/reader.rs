// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{RxCounters, RxError, sampled_clock::DeviceClockSampler};
use num_complex::Complex32;
use sdr_hal::{RadioRx, RxReadResult};
use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use umts_runtime::{
    mailbox::{self, Receiver, Sender},
    worker::{JoinError, StartGate, StopToken, Worker, WorkerExit},
};

const EMPTY_READ_WAIT: Duration = Duration::from_micros(100);
pub(super) const JOIN_WAIT: Duration = Duration::from_millis(100);
pub(super) const MAX_QUEUE_BLOCKS: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReaderCounters {
    pub reads: u64,
    pub empty_reads: u64,
    pub reported_overflows: u64,
    pub published_blocks: u64,
    pub discarded_blocks: u64,
    pub discarded_samples: u64,
    pub stale_blocks: u64,
    pub stale_samples: u64,
    pub maximum_read_ns: u128,
}

pub(super) struct DeviceOwner {
    pub rx: Box<dyn RadioRx>,
    pub sampler: Option<DeviceClockSampler>,
    pub counters: RxCounters,
    pub reader_counters: ReaderCounters,
}

struct Block {
    samples: Vec<Complex32>,
    result: RxReadResult,
    returned_at: Instant,
    counters: RxCounters,
    reader_counters: ReaderCounters,
}

fn empty_result() -> RxReadResult {
    RxReadResult {
        samples_read: 0,
        time_ticks: 0,
        overflow: false,
    }
}

impl Block {
    fn new(samples: usize) -> Self {
        Self {
            samples: vec![Complex32::default(); samples],
            result: empty_result(),
            returned_at: Instant::now(),
            counters: RxCounters::default(),
            reader_counters: ReaderCounters::default(),
        }
    }
}

pub(super) struct Read {
    pub result: RxReadResult,
    pub returned_at: Instant,
    pub counters: RxCounters,
    pub reader_counters: ReaderCounters,
}

struct Producer {
    ready: Sender<Block>,
    free: Receiver<Block>,
    current: Block,
    consumer: thread::Thread,
    pending_overflow: bool,
    discarded_through: Arc<AtomicU64>,
}

impl Producer {
    fn cycle(&mut self, owner: &mut DeviceOwner, timeout_us: i64) -> Result<bool, RxError> {
        if let Some(sampler) = &mut owner.sampler {
            sampler.sample_if_due(&*owner.rx, &mut owner.counters)?;
        }
        let started = Instant::now();
        owner.reader_counters.reads = owner.reader_counters.reads.saturating_add(1);
        let mut result = match owner.rx.rx_read(&mut self.current.samples, timeout_us) {
            Ok(result) => result,
            Err(error) if error.kind() == sdr_hal::error::ErrorKind::Timeout => empty_result(),
            Err(error) => return Err(RxError::Driver(error)),
        };
        let returned_at = Instant::now();
        owner.reader_counters.maximum_read_ns = owner
            .reader_counters
            .maximum_read_ns
            .max(returned_at.duration_since(started).as_nanos());
        if result.samples_read > self.current.samples.len() {
            return Err(RxError::InvalidRead);
        }
        owner.reader_counters.reported_overflows = owner
            .reader_counters
            .reported_overflows
            .saturating_add(u64::from(result.overflow));
        self.pending_overflow |= result.overflow;
        if result.samples_read == 0 && !result.overflow {
            owner.reader_counters.empty_reads = owner.reader_counters.empty_reads.saturating_add(1);
            return Ok(false);
        }
        let Some(replacement) = self.free.try_recv() else {
            owner.reader_counters.discarded_blocks =
                owner.reader_counters.discarded_blocks.saturating_add(1);
            owner.reader_counters.discarded_samples = owner
                .reader_counters
                .discarded_samples
                .saturating_add(result.samples_read as u64);
            self.discarded_through
                .store(owner.reader_counters.reads, Ordering::Release);
            self.consumer.unpark();
            return Ok(true);
        };
        owner.reader_counters.published_blocks =
            owner.reader_counters.published_blocks.saturating_add(1);
        result.overflow |= self.pending_overflow;
        self.pending_overflow = false;
        self.current.result = result;
        self.current.returned_at = returned_at;
        self.current.counters = owner.counters;
        self.current.reader_counters = owner.reader_counters;
        let completed = std::mem::replace(&mut self.current, replacement);
        // A free buffer reserves a ready slot. Each buffer has exactly one owner.
        self.ready
            .try_send(completed)
            .map_err(|_| RxError::ReaderQueue)?;
        self.consumer.unpark();
        Ok(true)
    }
}

pub(super) struct SpawnFailure {
    pub error: std::io::Error,
    pub owner: DeviceOwner,
}

pub(super) struct BufferedRx {
    worker: Option<Worker<DeviceOwner>>,
    ready: Receiver<Block>,
    free: Sender<Block>,
    discarded_through: Arc<AtomicU64>,
    stale_blocks: u64,
    stale_samples: u64,
    pending_overflow: bool,
}

pub(super) struct Prepared {
    producer: Producer,
    ready: Receiver<Block>,
    free: Sender<Block>,
    discarded_through: Arc<AtomicU64>,
}

impl Prepared {
    pub fn new(samples: usize, capacity: NonZeroUsize) -> Self {
        let (ready, ready_rx) = mailbox::bounded(capacity);
        let (free, free_rx) = mailbox::bounded(capacity);
        for _ in 0..capacity.get() {
            assert!(free.try_send(Block::new(samples)).is_ok());
        }
        let discarded_through = Arc::new(AtomicU64::new(0));
        let producer = Producer {
            ready,
            free: free_rx,
            current: Block::new(samples),
            consumer: thread::current(),
            pending_overflow: false,
            discarded_through: Arc::clone(&discarded_through),
        };
        Self {
            producer,
            ready: ready_rx,
            free,
            discarded_through,
        }
    }

    pub fn spawn(
        self,
        owner: DeviceOwner,
        timeout_us: i64,
        stop: StopToken,
    ) -> Result<BufferedRx, Box<SpawnFailure>> {
        let Self {
            mut producer,
            ready,
            free,
            discarded_through,
        } = self;
        let gate = StartGate::default();
        gate.release();
        let worker = Worker::spawn_owned_gated_with_role(
            "hnb-rx-reader",
            stop,
            gate,
            umts_runtime::scheduling::Role::RxReader,
            Box::new(owner),
            move |owner, stop| {
                while !stop.is_stopping() {
                    match producer.cycle(owner, timeout_us) {
                        Ok(true) => {}
                        Ok(false) => {
                            stop.wait_or_stop(EMPTY_READ_WAIT);
                        }
                        Err(error) => return Err(format!("RX reader: {error:?}")),
                    }
                }
                Ok(())
            },
            |_| {},
        );
        match worker {
            Ok(worker) => Ok(BufferedRx {
                worker: Some(worker),
                ready,
                free,
                discarded_through,
                stale_blocks: 0,
                stale_samples: 0,
                pending_overflow: false,
            }),
            Err(failure) => Err(Box::new(SpawnFailure {
                error: failure.error,
                owner: *failure.state,
            })),
        }
    }
}

impl BufferedRx {
    pub fn read(&mut self, samples: &mut Vec<Complex32>) -> Result<Option<Read>, RxError> {
        // A fixed cutoff and budget prevent concurrent production from extending recovery indefinitely.
        let cutoff = self.discarded_through.load(Ordering::Acquire);
        for _ in 0..MAX_QUEUE_BLOCKS {
            let Some(mut block) = self.ready.try_recv() else {
                return Ok(None);
            };
            if block.reader_counters.reads <= cutoff {
                self.stale_blocks = self.stale_blocks.saturating_add(1);
                self.stale_samples = self
                    .stale_samples
                    .saturating_add(block.result.samples_read as u64);
                self.pending_overflow |= block.result.overflow;
                self.free
                    .try_send(block)
                    .map_err(|_| RxError::ReaderQueue)?;
                continue;
            }
            if block.samples.len() != samples.len() {
                return Err(RxError::InvalidRead);
            }
            std::mem::swap(samples, &mut block.samples);
            block.result.overflow |= std::mem::take(&mut self.pending_overflow);
            block.reader_counters.stale_blocks = self.stale_blocks;
            block.reader_counters.stale_samples = self.stale_samples;
            let read = Read {
                result: std::mem::replace(&mut block.result, empty_result()),
                returned_at: block.returned_at,
                counters: block.counters,
                reader_counters: block.reader_counters,
            };
            self.free
                .try_send(block)
                .map_err(|_| RxError::ReaderQueue)?;
            return Ok(Some(read));
        }
        Ok(None)
    }

    pub fn finish(&mut self) -> Result<(DeviceOwner, WorkerExit), RxError> {
        let worker = self.worker.as_mut().ok_or(RxError::ReaderQueue)?;
        worker.request_stop();
        let (exit, mut owner) = worker.join_owned(JOIN_WAIT).map_err(|error| match error {
            JoinError::Deadline => RxError::ReaderJoinPending,
            JoinError::AlreadyJoined => RxError::ReaderQueue,
        })?;
        self.worker.take();
        owner.reader_counters.stale_blocks = self.stale_blocks;
        owner.reader_counters.stale_samples = self.stale_samples;
        Ok((*owner, exit))
    }
}

impl Drop for BufferedRx {
    fn drop(&mut self) {
        if let Some(mut worker) = self.worker.take()
            && let Some((_, mut owner)) = worker.stop_and_join()
        {
            let _ = owner.rx.stop();
        }
    }
}

#[cfg(test)]
mod tests;
