// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use sdr_hal::{
    RadioRx, RxReadResult,
    error::{Error, ErrorKind},
};
use std::{num::NonZeroU64, sync::atomic::AtomicU64};

fn settings() -> ClockSettings {
    ClockSettings {
        maximum_age: None,
        maximum_query: Duration::from_millis(100),
        maximum_drift_ppm: 0,
    }
}

fn sample(sequence: u64, start: u64, finish: u64) -> ClockSample {
    ClockSample {
        sequence,
        observation: Observation {
            started: Duration::from_millis(start),
            finished: Duration::from_millis(finish),
            tick: 10_000 + start,
        },
    }
}

struct Fixture {
    now: Arc<AtomicU64>,
    sampler: DeviceClockSampler,
    preparation: SampledClock,
    submission: SampledClock,
}
impl Fixture {
    fn new(settings: ClockSettings) -> Self {
        let now = Arc::new(AtomicU64::new(1000));
        let time = now.clone();
        let origin = HostClock::Controlled(Arc::new(move || {
            Duration::from_millis(time.load(Ordering::Acquire))
        }));
        let mut model = ClockModel::new(NonZeroU64::new(1000).unwrap(), settings).unwrap();
        model.observe(sample(0, 0, 0).observation).unwrap();
        let calibration = Calibration {
            model,
            accepted_sequence: 0,
            last_received: None,
            last_accepted: None,
            outcome: ClockObservationOutcome::None,
        };
        let state = ClockState::new(calibration);
        Self {
            now,
            sampler: DeviceClockSampler {
                origin: origin.clone(),
                interval: Duration::from_secs(1),
                next: Duration::from_secs(1),
                next_sequence: 1,
                calibration,
                state: state.clone(),
            },
            preparation: SampledClock {
                origin: origin.clone(),
                calibration,
                state: state.clone(),
            },
            submission: SampledClock {
                origin,
                calibration,
                state,
            },
        }
    }
}

#[test]
fn both_workers_use_identical_seed_and_published_calibration() {
    let mut f = Fixture::new(settings());
    assert_eq!(
        f.preparation.read_with_sequence(),
        f.submission.read_with_sequence()
    );
    assert!(f.preparation.snapshot().startup_seeded);
    assert_eq!(f.preparation.snapshot().last_received_sequence, None);
    for sequence in 1..=16 {
        let time = sequence * 10;
        f.sampler.observe(sample(sequence, time, time)).unwrap();
        let first = f.preparation.read_with_sequence().unwrap();
        assert_eq!(first, f.submission.read_with_sequence().unwrap());
        assert_eq!(first.1, sequence);
    }
}

#[test]
fn lagging_reader_uses_latest_validated_anchor_without_queue_pressure() {
    let mut f = Fixture::new(settings());
    for sequence in 1..=100 {
        f.sampler
            .observe(sample(sequence, sequence, sequence))
            .unwrap();
        assert_eq!(f.preparation.read_with_sequence().unwrap().1, sequence);
    }
    assert_eq!(f.submission.read_with_sequence().unwrap().1, 100);
    assert_eq!(f.submission.read(), f.preparation.read());
}

#[test]
fn slow_queries_preserve_anchor_and_received_and_accepted_provenance() {
    let mut f = Fixture::new(settings());
    f.sampler.observe(sample(1, 100, 100)).unwrap();
    assert_eq!(f.preparation.read_with_sequence().unwrap().1, 1);
    f.sampler.observe(sample(2, 200, 301)).unwrap();
    assert_eq!(f.preparation.read_with_sequence().unwrap().1, 1);
    let snapshot = f.preparation.snapshot();
    assert_eq!(snapshot.last_received_sequence, Some(2));
    assert_eq!(snapshot.last_accepted_sequence, Some(1));
    assert_eq!(
        snapshot.last_observation_outcome,
        ClockObservationOutcome::RejectedSlow
    );
    assert_eq!(snapshot.last_received_age, Some(Duration::from_millis(699)));
    assert_eq!(snapshot.last_accepted_age, Some(Duration::from_millis(900)));
    f.sampler.observe(sample(3, 302, 302)).unwrap();
    assert_eq!(f.submission.read_with_sequence().unwrap().1, 3);
    assert_eq!(
        f.submission.snapshot().last_observation_outcome,
        ClockObservationOutcome::Accepted
    );
}

