// SPDX-License-Identifier: AGPL-3.0-or-later

use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

mod broadcast;
mod carriers;
mod control;
use broadcast::BroadcastCycle;
pub(crate) use control::PlanReceipt;
pub use control::{
    AcquisitionCancellation, AcquisitionEvent, AcquisitionPort, Cancellation, CommonTxPort,
    ControlPorts, PlanEvent, PlanId, PlanLane, PlanRejection, PlanStage,
};

pub trait CommonTxTarget {
    fn available(&self) -> bool;
    fn information(&self) -> &CellInformation;
    fn next_chip(&self) -> ChipInstant;
    fn publish_transport(
        &mut self,
        start: ChipInstant,
        channel: usize,
        payload: &[Bit],
        indicators: &[bool],
        owner: Option<umts_types::UeKey>,
    ) -> Result<crate::tx_evidence::TransportSubmission, CommonTxError>;
}

use anyhow::{Result, anyhow, bail};
use num_complex::Complex32;
use umts_dsp::{
    DspError,
    bits::{Bit, TxBit},
    common_downlink::{CommonChannelAmplitudes, CommonDownlink},
    convolutional::CodeRate,
    crc::TransportCrc,
    crest_reduction::{CrestReducer, CrestReport},
    downlink_transport::{ConvolutionalDownlink, ConvolutionalFormat, FlexibleCommonFrame},
    indicators::{
        ACCESS_SLOT_CHIPS, Acquisition, AcquisitionDownlink, PAGING_LEAD_CHIPS, PagingCount,
        PagingDownlink, paging_start,
    },
    interleaving::Rel99Tti,
    prach::PrachCode,
    pulse_shaping::{PulseShaper, SamplesPerChip},
    secondary_downlink::SecondaryDownlink,
    tfci::Tfci,
    waveform::WaveformWindow,
};
use umts_rrc::{
    broadcast::ScheduleError,
    common_channels::{PagingIndicators, Position, SecondaryChannel, TransportRole},
    transport::{ChannelCoding, ConvolutionalRate, CrcSize, TransportInterval},
};
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant, SampleInstant};

use crate::{
    access::{AccessCalendar, AccessObservation, AccessScheduleError, AccessWindow},
    cell::{CellInformation, CellPlan},
};

pub(crate) const MAX_PENDING_TRANSPORTS: usize = 64;
const MAX_PAYLOAD_BITS: usize = 4968;
const MAX_PAGING_INDICATORS: usize = 144;
const BCH_FRAMES: u64 = 2;

