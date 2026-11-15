// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    access::{AccessObservation, AccessScheduleError, AccessTiming, AccessTxTarget},
    access_message::{
        AccessMessages, MessageDelivery, MessageError, MessageLimits, MessageOutcome,
    },
    common_tx::{AcquisitionPort, CommonTxError},
    radio_io::{FilteredRx, RxError, TimedRx},
};
use num_complex::Complex32;
use std::{num::NonZeroUsize, sync::Arc};
use umts_dsp::{
    DspError,
    prach::{PREAMBLE_CHIPS, PrachCode, PreambleSignature},
    prach_search::{PreambleSearchPlan, PreambleTimingSearch},
    pulse_shaping::SamplesPerChip,
};
use umts_types::{
    RadioEpoch,
    time::{CHIPS_PER_SLOT, ChipInstant},
};

mod ingress;
mod timing;
mod worker;
pub use ingress::{IngressError, RachIngress, RachQueueLimits};
pub use timing::{RxCycleTiming, RxTimingSummary};
pub use worker::{RxStatistics, RxWorker, RxWorkerError, RxWorkerReport};

const MAX_PRACH_CHANNELS: usize = 16;
const MAX_PLAN_EVENTS: usize = crate::access::MAX_ACCESS_RESPONSES;
const DEDICATED_SERVICE_ROUNDS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessError {
    Configuration,
    Capacity,
    Epoch,
    Phase,
    Time,
    Poisoned,
    Schedule(AccessScheduleError),
    Dsp(DspError),
    Message(MessageError),
    Ingress(IngressError),
}

