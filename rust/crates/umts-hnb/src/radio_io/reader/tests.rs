// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::test_support::{at, count};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
struct Probe {
    reads: AtomicUsize,
    stops: AtomicUsize,
    drops: AtomicUsize,
    entered: AtomicBool,
    release: AtomicBool,
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Error,
    Panic,
    Hold,
}

struct Source {
    probe: Arc<Probe>,
    blocks: usize,
    fault: Fault,
}

impl RadioRx for Source {
    fn rx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn tick_rate(&self) -> u64 {
        30_720_000
    }
    fn get_hardware_time(&self) -> Result<u64, sdr_hal::error::Error> {
        Ok(0)
    }
    fn rx_activate(&mut self, _: Option<u64>) -> Result<(), sdr_hal::error::Error> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), sdr_hal::error::Error> {
        self.probe.stops.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn rx_read(
        &mut self,
        samples: &mut [Complex32],
        _: i64,
    ) -> Result<RxReadResult, sdr_hal::error::Error> {
        match self.fault {
            Fault::Error => return Err("reader failure".into()),
            Fault::Panic => panic!("reader panic"),
            Fault::Hold => {
                self.probe.entered.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(1);
                while !self.probe.release.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline);
                    thread::yield_now();
                }
            }
            Fault::None => {}
        }
        let index = self.probe.reads.load(Ordering::Acquire);
        if index >= self.blocks {
            return Ok(empty_result());
        }
        let length = samples.len();
        for (offset, sample) in samples.iter_mut().enumerate() {
            let position = index * length + offset;
            *sample = Complex32::new(position as f32 * 0.125, -(position as f32) * 0.0625);
        }
        self.probe.reads.fetch_add(1, Ordering::AcqRel);
        Ok(RxReadResult {
            samples_read: samples.len(),
            time_ticks: (index * samples.len() * 4) as u64,
            overflow: index == 1,
        })
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::AcqRel);
    }
}

fn owner(blocks: usize, fault: Fault) -> (DeviceOwner, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    (
        DeviceOwner {
            rx: Box::new(Source {
                probe: probe.clone(),
                blocks,
                fault,
            }),
            sampler: None,
            counters: RxCounters::default(),
            reader_counters: ReaderCounters::default(),
        },
        probe,
    )
}

#[test]
fn full_pool_keeps_draining_and_resumption_preserves_raw_samples_and_ticks() {
    let (ready, ready_rx) = mailbox::bounded(count(1));
    let (free, free_rx) = mailbox::bounded(count(1));
    assert!(free.try_send(Block::new(8)).is_ok());
    let discarded_through = Arc::new(AtomicU64::new(0));
    let mut reader = BufferedRx {
        worker: None,
        ready: ready_rx,
        free,
        discarded_through: Arc::clone(&discarded_through),
        stale_blocks: 0,
        stale_samples: 0,
        pending_overflow: false,
    };
    let mut producer = Producer {
        ready,
        free: free_rx,
        current: Block::new(8),
        consumer: thread::current(),
        pending_overflow: false,
        discarded_through,
    };
    let (mut owner, _) = owner(3, Fault::None);
    assert!(producer.cycle(&mut owner, 1000).unwrap());
    assert!(producer.cycle(&mut owner, 1000).unwrap());
    assert_eq!(owner.reader_counters.discarded_blocks, 1);
    assert_eq!(owner.reader_counters.discarded_samples, 8);
    let mut samples = vec![Complex32::default(); 8];
    assert!(reader.read(&mut samples).unwrap().is_none());
    assert_eq!(reader.stale_blocks, 1);
    assert_eq!(reader.stale_samples, 8);
    assert!(producer.cycle(&mut owner, 1000).unwrap());
    let resumed = reader.read(&mut samples).unwrap().unwrap();
    assert_eq!(resumed.result.time_ticks, 64);
    assert!(resumed.result.overflow);
    assert_eq!(resumed.result.samples_read, 8);
    assert_eq!(samples[0], Complex32::new(2.0, -1.0));
    assert_eq!(resumed.reader_counters.discarded_samples, 8);
    assert_eq!(resumed.reader_counters.stale_samples, 8);
    assert!(reader.read(&mut samples).unwrap().is_none());
}

