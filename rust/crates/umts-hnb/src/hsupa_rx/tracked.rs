// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    HarqReceiver, Publication, ReceiveError, ReceivePort, Received,
    delivery::{ControlLoss, Delivery, ProgressEvent},
};
use crate::dedicated_rx::{FrameAnchor, ReceiveIdentity};
use num_complex::Complex64;
use std::{
    num::{NonZeroU8, NonZeroUsize},
    sync::Arc,
    time::{Duration, Instant},
};
use umts_dsp::{
    DspError,
    coding::DecodeWork,
    dedicated::{LikelihoodSettings, UplinkDpchCode, UplinkSlot},
    hsupa::{
        CollectionProgress, ControlDecision, ControlDecoder, SingleReferencePower, TransportPlan,
        TtiCollector, UplinkChannels,
    },
};
use umts_runtime::mailbox::Sender;
#[cfg(test)]
use umts_runtime::mailbox::{self, Receiver};
use umts_types::{
    hsupa::{CodeLayout, Etfci, LayoutSet, Rel6Category, TransportBlockTable, Tti},
    time::{CHIPS_PER_SLOT, ChipInstant},
};

#[cfg(test)]
const MAX_CONTROL_OBSERVATIONS: usize = 512;
const MAX_DATA_CHANNELS: usize = 4;
const CONTROL_LOSS_REJECTIONS: u64 = 64;
const CONTROL_LOSS_ADMISSIONS: u64 = 64;

#[derive(Default)]
struct ControlLossRecorder {
    last_admitted: Option<ControlObservation>,
    consecutive_rejections: u64,
    consecutive_admissions: u64,
    armed: bool,
    recorded: bool,
}

impl ControlLossRecorder {
    fn observe(&mut self, observation: ControlObservation) -> Option<ControlLoss> {
        if self.recorded {
            return None;
        }
        if observation.admitted {
            self.last_admitted = Some(observation);
            self.consecutive_rejections = 0;
            self.consecutive_admissions = self.consecutive_admissions.saturating_add(1);
            self.armed |= self.consecutive_admissions >= CONTROL_LOSS_ADMISSIONS;
            return None;
        }
        self.consecutive_admissions = 0;
        if !self.armed {
            return None;
        }
        let last_admitted = self.last_admitted?;
        self.consecutive_rejections = self.consecutive_rejections.saturating_add(1);
        if self.consecutive_rejections < CONTROL_LOSS_REJECTIONS {
            return None;
        }
        self.recorded = true;
        Some(ControlLoss {
            last_admitted,
            rejected: observation,
            consecutive_rejections: self.consecutive_rejections,
        })
    }
}

#[derive(Clone, Copy, Debug)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots."
)]
pub struct ControlObservation {
    pub start: ChipInstant,
    pub decision: Option<ControlDecision>,
    pub admitted: bool,
    pub evidence: [f64; umts_dsp::hsupa::CONTROL_BITS],
    pub measurements: Option<umts_dsp::hsupa::ControlMeasurements>,
    pub data_measurements: [Option<umts_dsp::hsupa::DataMeasurements>; MAX_DATA_CHANNELS],
    pub presence_log_likelihood: Option<f64>,
}

struct ControlTrace {
    start_chip: u64,
    remaining: usize,
    bytes: usize,
    sender: Sender<ControlObservation>,
}

#[derive(Clone, Copy, Debug)]
pub struct ControlAdmission {
    minimum_score: f64,
    minimum_margin: f64,
}

impl ControlAdmission {
    pub fn new(minimum_score: f64, minimum_margin: f64) -> Result<Self, ReceiveError> {
        if !minimum_score.is_finite()
            || !(0.0..=1.0).contains(&minimum_score)
            || !minimum_margin.is_finite()
            || !(0.0..=2.0).contains(&minimum_margin)
        {
            return Err(ReceiveError::Format);
        }
        Ok(Self {
            minimum_score,
            minimum_margin,
        })
    }

    fn accepts(self, decision: ControlDecision) -> bool {
        decision.normalized_score >= self.minimum_score
            && decision
                .normalized_margin
                .is_some_and(|margin| margin >= self.minimum_margin)
    }
}

