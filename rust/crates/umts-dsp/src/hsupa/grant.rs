// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    DspError,
    bits::Bit,
    convolutional::{self, CodeRate},
    crc::TransportCrc,
};
use crate::{
    channel_codes::OvsfCode,
    scrambling::{PrimaryScramblingCode, downlink_sequence},
};
use num_complex::Complex32;
use umts_types::time::{CHIPS_PER_FRAME, CHIPS_PER_SLOT, ChipInstant};

const VALUE_BITS: usize = 5;
const INFORMATION_BITS: usize = 6;
const IDENTITY_BITS: usize = 16;
const CODED_BITS: usize = 90;
pub const GRANT_BITS: usize = 60;
// TS 25.211 7.12 measures the E-AGCH offset from P-CCPCH.
pub const GRANT_FRAME_OFFSET_CHIPS: u64 = 5120;
const SPREADING: usize = 256;
const SLOT_BITS: usize = 20;
const SUBFRAME_SLOTS: usize = 3;

// TS 25.212 4.10.4 uses one-based puncturing positions.
const PUNCTURED: [usize; 30] = [
    1, 2, 5, 6, 7, 11, 12, 14, 15, 17, 23, 24, 31, 37, 44, 47, 61, 63, 64, 71, 72, 75, 77, 80, 83,
    84, 85, 87, 88, 90,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantScope {
    AllProcesses,
    PerProcess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsoluteGrant {
    value: u8,
    scope: GrantScope,
}

pub struct GrantSpreader {
    weights: Vec<Complex32>,
}

impl GrantSpreader {
    pub fn new(primary: PrimaryScramblingCode, channel_code: u16) -> Result<Self, DspError> {
        let mut code = [0.0; SPREADING];
        OvsfCode::new(SPREADING as u16, channel_code)?.write(&mut code)?;
        let mut weights = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
        downlink_sequence(primary.sequence_number(), &mut weights)?;
        for (index, weight) in weights.iter_mut().enumerate() {
            *weight *= 0.5 * code[index % SPREADING];
        }
        Ok(Self { weights })
    }

    // TS 25.212 4.10.5 repeats the same 60 bits in every subframe for a 10 ms grant.
    pub fn add_slot(
        &self,
        bits: &[Bit; GRANT_BITS],
        start: ChipInstant,
        grant_slot: u8,
        amplitude: f32,
        output: &mut [Complex32],
    ) -> Result<(), DspError> {
        if grant_slot >= 15 || output.len() != CHIPS_PER_SLOT as usize {
            return Err(DspError::InvalidLength);
        }
        if !start.index.is_multiple_of(CHIPS_PER_SLOT)
            || start.index.checked_add(CHIPS_PER_SLOT).is_none()
        {
            return Err(DspError::DiscontinuousTime);
        }
        if !amplitude.is_finite() || !(0.0..=1.0).contains(&amplitude) {
            return Err(DspError::InvalidAmplitude);
        }
        let offset = (start.index % CHIPS_PER_FRAME) as usize;
        let first = usize::from(grant_slot) % SUBFRAME_SLOTS * SLOT_BITS;
        let sign = |bit: Bit| if bit == Bit::Zero { 1.0 } else { -1.0 };
        for (symbol, chips) in output.chunks_exact_mut(SPREADING).enumerate() {
            let value = Complex32::new(
                sign(bits[first + 2 * symbol]),
                sign(bits[first + 2 * symbol + 1]),
            ) * amplitude;
            for (chip, output) in chips.iter_mut().enumerate() {
                *output +=
                    value * self.weights[(offset + symbol * SPREADING + chip) % self.weights.len()];
            }
        }
        Ok(())
    }
}

impl AbsoluteGrant {
    pub fn new(value: u8, scope: GrantScope) -> Result<Self, DspError> {
        if usize::from(value) >= 1 << VALUE_BITS {
            return Err(DspError::InvalidCode);
        }
        Ok(Self { value, scope })
    }

    pub fn value(self) -> u8 {
        self.value
    }
    pub fn scope(self) -> GrantScope {
        self.scope
    }

    pub fn encode(self, e_rnti: u16) -> Result<[Bit; GRANT_BITS], DspError> {
        let mut information = [Bit::Zero; INFORMATION_BITS + IDENTITY_BITS];
        for (index, bit) in information[..VALUE_BITS].iter_mut().enumerate() {
            *bit = Bit::from((self.value >> (VALUE_BITS - 1 - index)) & 1 != 0);
        }
        information[VALUE_BITS] = Bit::from(self.scope == GrantScope::PerProcess);
        let (payload, parity) = information.split_at_mut(INFORMATION_BITS);
        TransportCrc::Crc16.write(payload, parity)?;
        for (index, bit) in parity.iter_mut().enumerate() {
            let mask = ((e_rnti >> (IDENTITY_BITS - 1 - index)) & 1) as u8;
            *bit = Bit::from((*bit as u8 ^ mask) != 0);
        }
        let mut coded = [Bit::Zero; CODED_BITS];
        convolutional::encode(CodeRate::Third, &information, &mut coded)?;
        let mut output = [Bit::Zero; GRANT_BITS];
        let mut destination = 0;
        let mut puncture = 0;
        for (index, bit) in coded.into_iter().enumerate() {
            if PUNCTURED.get(puncture) == Some(&(index + 1)) {
                puncture += 1;
            } else {
                output[destination] = bit;
                destination += 1;
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_identity_and_scope_change_the_protected_codeword() {
        let all = AbsoluteGrant::new(12, GrantScope::AllProcesses).unwrap();
        // Independent polynomial division and delay-tap evaluation of TS 25.212 4.10.
        let expected = "010010100000111111100010111001110100000000010011111111110011";
        assert_eq!(
            all.encode(0x1234).unwrap().as_slice(),
            expected
                .bytes()
                .map(|bit| Bit::from(bit == b'1'))
                .collect::<Vec<_>>()
        );
        let per = AbsoluteGrant::new(12, GrantScope::PerProcess).unwrap();
        assert_ne!(all.encode(0x1234).unwrap(), per.encode(0x1234).unwrap());
        assert_ne!(all.encode(0x1234).unwrap(), all.encode(0x4321).unwrap());
        assert_eq!(
            AbsoluteGrant::new(0, GrantScope::AllProcesses)
                .unwrap()
                .encode(0)
                .unwrap(),
            [Bit::Zero; GRANT_BITS]
        );
        assert!(AbsoluteGrant::new(32, GrantScope::AllProcesses).is_err());
    }
}
