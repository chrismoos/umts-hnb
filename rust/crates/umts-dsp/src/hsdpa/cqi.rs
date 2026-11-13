// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Cqi, Modulation, ScchAssignment, TransmissionVersion, TransportTable};
use crate::DspError;
use std::num::NonZeroUsize;
use umts_types::hsdpa::Category;

const MAX_CQI: u8 = 30;
const LAST_QPSK_CQI: u8 = 15;
const TRANSPORT_SIZE_INDICES: u8 = 64;
const REFERENCE_VERSION: u8 = 0;

// TS 25.214 6A.2.3 tables 7A–7E share these rows up to each table's saturation point.
const BLOCK_BITS: [u16; MAX_CQI as usize] = [
    137, 173, 233, 317, 377, 461, 650, 792, 931, 1262, 1483, 1742, 2279, 2583, 3319, 3565, 4189,
    4664, 5287, 5887, 6554, 7168, 9719, 11418, 14411, 17237, 21754, 23370, 24222, 25558,
];
const CODES: [u8; MAX_CQI as usize] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 5, 5, 5, 5, 5, 5, 5, 5, 7, 8, 10, 12, 15, 15, 15, 15,
];
const OCTET_BLOCK_BITS: [u16; MAX_CQI as usize] = [
    136, 176, 232, 320, 376, 464, 648, 792, 928, 1264, 1488, 1744, 2288, 2592, 3328, 3576, 4200,
    4672, 5296, 5896, 6568, 7184, 9736, 11432, 14424, 15776, 21768, 26504, 32264, 38576,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CqiReference {
    pub transport_bits: NonZeroUsize,
    pub codes: u8,
    pub modulation: Modulation,
    pub power_adjustment_db: i8,
    pub reference_ir_bits: NonZeroUsize,
    pub table: TransportTable,
}

impl Cqi {
    // TS 25.214 table 7a selects the reference table by category, not by configured HARQ memory.
    pub fn reference(self, category: Category) -> Option<CqiReference> {
        if self.value() == 0 {
            return None;
        }
        if category.limits().qam64 {
            // TS 25.214 Tables 7F–7G use octet-aligned blocks for every modulation.
            let row = if matches!(category.value(), 13 | 19 | 23) {
                self.value().min(29)
            } else {
                self.value()
            };
            let codes = match row {
                26 => 10,
                27 => 12,
                28 => 13,
                29 => 14,
                30 => 15,
                _ => CODES[usize::from(row - 1)],
            };
            return Some(CqiReference {
                transport_bits: NonZeroUsize::new(usize::from(
                    OCTET_BLOCK_BITS[usize::from(row - 1)],
                ))
                .expect("nonzero CQI block size"),
                codes,
                modulation: match row {
                    1..=15 => Modulation::Qpsk,
                    16..=25 => Modulation::Qam16,
                    _ => Modulation::Qam64,
                },
                power_adjustment_db: if matches!(category.value(), 13 | 19 | 23)
                    && self.value() == 30
                {
                    -2
                } else {
                    0
                },
                reference_ir_bits: NonZeroUsize::new(43200).expect("nonzero reference IR size"),
                table: TransportTable::OctetAligned,
            });
        }
        let (ceiling, ir_bits) = match category.value() {
            1..=6 => (22, 9600),
            7..=8 => (25, 19200),
            9 | 13 | 19 | 21 | 23 => (26, 28800),
            10 | 14 | 20 | 22 | 24 => (MAX_CQI, 28800),
            _ => (LAST_QPSK_CQI, 4800),
        };
        let row = self.value().min(ceiling);
        Some(CqiReference {
            transport_bits: NonZeroUsize::new(usize::from(BLOCK_BITS[usize::from(row - 1)]))
                .expect("nonzero CQI block size"),
            codes: CODES[usize::from(row - 1)],
            modulation: if row <= LAST_QPSK_CQI {
                Modulation::Qpsk
            } else {
                Modulation::Qam16
            },
            power_adjustment_db: -(self.value().saturating_sub(ceiling) as i8),
            reference_ir_bits: NonZeroUsize::new(ir_bits).expect("nonzero reference IR size"),
            table: TransportTable::BitAligned,
        })
    }
}

impl CqiReference {
    // TS 25.214 6A.2.1 divides CPICH-relative total reference power equally across data codes.
    pub fn amplitude_per_code(
        self,
        cpich_rms: f32,
        measurement_offset_db: f32,
    ) -> Result<f32, DspError> {
        if !cpich_rms.is_finite()
            || !(0.0..=1.0).contains(&cpich_rms)
            || cpich_rms == 0.0
            || !measurement_offset_db.is_finite()
            || self.codes == 0
        {
            return Err(DspError::InvalidAmplitude);
        }
        let offset = f64::from(measurement_offset_db) + f64::from(self.power_adjustment_db);
        let amplitude = (f64::from(cpich_rms) * 10.0_f64.powf(offset / 20.0)
            / f64::from(self.codes).sqrt()) as f32;
        if !amplitude.is_finite() || amplitude <= 0.0 || amplitude > 1.0 {
            return Err(DspError::InvalidAmplitude);
        }
        Ok(amplitude)
    }

    pub fn assignment(
        self,
        first_code: u8,
        process: u8,
        new_data: bool,
    ) -> Result<ScchAssignment, DspError> {
        let index = (0..TRANSPORT_SIZE_INDICES)
            .find(|&index| {
                self.table
                    .transport_bits(self.modulation, self.codes, index)
                    == Ok(self.transport_bits)
            })
            .ok_or(DspError::InvalidCode)?;
        ScchAssignment::new(
            first_code,
            self.codes,
            self.modulation,
            index,
            process,
            TransmissionVersion::new(REFERENCE_VERSION)?,
            new_data,
        )?
        .with_table(self.table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_cell_reference_tables_follow_configured_modulation() {
        for (dual, legacy, evolved) in [(23, 9, 13), (24, 10, 14)] {
            let category = umts_types::hsdpa::DualCellCategory::new(dual).unwrap();
            for value in 0..=MAX_CQI {
                let cqi = Cqi::new(value).unwrap();
                assert_eq!(
                    cqi.reference(category.with_qam64(false).unwrap().per_cell_category()),
                    cqi.reference(Category::new(legacy).unwrap()),
                );
                assert_eq!(
                    cqi.reference(category.per_cell_category()),
                    cqi.reference(Category::new(evolved).unwrap()),
                );
            }
        }
    }
    use crate::hsdpa::transport_bits;

    #[test]
    fn normative_category_endpoints_and_power_saturation() {
        for (category, bits, codes, adjustment, memory) in [
            (1, 7168, 5, -8, 9600),
            (6, 7168, 5, -8, 9600),
            (7, 14411, 10, -5, 19200),
            (8, 14411, 10, -5, 19200),
            (9, 17237, 12, -4, 28800),
            (10, 25558, 15, 0, 28800),
            (11, 3319, 5, -15, 4800),
            (12, 3319, 5, -15, 4800),
        ] {
            let category = Category::new(category).unwrap();
            let value = Cqi::new(30).unwrap().reference(category).unwrap();
            assert_eq!(value.transport_bits.get(), bits);
            assert_eq!(value.codes, codes);
            assert_eq!(value.power_adjustment_db, adjustment);
            assert_eq!(value.reference_ir_bits.get(), memory);
        }
    }

    #[test]
    fn every_reference_has_an_exact_scch_size_and_respects_category() {
        for category in 1..=12 {
            let category = Category::new(category).unwrap();
            assert_eq!(Cqi::new(0).unwrap().reference(category), None);
            for cqi in 1..=MAX_CQI {
                let reference = Cqi::new(cqi).unwrap().reference(category).unwrap();
                let assignment = reference.assignment(1, 0, false).unwrap();
                assignment
                    .validate_category(category, reference.transport_bits.get())
                    .unwrap();
                assert_eq!(
                    transport_bits(
                        assignment.modulation(),
                        assignment.code_count(),
                        assignment.transport_size_index()
                    ),
                    Ok(reference.transport_bits)
                );
            }
        }
    }

    #[test]
    fn reference_power_is_total_power_not_power_per_code() {
        let category = Category::new(6).unwrap();
        for cqi in [1, 7, 15, 22, 30] {
            let reference = Cqi::new(cqi).unwrap().reference(category).unwrap();
            let amplitude = reference.amplitude_per_code(0.1, 6.0).unwrap();
            let ratio = f64::from(amplitude).powi(2) * f64::from(reference.codes) / 0.01;
            let expected = 10.0_f64.powf((6.0 + f64::from(reference.power_adjustment_db)) / 10.0);
            assert!((ratio - expected).abs() < 1e-6);
            assert!(reference.amplitude_per_code(0.0, 6.0).is_err());
            assert!(reference.amplitude_per_code(0.1, f32::NAN).is_err());
            assert!(reference.amplitude_per_code(1.0, 100.0).is_err());
        }
    }

    #[test]
    fn larger_single_cell_memory_does_not_change_normative_cqi_reference_memory() {
        for (larger, base) in [(19, 13), (20, 14)] {
            for value in 0..=MAX_CQI {
                let cqi = Cqi::new(value).unwrap();
                assert_eq!(
                    cqi.reference(Category::new(larger).unwrap()),
                    cqi.reference(Category::new(base).unwrap())
                );
            }
        }
    }
}