#[derive(Clone, Copy, Debug)]
pub struct CommonAmplitudes {
    pub primary: CommonChannelAmplitudes,
    pub fach: f32,
    pub pch: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonTxError {
    Dsp(DspError),
    Schedule(ScheduleError),
    Access(AccessScheduleError),
    Time,
    Late,
    Capacity,
    Conflict,
    Channel,
    Payload,
    ComponentPeak,
    Poisoned,
}

impl From<DspError> for CommonTxError {
    fn from(error: DspError) -> Self {
        Self::Dsp(error)
    }
}

struct PendingTransport {
    occupied: bool,
    start: ChipInstant,
    channel: usize,
    payload: Vec<Bit>,
    indicators: [bool; MAX_PAGING_INDICATORS],
    plan: Option<control::PlanWriter>,
}

struct PagingStream {
    generator: PagingDownlink,
    count: usize,
    start: Option<ChipInstant>,
    chips: Vec<Complex32>,
}

struct SecondaryStream {
    generator: SecondaryDownlink,
    transport: ConvolutionalDownlink,
    multiplex: FlexibleCommonFrame,
    matched: Vec<Bit>,
    data: Vec<TxBit>,
    payload_bits: usize,
    empty_tfci: Tfci,
    full_tfci: Tfci,
    selected_tfci: Tfci,
    paging: Option<PagingStream>,
}

fn secondary(
    channel: &SecondaryChannel,
    cell: &CellInformation,
    amplitude: f32,
    pich_amplitude: f32,
) -> Result<SecondaryStream> {
    let settings = channel.settings();
    if channel.transports().len() != 1
        || settings.secondary_scrambling_code.is_some()
        || settings.transmit_diversity
        || settings.pilots
        || !settings.tfci
        || settings.timing_offset_chips != 0
        || !matches!(settings.position, Position::Flexible)
    {
        bail!("common runtime does not support this S-CCPCH construction");
    }
    let transport = &channel.transports()[0];
    let tfs = &transport.formats;
    if !matches!(tfs.interval, TransportInterval::Ms10)
        || tfs.groups.len() != 1
        || tfs.groups[0].block_counts.as_slice() != [0, 1]
    {
        bail!("common runtime requires the cell plan's zero/one-block 10 ms transport set");
    }
    let payload_bits = usize::from(tfs.groups[0].transport_block_bits);
    if payload_bits > MAX_PAYLOAD_BITS {
        bail!("common transport exceeds the preallocated payload capacity");
    }
    let rate = match tfs.coding {
        ChannelCoding::Convolutional(ConvolutionalRate::Half) => CodeRate::Half,
        ChannelCoding::Convolutional(ConvolutionalRate::Third) => CodeRate::Third,
        _ => bail!("common runtime requires convolutional coding"),
    };
    let crc = match tfs.crc {
        CrcSize::None => TransportCrc::None,
        CrcSize::Bits8 => TransportCrc::Crc8,
        CrcSize::Bits12 => TransportCrc::Crc12,
        CrcSize::Bits16 => TransportCrc::Crc16,
        CrcSize::Bits24 => TransportCrc::Crc24,
    };
    let generator =
        SecondaryDownlink::new(cell.primary_code(), settings.code, settings.tfci, amplitude)
            .map_err(|e| anyhow!("S-CCPCH: {e:?}"))?;
    let capacity = generator.format().data_bits_per_frame();
    let tfci = |ctfc| {
        channel
            .combinations()
            .ctfc_values()
            .iter()
            .position(|value| *value == ctfc)
            .ok_or_else(|| anyhow!("S-CCPCH TFCS missing CTFC {ctfc}"))
            .and_then(|index| Tfci::new(index as u16).map_err(|e| anyhow!("TFCI: {e:?}")))
    };
    let paging = settings
        .pich
        .as_ref()
        .map(|pich| {
            let count = match pich.indicators {
                PagingIndicators::PerFrame18 => PagingCount::PerFrame18,
                PagingIndicators::PerFrame36 => PagingCount::PerFrame36,
                PagingIndicators::PerFrame72 => PagingCount::PerFrame72,
                PagingIndicators::PerFrame144 => PagingCount::PerFrame144,
            };
            Ok::<_, anyhow::Error>(PagingStream {
                generator: PagingDownlink::new(
                    cell.primary_code(),
                    pich.code256,
                    count,
                    pich_amplitude,
                )
                .map_err(|e| anyhow!("PICH: {e:?}"))?,
                count: count.get(),
                start: None,
                chips: vec![Complex32::default(); CHIPS_PER_FRAME as usize],
            })
        })
        .transpose()?;
    Ok(SecondaryStream {
        transport: ConvolutionalDownlink::new(ConvolutionalFormat {
            block_bits: payload_bits,
            blocks: 1,
            crc,
            rate,
            interval: Rel99Tti::Ms10,
            rate_matched_bits: capacity,
        })
        .map_err(|e| anyhow!("common transport: {e:?}"))?,
        multiplex: FlexibleCommonFrame::new(capacity)
            .map_err(|e| anyhow!("common multiplex: {e:?}"))?,
        generator,
        matched: vec![Bit::Zero; capacity],
        data: vec![TxBit::Dtx; capacity],
        payload_bits,
        empty_tfci: tfci(0)?,
        full_tfci: tfci(1)?,
        selected_tfci: tfci(0)?,
        paging,
    })
}

struct AccessStream {
    profile: umts_rrc::EncodedInformation,
    generator: AcquisitionDownlink,
    calendar: AccessCalendar,
    chips: Vec<Complex32>,
    plans: Vec<Option<control::PendingAcquisition>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DynamicPowerStatistics {
    slots: u64,
    limited_slots: u64,
    maximum_dpch_input_power: f64,
    maximum_reserved_hsdpa_power: f64,
    minimum_gain: f32,
}

fn mix_dynamic(
    fixed: &mut [Complex32],
    dynamic: &[Complex32],
    limit: f32,
) -> Result<(f32, f64), DspError> {
    if fixed.len() != dynamic.len()
        || dynamic.is_empty()
        || !limit.is_finite()
        || !(0.0..=1.0).contains(&limit)
    {
        return Err(DspError::InvalidAmplitude);
    }
    let power = dynamic
        .iter()
        .map(|chip| f64::from(chip.re).powi(2) + f64::from(chip.im).powi(2))
        .sum::<f64>()
        / dynamic.len() as f64;
    if !power.is_finite() {
        return Err(DspError::InvalidAmplitude);
    }
    let gain = if power > f64::from(limit) {
        ((f64::from(limit) / power).sqrt() as f32)
            .next_down()
            .max(0.0)
    } else {
        1.0
    };
    for (output, dynamic) in fixed.iter_mut().zip(dynamic) {
        *output += *dynamic * gain;
    }
    Ok((gain, power))
}

pub struct CommonTx {
    carriers: Option<carriers::DualCarrier>,
    progress: crate::tx_evidence::Reader,
    submission_writer: Option<crate::tx_evidence::Writer>,
    information: Arc<CellInformation>,
    broadcast: BroadcastCycle,
    next: ChipInstant,
    rate: usize,
    common: CommonDownlink,
    slot: Vec<Complex32>,
    dynamic: Vec<Complex32>,
    dynamic_power_limit: Option<f32>,
    dynamic_power_statistics: DynamicPowerStatistics,
    shaper: PulseShaper,
    crest_reducer: Option<CrestReducer>,
    crest_report: CrestReport,
    generated: bool,
    component_peak_limit: Option<f32>,
    secondary: Vec<SecondaryStream>,
    access: Vec<AccessStream>,
    pending: Vec<PendingTransport>,
    poisoned: bool,
    control: Option<control::ControlBridge>,
    next_acquisition_id: Option<NonZeroU64>,
}

impl CommonTx {
    pub fn new(
        plan: CellPlan,
        origin: ChipInstant,
        rate: SamplesPerChip,
        span_chips: usize,
        amplitudes: CommonAmplitudes,
        capacity: NonZeroUsize,
        maximum_arrival_delay: u64,
    ) -> Result<Self> {
        if !origin.index.is_multiple_of(BCH_FRAMES * CHIPS_PER_FRAME) {
            bail!("common TX origin must be an even radio frame");
        }
        if capacity.get() > MAX_PENDING_TRANSPORTS {
            bail!("common TX command capacity exceeds {MAX_PENDING_TRANSPORTS}");
        }
        let (information, schedule) = plan.into_parts();
        let information = Arc::new(information);
        schedule
            .message_at(origin)
            .map_err(|e| anyhow!("broadcast origin: {e:?}"))?;
        let broadcast = BroadcastCycle::new(&schedule, origin)
            .map_err(|e| anyhow!("broadcast cycle: {e:?}"))?;
        let sib5 = information.sib5();
        let settings = sib5.settings();
        let relative_amplitude =
            |offset_db: i8| amplitudes.primary.cpich * 10.0_f32.powf(f32::from(offset_db) / 20.0);
        let mut secondary_streams = Vec::with_capacity(sib5.secondary_channels().len());
        for channel in sib5.secondary_channels() {
            let amplitude = if channel.transports()[0].role == TransportRole::Paging {
                amplitudes.pch
            } else {
                amplitudes.fach
            };
            secondary_streams.push(secondary(
                channel,
                &information,
                amplitude,
                relative_amplitude(settings.pich_power_offset_db),
            )?);
        }
        let mut access = Vec::with_capacity(sib5.prach().len());
        for prach in sib5.prach() {
            let settings = prach.settings();
            let code = PrachCode::for_cell(
                information.primary_code(),
                settings.preamble_scrambling_code,
            )
            .map_err(|e| anyhow!("PRACH code: {e:?}"))?;
            access.push(AccessStream {
                profile: prach
                    .encode()
                    .map_err(|e| anyhow!("PRACH profile: {e:?}"))?,
                generator: AcquisitionDownlink::new(
                    information.primary_code(),
                    settings.aich.code256,
                    relative_amplitude(sib5.settings().aich_power_offset_db),
                )
                .map_err(|e| anyhow!("AICH: {e:?}"))?,
                calendar: AccessCalendar::new(
                    origin.epoch,
                    code,
                    settings,
                    capacity,
                    maximum_arrival_delay,
                )
                .map_err(|e| anyhow!("AICH calendar: {e:?}"))?,
                chips: vec![Complex32::default(); ACCESS_SLOT_CHIPS],
                plans: (0..crate::access::MAX_ACCESS_RESPONSES)
                    .map(|_| None)
                    .collect(),
            });
        }
        let common = CommonDownlink::new(information.primary_code(), amplitudes.primary)
            .map_err(|e| anyhow!("common channels: {e:?}"))?;
        let shaper =
            PulseShaper::new(rate, span_chips).map_err(|e| anyhow!("pulse shaper: {e:?}"))?;
        origin
            .index
            .checked_add(BCH_FRAMES * CHIPS_PER_FRAME)
            .and_then(|n| n.checked_mul(rate.get() as u64))
            .and_then(|n| n.checked_add(shaper.group_delay_samples() as u64))
            .ok_or_else(|| anyhow!("common TX origin overflows sample time"))?;
        let (progress, submission_writer) = crate::tx_evidence::channel(SampleInstant {
            epoch: origin.epoch,
            index: origin
                .index
                .checked_mul(rate.get() as u64)
                .ok_or_else(|| anyhow!("common TX origin overflows sample time"))?,
        })
        .ok_or_else(|| anyhow!("common TX origin exceeds submission time range"))?;
        Ok(Self {
            carriers: None,
            progress,
            submission_writer: Some(submission_writer),
            information,
            broadcast,
            next: origin,
            rate: rate.get(),
            common,
            slot: vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            dynamic: vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            dynamic_power_limit: None,
            dynamic_power_statistics: DynamicPowerStatistics {
                slots: 0,
                limited_slots: 0,
                maximum_dpch_input_power: 0.0,
                maximum_reserved_hsdpa_power: 0.0,
                minimum_gain: 1.0,
            },
            shaper,
            crest_reducer: None,
            crest_report: CrestReport::default(),
            generated: false,
            component_peak_limit: None,
            secondary: secondary_streams,
            access,
            pending: (0..capacity.get())
                .map(|_| PendingTransport {
                    occupied: false,
                    start: origin,
                    channel: 0,
                    payload: vec![Bit::Zero; MAX_PAYLOAD_BITS],
                    indicators: [false; MAX_PAGING_INDICATORS],
                    plan: None,
                })
                .collect(),
            poisoned: false,
            control: None,
            next_acquisition_id: Some(NonZeroU64::MIN),
        })
    }

