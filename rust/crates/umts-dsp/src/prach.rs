// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    channel_codes::OvsfCode,
    scrambling::PrimaryScramblingCode,
    uplink_scrambling::{UplinkLongSequence, UplinkScramblingCode, preamble_real},
};
use num_complex::{Complex32, Complex64};
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SECOND};

pub const PREAMBLE_CHIPS: usize = 4096;
const SIGNATURE_CHIPS: usize = 16;
const CODE_COUNT: u16 = 8192;
const CODES_PER_CELL: u16 = 16;
const MESSAGE_OFFSET: u32 = 4096;
const CONTROL_SF: u16 = 256;
const MIN_DATA_SF: u16 = 32;
const PROBE_BLOCK_CHIPS: usize = 256;
const PROBE_BLOCKS: usize = PREAMBLE_CHIPS / PROBE_BLOCK_CHIPS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreambleSignature(u8);

impl PreambleSignature {
    pub fn new(number: u8) -> Result<Self, DspError> {
        if usize::from(number) >= SIGNATURE_CHIPS {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(number))
    }
    pub fn number(self) -> u8 {
        self.0
    }
    // TS 25.213 Table 3 uses natural Hadamard order, not reversed OVSF indices.
    pub fn chips(self) -> [f32; SIGNATURE_CHIPS] {
        std::array::from_fn(|index| {
            if (index & usize::from(self.0)).count_ones().is_multiple_of(2) {
                1.0
            } else {
                -1.0
            }
        })
    }
    // TS 25.213 4.3.1.3 places data/control in different branches below signature s.
    pub fn message_codes(self, data_sf: u16) -> Result<(OvsfCode, OvsfCode), DspError> {
        if !(MIN_DATA_SF..=CONTROL_SF).contains(&data_sf) || !data_sf.is_power_of_two() {
            return Err(DspError::InvalidCode);
        }
        let data = OvsfCode::new(data_sf, data_sf * u16::from(self.0) / CODES_PER_CELL)?;
        let control = OvsfCode::new(
            CONTROL_SF,
            CODES_PER_CELL * u16::from(self.0) + CODES_PER_CELL - 1,
        )?;
        Ok((data, control))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrachCode(u16);

impl PrachCode {
    pub fn new(number: u16) -> Result<Self, DspError> {
        if number >= CODE_COUNT {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(number))
    }
    pub fn for_cell(primary: PrimaryScramblingCode, word: u8) -> Result<Self, DspError> {
        if u16::from(word) >= CODES_PER_CELL {
            return Err(DspError::InvalidCode);
        }
        Self::new(primary.index() * CODES_PER_CELL + u16::from(word))
    }
    pub fn number(self) -> u16 {
        self.0
    }

    // TS 25.213 4.3.3.1 multiplies the real Gold/signature product by exp(j(pi/4+k*pi/2)).
    pub fn write_preamble(
        self,
        signature: PreambleSignature,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if output.len() != PREAMBLE_CHIPS {
            return Err(DspError::InvalidLength);
        }
        let mut real = [0.0; PREAMBLE_CHIPS];
        preamble_real(UplinkScramblingCode::new(u32::from(self.0))?, &mut real);
        let signature = signature.chips();
        let scale = std::f32::consts::FRAC_1_SQRT_2;
        let rotation = [
            Complex32::new(scale, scale),
            Complex32::new(-scale, scale),
            Complex32::new(-scale, -scale),
            Complex32::new(scale, -scale),
        ];
        for (index, chip) in output.iter_mut().enumerate() {
            *chip = rotation[index % rotation.len()]
                * (real[index] * signature[index % SIGNATURE_CHIPS]);
        }
        Ok(())
    }

    pub fn message_sequence(self) -> Result<UplinkLongSequence, DspError> {
        UplinkLongSequence::new(
            UplinkScramblingCode::new(u32::from(self.0))?,
            MESSAGE_OFFSET,
        )
    }

    // TS 25.213 4.3.2.5 restarts the 38400-chip sequence at every message radio frame.
    // Serving paths must precompute chips or clone message_sequence(), not repeat jump setup per frame.
    pub fn write_message_scrambling(self, output: &mut [Complex32]) -> Result<(), DspError> {
        if output.len() != CHIPS_PER_FRAME as usize {
            return Err(DspError::InvalidLength);
        }
        self.message_sequence()?.write(output);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PreambleCorrelation {
    pub signature: PreambleSignature,
    pub amplitude: Complex64,
    pub coherent_fraction: f64,
    pub blockwise_fraction: f64,
    // Adjacent-block phase slope aliases outside +/-7500 Hz and is not a frequency-lock decision.
    pub phase_slope_hz: Option<f64>,
}

pub struct PreambleProbe {
    base: Vec<Complex32>,
    available: u16,
}

impl PreambleProbe {
    pub fn new(code: PrachCode, available: u16) -> Result<Self, DspError> {
        if available == 0 {
            return Err(DspError::InvalidCode);
        }
        let mut base = vec![Complex32::default(); PREAMBLE_CHIPS];
        code.write_preamble(PreambleSignature::new(0)?, &mut base)?;
        Ok(Self { base, available })
    }

    // Input is an aligned chip-rate preamble window after receive filtering and timing selection.
    // Fractions are normalized correlations, not SIR or a detection/admission threshold.
    pub fn correlate(
        &self,
        input: &[Complex32],
    ) -> Result<[Option<PreambleCorrelation>; SIGNATURE_CHIPS], DspError> {
        if input.len() != PREAMBLE_CHIPS {
            return Err(DspError::InvalidLength);
        }
        self.correlate_span(input, 0)
    }

    // A partial capture may contain only whole 256-chip blocks at a known preamble offset.
    pub fn correlate_span(
        &self,
        input: &[Complex32],
        preamble_offset: usize,
    ) -> Result<[Option<PreambleCorrelation>; SIGNATURE_CHIPS], DspError> {
        if input.is_empty()
            || !input.len().is_multiple_of(PROBE_BLOCK_CHIPS)
            || preamble_offset
                .checked_add(input.len())
                .is_none_or(|end| end > PREAMBLE_CHIPS)
        {
            return Err(DspError::InvalidLength);
        }
        let block_count = input.len() / PROBE_BLOCK_CHIPS;
        let mut blocks = [[Complex64::default(); SIGNATURE_CHIPS]; PROBE_BLOCKS];
        let blocks = &mut blocks[..block_count];
        let mut energy = 0.0;
        for (block, output) in blocks.iter_mut().enumerate() {
            for (index, value) in input[block * PROBE_BLOCK_CHIPS..(block + 1) * PROBE_BLOCK_CHIPS]
                .iter()
                .enumerate()
            {
                if !value.re.is_finite() || !value.im.is_finite() {
                    return Err(DspError::NonFiniteEvidence);
                }
                let value = Complex64::new(f64::from(value.re), f64::from(value.im));
                energy += value.norm_sqr();
                let position = preamble_offset + block * PROBE_BLOCK_CHIPS + index;
                let base = self.base[position];
                output[position % SIGNATURE_CHIPS] +=
                    value * Complex64::new(f64::from(base.re), -f64::from(base.im));
            }
            let mut width = 1;
            while width < SIGNATURE_CHIPS {
                for pair in output.chunks_exact_mut(2 * width) {
                    for index in 0..width {
                        let a = pair[index];
                        let b = pair[index + width];
                        pair[index] = a + b;
                        pair[index + width] = a - b;
                    }
                }
                width *= 2;
            }
            for value in output {
                *value /= PROBE_BLOCK_CHIPS as f64;
            }
        }
        let power = energy / input.len() as f64;
        Ok(std::array::from_fn(|signature| {
            if self.available & (1 << signature) == 0 {
                return None;
            }
            let amplitude = blocks
                .iter()
                .map(|block| block[signature])
                .sum::<Complex64>()
                / block_count as f64;
            let block_power = blocks
                .iter()
                .map(|block| block[signature].norm_sqr())
                .sum::<f64>()
                / block_count as f64;
            let phase = blocks
                .windows(2)
                .map(|pair| pair[1][signature] * pair[0][signature].conj())
                .sum::<Complex64>();
            Some(PreambleCorrelation {
                signature: PreambleSignature(signature as u8),
                amplitude,
                coherent_fraction: if power > 0.0 {
                    amplitude.norm_sqr() / power
                } else {
                    0.0
                },
                blockwise_fraction: if power > 0.0 {
                    block_power / power
                } else {
                    0.0
                },
                phase_slope_hz: (phase.norm_sqr() > 0.0).then(|| {
                    phase.arg() * CHIPS_PER_SECOND as f64
                        / (std::f64::consts::TAU * PROBE_BLOCK_CHIPS as f64)
                }),
            })
        }))
    }
}

#[cfg(test)]
mod tests;
