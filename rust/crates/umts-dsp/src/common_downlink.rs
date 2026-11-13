// SPDX-License-Identifier: AGPL-3.0-or-later

use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, SLOTS_PER_FRAME};

use crate::{
    DspError,
    bits::Bit,
    broadcast::FRAME_BITS,
    channel_codes::OvsfCode,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
    synchronization::{self, SYNCHRONIZATION_CHIPS},
};

const COMMON_SF: usize = 256;
const BITS_PER_SYMBOL: usize = 2;
const BCH_SYMBOLS_PER_SLOT: usize = 9;
// TS 25.211 5.3.3.5 requires a = -1 for the single-antenna, non-STTD P-CCPCH.
const SCH_NON_STTD_SYMBOL: f32 = -1.0;

#[derive(Clone, Copy, Debug)]
pub struct CommonChannelAmplitudes {
    pub cpich: f32,
    pub pccpch: f32,
    pub primary_sch: f32,
    pub secondary_sch: f32,
}

pub struct CommonDownlink {
    base: Vec<Complex32>,
    pccpch: Vec<Complex32>,
}

impl CommonDownlink {
    // Amplitudes are RMS complex-chip magnitudes during each channel's active chips.
    // SCH is not scrambled. Scrambling and symbol normalization each contribute 1/sqrt(2).
    pub fn new(
        code: PrimaryScramblingCode,
        amplitudes: CommonChannelAmplitudes,
    ) -> Result<Self, DspError> {
        if [
            amplitudes.cpich,
            amplitudes.pccpch,
            amplitudes.primary_sch,
            amplitudes.secondary_sch,
        ]
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            return Err(DspError::InvalidAmplitude);
        }
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(code.sequence_number(), &mut scrambling)?;
        let primary = synchronization::primary();
        let secondary: Vec<_> = synchronization::secondary_allocation(code.group())?
            .iter()
            .map(|&number| synchronization::secondary(number))
            .collect::<Result<_, _>>()?;
        let mut ovsf = [0.0; COMMON_SF];
        OvsfCode::new(COMMON_SF as u16, 1)?.write(&mut ovsf)?;
        let pilot = Complex32::new(amplitudes.cpich * 0.5, amplitudes.cpich * 0.5);
        let mut base = scrambling
            .iter()
            .map(|&chip| pilot * chip)
            .collect::<Vec<_>>();
        let mut pccpch = vec![Complex32::default(); scrambling.len()];
        for (slot, secondary) in secondary.iter().enumerate() {
            let start = slot * CHIPS_PER_SLOT as usize;
            for chip in 0..SYNCHRONIZATION_CHIPS {
                base[start + chip] += (primary[chip] * amplitudes.primary_sch
                    + secondary[chip] * amplitudes.secondary_sch)
                    * (SCH_NON_STTD_SYMBOL * std::f32::consts::FRAC_1_SQRT_2);
            }
            // TS 25.211 5.3.3.3 reserves the first 256 chips for SCH, not BCH data.
            for chip in SYNCHRONIZATION_CHIPS..CHIPS_PER_SLOT as usize {
                pccpch[start + chip] =
                    scrambling[start + chip] * (ovsf[chip % COMMON_SF] * amplitudes.pccpch * 0.5);
            }
        }
        Ok(Self { base, pccpch })
    }

    // TS 25.213 5.1.1.1 maps consecutive even/odd bits to I/Q with 0 -> +1.
    pub fn compose_frame(
        &self,
        bch: &[Bit; FRAME_BITS],
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if output.len() != CHIPS_PER_FRAME as usize {
            return Err(DspError::InvalidLength);
        }
        for slot in 0..SLOTS_PER_FRAME as usize {
            let start = slot * CHIPS_PER_SLOT as usize;
            self.compose_slot(
                bch,
                slot,
                &mut output[start..start + CHIPS_PER_SLOT as usize],
            )?;
        }
        Ok(())
    }

    pub fn compose_slot(
        &self,
        bch: &[Bit; FRAME_BITS],
        slot: usize,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if slot >= SLOTS_PER_FRAME as usize || output.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        let frame_start = slot * CHIPS_PER_SLOT as usize;
        output[..SYNCHRONIZATION_CHIPS]
            .copy_from_slice(&self.base[frame_start..frame_start + SYNCHRONIZATION_CHIPS]);
        for symbol in 0..BCH_SYMBOLS_PER_SLOT {
            let bit_start = (slot * BCH_SYMBOLS_PER_SLOT + symbol) * BITS_PER_SYMBOL;
            let sign = |bit| if bit == Bit::Zero { 1.0 } else { -1.0 };
            let value = Complex32::new(sign(bch[bit_start]), sign(bch[bit_start + 1]));
            let start = SYNCHRONIZATION_CHIPS + symbol * COMMON_SF;
            for (index, chip) in output[start..start + COMMON_SF].iter_mut().enumerate() {
                *chip = self.base[frame_start + start + index]
                    + value * self.pccpch[frame_start + start + index];
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broadcast::{BroadcastDecoder, BroadcastEncoder, PAYLOAD_BITS, TTI_BITS};

    fn amplitudes() -> CommonChannelAmplitudes {
        CommonChannelAmplitudes {
            cpich: 0.0,
            pccpch: 0.0,
            primary_sch: 0.0,
            secondary_sch: 0.0,
        }
    }

    #[test]
    fn pilot_has_constant_complex_power_and_frame_aligned_scrambling() {
        let code = PrimaryScramblingCode::new(42).unwrap();
        let generator = CommonDownlink::new(
            code,
            CommonChannelAmplitudes {
                cpich: 0.25,
                ..amplitudes()
            },
        )
        .unwrap();
        let mut frame = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        generator
            .compose_frame(&[Bit::Zero; FRAME_BITS], &mut frame)
            .unwrap();
        let mut scrambling = vec![Complex32::default(); frame.len()];
        downlink_sequence(code.sequence_number(), &mut scrambling).unwrap();
        for (&chip, &scrambling) in frame.iter().zip(&scrambling) {
            assert!((chip.norm_sqr() - 0.25_f32.powi(2)).abs() < 1e-7);
            let recovered = chip * scrambling.conj() * 0.5;
            assert_eq!(recovered, Complex32::new(0.125, 0.125));
        }
    }

    #[test]
    fn synchronization_is_unscrambled_gated_and_group_specific() {
        let primary = synchronization::primary();
        for index in [0, 7, 8, 511] {
            let code = PrimaryScramblingCode::new(index).unwrap();
            let generator = CommonDownlink::new(
                code,
                CommonChannelAmplitudes {
                    primary_sch: 0.3,
                    secondary_sch: 0.2,
                    ..amplitudes()
                },
            )
            .unwrap();
            let mut frame = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
            generator
                .compose_frame(&[Bit::Zero; FRAME_BITS], &mut frame)
                .unwrap();
            for (slot, &number) in synchronization::secondary_allocation(code.group())
                .unwrap()
                .iter()
                .enumerate()
            {
                let secondary = synchronization::secondary(number).unwrap();
                let chips =
                    &frame[slot * CHIPS_PER_SLOT as usize..(slot + 1) * CHIPS_PER_SLOT as usize];
                for chip in 0..SYNCHRONIZATION_CHIPS {
                    assert_eq!(
                        chips[chip],
                        -(primary[chip] * 0.3 + secondary[chip] * 0.2)
                            * std::f32::consts::FRAC_1_SQRT_2
                    );
                }
                assert!(
                    chips[SYNCHRONIZATION_CHIPS..]
                        .iter()
                        .all(|&chip| chip == Complex32::default())
                );
            }
        }
    }

    #[test]
    fn non_sttd_sch_matches_signed_spec_vectors_without_changing_raw_codes() {
        // TS 25.213 5.2.3.1 defines the first 16 chips of PSC and SSC 1.
        const FIRST_PSC_SIGNS: [f32; 16] = [
            1.0, 1.0, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0, -1.0, 1.0,
        ];
        const FIRST_SSC_SIGNS: [f32; 16] = [
            1.0, 1.0, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, 1.0, -1.0,
        ];
        let mut frame = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        let code = PrimaryScramblingCode::new(0).unwrap();
        for (primary_sch, secondary_sch, signs) in
            [(0.25, 0.0, FIRST_PSC_SIGNS), (0.0, 0.25, FIRST_SSC_SIGNS)]
        {
            let physical = CommonDownlink::new(
                code,
                CommonChannelAmplitudes {
                    primary_sch,
                    secondary_sch,
                    ..amplitudes()
                },
            )
            .unwrap();
            physical
                .compose_frame(&[Bit::Zero; FRAME_BITS], &mut frame)
                .unwrap();
            for (chip, sign) in frame.iter().zip(signs) {
                let expected = -sign * 0.25 * std::f32::consts::FRAC_1_SQRT_2;
                assert_eq!(*chip, Complex32::new(expected, expected));
            }
            assert!(
                frame[SYNCHRONIZATION_CHIPS..CHIPS_PER_SLOT as usize]
                    .iter()
                    .all(|chip| *chip == Complex32::default())
            );
        }
        for (chip, sign) in synchronization::primary().iter().zip(FIRST_PSC_SIGNS) {
            assert_eq!(*chip, Complex32::new(sign, sign));
        }
        for (chip, sign) in synchronization::secondary(1)
            .unwrap()
            .iter()
            .zip(FIRST_SSC_SIGNS)
        {
            assert_eq!(*chip, Complex32::new(sign, sign));
        }
    }

    #[test]
    fn pccpch_despreads_to_ordered_bch_bits_and_valid_transport_crc() {
        let code = PrimaryScramblingCode::new(511).unwrap();
        let generator = CommonDownlink::new(
            code,
            CommonChannelAmplitudes {
                cpich: 0.3,
                pccpch: 0.4,
                primary_sch: 0.1,
                secondary_sch: 0.1,
            },
        )
        .unwrap();
        let payload = std::array::from_fn(|i| Bit::from((i * 17 + i / 3) % 11 < 5));
        let mut bch = [Bit::Zero; TTI_BITS];
        BroadcastEncoder::new()
            .unwrap()
            .encode(&payload, &mut bch)
            .unwrap();
        let mut recovered = [0.0; TTI_BITS];
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(code.sequence_number(), &mut scrambling).unwrap();
        let mut frame = vec![Complex32::default(); scrambling.len()];
        for f in 0..2 {
            let input: &[Bit; FRAME_BITS] = bch[f * FRAME_BITS..(f + 1) * FRAME_BITS]
                .try_into()
                .unwrap();
            generator.compose_frame(input, &mut frame).unwrap();
            let mut bit = f * FRAME_BITS;
            for slot in 0..SLOTS_PER_FRAME as usize {
                for symbol in 1..=BCH_SYMBOLS_PER_SLOT {
                    let start = slot * CHIPS_PER_SLOT as usize + symbol * COMMON_SF;
                    let mut value = Complex32::default();
                    for chip in 0..COMMON_SF {
                        let sign = if chip < COMMON_SF / 2 { 1.0 } else { -1.0 };
                        value += frame[start + chip] * scrambling[start + chip].conj() * sign;
                    }
                    recovered[bit] = value.re;
                    recovered[bit + 1] = value.im;
                    bit += BITS_PER_SYMBOL;
                }
            }
        }
        for (&llr, &bit) in recovered.iter().zip(&bch) {
            assert_eq!(llr < 0.0, bit == Bit::One);
            assert!((llr.abs() - 0.4 * COMMON_SF as f32).abs() < 0.001);
        }
        let decoded: [Bit; PAYLOAD_BITS] = BroadcastDecoder::new()
            .unwrap()
            .decode(&recovered)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, payload);
    }

    #[test]
    fn invalid_amplitudes_and_output_geometry_are_rejected() {
        let code = PrimaryScramblingCode::new(0).unwrap();
        for value in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            assert!(matches!(
                CommonDownlink::new(
                    code,
                    CommonChannelAmplitudes {
                        cpich: value,
                        ..amplitudes()
                    }
                ),
                Err(DspError::InvalidAmplitude)
            ));
        }
        assert_eq!(
            CommonDownlink::new(code, amplitudes())
                .unwrap()
                .compose_frame(&[Bit::Zero; FRAME_BITS], &mut []),
            Err(DspError::InvalidLength)
        );
    }

    #[test]
    fn slot_kernel_matches_independent_frame_reference() {
        let generator = CommonDownlink::new(
            PrimaryScramblingCode::new(37).unwrap(),
            CommonChannelAmplitudes {
                cpich: 0.11,
                pccpch: 0.23,
                primary_sch: 0.07,
                secondary_sch: 0.13,
            },
        )
        .unwrap();
        let bch = std::array::from_fn(|index| Bit::from((index * 13 + 5) % 7 < 3));
        let mut expected = generator.base.clone();
        for slot in 0..SLOTS_PER_FRAME as usize {
            for symbol in 0..BCH_SYMBOLS_PER_SLOT {
                let bit_start = (slot * BCH_SYMBOLS_PER_SLOT + symbol) * BITS_PER_SYMBOL;
                let sign = |bit| if bit == Bit::Zero { 1.0 } else { -1.0 };
                let value = Complex32::new(sign(bch[bit_start]), sign(bch[bit_start + 1]));
                let frame_start = slot * CHIPS_PER_SLOT as usize;
                let start = frame_start + SYNCHRONIZATION_CHIPS + symbol * COMMON_SF;
                for (index, chip) in expected[start..start + COMMON_SF].iter_mut().enumerate() {
                    *chip += value * generator.pccpch[start + index];
                }
            }
        }
        let mut frame = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        generator.compose_frame(&bch, &mut frame).unwrap();
        assert_eq!(frame, expected);

        let mut slots = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        for slot in 0..SLOTS_PER_FRAME as usize {
            let start = slot * CHIPS_PER_SLOT as usize;
            generator
                .compose_slot(
                    &bch,
                    slot,
                    &mut slots[start..start + CHIPS_PER_SLOT as usize],
                )
                .unwrap();
        }
        assert_eq!(slots, expected);
        let mut invalid_slot = vec![Complex32::default(); CHIPS_PER_SLOT as usize];
        assert!(
            generator
                .compose_slot(&bch, SLOTS_PER_FRAME as usize, &mut invalid_slot)
                .is_err()
        );
    }
}
