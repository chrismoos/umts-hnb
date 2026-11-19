// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_types::hsupa::{Etfci, Rsn};

use crate::{
    DspError,
    bits::Bit,
    tfci::{Tfci, TfciDecoder, UPLINK_CODE_BITS},
};

mod grant;
mod indicator;
mod physical;
mod power;
mod rate_matching;
mod receive;
mod timing;
mod transport;
pub use grant::{AbsoluteGrant, GRANT_BITS, GRANT_FRAME_OFFSET_CHIPS, GrantScope, GrantSpreader};
pub use indicator::{Indicator, IndicatorSpreader, Signature};
pub use physical::UplinkChannels;
pub use power::{ChannelGains, SingleReferencePower, control_gain_ratio};
pub use rate_matching::HarqRateMatching;
pub use receive::{
    ChipEqualizer, CollectedTti, CollectionProgress, ControlMeasurements, DataMeasurements,
    TtiCollector,
};
pub use timing::HarqFeedbackTiming;
pub use transport::{TransportDecoder, TransportEncoder, TransportPlan};

pub const CONTROL_BITS: usize = UPLINK_CODE_BITS;
const RSN_SHIFT: u32 = 1;
const ETFCI_SHIFT: u32 = 3;
const RSN_MASK: u16 = 3;
const HAPPY_MASK: u16 = 1;
const CONTROL_VARIANTS: usize = 8;
const MAX_FORMATS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlInformation {
    pub format: Etfci,
    pub sequence: Rsn,
    pub happy: bool,
}

impl ControlInformation {
    // TS 25.212 4.9.3 places happy, RSN LSB first, then E-TFCI LSB first.
    fn code(self) -> Tfci {
        let index = (u16::from(self.format.value()) << ETFCI_SHIFT)
            | (u16::from(self.sequence.value()) << RSN_SHIFT)
            | u16::from(self.happy);
        Tfci::new(index).expect("validated E-DPCCH information fits ten bits")
    }