#[test]
fn persistent_slow_queries_recalibrate_from_an_estimate() {
    let mut f = Fixture::new(settings());
    for sequence in 1..=4 {
        let start = sequence * 150;
        f.sampler
            .observe(sample(sequence, start, start + 101))
            .unwrap();
        assert_eq!(f.preparation.read_with_sequence().unwrap().1, 0);
        assert_eq!(
            f.preparation.snapshot().last_observation_outcome,
            ClockObservationOutcome::RejectedSlow
        );
    }
    f.sampler.observe(sample(5, 750, 851)).unwrap();
    assert_eq!(f.preparation.read_with_sequence().unwrap().1, 5);
    assert_eq!(f.submission.read_with_sequence().unwrap().1, 5);
    assert_eq!(
        f.submission.snapshot().last_observation_outcome,
        ClockObservationOutcome::Estimated
    );
}

#[test]
fn stale_anchor_is_rejected_only_when_age_expiry_is_configured() {
    let mut f = Fixture::new(ClockSettings {
        maximum_age: Some(Duration::from_millis(500)),
        ..settings()
    });
    assert_eq!(f.preparation.read(), Err(ClockError::Stale));
    assert_eq!(f.submission.read(), Err(ClockError::Discontinuity));
    let mut f = Fixture::new(settings());
    f.now.store(3_600_000, Ordering::Release);
    assert!(f.preparation.read().is_ok());
    assert_eq!(f.preparation.snapshot().accepted_sequence, 0);
}

#[test]
fn invalid_observations_close_all_readers_without_replacing_a_valid_anchor() {
    for bad in [
        ClockSample {
            observation: Observation {
                tick: 1,
                ..sample(1, 100, 100).observation
            },
            ..sample(1, 100, 100)
        },
        ClockSample {
            observation: Observation {
                started: Duration::from_millis(101),
                ..sample(1, 100, 100).observation
            },
            ..sample(1, 100, 100)
        },
    ] {
        let mut f = Fixture::new(settings());
        assert!(f.sampler.observe(bad).is_err());
        assert_eq!(f.sampler.next_sequence, 1);
        assert_eq!(f.preparation.read(), Err(ClockError::Discontinuity));
        assert_eq!(f.submission.read(), Err(ClockError::Discontinuity));
        assert_eq!(f.sampler.state.calibration.load().accepted_sequence, 0);
    }
}

#[test]
fn gap_duplicate_and_exhausted_sequence_are_terminal() {
    for sequence in [0, 2, u64::MAX] {
        let mut f = Fixture::new(settings());
        if sequence == u64::MAX {
            f.sampler.next_sequence = sequence;
        }
        assert!(f.sampler.observe(sample(sequence, 100, 100)).is_err());
        assert_eq!(f.submission.read(), Err(ClockError::Discontinuity));
    }
}

#[test]
fn dropping_any_epoch_clock_owner_closes_the_remaining_readers() {
    let mut f = Fixture::new(settings());
    drop(f.sampler);
    assert_eq!(f.preparation.read(), Err(ClockError::Discontinuity));
    assert_eq!(f.submission.read(), Err(ClockError::Discontinuity));
    let mut f = Fixture::new(settings());
    drop(f.preparation);
    assert_eq!(f.submission.read(), Err(ClockError::Discontinuity));
    assert!(f.sampler.observe(sample(1, 100, 100)).is_err());
}

