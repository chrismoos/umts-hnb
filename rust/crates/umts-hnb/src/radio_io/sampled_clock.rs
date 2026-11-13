// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{PumpError, RxCounters, RxError, TimedRx, TxPump};
use arc_swap::ArcSwap;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use umts_radio::clock_model::{
    ClockBounds, ClockError, ClockFloor, ClockModel, ClockSettings, Observation, ObserveResult,
};

const STARTUP_CLOCK_QUERIES: usize = 8;
const POLL_READS: usize = 4;
const POLL_READ_SPACING: Duration = Duration::from_millis(2);
const POLL_THREAD: &str = "hnb-clock";
const NO_FLOOR: u64 = u64::MAX;

#[derive(Clone)]
pub(crate) enum HostClock {
    System(Instant),
    #[cfg(test)]
    Controlled(std::sync::Arc<dyn Fn() -> Duration + Send + Sync>),
}

impl HostClock {
    fn elapsed(&self) -> Duration {
        match self {
            Self::System(origin) => origin.elapsed(),
            #[cfg(test)]
            Self::Controlled(read) => read(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ClockSamplingSettings {
    pub interval: Duration,
    pub model: ClockSettings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClockSample {
    sequence: u64,
    observation: Observation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockObservationOutcome {
    None,
    Accepted,
    Estimated,
    RejectedSlow,
}

#[derive(Default)]
pub(super) struct ClockQueryStats {
    queries: AtomicU64,
    total_ns: AtomicU64,
    maximum_ns: AtomicU64,
}

impl ClockQueryStats {
    fn record(&self, sample: Observation) {
        let elapsed =
            u64::try_from((sample.finished - sample.started).as_nanos()).unwrap_or(u64::MAX);
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(elapsed, Ordering::Relaxed);
        self.maximum_ns.fetch_max(elapsed, Ordering::Relaxed);
    }

    pub(super) fn apply(&self, counters: &mut RxCounters) {
        counters.clock_queries = self.queries.load(Ordering::Relaxed);
        counters.clock_query_total_ns = u128::from(self.total_ns.load(Ordering::Relaxed));
        counters.clock_query_maximum_ns = u128::from(self.maximum_ns.load(Ordering::Relaxed));
    }
}

pub(super) struct ClockPoller {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ClockPoller {
    fn spawn(
        mut sampler: DeviceClockSampler,
        clock: Box<dyn sdr_hal::RadioClock>,
        stats: Arc<ClockQueryStats>,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = umts_runtime::scheduling::spawn_background(POLL_THREAD, move || {
            while !stopping.load(Ordering::Acquire) {
                let wait = sampler.next.saturating_sub(sampler.origin.elapsed());
                if !wait.is_zero() {
                    thread::park_timeout(wait);
                } else if sampler.poll(&*clock, &stats).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for ClockPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSamplingSnapshot {
    pub startup_seeded: bool,
    pub accepted_sequence: u64,
    pub last_received_sequence: Option<u64>,
    pub last_accepted_sequence: Option<u64>,
    pub last_received_started: Option<Duration>,
    pub last_received_finished: Option<Duration>,
    pub last_received_tick: Option<u64>,
    pub last_accepted_started: Option<Duration>,
    pub last_accepted_finished: Option<Duration>,
    pub last_accepted_tick: Option<u64>,
    pub last_observation_outcome: ClockObservationOutcome,
    pub last_received_age: Option<Duration>,
    pub last_accepted_age: Option<Duration>,
}

#[derive(Clone, Copy)]
struct Calibration {
    model: ClockModel,
    accepted_sequence: u64,
    last_received: Option<ClockSample>,
    last_accepted: Option<ClockSample>,
    outcome: ClockObservationOutcome,
}

struct ClockState {
    calibration: ArcSwap<Calibration>,
    failed: AtomicBool,
    floor_host_ns: AtomicU64,
    floor_tick: AtomicU64,
}

impl ClockState {
    fn new(calibration: Calibration) -> Arc<Self> {
        Arc::new(Self {
            calibration: ArcSwap::from_pointee(calibration),
            failed: AtomicBool::new(false),
            floor_host_ns: AtomicU64::new(NO_FLOOR),
            floor_tick: AtomicU64::new(0),
        })
    }
    // The host time is stored first and loaded last, so a torn read pairs an older tick
    // with a newer host time, which only lowers the floor.
    fn store_floor(&self, floor: ClockFloor) {
        let Ok(host) = u64::try_from(floor.host.as_nanos()) else {
            return;
        };
        self.floor_host_ns.store(host, Ordering::Release);
        self.floor_tick.store(floor.tick, Ordering::Release);
    }
    fn floor(&self) -> Option<ClockFloor> {
        let tick = self.floor_tick.load(Ordering::Acquire);
        let host = self.floor_host_ns.load(Ordering::Acquire);
        (host != NO_FLOOR).then(|| ClockFloor {
            host: Duration::from_nanos(host),
            tick,
        })
    }
    fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
    fn check(&self) -> Result<(), ClockError> {
        if self.failed.load(Ordering::Acquire) {
            Err(ClockError::Discontinuity)
        } else {
            Ok(())
        }
    }
}

pub(super) struct SampledClock {
    origin: HostClock,
    calibration: Calibration,
    state: Arc<ClockState>,
}

impl SampledClock {
    pub(super) fn accepted_sequence(&self) -> u64 {
        self.calibration.accepted_sequence
    }

    pub(super) fn snapshot(&self) -> ClockSamplingSnapshot {
        let calibration = &self.calibration;
        let received = calibration.last_received.map(|sample| sample.observation);
        let accepted = calibration.last_accepted.map(|sample| sample.observation);
        let elapsed = self.origin.elapsed();
        ClockSamplingSnapshot {
            startup_seeded: true,
            accepted_sequence: calibration.accepted_sequence,
            last_received_sequence: calibration.last_received.map(|sample| sample.sequence),
            last_accepted_sequence: calibration.last_accepted.map(|sample| sample.sequence),
            last_received_started: received.map(|observation| observation.started),
            last_received_finished: received.map(|observation| observation.finished),
            last_received_tick: received.map(|observation| observation.tick),
            last_accepted_started: accepted.map(|observation| observation.started),
            last_accepted_finished: accepted.map(|observation| observation.finished),
            last_accepted_tick: accepted.map(|observation| observation.tick),
            last_observation_outcome: calibration.outcome,
            last_received_age: received
                .and_then(|observation| elapsed.checked_sub(observation.finished)),
            last_accepted_age: accepted
                .and_then(|observation| elapsed.checked_sub(observation.finished)),
        }
    }

    pub(super) fn read(&mut self) -> Result<ClockBounds, ClockError> {
        self.read_with_sequence().map(|(bounds, _)| bounds)
    }

    pub(super) fn read_with_sequence(&mut self) -> Result<(ClockBounds, u64), ClockError> {
        self.state.check()?;
        let published = self.state.calibration.load();
        if published.last_received.map(|sample| sample.sequence)
            != self.calibration.last_received.map(|sample| sample.sequence)
        {
            self.calibration = **published;
        }
        drop(published);
        let floor = self.state.floor();
        let now = self.origin.elapsed();
        let model = &self.calibration.model;
        let bounds = match floor {
            Some(floor) => model.bounds_above(now, floor),
            None => model.bounds(now),
        }
        .inspect_err(|_| self.state.fail())?;
        self.state.check()?;
        Ok((bounds, self.calibration.accepted_sequence))
    }
}

impl Drop for SampledClock {
    fn drop(&mut self) {
        self.state.fail();
    }
}

/// Received samples include device timestamps, so the newest one is a lower bound on the clock.
pub(super) struct ClockFloorWriter {
    origin: Instant,
    model: ClockModel,
    state: Arc<ClockState>,
    last: Option<ClockFloor>,
}

impl ClockFloorWriter {
    pub(super) fn raise(&mut self, returned_at: Instant, tick: u64) {
        let Some(host) = returned_at.checked_duration_since(self.origin) else {
            return;
        };
        if let Some(last) = self.last {
            let projected = host
                .checked_sub(last.host)
                .and_then(|elapsed| self.model.nominal_ticks(elapsed).ok())
                .and_then(|elapsed| last.tick.checked_add(elapsed));
            if projected.is_none_or(|projected| tick < projected) {
                return;
            }
        }
        let floor = ClockFloor { host, tick };
        self.last = Some(floor);
        self.state.store_floor(floor);
    }
}

pub(super) struct DeviceClockSampler {
    origin: HostClock,
    interval: Duration,
    next: Duration,
    next_sequence: u64,
    calibration: Calibration,
    state: Arc<ClockState>,
}

fn observation(
    origin: &HostClock,
    query: impl FnOnce() -> Result<u64, sdr_hal::error::Error>,
) -> Result<Observation, sdr_hal::error::Error> {
    let started = origin.elapsed();
    let tick = query()?;
    Ok(Observation {
        started,
        finished: origin.elapsed(),
        tick,
    })
}

impl DeviceClockSampler {
    pub(super) fn sample_if_due(
        &mut self,
        rx: &dyn sdr_hal::RadioRx,
        counters: &mut RxCounters,
    ) -> Result<(), RxError> {
        self.state.check().map_err(RxError::ClockModel)?;
        if self.origin.elapsed() < self.next {
            return Ok(());
        }
        let sample = match observation(&self.origin, || rx.get_hardware_time()) {
            Ok(sample) => sample,
            Err(error) => {
                self.state.fail();
                return Err(RxError::Driver(error));
            }
        };
        record_query(counters, sample);
        self.next = match sample.finished.checked_add(self.interval) {
            Some(next) => next,
            None => {
                self.state.fail();
                return Err(RxError::ClockModel(ClockError::Overflow));
            }
        };
        let sequence = self.next_sequence;
        self.observe(ClockSample {
            sequence,
            observation: sample,
        })
        .map_err(RxError::ClockModel)?;
        Ok(())
    }
    fn poll(
        &mut self,
        clock: &dyn sdr_hal::RadioClock,
        stats: &ClockQueryStats,
    ) -> Result<(), ClockError> {
        self.state.check()?;
        let maximum_query = self.calibration.model.settings().maximum_query;
        let mut best: Option<Observation> = None;
        for attempt in 0..POLL_READS {
            if attempt > 0 {
                thread::sleep(POLL_READ_SPACING);
            }
            let Ok(sample) = observation(&self.origin, || clock.get_hardware_time()) else {
                self.state.fail();
                return Err(ClockError::Discontinuity);
            };
            stats.record(sample);
            let elapsed = sample.finished - sample.started;
            if best.is_none_or(|best| elapsed < best.finished - best.started) {
                best = Some(sample);
            }
            if elapsed <= maximum_query {
                break;
            }
        }
        let sample = best.expect("at least one clock reading");
        self.next = sample.finished.checked_add(self.interval).ok_or_else(|| {
            self.state.fail();
            ClockError::Overflow
        })?;
        self.observe(ClockSample {
            sequence: self.next_sequence,
            observation: sample,
        })
    }

    fn observe(&mut self, sample: ClockSample) -> Result<(), ClockError> {
        self.state.check()?;
        if sample.sequence != self.next_sequence {
            self.state.fail();
            return Err(ClockError::Discontinuity);
        }
        let next = sample.sequence.checked_add(1).ok_or_else(|| {
            self.state.fail();
            ClockError::Overflow
        })?;
        self.calibration.last_received = Some(sample);
        self.calibration.outcome = ClockObservationOutcome::None;
        match self.calibration.model.observe_estimated(sample.observation) {
            Ok(result @ (ObserveResult::Accepted | ObserveResult::Estimated)) => {
                self.calibration.accepted_sequence = sample.sequence;
                self.calibration.last_accepted = Some(sample);
                self.calibration.outcome = if result == ObserveResult::Accepted {
                    ClockObservationOutcome::Accepted
                } else {
                    ClockObservationOutcome::Estimated
                };
            }
            Ok(ObserveResult::RejectedSlow) => {
                self.calibration.outcome = ClockObservationOutcome::RejectedSlow
            }
            Err(error) => {
                self.state.fail();
                return Err(error);
            }
        }
        self.state.calibration.store(Arc::new(self.calibration));
        self.next_sequence = next;
        Ok(())
    }
}

impl Drop for DeviceClockSampler {
    fn drop(&mut self) {
        self.state.fail();
    }
}

fn record_query(counters: &mut RxCounters, sample: Observation) {
    let elapsed = (sample.finished - sample.started).as_nanos();
    counters.clock_queries = counters.clock_queries.saturating_add(1);
    counters.clock_query_total_ns = counters.clock_query_total_ns.saturating_add(elapsed);
    counters.clock_query_maximum_ns = counters.clock_query_maximum_ns.max(elapsed);
}

impl TxPump {
    pub fn use_sampled_clock_pair(
        &mut self,
        preparation: &mut super::TxPreparation,
        receiver: &mut TimedRx,
        settings: ClockSamplingSettings,
    ) -> Result<(), PumpError> {
        self.use_sampled_clock_pair_with_host(
            preparation,
            receiver,
            settings,
            HostClock::System(Instant::now()),
        )
    }

    pub(crate) fn use_sampled_clock_pair_with_host(
        &mut self,
        preparation: &mut super::TxPreparation,
        receiver: &mut TimedRx,
        settings: ClockSamplingSettings,
        origin: HostClock,
    ) -> Result<(), PumpError> {
        if self.submission.is_active()
            || self.failed
            || self.stop.is_stopping()
            || preparation.failed
            || preparation.stop.is_stopping()
            || receiver.active
            || receiver.failed
            || receiver.air_origin != self.air_origin
            || preparation.air_origin != self.air_origin
            || receiver.clock_sampler.is_some()
            || self.sampled_clock.is_some()
            || preparation.sampled_clock.is_some()
            || self.pacing.is_none()
            || preparation.pacing.is_none()
            || !preparation.paired_with(self)
        {
            return Err(PumpError::ClockSamplingState);
        }
        if settings.interval.is_zero()
            || settings
                .model
                .maximum_age
                .is_some_and(|age| settings.interval >= age)
        {
            return Err(PumpError::ClockModel(ClockError::InvalidSettings));
        }
        let model = startup_model(self, receiver, settings.model, &origin)?;
        let bounds = model
            .bounds(origin.elapsed())
            .map_err(PumpError::ClockModel)?;
        let calibration = Calibration {
            model,
            accepted_sequence: 0,
            last_received: None,
            last_accepted: None,
            outcome: ClockObservationOutcome::None,
        };
        let state = ClockState::new(calibration);
        let next = origin
            .elapsed()
            .checked_add(settings.interval)
            .ok_or(PumpError::ClockModel(ClockError::Overflow))?;
        self.pacing
            .as_mut()
            .expect("validated pacing")
            .observed
            .observe_bounds(bounds)
            .map_err(PumpError::Clock)?;
        let sampler = DeviceClockSampler {
            origin: origin.clone(),
            interval: settings.interval,
            next,
            next_sequence: 1,
            calibration,
            state: state.clone(),
        };
        match (&origin, self.submission.clock_reader()) {
            (HostClock::System(started), Some(clock)) => {
                receiver.clock_floor = Some(ClockFloorWriter {
                    origin: *started,
                    model,
                    state: state.clone(),
                    last: None,
                });
                let stats = Arc::new(ClockQueryStats::default());
                receiver.clock_stats = Some(stats.clone());
                self.clock_poller = Some(
                    ClockPoller::spawn(sampler, clock, stats)
                        .map_err(|_| PumpError::ClockSamplingState)?,
                );
            }
            _ => receiver.clock_sampler = Some(sampler),
        }
        self.sampled_clock = Some(SampledClock {
            origin: origin.clone(),
            calibration,
            state: state.clone(),
        });
        preparation.sampled_clock = Some(SampledClock {
            origin,
            calibration,
            state,
        });
        Ok(())
    }
}

fn startup_model(
    pump: &TxPump,
    receiver: &mut TimedRx,
    settings: ClockSettings,
    origin: &HostClock,
) -> Result<ClockModel, PumpError> {
    let mut model =
        ClockModel::new(pump.submission.tick_rate(), settings).map_err(PumpError::ClockModel)?;
    let mut fastest: Option<Observation> = None;
    let mut accepted = false;
    for _ in 0..STARTUP_CLOCK_QUERIES {
        let sample = observation(origin, || pump.submission.hardware_time())
            .map_err(PumpError::DriverClock)?;
        let elapsed = sample.finished - sample.started;
        if elapsed <= settings.maximum_query {
            model.observe(sample).map_err(PumpError::ClockModel)?;
            accepted = true;
            break;
        }
        if fastest.is_none_or(|best| elapsed < best.finished - best.started) {
            fastest = Some(sample);
        }
    }
    if !accepted {
        let sample = fastest.ok_or(PumpError::ClockModel(ClockError::Unobserved))?;
        model
            .observe_estimated(sample)
            .map_err(PumpError::ClockModel)?;
    }
    for _ in 0..STARTUP_CLOCK_QUERIES {
        let sample = observation(origin, || {
            receiver
                .rx
                .as_ref()
                .expect("inactive RX handle")
                .get_hardware_time()
        })
        .map_err(PumpError::DriverClock)?;
        record_query(&mut receiver.counters, sample);
        model
            .observe_estimated(sample)
            .map_err(PumpError::ClockModel)?;
    }
    model
        .bounds(origin.elapsed())
        .map_err(PumpError::ClockModel)?;
    Ok(model)
}

#[cfg(test)]
mod tests;

impl TimedRx {
    pub(crate) fn sample_clock(&mut self) -> Result<(), RxError> {
        if let Some(sampler) = &mut self.clock_sampler {
            sampler.sample_if_due(
                &**self.rx.as_ref().ok_or(RxError::Poisoned)?,
                &mut self.counters,
            )?;
        }
        Ok(())
    }
}