/// Normalized E-DPCCH score band within which a retransmission's stored format outranks the
/// raw codeword winner. Near-ties can flip the decoded E-TFCI between attempts of one
/// block and reset HARQ combining. The admission margin itself stays as configured.
const CONTEXT_TOLERANCE: f64 = 0.2;

pub struct TrackedSettings {
    pub identity: ReceiveIdentity,
    pub anchor: FrameAnchor,
    pub interval: Tti,
    pub maximum_dpdch: u8,
    pub hsdpa_configured: bool,
    pub control_gain_ratio: f64,
    pub likelihood: LikelihoodSettings,
    pub admission: ControlAdmission,
    pub rv_zero_only: bool,
    pub iterations: NonZeroU8,
    pub retained_sample_bytes: NonZeroUsize,
    pub soft_buffer_bytes: NonZeroUsize,
    pub scratch_bytes: NonZeroUsize,
}

pub struct TrackedFormat {
    pub identity: Etfci,
    pub plan: Arc<TransportPlan>,
    pub layout: CodeLayout,
    pub gain_ratios: Vec<f64>,
}

impl TrackedFormat {
    pub fn from_table(
        table: TransportBlockTable,
        identity: Etfci,
        category: Rel6Category,
        layouts: &LayoutSet,
        power: SingleReferencePower,
        harq_offset_db: u8,
        maximum_bits: NonZeroUsize,
    ) -> Result<Self, ReceiveError> {
        let (plan, layout) =
            TransportPlan::from_table(table, identity, category, layouts, maximum_bits)
                .map_err(ReceiveError::Dsp)?;
        let payload = NonZeroUsize::new(plan.payload_bits()).ok_or(ReceiveError::Format)?;
        let gains = power
            .gains(payload, layout, harq_offset_db)
            .map_err(ReceiveError::Dsp)?;
        Ok(Self {
            identity,
            plan: Arc::new(plan),
            layout,
            gain_ratios: gains.as_slice().to_vec(),
        })
    }
}

struct Format {
    identity: Etfci,
    channels: UplinkChannels,
    gains: Vec<f64>,
    channel_bits: Vec<usize>,
}

pub struct DecodedTti<'a> {
    pub timing: Option<DecodeTiming>,
    pub identity: ReceiveIdentity,
    pub start: ChipInstant,
    pub absolute_tti: u64,
    pub control: ControlDecision,
    pub received: Received<'a>,
}

#[derive(Clone, Copy, Debug, Default)]
#[expect(
    dead_code,
    reason = "Retains the complete observation for diagnostic snapshots and replay."
)]
pub struct DecodeTiming {
    pub collection_clock_upper: Option<ChipInstant>,
    pub collection_wait: Duration,
    pub preparation: Duration,
    pub service: Duration,
    pub elapsed: Duration,
    pub service_calls: u32,
}

#[expect(
    dead_code,
    reason = "Retains the complete typed result and resource custody."
)]
pub enum StartProgress {
    NoControl {
        start: ChipInstant,
    },
    RejectedControl {
        start: ChipInstant,
        decision: ControlDecision,
    },
    Started,
}

#[derive(Clone, Copy)]
struct PendingTti {
    begun: Instant,
    timing: DecodeTiming,
    start: ChipInstant,
    absolute_tti: u64,
    control: ControlDecision,
    presence_log_likelihood: Option<f64>,
}

#[derive(Clone, Copy)]
struct CollectionEvidence {
    completed: Instant,
    clock_upper: Option<ChipInstant>,
}

enum Prepared {
    NoControl(ChipInstant),
    Rejected(ChipInstant, ControlDecision),
    Ready {
        start: ChipInstant,
        absolute_tti: u64,
        decision: ControlDecision,
        length: usize,
        presence_log_likelihood: Option<f64>,
    },
}

#[cfg(test)]
struct PhysicalTrace {
    targets: [Option<u64>; 2],
    sender: Sender<(u64, Vec<f32>)>,
}