    pub fn encode(self) -> [Bit; CONTROL_BITS] {
        let word = self.code().encode();
        std::array::from_fn(|index| word[index])
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ControlDecision {
    pub information: ControlInformation,
    pub normalized_score: f64,
    pub normalized_margin: Option<f64>,
}

pub struct ControlDecoder {
    codebook: TfciDecoder,
}

impl ControlDecoder {
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.codebook.heap_bytes()
    }
    pub fn new(formats: &[Etfci]) -> Result<Self, DspError> {
        if formats.is_empty() || formats.len() > MAX_FORMATS {
            return Err(DspError::InvalidCode);
        }
        let mut codes = Vec::with_capacity(formats.len() * CONTROL_VARIANTS);
        for format in formats {
            for sequence in 0..=RSN_MASK as u8 {
                for happy in [false, true] {
                    codes.push(
                        ControlInformation {
                            format: *format,
                            sequence: Rsn::new(sequence).expect("bounded RSN"),
                            happy,
                        }
                        .code(),
                    );
                }
            }
        }
        Ok(Self {
            codebook: TfciDecoder::new(&codes)?,
        })
    }

    // E-DPCCH has no CRC. A winning codeword is evidence, not permission to ACK E-DCH.
    pub fn decode(&self, input: &[f32; CONTROL_BITS]) -> Result<Option<ControlDecision>, DspError> {
        self.decode_with_context(input, None, 0.0)
    }

    /// `expected` specifies the required retransmission format and RSN. Either happy-bit
    /// variant of that word wins over the raw winner when it scores within `tolerance`.
    pub fn decode_with_context(
        &self,
        input: &[f32; CONTROL_BITS],
        expected: Option<(Etfci, Rsn)>,
        tolerance: f64,
    ) -> Result<Option<ControlDecision>, DspError> {
        let preferred = expected.map(|(format, sequence)| {
            [false, true].map(|happy| {
                ControlInformation {
                    format,
                    sequence,
                    happy,
                }
                .code()
            })
        });
        let preferred = preferred.as_ref().map_or(&[][..], |words| words.as_slice());
        let Some(decision) = self
            .codebook
            .decode_preferring(input, preferred, tolerance)?
        else {
            return Ok(None);
        };
        let index = decision.tfci.index();
        Ok(Some(ControlDecision {
            information: ControlInformation {
                format: Etfci::new((index >> ETFCI_SHIFT) as u8).expect("ten-bit codeword"),
                sequence: Rsn::new(((index >> RSN_SHIFT) & RSN_MASK) as u8).expect("masked RSN"),
                happy: index & HAPPY_MASK != 0,
            },
            normalized_score: decision.normalized_score,
            normalized_margin: decision.normalized_margin,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_slot_control_suffix_is_ambiguous_even_with_perfect_observations() {
        let formats: Vec<_> = (0..128).map(|n| Etfci::new(n).unwrap()).collect();
        let decoder = ControlDecoder::new(&formats).unwrap();
        let target = ControlInformation {
            format: Etfci::new(127).unwrap(),
            sequence: Rsn::new(0).unwrap(),
            happy: false,
        };
        let word = target.encode();
        // A single retained E-DPCCH slot cannot establish the missing
        // transmission's format/generation even with noiseless retained bits.
        for (slot, expected_matches) in [(0, 2), (1, 4), (2, 4)] {
            let range = slot * 10..(slot + 1) * 10;
            let mut input = [0.0; CONTROL_BITS];
            for index in range.clone() {
                input[index] = if word[index] == Bit::Zero { 1.0 } else { -1.0 };
            }
            let mut matches = 0;
            let mut signatures = [0_usize; 1024];
            for format in &formats {
                for sequence in 0..4 {
                    for happy in [false, true] {
                        let candidate = ControlInformation {
                            format: *format,
                            sequence: Rsn::new(sequence).unwrap(),
                            happy,
                        }
                        .encode();
                        let signature = candidate[range.clone()]
                            .iter()
                            .enumerate()
                            .fold(0_usize, |word, (index, bit)| {
                                word | (usize::from(*bit == Bit::One) << index)
                            });
                        signatures[signature] += 1;
                        matches += usize::from(candidate[range.clone()] == word[range.clone()]);
                    }
                }
            }
            assert_eq!(matches, expected_matches);
            assert_eq!(
                signatures.iter().filter(|count| **count != 0).count(),
                1024 / expected_matches
            );
            assert!(
                signatures
                    .into_iter()
                    .all(|count| count == 0 || count == expected_matches)
            );
            for expected in [None, Some((target.format, target.sequence))] {
                for tolerance in [0.0, 0.2, 1.0] {
                    assert!(
                        decoder
                            .decode_with_context(&input, expected, tolerance)
                            .unwrap()
                            .is_none(),
                        "a context hint must not break an exact partial-word tie"
                    );
                }
            }
        }
        // Two real adjacent slots distinguish this word without a context hint.
        for range in [0..20, 10..30] {
            let mut input = [0.0; CONTROL_BITS];
            for index in range {
                input[index] = if word[index] == Bit::Zero { 1.0 } else { -1.0 };
            }
            let decision = decoder.decode(&input).unwrap().unwrap();
            assert_eq!(decision.information, target);
            assert!(decision.normalized_margin.unwrap() > 0.0);
        }
    }

    #[test]
    fn control_field_basis_matches_spec_columns() {
        let vectors = [
            (0, 0, true, "101010101010101101010101010101"),
            (0, 1, false, "011001100110011011001100110011"),
            (0, 2, false, "000111100001111000111100001111"),
            (1, 0, false, "000000011111111000000011111111"),
        ];
        for (format, sequence, happy, expected) in vectors {
            let information = ControlInformation {
                format: Etfci::new(format).unwrap(),
                sequence: Rsn::new(sequence).unwrap(),
                happy,
            };
            let expected: Vec<_> = expected.bytes().map(|bit| Bit::from(bit == b'1')).collect();
            assert_eq!(information.encode().as_slice(), expected);
        }
    }

    #[test]
    fn restricted_control_decode_preserves_all_fields_and_rejects_silence() {
        let formats = [0, 1, 63, 127].map(|value| Etfci::new(value).unwrap());
        let decoder = ControlDecoder::new(&formats).unwrap();
        for format in formats {
            for sequence in 0..4 {
                for happy in [false, true] {
                    let information = ControlInformation {
                        format,
                        sequence: Rsn::new(sequence).unwrap(),
                        happy,
                    };
                    let bits = information.encode();
                    for amplitude in [0.001, 1.0, 100.0] {
                        let input = bits.map(|bit| {
                            if bit == Bit::Zero {
                                amplitude
                            } else {
                                -amplitude
                            }
                        });
                        assert_eq!(
                            decoder.decode(&input).unwrap().unwrap().information,
                            information
                        );
                    }
                }
            }
        }
        assert!(decoder.decode(&[0.0; CONTROL_BITS]).unwrap().is_none());
        assert!(decoder.decode(&[f32::NAN; CONTROL_BITS]).is_err());
        assert!(ControlDecoder::new(&[formats[0], formats[0]]).is_err());
    }
}