    pub fn information(&self) -> &CellInformation {
        &self.information
    }
    pub(crate) fn hsdpa_power_limit(&self) -> Option<f32> {
        self.dynamic_power_limit
    }
    pub fn next_chip(&self) -> ChipInstant {
        self.next
    }
    pub fn samples_per_slot(&self) -> usize {
        CHIPS_PER_SLOT as usize * self.rate
    }
    pub fn group_delay_samples(&self) -> usize {
        self.shaper.group_delay_samples()
            + self
                .crest_reducer
                .as_ref()
                .map_or(0, CrestReducer::group_delay_samples)
    }
    pub(crate) fn submission_progress(&self) -> crate::tx_evidence::Reader {
        self.progress.clone()
    }
    pub(crate) fn dedicated_frame_source(&self) -> crate::dedicated_tx::DedicatedFrameSource {
        crate::dedicated_tx::DedicatedFrameSource::new(
            self.progress.clone(),
            self.rate,
            self.group_delay_samples(),
        )
    }
    pub(crate) fn submission_paired_with(&self, reader: &crate::tx_evidence::Reader) -> bool {
        self.progress.paired_with(reader)
    }
    pub(crate) fn submission_writer(
        &mut self,
    ) -> Result<crate::tx_evidence::Writer, CommonTxError> {
        if self.submission_writer.is_none() {
            return Err(CommonTxError::Conflict);
        }
        let first = SampleInstant {
            epoch: self.next.epoch,
            index: self
                .next
                .index
                .checked_mul(self.rate as u64)
                .ok_or(CommonTxError::Time)?,
        };
        if self.progress.origin() != first {
            let (progress, writer) =
                crate::tx_evidence::channel(first).ok_or(CommonTxError::Time)?;
            self.submission_writer = Some(writer);
            self.progress = progress;
        }
        self.submission_writer.take().ok_or(CommonTxError::Conflict)
    }
    pub(crate) fn transport_submission(
        &self,
        start: ChipInstant,
    ) -> Result<crate::tx_evidence::TransportSubmission, CommonTxError> {
        self.progress
            .transport_receipt(start, self.rate, self.group_delay_samples())
            .ok_or(CommonTxError::Time)
    }

