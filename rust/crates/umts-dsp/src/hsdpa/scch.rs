// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ConstellationVersion, MAX_CODES, Modulation, Redundancy, TransportTable};
use crate::{
    DspError,
    bits::Bit,
    convolutional::{self, CodeRate},
    crc::TransportCrc,
};

const PART1_BITS: usize = 8;
const PART2_BITS: usize = 13;
const IDENTITY_BITS: usize = 16;
const CODED_PART1: usize = 48;
const CODED_PART2: usize = 111;
pub const SCCH_BITS: usize = 120;
const FIRST_SLOT_BITS: usize = 40;
// TS 25.212 4.6.6 lists one-based positions.
const PUNCTURE1: [usize; 8] = [1, 2, 4, 8, 42, 45, 47, 48];
const PUNCTURE2: [usize; 31] = [
    1, 2, 3, 4, 5, 6, 7, 8, 12, 14, 15, 24, 42, 48, 54, 57, 60, 66, 69, 96, 99, 101, 102, 104, 105,
    106, 107, 108, 109, 110, 111,
];

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TransmissionVersion(u8);

impl TransmissionVersion {
    pub fn new(value: u8) -> Result<Self, DspError> {
        if value >= 8 {
            return Err(DspError::InvalidCode);
        }
        Ok(Self(value))
    }

