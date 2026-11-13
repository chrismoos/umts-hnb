// SPDX-License-Identifier: AGPL-3.0-or-later

use super::coherent::{
    CarrierObservation, CarrierTrackingSettings, DpcchStatistics, LikelihoodSettings, PilotCarrier,
    UplinkGains,
};
use crate::{
    DspError,
    channel_codes::OvsfCode,
    pilots::{PilotEstimate, UplinkPilots},
    soft_bits::normalize_soft_bits,
    tfci::{TfciDecision, TfciDecoder, UPLINK_CODE_BITS},
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode},
};
use num_complex::{Complex32, Complex64};
use std::sync::Arc;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant, SLOTS_PER_FRAME};

const CONTROL_SF: usize = 256;

const CONTROL_BITS: usize = 10;
const MAX_DATA_CHANNELS: usize = 6;
const MULTICODE_SF: u16 = 4;
const SINGLE_DATA_SFS: [u16; 7] = [256, 128, 64, 32, 16, 8, 4];
const DATA_CODE_COUNT: usize = 9;
const CONTROL_NOISE_GROUP_CHIPS: usize = 8;
const CONTROL_NOISE_HALF_CHIPS: usize = CONTROL_SF / 2;
// F(60, 2*(pilot_count-1)) 99th percentiles bound stationary variance-estimation scatter.
const CONTROL_VARIANCE_RATIO_99: [f64; 4] = [
    5.03161770679492,
    4.081855265474271,
    3.535473469281974,
    3.181274093717457,
];

fn control_symbol_disturbance(samples: &[Complex64]) -> f64 {
    let mut residual = 0.0;
    for half in samples.chunks_exact(CONTROL_NOISE_HALF_CHIPS) {
        let mean = half.iter().copied().sum::<Complex64>() / half.len() as f64;
        for group in half.chunks_exact(CONTROL_NOISE_GROUP_CHIPS) {
            let value = group.iter().copied().sum::<Complex64>() / CONTROL_NOISE_GROUP_CHIPS as f64;
            residual += (value - mean).norm_sqr();
        }
    }
    // TS 25.213 4.3.1.2: eight-chip sums reject data/HS-DPCCH codes except C(256,1), removed by half means.
    let groups = CONTROL_SF / CONTROL_NOISE_GROUP_CHIPS;
    residual / ((groups - 2) * groups) as f64
}