#[derive(Clone, Copy, Debug)]
pub struct AccessRxSettings {
    pub maximum_channels: NonZeroUsize,
    pub messages: MessageLimits,
    pub minimum_peak_to_background_ratio: f64,
    pub minimum_tx_lead_chips: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessCounters {
    pub searched: u64,
    pub candidates: u64,
    pub admitted: u64,
    pub late: u64,
    pub capacity: u64,
    pub discontinuities: u64,
    pub decoded: u64,
    pub failed_crc: u64,
    pub expired: u64,
    pub transmit_rejected: u64,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct AccessSearchPeak {
    pub code: PrachCode,
    pub signature: PreambleSignature,
    pub generation: u64,
    pub nominal_start: ChipInstant,
    pub correlation_start: ChipInstant,
    pub coherent_fraction: f64,
    pub peak_to_background_ratio: f64,
    /// Mean complex power of filtered chips, not raw ADC power or pilot SIR.
    pub mean_input_power: f64,
}

#[derive(Clone, Copy, Debug, Default)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct AccessSearchEvidence {
    pub minimum_peak_to_background_ratio: f64,
    pub signature_peaks: u64,
    pub last: Option<AccessSearchPeak>,
    pub strongest_coherence: Option<AccessSearchPeak>,
    /// Power is compared only at each signature's correlation-maximizing delay.
    pub strongest_peak_input_power: Option<AccessSearchPeak>,
}

impl AccessSearchEvidence {
    fn observe(&mut self, peak: AccessSearchPeak) {
        self.signature_peaks = self.signature_peaks.saturating_add(1);
        self.last = Some(peak);
        if self
            .strongest_coherence
            .is_none_or(|previous| peak.coherent_fraction > previous.coherent_fraction)
        {
            self.strongest_coherence = Some(peak);
        }
        if self
            .strongest_peak_input_power
            .is_none_or(|previous| peak.mean_input_power > previous.mean_input_power)
        {
            self.strongest_peak_input_power = Some(peak);
        }
    }
}

struct History {
    chips: Vec<Complex32>,
    capacity: usize,
    first: u64,
    end: u64,
}
impl History {
    fn new(capacity: usize) -> Self {
        Self {
            chips: vec![Complex32::default(); capacity * 2],
            capacity,
            first: 0,
            end: 0,
        }
    }
    fn reset(&mut self, first: u64) {
        self.first = first;
        self.end = first;
    }
    fn append(&mut self, first: u64, chips: &[Complex32]) -> Result<(), AccessError> {
        if first != self.end || chips.len() > self.capacity {
            return Err(AccessError::Time);
        }
        let end = first
            .checked_add(chips.len() as u64)
            .ok_or(AccessError::Time)?;
        let offset = (first % self.capacity as u64) as usize;
        let prefix = chips.len().min(self.capacity - offset);
        self.chips[offset..offset + prefix].copy_from_slice(&chips[..prefix]);
        self.chips[offset + self.capacity..offset + self.capacity + prefix]
            .copy_from_slice(&chips[..prefix]);
        let tail = chips.len() - prefix;
        self.chips[..tail].copy_from_slice(&chips[prefix..]);
        self.chips[self.capacity..self.capacity + tail].copy_from_slice(&chips[prefix..]);
        self.end = end;
        self.first = self.first.max(end.saturating_sub(self.capacity as u64));
        Ok(())
    }
    fn window(&self, first: u64, length: usize) -> Result<&[Complex32], AccessError> {
        if first < self.first
            || first
                .checked_add(length as u64)
                .is_none_or(|end| end > self.end)
            || length > self.capacity
        {
            return Err(AccessError::Time);
        }
        let offset = (first % self.capacity as u64) as usize;
        Ok(&self.chips[offset..offset + length])
    }
}

struct Channel {
    timing: AccessTiming,
    search: PreambleTimingSearch,
    length: usize,
    messages: AccessMessages,
    next: Option<ChipInstant>,
}

pub struct AccessReceiver {
    epoch: RadioEpoch,
    rate: usize,
    settings: AccessRxSettings,
    history: History,
    channels: Vec<Channel>,
    generation: Option<u64>,
    next: Option<u64>,
    decode_cursor: usize,
    failed: bool,
    counters: AccessCounters,
    search_evidence: AccessSearchEvidence,
}

impl AccessReceiver {
    pub fn new(
        source: &dyn AccessTxTarget,
        rate: SamplesPerChip,
        settings: AccessRxSettings,
    ) -> Result<Self, AccessError> {
        let count = source.information().sib5().prach().len();
        if count == 0
            || count > settings.maximum_channels.get()
            || count > MAX_PRACH_CHANNELS
            || !settings.minimum_peak_to_background_ratio.is_finite()
            || settings.minimum_peak_to_background_ratio <= 1.0
        {
            return Err(AccessError::Configuration);
        }
        let mut channels = Vec::with_capacity(count);
        let mut maximum_window = 0;
        for index in 0..count {
            let timing = source
                .access_timing(index)
                .ok_or(AccessError::Configuration)?;
            let delays = usize::try_from(timing.maximum_arrival_delay())
                .map_err(|_| AccessError::Capacity)?
                .checked_add(1)
                .ok_or(AccessError::Capacity)?;
            if (PREAMBLE_CHIPS as u64)
                .checked_add(timing.maximum_arrival_delay())
                .and_then(|end| end.checked_add(settings.minimum_tx_lead_chips))
                .is_none_or(|end| end > timing.indicator_delay())
            {
                return Err(AccessError::Configuration);
            }
            let plan = Arc::new(
                PreambleSearchPlan::new(
                    timing.code(),
                    timing.signature_mask(),
                    0..PREAMBLE_CHIPS,
                    delays,
                )
                .map_err(AccessError::Dsp)?,
            );
            let length = plan.input_length();
            maximum_window = maximum_window.max(length);
            let mut limits = settings.messages;
            // The retained message budget is cell-wide, not multiplied by the configured PRACH count.
            limits.maximum_buffer_bytes /= count;
            let messages =
                AccessMessages::new(source, index, rate, limits).map_err(AccessError::Message)?;
            channels.push(Channel {
                timing,
                search: PreambleTimingSearch::new(plan),
                length,
                messages,
                next: None,
            });
        }
        Ok(Self {
            epoch: source.next_chip().epoch,
            rate: rate.get(),
            settings,
            history: History::new(maximum_window + CHIPS_PER_SLOT as usize),
            channels,
            generation: None,
            next: None,
            decode_cursor: 0,
            failed: false,
            counters: AccessCounters::default(),
            search_evidence: AccessSearchEvidence {
                minimum_peak_to_background_ratio: settings.minimum_peak_to_background_ratio,
                ..AccessSearchEvidence::default()
            },
        })
    }

    pub fn counters(&self) -> AccessCounters {
        self.counters
    }

