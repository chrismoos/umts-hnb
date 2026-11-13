// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, ChipInstant, SampleInstant};

use crate::{
    DspError,
    bits::Bit,
    broadcast::{BroadcastEncoder, FRAME_BITS, PAYLOAD_BITS, TTI_BITS},
    common_downlink::{CommonChannelAmplitudes, CommonDownlink},
    pulse_shaping::{PulseShaper, SamplesPerChip},
    scrambling::PrimaryScramblingCode,
};

const BCH_FRAMES: u64 = 2;
const BCH_CHIPS: u64 = BCH_FRAMES * CHIPS_PER_FRAME;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaveformWindow {
    pub first_sample: SampleInstant,
    pub next_sample: SampleInstant,
    // Causal filter delay separates the input frame origin from its output reference time.
    pub chip_origin_sample: SampleInstant,
}

pub struct BroadcastWaveform {
    next: ChipInstant,
    samples_per_chip: usize,
    encoder: BroadcastEncoder,
    common: CommonDownlink,
    shaper: PulseShaper,
    chips: Vec<Complex32>,
}

impl BroadcastWaveform {
    pub fn new(
        code: PrimaryScramblingCode,
        amplitudes: CommonChannelAmplitudes,
        rate: SamplesPerChip,
        span_chips: usize,
        origin: ChipInstant,
    ) -> Result<Self, DspError> {
        // TS 25.211 7.1 figure 29 aligns the two-frame P-CCPCH cycle to even SFNs.
        if !origin.index.is_multiple_of(BCH_CHIPS) {
            return Err(DspError::DiscontinuousTime);
        }
        origin
            .index
            .checked_mul(rate.get() as u64)
            .ok_or(DspError::InvalidLength)?;
        Ok(Self {
            next: origin,
            samples_per_chip: rate.get(),
            encoder: BroadcastEncoder::new()?,
            common: CommonDownlink::new(code, amplitudes)?,
            shaper: PulseShaper::new(rate, span_chips)?,
            chips: vec![Complex32::default(); CHIPS_PER_FRAME as usize],
        })
    }

    pub fn samples_per_tti(&self) -> usize {
        BCH_CHIPS as usize * self.samples_per_chip
    }

    pub fn next_chip(&self) -> ChipInstant {
        self.next
    }

    pub fn generate(
        &mut self,
        start: ChipInstant,
        payload: &[Bit; PAYLOAD_BITS],
        output: &mut [Complex32],
    ) -> Result<WaveformWindow, DspError> {
        if start != self.next {
            return Err(DspError::DiscontinuousTime);
        }
        if output.len() != self.samples_per_tti() {
            return Err(DspError::InvalidLength);
        }
        let next_chip = start
            .index
            .checked_add(BCH_CHIPS)
            .ok_or(DspError::InvalidLength)?;
        let sample_index = |chip: u64| {
            chip.checked_mul(self.samples_per_chip as u64)
                .ok_or(DspError::InvalidLength)
        };
        let first_index = sample_index(start.index)?;
        let next_index = sample_index(next_chip)?;
        let origin_index = first_index
            .checked_add(self.shaper.group_delay_samples() as u64)
            .ok_or(DspError::InvalidLength)?;
        let mut coded = [Bit::Zero; TTI_BITS];
        self.encoder.encode(payload, &mut coded)?;
        let frame_samples = output.len() / BCH_FRAMES as usize;
        for (frame, output) in coded
            .chunks_exact(FRAME_BITS)
            .zip(output.chunks_exact_mut(frame_samples))
        {
            self.common.compose_frame(
                frame.try_into().map_err(|_| DspError::InvalidLength)?,
                &mut self.chips,
            )?;
            self.shaper.process(&self.chips, output)?;
        }
        self.next.index = next_chip;
        let instant = |index| SampleInstant {
            epoch: start.epoch,
            index,
        };
        Ok(WaveformWindow {
            first_sample: instant(first_index),
            next_sample: instant(next_index),
            chip_origin_sample: instant(origin_index),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use umts_types::{RadioEpoch, time::SYSTEM_FRAME_MODULUS};

    use super::*;

    fn origin() -> ChipInstant {
        ChipInstant {
            epoch: RadioEpoch::new(NonZeroU64::new(1).unwrap()),
            index: (SYSTEM_FRAME_MODULUS - 2) * CHIPS_PER_FRAME,
        }
    }

    fn producer() -> BroadcastWaveform {
        BroadcastWaveform::new(
            PrimaryScramblingCode::new(17).unwrap(),
            CommonChannelAmplitudes {
                cpich: 0.15,
                pccpch: 0.2,
                primary_sch: 0.1,
                secondary_sch: 0.1,
            },
            SamplesPerChip::Two,
            32,
            origin(),
        )
        .unwrap()
    }

    #[test]
    fn continuous_broadcast_crosses_sfn_wrap_without_reset_or_sample_gaps() {
        let mut waveform = producer();
        let mut samples = vec![Complex32::default(); waveform.samples_per_tti()];
        let first = waveform
            .generate(origin(), &[Bit::One; PAYLOAD_BITS], &mut samples)
            .unwrap();
        assert_eq!(first.first_sample.index, origin().index * 2);
        assert_eq!(
            first.chip_origin_sample.index,
            first.first_sample.index + 32
        );
        assert_eq!(waveform.next_chip().system_frame(), 0);
        let boundary_tail = samples[samples.len() - 1];
        let second = waveform
            .generate(
                waveform.next_chip(),
                &[Bit::Zero; PAYLOAD_BITS],
                &mut samples,
            )
            .unwrap();
        assert_eq!(second.first_sample, first.next_sample);
        assert_eq!(waveform.next_chip().system_frame(), 2);
        assert!(
            samples
                .iter()
                .all(|value| value.re.is_finite() && value.im.is_finite())
        );
        assert_ne!(samples[0], Complex32::default());
        assert!(boundary_tail.norm() > 0.0);
    }

    #[test]
    fn rejected_time_or_length_does_not_advance_waveform_state() {
        let mut tested = producer();
        let mut reference = producer();
        let mut wrong = origin();
        wrong.index += CHIPS_PER_FRAME;
        let mut samples = vec![Complex32::default(); tested.samples_per_tti()];
        assert_eq!(
            tested.generate(wrong, &[Bit::Zero; PAYLOAD_BITS], &mut samples),
            Err(DspError::DiscontinuousTime)
        );
        assert_eq!(
            tested.generate(origin(), &[Bit::Zero; PAYLOAD_BITS], &mut []),
            Err(DspError::InvalidLength)
        );
        wrong = origin();
        wrong.epoch = RadioEpoch::new(NonZeroU64::new(2).unwrap());
        assert_eq!(
            tested.generate(wrong, &[Bit::Zero; PAYLOAD_BITS], &mut samples),
            Err(DspError::DiscontinuousTime)
        );
        let mut expected = samples.clone();
        tested
            .generate(origin(), &[Bit::Zero; PAYLOAD_BITS], &mut samples)
            .unwrap();
        reference
            .generate(origin(), &[Bit::Zero; PAYLOAD_BITS], &mut expected)
            .unwrap();
        assert_eq!(samples, expected);
        assert_eq!(
            tested.generate(origin(), &[Bit::Zero; PAYLOAD_BITS], &mut samples),
            Err(DspError::DiscontinuousTime)
        );
    }
}
