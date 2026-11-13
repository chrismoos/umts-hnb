// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, SLOTS_PER_FRAME};

use crate::{
    DspError,
    bits::TxBit,
    channel_codes::OvsfCode,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
    tfci::{CODEWORD_BITS, Tfci},
};

const MIN_SF: u16 = 4;
const MAX_SF: u16 = 256;
const TFCI_REPETITION_THRESHOLD: u16 = 128;
const SHORT_TFCI_BITS_PER_SLOT: usize = 2;
const LONG_TFCI_BITS_PER_SLOT: usize = 8;
const QPSK_BITS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecondaryFormat {
    spreading_factor: u16,
    tfci: bool,
}

impl SecondaryFormat {
    // TS 25.211 Table 18, QPSK formats without pilots.
    pub fn new(spreading_factor: u16, tfci: bool) -> Result<Self, DspError> {
        if !(MIN_SF..=MAX_SF).contains(&spreading_factor) || !spreading_factor.is_power_of_two() {
            return Err(DspError::InvalidCode);
        }
        Ok(Self {
            spreading_factor,
            tfci,
        })
    }

    pub fn spreading_factor(self) -> u16 {
        self.spreading_factor
    }

    pub fn control_bits_per_slot(self) -> usize {
        if !self.tfci && self.spreading_factor >= TFCI_REPETITION_THRESHOLD {
            0
        } else if self.spreading_factor >= TFCI_REPETITION_THRESHOLD {
            SHORT_TFCI_BITS_PER_SLOT
        } else {
            LONG_TFCI_BITS_PER_SLOT
        }
    }

    pub fn data_bits_per_slot(self) -> usize {
        let bits = CHIPS_PER_SLOT as usize / usize::from(self.spreading_factor) * QPSK_BITS;
        // Table 18 retains a DTX TFCI field at SF64 and below when TFCI is absent.
        bits - self.control_bits_per_slot()
    }

    pub fn data_bits_per_frame(self) -> usize {
        self.data_bits_per_slot() * SLOTS_PER_FRAME as usize
    }
}

pub struct SecondaryDownlink {
    format: SecondaryFormat,
    weights: Vec<Complex32>,
}

impl SecondaryDownlink {
    // The frame begins at the P-CCPCH origin. Nonzero S-CCPCH timing offsets are not accepted here.
    // Amplitude is the RMS complex-chip magnitude with both QPSK branches active.
    pub fn new(
        primary: PrimaryScramblingCode,
        channel: OvsfCode,
        tfci: bool,
        amplitude: f32,
    ) -> Result<Self, DspError> {
        let format = SecondaryFormat::new(channel.spreading_factor(), tfci)?;
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let mut weights = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(primary.sequence_number(), &mut weights)?;
        let mut code = vec![0.0; usize::from(channel.spreading_factor())];
        channel.write(&mut code)?;
        for (index, weight) in weights.iter_mut().enumerate() {
            *weight *= code[index % code.len()] * amplitude * 0.5;
        }
        Ok(Self { format, weights })
    }

    pub fn format(&self) -> SecondaryFormat {
        self.format
    }

    // TS 25.212 4.3.5.1 repeats b[k mod 32], not separate copies within each slot.
    pub fn add_frame(
        &self,
        data: &[TxBit],
        tfci: Option<Tfci>,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if data.len() != self.format.data_bits_per_frame() || output.len() != self.weights.len() {
            return Err(DspError::InvalidLength);
        }
        if tfci.is_some() != self.format.tfci {
            return Err(DspError::InvalidCode);
        }
        for slot in 0..SLOTS_PER_FRAME as usize {
            let start = slot * CHIPS_PER_SLOT as usize;
            self.add_slot(
                data,
                tfci,
                slot,
                &mut output[start..start + CHIPS_PER_SLOT as usize],
            )?;
        }
        Ok(())
    }

