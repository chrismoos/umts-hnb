// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::{Bit, TxBit},
    channel_codes::OvsfCode,
    pilots::DownlinkPilots,
    scrambling::{DownlinkScramblingCode, downlink_sequence},
    tfci::{CODEWORD_BITS, Tfci},
};
use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, SLOTS_PER_FRAME};

const TIMING_QUANTUM_CHIPS: u64 = 256;
const TIMING_OFFSETS: u8 = 150;
const QPSK_BRANCHES: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fields {
    sf: u16,
    data1: u16,
    data2: u16,
    tpc: u8,
    tfci: u8,
    pilot: DownlinkPilots,
}

// TS 25.211 Table 11, normal slot formats 0 through 16.
const FORMATS: [Fields; 17] = [
    Fields {
        sf: 512,
        data1: 0,
        data2: 4,
        tpc: 2,
        tfci: 0,
        pilot: DownlinkPilots::Four,
    },
    Fields {
        sf: 512,
        data1: 0,
        data2: 2,
        tpc: 2,
        tfci: 2,
        pilot: DownlinkPilots::Four,
    },
    Fields {
        sf: 256,
        data1: 2,
        data2: 14,
        tpc: 2,
        tfci: 0,
        pilot: DownlinkPilots::Two,
    },
    Fields {
        sf: 256,
        data1: 2,
        data2: 12,
        tpc: 2,
        tfci: 2,
        pilot: DownlinkPilots::Two,
    },
    Fields {
        sf: 256,
        data1: 2,
        data2: 12,
        tpc: 2,
        tfci: 0,
        pilot: DownlinkPilots::Four,
    },
    Fields {
        sf: 256,
        data1: 2,
        data2: 10,
        tpc: 2,
        tfci: 2,
        pilot: DownlinkPilots::Four,
    },
    Fields {
        sf: 256,
        data1: 2,
        data2: 8,
        tpc: 2,
        tfci: 0,
        pilot: DownlinkPilots::Eight,
    },
    Fields {
        sf: 256,
        data1: 2,
        data2: 6,
        tpc: 2,
        tfci: 2,
        pilot: DownlinkPilots::Eight,
    },
    Fields {
        sf: 128,
        data1: 6,
        data2: 28,
        tpc: 2,
        tfci: 0,
        pilot: DownlinkPilots::Four,
    },
    Fields {
        sf: 128,
        data1: 6,
        data2: 26,
        tpc: 2,
        tfci: 2,
        pilot: DownlinkPilots::Four,
    },
    Fields {
        sf: 128,
        data1: 6,
        data2: 24,
        tpc: 2,
        tfci: 0,
        pilot: DownlinkPilots::Eight,
    },
    Fields {
        sf: 128,
        data1: 6,
        data2: 22,
        tpc: 2,
        tfci: 2,
        pilot: DownlinkPilots::Eight,
    },
    Fields {
        sf: 64,
        data1: 12,
        data2: 48,
        tpc: 4,
        tfci: 8,
        pilot: DownlinkPilots::Eight,
    },
    Fields {
        sf: 32,
        data1: 28,
        data2: 112,
        tpc: 4,
        tfci: 8,
        pilot: DownlinkPilots::Eight,
    },
    Fields {
        sf: 16,
        data1: 56,
        data2: 232,
        tpc: 8,
        tfci: 8,
        pilot: DownlinkPilots::Sixteen,
    },
    Fields {
        sf: 8,
        data1: 120,
        data2: 488,
        tpc: 8,
        tfci: 8,
        pilot: DownlinkPilots::Sixteen,
    },
    Fields {
        sf: 4,
        data1: 248,
        data2: 1000,
        tpc: 8,
        tfci: 8,
        pilot: DownlinkPilots::Sixteen,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownlinkFormat {
    number: u8,
    tfci: bool,
    fields: Fields,
}
impl DownlinkFormat {
    pub fn new(number: u8, tfci: bool) -> Result<Self, DspError> {
        let fields = *FORMATS
            .get(usize::from(number))
            .ok_or(DspError::InvalidCode)?;
        if tfci && fields.tfci == 0 || !tfci && fields.tfci != 0 && fields.sf >= 128 {
            return Err(DspError::InvalidCode);
        }
        Ok(Self {
            number,
            tfci,
            fields,
        })
    }
    pub fn number(self) -> u8 {
        self.number
    }
    pub fn spreading_factor(self) -> u16 {
        self.fields.sf
    }
    pub fn tfci(self) -> bool {
        self.tfci
    }
    pub fn data1_bits(self) -> usize {
        usize::from(self.fields.data1)
    }
    pub fn data2_bits(self) -> usize {
        usize::from(self.fields.data2)
    }
    pub fn data_bits(self) -> usize {
        self.data1_bits() + self.data2_bits()
    }
    pub fn tpc_bits(self) -> usize {
        usize::from(self.fields.tpc)
    }
    pub fn tfci_bits(self) -> usize {
        usize::from(self.fields.tfci)
    }
    pub fn pilots(self) -> DownlinkPilots {
        self.fields.pilot
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DpchTiming(u8);
impl DpchTiming {
    // TS 25.211 7.1 defines the offset in multiples of 256 chips relative to P-CCPCH.
    pub fn new(units: u8) -> Result<Self, DspError> {
        if units >= TIMING_OFFSETS {
            return Err(DspError::InvalidLength);
        }
        Ok(Self(units))
    }
    pub fn chips(self) -> u64 {
        u64::from(self.0) * TIMING_QUANTUM_CHIPS
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DpchRole {
    Primary,
    Additional,
}

#[derive(Clone, Copy, Debug)]
pub struct DpchAmplitudes {
    // RMS complex-chip amplitudes with both QPSK branches active.
    pub data: f32,
    pub tpc: f32,
    pub tfci: f32,
    pub pilot: f32,
}

pub struct DownlinkDpch {
    format: DownlinkFormat,
    timing: DpchTiming,
    role: DpchRole,
    amplitudes: DpchAmplitudes,
    weights: Vec<Complex32>,
}

impl DownlinkDpch {
    pub fn new(
        scrambling_code: impl Into<DownlinkScramblingCode>,
        code: OvsfCode,
        format: DownlinkFormat,
        timing: DpchTiming,
        role: DpchRole,
        amplitudes: DpchAmplitudes,
    ) -> Result<Self, DspError> {
        if code.spreading_factor() != format.spreading_factor() {
            return Err(DspError::InvalidCode);
        }
        if [
            amplitudes.data,
            amplitudes.tpc,
            amplitudes.tfci,
            amplitudes.pilot,
        ]
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            return Err(DspError::InvalidAmplitude);
        }
        let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(scrambling_code.into().sequence_number(), &mut scrambling)?;
        let mut channel = vec![0.0; usize::from(code.spreading_factor())];
        code.write(&mut channel)?;
        // TS 25.213 5.1.4 keeps scrambling cell-aligned while channelization is symbol-aligned.
        let weights = (0..CHIPS_PER_FRAME as usize)
            .map(|index| {
                scrambling[(index + timing.chips() as usize) % scrambling.len()]
                    * (channel[index % channel.len()] * 0.5)
            })
            .collect();
        Ok(Self {
            format,
            timing,
            role,
            amplitudes,
            weights,
        })
    }
    pub fn format(&self) -> DownlinkFormat {
        self.format
    }
    pub fn timing(&self) -> DpchTiming {
        self.timing
    }

    pub fn add_slot(
        &self,
        slot: u8,
        data: &[TxBit],
        tfci: Option<Tfci>,
        tpc: Bit,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if u64::from(slot) >= SLOTS_PER_FRAME
            || data.len() != self.format.data_bits()
            || output.len() != CHIPS_PER_SLOT as usize
        {
            return Err(DspError::InvalidLength);
        }
        let primary = self.role == DpchRole::Primary;
        if tfci.is_some() != (primary && self.format.tfci) {
            return Err(DspError::InvalidCode);
        }
        let word = tfci.map(Tfci::encode);
        let pilots = self.format.pilots().word(slot).expect("validated slot");
        let data1_end = self.format.data1_bits();
        let tpc_end = data1_end + self.format.tpc_bits();
        let tfci_end = tpc_end + self.format.tfci_bits();
        let data2_end = tfci_end + self.format.data2_bits();
        let value = |bit: usize| {
            if bit < data1_end {
                return data[bit].amplitude() * self.amplitudes.data;
            }
            if bit >= tfci_end && bit < data2_end {
                return data[data1_end + bit - tfci_end].amplitude() * self.amplitudes.data;
            }
            // TS 25.211 5.3.2 transmits control only on the first DPCH of a multicode CCTrCH.
            if !primary {
                return 0.0;
            }
            if bit < tpc_end {
                TxBit::Data(tpc).amplitude() * self.amplitudes.tpc
            } else if bit < tfci_end {
                word.as_ref().map_or(0.0, |word| {
                    TxBit::Data(
                        word[(usize::from(slot) * self.format.tfci_bits() + bit - tpc_end)
                            % CODEWORD_BITS],
                    )
                    .amplitude()
                        * self.amplitudes.tfci
                })
            } else {
                TxBit::Data(Bit::from(
                    pilots & (1 << (self.format.pilots().bits() - 1 - (bit - data2_end))) != 0,
                ))
                .amplitude()
                    * self.amplitudes.pilot
            }
        };
        let sf = usize::from(self.format.spreading_factor());
        let first = usize::from(slot) * CHIPS_PER_SLOT as usize;
        for (symbol, chips) in output.chunks_exact_mut(sf).enumerate() {
            let modulation = Complex32::new(
                value(symbol * QPSK_BRANCHES),
                value(symbol * QPSK_BRANCHES + 1),
            );
            let offset = first + symbol * sf;
            for (chip, weight) in chips.iter_mut().zip(&self.weights[offset..offset + sf]) {
                *chip += modulation * weight;
            }
        }
        Ok(())
    }
}