    pub fn search_evidence(&self) -> AccessSearchEvidence {
        self.search_evidence
    }

    fn reconcile_transmit(&mut self) {
        let rejected: usize = self
            .channels
            .iter_mut()
            .map(|channel| channel.messages.reconcile_transmit())
            .sum();
        self.counters.transmit_rejected = self
            .counters
            .transmit_rejected
            .saturating_add(rejected as u64);
    }
    pub fn maximum_payload_bits(&self) -> usize {
        self.channels
            .iter()
            .map(|channel| channel.messages.maximum_payload_bits())
            .max()
            .expect("configured PRACH")
    }

    fn reset(&mut self) {
        self.next = None;
        self.generation = None;
        for channel in &mut self.channels {
            channel.next = None;
            channel.messages.reset_receive();
        }
    }

    pub fn feed(
        &mut self,
        source: &mut dyn AccessTxTarget,
        input: FilteredRx<'_>,
    ) -> Result<(), AccessError> {
        if self.failed {
            return Err(AccessError::Poisoned);
        }
        let result = self.feed_inner(source, input);
        if result.is_err() {
            self.failed = true;
            self.reset();
        }
        result
    }

    fn feed_inner(
        &mut self,
        source: &mut dyn AccessTxTarget,
        input: FilteredRx<'_>,
    ) -> Result<(), AccessError> {
        if input.window.next_input.epoch != self.epoch || source.next_chip().epoch != self.epoch {
            return Err(AccessError::Epoch);
        }
        if input.window.sample_stride != self.rate {
            return Err(AccessError::Phase);
        }
        if input.chips.len() > CHIPS_PER_SLOT as usize {
            return Err(AccessError::Capacity);
        }
        if input
            .chips
            .iter()
            .any(|chip| !chip.re.is_finite() || !chip.im.is_finite())
        {
            return Err(AccessError::Dsp(DspError::NonFiniteEvidence));
        }
        let first = match input.window.first_center {
            Some(first) => {
                if first.epoch != self.epoch {
                    return Err(AccessError::Epoch);
                }
                if !first.index.is_multiple_of(self.rate as u64) {
                    return Err(AccessError::Phase);
                }
                first.index / self.rate as u64
            }
            None if input.chips.is_empty() => {
                if input.discontinuity
                    || self
                        .generation
                        .is_some_and(|generation| generation != input.generation)
                {
                    self.reset();
                    self.counters.discontinuities = self.counters.discontinuities.saturating_add(1);
                }
                return Ok(());
            }
            None => return Err(AccessError::Time),
        };
        let end = first
            .checked_add(input.chips.len() as u64)
            .ok_or(AccessError::Time)?;
        if end
            .checked_mul(self.rate as u64)
            .is_none_or(|end| end > input.window.next_input.index)
        {
            return Err(AccessError::Time);
        }
        let gap = self
            .generation
            .is_some_and(|generation| generation != input.generation || input.discontinuity)
            || self.next.is_some_and(|next| next != first);
        if gap {
            self.reset();
            self.counters.discontinuities = self.counters.discontinuities.saturating_add(1);
        }
        if self.next.is_none() {
            self.history.reset(first);
            for channel in &mut self.channels {
                channel.next = Some(
                    channel
                        .timing
                        .next_preamble(ChipInstant {
                            epoch: self.epoch,
                            index: first,
                        })
                        .map_err(AccessError::Schedule)?,
                );
            }
        }
        self.generation = Some(input.generation);
        self.next = Some(end);
        self.history.append(first, input.chips)?;
        let now = ChipInstant {
            epoch: self.epoch,
            index: input.window.next_input.index.div_ceil(self.rate as u64),
        };
        self.reconcile_transmit();
        for channel in &mut self.channels {
            channel.messages.feed(input).map_err(AccessError::Message)?;
            // Filtered chip progress, not the later raw-input clock, determines whether message samples were missed.
            let expired = channel
                .messages
                .expire_before(ChipInstant {
                    epoch: self.epoch,
                    index: end,
                })
                .map_err(AccessError::Message)?;
            self.counters.expired = self.counters.expired.saturating_add(expired as u64);
            let nominal = channel.next.expect("scheduled access occasion");
            if nominal
                .index
                .checked_add(channel.length as u64)
                .ok_or(AccessError::Time)?
                > end
            {
                continue;
            }
            let input = self.history.window(nominal.index, channel.length)?;
            let window = channel
                .search
                .search(nominal, input)
                .map_err(AccessError::Dsp)?;
            self.counters.searched = self.counters.searched.saturating_add(1);
            for number in 0..16 {
                if channel.timing.signature_mask() & (1 << number) == 0 {
                    continue;
                }
                let signature = PreambleSignature::new(number).map_err(AccessError::Dsp)?;
                let Some(detection) = window.strongest_peak(signature) else {
                    continue;
                };
                let peak = detection.correlation;
                self.search_evidence.observe(AccessSearchPeak {
                    code: peak.code,
                    signature,
                    generation: self.generation.expect("observed receive generation"),
                    nominal_start: nominal,
                    correlation_start: peak.correlation_start,
                    coherent_fraction: peak.coherent_fraction,
                    peak_to_background_ratio: detection.peak_to_background_ratio,
                    mean_input_power: peak.mean_input_power,
                });
                if detection.peak_to_background_ratio
                    < self.settings.minimum_peak_to_background_ratio
                {
                    continue;
                }
                self.counters.candidates = self.counters.candidates.saturating_add(1);
                let observation = AccessObservation {
                    code: peak.code,
                    signature,
                    nominal_start: nominal,
                    received_start: peak.correlation_start,
                };
                match channel.messages.admit(
                    source,
                    observation,
                    now,
                    self.settings.minimum_tx_lead_chips,
                ) {
                    Ok(_) => self.counters.admitted = self.counters.admitted.saturating_add(1),
                    Err(
                        MessageError::Capacity
                        | MessageError::Transmit(
                            CommonTxError::Capacity
                            | CommonTxError::Access(AccessScheduleError::Capacity),
                        ),
                    ) => self.counters.capacity = self.counters.capacity.saturating_add(1),
                    Err(MessageError::Transmit(
                        CommonTxError::Late | CommonTxError::Access(AccessScheduleError::Late),
                    )) => self.counters.late = self.counters.late.saturating_add(1),
                    Err(error) => return Err(AccessError::Message(error)),
                }
            }
            let first = ChipInstant {
                epoch: self.epoch,
                index: nominal.index.checked_add(1).ok_or(AccessError::Time)?,
            };
            channel.next = Some(
                channel
                    .timing
                    .next_preamble(first)
                    .map_err(AccessError::Schedule)?,
            );
        }
        Ok(())
    }