    pub fn queue_transport(
        &mut self,
        start: ChipInstant,
        channel: usize,
        payload: &[Bit],
        indicators: &[bool],
    ) -> Result<(), CommonTxError> {
        if self.control.is_some() {
            return Err(CommonTxError::Conflict);
        }
        self.queue_transport_inner(start, channel, payload, indicators)
            .map(|_| ())
    }

    fn queue_transport_inner(
        &mut self,
        start: ChipInstant,
        channel: usize,
        payload: &[Bit],
        indicators: &[bool],
    ) -> Result<usize, CommonTxError> {
        if self.poisoned {
            return Err(CommonTxError::Poisoned);
        }
        if start.epoch != self.next.epoch || !start.index.is_multiple_of(CHIPS_PER_FRAME) {
            return Err(CommonTxError::Time);
        }
        let stream = self.secondary.get(channel).ok_or(CommonTxError::Channel)?;
        if payload.len() != stream.payload_bits {
            return Err(CommonTxError::Payload);
        }
        let deadline = if let Some(paging) = &stream.paging {
            if indicators.len() != paging.count {
                return Err(CommonTxError::Payload);
            }
            paging_start(start)?.index
        } else {
            if !indicators.is_empty() {
                return Err(CommonTxError::Payload);
            }
            start.index
        };
        if deadline < self.next.index {
            return Err(CommonTxError::Late);
        }
        if start.index - self.next.index > MAX_PENDING_TRANSPORTS as u64 * CHIPS_PER_FRAME {
            return Err(CommonTxError::Capacity);
        }
        if self
            .pending
            .iter()
            .any(|entry| entry.occupied && entry.start == start && entry.channel == channel)
        {
            return Err(CommonTxError::Conflict);
        }
        let (index, entry) = self
            .pending
            .iter_mut()
            .enumerate()
            .find(|(_, entry)| !entry.occupied)
            .ok_or(CommonTxError::Capacity)?;
        entry.payload[..payload.len()].copy_from_slice(payload);
        entry.indicators.fill(false);
        entry.indicators[..indicators.len()].copy_from_slice(indicators);
        entry.start = start;
        entry.channel = channel;
        entry.occupied = true;
        entry.plan = None;
        Ok(index)
    }

