// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use sdr_hal::{RadioRx, RadioTx};
use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Instant;
use umts_dsp::{
    DspError,
    pulse_shaping::SamplesPerChip,
    receive_filter::{FilteredWindow, ReceiveFilter},
    waveform::WaveformWindow,
};
use umts_radio::{
    RxTimeline,
    clock_model::ClockBounds,
    pacing::{Gate, LeadBudget, LeadDurations, LeadError},
    transmit::{PrepareError, SubmitFailure, SubmitFailureKind, TxSubmission},
};
use umts_runtime::mailbox::{self, Receiver, Sender};
use umts_runtime::worker::StopToken;
use umts_types::time::{
    CHIPS_PER_SECOND, CHIPS_PER_SLOT, ChipInstant, SampleClock, SampleInstant, SampleRate,
    TimeError,
};

use crate::common_tx::{CommonTx, CommonTxError};

pub mod capture;
mod clock;
mod health;
mod pacing;
mod reader;
pub(crate) mod sampled_clock;
mod shutdown;
pub mod telemetry;
mod timing;
mod workers;
pub use clock::{RadioClock, RadioClockError, RadioTime};
pub use health::{TxHealthFailure, TxHealthSettings, TxHealthSnapshot};
pub use pacing::{ClockFailure, UncertainWaitError};
use pacing::{PreparationPacing, SubmissionPacing};
pub use reader::ReaderCounters;
pub use sampled_clock::{ClockSamplingSettings, ClockSamplingSnapshot};
pub use shutdown::{HalStopOutcome, TerminalDrainError, TerminalTransmitDrain, TransmitTail};
#[cfg(test)]
pub use timing::WAIT_HISTORY_CAPACITY;
pub use timing::{
    HostOffset, PreparationTimings, PublicationEvidence, RxReadTiming, SubmissionTimings,
    WallTimeSummary,
};
pub use workers::{TxWorkerError, TxWorkerReport, TxWorkers};

const MAX_PREPARED_SLOTS: usize = 64;
const MAX_RX_TIMEOUT_US: i64 = 10_000;