#[test]
fn recovery_preserves_overflow_from_discarded_queue_and_keeps_newer_blocks() {
    let Prepared {
        mut producer,
        ready,
        free,
        discarded_through,
    } = Prepared::new(8, count(2));
    let mut reader = BufferedRx {
        worker: None,
        ready,
        free,
        discarded_through,
        stale_blocks: 0,
        stale_samples: 0,
        pending_overflow: false,
    };
    let (mut owner, _) = owner(5, Fault::None);
    for _ in 0..3 {
        assert!(producer.cycle(&mut owner, 1000).unwrap());
    }
    let mut samples = vec![Complex32::default(); 8];
    assert!(reader.read(&mut samples).unwrap().is_none());
    assert_eq!(reader.stale_blocks, 2);
    assert_eq!(reader.stale_samples, 16);
    for _ in 0..2 {
        assert!(producer.cycle(&mut owner, 1000).unwrap());
    }
    let resumed = reader.read(&mut samples).unwrap().unwrap();
    assert_eq!(resumed.result.time_ticks, 96);
    assert!(resumed.result.overflow);
    assert_eq!(samples[0], Complex32::new(3.0, -1.5));
    let next = reader.read(&mut samples).unwrap().unwrap();
    assert_eq!(next.result.time_ticks, 128);
    assert!(!next.result.overflow);
    assert_eq!(samples[0], Complex32::new(4.0, -2.0));
    assert_eq!(reader.stale_blocks, 2);
    assert!(reader.read(&mut samples).unwrap().is_none());
}

