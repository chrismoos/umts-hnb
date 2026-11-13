// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    HarqRateMatching, PhysicalChannelInterleaver, ScchAssignment, ScchEncoder, scch::SCCH_BITS,
};
use crate::{
    DspError,
    bits::Bit,
    crc::TransportCrc,
    turbo::{TurboBlockFormat, TurboBlockPlan},
};
use std::num::NonZeroUsize;
use umts_types::hsdpa::Category;

const INVERT_BIT: u32 = 1;
const SOURCE_INDEX_SHIFT: u32 = 1;
// Little-endian 24-bit entries store inversion in bit 0 and the coded-bit index in bits 1..23.
const SOURCE_ENTRY_BYTES: usize = 3;

pub struct TransmissionPlan {
    payload_bits: NonZeroUsize,
    soft_bits: NonZeroUsize,
    encoder: ScchEncoder,
    assignment: ScchAssignment,
    control: [Bit; SCCH_BITS],
    coded_bits: usize,
    sources: Box<[[u8; SOURCE_ENTRY_BYTES]]>,
}

impl TransmissionPlan {
    /// Requested retained storage, excluding allocator overhead and construction scratch.
    pub fn required_storage_bytes(assignment: ScchAssignment) -> Result<usize, DspError> {
        usize::from(assignment.code_count())
            .checked_mul(assignment.modulation().bits_per_code())
            .and_then(|bits| bits.checked_mul(SOURCE_ENTRY_BYTES))
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Self>()))
            .ok_or(DspError::Capacity)
    }

    pub fn new(
        assignment: ScchAssignment,
        category: Category,
        control_encoder: &ScchEncoder,
        ir_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        if matches!(category.value(), 13 | 19 | 23)
            && assignment.modulation() == super::Modulation::Qam64
            && assignment.transport_size_index() > 52
        {
            return Err(DspError::InvalidCode);
        }
        let payload_bits = assignment.table().transport_bits(
            assignment.modulation(),
            assignment.code_count(),
            assignment.transport_size_index(),
        )?;
        Self::with_payload(
            assignment,
            category,
            control_encoder,
            payload_bits,
            ir_bits,
            maximum_coded_bits,
        )
    }

    // TS 25.321 9.2.3.1 permits index 63 to retain an unmappable original TB size.
    pub fn retransmission(
        assignment: ScchAssignment,
        category: Category,
        control_encoder: &ScchEncoder,
        original_payload_bits: NonZeroUsize,
        ir_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        let mapped = assignment.table().transport_bits(
            assignment.modulation(),
            assignment.code_count(),
            assignment.transport_size_index(),
        );
        if mapped != Ok(original_payload_bits) {
            let representable = (0..64).any(|index| {
                assignment.table().transport_bits(
                    assignment.modulation(),
                    assignment.code_count(),
                    index,
                ) == Ok(original_payload_bits)
            });
            if assignment.transport_size_index() != 63 || representable {
                return Err(DspError::InvalidLength);
            }
        }
        Self::with_payload(
            assignment,
            category,
            control_encoder,
            original_payload_bits,
            ir_bits,
            maximum_coded_bits,
        )
    }

    fn with_payload(
        assignment: ScchAssignment,
        category: Category,
        control_encoder: &ScchEncoder,
        payload_bits: NonZeroUsize,
        ir_bits: NonZeroUsize,
        maximum_coded_bits: NonZeroUsize,
    ) -> Result<Self, DspError> {
        assignment.validate_category(category, payload_bits.get())?;
        if ir_bits.get() > category.limits().total_soft_bits {
            return Err(DspError::Capacity);
        }
        let coding = TurboBlockPlan::new(
            TurboBlockFormat {
                block_bits: payload_bits.get(),
                blocks: 1,
                crc: TransportCrc::Crc24,
            },
            maximum_coded_bits,
        )?;
        let coded_bits = NonZeroUsize::new(coding.coded_bits()).ok_or(DspError::InvalidLength)?;
        let (redundancy, constellation) = assignment.version().parameters(assignment.modulation());
        let matching = HarqRateMatching::new(
            coded_bits,
            ir_bits,
            assignment.modulation(),
            usize::from(assignment.code_count()),
            redundancy,
            maximum_coded_bits,
        )?;
        let collected = matching
            .source_indices()
            .iter()
            .map(|&index| {
                u32::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_mul(1 << SOURCE_INDEX_SHIFT))
                    .ok_or(DspError::Capacity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut sources = vec![0; collected.len()];
        PhysicalChannelInterleaver::new()?.prepare_with(
            assignment.modulation(),
            constellation,
            &collected,
            &mut sources,
            |source| source ^ INVERT_BIT,
        )?;
        let sources = sources
            .into_iter()
            .map(|source| {
                let [first, second, third, overflow] = source.to_le_bytes();
                if overflow != 0 {
                    return Err(DspError::Capacity);
                }
                Ok([first, second, third])
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            payload_bits,
            soft_bits: ir_bits,
            encoder: control_encoder.clone(),
            assignment,
            control: control_encoder.encode(assignment)?,
            coded_bits: coded_bits.get(),
            sources,
        })
    }

    pub fn assignment(&self) -> ScchAssignment {
        self.assignment
    }

    pub fn transport_bits(&self) -> NonZeroUsize {
        self.payload_bits
    }

    pub fn soft_bits(&self) -> NonZeroUsize {
        self.soft_bits
    }

    pub fn h_rnti(&self) -> u16 {
        self.encoder.h_rnti()
    }

    pub fn select_process(&mut self, process: u8, new_data: bool) -> Result<(), DspError> {
        self.select_assignment(self.assignment.first_code(), process, new_data)
    }

    pub fn select_first_code(&mut self, first_code: u8) -> Result<(), DspError> {
        self.select_assignment(
            first_code,
            self.assignment.harq_process(),
            self.assignment.new_data(),
        )
    }

    pub fn select_first_code_on(
        &mut self,
        first_code: u8,
        control_list_number: u8,
    ) -> Result<(), DspError> {
        let assignment = ScchAssignment::new(
            first_code,
            self.assignment.code_count(),
            self.assignment.modulation(),
            self.assignment.transport_size_index(),
            self.assignment.harq_process(),
            self.assignment.version(),
            self.assignment.new_data(),
        )?
        .with_table(self.assignment.table())?
        .on_control(control_list_number)?;
        let control = self.encoder.encode(assignment)?;
        self.assignment = assignment;
        self.control = control;
        Ok(())
    }

    fn select_assignment(
        &mut self,
        first_code: u8,
        process: u8,
        new_data: bool,
    ) -> Result<(), DspError> {
        let assignment = ScchAssignment::new(
            first_code,
            self.assignment.code_count(),
            self.assignment.modulation(),
            self.assignment.transport_size_index(),
            process,
            self.assignment.version(),
            new_data,
        )?
        .with_table(self.assignment.table())?
        .on_control(self.assignment.control_list_number())?;
        let control = self.encoder.encode(assignment)?;
        self.assignment = assignment;
        self.control = control;
        Ok(())
    }

    pub fn control_bits(&self) -> &[Bit; SCCH_BITS] {
        &self.control
    }

    pub fn physical_bits(&self) -> usize {
        self.sources.len()
    }

    pub fn physical_symbols(&self) -> usize {
        self.physical_bits() / self.assignment.modulation().bits_per_symbol()
    }

    /// Code-major labels store physical symbol bit i in label bit i.
    pub fn prepare_labels(&self, coded: &[Bit], output: &mut [u8]) -> Result<(), DspError> {
        if coded.len() != self.coded_bits || output.len() != self.physical_symbols() {
            return Err(DspError::InvalidLength);
        }
        let width = self.assignment.modulation().bits_per_symbol();
        for (label, sources) in output.iter_mut().zip(self.sources.chunks_exact(width)) {
            *label = sources.iter().enumerate().fold(0, |label, (bit, source)| {
                let [first, second, third] = *source;
                let source = u32::from_le_bytes([first, second, third, 0]);
                let value = (coded[(source >> SOURCE_INDEX_SHIFT) as usize] == Bit::One)
                    ^ (source & INVERT_BIT != 0);
                label | (u8::from(value) << bit)
            });
        }
        Ok(())
    }

    pub fn prepare(&mut self, coded: &[Bit], output: &mut [Bit]) -> Result<(), DspError> {
        if coded.len() != self.coded_bits || output.len() != self.physical_bits() {
            return Err(DspError::InvalidLength);
        }
        for (bit, &[first, second, third]) in output.iter_mut().zip(&self.sources) {
            let source = u32::from_le_bytes([first, second, third, 0]);
            *bit = Bit::from(
                (coded[(source >> SOURCE_INDEX_SHIFT) as usize] == Bit::One)
                    ^ (source & INVERT_BIT != 0),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsdpa::{Modulation, TransmissionVersion};

    #[test]
    fn compiled_mapping_matches_separate_passes_and_survives_late_binding() {
        let count = |value| NonZeroUsize::new(value).unwrap();
        let encoder = ScchEncoder::new(1234).unwrap();
        for modulation in [Modulation::Qpsk, Modulation::Qam16, Modulation::Qam64] {
            let evolved = modulation == Modulation::Qam64;
            let memory = count(if evolved { 43_200 } else { 28_800 });
            for codes in 1..=15 {
                for version in 0..8 {
                    let assignment = ScchAssignment::new(
                        1,
                        codes,
                        modulation,
                        if evolved { 62 } else { 20 },
                        0,
                        TransmissionVersion::new(version).unwrap(),
                        false,
                    )
                    .unwrap();
                    let assignment = if evolved {
                        assignment
                            .with_table(super::super::TransportTable::OctetAligned)
                            .unwrap()
                            .on_control(if codes < 8 { 2 } else { 1 })
                            .unwrap()
                    } else {
                        assignment
                    };
                    let mut plan = TransmissionPlan::new(
                        assignment,
                        Category::new(if evolved { 14 } else { 10 }).unwrap(),
                        &encoder,
                        memory,
                        count(150_000),
                    )
                    .unwrap();
                    let input: Vec<_> = (0..plan.coded_bits)
                        .map(|index| Bit::from((index * 13 + index / 7) % 11 < 5))
                        .collect();
                    let (redundancy, constellation) = assignment.version().parameters(modulation);
                    let matching = HarqRateMatching::new(
                        count(input.len()),
                        memory,
                        modulation,
                        usize::from(codes),
                        redundancy,
                        count(150_000),
                    )
                    .unwrap();
                    let mut collected = vec![Bit::Zero; plan.physical_bits()];
                    let mut expected = collected.clone();
                    matching.apply(&input, &mut collected).unwrap();
                    PhysicalChannelInterleaver::new()
                        .unwrap()
                        .prepare(modulation, constellation, &collected, &mut expected)
                        .unwrap();
                    for (process, new_data) in [(0, false), (5, true)] {
                        plan.select_process(process, new_data).unwrap();
                        let mut actual = vec![Bit::Zero; expected.len()];
                        plan.prepare(&input, &mut actual).unwrap();
                        assert_eq!(actual, expected);
                        let expected_labels: Vec<u8> = expected
                            .chunks_exact(modulation.bits_per_symbol())
                            .map(|bits| {
                                bits.iter()
                                    .enumerate()
                                    .fold(0, |label, (index, &bit)| label | ((bit as u8) << index))
                            })
                            .collect();
                        let mut labels = vec![u8::MAX; plan.physical_symbols()];
                        plan.prepare_labels(&input, &mut labels).unwrap();
                        assert_eq!(labels, expected_labels);
                        let before_labels = labels.clone();
                        assert_eq!(
                            plan.prepare_labels(&input[..input.len() - 1], &mut labels),
                            Err(DspError::InvalidLength)
                        );
                        assert_eq!(labels, before_labels);
                        let short = labels.len() - 1;
                        assert_eq!(
                            plan.prepare_labels(&input, &mut labels[..short]),
                            Err(DspError::InvalidLength)
                        );
                        assert_eq!(labels, before_labels);

                        assert_eq!(
                            plan.control_bits(),
                            &encoder.encode(plan.assignment()).unwrap()
                        );
                        let before = actual.clone();
                        assert!(
                            plan.prepare(&input[..input.len() - 1], &mut actual)
                                .is_err()
                        );
                        assert_eq!(actual, before);
                    }
                }
            }
        }
    }
}