pub struct TrackedReceiver {
    settings: TrackedSettings,
    collector: TtiCollector,
    collection: Option<CollectionEvidence>,
    control: ControlDecoder,
    formats: Vec<Format>,
    harq: HarqReceiver,
    symbols: Vec<Complex64>,
    likelihoods: Vec<f64>,
    physical: Vec<f32>,
    pending: Option<PendingTti>,
    delivery: Option<Delivery>,
    started: bool,
    dropped_intervals: u64,
    decode_work_per_slot: usize,
    control_trace: Option<ControlTrace>,
    control_loss: ControlLossRecorder,
    #[cfg(test)]
    physical_trace: Option<PhysicalTrace>,
    #[cfg(test)]
    data_phase_tracking: bool,
    #[cfg(test)]
    data_gain_tracking: bool,
}

impl TrackedReceiver {
    #[cfg(test)]
    pub fn trace_control(
        &mut self,
        start_chip: u64,
        capacity: NonZeroUsize,
    ) -> Result<Receiver<ControlObservation>, ReceiveError> {
        if self.started || self.control_trace.is_some() || capacity.get() > MAX_CONTROL_OBSERVATIONS
        {
            return Err(ReceiveError::Format);
        }
        let bytes = mailbox::retained_storage_bound::<ControlObservation>(capacity)
            .ok_or(ReceiveError::Budget)?;
        let (sender, receiver) = mailbox::bounded(capacity);
        self.control_trace = Some(ControlTrace {
            start_chip,
            remaining: capacity.get(),
            bytes,
            sender,
        });
        Ok(receiver)
    }

    pub fn decode_work_per_slot(&self) -> usize {
        self.decode_work_per_slot
    }
    pub fn identity(&self) -> ReceiveIdentity {
        self.settings.identity
    }
    pub fn interval(&self) -> Tti {
        self.settings.interval
    }
    pub fn anchor(&self) -> FrameAnchor {
        self.settings.anchor
    }
    pub fn is_started(&self) -> bool {
        self.started
    }
    pub fn uses_code(&self, code: &Arc<UplinkDpchCode>) -> bool {
        self.collector.uses_code(code)
    }
    pub fn maximum_dpdch(&self) -> u8 {
        self.settings.maximum_dpdch
    }
    pub fn has_output(&self) -> bool {
        self.delivery.is_some()
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some() || self.collector.complete().is_some()
    }