#[cfg(test)]
#[path = "tests/control_noise.rs"]
mod control_noise_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UplinkControlFormat(u8);
impl UplinkControlFormat {
    pub fn new(number: u8) -> Result<Self, DspError> {
        if number > 4 {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(number))
    }
    pub fn number(self) -> u8 {
        self.0
    }
    // TS 25.211 Table 2, normal DPCCH formats. Compressed A/B formats are distinct configurations.
    pub fn pilots(self) -> UplinkPilots {
        match self.0 {
            0 | 4 => UplinkPilots::Six,
            1 => UplinkPilots::Eight,
            2 => UplinkPilots::Five,
            3 => UplinkPilots::Seven,
            _ => unreachable!("validated control format"),
        }
    }
    pub fn tfci_bits(self) -> usize {
        if matches!(self.0, 0 | 2) { 2 } else { 0 }
    }
    pub fn fbi_bits(self) -> usize {
        usize::from(matches!(self.0, 2 | 3))
    }
    pub fn tpc_bits(self) -> usize {
        if self.0 == 4 { 4 } else { 2 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UplinkBranch {
    I,
    Q,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataChannel {
    code: OvsfCode,
    branch: UplinkBranch,
}
impl DataChannel {
    pub fn code(self) -> OvsfCode {
        self.code
    }
    pub fn branch(self) -> UplinkBranch {
        self.branch
    }
    pub fn bits_per_slot(self) -> usize {
        CHIPS_PER_SLOT as usize / usize::from(self.code.spreading_factor())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UplinkLayout {
    channels: [Option<DataChannel>; MAX_DATA_CHANNELS],
    count: usize,
}
impl UplinkLayout {
    pub const MAX_CHANNELS: usize = MAX_DATA_CHANNELS;
    pub fn control_only() -> Self {
        Self {
            channels: [None; MAX_DATA_CHANNELS],
            count: 0,
        }
    }
    // TS 25.213 4.3.1.2.1 gives C(SF,SF/4) on I for a single DPDCH.
    pub fn single(sf: u16) -> Result<Self, DspError> {
        if !(4..=256).contains(&sf) || !sf.is_power_of_two() {
            return Err(DspError::InvalidCode);
        }
        let mut result = Self::control_only();
        result.channels[0] = Some(DataChannel {
            code: OvsfCode::new(sf, sf / 4)?,
            branch: UplinkBranch::I,
        });
        result.count = 1;
        Ok(result)
    }
    // TS 25.213 Figure 1A and 4.3.1.2.1 alternate I/Q at SF4 with code pairs 1, 3 and 2.
    pub fn multicode(count: usize) -> Result<Self, DspError> {
        const CODE_PAIRS: [u16; 3] = [1, 3, 2];
        if !(2..=MAX_DATA_CHANNELS).contains(&count) {
            return Err(DspError::InvalidCode);
        }
        let mut result = Self::control_only();
        for (index, channel) in result.channels[..count].iter_mut().enumerate() {
            *channel = Some(DataChannel {
                code: OvsfCode::new(MULTICODE_SF, CODE_PAIRS[index / 2])?,
                branch: if index.is_multiple_of(2) {
                    UplinkBranch::I
                } else {
                    UplinkBranch::Q
                },
            });
        }
        result.count = count;
        Ok(result)
    }
    pub fn len(self) -> usize {
        self.count
    }
    pub fn is_empty(self) -> bool {
        self.count == 0
    }
    pub fn channel(self, index: usize) -> Option<DataChannel> {
        self.channels.get(index).copied().flatten()
    }
}

pub struct UplinkDpchCode {
    code: UplinkScramblingCode,
    scrambling: Vec<Complex32>,
}
impl UplinkDpchCode {
    pub fn new(code: UplinkScramblingCode) -> Result<Self, DspError> {
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        // TS 25.213 4.3.2.4 uses a 38400-chip segment, not free-running continuation of the 25-bit generators.
        UplinkLongSequence::new(code, 0)?.write(&mut scrambling);
        Ok(Self { code, scrambling })
    }
    pub fn code(&self) -> UplinkScramblingCode {
        self.code
    }
    pub(crate) fn scrambling(&self) -> &[Complex32] {
        &self.scrambling
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UplinkControl {
    pub pilot: PilotEstimate,
    pub tfci: Option<[f64; 2]>,
    pub fbi: Option<f64>,
    pub tpc: f64,
    pub carrier: Option<CarrierObservation>,
}

pub struct UplinkDpchReceiver {
    code: Arc<UplinkDpchCode>,
    format: UplinkControlFormat,
    data_codes: [Vec<f32>; DATA_CODE_COUNT],
    samples: Vec<Complex64>,
    controls: [UplinkControl; SLOTS_PER_FRAME as usize],
    start: Option<ChipInstant>,
    slots: usize,
    first_slot: usize,
    tracking: Option<PilotCarrier>,
}
impl UplinkDpchReceiver {
    pub fn sample_storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.samples.len() * std::mem::size_of::<Complex64>()
            + self.code.scrambling.len() * std::mem::size_of::<Complex32>()
            + self
                .data_codes
                .iter()
                .map(|code| code.len() * std::mem::size_of::<f32>())
                .sum::<usize>()
    }
    pub fn new(code: Arc<UplinkDpchCode>, format: UplinkControlFormat) -> Result<Self, DspError> {
        let mut data_codes = std::array::from_fn(|_| Vec::new());
        for (index, values) in data_codes.iter_mut().enumerate() {
            let (sf, number) = if index < SINGLE_DATA_SFS.len() {
                let sf = SINGLE_DATA_SFS[index];
                (sf, sf / 4)
            } else {
                (
                    MULTICODE_SF,
                    if index == SINGLE_DATA_SFS.len() { 3 } else { 2 },
                )
            };
            values.resize(usize::from(sf), 0.0);
            OvsfCode::new(sf, number)?.write(values)?;
        }
        Ok(Self {
            code,
            format,
            data_codes,
            samples: vec![Complex64::default(); CHIPS_PER_FRAME as usize],
            controls: [UplinkControl::default(); SLOTS_PER_FRAME as usize],
            start: None,
            slots: 0,
            first_slot: 0,
            tracking: None,
        })
    }
    pub fn with_tracking(
        code: Arc<UplinkDpchCode>,
        format: UplinkControlFormat,
        settings: CarrierTrackingSettings,
    ) -> Result<Self, DspError> {
        let mut receiver = Self::new(code, format)?;
        receiver.tracking = Some(PilotCarrier::new(settings)?);
        Ok(receiver)
    }
    pub fn format(&self) -> UplinkControlFormat {
        self.format
    }
    pub fn restore_confirmed_carrier(
        &mut self,
        offset: crate::carrier::CarrierOffsetHz,
    ) -> Result<(), DspError> {
        if self.slots != SLOTS_PER_FRAME as usize {
            return Err(DspError::DiscontinuousTime);
        }
        self.tracking
            .as_mut()
            .ok_or(DspError::DiscontinuousTime)?
            .restore_confirmed_offset(offset)
    }

    pub fn code(&self) -> &Arc<UplinkDpchCode> {
        &self.code
    }

    pub fn begin_frame(&mut self, start: ChipInstant) -> Result<(), DspError> {
        self.begin_frame_with_format(start, self.format)
    }

    pub fn begin_frame_with_format(
        &mut self,
        start: ChipInstant,
        format: UplinkControlFormat,
    ) -> Result<(), DspError> {
        self.begin_frame_suffix(start, format, 0)
    }

    /// Begin at an acquired slot within a known radio frame. Only real suffix
    /// slots may be received. A missing prefix never produces a complete frame.
    /// This establishes neither timing acquisition nor transport admission.
    pub fn begin_frame_suffix(
        &mut self,
        start: ChipInstant,
        format: UplinkControlFormat,
        first_slot: u8,
    ) -> Result<(), DspError> {
        if u64::from(first_slot) >= SLOTS_PER_FRAME {
            return Err(DspError::InvalidLength);
        }
        start
            .index
            .checked_add(CHIPS_PER_FRAME)
            .ok_or(DspError::InvalidLength)?;
        if self.start.is_some_and(|previous| {
            previous.epoch != start.epoch || start.index < previous.index + CHIPS_PER_FRAME
        }) {
            return Err(DspError::DiscontinuousTime);
        }
        if let Some(tracking) = &mut self.tracking {
            tracking.begin(ChipInstant {
                index: start.index + u64::from(first_slot) * CHIPS_PER_SLOT,
                ..start
            })?;
        }
        self.start = Some(start);
        self.first_slot = usize::from(first_slot);
        self.slots = self.first_slot;
        self.format = format;
        Ok(())
    }

    // Input is one acquired, matched-filtered, chip-aligned local DPCH slot. Without tracking, CFO correction is external.
    pub fn receive(
        &mut self,
        time: ChipInstant,
        input: &[Complex32],
    ) -> Result<UplinkControl, DspError> {
        let start = self.start.ok_or(DspError::DiscontinuousTime)?;
        let expected = start
            .index
            .checked_add(self.slots as u64 * CHIPS_PER_SLOT)
            .ok_or(DspError::InvalidLength)?;
        if time.epoch != start.epoch
            || time.index != expected
            || self.slots >= SLOTS_PER_FRAME as usize
        {
            return Err(DspError::DiscontinuousTime);
        }
        if input.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        if input
            .iter()
            .any(|value| !value.re.is_finite() || !value.im.is_finite())
        {
            return Err(DspError::NonFiniteEvidence);
        }
        let first = self.slots * CHIPS_PER_SLOT as usize;
        let samples = &mut self.samples[first..first + input.len()];
        for ((sample, input), code) in samples
            .iter_mut()
            .zip(input)
            .zip(&self.code.scrambling[first..first + input.len()])
        {
            *sample = Complex64::new(f64::from(input.re), f64::from(input.im))
                * Complex64::new(f64::from(code.re), -f64::from(code.im))
                * 0.5;
        }
        if let Some(tracking) = &mut self.tracking {
            tracking.correct(time, samples)?;
        }
        // TS 25.213 Figure 1A places C(256,0) on Q.
        let control: [Complex64; CONTROL_BITS] = std::array::from_fn(|index| {
            let value = samples[index * CONTROL_SF..(index + 1) * CONTROL_SF]
                .iter()
                .sum::<Complex64>()
                / CONTROL_SF as f64;
            Complex64::new(value.im, -value.re)
        });
        let count = self.format.pilots().bits();
        let word = self
            .format
            .pilots()
            .word(self.slots as u8)
            .expect("validated slot");
        let pilots: [Complex64; 8] = std::array::from_fn(|index| {
            if index < count {
                control[index]
                    * if word & (1 << (count - 1 - index)) == 0 {
                        1.0
                    } else {
                        -1.0
                    }
            } else {
                Complex64::default()
            }
        });
        let channel = pilots[..count].iter().sum::<Complex64>() / count as f64;
        let pilot = PilotEstimate {
            channel,
            observed_power: pilots[..count]
                .iter()
                .map(|value| value.norm_sqr())
                .sum::<f64>()
                / count as f64,
            residual_power: pilots[..count]
                .iter()
                .map(|value| (*value - channel).norm_sqr())
                .sum::<f64>()
                / count as f64,
        };
        let project = |value: Complex64| (value * channel.conj()).re;
        let tfci = (self.format.tfci_bits() != 0)
            .then(|| [project(control[count]), project(control[count + 1])]);
        let fbi_at = count + self.format.tfci_bits();
        let fbi = (self.format.fbi_bits() != 0).then(|| project(control[fbi_at]));
        let tpc_at = fbi_at + self.format.fbi_bits();
        let tpc = control[tpc_at..]
            .iter()
            .map(|value| project(*value))
            .sum::<f64>()
            / self.format.tpc_bits() as f64;
        let result = UplinkControl {
            pilot,
            tfci,
            fbi,
            tpc,
            carrier: self
                .tracking
                .as_mut()
                .map(|tracking| tracking.update(&pilots[..count]))
                .transpose()?,
        };
        self.controls[self.slots] = result;
        self.slots += 1;
        Ok(result)
    }

    pub fn frame(&self) -> Option<UplinkFrame<'_>> {
        (self.first_slot == 0 && self.slots == SLOTS_PER_FRAME as usize)
            .then_some(UplinkFrame { receiver: self })
    }

    pub fn last_slot(&self) -> Option<UplinkSlot<'_>> {
        if self.slots == self.first_slot {
            return None;
        }
        self.slots.checked_sub(1).map(|slot| UplinkSlot {
            receiver: self,
            slot,
        })
    }
}

pub struct UplinkSlot<'a> {
    receiver: &'a UplinkDpchReceiver,
    slot: usize,
}

impl UplinkSlot<'_> {
    pub fn start(&self) -> ChipInstant {
        let start = self.receiver.start.expect("received slot");
        ChipInstant {
            index: start.index + self.slot as u64 * CHIPS_PER_SLOT,
            ..start
        }
    }

    pub fn control(&self) -> UplinkControl {
        self.receiver.controls[self.slot]
    }

    pub fn pilot_count(&self) -> usize {
        self.receiver.format.pilots().bits()
    }

    pub fn code(&self) -> &Arc<UplinkDpchCode> {
        &self.receiver.code
    }

    /// Samples have already been descrambled and carrier-corrected using this UE's DPCCH tracker.
    pub fn corrected_samples(&self) -> &[Complex64] {
        let first = self.slot * CHIPS_PER_SLOT as usize;
        &self.receiver.samples[first..first + CHIPS_PER_SLOT as usize]
    }
}

pub struct UplinkFrame<'a> {
    receiver: &'a UplinkDpchReceiver,
}
impl UplinkFrame<'_> {
    pub fn decode_tfci_reliable(
        &self,
        decoder: &TfciDecoder,
        settings: LikelihoodSettings,
    ) -> Result<Option<TfciDecision>, DspError> {
        let Some(soft) = self.tfci_likelihoods(settings)? else {
            return Ok(None);
        };
        let mut normalized = [0.0; UPLINK_CODE_BITS];
        normalize_soft_bits(&soft, &mut normalized)?;
        decoder.decode(&normalized)
    }

    pub fn tfci_likelihoods(
        &self,
        settings: LikelihoodSettings,
    ) -> Result<Option<[f64; UPLINK_CODE_BITS]>, DspError> {
        if self.receiver.format.tfci_bits() == 0 {
            return Ok(None);
        }
        let mut soft = [0.0; UPLINK_CODE_BITS];
        for (slot, control) in self.receiver.controls.iter().enumerate() {
            let statistics =
                DpcchStatistics::new(control.pilot, self.receiver.format.pilots().bits())?;
            let values = control.tfci.expect("configured TFCI");
            for (bit, value) in values.into_iter().enumerate() {
                let start = slot * CHIPS_PER_SLOT as usize
                    + (self.receiver.format.pilots().bits() + bit) * CONTROL_SF;
                let local =
                    control_symbol_disturbance(&self.receiver.samples[start..start + CONTROL_SF])
                        / CONTROL_VARIANCE_RATIO_99[self.receiver.format.pilots().bits() - 5];
                let mut statistics = statistics;
                statistics.disturbance_chip_power = statistics
                    .disturbance_chip_power
                    .max(local * CONTROL_SF as f64);
                soft[2 * slot + bit] = statistics.llr(value, 1.0, CONTROL_SF, settings);
            }
        }
        Ok(Some(soft))
    }
    pub fn despread_llrs(
        &self,
        layout: UplinkLayout,
        channel: usize,
        gains: UplinkGains,
        settings: LikelihoodSettings,
        output: &mut [f64],
    ) -> Result<(), DspError> {
        let allocation = layout.channel(channel).ok_or(DspError::InvalidCode)?;
        let mut statistics = [DpcchStatistics::default(); SLOTS_PER_FRAME as usize];
        for (statistics, control) in statistics.iter_mut().zip(&self.receiver.controls) {
            *statistics =
                DpcchStatistics::new(control.pilot, self.receiver.format.pilots().bits())?;
        }
        self.despread_data(layout, channel, output)?;
        for (slot, output) in output
            .chunks_exact_mut(allocation.bits_per_slot())
            .enumerate()
        {
            for value in output {
                *value = statistics[slot].llr(
                    *value,
                    gains.ratio(),
                    usize::from(allocation.code.spreading_factor()),
                    settings,
                );
            }
        }
        Ok(())
    }
    pub fn start(&self) -> ChipInstant {
        self.receiver.start.expect("completed frame")
    }
    pub fn controls(&self) -> &[UplinkControl; SLOTS_PER_FRAME as usize] {
        &self.receiver.controls
    }
    pub fn decode_tfci(&self, decoder: &TfciDecoder) -> Result<Option<TfciDecision>, DspError> {
        if self.receiver.format.tfci_bits() == 0 {
            return Ok(None);
        }
        let mut soft = [0.0; UPLINK_CODE_BITS];
        for (slot, control) in self.receiver.controls.iter().enumerate() {
            soft[slot * 2..slot * 2 + 2].copy_from_slice(&control.tfci.expect("configured TFCI"));
        }
        let mut normalized = [0.0; UPLINK_CODE_BITS];
        normalize_soft_bits(&soft, &mut normalized)?;
        decoder.decode(&normalized)
    }

    // Mean despread projections retain beta_c, beta_d, SF and noise scaling for the transport consumer.
    pub fn despread_data(
        &self,
        layout: UplinkLayout,
        channel: usize,
        output: &mut [f64],
    ) -> Result<(), DspError> {
        let allocation = layout.channel(channel).ok_or(DspError::InvalidCode)?;
        if output.len() != allocation.bits_per_slot() * SLOTS_PER_FRAME as usize {
            return Err(DspError::InvalidLength);
        }
        let sf = allocation.code.spreading_factor();
        let index = if sf != MULTICODE_SF || allocation.code.index() == 1 {
            (256_u16.ilog2() - sf.ilog2()) as usize
        } else if allocation.code.index() == 3 {
            SINGLE_DATA_SFS.len()
        } else {
            SINGLE_DATA_SFS.len() + 1
        };
        let code = &self.receiver.data_codes[index];
        for (symbol, (input, output)) in self
            .receiver
            .samples
            .chunks_exact(code.len())
            .zip(output)
            .enumerate()
        {
            let mut value = input
                .iter()
                .zip(code)
                .map(|(value, code)| value * f64::from(*code))
                .sum::<Complex64>()
                / code.len() as f64;
            if allocation.branch == UplinkBranch::Q {
                value = Complex64::new(value.im, -value.re);
            }
            *output = (value
                * self.receiver.controls[symbol / allocation.bits_per_slot()]
                    .pilot
                    .channel
                    .conj())
            .re;
        }
        Ok(())
    }
}
