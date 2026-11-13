// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::DspError;
use std::num::NonZeroUsize;
use umts_types::hsupa::CodeLayout;

const DENOMINATOR: f64 = 15.0;
const MAX_HARQ_OFFSET_DB: u8 = 6;
const MAX_CHANNELS: usize = 4;
// TS 25.213 Tables 1B.1 and 1B.2, without enhanced E-DPCCH boosting.
const DATA_NUMERATORS: [u16; 30] = [
    5, 6, 7, 8, 9, 11, 12, 13, 15, 17, 19, 21, 24, 27, 30, 34, 38, 42, 47, 53, 60, 67, 75, 84, 95,
    106, 119, 134, 150, 168,
];
const CONTROL_NUMERATORS: [u8; 9] = [5, 6, 8, 9, 12, 15, 19, 24, 30];

// TS 25.213 Table 1B applies to non-compressed, unboosted E-DPCCH.
pub fn control_gain_ratio(signaled: u8) -> Result<f64, DspError> {
    CONTROL_NUMERATORS
        .get(usize::from(signaled))
        .map(|value| f64::from(*value) / DENOMINATOR)
        .ok_or(DspError::InvalidAmplitude)
}

#[derive(Clone, Copy, Debug)]
pub struct ChannelGains {
    values: [f64; MAX_CHANNELS],
    count: usize,
}

impl ChannelGains {
    pub fn as_slice(&self) -> &[f64] {
        &self.values[..self.count]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SingleReferencePower {
    reference_bits: NonZeroUsize,
    equivalent_channels: u8,
    ratio: f64,
}

impl SingleReferencePower {
    pub fn new(
        reference_bits: NonZeroUsize,
        reference_layout: CodeLayout,
        signaled: u8,
    ) -> Result<Self, DspError> {
        let numerator = DATA_NUMERATORS
            .get(usize::from(signaled))
            .ok_or(DspError::InvalidAmplitude)?;
        Ok(Self {
            reference_bits,
            equivalent_channels: equivalent_channels(reference_layout),
            ratio: f64::from(*numerator) / DENOMINATOR,
        })
    }

    // TS 25.214 5.1.2.5B.2.3 extrapolates power, then floors each amplitude to Table 1B.2.
    pub fn gains(
        self,
        bits: NonZeroUsize,
        layout: CodeLayout,
        harq_offset_db: u8,
    ) -> Result<ChannelGains, DspError> {
        if harq_offset_db > MAX_HARQ_OFFSET_DB {
            return Err(DspError::InvalidAmplitude);
        }
        let ratio = self.ratio
            * (f64::from(self.equivalent_channels) / f64::from(equivalent_channels(layout))
                * bits.get() as f64
                / self.reference_bits.get() as f64)
                .sqrt()
            * 10.0_f64.powf(f64::from(harq_offset_db) / 20.0);
        if !ratio.is_finite() {
            return Err(DspError::InvalidAmplitude);
        }
        let mut result = ChannelGains {
            values: [0.0; MAX_CHANNELS],
            count: layout.spreading_factors().len(),
        };
        for (value, sf) in result.values.iter_mut().zip(layout.spreading_factors()) {
            let amplitude = if *sf == 2 {
                ratio * std::f64::consts::SQRT_2
            } else {
                ratio
            };
            *value = quantize(amplitude);
        }
        Ok(result)
    }
}

fn equivalent_channels(layout: CodeLayout) -> u8 {
    layout
        .spreading_factors()
        .iter()
        .map(|sf| if *sf == 2 { 2 } else { 1 })
        .sum()
}

fn quantize(amplitude: f64) -> f64 {
    DATA_NUMERATORS
        .iter()
        .rev()
        .copied()
        .find(|value| f64::from(*value) / DENOMINATOR <= amplitude)
        .map_or(f64::from(DATA_NUMERATORS[0]), f64::from)
        / DENOMINATOR
}

#[cfg(test)]
mod tests {
    use super::*;
    fn nz(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }

    #[test]
    fn reference_and_control_tables_preserve_wire_indices() {
        let layout = CodeLayout::new(&[4]).unwrap();
        for (index, numerator) in DATA_NUMERATORS.iter().enumerate() {
            let power = SingleReferencePower::new(nz(100), layout, index as u8).unwrap();
            assert_eq!(
                power.gains(nz(100), layout, 0).unwrap().as_slice(),
                &[f64::from(*numerator) / DENOMINATOR]
            );
        }
        assert_eq!(control_gain_ratio(0).unwrap(), 5.0 / 15.0);
        assert_eq!(control_gain_ratio(5).unwrap(), 1.0);
        assert_eq!(control_gain_ratio(8).unwrap(), 2.0);
        assert!(control_gain_ratio(9).is_err());
        assert!(SingleReferencePower::new(nz(100), layout, 30).is_err());
    }

    #[test]
    fn extrapolation_counts_sf2_twice_and_quantizes_downward() {
        let reference = CodeLayout::new(&[4]).unwrap();
        let power = SingleReferencePower::new(nz(100), reference, 8).unwrap();
        let target = CodeLayout::new(&[2, 2, 4, 4]).unwrap();
        assert_eq!(
            power.gains(nz(600), target, 0).unwrap().as_slice(),
            &[21.0 / 15.0, 21.0 / 15.0, 1.0, 1.0]
        );
        assert_eq!(
            power.gains(nz(400), reference, 0).unwrap().as_slice(),
            &[2.0]
        );
        assert_eq!(
            power.gains(nz(100), reference, 6).unwrap().as_slice(),
            &[27.0 / 15.0]
        );
        assert!(power.gains(nz(100), reference, 7).is_err());
        assert_eq!(quantize(0.0), 5.0 / 15.0);
        assert_eq!(quantize(1000.0), 168.0 / 15.0);
    }
}
