// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use num_complex::Complex64;
use umts_types::time::{CHIPS_PER_SLOT, ChipInstant};

use crate::{
    DspError,
    channel_codes::OvsfCode,
    dedicated::{DpcchStatistics, LikelihoodSettings, UplinkDpchCode, UplinkSlot},
    pilots::PilotEstimate,
};

const CONTROL_SF: usize = 256;
const CONTROL_SYMBOLS: usize = CHIPS_PER_SLOT as usize / CONTROL_SF;

pub(super) struct ControlSlotEvidence {
    pub llrs: [f64; CONTROL_SYMBOLS],
    pub presence: Option<([f64; CONTROL_SYMBOLS], f64)>,
}

struct Channel {
    signs: Vec<f32>,
    quadrature: bool,
}

pub(super) struct SlotEvidence<'a> {
    pub code: &'a Arc<UplinkDpchCode>,
    pub start: ChipInstant,
    pub samples: &'a [Complex64],
    pub pilot: PilotEstimate,
    pub pilot_count: usize,
}

pub struct UplinkChannels {
    scrambling: Arc<UplinkDpchCode>,
    origin: ChipInstant,
    control: [f32; CONTROL_SF],
    data: Vec<Channel>,
}

impl UplinkChannels {
    // The common dedicated receiver accounts for the shared scrambling code.
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.data.capacity() * std::mem::size_of::<Channel>()
            + self
                .data
                .iter()
                .map(|channel| channel.signs.capacity() * std::mem::size_of::<f32>())
                .sum::<usize>()
    }
    pub fn new(
        scrambling: Arc<UplinkDpchCode>,
        origin: ChipInstant,
        maximum_dpdch: u8,
        hsdpa_configured: bool,
        spreading_factors: &[u16],
    ) -> Result<Self, DspError> {
        let legal = match spreading_factors {
            [sf] => (4..=256).contains(sf) && sf.is_power_of_two(),
            [4, 4] | [2, 2] => true,
            [2, 2, 4, 4] => maximum_dpdch == 0,
            _ => false,
        };
        if !legal || maximum_dpdch > 1 {
            return Err(DspError::InvalidCode);
        }
        let mut control = [0.0; CONTROL_SF];
        OvsfCode::new(CONTROL_SF as u16, 1)?.write(&mut control)?;
        let mut data = Vec::with_capacity(spreading_factors.len());
        for (channel, &sf) in spreading_factors.iter().enumerate() {
            // TS 25.213 Tables 1C and 1E depend on configured DPDCH count, not its activity.
            let index = if maximum_dpdch == 1 {
                sf / 2
            } else {
                (sf / 4).max(1)
            };
            let quadrature = (channel % 2 == 1) ^ (maximum_dpdch == 1 && !hsdpa_configured);
            let mut signs = vec![0.0; usize::from(sf)];
            OvsfCode::new(sf, index)?.write(&mut signs)?;
            data.push(Channel { signs, quadrature });
        }
        Ok(Self {
            scrambling,
            origin,
            control,
            data,
        })
    }

    fn validate(&self, slot: &UplinkSlot<'_>) -> Result<(), DspError> {
        self.validate_at(slot.code(), slot.start())
    }

    pub(super) fn validate_at(
        &self,
        code: &Arc<UplinkDpchCode>,
        start: ChipInstant,
    ) -> Result<(), DspError> {
        if !Arc::ptr_eq(code, &self.scrambling)
            || start.epoch != self.origin.epoch
            || start
                .index
                .checked_sub(self.origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(CHIPS_PER_SLOT))
        {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(())
    }

    pub fn data_symbols_per_slot(&self, channel: usize) -> Option<usize> {
        self.data
            .get(channel)
            .map(|channel| CHIPS_PER_SLOT as usize / channel.signs.len())
    }

    // UplinkSlot already shares this UE's matched filtering, descrambling and DPCCH carrier tracking.
    // Complex symbols retain residual channel phase for the common pilot likelihood estimator.
    pub fn despread_control(
        &self,
        slot: &UplinkSlot<'_>,
    ) -> Result<[Complex64; CONTROL_SYMBOLS], DspError> {
        self.validate(slot)?;
        let mut output = [Complex64::default(); CONTROL_SYMBOLS];
        despread(slot.corrected_samples(), &self.control, false, &mut output)?;
        Ok(output)
    }

    pub fn despread_data(
        &self,
        slot: &UplinkSlot<'_>,
        channel: usize,
        output: &mut [Complex64],
    ) -> Result<(), DspError> {
        self.validate(slot)?;
        let channel = self.data.get(channel).ok_or(DspError::InvalidCode)?;
        despread(
            slot.corrected_samples(),
            &channel.signs,
            channel.quadrature,
            output,
        )
    }

    pub(super) fn control_symbols_from_samples(
        &self,
        samples: &[Complex64],
    ) -> Result<[Complex64; CONTROL_SYMBOLS], DspError> {
        let mut symbols = [Complex64::default(); CONTROL_SYMBOLS];
        despread(samples, &self.control, false, &mut symbols)?;
        Ok(symbols)
    }

    pub fn control_llrs(
        &self,
        slot: &UplinkSlot<'_>,
        gain_ratio: f64,
        settings: LikelihoodSettings,
    ) -> Result<[f64; CONTROL_SYMBOLS], DspError> {
        Ok(self.control_slot_evidence(slot, gain_ratio, settings)?.llrs)
    }

    pub(super) fn control_slot_evidence(
        &self,
        slot: &UplinkSlot<'_>,
        gain_ratio: f64,
        settings: LikelihoodSettings,
    ) -> Result<ControlSlotEvidence, DspError> {
        let symbols = self.despread_control(slot)?;
        let mut output = [0.0; CONTROL_SYMBOLS];
        project_llrs(
            &symbols,
            slot.control().pilot,
            slot.pilot_count(),
            CONTROL_SF,
            gain_ratio,
            settings,
            &mut output,
        )?;
        let presence = control_presence(
            &symbols,
            slot.control().pilot,
            slot.pilot_count(),
            gain_ratio,
        )?;
        Ok(ControlSlotEvidence {
            llrs: output,
            presence,
        })
    }

    // Scratch and output are caller-owned so slot processing never allocates.
    pub fn data_llrs(
        &self,
        slot: &UplinkSlot<'_>,
        channel: usize,
        gain_ratio: f64,
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: &mut [f64],
    ) -> Result<(), DspError> {
        self.data_evidence_llrs(
            &SlotEvidence {
                code: slot.code(),
                start: slot.start(),
                samples: slot.corrected_samples(),
                pilot: slot.control().pilot,
                pilot_count: slot.pilot_count(),
            },
            channel,
            gain_ratio,
            settings,
            scratch,
            output,
        )
    }

    pub(super) fn data_evidence_llrs(
        &self,
        slot: &SlotEvidence<'_>,
        channel: usize,
        gain_ratio: f64,
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: &mut [f64],
    ) -> Result<(), DspError> {
        self.data_evidence_symbols(slot, channel, scratch)?;
        let channel = self.data.get(channel).ok_or(DspError::InvalidCode)?;
        project_llrs(
            scratch,
            slot.pilot,
            slot.pilot_count,
            channel.signs.len(),
            gain_ratio,
            settings,
            output,
        )
    }

    // Adjacent E-DPDCH I/Q channels share one OVSF sum. Each channel's orientation is projected
    // separately so per-channel gains and pilot reliability are preserved.
    pub(super) fn data_pair_evidence_llrs(
        &self,
        slot: &SlotEvidence<'_>,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.data_pair_evidence::<false, false>(
            slot,
            first_channel,
            gain_ratios,
            settings,
            scratch,
            output,
        )
    }

    pub(super) fn data_pair_evidence_tracked_llrs(
        &self,
        slot: &SlotEvidence<'_>,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.data_pair_evidence::<true, false>(
            slot,
            first_channel,
            gain_ratios,
            settings,
            scratch,
            output,
        )
    }

    pub(super) fn data_pair_evidence_calibrated_llrs(
        &self,
        slot: &SlotEvidence<'_>,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.data_pair_evidence::<true, true>(
            slot,
            first_channel,
            gain_ratios,
            settings,
            scratch,
            output,
        )
    }

    fn data_pair_evidence<const TRACK_PHASE: bool, const TRACK_GAIN: bool>(
        &self,
        slot: &SlotEvidence<'_>,
        first_channel: usize,
        gain_ratios: [f64; 2],
        settings: LikelihoodSettings,
        scratch: &mut [Complex64],
        output: [&mut [f64]; 2],
    ) -> Result<(), DspError> {
        self.validate_at(slot.code, slot.start)?;
        let second_channel = first_channel.checked_add(1).ok_or(DspError::InvalidCode)?;
        let first = self.data.get(first_channel).ok_or(DspError::InvalidCode)?;
        let second = self.data.get(second_channel).ok_or(DspError::InvalidCode)?;
        if !first_channel.is_multiple_of(2)
            || first.signs != second.signs
            || first.quadrature == second.quadrature
        {
            return Err(DspError::InvalidCode);
        }
        despread(slot.samples, &first.signs, first.quadrature, scratch)?;
        let pilot = if TRACK_PHASE && gain_ratios[0] == gain_ratios[1] {
            paired_data_pilot::<TRACK_GAIN>(scratch, slot.pilot, slot.pilot_count, gain_ratios[0])
        } else {
            slot.pilot
        };
        let [first_output, second_output] = output;
        project_llrs(
            scratch,
            pilot,
            slot.pilot_count,
            first.signs.len(),
            gain_ratios[0],
            settings,
            first_output,
        )?;
        for value in scratch.iter_mut() {
            // Inverse rotation covers maxDPDCH=1 without HSDPA, whose pair is Q/I.
            *value = if first.quadrature {
                Complex64::new(-value.im, value.re)
            } else {
                Complex64::new(value.im, -value.re)
            };
        }
        project_llrs(
            scratch,
            pilot,
            slot.pilot_count,
            second.signs.len(),
            gain_ratios[1],
            settings,
            second_output,
        )
    }

    pub(super) fn data_evidence_symbols(
        &self,
        slot: &SlotEvidence<'_>,
        channel: usize,
        scratch: &mut [Complex64],
    ) -> Result<(), DspError> {
        self.validate_at(slot.code, slot.start)?;
        let channel = self.data.get(channel).ok_or(DspError::InvalidCode)?;
        despread(slot.samples, &channel.signs, channel.quadrature, scratch)
    }

    pub(super) fn data_reference_chips(
        &self,
        slot: &SlotEvidence<'_>,
        gain_ratios: &[f64],
        scratch: &mut [Complex64],
        reference: &mut [Complex64],
    ) -> Result<(), DspError> {
        self.validate_at(slot.code, slot.start)?;
        if gain_ratios.len() != self.data.len() || reference.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        if gain_ratios
            .iter()
            .any(|gain| !gain.is_finite() || *gain < 0.0)
        {
            return Err(DspError::InvalidAmplitude);
        }
        reference.fill(Complex64::default());
        let mut index = 0;
        while index < self.data.len() {
            let channel = &self.data[index];
            let sf = channel.signs.len();
            let symbols = scratch
                .get_mut(..CHIPS_PER_SLOT as usize / sf)
                .ok_or(DspError::InvalidLength)?;
            let paired = index.is_multiple_of(2)
                && self.data.get(index + 1).is_some_and(|next| {
                    next.signs == channel.signs && next.quadrature != channel.quadrature
                });
            despread(slot.samples, &channel.signs, false, symbols)?;
            let pilot = if paired && gain_ratios[index] == gain_ratios[index + 1] {
                paired_data_pilot::<true>(symbols, slot.pilot, slot.pilot_count, gain_ratios[index])
            } else {
                slot.pilot
            };
            let members = if paired { 2 } else { 1 };
            let span = index..index + members;
            for (channel, &gain) in self.data[span.clone()].iter().zip(&gain_ratios[span]) {
                let amplitude = if channel.quadrature {
                    pilot.channel * Complex64::new(0.0, gain)
                } else {
                    pilot.channel * gain
                };
                for (symbol, chips) in symbols.iter().zip(reference.chunks_exact_mut(sf)) {
                    let aligned = *symbol * pilot.channel.conj();
                    let projection = if channel.quadrature {
                        aligned.im
                    } else {
                        aligned.re
                    };
                    let value = if projection < 0.0 {
                        -amplitude
                    } else {
                        amplitude
                    };
                    for (chip, sign) in chips.iter_mut().zip(&channel.signs) {
                        *chip += value * f64::from(*sign);
                    }
                }
            }
            index += members;
        }
        Ok(())
    }
}