    // The shared DPCCH scrambling allocation is charged by the dedicated receiver.
    pub fn retained_bytes(&self) -> Result<usize, ReceiveError> {
        let parts = [
            std::mem::size_of::<Self>(),
            self.collector.retained_bytes() - std::mem::size_of::<TtiCollector>(),
            self.control.retained_bytes() - std::mem::size_of::<ControlDecoder>(),
            self.harq.retained_bytes()? - std::mem::size_of::<HarqReceiver>(),
            self.formats.capacity() * std::mem::size_of::<Format>(),
            self.symbols.capacity() * std::mem::size_of::<Complex64>(),
            self.likelihoods.capacity() * std::mem::size_of::<f64>(),
            self.physical.capacity() * std::mem::size_of::<f32>(),
            self.output_bytes(),
            self.control_trace.as_ref().map_or(0, |trace| trace.bytes),
        ];
        let base = parts
            .into_iter()
            .try_fold(0usize, usize::checked_add)
            .ok_or(ReceiveError::Budget)?;
        self.formats
            .iter()
            .try_fold(base, |bytes, format| {
                bytes
                    .checked_add(
                        format.channels.retained_bytes() - std::mem::size_of::<UplinkChannels>(),
                    )?
                    .checked_add(format.gains.capacity() * std::mem::size_of::<f64>())?
                    .checked_add(format.channel_bits.capacity() * std::mem::size_of::<usize>())
            })
            .ok_or(ReceiveError::Budget)
    }
    // Decoder plans and their working memory require separate cell-wide admission.
    pub fn new(
        code: Arc<UplinkDpchCode>,
        settings: TrackedSettings,
        definitions: Vec<TrackedFormat>,
    ) -> Result<Self, ReceiveError> {
        if settings.anchor.time.epoch != settings.identity.ue.epoch
            || definitions.is_empty()
            || definitions.len() > 128
        {
            return Err(ReceiveError::Format);
        }
        let slots = (settings.interval.chips() / CHIPS_PER_SLOT) as usize;
        let mut formats = Vec::with_capacity(definitions.len());
        let mut plans = Vec::with_capacity(definitions.len());
        let mut identities = Vec::with_capacity(definitions.len());
        let mut maximum_symbols = 0;
        let mut maximum_physical = 0;
        let mut decode_work_per_slot = 0;
        for definition in definitions {
            let work = definition
                .plan
                .decode_state_updates(settings.iterations)
                .map_err(ReceiveError::Dsp)?;
            decode_work_per_slot = decode_work_per_slot.max(work.div_ceil(slots));
            let sfs = definition.layout.spreading_factors();
            let channel_bits: Vec<_> = definition.plan.channel_bits().collect();
            if definition.plan.interval() != settings.interval
                || channel_bits.len() != sfs.len()
                || definition.gain_ratios.len() != sfs.len()
                || identities.contains(&definition.identity)
                || definition
                    .gain_ratios
                    .iter()
                    .any(|gain| !gain.is_finite() || *gain <= 0.0)
            {
                return Err(ReceiveError::Format);
            }
            let channels = UplinkChannels::new(
                Arc::clone(&code),
                settings.anchor.time,
                settings.maximum_dpdch,
                settings.hsdpa_configured,
                sfs,
            )
            .map_err(ReceiveError::Dsp)?;
            for (index, bits) in channel_bits.iter().enumerate() {
                let symbols = channels
                    .data_symbols_per_slot(index)
                    .ok_or(ReceiveError::Format)?;
                if symbols * slots != *bits {
                    return Err(ReceiveError::Format);
                }
                maximum_symbols = maximum_symbols.max(symbols);
            }
            maximum_physical = maximum_physical.max(definition.plan.physical_bits());
            identities.push(definition.identity);
            plans.push((definition.identity, definition.plan));
            formats.push(Format {
                identity: definition.identity,
                channels,
                gains: definition.gain_ratios,
                channel_bits,
            });
        }
        let bytes = maximum_physical
            .checked_mul(std::mem::size_of::<f64>() + std::mem::size_of::<f32>())
            .and_then(|bytes| {
                bytes.checked_add(maximum_symbols.checked_mul(std::mem::size_of::<Complex64>())?)
            })
            .ok_or(ReceiveError::Budget)?;
        if bytes > settings.scratch_bytes.get() {
            return Err(ReceiveError::Budget);
        }
        let collector = TtiCollector::new(
            code,
            settings.anchor.time,
            settings.interval,
            settings.control_gain_ratio,
            settings.likelihood,
            settings.retained_sample_bytes,
        )
        .map_err(ReceiveError::Dsp)?;
        let control = ControlDecoder::new(&identities).map_err(ReceiveError::Dsp)?;
        let harq = HarqReceiver::new(
            settings.identity.ue,
            settings.interval,
            settings.rv_zero_only,
            plans,
            settings.iterations,
            settings.soft_buffer_bytes,
        )?;
        Ok(Self {
            settings,
            collector,
            collection: None,
            control,
            formats,
            harq,
            symbols: vec![Complex64::default(); maximum_symbols],
            likelihoods: vec![0.0; maximum_physical],
            physical: vec![0.0; maximum_physical],
            pending: None,
            delivery: None,
            started: false,
            dropped_intervals: 0,
            decode_work_per_slot,
            control_trace: None,
            control_loss: ControlLossRecorder::default(),
            #[cfg(test)]
            physical_trace: None,
            #[cfg(test)]
            data_phase_tracking: true,
            #[cfg(test)]
            data_gain_tracking: true,
        })
    }

    pub fn feed(&mut self, slot: &UplinkSlot<'_>) -> Result<CollectionProgress, ReceiveError> {
        self.started = true;
        let progress = self.collector.feed(slot).map_err(ReceiveError::Dsp)?;
        self.collection = (progress == CollectionProgress::Complete).then(|| CollectionEvidence {
            completed: Instant::now(),
            clock_upper: None,
        });
        Ok(progress)
    }

    pub(crate) fn observe_collection_clock(&mut self, slot: ChipInstant, upper: ChipInstant) {
        let Some(collected) = self.collector.complete() else {
            return;
        };
        let start = collected.start();
        let Some(end) = start.index.checked_add(collected.interval().chips()) else {
            return;
        };
        if slot.epoch != start.epoch
            || upper.epoch != start.epoch
            || slot.index.checked_add(CHIPS_PER_SLOT) != Some(end)
        {
            return;
        }
        if let Some(evidence) = &mut self.collection
            && evidence.clock_upper.is_none()
        {
            evidence.clock_upper = Some(upper);
        }
    }