    // TS 25.212 Tables 12 and 13 jointly select rate matching and constellation.
    pub fn parameters(self, modulation: Modulation) -> (Redundancy, ConstellationVersion) {
        let (systematic, version, constellation) = match modulation {
            Modulation::Qpsk => (
                self.0.is_multiple_of(2),
                self.0 / 2,
                ConstellationVersion::Original,
            ),
            Modulation::Qam16 | Modulation::Qam64 => match self.0 {
                0 => (true, 0, ConstellationVersion::Original),
                1 => (false, 0, ConstellationVersion::Original),
                2 => (true, 1, ConstellationVersion::Swap),
                3 => (false, 1, ConstellationVersion::Swap),
                4 => (true, 0, ConstellationVersion::Swap),
                5 => (true, 0, ConstellationVersion::Invert),
                6 => (true, 0, ConstellationVersion::SwapInvert),
                _ => (true, 1, ConstellationVersion::Original),
            },
        };
        (
            Redundancy {
                prioritize_systematic: systematic,
                version,
            },
            constellation,
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ScchAssignment {
    first_code: u8,
    codes: u8,
    modulation: Modulation,
    transport_size_index: u8,
    harq_process: u8,
    version: TransmissionVersion,
    new_data: bool,
    table: TransportTable,
    control_list_number: u8,
}

impl ScchAssignment {
    pub fn validate_category(
        self,
        category: umts_types::hsdpa::Category,
        payload_bits: usize,
    ) -> Result<(), DspError> {
        let limits = category.limits();
        if self.codes > limits.maximum_codes
            || payload_bits == 0
            || payload_bits > limits.maximum_transport_bits
            || (self.modulation == Modulation::Qam16 && !limits.qam16)
            || (self.modulation == Modulation::Qam64 && !limits.qam64)
            || (self.table == TransportTable::OctetAligned) != limits.qam64
        {
            return Err(DspError::InvalidCode);
        }
        Ok(())
    }

    pub fn first_code(self) -> u8 {
        self.first_code
    }

    pub fn table(self) -> TransportTable {
        self.table
    }

    pub fn with_table(mut self, table: TransportTable) -> Result<Self, DspError> {
        if self.modulation == Modulation::Qam64 && table != TransportTable::OctetAligned {
            return Err(DspError::InvalidCode);
        }
        self.table = table;
        Ok(self)
    }

    pub fn control_list_number(self) -> u8 {
        self.control_list_number
    }

    pub fn on_control(mut self, number: u8) -> Result<Self, DspError> {
        self.information_on(number)?;
        self.control_list_number = number;
        Ok(self)
    }

    pub fn code_count(self) -> u8 {
        self.codes
    }

    pub fn modulation(self) -> Modulation {
        self.modulation
    }

    pub fn version(self) -> TransmissionVersion {
        self.version
    }

    pub fn transport_size_index(self) -> u8 {
        self.transport_size_index
    }

    pub fn harq_process(self) -> u8 {
        self.harq_process
    }

    pub fn new_data(self) -> bool {
        self.new_data
    }

    pub fn new(
        first_code: u8,
        codes: u8,
        modulation: Modulation,
        transport_size_index: u8,
        harq_process: u8,
        version: TransmissionVersion,
        new_data: bool,
    ) -> Result<Self, DspError> {
        if first_code == 0
            || codes == 0
            || usize::from(codes) > MAX_CODES
            || u16::from(first_code) + u16::from(codes) > 16
            || transport_size_index >= 64
            || harq_process >= 8
        {
            return Err(DspError::InvalidCode);
        }
        Ok(Self {
            first_code,
            codes,
            modulation,
            transport_size_index,
            harq_process,
            version,
            new_data,
            table: if modulation == Modulation::Qam64 {
                TransportTable::OctetAligned
            } else {
                TransportTable::BitAligned
            },
            control_list_number: 1,
        })
    }

    fn information_on(
        self,
        control_list_number: u8,
    ) -> Result<[Bit; PART1_BITS + PART2_BITS], DspError> {
        if !(1..=4).contains(&control_list_number) {
            return Err(DspError::InvalidCode);
        }
        let mut bits = [Bit::Zero; PART1_BITS + PART2_BITS];
        write_msb((self.codes - 1).min(15 - self.codes).into(), &mut bits[..3]);
        let offset =
            (i16::from(self.first_code) - 1 - i16::from(self.codes / 8) * 15).unsigned_abs();
        if self.table == TransportTable::OctetAligned && self.modulation != Modulation::Qpsk {
            // TS 25.212 4.6.2.3 replaces the offset LSB with modulation. Its parity is implicit in the control-list position.
            if offset % 2 != u16::from(control_list_number % 2) {
                return Err(DspError::InvalidCode);
            }
            write_msb(offset / 2, &mut bits[3..6]);
            bits[6] = Bit::from(self.modulation == Modulation::Qam64);
        } else {
            write_msb(offset, &mut bits[3..7]);
        }
        bits[7] = Bit::from(self.modulation != Modulation::Qpsk);
        write_msb(self.transport_size_index.into(), &mut bits[8..14]);
        write_msb(self.harq_process.into(), &mut bits[14..17]);
        write_msb(self.version.0.into(), &mut bits[17..20]);
        bits[20] = Bit::from(self.new_data);
        Ok(bits)
    }

    #[cfg(test)]
    fn information(self) -> [Bit; PART1_BITS + PART2_BITS] {
        self.information_on(self.control_list_number)
            .expect("valid test assignment")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondaryCellOrder {
    Activate,
    Deactivate,
}

impl SecondaryCellOrder {
    fn information(self) -> [Bit; PART1_BITS + PART2_BITS] {
        // TS 25.212 4.6.1 and 4.6C.2.2.2. Secondary uplink remains deactivated.
        const ORDER_PART1: u16 = 0b11100000;
        const ORDER_TRANSPORT: u16 = 0b111101;
        const SECONDARY_ORDER_TYPE: u16 = 0b001;
        let mut bits = [Bit::Zero; PART1_BITS + PART2_BITS];
        write_msb(ORDER_PART1, &mut bits[..PART1_BITS]);
        write_msb(ORDER_TRANSPORT, &mut bits[8..14]);
        write_msb(SECONDARY_ORDER_TYPE, &mut bits[14..17]);
        bits[19] = Bit::from(self == Self::Activate);
        bits
    }
}

#[derive(Clone)]
pub struct ScchEncoder {
    h_rnti: u16,
    identity: [Bit; IDENTITY_BITS],
    mask: [Bit; FIRST_SLOT_BITS],
}

impl ScchEncoder {
    pub fn new(h_rnti: u16) -> Result<Self, DspError> {
        let mut identity = [Bit::Zero; IDENTITY_BITS];
        write_msb(h_rnti, &mut identity);
        let mut coded = [Bit::Zero; CODED_PART1];
        convolutional::encode(CodeRate::Half, &identity, &mut coded)?;
        let mut mask = [Bit::Zero; FIRST_SLOT_BITS];
        puncture(&coded, &PUNCTURE1, &mut mask);
        Ok(Self {
            h_rnti,
            identity,
            mask,
        })
    }

    pub fn h_rnti(&self) -> u16 {
        self.h_rnti
    }

    pub fn encode(&self, assignment: ScchAssignment) -> Result<[Bit; SCCH_BITS], DspError> {
        self.encode_on(assignment, assignment.control_list_number)
    }

    pub fn encode_on(
        &self,
        assignment: ScchAssignment,
        control_list_number: u8,
    ) -> Result<[Bit; SCCH_BITS], DspError> {
        self.encode_information(assignment.information_on(control_list_number)?)
    }

    pub fn encode_order(&self, order: SecondaryCellOrder) -> Result<[Bit; SCCH_BITS], DspError> {
        self.encode_information(order.information())
    }

    fn encode_information(
        &self,
        information: [Bit; PART1_BITS + PART2_BITS],
    ) -> Result<[Bit; SCCH_BITS], DspError> {
        let mut part2 = [Bit::Zero; PART2_BITS + IDENTITY_BITS];
        part2[..PART2_BITS].copy_from_slice(&information[PART1_BITS..]);
        TransportCrc::Crc16.write(&information, &mut part2[PART2_BITS..])?;
        for (bit, identity) in part2[PART2_BITS..].iter_mut().zip(self.identity) {
            *bit = Bit::from(*bit != identity);
        }
        let mut coded1 = [Bit::Zero; CODED_PART1];
        let mut coded2 = [Bit::Zero; CODED_PART2];
        convolutional::encode(CodeRate::Third, &information[..PART1_BITS], &mut coded1)?;
        convolutional::encode(CodeRate::Third, &part2, &mut coded2)?;
        let mut output = [Bit::Zero; SCCH_BITS];
        puncture(&coded1, &PUNCTURE1, &mut output[..FIRST_SLOT_BITS]);
        puncture(&coded2, &PUNCTURE2, &mut output[FIRST_SLOT_BITS..]);
        for (bit, mask) in output[..FIRST_SLOT_BITS].iter_mut().zip(self.mask) {
            *bit = Bit::from(*bit != mask);
        }
        Ok(output)
    }
}

fn write_msb(value: u16, output: &mut [Bit]) {
    let width = output.len();
    for (index, bit) in output.iter_mut().enumerate() {
        *bit = Bit::from(value & (1 << (width - 1 - index)) != 0);
    }
}

fn puncture(input: &[Bit], omitted: &[usize], output: &mut [Bit]) {
    let mut omitted = omitted.iter().copied().peekable();
    let mut output = output.iter_mut();
    for (index, &bit) in input.iter().enumerate() {
        if omitted.peek() == Some(&(index + 1)) {
            omitted.next();
        } else {
            *output.next().expect("fixed HS-SCCH puncturing geometry") = bit;
        }
    }
    assert!(output.next().is_none(), "fixed HS-SCCH puncturing geometry");
}