// Paired equal-gain E-DPDCHs form QPSK. Their fourth moment removes data signs.
// The pilot selects the quadrant. Sample and pilot phase variances determine
// the blend. Low fourth-moment evidence leaves the pilot unchanged.
fn paired_data_pilot<const TRACK_GAIN: bool>(
    symbols: &[Complex64],
    pilot: PilotEstimate,
    pilot_count: usize,
    gain: f64,
) -> PilotEstimate {
    let Some(channel_norm) = (gain.is_finite() && gain > 0.0)
        .then(|| pilot.channel.norm())
        .filter(|value| value.is_finite() && *value > 0.0)
    else {
        return pilot;
    };
    let Ok(statistics) = DpcchStatistics::new(pilot, pilot_count) else {
        return pilot;
    };
    let pilot_variance = statistics.channel_error_power / (2.0 * channel_norm * channel_norm);
    let scale = 1.0 / (gain * channel_norm);
    if !pilot_variance.is_finite()
        || pilot_variance <= 0.0
        || !scale.is_finite()
        || scale <= 0.0
        || symbols.len() < 2
    {
        return pilot;
    }
    let unit = pilot.channel / channel_norm;
    let mut absolute_sum = 0.0;
    let mut absolute_squared_sum = 0.0;
    let mut moment = Complex64::default();
    let mut energy = 0.0;
    let mut second_moment = Complex64::default();
    let mut second_energy = 0.0;
    for symbol in symbols {
        let value = symbol * scale;
        if TRACK_GAIN {
            let aligned = value * unit.conj();
            let absolute = aligned.re.abs() + aligned.im.abs();
            absolute_sum += absolute;
            absolute_squared_sum += absolute * absolute;
        }
        let squared = value * value;
        let fourth = squared * squared;
        second_moment += squared;
        second_energy += squared.norm_sqr();
        moment += fourth;
        energy += fourth.norm_sqr();
    }
    let power = moment.norm_sqr();
    // Require concentration beyond a few individual high-energy symbols.
    if !power.is_finite()
        || !energy.is_finite()
        || power <= 25.0 * energy
        || second_moment.norm_sqr() > 25.0 * second_energy
    {
        return pilot;
    }
    let count = symbols.len() as f64;
    let data_variance = (count * energy - power).max(0.0) / (32.0 * (count - 1.0) * power);
    let total_variance = pilot_variance + data_variance;
    if !total_variance.is_finite() || total_variance <= 0.0 {
        return pilot;
    }
    let squared = unit.conj() * unit.conj();
    let relative = -moment * squared * squared;
    let phase = 0.25 * relative.arg() * (pilot_variance / total_variance);
    let (sin, cos) = phase.sin_cos();
    let amplitude = if TRACK_GAIN {
        // |I|+|Q| = 2 A cos(phase), independently of its data signs
        let estimate = absolute_sum / (2.0 * count * cos);
        let sample_variance = (absolute_squared_sum - absolute_sum * absolute_sum / count).max(0.0)
            / (4.0 * cos * cos * count * (count - 1.0));
        let phase_sensitivity = estimate * sin / cos;
        let data_amplitude_variance =
            sample_variance + phase_sensitivity * phase_sensitivity * data_variance;
        let variance = pilot_variance + data_amplitude_variance;
        if !estimate.is_finite() || !variance.is_finite() || variance <= 0.0 {
            return pilot;
        }
        let difference = estimate - 1.0;
        // Retain the nominal gain inside the combined two-standard-error band.
        // This is an estimator confidence rule, not an RF false-alarm guarantee.
        if difference * difference <= 4.0 * variance {
            1.0
        } else {
            1.0 + difference * (pilot_variance / variance)
        }
    } else {
        1.0
    };
    if phase == 0.0 && amplitude == 1.0 {
        return pilot;
    }
    PilotEstimate {
        channel: pilot.channel * (Complex64::new(cos, sin) * amplitude),
        ..pilot
    }
}