    fn decode_one(&mut self) -> Result<Option<MessageDelivery<'_>>, AccessError> {
        let count = self.channels.len();
        let selected = (0..count)
            .map(|offset| (self.decode_cursor + offset) % count)
            .find(|index| self.channels[*index].messages.ready());
        let Some(index) = selected else {
            return Ok(None);
        };
        self.decode_cursor = (index + 1) % count;
        match self.channels[index]
            .messages
            .decode_one()
            .map_err(AccessError::Message)?
        {
            Some(MessageOutcome::Decoded(delivery)) => {
                self.counters.decoded = self.counters.decoded.saturating_add(1);
                Ok(Some(delivery))
            }
            Some(MessageOutcome::Rejected(_)) => {
                self.counters.failed_crc = self.counters.failed_crc.saturating_add(1);
                Ok(None)
            }
            None => Ok(None),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeProgress {
    Idle,
    Backpressure,
    Published,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CycleProgress {
    pub received: bool,
    pub access: DecodeProgress,
    pub sampled_slots: u64,
    pub decode_state_updates: u64,
}

impl CycleProgress {
    pub fn is_idle(self) -> bool {
        !self.received
            && self.access != DecodeProgress::Published
            && self.sampled_slots == 0
            && self.decode_state_updates == 0
    }
}

#[derive(Debug)]
pub enum CommonRxError {
    Access(AccessError),
    Radio(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        RxError,
    ),
    Closed,
    Dedicated(
        #[expect(
            dead_code,
            reason = "Retains typed failure context for Debug diagnostics."
        )]
        crate::dedicated_rx::ReceiveError,
    ),
}

pub struct CommonRx {
    receiver: TimedRx,
    access: AccessReceiver,
    port: AcquisitionPort,
    publisher: ingress::RachPublisher,
    published: u64,
    dedicated: Option<crate::dedicated_rx::DedicatedSampleRuntime>,
    started: bool,
    timings: Option<timing::RxTimings>,
}

pub fn prepare_common_rx(
    receiver: TimedRx,
    port: AcquisitionPort,
    rate: SamplesPerChip,
    settings: AccessRxSettings,
    queue: RachQueueLimits,
) -> Result<(CommonRx, RachIngress), AccessError> {
    if receiver.epoch() != port.next_chip().epoch {
        return Err(AccessError::Epoch);
    }
    if receiver.samples_per_chip() != rate.get() || receiver.chip_phase() != 0 {
        return Err(AccessError::Phase);
    }
    let access = AccessReceiver::new(&port, rate, settings)?;
    let (publisher, ingress) =
        ingress::channel(receiver.epoch(), queue).map_err(AccessError::Ingress)?;
    if access.maximum_payload_bits() > publisher.maximum_bits() {
        return Err(AccessError::Capacity);
    }
    Ok((
        CommonRx {
            receiver,
            access,
            port,
            publisher,
            published: 0,
            dedicated: None,
            started: false,
            timings: None,
        },
        ingress,
    ))
}

impl CommonRx {
    pub(crate) fn dedicated_paired_with(
        &self,
        port: &crate::dedicated_rx::DedicatedReceivePort,
    ) -> bool {
        self.dedicated
            .as_ref()
            .is_some_and(|runtime| port.paired_with(runtime))
    }
    pub fn configure_dedicated(
        &mut self,
        limits: crate::dedicated_rx::SampleRuntimeLimits,
    ) -> Result<crate::dedicated_rx::DedicatedReceivePort, crate::dedicated_rx::ReceiveError> {
        use crate::dedicated_rx::{DedicatedSampleRuntime, ReceiveError};
        if self.started || self.dedicated.is_some() {
            return Err(ReceiveError::Occupied);
        }
        let rate = match self.receiver.samples_per_chip() {
            2 => SamplesPerChip::Two,
            4 => SamplesPerChip::Four,
            8 => SamplesPerChip::Eight,
            _ => return Err(ReceiveError::Configuration),
        };
        let (runtime, port) = DedicatedSampleRuntime::new(self.receiver.epoch(), rate, limits)?;
        self.dedicated = Some(runtime);
        Ok(port)
    }
    pub fn configure_dedicated_feedback(
        &mut self,
        source: Box<crate::dedicated_rx::DedicatedFeedback>,
    ) -> Result<(), Box<crate::dedicated_rx::DedicatedFeedback>> {
        if self.started || source.origin() != self.receiver.air_origin() {
            return Err(source);
        }
        let Some(dedicated) = &mut self.dedicated else {
            return Err(source);
        };
        dedicated.attach_feedback(source)
    }