    pub fn feed_worker(&mut self, slot: &UplinkSlot<'_>) -> Result<(), ReceiveError> {
        if self.collector.complete().is_some() {
            self.collector.release();
            self.dropped_intervals = self.dropped_intervals.saturating_add(1);
            self.observe(ProgressEvent::DroppedInterval);
        }
        if self.feed(slot)? == CollectionProgress::Complete {
            self.observe(ProgressEvent::Collected(slot.start()));
        }
        Ok(())
    }

    fn observe(&self, event: ProgressEvent) {
        if let Some(delivery) = &self.delivery {
            delivery.observe(event);
        }
    }

    pub(crate) fn observe_samples(
        &self,
        counters: crate::dedicated_rx::SampleReceiveCounters,
        search: Option<ChipInstant>,
    ) {
        if let Some(delivery) = &self.delivery {
            delivery.observe_samples(counters, search);
        }
    }

    pub fn dropped_intervals(&self) -> u64 {
        self.dropped_intervals
    }
    pub fn outstanding_reports(&self) -> usize {
        self.delivery.as_ref().map_or(0, Delivery::outstanding)
    }

    #[cfg(test)]
    pub fn service_worker(&mut self, work: &mut DecodeWork) -> Result<Publication, ReceiveError> {
        self.service_worker_with_observer(work, |_, _| ())
    }