fn control_presence(
    symbols: &[Complex64; CONTROL_SYMBOLS],
    pilot: PilotEstimate,
    pilot_count: usize,
    gain_ratio: f64,
) -> Result<Option<([f64; CONTROL_SYMBOLS], f64)>, DspError> {
    let statistics = DpcchStatistics::new(pilot, pilot_count)?;
    let variance = statistics.disturbance_chip_power / CONTROL_SF as f64
        + gain_ratio * gain_ratio * statistics.channel_error_power;
    if !variance.is_finite() || variance <= 0.0 {
        return Ok(None);
    }
    let correlation =
        symbols.map(|symbol| 2.0 * gain_ratio * (symbol * pilot.channel.conj()).re / variance);
    let expected =
        CONTROL_SYMBOLS as f64 * gain_ratio * gain_ratio * pilot.channel.norm_sqr() / variance;
    Ok(
        (correlation.iter().all(|value| value.is_finite()) && expected.is_finite())
            .then_some((correlation, expected)),
    )
}

fn project_llrs(
    symbols: &[Complex64],
    pilot: PilotEstimate,
    pilot_count: usize,
    sf: usize,
    gain_ratio: f64,
    settings: LikelihoodSettings,
    output: &mut [f64],
) -> Result<(), DspError> {
    if symbols.len() != output.len() {
        return Err(DspError::InvalidLength);
    }
    if !gain_ratio.is_finite() || gain_ratio < 0.0 {
        return Err(DspError::InvalidAmplitude);
    }
    let statistics = DpcchStatistics::new(pilot, pilot_count)?;
    for (symbol, output) in symbols.iter().zip(output) {
        let projection = (*symbol * pilot.channel.conj()).re;
        if !projection.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        *output = statistics.llr(projection, gain_ratio, sf, settings);
        if !output.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
    }
    Ok(())
}