    pub fn schedule_acquisition(
        &mut self,
        channel: usize,
        observation: AccessObservation,
        indicator: Acquisition,
        now: ChipInstant,
        minimum_tx_lead_chips: u64,
    ) -> Result<AccessWindow, CommonTxError> {
        if self.control.is_some() {
            return Err(CommonTxError::Conflict);
        }
        if self.poisoned {
            return Err(CommonTxError::Poisoned);
        }
        self.access
            .get_mut(channel)
            .ok_or(CommonTxError::Channel)?
            .calendar
            .schedule(
                observation,
                indicator,
                now,
                self.next,
                minimum_tx_lead_chips,
            )
            .map_err(CommonTxError::Access)
    }

    pub(crate) fn access_profile(&self, channel: usize) -> Option<&umts_rrc::EncodedInformation> {
        self.access.get(channel).map(|stream| &stream.profile)
    }

    fn compose_frame(&mut self) -> Result<(), CommonTxError> {
        for (index, stream) in self.secondary.iter_mut().enumerate() {
            let entry = self
                .pending
                .iter_mut()
                .find(|entry| entry.occupied && entry.start == self.next && entry.channel == index);
            let tfci = if let Some(entry) = entry {
                if entry.plan.as_ref().is_some_and(|plan| !plan.commit()) {
                    entry.occupied = false;
                    entry.plan = None;
                    stream.multiplex.compose(&[], &mut stream.data)?;
                    stream.selected_tfci = stream.empty_tfci;
                    continue;
                }
                stream
                    .transport
                    .encode(&entry.payload[..stream.payload_bits], &mut stream.matched)?;
                stream
                    .multiplex
                    .compose(&[&stream.matched], &mut stream.data)?;
                entry.occupied = false;
                entry.plan = None;
                stream.full_tfci
            } else {
                stream.multiplex.compose(&[], &mut stream.data)?;
                stream.empty_tfci
            };
            stream.selected_tfci = tfci;
        }
        Ok(())
    }