    pub fn service_worker_with_observer(
        &mut self,
        work: &mut DecodeWork,
        observe: impl FnMut(&DecodedTti<'_>, Option<f64>),
    ) -> Result<Publication, ReceiveError> {
        if !self.delivery.as_ref().is_some_and(Delivery::available) {
            return Ok(Publication::Closed);
        }
        if self.pending.is_none() && self.collector.complete().is_some() {
            match self.begin_ready()? {
                StartProgress::Started => self.observe(ProgressEvent::AdmittedControl),
                StartProgress::NoControl { .. } => {
                    self.observe(ProgressEvent::NoControl);
                    return Ok(Publication::Idle);
                }
                StartProgress::RejectedControl { .. } => {
                    self.observe(ProgressEvent::RejectedControl);
                    return Ok(Publication::Idle);
                }
            }
        }
        let result = self.service_output_with_observer(work, observe)?;
        if result == Publication::Backpressure {
            self.observe(ProgressEvent::OutputBackpressure);
        }
        Ok(result)
    }

    pub fn connect(
        &mut self,
        capacity: NonZeroUsize,
        maximum_bytes: NonZeroUsize,
    ) -> Result<ReceivePort, ReceiveError> {
        if self.started || self.delivery.is_some() {
            return Err(ReceiveError::Format);
        }
        let maximum_payload =
            NonZeroUsize::new(self.harq.payload.len()).ok_or(ReceiveError::Format)?;
        let (delivery, port) = Delivery::new(capacity, maximum_payload, maximum_bytes)?;
        self.delivery = Some(delivery);
        Ok(port)
    }

    pub fn output_bytes(&self) -> usize {
        self.delivery.as_ref().map_or(0, Delivery::bytes)
    }

    fn service_output_with_observer(
        &mut self,
        work: &mut DecodeWork,
        mut observe: impl FnMut(&DecodedTti<'_>, Option<f64>),
    ) -> Result<Publication, ReceiveError> {
        let delivery = self.delivery.as_mut().ok_or(ReceiveError::Format)?;
        let Some(pending) = self.pending.as_mut() else {
            return Ok(Publication::Idle);
        };
        match delivery.reserve() {
            Publication::Pending => (),
            other => return Ok(other),
        }
        let started = Instant::now();
        let received = self.harq.service(work)?;
        pending.timing.service = pending.timing.service.saturating_add(started.elapsed());
        pending.timing.service_calls = pending.timing.service_calls.saturating_add(1);
        let Some(received) = received else {
            return Ok(Publication::Pending);
        };
        pending.timing.elapsed = pending.begun.elapsed();
        let decoded = DecodedTti {
            timing: Some(pending.timing),
            identity: self.settings.identity,
            start: pending.start,
            absolute_tti: pending.absolute_tti,
            control: pending.control,
            received,
        };
        observe(&decoded, pending.presence_log_likelihood);
        delivery.publish(decoded);
        self.pending = None;
        Ok(Publication::Published)
    }

    pub fn discard_incomplete(&mut self) {
        if self.collector.complete().is_none() {
            self.collector.release();
            self.collection = None;
        }
    }

    // Turbo work belongs on a DSP owner, never the radio reader or TX worker.

    pub fn begin_ready(&mut self) -> Result<StartProgress, ReceiveError> {
        if self.pending.is_some() {
            return Err(ReceiveError::Harq(umts_mac::hsupa::HarqError::Pending));
        }
        if self.collector.complete().is_none() {
            return Err(ReceiveError::NotReady);
        }
        let begun = Instant::now();
        let collection = self.collection.take();
        let prepared = self.prepare();
        self.collector.release();
        match prepared? {
            Prepared::NoControl(start) => Ok(StartProgress::NoControl { start }),
            Prepared::Rejected(start, decision) => {
                Ok(StartProgress::RejectedControl { start, decision })
            }
            Prepared::Ready {
                start,
                absolute_tti,
                decision,
                length,
                presence_log_likelihood,
            } => {
                self.harq.begin(
                    self.settings.identity.ue,
                    absolute_tti,
                    decision.information,
                    &self.physical[..length],
                )?;
                self.pending = Some(PendingTti {
                    begun,
                    timing: DecodeTiming {
                        collection_clock_upper: collection.and_then(|value| value.clock_upper),
                        collection_wait: collection.map_or(Duration::ZERO, |value| {
                            begun.duration_since(value.completed)
                        }),
                        preparation: begun.elapsed(),
                        ..DecodeTiming::default()
                    },
                    start,
                    absolute_tti,
                    control: decision,
                    presence_log_likelihood,
                });
                Ok(StartProgress::Started)
            }
        }
    }

    fn prepare(&mut self) -> Result<Prepared, ReceiveError> {
        let collected = self.collector.complete().ok_or(ReceiveError::NotReady)?;
        let start = collected.start();
        let initial = u64::from(
            self.settings
                .interval
                .number(self.settings.anchor.cfn, 0)
                .map_err(|_| ReceiveError::Format)?,
        );
        let elapsed = start
            .index
            .checked_sub(self.settings.anchor.time.index)
            .ok_or(ReceiveError::Dsp(DspError::DiscontinuousTime))?;
        let absolute_tti = initial
            .checked_add(elapsed / self.settings.interval.chips())
            .ok_or(ReceiveError::Dsp(DspError::DiscontinuousTime))?;
        // A HARQ retransmission must repeat its first attempt's E-TFCI (TS 25.321 11.8.1.4), so
        // a near-tie on the CRC-less E-DPCCH resolves toward the stored format.
        let expected = self.harq.expected_retransmission(absolute_tti);
        let decision = collected
            .decode_control_with_context(&self.control, expected, CONTEXT_TOLERANCE)
            .map_err(ReceiveError::Dsp)?;
        let presence_log_likelihood =
            decision.and_then(|value| collected.control_presence_log_likelihood(value));
        if let Some(delivery) = self.delivery.as_ref()
            && !self.control_loss.recorded
        {
            let observation = ControlObservation {
                start,
                decision,
                admitted: decision.is_some_and(|value| self.settings.admission.accepts(value)),
                evidence: *collected.control_evidence(),
                measurements: None,
                data_measurements: [None; MAX_DATA_CHANNELS],
                presence_log_likelihood,
            };
            if let Some(loss) = self.control_loss.observe(observation) {
                delivery.record_control_loss(loss);
            }
        }
        if let Some(trace) = self.control_trace.as_mut()
            && trace.remaining > 0
            && start.index >= trace.start_chip
        {
            let mut data_measurements = [None; MAX_DATA_CHANNELS];
            if let Some(decision) = decision
                && self.settings.admission.accepts(decision)
                && let Some(format) = self
                    .formats
                    .iter()
                    .find(|format| format.identity == decision.information.format)
            {
                for (channel, gain) in format.gains.iter().enumerate() {
                    if let Some(symbols) = format.channels.data_symbols_per_slot(channel) {
                        data_measurements[channel] = collected
                            .measure_data(
                                &format.channels,
                                channel,
                                *gain,
                                &mut self.symbols[..symbols],
                            )
                            .ok();
                    }
                }
            }
            // The queue retains at most the first capacity observations, even if drained.
            let _ = trace.sender.try_send(ControlObservation {
                start,
                decision,
                admitted: decision.is_some_and(|value| self.settings.admission.accepts(value)),
                evidence: *collected.control_evidence(),
                measurements: collected.measure_control().ok(),
                data_measurements,
                presence_log_likelihood,
            });
            trace.remaining -= 1;
        }
        let Some(decision) = decision else {
            return Ok(Prepared::NoControl(start));
        };
        if !self.settings.admission.accepts(decision) {
            return Ok(Prepared::Rejected(start, decision));
        }
        let format = self
            .formats
            .iter()
            .find(|format| format.identity == decision.information.format)
            .ok_or(ReceiveError::Format)?;
        let mut offset = 0;
        let mut channel = 0;
        while channel < format.channel_bits.len() {
            let bits = format.channel_bits[channel];
            let symbols = format
                .channels
                .data_symbols_per_slot(channel)
                .ok_or(ReceiveError::Format)?;
            if channel + 1 < format.channel_bits.len()
                && format.channel_bits[channel + 1] == bits
                && format.channels.data_symbols_per_slot(channel + 1) == Some(symbols)
            {
                let (first, second) =
                    self.likelihoods[offset..offset + 2 * bits].split_at_mut(bits);
                #[cfg(test)]
                let track_phase = self.data_phase_tracking;
                #[cfg(not(test))]
                let track_phase = true;
                #[cfg(test)]
                let track_gain = self.data_gain_tracking;
                #[cfg(not(test))]
                let track_gain = true;
                if track_phase && track_gain {
                    collected
                        .data_pair_calibrated_llrs(
                            &format.channels,
                            channel,
                            [format.gains[channel], format.gains[channel + 1]],
                            self.settings.likelihood,
                            &mut self.symbols[..symbols],
                            [first, second],
                        )
                        .map_err(ReceiveError::Dsp)?;
                } else if track_phase {
                    collected
                        .data_pair_tracked_llrs(
                            &format.channels,
                            channel,
                            [format.gains[channel], format.gains[channel + 1]],
                            self.settings.likelihood,
                            &mut self.symbols[..symbols],
                            [first, second],
                        )
                        .map_err(ReceiveError::Dsp)?;
                } else {
                    collected
                        .data_pair_llrs(
                            &format.channels,
                            channel,
                            [format.gains[channel], format.gains[channel + 1]],
                            self.settings.likelihood,
                            &mut self.symbols[..symbols],
                            [first, second],
                        )
                        .map_err(ReceiveError::Dsp)?;
                }
                offset += 2 * bits;
                channel += 2;
            } else {
                collected
                    .data_llrs(
                        &format.channels,
                        channel,
                        format.gains[channel],
                        self.settings.likelihood,
                        &mut self.symbols[..symbols],
                        &mut self.likelihoods[offset..offset + bits],
                    )
                    .map_err(ReceiveError::Dsp)?;
                offset += bits;
                channel += 1;
            }
        }
        // Preserve the pilot-derived relative reliability between HARQ attempts.
        for (source, target) in self.likelihoods[..offset]
            .iter()
            .zip(&mut self.physical[..offset])
        {
            *target = *source as f32;
            if !target.is_finite() {
                return Err(ReceiveError::Dsp(DspError::NonFiniteEvidence));
            }
        }
        #[cfg(test)]
        if let Some(trace) = self.physical_trace.as_mut()
            && let Some(index) = trace
                .targets
                .iter()
                .position(|target| *target == Some(absolute_tti))
        {
            trace.targets[index] = None;
            let _ = trace
                .sender
                .try_send((absolute_tti, self.physical[..offset].to_vec()));
        }
        Ok(Prepared::Ready {
            start,
            absolute_tti,
            decision,
            length: offset,
            presence_log_likelihood,
        })
    }
}

#[cfg(test)]
mod tests;