struct ClockRx {
    now: Arc<AtomicU64>,
    calls: AtomicU64,
    fail: AtomicBool,
}
impl RadioRx for ClockRx {
    fn rx_sample_rate_hz(&self) -> f64 {
        7_680_000.0
    }
    fn tick_rate(&self) -> u64 {
        30_720_000
    }
    fn get_hardware_time(&self) -> Result<u64, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail.load(Ordering::Relaxed) {
            Err(Error::new(
                ErrorKind::Timeout,
                "injected clock query failure",
            ))
        } else {
            Ok(10_000 + self.now.load(Ordering::Acquire))
        }
    }
    fn rx_activate(&mut self, _: Option<u64>) -> Result<(), Error> {
        panic!("clock test cannot activate RX")
    }
    fn stop(&mut self) -> Result<(), Error> {
        panic!("clock test cannot stop RX")
    }
    fn rx_read(&mut self, _: &mut [num_complex::Complex32], _: i64) -> Result<RxReadResult, Error> {
        panic!("clock test cannot read samples")
    }
}

#[test]
fn sampler_queries_at_one_second_cadence_and_never_from_tx_readers() {
    let mut f = Fixture::new(settings());
    let rx = ClockRx {
        now: f.now.clone(),
        calls: AtomicU64::new(0),
        fail: AtomicBool::new(false),
    };
    let mut counters = RxCounters::default();
    for time in [0, 12, 50, 250, 999, 1000, 1012, 1999, 2000] {
        f.now.store(time, Ordering::Release);
        f.sampler.sample_if_due(&rx, &mut counters).unwrap();
        assert_eq!(f.preparation.read(), f.submission.read());
        let expected = time / 1000;
        assert_eq!(rx.calls.load(Ordering::Relaxed), expected);
        assert_eq!(counters.clock_queries, expected);
    }
    assert_eq!(f.sampler.next, Duration::from_secs(3));
    assert_eq!(f.preparation.snapshot().accepted_sequence, 2);
}

#[test]
fn hardware_query_failure_closes_both_readers_and_preserves_last_calibration() {
    let mut f = Fixture::new(settings());
    let rx = ClockRx {
        now: f.now.clone(),
        calls: AtomicU64::new(0),
        fail: AtomicBool::new(true),
    };
    assert!(matches!(
        f.sampler.sample_if_due(&rx, &mut RxCounters::default()),
        Err(RxError::Driver(_))
    ));
    assert_eq!(f.sampler.state.calibration.load().accepted_sequence, 0);
    assert_eq!(f.preparation.read(), Err(ClockError::Discontinuity));
    assert_eq!(f.submission.read(), Err(ClockError::Discontinuity));
}

#[test]
fn floor_writer_keeps_the_highest_projected_floor() {
    let model = ClockModel::new(
        std::num::NonZeroU64::new(1_000).unwrap(),
        ClockSettings {
            maximum_age: None,
            maximum_query: Duration::from_millis(10),
            maximum_drift_ppm: 0,
        },
    )
    .unwrap();
    let state = ClockState::new(Calibration {
        model,
        accepted_sequence: 0,
        last_received: None,
        last_accepted: None,
        outcome: ClockObservationOutcome::None,
    });
    let origin = Instant::now();
    let mut writer = ClockFloorWriter {
        origin,
        model,
        state: state.clone(),
        last: None,
    };
    let floor = |host_ms, tick| {
        Some(ClockFloor {
            host: Duration::from_millis(host_ms),
            tick,
        })
    };
    assert_eq!(state.floor(), None);
    writer.raise(origin + Duration::from_millis(10), 500);
    assert_eq!(state.floor(), floor(10, 500));
    writer.raise(origin + Duration::from_millis(20), 505);
    assert_eq!(state.floor(), floor(10, 500));
    writer.raise(origin + Duration::from_millis(30), 530);
    assert_eq!(state.floor(), floor(30, 530));
}