    fn compose_slot(&mut self) -> Result<(), CommonTxError> {
        if self.next.index.is_multiple_of(CHIPS_PER_FRAME) {
            self.compose_frame()?;
        }
        let offset = self.next.index % CHIPS_PER_FRAME;
        let frame = ChipInstant {
            epoch: self.next.epoch,
            index: self.next.index - offset,
        };
        let slot = (offset / CHIPS_PER_SLOT) as usize;
        self.common
            .compose_slot(self.broadcast.frame_at(frame)?, slot, &mut self.slot)?;
        for stream in &self.secondary {
            stream.generator.add_slot(
                &stream.data,
                Some(stream.selected_tfci),
                slot,
                &mut self.slot,
            )?;
        }
        for access in &mut self.access {
            if self.next.index.is_multiple_of(ACCESS_SLOT_CHIPS as u64) {
                for plan in access
                    .plans
                    .iter()
                    .flatten()
                    .filter(|plan| plan.window.indicator_start == self.next)
                {
                    if !plan.writer.commit() {
                        access
                            .calendar
                            .cancel(self.next, plan.observation.signature)
                            .map_err(CommonTxError::Access)?;
                    }
                }
                access.chips.fill(Complex32::default());
                if let Some(indicators) = access
                    .calendar
                    .take(self.next)
                    .map_err(CommonTxError::Access)?
                {
                    access
                        .generator
                        .add_access_slot(self.next, &indicators, &mut access.chips)?;
                }
                for plan in &mut access.plans {
                    if plan
                        .as_ref()
                        .is_some_and(|plan| plan.window.indicator_start == self.next)
                    {
                        *plan = None;
                    }
                }
            }
            let offset = (self.next.index % ACCESS_SLOT_CHIPS as u64) as usize;
            for (chip, indicator) in self
                .slot
                .iter_mut()
                .zip(&access.chips[offset..offset + CHIPS_PER_SLOT as usize])
            {
                *chip += indicator;
            }
        }
        for (channel, stream) in self.secondary.iter_mut().enumerate() {
            let Some(paging) = &mut stream.paging else {
                continue;
            };
            let associated = self
                .next
                .index
                .checked_add(PAGING_LEAD_CHIPS)
                .ok_or(CommonTxError::Time)?;
            if associated.is_multiple_of(CHIPS_PER_FRAME) {
                let mut indicators = [false; MAX_PAGING_INDICATORS];
                if let Some(entry) = self.pending.iter().find(|entry| {
                    entry.occupied && entry.start.index == associated && entry.channel == channel
                }) && entry.plan.as_ref().is_none_or(|plan| plan.commit())
                {
                    indicators.copy_from_slice(&entry.indicators);
                }
                paging.chips.fill(Complex32::default());
                paging.generator.add_frame(
                    self.next,
                    &indicators[..paging.count],
                    &mut paging.chips,
                )?;
                paging.start = Some(self.next);
            }
            if let Some(start) = paging.start {
                let offset = (self.next.index - start.index) as usize;
                for (chip, indicator) in self
                    .slot
                    .iter_mut()
                    .zip(&paging.chips[offset..offset + CHIPS_PER_SLOT as usize])
                {
                    *chip += indicator;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn set_component_peak_limit(&mut self, limit: f32) -> Result<(), CommonTxError> {
        if !limit.is_finite() || limit <= 0.0 || limit > 1.0 {
            return Err(CommonTxError::ComponentPeak);
        }
        self.component_peak_limit = Some(limit);
        Ok(())
    }

    pub(crate) fn take_component_peak_limit(&mut self) -> Option<f32> {
        self.component_peak_limit.take()
    }

    pub(crate) fn set_dynamic_power_limit(&mut self, power: f32) -> Result<(), CommonTxError> {
        if self.generated || !power.is_finite() || power <= 0.0 || power > 1.0 {
            return Err(CommonTxError::Dsp(DspError::InvalidAmplitude));
        }
        self.dynamic_power_limit = Some(power);
        Ok(())
    }

    pub(crate) fn report_dynamic_power(&self) {
        if self.dynamic_power_limit.is_some() {
            tracing::debug!(statistics = ?self.dynamic_power_statistics, limit = ?self.dynamic_power_limit, "Final dynamic downlink power");
        }
    }

    pub(crate) fn enable_crest_reduction(
        &mut self,
        rate: SamplesPerChip,
    ) -> Result<(), CommonTxError> {
        if self.poisoned
            || self.generated
            || self.control.is_some()
            || self.submission_writer.is_none()
            || self.crest_reducer.is_some()
            || rate.get() != self.rate
        {
            return Err(CommonTxError::Conflict);
        }
        let limit = self
            .component_peak_limit
            .ok_or(CommonTxError::ComponentPeak)?;
        let reducer = if self.carriers.is_some() {
            CrestReducer::for_dual_carrier(limit)?
        } else {
            CrestReducer::new(rate, limit)?
        };
        self.next
            .index
            .checked_add(BCH_FRAMES * CHIPS_PER_FRAME)
            .and_then(|index| index.checked_mul(self.rate as u64))
            .and_then(|index| index.checked_add(self.shaper.group_delay_samples() as u64))
            .and_then(|index| index.checked_add(reducer.group_delay_samples() as u64))
            .ok_or(CommonTxError::Time)?;
        self.crest_reducer = Some(reducer);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn take_crest_rejected(&mut self) -> bool {
        self.take_crest_report().rejected()
    }

    pub(crate) fn take_crest_report(&mut self) -> CrestReport {
        std::mem::take(&mut self.crest_report)
    }

    pub fn generate_slot(
        &mut self,
        output: &mut [Complex32],
    ) -> Result<WaveformWindow, CommonTxError> {
        self.generate_slot_inner(output, None, None, None, None, &mut |_| Ok(Bit::Zero))
    }

    pub fn generate_slot_with_dedicated(
        &mut self,
        output: &mut [Complex32],
        dedicated: &mut crate::dedicated_tx::DedicatedTransmitBank,
        mut tpc: impl FnMut(crate::dedicated_tx::TpcRequest) -> Bit,
    ) -> Result<WaveformWindow, CommonTxError> {
        if dedicated.next_chip() != self.next || !dedicated.matches_cell(&self.information) {
            return Err(CommonTxError::Conflict);
        }
        self.generate_slot_inner(output, Some(dedicated), None, None, None, &mut |request| {
            Ok(tpc(request))
        })
    }

    #[cfg(test)]
    pub(crate) fn generate_slot_with_hspa(
        &mut self,
        output: &mut [Complex32],
        dedicated: &mut crate::dedicated_tx::DedicatedTransmitBank,
        hsdpa: Option<&mut crate::hsdpa_tx::TransmitBank>,
        hsupa: Option<&mut crate::hsupa_tx::TransmitBank>,
        mut tpc: impl FnMut(
            crate::dedicated_tx::TpcRequest,
        ) -> Result<Bit, crate::dedicated_tx::TransmitError>,
    ) -> Result<WaveformWindow, CommonTxError> {
        if dedicated.next_chip() != self.next
            || !dedicated.matches_cell(&self.information)
            || hsupa.as_ref().is_some_and(|bank| !bank.paired_with(self))
        {
            return Err(CommonTxError::Conflict);
        }
        let mut hsdpa = hsdpa;
        if let Some(bank) = hsdpa.as_deref_mut() {
            if !bank.matches_progress(&self.progress)
                || bank.primary_code() != self.information.primary_code()
            {
                bank.isolate();
            }
            bank.preflight_slot(self.next, None);
        }
        self.generate_slot_inner(output, Some(dedicated), hsdpa, None, hsupa, &mut tpc)
    }

    pub(crate) fn generate_slot_with_carriers(
        &mut self,
        output: &mut [Complex32],
        dedicated: &mut crate::dedicated_tx::DedicatedTransmitBank,
        banks: &mut [crate::hsdpa_tx::TransmitBank],
        hsupa: Option<&mut crate::hsupa_tx::TransmitBank>,
        mut tpc: impl FnMut(
            crate::dedicated_tx::TpcRequest,
        ) -> Result<Bit, crate::dedicated_tx::TransmitError>,
    ) -> Result<WaveformWindow, CommonTxError> {
        if banks.len() > 2
            || dedicated.next_chip() != self.next
            || !dedicated.matches_cell(&self.information)
            || hsupa.as_ref().is_some_and(|bank| !bank.paired_with(self))
        {
            return Err(CommonTxError::Conflict);
        }
        let mut primary = None;
        let mut secondary = None;
        for bank in banks {
            if !bank.matches_progress(&self.progress) || !self.has_carrier(bank.primary_code()) {
                bank.isolate();
                continue;
            }
            bank.preflight_slot(self.next, None);
            if bank.primary_code() == self.information.primary_code() {
                if primary.is_some() {
                    bank.isolate();
                } else {
                    primary = Some(bank);
                }
            } else if secondary.is_some() {
                bank.isolate();
            } else {
                secondary = Some(bank);
            }
        }
        self.generate_slot_inner(output, Some(dedicated), primary, secondary, hsupa, &mut tpc)
    }

    fn generate_slot_inner(
        &mut self,
        output: &mut [Complex32],
        dedicated: Option<&mut crate::dedicated_tx::DedicatedTransmitBank>,
        hsdpa: Option<&mut crate::hsdpa_tx::TransmitBank>,
        secondary_hsdpa: Option<&mut crate::hsdpa_tx::TransmitBank>,
        hsupa: Option<&mut crate::hsupa_tx::TransmitBank>,
        tpc: &mut impl FnMut(
            crate::dedicated_tx::TpcRequest,
        ) -> Result<Bit, crate::dedicated_tx::TransmitError>,
    ) -> Result<WaveformWindow, CommonTxError> {
        if self.poisoned {
            return Err(CommonTxError::Poisoned);
        }
        if output.len() != self.samples_per_slot() {
            return Err(CommonTxError::Dsp(DspError::InvalidLength));
        }
        let next_chip = self
            .next
            .index
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(CommonTxError::Time)?;
        let first_index = self
            .next
            .index
            .checked_mul(self.rate as u64)
            .ok_or(CommonTxError::Time)?;
        let next_index = next_chip
            .checked_mul(self.rate as u64)
            .ok_or(CommonTxError::Time)?;
        let origin = first_index
            .checked_add(self.group_delay_samples() as u64)
            .ok_or(CommonTxError::Time)?;
        self.service_control()?;
        // Command intake for this slot is closed before synthesis starts.
        if let Some(control) = &self.control {
            control.advance(next_chip);
        }
        let result = self.compose_slot().and_then(|()| {
            self.dynamic.fill(Complex32::default());
            let dynamic = if self.dynamic_power_limit.is_some() { &mut self.dynamic } else { &mut self.slot };
            if let Some(dedicated) = dedicated {
                dedicated
                    .mix_slot_checked(self.next, dynamic, tpc)
                    .map_err(|_| CommonTxError::Conflict)?;
            }
            // TS 25.214 5.2.11 permits a constant HS-PDSCH power assumption for the entire subframe.
            let hsdpa_power = if let Some(hsdpa) = hsdpa {
                hsdpa.preflight_slot(self.next, self.dynamic_power_limit.map(f64::from));
                hsdpa.mix_slot(self.next, &mut self.slot).map_err(|_| CommonTxError::Conflict)?
            } else { 0.0 };
            if let Some(limit) = self.dynamic_power_limit {
                if !hsdpa_power.is_finite()
                    || hsdpa_power > f64::from(limit) * (1.0 + 8.0 * f64::from(f32::EPSILON))
                {
                    return Err(CommonTxError::Dsp(DspError::InvalidAmplitude));
                }
                let remaining = (f64::from(limit) - hsdpa_power).max(0.0) as f32;
                let (gain, power) = mix_dynamic(&mut self.slot, &self.dynamic, remaining).map_err(CommonTxError::Dsp)?;
                let statistics = &mut self.dynamic_power_statistics;
                statistics.slots = statistics.slots.saturating_add(1);
                statistics.limited_slots = statistics.limited_slots.saturating_add(u64::from(gain < 1.0));
                statistics.maximum_dpch_input_power = statistics.maximum_dpch_input_power.max(power);
                statistics.maximum_reserved_hsdpa_power = statistics.maximum_reserved_hsdpa_power.max(hsdpa_power);
                statistics.minimum_gain = statistics.minimum_gain.min(gain);
            }
            if let Some(hsupa) = hsupa {
                hsupa
                    .mix_slot(self.next, &mut self.slot)
                    .map_err(|_| CommonTxError::Conflict)?;
            }
            self.shaper
                .process(&self.slot, output)
                .map_err(CommonTxError::Dsp)?;
            if let Some(carriers) = &mut self.carriers {
                carriers.mix(self.next, secondary_hsdpa, output)?;
            }
            if let Some(reducer) = &mut self.crest_reducer {
                let report = reducer.process(output)?;
                self.crest_report.corrections = self.crest_report.corrections.saturating_add(report.corrections);
                self.crest_report.limited_components = self.crest_report.limited_components.saturating_add(report.limited_components);
                self.crest_report.work_limit_exceeded |= report.work_limit_exceeded;
                self.crest_report.relative_error = self.crest_report.relative_error.max(report.relative_error);
                self.crest_report.error_rms = self.crest_report.error_rms.max(report.error_rms);
            }
            if self.crest_report.rejected() && self.component_peak_limit.is_some() {
                return Err(CommonTxError::ComponentPeak);
            }
            if let Some(limit) = self.component_peak_limit
                && let Some(sample) = output.iter().find(|sample| {
                    !sample.re.is_finite()
                        || !sample.im.is_finite()
                        || sample.re.abs() > limit
                        || sample.im.abs() > limit
                })
            {
                tracing::warn!(
                    "TX waveform exceeds component peak margin before submission: chip={} real={} imag={} limit={}",
                    self.next.index, sample.re, sample.im, limit
                );
                return Err(CommonTxError::ComponentPeak);
            }
            Ok(())
        });
        if let Err(error) = result {
            self.poisoned = true;
            return Err(error);
        }
        let instant = |index| SampleInstant {
            epoch: self.next.epoch,
            index,
        };
        let window = WaveformWindow {
            first_sample: instant(first_index),
            next_sample: instant(next_index),
            chip_origin_sample: instant(origin),
        };
        self.next.index = next_chip;
        self.generated = true;
        Ok(window)
    }
}

impl CommonTxTarget for CommonTx {
    fn available(&self) -> bool {
        !self.poisoned && self.control.is_none() && self.progress.available()
    }
    fn information(&self) -> &CellInformation {
        self.information()
    }
    fn next_chip(&self) -> ChipInstant {
        self.next_chip()
    }
    fn publish_transport(
        &mut self,
        start: ChipInstant,
        channel: usize,
        payload: &[Bit],
        indicators: &[bool],
        owner: Option<umts_types::UeKey>,
    ) -> Result<crate::tx_evidence::TransportSubmission, CommonTxError> {
        if owner.is_some_and(|key| key.epoch != self.next.epoch) {
            return Err(CommonTxError::Time);
        }
        let first = if self
            .secondary
            .get(channel)
            .is_some_and(|stream| stream.paging.is_some())
        {
            paging_start(start)?
        } else {
            start
        };
        let receipt = if first == start {
            self.transport_submission(start)?
        } else {
            self.progress
                .interval_receipt(
                    first,
                    ChipInstant {
                        index: start
                            .index
                            .checked_add(CHIPS_PER_FRAME)
                            .ok_or(CommonTxError::Time)?,
                        ..start
                    },
                    self.rate,
                    self.group_delay_samples(),
                )
                .ok_or(CommonTxError::Time)?
        };
        self.queue_transport(start, channel, payload, indicators)?;
        Ok(receipt)
    }
}

#[cfg(test)]
pub(crate) mod tests;