    pub(crate) fn paired_with(
        &self,
        source: &crate::common_tx::CommonTx,
        ingress: &RachIngress,
        origin: crate::radio_io::AirClockOrigin,
    ) -> bool {
        self.port.belongs_to(source)
            && self.publisher.paired_with(ingress)
            && self.receiver.air_origin() == origin
    }
    pub fn statistics(&self) -> RxStatistics {
        RxStatistics {
            radio: self.receiver.counters(),
            access: self.access.counters(),
            access_search: self.access.search_evidence(),
            published: self.published,
            dedicated: self.dedicated.as_ref().map(|runtime| runtime.counters()),
            first_retired_feedback: self
                .dedicated
                .as_ref()
                .and_then(|runtime| runtime.first_retired_feedback()),
            timing: self.timings.as_ref().map(|timings| timings.summary),
        }
    }

    pub fn first_dedicated_failure(&self) -> Option<&crate::dedicated_rx::SampleRuntimeFailure> {
        self.dedicated.as_ref()?.first_failure()
    }

    #[cfg(test)]
    pub(crate) fn inject_transport(
        &mut self,
        transport: umts_dsp::prach_message::DecodedPrachMessage,
        block_bits: usize,
        blocks: usize,
        bytes: &[u8],
    ) {
        self.publisher
            .inject_transport(transport, block_bits, blocks, bytes);
    }
    pub fn start(&mut self, tick: Option<u64>) -> Result<(), CommonRxError> {
        if !self.port.available() || !self.publisher.available() {
            return Err(CommonRxError::Closed);
        }
        self.receiver.start(tick).map_err(CommonRxError::Radio)?;
        self.started = true;
        Ok(())
    }
    pub fn cycle(&mut self) -> Result<CycleProgress, CommonRxError> {
        if !self.port.available() || !self.publisher.available() {
            return Err(CommonRxError::Closed);
        }
        for _ in 0..MAX_PLAN_EVENTS {
            if self.port.next_event().is_none() {
                break;
            }
        }
        let cycle_started = self.timings.as_ref().map(|_| std::time::Instant::now());
        let mut trace = RxCycleTiming::default();
        let read_started = cycle_started;
        let input = self.receiver.read_matched().map_err(CommonRxError::Radio)?;
        trace.read_filter_ns = timing::elapsed(read_started);
        let received = input.is_some();
        let access_result = if let Some(input) = input {
            trace.input_next_sample = Some(input.full.window.next_input.index);
            let feed_started = cycle_started.map(|_| std::time::Instant::now());
            if let Some(dedicated) = &mut self.dedicated {
                dedicated
                    .feed(input.full)
                    .map_err(CommonRxError::Dedicated)?;
            }
            trace.dedicated_feed_ns = timing::elapsed(feed_started);
            let search_started = cycle_started.map(|_| std::time::Instant::now());
            let result = self
                .access
                .feed(&mut self.port, input.common)
                .map_err(CommonRxError::Access);
            trace.access_search_ns = timing::elapsed(search_started);
            result
        } else {
            self.access.reconcile_transmit();
            Ok(())
        };
        trace.read = self.receiver.read_timing();
        access_result?;
        let dedicated_started = cycle_started.map(|_| std::time::Instant::now());
        let dedicated_cpu_started = dedicated_started.and_then(|_| timing::thread_cpu_time());
        if let Some(dedicated) = &mut self.dedicated {
            let before_cycle = dedicated.counters();
            let mut work = dedicated
                .radio_decode_work()
                .map_err(CommonRxError::Dedicated)?;
            // A single slot per radio read cannot recover time lost to a feedback wait.
            for _ in 0..DEDICATED_SERVICE_ROUNDS {
                let before = dedicated.counters().sampled_slots;
                dedicated.service_with_work(&mut work, |_, _| {});
                if dedicated.counters().sampled_slots == before {
                    break;
                }
            }
            let after_cycle = dedicated.counters();
            trace.sampled_slots = after_cycle.sampled_slots - before_cycle.sampled_slots;
            trace.decode_state_updates =
                after_cycle.decode_state_updates - before_cycle.decode_state_updates;
        }
        trace.dedicated_service_cpu_ns = dedicated_cpu_started.and_then(timing::thread_cpu_elapsed);
        trace.dedicated_service_ns = timing::elapsed(dedicated_started);
        let access_decode_started = cycle_started.map(|_| std::time::Instant::now());
        let result = self
            .publisher
            .publish_one(|| self.access.decode_one())
            .map_err(CommonRxError::Access)?;
        trace.access_decode_ns = timing::elapsed(access_decode_started);
        if result == DecodeProgress::Published {
            self.published = self.published.saturating_add(1);
        }
        let clock_started = cycle_started.map(|_| std::time::Instant::now());
        self.receiver.sample_clock().map_err(CommonRxError::Radio)?;
        trace.clock_sample_ns = timing::elapsed(clock_started);
        if let Some(started) = cycle_started {
            let freeze = false;
            self.timings
                .as_mut()
                .expect("enabled timing")
                .record(trace, started, freeze);
        }
        Ok(CycleProgress {
            received,
            access: result,
            sampled_slots: trace.sampled_slots,
            decode_state_updates: trace.decode_state_updates,
        })
    }
    pub fn stop(&mut self) -> Result<(), CommonRxError> {
        self.receiver.stop().map_err(CommonRxError::Radio)
    }
    pub(crate) fn stop_for_retirement(&mut self) -> Result<(), CommonRxError> {
        self.receiver
            .stop_for_retirement()
            .map_err(CommonRxError::Radio)
    }
}

#[cfg(test)]
pub(crate) mod tests;