fn despread(
    samples: &[Complex64],
    signs: &[f32],
    quadrature: bool,
    output: &mut [Complex64],
) -> Result<(), DspError> {
    if samples.len() != CHIPS_PER_SLOT as usize || output.len() != samples.len() / signs.len() {
        return Err(DspError::InvalidLength);
    }
    for (chips, symbol) in samples.chunks_exact(signs.len()).zip(output) {
        let value = chips
            .iter()
            .zip(signs)
            .map(|(sample, sign)| *sample * f64::from(*sign))
            .sum::<Complex64>()
            / signs.len() as f64;
        if !value.re.is_finite() || !value.im.is_finite() {
            return Err(DspError::NonFiniteEvidence);
        }
        *symbol = if quadrature {
            Complex64::new(value.im, -value.re)
        } else {
            value
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_likelihood_distinguishes_known_signal_from_silence_without_gain_dependence() {
        let mut reference: Option<f64> = None;
        for amplitude in [0.001, 1.0, 1000.0] {
            let channel = Complex64::from_polar(amplitude, 0.7);
            let pilot = PilotEstimate {
                channel,
                observed_power: amplitude * amplitude * 1.1,
                residual_power: amplitude * amplitude * 0.1,
            };
            let (correlation, expected) =
                control_presence(&[channel * 0.7; CONTROL_SYMBOLS], pilot, 6, 0.7)
                    .unwrap()
                    .unwrap();
            let likelihood = correlation.iter().sum::<f64>() - expected;
            assert!(likelihood > 0.0);
            if let Some(reference) = reference {
                assert!((likelihood - reference).abs() < 1e-10);
            }
            reference = Some(likelihood);
            let (silent, penalty) =
                control_presence(&[Complex64::default(); CONTROL_SYMBOLS], pilot, 6, 0.7)
                    .unwrap()
                    .unwrap();
            assert!(silent.iter().sum::<f64>() - penalty < 0.0);
            let noiseless = PilotEstimate {
                residual_power: 0.0,
                ..pilot
            };
            assert!(
                control_presence(&[channel; CONTROL_SYMBOLS], noiseless, 6, 0.7)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn pilot_projection_preserves_phase_and_common_amplitude_invariance() {
        let settings = LikelihoodSettings::new(100.0).unwrap();
        let mut reference: Option<f64> = None;
        for amplitude in [0.001, 1.0, 1000.0] {
            for phase in [0.0_f64, 0.7, 2.3] {
                let channel = Complex64::from_polar(amplitude, phase);
                let pilot = PilotEstimate {
                    channel,
                    observed_power: amplitude * amplitude * 1.1,
                    residual_power: amplitude * amplitude * 0.1,
                };
                let symbols = [channel * 0.5, channel * -0.5];
                let mut output = [0.0; 2];
                project_llrs(&symbols, pilot, 6, 16, 0.5, settings, &mut output).unwrap();
                assert!(output[0] > 0.0 && output[1] < 0.0);
                if let Some(value) = reference {
                    assert!((output[0] - value).abs() < 1e-10);
                } else {
                    reference = Some(output[0]);
                }
            }
        }
    }

    #[test]
    fn paired_despreading_preserves_scalar_llr_and_symbol_bits_in_both_orientations() {
        use crate::uplink_scrambling::UplinkScramblingCode;
        use std::num::NonZeroU64;
        use umts_types::RadioEpoch;

        let epoch = RadioEpoch::new(NonZeroU64::MIN);
        let at = |index| ChipInstant { epoch, index };
        let code =
            Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(12345).unwrap()).unwrap());
        let settings = LikelihoodSettings::new(64.0).unwrap();
        let pilot = PilotEstimate {
            channel: Complex64::new(0.75, -0.25),
            observed_power: 0.75,
            residual_power: 0.125,
        };
        for layout in [&[4, 4][..], &[2, 2][..], &[2, 2, 4, 4][..]] {
            for maximum_dpdch in [0, 1] {
                if layout.len() == 4 && maximum_dpdch != 0 {
                    continue;
                }
                for hsdpa in [false, true] {
                    let channels =
                        UplinkChannels::new(code.clone(), at(0), maximum_dpdch, hsdpa, layout)
                            .unwrap();
                    for scale in [0.0_f64, 1e-30, 0.1, 1e30] {
                        let samples: Vec<_> = (0..CHIPS_PER_SLOT as usize)
                            .map(|index| {
                                let zero = if index.is_multiple_of(2) { 0.0 } else { -0.0 };
                                if index.is_multiple_of(17) {
                                    Complex64::new(zero, -zero)
                                } else {
                                    Complex64::new(
                                        (index * 7 % 19) as f64 - 9.0,
                                        (index * 11 % 23) as f64 - 11.0,
                                    ) * scale
                                }
                            })
                            .collect();
                        let slot = SlotEvidence {
                            code: &code,
                            start: at(CHIPS_PER_SLOT),
                            samples: &samples,
                            pilot,
                            pilot_count: 6,
                        };
                        for first in (0..layout.len()).step_by(2) {
                            let count = channels.data_symbols_per_slot(first).unwrap();
                            for gains in [[0.0, 1.0], [0.3, 0.7], [1e-6, 1e6]] {
                                let mut scalar_scratch = vec![Complex64::default(); count];
                                let mut pair_scratch = scalar_scratch.clone();
                                let mut first_expected = vec![0.0; count];
                                let mut second_expected = vec![0.0; count];
                                let mut first_actual = first_expected.clone();
                                let mut second_actual = second_expected.clone();
                                channels
                                    .data_evidence_llrs(
                                        &slot,
                                        first,
                                        gains[0],
                                        settings,
                                        &mut scalar_scratch,
                                        &mut first_expected,
                                    )
                                    .unwrap();
                                channels
                                    .data_evidence_llrs(
                                        &slot,
                                        first + 1,
                                        gains[1],
                                        settings,
                                        &mut scalar_scratch,
                                        &mut second_expected,
                                    )
                                    .unwrap();
                                channels
                                    .data_pair_evidence_llrs(
                                        &slot,
                                        first,
                                        gains,
                                        settings,
                                        &mut pair_scratch,
                                        [&mut first_actual, &mut second_actual],
                                    )
                                    .unwrap();
                                for (actual, expected) in first_actual
                                    .iter()
                                    .chain(&second_actual)
                                    .zip(first_expected.iter().chain(&second_expected))
                                {
                                    assert_eq!(
                                        actual.to_bits(),
                                        expected.to_bits(),
                                        "layout={layout:?} maximumDPDCH={maximum_dpdch} HSDPA={hsdpa} first={first} scale={scale}"
                                    );
                                }
                                for (actual, expected) in pair_scratch.iter().zip(&scalar_scratch) {
                                    assert_eq!(actual.re.to_bits(), expected.re.to_bits());
                                    assert_eq!(actual.im.to_bits(), expected.im.to_bits());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn paired_despreading_rejects_invalid_gain_length_nonfinite_and_channel_evidence() {
        use crate::uplink_scrambling::UplinkScramblingCode;
        use std::num::NonZeroU64;
        use umts_types::RadioEpoch;

        let epoch = RadioEpoch::new(NonZeroU64::MIN);
        let at = |index| ChipInstant { epoch, index };
        let code =
            Arc::new(UplinkDpchCode::new(UplinkScramblingCode::new(12345).unwrap()).unwrap());
        let channels = UplinkChannels::new(code.clone(), at(0), 0, true, &[2, 2]).unwrap();
        let single = UplinkChannels::new(code.clone(), at(0), 0, true, &[4]).unwrap();
        let settings = LikelihoodSettings::new(64.0).unwrap();
        let pilot = PilotEstimate {
            channel: Complex64::new(0.75, -0.25),
            observed_power: 0.75,
            residual_power: 0.125,
        };
        let mut samples = vec![Complex64::new(0.2, -0.3); CHIPS_PER_SLOT as usize];
        let count = channels.data_symbols_per_slot(0).unwrap();
        let mut scratch = vec![Complex64::default(); count];
        let mut first = vec![0.0; count];
        let mut second = vec![0.0; count];
        let slot = SlotEvidence {
            code: &code,
            start: at(0),
            samples: &samples,
            pilot,
            pilot_count: 6,
        };
        for invalid in [-1.0, f64::NAN, f64::INFINITY] {
            for gains in [[invalid, 1.0], [1.0, invalid]] {
                assert_eq!(
                    channels.data_pair_evidence_llrs(
                        &slot,
                        0,
                        gains,
                        settings,
                        &mut scratch,
                        [&mut first, &mut second]
                    ),
                    Err(DspError::InvalidAmplitude)
                );
            }
        }
        assert_eq!(
            channels.data_pair_evidence_llrs(
                &slot,
                0,
                [1.0; 2],
                settings,
                &mut scratch[..count - 1],
                [&mut first, &mut second]
            ),
            Err(DspError::InvalidLength)
        );
        assert_eq!(
            channels.data_pair_evidence_llrs(
                &slot,
                0,
                [1.0; 2],
                settings,
                &mut scratch,
                [&mut first[..count - 1], &mut second]
            ),
            Err(DspError::InvalidLength)
        );
        assert_eq!(
            channels.data_pair_evidence_llrs(
                &slot,
                0,
                [1.0; 2],
                settings,
                &mut scratch,
                [&mut first, &mut second[..count - 1]]
            ),
            Err(DspError::InvalidLength)
        );
        assert_eq!(
            channels.data_pair_evidence_llrs(
                &slot,
                1,
                [1.0; 2],
                settings,
                &mut scratch,
                [&mut first, &mut second]
            ),
            Err(DspError::InvalidCode)
        );
        assert_eq!(
            single.data_pair_evidence_llrs(
                &slot,
                0,
                [1.0; 2],
                settings,
                &mut scratch,
                [&mut first, &mut second]
            ),
            Err(DspError::InvalidCode)
        );
        samples[0].re = f64::NAN;
        let slot = SlotEvidence {
            code: &code,
            start: at(0),
            samples: &samples,
            pilot,
            pilot_count: 6,
        };
        assert_eq!(
            channels.data_pair_evidence_llrs(
                &slot,
                0,
                [1.0; 2],
                settings,
                &mut scratch,
                [&mut first, &mut second]
            ),
            Err(DspError::NonFiniteEvidence)
        );
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;

    #[test]
    fn paired_phase_estimate_is_gain_invariant_and_pilot_quadrant_anchored() {
        for amplitude in [1e-30, 0.001, 1.0, 1000.0, 1e30] {
            for gain in [0.01, 1.0, 11.2] {
                for degrees in [-30.0_f64, -16.0, 0.0, 16.0, 30.0] {
                    let channel = Complex64::from_polar(amplitude, 0.73);
                    let pilot = PilotEstimate {
                        channel,
                        observed_power: amplitude * amplitude * 1.1,
                        residual_power: amplitude * amplitude * 0.1,
                    };
                    let actual_channel = channel * Complex64::from_polar(1.0, degrees.to_radians());
                    let symbols: Vec<_> = (0..1280)
                        .map(|n| {
                            let i = if n % 2 == 0 { 1.0 } else { -1.0 };
                            let q = if n % 4 < 2 { 1.0 } else { -1.0 };
                            actual_channel * gain * Complex64::new(i, q)
                        })
                        .collect();
                    let corrected = paired_data_pilot::<false>(&symbols, pilot, 6, gain);
                    assert!(
                        (corrected.channel / actual_channel - Complex64::new(1.0, 0.0)).norm()
                            < 1e-7,
                        "amplitude={amplitude} gain={gain} phase={degrees}"
                    );
                    assert_eq!(
                        corrected.observed_power.to_bits(),
                        pilot.observed_power.to_bits()
                    );
                    assert_eq!(
                        corrected.residual_power.to_bits(),
                        pilot.residual_power.to_bits()
                    );
                }
            }
        }
    }

    #[test]
    fn trusted_pilots_and_non_qpsk_evidence_remain_unchanged() {
        let pilot = PilotEstimate {
            channel: Complex64::from_polar(1.0, 0.7),
            observed_power: 1.1,
            residual_power: 0.1,
        };
        let at_phase = |symbols: Vec<Complex64>, pilot: PilotEstimate| {
            let actual = paired_data_pilot::<false>(&symbols, pilot, 6, 1.0);
            assert_eq!(actual.channel.re.to_bits(), pilot.channel.re.to_bits());
            assert_eq!(actual.channel.im.to_bits(), pilot.channel.im.to_bits());
        };
        at_phase(vec![Complex64::default(); 640], pilot);
        at_phase(vec![pilot.channel; 640], pilot);
        at_phase(
            (0..640)
                .map(|n| pilot.channel * if n % 2 == 0 { 1.0 } else { -1.0 })
                .collect(),
            pilot,
        );
        at_phase(
            (0..640)
                .map(|n| Complex64::from_polar(1.0, n as f64 * 0.731))
                .collect(),
            pilot,
        );
        let mut impulse = vec![Complex64::default(); 640];
        impulse[13] = Complex64::new(1e150, 1e150);
        at_phase(impulse, pilot);
        let exact = PilotEstimate {
            residual_power: 0.0,
            ..pilot
        };
        at_phase(
            (0..640)
                .map(|n| {
                    pilot.channel
                        * Complex64::new(
                            if n % 2 == 0 { 1.0 } else { -1.0 },
                            if n % 4 < 2 { 1.0 } else { -1.0 },
                        )
                })
                .collect(),
            exact,
        );
    }

    #[test]
    fn noisy_qpsk_refines_weak_pilot_phase_without_data_signs() {
        let channel = Complex64::from_polar(1.0, 0.7);
        let pilot = PilotEstimate {
            channel,
            observed_power: 1.2,
            residual_power: 0.2,
        };
        for count in [640, 1280] {
            let mut state = 123456789_u64;
            let mut uniform = || {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                ((state >> 32) as f64 / u32::MAX as f64) * 2.0 - 1.0
            };
            let actual = channel * Complex64::from_polar(1.0, 16.0_f64.to_radians());
            let symbols: Vec<_> = (0..count)
                .map(|_| {
                    let i = if uniform() < 0.0 { 1.0 } else { -1.0 };
                    let q = if uniform() < 0.0 { 1.0 } else { -1.0 };
                    actual * Complex64::new(i, q) + Complex64::new(uniform(), uniform()) * 0.5
                })
                .collect();
            let refined = paired_data_pilot::<false>(&symbols, pilot, 6, 1.0);
            assert!((refined.channel / actual).arg().abs() < 3.0_f64.to_radians());
        }
    }
}

#[cfg(test)]
mod amplitude_tests {
    use super::*;

    #[test]
    fn paired_amplitude_estimate_tracks_data_gain_with_common_scale_invariance() {
        for scale in [1e-30, 0.001, 1.0, 1000.0, 1e30] {
            for gain in [0.01, 1.0, 11.2] {
                for ratio in [0.2, 0.6, 1.0, 1.8] {
                    let channel = Complex64::from_polar(scale, 0.7);
                    let pilot = PilotEstimate {
                        channel,
                        observed_power: scale * scale * 1.2,
                        residual_power: scale * scale * 0.2,
                    };
                    let actual = channel * Complex64::from_polar(ratio, 16.0_f64.to_radians());
                    let symbols: Vec<_> = (0..1280)
                        .map(|n| {
                            actual
                                * gain
                                * Complex64::new(
                                    if n % 2 == 0 { 1.0 } else { -1.0 },
                                    if n % 4 < 2 { 1.0 } else { -1.0 },
                                )
                        })
                        .collect();
                    let refined = paired_data_pilot::<true>(&symbols, pilot, 6, gain);
                    assert!(
                        (refined.channel / actual - Complex64::new(1.0, 0.0)).norm() < 1e-7,
                        "scale={scale} gain={gain} ratio={ratio}"
                    );
                    assert_eq!(
                        refined.residual_power.to_bits(),
                        pilot.residual_power.to_bits()
                    );
                    assert_eq!(
                        refined.observed_power.to_bits(),
                        pilot.observed_power.to_bits()
                    );
                }
            }
        }
    }

    #[test]
    fn noisy_paired_amplitude_estimate_does_not_use_data_signs() {
        let pilot = PilotEstimate {
            channel: Complex64::from_polar(1.0, 0.7),
            observed_power: 1.2,
            residual_power: 0.2,
        };
        let actual = pilot.channel * Complex64::from_polar(0.6, 16.0_f64.to_radians());
        let mut state = 123456789_u64;
        let mut uniform = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 32) as f64 / u32::MAX as f64) * 2.0 - 1.0
        };
        let symbols: Vec<_> = (0..1280)
            .map(|_| {
                let i = if uniform() < 0.0 { 1.0 } else { -1.0 };
                let q = if uniform() < 0.0 { 1.0 } else { -1.0 };
                actual * Complex64::new(i, q) + Complex64::new(uniform(), uniform()) * 0.2
            })
            .collect();
        let refined = paired_data_pilot::<true>(&symbols, pilot, 6, 1.0);
        assert!((refined.channel / actual).arg().abs() < 3.0_f64.to_radians());
        assert!((refined.channel.norm() - actual.norm()).abs() < 0.05);
    }
}

#[cfg(test)]
mod gain_confidence_tests {
    use super::*;

    #[test]
    fn small_amplitude_difference_keeps_the_nominal_gain() {
        let pilot = PilotEstimate {
            channel: Complex64::from_polar(1.0, 0.7),
            observed_power: 1.2,
            residual_power: 0.2,
        };
        let symbols: Vec<_> = (0..1280)
            .map(|n| {
                pilot.channel
                    * Complex64::from_polar(0.9, 16.0_f64.to_radians())
                    * Complex64::new(
                        if n % 2 == 0 { 1.0 } else { -1.0 },
                        if n % 4 < 2 { 1.0 } else { -1.0 },
                    )
            })
            .collect();
        let refined = paired_data_pilot::<true>(&symbols, pilot, 6, 1.0);
        assert!((refined.channel.norm() - pilot.channel.norm()).abs() < 1e-12);
        assert!((refined.channel / pilot.channel).arg() > 10.0_f64.to_radians());
    }
}
