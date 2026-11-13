// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_dsp::carrier_mixer::{CarrierMixConfig, CarrierMixer};
use umts_types::time::{CHIPS_PER_SECOND, SLOTS_PER_FRAME, SampleRate};

const CARRIER_BANDWIDTH_HZ: f64 = 5_000_000.0;

pub(super) struct DualCarrier {
    next: ChipInstant,
    code: umts_dsp::scrambling::PrimaryScramblingCode,
    common: CommonDownlink,
    shaper: PulseShaper,
    mixer: CarrierMixer,
    chips: Vec<Complex32>,
    primary_samples: Vec<Complex32>,
    secondary_samples: Vec<Complex32>,
}

impl DualCarrier {
    pub(super) fn new(
        origin: ChipInstant,
        span_chips: usize,
        code: umts_dsp::scrambling::PrimaryScramblingCode,
        mut amplitudes: CommonChannelAmplitudes,
        offsets_hz: [i64; 2],
        secondary_fraction: f64,
    ) -> Result<Self> {
        amplitudes.pccpch = 0.0;
        let rate = SamplesPerChip::Four;
        let samples = CHIPS_PER_SLOT as usize * rate.get();
        let sample_origin = SampleInstant {
            epoch: origin.epoch,
            index: origin
                .index
                .checked_mul(rate.get() as u64)
                .ok_or_else(|| anyhow!("dual-carrier sample origin overflow"))?,
        };
        let sample_rate = SampleRate::new(CHIPS_PER_SECOND * rate.get() as u64, 1)
            .map_err(|error| anyhow!("dual-carrier sample rate: {error:?}"))?;
        let configs = [
            CarrierMixConfig {
                offset_hz: offsets_hz[0] as f64,
                occupied_bandwidth_hz: CARRIER_BANDWIDTH_HZ,
                power_fraction: 1.0 - secondary_fraction,
            },
            CarrierMixConfig {
                offset_hz: offsets_hz[1] as f64,
                occupied_bandwidth_hz: CARRIER_BANDWIDTH_HZ,
                power_fraction: secondary_fraction,
            },
        ];
        Ok(Self {
            next: origin,
            code,
            common: CommonDownlink::new(code, amplitudes)
                .map_err(|error| anyhow!("secondary common channels: {error:?}"))?,
            shaper: PulseShaper::new(rate, span_chips)
                .map_err(|error| anyhow!("secondary pulse shaper: {error:?}"))?,
            mixer: CarrierMixer::new(sample_rate, sample_origin, &configs)
                .map_err(|error| anyhow!("dual-carrier mixer: {error:?}"))?,
            chips: vec![Complex32::default(); CHIPS_PER_SLOT as usize],
            primary_samples: vec![Complex32::default(); samples],
            secondary_samples: vec![Complex32::default(); samples],
        })
    }

    pub(super) fn mix(
        &mut self,
        start: ChipInstant,
        bank: Option<&mut crate::hsdpa_tx::TransmitBank>,
        output: &mut [Complex32],
    ) -> Result<(), CommonTxError> {
        if start != self.next || output.len() != self.primary_samples.len() {
            return Err(CommonTxError::Time);
        }
        let next = start
            .index
            .checked_add(CHIPS_PER_SLOT)
            .ok_or(CommonTxError::Time)?;
        let slot = (start.index / CHIPS_PER_SLOT) % SLOTS_PER_FRAME;
        self.common.compose_slot(
            &[Bit::Zero; umts_dsp::broadcast::FRAME_BITS],
            slot as usize,
            &mut self.chips,
        )?;
        if let Some(bank) = bank {
            if bank.primary_code() != self.code {
                bank.isolate();
            }
            bank.preflight_slot(start, None);
            bank.mix_slot(start, &mut self.chips)
                .map_err(|_| CommonTxError::Conflict)?;
        }
        self.shaper
            .process(&self.chips, &mut self.secondary_samples)?;
        self.primary_samples.copy_from_slice(output);
        self.mixer.mix(
            SampleInstant {
                epoch: start.epoch,
                index: start
                    .index
                    .checked_mul(SamplesPerChip::Four.get() as u64)
                    .ok_or(CommonTxError::Time)?,
            },
            &[&self.primary_samples, &self.secondary_samples],
            output,
        )?;
        self.next.index = next;
        Ok(())
    }
}

impl CommonTx {
    pub(crate) fn has_carrier(&self, code: umts_dsp::scrambling::PrimaryScramblingCode) -> bool {
        code == self.information.primary_code()
            || self
                .carriers
                .as_ref()
                .is_some_and(|carrier| carrier.code == code)
    }

    pub(crate) fn enable_secondary_carrier(
        &mut self,
        span_chips: usize,
        code: umts_dsp::scrambling::PrimaryScramblingCode,
        amplitudes: CommonChannelAmplitudes,
        offsets_hz: [i64; 2],
        secondary_fraction: f64,
    ) -> Result<()> {
        if self.generated
            || self.rate != SamplesPerChip::Four.get()
            || self.carriers.is_some()
            || self.crest_reducer.is_some()
            || code == self.information.primary_code()
            || !secondary_fraction.is_finite()
            || !(0.0..1.0).contains(&secondary_fraction)
            || secondary_fraction == 0.0
        {
            bail!("secondary carrier requires an unstarted four-sample-per-chip transmitter");
        }
        self.carriers = Some(DualCarrier::new(
            self.next,
            span_chips,
            code,
            amplitudes,
            offsets_hz,
            secondary_fraction,
        )?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;
    use umts_dsp::scrambling::PrimaryScramblingCode;

    #[test]
    fn secondary_has_no_bch_and_rejects_gaps_before_advancing_filters() {
        let amplitudes = CommonChannelAmplitudes {
            cpich: 0.1,
            pccpch: 0.8,
            primary_sch: 0.05,
            secondary_sch: 0.05,
        };
        let make = |pccpch| {
            DualCarrier::new(
                at(0),
                8,
                PrimaryScramblingCode::new(470).unwrap(),
                CommonChannelAmplitudes {
                    pccpch,
                    ..amplitudes
                },
                [-2_500_000, 2_500_000],
                0.5,
            )
            .unwrap()
        };
        let mut carrier = make(0.8);
        let mut reference = make(0.0);
        let samples = CHIPS_PER_SLOT as usize * SamplesPerChip::Four.get();
        let mut actual = vec![Complex32::default(); samples];
        let mut expected = actual.clone();
        assert!(carrier.mix(at(CHIPS_PER_SLOT), None, &mut actual).is_err());
        for slot in 0..3 {
            actual.fill(Complex32::default());
            expected.fill(Complex32::default());
            carrier
                .mix(at(slot * CHIPS_PER_SLOT), None, &mut actual)
                .unwrap();
            reference
                .mix(at(slot * CHIPS_PER_SLOT), None, &mut expected)
                .unwrap();
            assert_eq!(actual, expected);
            assert!(actual.iter().any(|sample| sample.norm_sqr() > 0.0));
        }
    }
}