fn deadline_wait_duration(
    until_tick: u64,
    deadline_tick: u64,
    bounds: ClockBounds,
    tick_rate: NonZeroU64,
    maximum: std::time::Duration,
) -> Result<std::time::Duration, UncertainWaitError> {
    match pacing::bounded_uncertain_wait_duration(
        until_tick,
        deadline_tick,
        bounds,
        tick_rate,
        maximum,
    ) {
        Err(UncertainWaitError::DeadlinePassed) => Ok(std::time::Duration::ZERO),
        result => result,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TxTiming {
    #[cfg_attr(not(test), expect(dead_code, reason = "Constructed by test helpers."))]
    Offline,
    DeviceClock(LeadDurations),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxBatch {
    OneSlot,
    TwoSlots,
}

impl TxBatch {
    fn slots(self) -> usize {
        match self {
            Self::OneSlot => 1,
            Self::TwoSlots => 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TxSettings {
    pub origin_tick: u64,
    pub prepared_slots: NonZeroUsize,
    pub maximum_calls: NonZeroUsize,
    pub maximum_attempts: NonZeroUsize,
    pub timing: TxTiming,
}

impl TxSettings {
    #[cfg(test)]
    pub fn offline(
        origin_tick: u64,
        prepared_slots: NonZeroUsize,
        maximum_calls: NonZeroUsize,
        maximum_attempts: NonZeroUsize,
    ) -> Self {
        Self {
            origin_tick,
            prepared_slots,
            maximum_calls,
            maximum_attempts,
            timing: TxTiming::Offline,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadlineFailure {
    pub device_tick: u64,
    pub first_tick: u64,
    pub shortfall_ticks: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AirClockOrigin {
    pub chip: ChipInstant,
    // Applied chip frequency in the HAL time domain, not the nominal 3.84 Mcps constant.
    pub chip_rate: SampleRate,
    pub tick: u64,
    pub tick_rate: NonZeroU64,
}

#[derive(Clone, Copy, Debug)]
pub struct RxSettings {
    pub rate: SamplesPerChip,
    pub span_chips: usize,
    pub phase: usize,
    pub maximum_samples: NonZeroUsize,
    pub timeout_us: i64,
}

fn exceeds_component_limit(samples: &[Complex32], limit: f32) -> Result<bool, CommonTxError> {
    umts_dsp::sample_validation::exceeds_component_limit(samples, limit).map_err(CommonTxError::Dsp)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BatchContents {
    Waveform,
    QualityMuted,
}

struct PreparedBatch {
    window: WaveformWindow,
    samples: Vec<Complex32>,
    accepted: usize,
    attempts: usize,
    queued_at: Option<Instant>,
    dequeued_at: Option<Instant>,
    publication_dequeued_at: Option<Instant>,
    late: Option<DeadlineFailure>,
    contents: BatchContents,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxRecoveryCounters {
    pub amplitude_dropped: u64,
    pub quality_muted_batches: u64,
    pub quality_muted_samples: u64,
    pub preparation_late: u64,
    pub submission_late: u64,
    pub driver_dropped: u64,
    pub retries_exhausted: u64,
    pub dropped_batches: u64,
    pub dropped_samples: u64,
    pub health_warnings: u64,
}

#[derive(Debug)]
pub enum PreparationError {
    Wait(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        UncertainWaitError,
    ),
    Common(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        CommonTxError,
    ),
    Dedicated(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::dedicated_tx::TransmitError,
    ),
    PoolInvariant,
    Stopped,
    Poisoned,
    Clock(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        ClockFailure,
    ),
    ClockModel(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        umts_radio::clock_model::ClockError,
    ),
    Time(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        TimeError,
    ),
    Lead(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        LeadError,
    ),
}

pub struct TxPreparation {
    source: CommonTx,
    component_peak_limit: Option<f32>,
    batch: TxBatch,
    sampled_clock: Option<sampled_clock::SampledClock>,
    pub(crate) submission_wake: Option<std::sync::Arc<std::sync::OnceLock<std::thread::Thread>>>,
    pub(crate) spin_wait: bool,
    dedicated: Option<crate::dedicated_tx::DedicatedTransmitRuntime>,
    #[cfg(test)]
    retirement_probe: Option<Box<dyn Send>>,
    air_origin: AirClockOrigin,
    ready: Sender<PreparedBatch>,
    free: Receiver<Vec<Complex32>>,
    recycle: Sender<Vec<Complex32>>,
    failed: bool,
    stop: StopToken,
    pacing: Option<PreparationPacing>,
    timings: Option<PreparationTimings>,
    amplitude_mutes: u64,
}

// Every mute is logged until this many, then one in each later block.
const LOGGED_AMPLITUDE_MUTES: u64 = 32;
const AMPLITUDE_MUTE_LOG_INTERVAL: u64 = 1024;

impl TxPreparation {
    #[cfg(test)]
    pub fn enable_timing(&mut self) {
        self.timings.get_or_insert_default();
    }

    pub fn timings(&self) -> Option<PreparationTimings> {
        self.timings
    }

    pub fn dedicated_ports(
        &mut self,
        maximum_owners: NonZeroUsize,
        maximum_channels: NonZeroUsize,
        maximum_retained_bits: NonZeroUsize,
        limits: crate::dedicated_tx::TransmitRuntimeLimits,
    ) -> Result<crate::dedicated_tx::DedicatedTransmitPorts, crate::dedicated_tx::TransmitError>
    {
        if self.dedicated.is_some() || self.failed || self.stop.is_stopping() {
            return Err(crate::dedicated_tx::TransmitError::Phase);
        }
        let (runtime, ports) = crate::dedicated_tx::DedicatedTransmitRuntime::new(
            &self.source,
            maximum_owners,
            maximum_channels,
            maximum_retained_bits,
            limits,
        )?;
        self.dedicated = Some(runtime);
        Ok(ports)
    }
    pub fn dedicated_paired_with(&self, port: &crate::dedicated_tx::DedicatedTransmitPort) -> bool {
        self.dedicated
            .as_ref()
            .is_some_and(|runtime| port.paired_with(runtime))
    }
    // Cold setup API. Rejection preserves the complete bank and its resource lease.
    pub fn attach_hsdpa(
        &mut self,
        bank: crate::hsdpa_tx::TransmitBank,
    ) -> Result<(), Box<crate::hsdpa_tx::TransmitBank>> {
        if self.failed || self.stop.is_stopping() || !bank.paired_with(&self.source) {
            return Err(Box::new(bank));
        }
        let Some(runtime) = &mut self.dedicated else {
            return Err(Box::new(bank));
        };
        runtime.attach_hsdpa(bank)
    }

    pub fn attach_hsupa(
        &mut self,
        bank: Box<crate::hsupa_tx::TransmitBank>,
    ) -> Result<(), Box<crate::hsupa_tx::TransmitBank>> {
        if self.failed || self.stop.is_stopping() || !bank.paired_with(&self.source) {
            return Err(bank);
        }
        let Some(runtime) = &mut self.dedicated else {
            return Err(bank);
        };
        runtime.attach_hsupa(bank)
    }

    pub fn dedicated_feedback(
        &self,
        port: crate::dedicated_tx::DedicatedTpcPort,
    ) -> Result<Box<crate::dedicated_rx::DedicatedFeedback>, crate::dedicated_tx::DedicatedTpcPort>
    {
        let Some(runtime) = &self.dedicated else {
            return Err(port);
        };
        let Some(pacing) = &self.pacing else {
            return Err(port);
        };
        if !port.paired_with(runtime)
            || port.outstanding() != 0
            || !port.available()
            || self.failed
            || self.stop.is_stopping()
        {
            return Err(port);
        }
        let ticks = pacing.budget.maximum_preparation_ticks();
        let Ok(chips) = self
            .air_origin
            .chip_rate
            .ticks_to_samples_ceil(ticks, self.air_origin.tick_rate)
        else {
            return Err(port);
        };
        if chips == 0 {
            return Err(port);
        }
        let clock = self
            .radio_clock()
            .expect("paced preparation owns its clock");
        Ok(Box::new(
            crate::dedicated_rx::DedicatedFeedback::from_preparation(port, clock, chips),
        ))
    }
    pub(crate) fn close_dedicated(&mut self) {
        if let Some(runtime) = &mut self.dedicated {
            runtime.close();
        }
    }
    pub fn radio_clock(&self) -> Option<RadioClock> {
        self.pacing.as_ref().map(|pacing| RadioClock {
            origin: self.air_origin,
            observed: pacing.observed.clone(),
        })
    }

    pub(crate) fn paired_with(&self, pump: &TxPump) -> bool {
        matches!((&self.pacing, &pump.pacing), (Some(first), Some(second)) if first.observed.same_clock(&second.observed))
    }
    pub fn control_ports(
        &mut self,
        capacity: NonZeroUsize,
        command_budget: NonZeroUsize,
        acquisition_capacity: NonZeroUsize,
        acquisition_budget: NonZeroUsize,
    ) -> Result<crate::common_tx::ControlPorts, CommonTxError> {
        self.source.control_ports(
            capacity,
            command_budget,
            acquisition_capacity,
            acquisition_budget,
        )
    }

    #[cfg(test)]
    pub fn control_port(
        &mut self,
        capacity: NonZeroUsize,
        command_budget: NonZeroUsize,
    ) -> Result<crate::common_tx::CommonTxPort, CommonTxError> {
        self.source.control_port(capacity, command_budget)
    }
    pub fn source(&self) -> &CommonTx {
        &self.source
    }
    #[cfg(test)]
    pub fn source_mut(&mut self) -> &mut CommonTx {
        &mut self.source
    }
    pub fn air_origin(&self) -> AirClockOrigin {
        self.air_origin
    }
    pub fn stop_token(&self) -> StopToken {
        self.stop.clone()
    }

    #[cfg(test)]
    pub fn prepare_one(&mut self) -> Result<Option<WaveformWindow>, PreparationError> {
        Ok(match self.prepare_progress()? {
            PreparationProgress::Prepared(window) | PreparationProgress::Skipped { window, .. } => {
                Some(window)
            }
            PreparationProgress::Idle | PreparationProgress::Waiting { .. } => None,
        })
    }

    pub(crate) fn wait_schedule(
        &mut self,
        until_tick: u64,
        deadline_tick: u64,
        maximum: std::time::Duration,
    ) -> Result<WaitSchedule, PreparationError> {
        let evidence = self.clock_evidence()?;
        let duration = if evidence.source == WaitClockSource::Published {
            maximum
        } else {
            deadline_wait_duration(
                until_tick,
                deadline_tick,
                evidence.bounds,
                self.air_origin.tick_rate,
                maximum,
            )
            .map_err(PreparationError::Wait)?
        };
        Ok(WaitSchedule {
            duration,
            target_tick: until_tick,
            deadline_tick,
            evidence,
        })
    }

    fn clock_evidence(&mut self) -> Result<ClockEvidence, PreparationError> {
        if let Some(clock) = &mut self.sampled_clock {
            let (bounds, sequence) = clock
                .read_with_sequence()
                .map_err(PreparationError::ClockModel)?;
            Ok(ClockEvidence {
                bounds,
                model_sequence: Some(sequence),
                source: WaitClockSource::FreshSampled,
            })
        } else {
            let bounds = self
                .pacing
                .as_ref()
                .ok_or(PreparationError::Clock(ClockFailure::Unavailable))?
                .observed
                .read_bounds()
                .map_err(PreparationError::Clock)?;
            Ok(ClockEvidence {
                bounds,
                model_sequence: None,
                source: WaitClockSource::Published,
            })
        }
    }

    pub(crate) fn prepare_progress(&mut self) -> Result<PreparationProgress, PreparationError> {
        let started = self.timings.as_ref().map(|_| Instant::now());
        let result = self.prepare_inner();
        if let (Some(timings), Some(started)) = (&mut self.timings, started) {
            timings.cycle.record(started.elapsed());
        }
        if result.is_err() && !matches!(result, Err(PreparationError::Stopped)) {
            self.failed = true;
            self.stop.request_stop();
        }
        result
    }

    fn prepare_inner(&mut self) -> Result<PreparationProgress, PreparationError> {
        if self.failed {
            return Err(PreparationError::Poisoned);
        }
        if self.stop.is_stopping() {
            return Err(PreparationError::Stopped);
        }
        if let Some(runtime) = &mut self.dedicated {
            runtime.service().map_err(PreparationError::Dedicated)?;
        }
        let mut late = None;
        if let Some(pacing) = &self.pacing {
            let now = match &mut self.sampled_clock {
                Some(clock) => clock.read().map_err(PreparationError::ClockModel)?,
                None => pacing
                    .observed
                    .read_bounds()
                    .map_err(PreparationError::Clock)?,
            };
            let rate = self.source.samples_per_slot() as u64 / CHIPS_PER_SLOT;
            let first = self
                .source
                .next_chip()
                .index
                .checked_mul(rate)
                .ok_or(PreparationError::Time(TimeError::Overflow))?;
            let end = first
                .checked_add((self.source.samples_per_slot() * self.batch.slots()) as u64)
                .ok_or(PreparationError::Time(TimeError::Overflow))?;
            let first = pacing
                .clock
                .tick_at(SampleInstant {
                    epoch: self.source.next_chip().epoch,
                    index: first,
                })
                .map_err(PreparationError::Time)?;
            let end = pacing
                .clock
                .tick_at(SampleInstant {
                    epoch: self.source.next_chip().epoch,
                    index: end,
                })
                .map_err(PreparationError::Time)?
                .checked_add(1)
                .ok_or(PreparationError::Time(TimeError::Overflow))?;
            match pacing
                .budget
                .preparation_bounds(now, first, end)
                .map_err(PreparationError::Lead)?
            {
                Gate::Ready => {}
                Gate::WaitUntil(until_tick) => {
                    return Ok(PreparationProgress::Waiting {
                        until_tick,
                        deadline_tick: first
                            .checked_sub(pacing.budget.minimum_ticks())
                            .ok_or(PreparationError::Time(TimeError::Overflow))?,
                        bounds: now,
                    });
                }
                Gate::Missed { shortfall_ticks } => {
                    late = Some(DeadlineFailure {
                        device_tick: now.latest,
                        first_tick: first,
                        shortfall_ticks,
                    });
                }
            }
        }
        let Some(mut samples) = self.free.try_recv() else {
            return Ok(PreparationProgress::Idle);
        };
        let started = self.timings.as_ref().map(|_| Instant::now());
        let generated = self.generate_batch(&mut samples, late.is_some());
        if let (Some(timings), Some(started)) = (&mut self.timings, started) {
            timings.synthesis.record(started.elapsed());
        }
        let window = match generated {
            Ok(window) => window,
            Err(error) => {
                self.failed = true;
                self.stop.request_stop();
                let _ = self.recycle.try_send(samples);
                return Err(error);
            }
        };
        let amplitude_rejected = if let Some(limit) = self.component_peak_limit {
            match exceeds_component_limit(&samples, limit) {
                Ok(rejected) => rejected,
                Err(error) => {
                    self.failed = true;
                    self.stop.request_stop();
                    let _ = self.recycle.try_send(samples);
                    return Err(PreparationError::Common(error));
                }
            }
        } else {
            false
        };
        let crest = self.source.take_crest_report();
        let amplitude_rejected = crest.rejected() || amplitude_rejected;
        if amplitude_rejected {
            let peak = samples.iter().fold(0.0_f32, |peak, sample| {
                peak.max(sample.re.abs()).max(sample.im.abs())
            });
            self.amplitude_mutes = self.amplitude_mutes.saturating_add(1);
            if self.amplitude_mutes <= LOGGED_AMPLITUDE_MUTES
                || self
                    .amplitude_mutes
                    .is_multiple_of(AMPLITUDE_MUTE_LOG_INTERVAL)
            {
                tracing::warn!(
                    mute = self.amplitude_mutes,
                    first_sample = window.first_sample.index,
                    peak,
                    limit = self.component_peak_limit,
                    relative_error = crest.relative_error,
                    error_rms = crest.error_rms,
                    corrections = crest.corrections,
                    limited_components = crest.limited_components,
                    work_limit_exceeded = crest.work_limit_exceeded,
                    "TX batch muted for amplitude"
                );
            }
            if let Some(timings) = self.timings.as_mut() {
                const MILLIONTHS: f64 = 1_000_000.0;
                timings.amplitude_failures.record(timing::AmplitudeFailure {
                    first_sample: window.first_sample,
                    peak_millionths: (f64::from(peak) * MILLIONTHS) as u64,
                    limit_millionths: self
                        .component_peak_limit
                        .map(|limit| (f64::from(limit) * MILLIONTHS) as u64),
                    relative_error_millionths: (crest.relative_error * MILLIONTHS) as u64,
                    corrections: crest.corrections,
                    limited_components: crest.limited_components,
                    work_limit_exceeded: crest.work_limit_exceeded,
                });
            }
        }
        let contents = if amplitude_rejected {
            samples.fill(Complex32::default());
            BatchContents::QualityMuted
        } else {
            BatchContents::Waveform
        };
        let queued_at = self.timings.as_ref().map(|_| Instant::now());
        // Every ready/pending batch owns one buffer from this same fixed pool.
        if let Err(slot) = self.ready.try_send(PreparedBatch {
            window,
            samples,
            accepted: 0,
            attempts: 0,
            queued_at,
            dequeued_at: None,
            publication_dequeued_at: None,
            late,
            contents,
        }) {
            self.failed = true;
            self.stop.request_stop();
            let _ = self.recycle.try_send(slot.samples);
            return Err(PreparationError::PoolInvariant);
        }
        let enqueue_returned_ns = queued_at.map(|started| started.elapsed().as_nanos());
        let mut unpark_started_ns = None;
        let mut unpark_returned_ns = None;
        let mut wake_registered = false;
        if let Some(wake) = &self.submission_wake
            && let Some(thread) = wake.get()
        {
            wake_registered = true;
            unpark_started_ns = queued_at.map(|started| started.elapsed().as_nanos());
            thread.unpark();
            unpark_returned_ns = queued_at.map(|started| started.elapsed().as_nanos());
        }
        if let Some(timings) = &mut self.timings {
            timings.published = timings.published.saturating_add(1);
            if let Some(enqueue_returned_ns) = enqueue_returned_ns {
                timings
                    .publication_completions
                    .push(timing::PublicationCompletionEvidence {
                        first_sample: window.first_sample,
                        next_sample: window.next_sample,
                        enqueue_returned_ns,
                        unpark_started_ns,
                        unpark_returned_ns,
                        wake_registered,
                    });
            }
        }
        Ok(match late {
            Some(deadline) => PreparationProgress::Skipped { window, deadline },
            None => PreparationProgress::Prepared(window),
        })
    }

    fn generate_batch(
        &mut self,
        samples: &mut [Complex32],
        lost: bool,
    ) -> Result<WaveformWindow, PreparationError> {
        let slot_samples = self.source.samples_per_slot();
        let mut batch: Option<WaveformWindow> = None;
        for (index, output) in samples.chunks_exact_mut(slot_samples).enumerate() {
            let window = match &mut self.dedicated {
                Some(runtime) => {
                    if index != 0 {
                        runtime.service().map_err(PreparationError::Dedicated)?;
                    }
                    if lost {
                        runtime.generate_lost_slot(&mut self.source, output)
                    } else {
                        runtime.generate_slot(&mut self.source, output)
                    }
                }
                None => self.source.generate_slot(output),
            }
            .map_err(PreparationError::Common)?;
            match &mut batch {
                Some(batch) => {
                    if batch.next_sample != window.first_sample {
                        return Err(PreparationError::PoolInvariant);
                    }
                    batch.next_sample = window.next_sample;
                }
                None => batch = Some(window),
            }
        }
        batch.ok_or(PreparationError::PoolInvariant)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PumpProgress {
    Idle,
    Waiting {
        until_tick: u64,
        deadline_tick: u64,
    },
    Submitted(WaveformWindow),
    Dropped {
        window: WaveformWindow,
        deadline: Option<DeadlineFailure>,
        cause: DropCause,
    },
    Partial {
        accepted: usize,
        total: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropCause {
    Amplitude,
    Preparation,
    Submission,
    Driver,
    Retries,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitClockSource {
    Published,
    FreshSampled,
    FreshHal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WaitSchedule {
    pub duration: std::time::Duration,
    pub target_tick: u64,
    pub deadline_tick: u64,
    pub evidence: ClockEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClockEvidence {
    pub bounds: ClockBounds,
    pub source: WaitClockSource,
    pub model_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreparationProgress {
    Idle,
    Waiting {
        until_tick: u64,
        deadline_tick: u64,
        bounds: ClockBounds,
    },
    Prepared(WaveformWindow),
    Skipped {
        window: WaveformWindow,
        deadline: DeadlineFailure,
    },
}

#[derive(Debug)]
pub enum PumpError {
    Wait(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        UncertainWaitError,
    ),
    Health(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        TxHealthFailure,
    ),
    DriverHealth(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        sdr_hal::error::Error,
    ),
    NotActive,
    Submission(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        SubmitFailure,
    ),
    PoolInvariant,
    Stopped,
    Poisoned,
    Clock(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        ClockFailure,
    ),
    DriverClock(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        sdr_hal::error::Error,
    ),
    Time(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        TimeError,
    ),
    Lead(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        LeadError,
    ),
    ClockModel(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        umts_radio::clock_model::ClockError,
    ),
    ClockSamplingState,
}

struct DroppedPublication {
    evidence: PublicationEvidence,
    enqueued: Instant,
    dequeued: Instant,
}

pub struct TxPump {
    recovery: TxRecoveryCounters,
    air_origin: AirClockOrigin,
    sampled_clock: Option<sampled_clock::SampledClock>,
    clock_poller: Option<sampled_clock::ClockPoller>,
    health: Option<health::HealthMonitor>,
    progress: crate::tx_evidence::Writer,
    submission: TxSubmission,
    next_expected_sample: SampleInstant,
    ready: Receiver<PreparedBatch>,
    free: Sender<Vec<Complex32>>,
    pending: Option<PreparedBatch>,
    dropped_publication: Option<DroppedPublication>,
    maximum_attempts: NonZeroUsize,
    failed: bool,
    stop: StopToken,
    pacing: Option<SubmissionPacing>,
    timings: Option<SubmissionTimings>,
    terminal_tail: Option<TransmitTail>,
    terminal_clock: Option<u64>,
    terminal_stopped: bool,
    recording: capture::TxRecording,
}

#[cfg(test)]
pub fn prepare_tx(
    source: CommonTx,
    tx: Box<dyn RadioTx>,
    settings: TxSettings,
) -> Result<(TxPreparation, TxPump), PrepareError> {
    prepare_tx_batched(source, tx, settings, TxBatch::OneSlot)
}

pub fn prepare_tx_batched(
    mut source: CommonTx,
    tx: Box<dyn RadioTx>,
    settings: TxSettings,
    batch: TxBatch,
) -> Result<(TxPreparation, TxPump), PrepareError> {
    let TxSettings {
        origin_tick,
        prepared_slots: capacity,
        maximum_calls,
        maximum_attempts,
        timing,
    } = settings;
    // One HAL write per paced cycle ensures every partial-write retry gets a fresh device-clock check.
    if matches!(timing, TxTiming::DeviceClock(_)) && maximum_calls.get() != 1 {
        return Err(PrepareError::Lead(LeadError::InvalidBudget));
    }
    if capacity.get() > MAX_PREPARED_SLOTS {
        return Err(PrepareError::Driver(
            sdr_hal::error::Error::invalid_setting("prepared TX pool exceeds capacity limit"),
        ));
    }
    // TS 25.211 7.3 AICH access slots span two radio slots and must not begin inside a batch.
    if !source
        .next_chip()
        .index
        .is_multiple_of(CHIPS_PER_SLOT * batch.slots() as u64)
    {
        return Err(PrepareError::Lead(LeadError::InvalidWindow));
    }
    let rate = source.samples_per_slot() / CHIPS_PER_SLOT as usize;
    let sample_rate = SampleRate::from_hz(tx.tx_sample_rate_hz()).map_err(PrepareError::Time)?;
    if sample_rate.hz().round() != CHIPS_PER_SECOND as f64 * rate as f64 {
        return Err(PrepareError::Time(TimeError::InvalidRate));
    }
    let chip_rate = sample_rate
        .divided_by(NonZeroU64::new(rate as u64).ok_or(PrepareError::Time(TimeError::InvalidRate))?)
        .map_err(PrepareError::Time)?;
    let origin = SampleInstant {
        epoch: source.next_chip().epoch,
        index: source
            .next_chip()
            .index
            .checked_mul(rate as u64)
            .ok_or(PrepareError::Time(TimeError::Overflow))?,
    };
    let length = source.samples_per_slot() * batch.slots();
    let tick_rate =
        NonZeroU64::new(tx.tick_rate()).ok_or(PrepareError::Time(TimeError::InvalidRate))?;
    let submission = TxSubmission::prepare(
        tx,
        origin,
        origin_tick,
        NonZeroUsize::new(length).expect("nonempty slot"),
        maximum_calls,
    )?;
    let (preparation_pacing, submission_pacing) = match timing {
        TxTiming::Offline => (None, None),
        TxTiming::DeviceClock(durations) => {
            let window = sample_rate
                .samples_to_ticks(length as u64, tick_rate)
                .map_err(PrepareError::Time)?
                .checked_add(1)
                .and_then(NonZeroU64::new)
                .ok_or(PrepareError::Time(TimeError::Overflow))?;
            let budget =
                LeadBudget::new(durations, tick_rate, window).map_err(PrepareError::Lead)?;
            let tick = submission.hardware_time().map_err(PrepareError::Driver)?;
            let (observed, writer) =
                pacing::clock_channel(tick).map_err(|_| PrepareError::Time(TimeError::Overflow))?;
            let clock = SampleClock::new(
                origin,
                submission.tick_at(origin).map_err(PrepareError::Time)?,
                sample_rate,
                tick_rate,
            );
            (
                Some(PreparationPacing {
                    clock,
                    budget,
                    observed,
                }),
                Some(SubmissionPacing {
                    budget,
                    observed: writer,
                }),
            )
        }
    };
    let reference_sample = SampleInstant {
        index: origin
            .index
            .checked_add(source.group_delay_samples() as u64)
            .ok_or(PrepareError::Time(TimeError::Overflow))?,
        ..origin
    };
    let air_origin = AirClockOrigin {
        chip: source.next_chip(),
        chip_rate,
        tick: submission
            .tick_at(reference_sample)
            .map_err(PrepareError::Time)?,
        tick_rate,
    };
    let (ready_tx, ready_rx) = mailbox::bounded(capacity);
    let (free_tx, free_rx) = mailbox::bounded(capacity);
    for _ in 0..capacity.get() {
        if free_tx
            .try_send(vec![Complex32::default(); length])
            .is_err()
        {
            return Err(PrepareError::Driver(
                sdr_hal::error::Error::invalid_setting("TX buffer-pool initialization failed"),
            ));
        }
    }
    let stop = StopToken::default();
    let progress = source.submission_writer().map_err(|_| {
        PrepareError::Driver(sdr_hal::error::Error::invalid_setting(
            "TX submission owner is already claimed",
        ))
    })?;
    let component_peak_limit = source.take_component_peak_limit();
    Ok((
        TxPreparation {
            source,
            component_peak_limit,
            batch,
            sampled_clock: None,
            submission_wake: None,
            spin_wait: false,
            dedicated: None,
            #[cfg(test)]
            retirement_probe: None,
            air_origin,
            ready: ready_tx,
            free: free_rx,
            recycle: free_tx.clone(),
            failed: false,
            stop: stop.clone(),
            pacing: preparation_pacing,
            amplitude_mutes: 0,
            timings: None,
        },
        TxPump {
            recovery: TxRecoveryCounters::default(),
            air_origin,
            sampled_clock: None,
            clock_poller: None,
            health: None,
            progress,
            submission,
            next_expected_sample: origin,
            ready: ready_rx,
            free: free_tx,
            pending: None,
            dropped_publication: None,
            maximum_attempts,
            failed: false,
            stop,
            pacing: submission_pacing,
            timings: None,
            terminal_tail: None,
            terminal_clock: None,
            terminal_stopped: false,
            recording: capture::TxRecording::default(),
        },
    ))
}

impl TxPump {
    fn urgent_queue_wait(&mut self, margin: std::time::Duration) -> Result<bool, PumpError> {
        let Some(pacing) = &self.pacing else {
            return Ok(false);
        };
        let first = self
            .submission
            .tick_at(self.next_expected_sample)
            .map_err(PumpError::Time)?;
        let Some(deadline) = first.checked_sub(pacing.budget.minimum_ticks()) else {
            return Ok(false);
        };
        let latest = pacing.observed.bounds().latest;
        let Some(remaining) = deadline.checked_sub(latest) else {
            return Ok(false);
        };
        let margin_ticks = margin
            .as_nanos()
            .checked_mul(u128::from(self.submission.tick_rate().get()))
            .ok_or(PumpError::Wait(UncertainWaitError::Overflow))?
            / 1_000_000_000;
        Ok(u128::from(remaining) <= margin_ticks)
    }

    pub(crate) fn attach_live_capture(&mut self, recorder: capture::live::Recorder) {
        self.recording.attach_live(recorder);
    }
    pub fn recovery(&self) -> TxRecoveryCounters {
        self.recovery
    }

    pub(crate) fn finish_capture(&mut self) {
        self.recording.finish()
    }

    #[cfg(test)]
    pub fn enable_timing(&mut self) {
        self.timings.get_or_insert_default();
    }

    pub fn timings(&self) -> Option<SubmissionTimings> {
        self.timings
    }

    pub fn monitor_health(&mut self, settings: TxHealthSettings) -> Result<(), PumpError> {
        if self.submission.is_active() || self.health.is_some() || self.pacing.is_none() {
            return Err(PumpError::Health(TxHealthFailure::InvalidState));
        }
        let mut monitor = health::HealthMonitor::new(
            settings,
            self.submission.tick_rate(),
            TxHealthSnapshot::default(),
        )
        .map_err(PumpError::Health)?;
        monitor.last = self
            .submission
            .health()
            .map_err(PumpError::DriverHealth)?
            .into();
        self.health = Some(monitor);
        Ok(())
    }

    pub fn health(&self) -> Option<TxHealthSnapshot> {
        self.health.as_ref().map(|monitor| monitor.last)
    }
    pub fn start(&mut self) -> Result<(), sdr_hal::error::Error> {
        if self.stop.is_stopping() {
            return Err(sdr_hal::error::Error::invalid_setting(
                "TX pipeline has been stopped",
            ));
        }
        self.submission.start()
    }

    pub(crate) fn wait_schedule(
        &mut self,
        until_tick: u64,
        deadline_tick: u64,
        maximum: std::time::Duration,
    ) -> Result<WaitSchedule, PumpError> {
        let evidence = self.clock_evidence()?;
        Ok(WaitSchedule {
            duration: deadline_wait_duration(
                until_tick,
                deadline_tick,
                evidence.bounds,
                self.submission.tick_rate(),
                maximum,
            )
            .map_err(PumpError::Wait)?,
            target_tick: until_tick,
            deadline_tick,
            evidence,
        })
    }

    fn clock_evidence(&mut self) -> Result<ClockEvidence, PumpError> {
        if let Some(clock) = &mut self.sampled_clock {
            let (bounds, sequence) = clock.read_with_sequence().map_err(PumpError::ClockModel)?;
            Ok(ClockEvidence {
                bounds,
                source: WaitClockSource::FreshSampled,
                model_sequence: Some(sequence),
            })
        } else {
            let tick = self
                .submission
                .hardware_time()
                .map_err(PumpError::DriverClock)?;
            if let Some(pacing) = &mut self.pacing {
                pacing.observed.observe(tick).map_err(PumpError::Clock)?;
            }
            Ok(ClockEvidence {
                bounds: ClockBounds {
                    earliest: tick,
                    latest: tick,
                },
                source: WaitClockSource::FreshHal,
                model_sequence: None,
            })
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_duration(
        &mut self,
        until_tick: u64,
        maximum: std::time::Duration,
    ) -> Result<std::time::Duration, PumpError> {
        Ok(self.wait_schedule(until_tick, u64::MAX, maximum)?.duration)
    }

    pub fn cycle(&mut self) -> Result<PumpProgress, PumpError> {
        let started = self.timings.as_ref().map(|_| Instant::now());
        let result = self.cycle_inner().and_then(|progress| {
            if !matches!(
                progress,
                PumpProgress::Waiting { .. } | PumpProgress::Partial { .. }
            ) {
                self.poll_health()?;
            }
            Ok(progress)
        });
        if let (Some(timings), Some(started)) = (&mut self.timings, started) {
            timings.cycle.record(started.elapsed());
        }
        if result.is_err() {
            self.progress.fail();
            if !matches!(result, Err(PumpError::Stopped)) {
                self.failed = true;
                self.stop.request_stop();
            }
        }
        result
    }

    fn poll_health(&mut self) -> Result<(), PumpError> {
        let Some(pacing) = &self.pacing else {
            return Ok(());
        };
        if !self
            .health
            .as_ref()
            .is_some_and(|monitor| monitor.due(pacing.observed.bounds().latest))
        {
            return Ok(());
        }
        // A zero-timeout UHD health read can still sleep, so collect only after pending writes.
        let bounds = self.clock_evidence()?.bounds;
        let started = self.timings.as_ref().map(|_| Instant::now());
        let health = self.submission.health();
        if let (Some(timings), Some(started)) = (&mut self.timings, started) {
            timings.health_query.record(started.elapsed());
        }
        let health = health.map_err(PumpError::DriverHealth)?.into();
        if let Some(timings) = &mut self.timings {
            timings.health_changes.observe(bounds, health);
        }
        if let Err(error) = self
            .health
            .as_mut()
            .expect("due health monitor")
            .observe(bounds.latest, health)
        {
            if matches!(error, TxHealthFailure::Budget { .. }) {
                self.recovery.health_warnings = self.recovery.health_warnings.saturating_add(1);
            } else {
                return Err(PumpError::Health(error));
            }
        }
        Ok(())
    }

    fn cycle_inner(&mut self) -> Result<PumpProgress, PumpError> {
        self.recording.poll();
        if self.failed {
            return Err(PumpError::Poisoned);
        }
        if self.stop.is_stopping() {
            return Err(PumpError::Stopped);
        }
        if !self.submission.is_active() {
            return Err(PumpError::NotActive);
        }
        if self.pending.is_none() {
            self.pending = self.ready.try_recv();
            if let (Some(timings), Some(slot)) = (&mut self.timings, &mut self.pending) {
                let now = Instant::now();
                if let Some(queued) = slot.queued_at {
                    timings.ready_queue.record(now.duration_since(queued));
                }
                slot.dequeued_at = Some(now);
                slot.publication_dequeued_at = Some(now);
            }
        }
        let observed = if let Some(pacing) = &mut self.pacing {
            let bounds = match &mut self.sampled_clock {
                Some(clock) => {
                    let bounds = clock.read().map_err(PumpError::ClockModel)?;
                    pacing
                        .observed
                        .observe_bounds(bounds)
                        .map_err(PumpError::Clock)?;
                    bounds
                }
                None => {
                    let started = self.timings.as_ref().map(|_| Instant::now());
                    let tick = self.submission.hardware_time();
                    if let (Some(timings), Some(started)) = (&mut self.timings, started) {
                        timings.clock_query.record(started.elapsed());
                    }
                    let tick = tick.map_err(PumpError::DriverClock)?;
                    pacing.observed.observe(tick).map_err(PumpError::Clock)?;
                    umts_radio::clock_model::ClockBounds {
                        earliest: tick,
                        latest: tick,
                    }
                }
            };
            Some((bounds, pacing.budget))
        } else {
            None
        };
        let Some(slot) = &mut self.pending else {
            return Ok(PumpProgress::Idle);
        };
        let first = SampleInstant {
            index: slot.window.first_sample.index + slot.accepted as u64,
            ..slot.window.first_sample
        };
        if let (Some(timings), Some(enqueued), Some(dequeued)) = (
            &mut self.timings,
            slot.queued_at,
            slot.publication_dequeued_at,
        ) && let Ok(first_tick) = self.submission.tick_at(first)
        {
            timings.last_publication = Some(PublicationEvidence {
                first_sample: first,
                first_tick,
                deadline_tick: observed
                    .and_then(|(_, budget)| first_tick.checked_sub(budget.minimum_ticks())),
                enqueue_started_to_dequeue_ns: dequeued.duration_since(enqueued).as_nanos(),
                enqueue_started_from_wait_start: None,
                dequeued_from_wait_wake: None,
            });
        }
        if let Some(failure) = slot.late {
            return self.drop_pending(DropCause::Preparation, Some(failure));
        }
        if let Some((now, budget)) = observed {
            let tick = self.submission.tick_at(first).map_err(PumpError::Time)?;
            let end = self
                .submission
                .tick_at(slot.window.next_sample)
                .map_err(PumpError::Time)?
                .checked_add(1)
                .ok_or(PumpError::Time(TimeError::Overflow))?;
            match budget
                .submission_bounds(now, tick, end)
                .map_err(PumpError::Lead)?
            {
                Gate::Ready => {}
                Gate::WaitUntil(until_tick) => {
                    return Ok(PumpProgress::Waiting {
                        until_tick,
                        deadline_tick: tick
                            .checked_sub(budget.minimum_ticks())
                            .ok_or(PumpError::Time(TimeError::Overflow))?,
                    });
                }
                Gate::Missed { shortfall_ticks } => {
                    if let (Some(timings), Some(started)) =
                        (&mut self.timings, slot.dequeued_at.take())
                    {
                        timings.pending.record(started.elapsed());
                    }
                    return self.drop_pending(
                        DropCause::Submission,
                        Some(DeadlineFailure {
                            device_tick: now.latest,
                            first_tick: tick,
                            shortfall_ticks,
                        }),
                    );
                }
            }
        }
        if let (Some(timings), Some(started)) = (&mut self.timings, slot.dequeued_at.take()) {
            timings.pending.record(started.elapsed());
        }
        slot.attempts += 1;
        let end = SampleInstant {
            epoch: first.epoch,
            index: first
                .index
                .checked_add((slot.samples.len() - slot.accepted) as u64)
                .ok_or(PumpError::Time(TimeError::Overflow))?,
        };
        let tick = self
            .submission
            .tick_at(end)
            .map_err(PumpError::Time)?
            .checked_add(1)
            .ok_or(PumpError::Time(TimeError::Overflow))?;
        if self
            .terminal_tail
            .is_none_or(|previous| tick > previous.tick())
        {
            self.terminal_tail = Some(TransmitTail::new(end, tick));
        }
        if let Some((bounds, _)) = observed {
            self.terminal_clock = Some(
                self.terminal_clock
                    .map_or(bounds.earliest, |previous| previous.max(bounds.earliest)),
            );
        }
        let cpu_started = self
            .timings
            .as_ref()
            .and_then(|_| timing::thread_cpu_time());
        let started = self.timings.as_ref().map(|_| Instant::now());
        let submitted = self
            .submission
            .submit(first, &slot.samples[slot.accepted..]);
        let elapsed = started.map(|started| started.elapsed());
        let cpu_elapsed =
            cpu_started.and_then(|started| timing::thread_cpu_time()?.checked_sub(started));
        if let (Some(timings), Some(elapsed)) = (&mut self.timings, elapsed) {
            timings.write.record(elapsed);
            if elapsed >= timing::SLOW_WRITE_THRESHOLD {
                timings.slow_writes.record(timing::SlowWrite {
                    first,
                    first_tick: self.submission.tick_at(first).map_err(PumpError::Time)?,
                    requested_samples: slot.samples.len() - slot.accepted,
                    accepted_samples: match &submitted {
                        Ok(count) => Some(*count),
                        Err(failure) => failure.accepted_samples,
                    },
                    failed: submitted.is_err(),
                    gate_bounds: observed.map(|(bounds, _)| bounds),
                    model_sequence: self
                        .sampled_clock
                        .as_ref()
                        .map(|clock| clock.accepted_sequence()),
                    wall_ns: elapsed.as_nanos(),
                    thread_cpu_ns: cpu_elapsed.map(|elapsed| elapsed.as_nanos()),
                });
            }
        }
        if self.recording.enabled() {
            self.recording
                .record_transmit(
                    first.index,
                    self.submission.tick_at(first),
                    &slot.samples[slot.accepted..],
                    match &submitted {
                        Ok(count) => Some(*count),
                        Err(failure) => failure.accepted_samples,
                    },
                    submitted.is_err(),
                )
                .map_err(PumpError::Time)?;
        }
        match submitted {
            Ok(accepted) => {
                slot.accepted += accepted;
                if slot.accepted != slot.samples.len() {
                    self.failed = true;
                    self.stop.request_stop();
                    return Err(PumpError::PoolInvariant);
                }
                self.finish_pending()
            }
            Err(failure) => {
                if let Some(accepted) = failure.accepted_samples {
                    if accepted > slot.samples.len() - slot.accepted {
                        self.failed = true;
                        self.stop.request_stop();
                        return Err(PumpError::PoolInvariant);
                    }
                    slot.accepted += accepted;
                }
                let recoverable_driver = matches!(
                    failure.kind,
                    SubmitFailureKind::Driver(
                        sdr_hal::error::ErrorKind::Late
                            | sdr_hal::error::ErrorKind::Timeout
                            | sdr_hal::error::ErrorKind::IncompleteWrite
                    )
                );
                if recoverable_driver && failure.accepted_samples.is_none() {
                    if !self.submission.discard_unknown(slot.window.next_sample) {
                        return Err(PumpError::PoolInvariant);
                    }
                    return self.drop_pending(DropCause::Driver, None);
                }
                if matches!(
                    failure.kind,
                    SubmitFailureKind::Driver(sdr_hal::error::ErrorKind::Late)
                ) {
                    return self.drop_pending(DropCause::Driver, None);
                }
                if !matches!(
                    failure.kind,
                    SubmitFailureKind::NoProgress
                        | SubmitFailureKind::CallBudget
                        | SubmitFailureKind::Driver(
                            sdr_hal::error::ErrorKind::IncompleteWrite
                                | sdr_hal::error::ErrorKind::Timeout
                        )
                ) || failure.accepted_samples.is_none()
                {
                    self.failed = true;
                    self.stop.request_stop();
                    return Err(PumpError::Submission(failure));
                }
                if slot.accepted == slot.samples.len() {
                    return self.finish_pending();
                }
                if slot.attempts >= self.maximum_attempts.get() {
                    return self.drop_pending(DropCause::Retries, None);
                }
                Ok(PumpProgress::Partial {
                    accepted: slot.accepted,
                    total: slot.samples.len(),
                })
            }
        }
    }

    fn finish_pending(&mut self) -> Result<PumpProgress, PumpError> {
        if self
            .pending
            .as_ref()
            .is_some_and(|slot| slot.contents == BatchContents::QualityMuted)
        {
            return self.drop_pending(DropCause::Amplitude, None);
        }
        let slot = self.pending.take().expect("submitted slot");
        let window = slot.window;
        if !self.progress.accepted(window) || self.free.try_send(slot.samples).is_err() {
            return Err(PumpError::PoolInvariant);
        }
        self.next_expected_sample = window.next_sample;
        if let Some(timings) = &mut self.timings {
            timings.submitted = timings.submitted.saturating_add(1);
        }
        Ok(PumpProgress::Submitted(window))
    }

    fn drop_pending(
        &mut self,
        cause: DropCause,
        deadline: Option<DeadlineFailure>,
    ) -> Result<PumpProgress, PumpError> {
        let slot = self.pending.take().ok_or(PumpError::PoolInvariant)?;
        let window = slot.window;
        // Radio acceptance of silence must never acknowledge the rejected bearer data.
        let accepted = if slot.contents == BatchContents::QualityMuted {
            self.recovery.quality_muted_samples = self
                .recovery
                .quality_muted_samples
                .saturating_add(slot.accepted as u64);
            if slot.accepted == slot.samples.len() {
                self.recovery.quality_muted_batches =
                    self.recovery.quality_muted_batches.saturating_add(1);
            }
            if cause != DropCause::Amplitude {
                self.recovery.amplitude_dropped = self.recovery.amplitude_dropped.saturating_add(1);
            }
            0
        } else {
            slot.accepted
        };
        let prefix_end = SampleInstant {
            index: window.first_sample.index + accepted as u64,
            ..window.first_sample
        };
        self.dropped_publication = self.timings.as_ref().and_then(|timings| {
            let evidence = timings.last_publication?;
            let failure = deadline?;
            (evidence.first_sample == prefix_end && evidence.first_tick == failure.first_tick)
                .then_some(DroppedPublication {
                    evidence,
                    enqueued: slot.queued_at?,
                    dequeued: slot.publication_dequeued_at?,
                })
        });
        if accepted != 0
            && !self.progress.accepted(WaveformWindow {
                next_sample: prefix_end,
                ..window
            })
        {
            return Err(PumpError::PoolInvariant);
        }
        if prefix_end != window.next_sample
            && !self.progress.lost(WaveformWindow {
                first_sample: prefix_end,
                ..window
            })
        {
            return Err(PumpError::PoolInvariant);
        }
        if self.free.try_send(slot.samples).is_err() {
            return Err(PumpError::PoolInvariant);
        }
        self.next_expected_sample = window.next_sample;
        let counter = match cause {
            DropCause::Amplitude => &mut self.recovery.amplitude_dropped,
            DropCause::Preparation => &mut self.recovery.preparation_late,
            DropCause::Submission => &mut self.recovery.submission_late,
            DropCause::Driver => &mut self.recovery.driver_dropped,
            DropCause::Retries => &mut self.recovery.retries_exhausted,
        };
        *counter = counter.saturating_add(1);
        self.recovery.dropped_batches = self.recovery.dropped_batches.saturating_add(1);
        self.recovery.dropped_samples = self
            .recovery
            .dropped_samples
            .saturating_add(window.next_sample.index - prefix_end.index);
        if prefix_end != window.next_sample
            && let Some(timings) = self.timings.as_mut()
        {
            timings.dropped_windows.record(timing::DroppedWindow {
                first: prefix_end,
                next: window.next_sample,
                cause,
            });
        }
        Ok(PumpProgress::Dropped {
            window,
            deadline,
            cause,
        })
    }

    pub fn stop(&mut self) -> Result<(), sdr_hal::error::Error> {
        self.progress.fail();
        self.stop.request_stop();
        self.submission.stop()?;
        self.terminal_stopped = true;
        Ok(())
    }
}

impl Drop for TxPreparation {
    fn drop(&mut self) {
        self.stop.request_stop();
        self.close_dedicated();
    }
}
impl Drop for TxPump {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Debug)]
pub enum RxError {
    Driver(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        sdr_hal::error::Error,
    ),
    Time(
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "Retains typed failure context for Debug diagnostics."
            )
        )]
        TimeError,
    ),
    Dsp(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        DspError,
    ),
    InvalidRead,
    NotActive,
    Poisoned,
    ClockModel(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        umts_radio::clock_model::ClockError,
    ),
    ReaderSpawn(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        std::io::Error,
    ),
    ReaderQueue,
    ReaderJoinPending,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxCounters {
    pub reads: u64,
    pub timeouts: u64,
    pub input_samples: u64,
    pub preroll_samples: u64,
    pub output_chips: u64,
    pub discontinuities: u64,
    pub reported_overflows: u64,
    pub missing_samples: u64,
    pub maximum_gap_samples: u64,
    pub last_discontinuity: Option<RxDiscontinuityEvidence>,
    pub overlaps: umts_radio::RxOverlapCounters,
    pub clock_queries: u64,
    pub clock_query_total_ns: u128,
    pub clock_query_maximum_ns: u128,
    pub reader: Option<ReaderCounters>,
    pub maximum_reader_queue_ns: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxDiscontinuityEvidence {
    pub expected_sample: SampleInstant,
    pub received_sample: SampleInstant,
    pub raw_tick: u64,
    pub driver: Option<umts_radio::Discontinuity>,
}

#[derive(Clone, Copy)]
pub struct FilteredRx<'a> {
    pub window: FilteredWindow,
    pub generation: u64,
    pub discontinuity: bool,
    pub chips: &'a [Complex32],
}

#[derive(Clone, Copy)]
pub struct MatchedSamples<'a> {
    pub window: FilteredWindow,
    pub generation: u64,
    pub discontinuity: bool,
    pub samples: &'a [Complex32],
}

pub struct MatchedRx<'a> {
    pub full: MatchedSamples<'a>,
    pub common: FilteredRx<'a>,
}

pub struct TimedRx {
    read_timing: Option<RxReadTiming>,
    clock_sampler: Option<sampled_clock::DeviceClockSampler>,
    clock_stats: Option<std::sync::Arc<sampled_clock::ClockQueryStats>>,
    clock_floor: Option<sampled_clock::ClockFloorWriter>,
    air_origin: AirClockOrigin,
    rx: Option<Box<dyn RadioRx>>,
    reader: Option<reader::BufferedRx>,
    reader_settings: Option<(StopToken, NonZeroUsize)>,
    reader_exit: Option<umts_runtime::worker::WorkerExit>,
    timeline: RxTimeline,
    cell_clock: SampleClock,
    receive_sample_anchor: Option<(SampleInstant, SampleInstant)>,
    origin_tick: u64,
    sample_rate: SampleRate,
    rate: SamplesPerChip,
    tick_rate: NonZeroU64,
    filter: ReceiveFilter,
    chip_phase: usize,
    raw: Vec<Complex32>,
    matched: Vec<Complex32>,
    chips: Vec<Complex32>,
    generation: u64,
    observed: bool,
    active: bool,
    failed: bool,
    timeout_us: i64,
    counters: RxCounters,
    recording: capture::RxRecording,
}

impl TimedRx {
    pub(crate) fn attach_live_capture(&mut self, recorder: capture::live::Recorder) {
        self.recording.attach_live(recorder);
    }
    pub(crate) fn read_timing(&self) -> Option<RxReadTiming> {
        self.read_timing
    }

    pub(crate) fn finish_capture(&mut self) {
        self.recording.finish()
    }

    pub fn air_origin(&self) -> AirClockOrigin {
        self.air_origin
    }
    pub fn chip_phase(&self) -> usize {
        self.chip_phase
    }
    pub fn epoch(&self) -> umts_types::RadioEpoch {
        self.filter.next_input().epoch
    }
    pub fn samples_per_chip(&self) -> usize {
        self.rate.get()
    }

    // The origin includes TX pulse delay and HAL calibration, not just a host buffer's first timestamp.
    pub fn new(
        rx: Box<dyn RadioRx>,
        air_origin: AirClockOrigin,
        settings: RxSettings,
    ) -> Result<Self, RxError> {
        let RxSettings {
            rate,
            span_chips,
            phase,
            maximum_samples,
            timeout_us,
        } = settings;
        let AirClockOrigin {
            chip: origin,
            chip_rate,
            tick: origin_tick,
            tick_rate: expected_tick_rate,
        } = air_origin;
        if maximum_samples.get() > CHIPS_PER_SLOT as usize * rate.get()
            || timeout_us <= 0
            || timeout_us > MAX_RX_TIMEOUT_US
        {
            return Err(RxError::InvalidRead);
        }
        let sample_rate = SampleRate::from_hz(rx.rx_sample_rate_hz()).map_err(RxError::Time)?;
        let expected_rate = chip_rate
            .multiplied_by(NonZeroU64::new(rate.get() as u64).expect("nonzero chip geometry"))
            .map_err(RxError::Time)?;
        if sample_rate.hz().round() != CHIPS_PER_SECOND as f64 * rate.get() as f64
            || sample_rate != expected_rate
        {
            return Err(RxError::Time(TimeError::InvalidRate));
        }
        let origin = SampleInstant {
            epoch: origin.epoch,
            index: origin
                .index
                .checked_mul(rate.get() as u64)
                .ok_or(RxError::Time(TimeError::Overflow))?,
        };
        let tick_rate =
            NonZeroU64::new(rx.tick_rate()).ok_or(RxError::Time(TimeError::InvalidRate))?;
        if tick_rate != expected_tick_rate {
            return Err(RxError::Time(TimeError::InvalidRate));
        }
        let clock = SampleClock::new(origin, origin_tick, sample_rate, tick_rate);
        let timeline = RxTimeline::from_hal(origin.epoch, &*rx).map_err(RxError::Time)?;
        if phase >= rate.get() {
            return Err(RxError::Dsp(DspError::InvalidFilter));
        }
        let filter = ReceiveFilter::full_rate(rate, span_chips, origin).map_err(RxError::Dsp)?;
        Ok(Self {
            clock_sampler: None,
            clock_stats: None,
            clock_floor: None,
            rx: Some(rx),
            reader: None,
            reader_settings: None,
            reader_exit: None,
            air_origin,
            timeline,
            cell_clock: clock,
            receive_sample_anchor: None,
            origin_tick,
            sample_rate,
            rate,
            tick_rate,
            filter,
            chip_phase: phase,
            raw: vec![Complex32::default(); maximum_samples.get()],
            matched: vec![Complex32::default(); maximum_samples.get()],
            chips: vec![Complex32::default(); maximum_samples.get().div_ceil(rate.get())],
            generation: 0,
            observed: false,
            active: false,
            failed: false,
            timeout_us,
            counters: RxCounters::default(),
            recording: capture::RxRecording::default(),
            read_timing: None,
        })
    }

    pub fn start(&mut self, tick: Option<u64>) -> Result<(), RxError> {
        if self.active || self.failed {
            return Err(RxError::Poisoned);
        }
        let prepared_reader = self
            .reader_settings
            .as_ref()
            .map(|(_, capacity)| reader::Prepared::new(self.raw.len(), *capacity));
        self.rx
            .as_mut()
            .ok_or(RxError::Poisoned)?
            .rx_activate(tick)
            .map_err(RxError::Driver)?;
        self.active = true;
        if let Some((stop, _)) = &self.reader_settings {
            let owner = reader::DeviceOwner {
                rx: self.rx.take().expect("inactive HAL owner"),
                sampler: self.clock_sampler.take(),
                counters: self.counters,
                reader_counters: ReaderCounters::default(),
            };
            match prepared_reader.expect("configured reader buffers").spawn(
                owner,
                self.timeout_us,
                stop.clone(),
            ) {
                Ok(reader) => self.reader = Some(reader),
                Err(failure) => {
                    self.rx = Some(failure.owner.rx);
                    self.clock_sampler = failure.owner.sampler;
                    return Err(RxError::ReaderSpawn(failure.error));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn enable_reader(
        &mut self,
        stop: StopToken,
        capacity: NonZeroUsize,
    ) -> Result<(), RxError> {
        if self.active
            || self.failed
            || self.reader_settings.is_some()
            || capacity.get() > reader::MAX_QUEUE_BLOCKS
        {
            return Err(RxError::InvalidRead);
        }
        self.reader_settings = Some((stop, capacity));
        Ok(())
    }

    pub(crate) fn reader_exit(&self) -> Option<&umts_runtime::worker::WorkerExit> {
        self.reader_exit.as_ref()
    }

    fn retain_reader_counters(&mut self, counters: RxCounters, reader: ReaderCounters) {
        self.counters.clock_queries = counters.clock_queries;
        self.counters.clock_query_total_ns = counters.clock_query_total_ns;
        self.counters.clock_query_maximum_ns = counters.clock_query_maximum_ns;
        self.counters.reader = Some(reader);
    }

    fn join_reader(&mut self) -> Result<(), RxError> {
        if let Some(reader) = &mut self.reader {
            let (owner, exit) = reader.finish()?;
            self.retain_reader_counters(owner.counters, owner.reader_counters);
            self.rx = Some(owner.rx);
            self.clock_sampler = owner.sampler;
            self.reader_exit = Some(exit);
            self.reader = None;
        }
        Ok(())
    }

    pub fn counters(&self) -> RxCounters {
        let mut counters = self.counters;
        if let Some(stats) = &self.clock_stats {
            stats.apply(&mut counters);
        }
        counters
    }

    #[cfg(test)]
    pub fn read_filtered(&mut self) -> Result<Option<FilteredRx<'_>>, RxError> {
        self.read_matched()
            .map(|batch| batch.map(|batch| batch.common))
    }

    pub fn read_matched(&mut self) -> Result<Option<MatchedRx<'_>>, RxError> {
        self.recording.poll();
        if self.failed {
            return Err(RxError::Poisoned);
        }
        if !self.active {
            return Err(RxError::NotActive);
        }
        let read_started = self.read_timing.as_mut().map(|timing| {
            *timing = RxReadTiming::default();
            Instant::now()
        });
        let (result, returned_at) = if let Some(reader) = &mut self.reader {
            let Some(read) = reader.read(&mut self.raw)? else {
                return Ok(None);
            };
            self.counters.maximum_reader_queue_ns = self
                .counters
                .maximum_reader_queue_ns
                .max(read.returned_at.elapsed().as_nanos());
            self.retain_reader_counters(read.counters, read.reader_counters);
            (read.result, read.returned_at)
        } else {
            let result = match self
                .rx
                .as_mut()
                .ok_or(RxError::Poisoned)?
                .rx_read(&mut self.raw, self.timeout_us)
            {
                Ok(result) => result,
                Err(error) if error.kind() == sdr_hal::error::ErrorKind::Timeout => {
                    self.counters.timeouts = self.counters.timeouts.saturating_add(1);
                    return Ok(None);
                }
                Err(error) => {
                    self.failed = true;
                    return Err(RxError::Driver(error));
                }
            };
            (result, Instant::now())
        };
        self.counters.reads = self.counters.reads.saturating_add(1);
        if let Some(started) = read_started {
            self.read_timing
                .as_mut()
                .expect("enabled timing")
                .radio_read_ns = started.elapsed().as_nanos();
        }
        let coordinate_started = read_started.map(|_| Instant::now());
        if result.samples_read > self.raw.len() {
            self.failed = true;
            return Err(RxError::InvalidRead);
        }
        self.counters.input_samples = self
            .counters
            .input_samples
            .saturating_add(result.samples_read as u64);
        let observation = self.timeline.observe(result);
        self.counters.overlaps = self.timeline.overlaps();
        let observation = match observation {
            Ok(Some(observation)) => observation,
            Ok(None) => return Ok(None),
            Err(error) => {
                self.failed = true;
                return Err(RxError::Time(error));
            }
        };
        let sample_rate = self.sample_rate;
        let tick_rate = self.tick_rate;
        let tick_base = observation.tick;
        let tick_at = |offset: usize| {
            sample_rate
                .samples_to_ticks(offset as u64, tick_rate)
                .and_then(|delta| tick_base.checked_add(delta).ok_or(TimeError::Overflow))
        };
        if let Some(floor) = &mut self.clock_floor
            && let Some(last) = observation.samples.checked_sub(1)
            && let Ok(tick) = tick_at(last)
        {
            floor.raise(returned_at, tick);
        }
        let mut skip = observation.discarded_prefix;
        if observation.tick < self.origin_tick {
            let mut end = observation.samples;
            while skip < end {
                let middle = skip + (end - skip) / 2;
                let tick = tick_at(middle).map_err(|error| {
                    self.failed = true;
                    RxError::Time(error)
                })?;
                if tick < self.origin_tick {
                    skip = middle + 1;
                } else {
                    end = middle;
                }
            }
            self.counters.preroll_samples = self
                .counters
                .preroll_samples
                .saturating_add((skip - observation.discarded_prefix) as u64);
            if skip == observation.samples {
                return Ok(None);
            }
        }
        let first_tick = tick_at(skip).map_err(|error| {
            self.failed = true;
            RxError::Time(error)
        })?;
        let retained = observation.first.index.checked_add(skip as u64);
        // Quantized timestamps must not round one validated RX sample grid against two origins.
        let mapped = match (self.receive_sample_anchor, retained) {
            (Some((receive, cell)), Some(retained)) => retained
                .checked_sub(receive.index)
                .and_then(|delta| cell.index.checked_add(delta))
                .map(|index| SampleInstant { index, ..cell })
                .ok_or(TimeError::Overflow),
            (None, Some(retained)) => self.cell_clock.sample_at_tick(first_tick).inspect(|&cell| {
                self.receive_sample_anchor = Some((
                    SampleInstant {
                        index: retained,
                        ..observation.first
                    },
                    cell,
                ));
            }),
            (_, None) => Err(TimeError::Overflow),
        };
        let first = match mapped {
            Ok(first) => first,
            Err(error) => {
                self.failed = true;
                return Err(RxError::Time(error));
            }
        };
        let discontinuity = observation.discontinuity.is_some()
            || (self.observed && first != self.filter.next_input());
        if discontinuity {
            let expected_sample = self.filter.next_input();
            let gap = if self.observed {
                first.index.saturating_sub(expected_sample.index)
            } else {
                0
            };
            self.counters.reported_overflows =
                self.counters.reported_overflows.saturating_add(u64::from(
                    observation
                        .discontinuity
                        .is_some_and(|event| event.reported_overflow),
                ));
            self.counters.missing_samples = self.counters.missing_samples.saturating_add(gap);
            self.counters.maximum_gap_samples = self.counters.maximum_gap_samples.max(gap);
            self.counters.last_discontinuity = Some(RxDiscontinuityEvidence {
                expected_sample,
                received_sample: first,
                raw_tick: first_tick,
                driver: observation.discontinuity,
            });
        }
        if !self.observed || discontinuity {
            self.filter.reset(first);
            self.generation = self.generation.checked_add(1).ok_or_else(|| {
                self.failed = true;
                RxError::InvalidRead
            })?;
        }
        if discontinuity {
            self.counters.discontinuities = self.counters.discontinuities.saturating_add(1);
        }
        self.observed = true;
        self.recording.record_receive(
            first.index,
            first_tick,
            returned_at,
            &self.raw[skip..observation.samples],
            discontinuity,
        );
        if let Some(started) = coordinate_started {
            self.read_timing
                .as_mut()
                .expect("enabled timing")
                .coordinate_capture_ns = started.elapsed().as_nanos();
        }
        let filter_started = read_started.map(|_| Instant::now());
        let count = self
            .filter
            .output_length(observation.samples - skip)
            .map_err(|error| {
                self.failed = true;
                RxError::Dsp(error)
            })?;
        let window = self
            .filter
            .process(
                first,
                &self.raw[skip..observation.samples],
                &mut self.matched[..count],
            )
            .map_err(|error| {
                self.failed = true;
                RxError::Dsp(error)
            })?;
        if let Some(started) = filter_started {
            self.read_timing.as_mut().expect("enabled timing").filter_ns =
                started.elapsed().as_nanos();
        }
        let decimation_started = read_started.map(|_| Instant::now());
        let rate = self.samples_per_chip();
        let delay = self.filter.group_delay_samples() as u64;
        let offset = window.first_center.map_or(0, |first| {
            ((self.chip_phase as u64 + rate as u64 - (first.index + delay) % rate as u64)
                % rate as u64) as usize
        });
        let common_count = count.saturating_sub(offset).div_ceil(rate);
        if common_count != 0 {
            for (output, input) in self.chips[..common_count]
                .iter_mut()
                .zip(self.matched[offset..count].iter().step_by(rate))
            {
                *output = *input;
            }
        }
        let common_window = FilteredWindow {
            first_center: (common_count != 0).then(|| {
                let first = window.first_center.expect("nonempty matched window");
                SampleInstant {
                    index: first.index + offset as u64,
                    ..first
                }
            }),
            sample_stride: rate,
            next_input: window.next_input,
        };
        self.counters.output_chips = self
            .counters
            .output_chips
            .saturating_add(common_count as u64);
        if let Some(started) = decimation_started {
            self.read_timing
                .as_mut()
                .expect("enabled timing")
                .decimation_ns = started.elapsed().as_nanos();
        }
        Ok(Some(MatchedRx {
            full: MatchedSamples {
                window,
                generation: self.generation,
                discontinuity,
                samples: &self.matched[..count],
            },
            common: FilteredRx {
                window: common_window,
                generation: self.generation,
                discontinuity,
                chips: &self.chips[..common_count],
            },
        }))
    }

    pub fn stop(&mut self) -> Result<(), RxError> {
        self.join_reader()?;
        if self.active {
            self.rx
                .as_mut()
                .ok_or(RxError::Poisoned)?
                .stop()
                .map_err(RxError::Driver)?;
            self.active = false;
        }
        Ok(())
    }

    pub(crate) fn stop_for_retirement(&mut self) -> Result<(), RxError> {
        self.join_reader()?;
        self.rx
            .as_mut()
            .ok_or(RxError::Poisoned)?
            .stop()
            .map_err(RxError::Driver)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TimedRx {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests;