    pub fn add_slot(
        &self,
        data: &[TxBit],
        tfci: Option<Tfci>,
        slot: usize,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if slot >= SLOTS_PER_FRAME as usize
            || data.len() != self.format.data_bits_per_frame()
            || output.len() != CHIPS_PER_SLOT as usize
        {
            return Err(DspError::InvalidLength);
        }
        if tfci.is_some() != self.format.tfci {
            return Err(DspError::InvalidCode);
        }
        let control = tfci.map(Tfci::encode);
        let control_bits = self.format.control_bits_per_slot();
        let sf = usize::from(self.format.spreading_factor);
        let symbols_per_slot = CHIPS_PER_SLOT as usize / sf;
        for symbol in 0..symbols_per_slot {
            let real = |branch: usize| {
                let bit = symbol * QPSK_BITS + branch;
                if bit < control_bits {
                    control.as_ref().map_or(0.0, |word| {
                        TxBit::Data(word[(slot * control_bits + bit) % CODEWORD_BITS]).amplitude()
                    })
                } else {
                    data[slot * self.format.data_bits_per_slot() + bit - control_bits].amplitude()
                }
            };
            let value = Complex32::new(real(0), real(1));
            let start = symbol * sf;
            let frame_start = slot * CHIPS_PER_SLOT as usize + symbol * sf;
            for (chip, weight) in output[start..start + sf]
                .iter_mut()
                .zip(&self.weights[frame_start..frame_start + sf])
            {
                *chip += value * weight;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bits::Bit;

    #[test]
    fn all_no_pilot_qpsk_formats_match_table_eighteen() {
        for (sf, with, without) in [
            (256, 18, 20),
            (128, 38, 40),
            (64, 72, 72),
            (32, 152, 152),
            (16, 312, 312),
            (8, 632, 632),
            (4, 1272, 1272),
        ] {
            assert_eq!(
                SecondaryFormat::new(sf, true).unwrap().data_bits_per_slot(),
                with
            );
            assert_eq!(
                SecondaryFormat::new(sf, false)
                    .unwrap()
                    .data_bits_per_slot(),
                without
            );
        }
        for sf in [0, 1, 2, 3, 63, 512] {
            assert!(SecondaryFormat::new(sf, true).is_err());
        }
    }

    #[test]
    fn ordered_data_control_repetition_and_dtx_survive_despreading() {
        for sf in [256, 128, 64, 32, 16, 8, 4] {
            for with_tfci in [false, true] {
                let primary = PrimaryScramblingCode::new(469).unwrap();
                let channel = OvsfCode::new(sf, sf - 1).unwrap();
                let generator = SecondaryDownlink::new(primary, channel, with_tfci, 0.4).unwrap();
                let format = generator.format();
                let data: Vec<_> = (0..format.data_bits_per_frame())
                    .map(|i| {
                        if i % 7 == 0 {
                            TxBit::Dtx
                        } else {
                            TxBit::Data(Bit::from(i % 3 == 0))
                        }
                    })
                    .collect();
                let tfci = with_tfci.then(|| Tfci::new(0x2a5).unwrap());
                let mut output = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
                generator.add_frame(&data, tfci, &mut output).unwrap();
                let mut scramble = vec![Complex32::default(); output.len()];
                downlink_sequence(primary.sequence_number(), &mut scramble).unwrap();
                let mut code = vec![0.0; usize::from(sf)];
                channel.write(&mut code).unwrap();
                let mut data_index = 0;
                let mut control_index = 0;
                let expected_control = tfci.map(Tfci::encode);
                let control_bits = if sf < 128 {
                    8
                } else if with_tfci {
                    2
                } else {
                    0
                };
                for slot in 0..15 {
                    for symbol in 0..2560 / usize::from(sf) {
                        let start = slot * 2560 + symbol * usize::from(sf);
                        let recovered: Complex32 = (0..usize::from(sf))
                            .map(|chip| {
                                output[start + chip] * scramble[start + chip].conj() * code[chip]
                            })
                            .sum::<Complex32>()
                            / (f32::from(sf) * 0.4);
                        for (branch, value) in [recovered.re, recovered.im].into_iter().enumerate()
                        {
                            let expected = if symbol * 2 + branch < control_bits {
                                let value = expected_control.as_ref().map_or(0.0, |bits| {
                                    TxBit::Data(bits[control_index % 32]).amplitude()
                                });
                                control_index += 1;
                                value
                            } else {
                                let value = data[data_index].amplitude();
                                data_index += 1;
                                value
                            };
                            assert!(
                                (value - expected).abs() < 1e-5,
                                "SF {sf}, TFCI {with_tfci}, slot {slot}, symbol {symbol}, branch {branch}"
                            );
                        }
                    }
                }
                assert_eq!(data_index, data.len());
                assert_eq!(control_index, control_bits * 15);
            }
        }
    }

    #[test]
    fn idle_data_does_not_transmit_zero_bits_and_validation_precedes_output() {
        let generator = SecondaryDownlink::new(
            PrimaryScramblingCode::new(0).unwrap(),
            OvsfCode::new(64, 3).unwrap(),
            false,
            0.5,
        )
        .unwrap();
        let mut output = vec![Complex32::new(0.125, -0.25); CHIPS_PER_FRAME as usize];
        let saved = output.clone();
        let data = vec![TxBit::Dtx; generator.format().data_bits_per_frame()];
        generator.add_frame(&data, None, &mut output).unwrap();
        assert_eq!(output, saved);
        assert!(
            generator
                .add_frame(&data, Some(Tfci::new(0).unwrap()), &mut output)
                .is_err()
        );
        assert_eq!(output, saved);
        assert!(generator.add_frame(&data[..1], None, &mut output).is_err());
        assert_eq!(output, saved);
        assert!(
            SecondaryDownlink::new(
                PrimaryScramblingCode::new(0).unwrap(),
                OvsfCode::new(64, 3).unwrap(),
                true,
                f32::NAN
            )
            .is_err()
        );
    }

    #[test]
    fn slot_kernel_matches_frame_and_rejects_invalid_slots() {
        let generator = SecondaryDownlink::new(
            PrimaryScramblingCode::new(469).unwrap(),
            OvsfCode::new(16, 7).unwrap(),
            true,
            0.37,
        )
        .unwrap();
        let data: Vec<_> = (0..generator.format().data_bits_per_frame())
            .map(|index| {
                if index % 11 == 0 {
                    TxBit::Dtx
                } else {
                    TxBit::Data(Bit::from(index % 5 == 0))
                }
            })
            .collect();
        let tfci = Some(Tfci::new(0x155).unwrap());
        let mut frame = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        generator.add_frame(&data, tfci, &mut frame).unwrap();
        let mut slots = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        for slot in 0..SLOTS_PER_FRAME as usize {
            let start = slot * CHIPS_PER_SLOT as usize;
            generator
                .add_slot(
                    &data,
                    tfci,
                    slot,
                    &mut slots[start..start + CHIPS_PER_SLOT as usize],
                )
                .unwrap();
        }
        assert_eq!(slots, frame);
        let mut invalid_slot = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
        assert!(
            generator
                .add_slot(&data, tfci, SLOTS_PER_FRAME as usize, &mut invalid_slot)
                .is_err()
        );
        assert!(
            generator
                .add_slot(
                    &data,
                    None,
                    0,
                    &mut vec![Complex32::default(); CHIPS_PER_SLOT as usize]
                )
                .is_err()
        );
    }
}