#[test]
fn hardware_reader_advances_without_a_processing_consumer_and_returns_owner() {
    let (owner, probe) = owner(24, Fault::None);
    let mut reader = Prepared::new(8, count(2))
        .spawn(owner, 1000, StopToken::default())
        .unwrap_or_else(|_| panic!("spawn failed"));
    let deadline = Instant::now() + Duration::from_secs(1);
    while probe.reads.load(Ordering::Acquire) < 24 {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    let (mut owner, exit) = reader.finish().unwrap();
    assert_eq!(exit, WorkerExit::Stopped);
    assert_eq!(owner.reader_counters.published_blocks, 2);
    assert_eq!(owner.reader_counters.discarded_blocks, 22);
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    assert_eq!(probe.stops.load(Ordering::Acquire), 0);
    owner.rx.stop().unwrap();
    drop(owner);
    assert_eq!(probe.stops.load(Ordering::Acquire), 1);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn pool_overrun_resumes_through_production_timeline_and_both_filter_outputs() {
    use crate::radio_io::{AirClockOrigin, RxSettings, TimedRx};
    use std::num::NonZeroU64;
    use umts_dsp::pulse_shaping::SamplesPerChip;
    use umts_types::time::{CHIPS_PER_SECOND, SampleRate};

    let make_receiver = |rx| {
        let mut receiver = TimedRx::new(
            rx,
            AirClockOrigin {
                chip: at(0),
                chip_rate: SampleRate::new(CHIPS_PER_SECOND, 1).unwrap(),
                tick: 0,
                tick_rate: NonZeroU64::new(30_720_000).unwrap(),
            },
            RxSettings {
                rate: SamplesPerChip::Two,
                span_chips: 32,
                phase: 0,
                maximum_samples: count(100),
                timeout_us: 1000,
            },
        )
        .unwrap();
        receiver.start(None).unwrap();
        receiver
    };
    let (device, _) = owner(6, Fault::None);
    let mut receiver = make_receiver(device.rx);
    let mut device = DeviceOwner {
        rx: receiver.rx.take().unwrap(),
        sampler: None,
        counters: RxCounters::default(),
        reader_counters: ReaderCounters::default(),
    };
    let Prepared {
        mut producer,
        ready,
        free,
        discarded_through,
    } = Prepared::new(100, count(2));
    receiver.reader = Some(BufferedRx {
        worker: None,
        ready,
        free,
        discarded_through,
        stale_blocks: 0,
        stale_samples: 0,
        pending_overflow: false,
    });
    assert!(producer.cycle(&mut device, 1000).unwrap());
    assert!(!receiver.read_matched().unwrap().unwrap().full.discontinuity);
    for _ in 0..3 {
        assert!(producer.cycle(&mut device, 1000).unwrap());
    }
    assert!(receiver.read_matched().unwrap().is_none());
    assert!(producer.cycle(&mut device, 1000).unwrap());
    let resumed = receiver.read_matched().unwrap().unwrap();
    assert!(resumed.full.discontinuity);
    assert_eq!(resumed.full.generation, 2);
    assert_eq!(resumed.full.window.first_center.unwrap().index, 432);
    let actual_full = resumed.full.samples.to_vec();
    let actual_common = resumed.common.chips.to_vec();
    let (reference, probe) = owner(6, Fault::None);
    probe.reads.store(4, Ordering::Release);
    let mut reference = make_receiver(reference.rx);
    let expected = reference.read_matched().unwrap().unwrap();
    assert_eq!(actual_full, expected.full.samples);
    assert_eq!(actual_common, expected.common.chips);
    assert!(producer.cycle(&mut device, 1000).unwrap());
    let next = receiver.read_matched().unwrap().unwrap();
    assert!(!next.full.discontinuity);
    assert_eq!(next.full.generation, 2);
    assert_eq!(receiver.counters().missing_samples, 300);
    assert_eq!(receiver.counters().discontinuities, 1);
    let counters = receiver.counters().reader.unwrap();
    assert_eq!(
        (counters.discarded_samples, counters.stale_samples),
        (100, 200)
    );
    receiver.reader.take();
    receiver.rx = Some(device.rx);
    receiver.stop().unwrap();
    reference.stop().unwrap();
}

#[test]
fn driver_failure_and_panic_stop_the_epoch_but_return_the_original_hal_owner() {
    for fault in [Fault::Error, Fault::Panic] {
        let (owner, probe) = owner(1, fault);
        let stop = StopToken::default();
        let mut reader = Prepared::new(8, count(2))
            .spawn(owner, 1000, stop.clone())
            .unwrap_or_else(|_| panic!("spawn failed"));
        let deadline = Instant::now() + Duration::from_secs(1);
        while !stop.is_stopping() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let (owner, exit) = reader.finish().unwrap();
        match fault {
            Fault::Error => assert!(
                matches!(exit, WorkerExit::Failed(ref error) if error.contains("reader failure"))
            ),
            Fault::Panic => assert_eq!(exit, WorkerExit::Panicked),
            Fault::None | Fault::Hold => unreachable!(),
        }
        assert_eq!(probe.drops.load(Ordering::Acquire), 0);
        drop(owner);
        assert_eq!(probe.drops.load(Ordering::Acquire), 1);
    }
}

#[test]
fn pending_join_retains_the_hal_owner_until_the_read_really_returns() {
    let (owner, probe) = owner(1, Fault::Hold);
    let mut reader = Prepared::new(8, count(2))
        .spawn(owner, 1000, StopToken::default())
        .unwrap_or_else(|_| panic!("spawn failed"));
    let deadline = Instant::now() + Duration::from_secs(1);
    while !probe.entered.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
    assert!(matches!(reader.finish(), Err(RxError::ReaderJoinPending)));
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    probe.release.store(true, Ordering::Release);
    let (owner, exit) = reader.finish().unwrap();
    assert_eq!(exit, WorkerExit::Stopped);
    assert_eq!(probe.drops.load(Ordering::Acquire), 0);
    drop(owner);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn buffered_and_direct_reads_produce_identical_matched_samples_and_discontinuities() {
    use crate::radio_io::{AirClockOrigin, RxSettings, TimedRx};
    use std::num::NonZeroU64;
    use umts_dsp::pulse_shaping::SamplesPerChip;
    use umts_types::time::SampleRate;

    let mut outputs = Vec::new();
    for buffered in [false, true] {
        let (owner, _) = owner(24, Fault::None);
        let mut rx = TimedRx::new(
            owner.rx,
            AirClockOrigin {
                chip: at(0),
                tick: 0,
                chip_rate: SampleRate::from_hz(3_840_000.0).unwrap(),
                tick_rate: NonZeroU64::new(30_720_000).unwrap(),
            },
            RxSettings {
                rate: SamplesPerChip::Two,
                span_chips: 4,
                phase: 0,
                maximum_samples: count(32),
                timeout_us: 1000,
            },
        )
        .unwrap();
        if buffered {
            rx.enable_reader(StopToken::default(), count(32)).unwrap();
        }
        rx.start(None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut output = Vec::new();
        while rx.counters().reads < 24 {
            assert!(Instant::now() < deadline);
            if let Some(block) = rx.read_matched().unwrap() {
                output.push((
                    block.full.window.first_center,
                    block.full.generation,
                    block.full.discontinuity,
                    block.full.samples.to_vec(),
                    block.common.chips.to_vec(),
                ));
            } else {
                thread::yield_now();
            }
        }
        rx.stop().unwrap();
        assert_eq!(rx.counters().reported_overflows, 1);
        if buffered {
            assert_eq!(rx.counters().reader.unwrap().discarded_samples, 0);
        }
        outputs.push(output);
    }
    assert_eq!(outputs[0], outputs[1]);
}
