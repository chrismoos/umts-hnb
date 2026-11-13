// SPDX-License-Identifier: AGPL-3.0-or-later

use super::adaptation::FeedbackTraining;
use crate::radio_resources::HsdpaCodeSet;
use umts_dsp::{
    DspError,
    hsdpa::{Cqi, CqiReference, ScchAssignment},
};
use umts_types::hsdpa::Category;

const CQI_VALUES: usize = 31;
const EXCESS_FORMATS: usize = 5;
const LAST_CQI: u8 = 30;
const FIRST_EXCESS_INDEX: u8 = 58;
const FIRST_QAM64_CQI: usize = 26;
const INTERMEDIATE_FORMATS: usize = 3;
const RATE_SUBDIVISIONS: usize = INTERMEDIATE_FORMATS + 1;
const TRANSPORT_SIZE_INDICES: u8 = 64;

#[derive(Clone, Copy, Debug)]
pub struct Rate {
    pub cqi: Cqi,
    pub reference: CqiReference,
    pub assignment: ScchAssignment,
}

impl Rate {
    pub(super) fn allocation_codes(self, available: u8) -> impl Iterator<Item = u8> {
        let maximum = if self.reference.modulation == umts_dsp::hsdpa::Modulation::Qam64 {
            available
        } else {
            available.min(self.reference.codes)
        };
        (self.reference.codes..=maximum).rev()
    }

    pub(super) fn amplitude_per_code(
        self,
        codes: u8,
        cpich_rms: f32,
        gamma_db: f32,
    ) -> Result<f32, DspError> {
        if !self.allocation_codes(codes).any(|allowed| allowed == codes) {
            return Err(DspError::InvalidCode);
        }
        // TS 25.214 6A.2.1 defines total reference power, not a per-code power increase.
        Ok(self.reference.amplitude_per_code(cpich_rms, gamma_db)?
            * (f32::from(self.reference.codes) / f32::from(codes)).sqrt())
    }
}

pub struct Rates {
    codes: HsdpaCodeSet,
    entries: [Option<Rate>; CQI_VALUES],
    intermediate: [[Option<Rate>; INTERMEDIATE_FORMATS]; CQI_VALUES],
    higher: [Option<Rate>; EXCESS_FORMATS],
}

impl Rates {
    pub fn codes(&self) -> HsdpaCodeSet {
        self.codes
    }
    #[cfg(test)]
    pub fn candidates(&self, reported: Cqi) -> impl Iterator<Item = Rate> + '_ {
        self.entries[..=usize::from(reported.value())]
            .iter()
            .rev()
            .flatten()
            .copied()
    }

    pub fn adapted_candidates(
        &self,
        adjusted: Cqi,
        excess_steps: u8,
    ) -> impl Iterator<Item = Rate> + '_ {
        let count = if adjusted.value() == LAST_CQI {
            usize::from(excess_steps).min(EXCESS_FORMATS)
        } else {
            0
        };
        self.higher[..count].iter().rev().flatten().copied().chain(
            (0..=usize::from(adjusted.value()))
                .rev()
                .flat_map(move |index| {
                    let count = if index == usize::from(adjusted.value()) {
                        usize::from(excess_steps).min(INTERMEDIATE_FORMATS)
                    } else {
                        INTERMEDIATE_FORMATS
                    };
                    self.intermediate[index][..count]
                        .iter()
                        .rev()
                        .flatten()
                        .copied()
                        .chain(self.entries[index])
                }),
        )
    }

    pub fn all_candidates(&self) -> impl Iterator<Item = Rate> + '_ {
        self.adapted_candidates(
            Cqi::new(LAST_CQI).expect("maximum CQI"),
            EXCESS_FORMATS as u8,
        )
    }

    pub fn demand_rate(
        &self,
        adjusted: Cqi,
        excess_steps: u8,
        pending_bits: usize,
    ) -> Option<Rate> {
        self.adapted_candidates(adjusted, excess_steps)
            .filter(|rate| rate.reference.transport_bits.get() >= pending_bits)
            .min_by_key(|rate| rate.reference.transport_bits.get())
            .or_else(|| self.adapted_candidates(adjusted, excess_steps).next())
    }

    /// `demand_rate` restricted to transport blocks of at most `limit` bits.
    pub fn demand_rate_within(
        &self,
        adjusted: Cqi,
        excess_steps: u8,
        pending_bits: usize,
        limit: Option<usize>,
    ) -> Option<Rate> {
        let Some(limit) = limit else {
            return self.demand_rate(adjusted, excess_steps, pending_bits);
        };
        let within = || {
            self.adapted_candidates(adjusted, excess_steps)
                .filter(move |rate| rate.reference.transport_bits.get() <= limit)
        };
        within()
            .filter(|rate| rate.reference.transport_bits.get() >= pending_bits)
            .min_by_key(|rate| rate.reference.transport_bits.get())
            .or_else(|| within().max_by_key(|rate| rate.reference.transport_bits.get()))
            .or_else(|| self.demand_rate(adjusted, excess_steps, pending_bits))
    }

    pub fn rate_limited(
        &self,
        adjusted: Cqi,
        excess_steps: u8,
        chosen: Rate,
        pending_bits: usize,
    ) -> bool {
        pending_bits >= chosen.reference.transport_bits.get()
            && self
                .adapted_candidates(adjusted, excess_steps)
                .next()
                .is_some_and(|maximum| maximum.reference == chosen.reference)
    }

    pub fn feedback_training(
        &self,
        adjusted: Cqi,
        excess_steps: u8,
        chosen: Rate,
        pending_bits: usize,
    ) -> FeedbackTraining {
        if pending_bits < chosen.reference.transport_bits.get() {
            FeedbackTraining::None
        } else if self.rate_limited(adjusted, excess_steps, chosen, pending_bits) {
            FeedbackTraining::Bidirectional
        } else {
            FeedbackTraining::DecreaseOnly
        }
    }

    pub fn contains(&self, rate: Rate) -> bool {
        self.all_candidates()
            .any(|known| known.cqi == rate.cqi && known.reference == rate.reference)
    }

    #[cfg(test)]
    pub fn demand_cqi(&self, reported: Cqi, pending_bits: usize) -> Cqi {
        self.candidates(reported)
            .filter(|rate| rate.reference.transport_bits.get() >= pending_bits)
            .min_by_key(|rate| rate.reference.transport_bits.get())
            .map_or(reported, |rate| rate.cqi)
    }

    pub fn encoders(
        &self,
        maximum_coded_bits: std::num::NonZeroUsize,
        method: umts_dsp::hsdpa::CrcMethod,
        maximum_bytes: std::num::NonZeroUsize,
    ) -> Result<crate::hsdpa_packet::EncoderBank, DspError> {
        let formats: Vec<_> = self
            .all_candidates()
            .map(|rate| rate.reference.transport_bits)
            .collect();
        crate::hsdpa_packet::EncoderBank::new(
            &formats,
            std::num::NonZeroUsize::new(formats.len()).ok_or(DspError::InvalidCode)?,
            maximum_coded_bits,
            method,
            maximum_bytes,
        )
    }

    pub fn new(category: Category, codes: HsdpaCodeSet) -> Result<Self, DspError> {
        let mut entries = [None; CQI_VALUES];
        for (value, entry) in entries.iter_mut().enumerate().skip(1) {
            let cqi = Cqi::new(value as u8)?;
            let reference = cqi.reference(category).expect("nonzero CQI");
            if reference.codes > codes.data_count().get() {
                continue;
            }
            let mut assignment = None;
            for first in
                codes.first_data()..=codes.first_data() + codes.data_count().get() - reference.codes
            {
                let candidate = reference.assignment(first, 0, false)?;
                assignment = codes
                    .control_codes()
                    .take(4)
                    .enumerate()
                    .find_map(|(index, _)| candidate.on_control(index as u8 + 1).ok());
                if assignment.is_some() {
                    break;
                }
            }
            let Some(assignment) = assignment else {
                continue;
            };
            assignment.validate_category(category, reference.transport_bits.get())?;
            *entry = Some(Rate {
                cqi,
                reference,
                assignment,
            });
        }
        let mut intermediate = [[None; INTERMEDIATE_FORMATS]; CQI_VALUES];
        if category.limits().qam64 {
            for index in FIRST_QAM64_CQI..usize::from(LAST_CQI) {
                let (Some(lower), Some(upper)) = (entries[index], entries[index + 1]) else {
                    continue;
                };
                let lower_bits = lower.reference.transport_bits.get();
                let upper_bits = upper.reference.transport_bits.get();
                if upper_bits <= lower_bits {
                    continue;
                }
                for (step, entry) in intermediate[index].iter_mut().enumerate() {
                    let target = lower_bits
                        + ((upper_bits - lower_bits) * (step + 1)).div_ceil(RATE_SUBDIVISIONS);
                    let mut reference = upper.reference;
                    let Some(bits) = (0..TRANSPORT_SIZE_INDICES)
                        .filter_map(|size| {
                            reference
                                .table
                                .transport_bits(reference.modulation, reference.codes, size)
                                .ok()
                        })
                        .find(|bits| bits.get() >= target && bits.get() < upper_bits)
                    else {
                        continue;
                    };
                    reference.transport_bits = bits;
                    let assignment = reference
                        .assignment(upper.assignment.first_code(), 0, false)?
                        .on_control(upper.assignment.control_list_number())?;
                    assignment.validate_category(category, bits.get())?;
                    *entry = Some(Rate {
                        cqi: lower.cqi,
                        reference,
                        assignment,
                    });
                }
            }
        }
        let mut higher = [None; EXCESS_FORMATS];
        if (matches!(category.value(), 10 | 22)
            || (category.limits().qam64 && matches!(category.value(), 14 | 20 | 24)))
            && let Some(base) = entries[usize::from(LAST_CQI)]
        {
            for (step, entry) in higher.iter_mut().enumerate() {
                let index = FIRST_EXCESS_INDEX + step as u8;
                let mut reference = base.reference;
                reference.transport_bits =
                    reference
                        .table
                        .transport_bits(reference.modulation, reference.codes, index)?;
                let assignment = ScchAssignment::new(
                    base.assignment.first_code(),
                    reference.codes,
                    reference.modulation,
                    index,
                    0,
                    umts_dsp::hsdpa::TransmissionVersion::new(0)?,
                    false,
                )?
                .with_table(reference.table)?
                .on_control(base.assignment.control_list_number())?;
                assignment.validate_category(category, reference.transport_bits.get())?;
                *entry = Some(Rate {
                    cqi: base.cqi,
                    reference,
                    assignment,
                });
            }
        }
        Ok(Self {
            codes,
            entries,
            intermediate,
            higher,
        })
    }

    #[cfg(test)]
    pub fn select(&self, reported: Cqi, available_codes: u8) -> Option<Rate> {
        self.entries[..=usize::from(reported.value())]
            .iter()
            .rev()
            .flatten()
            .find(|rate| rate.reference.codes <= available_codes)
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        radio_resources::RadioResources,
        test_support::{at, count, source},
    };
    use std::num::NonZeroU8;

    #[test]
    fn category10_saturated_acks_reach_27952_without_exceeding_capability() {
        use super::super::adaptation::LinkAdaptation;
        use umts_mac::hsdpa::{Feedback, ProcessId};
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let category = Category::new(10).unwrap();
        let rates = Rates::new(category, *lease.codes()).unwrap();
        let reported = Cqi::new(30).unwrap();
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        let process = ProcessId::new(0).unwrap();
        assert_eq!(
            rates
                .demand_rate(reported, 0, 100_000)
                .unwrap()
                .reference
                .transport_bits
                .get(),
            25558
        );
        for _ in 0..250 {
            adaptation.bind_packet(process, FeedbackTraining::Bidirectional);
            adaptation.observe_feedback(process, Some(Feedback::Ack), Some(reported));
        }
        let (adjusted, excess) = adaptation.fine_position(reported);
        let maximum = rates.demand_rate(adjusted, excess, 100_000).unwrap();
        assert_eq!(maximum.reference.transport_bits.get(), 27952);
        for (step, expected) in [26020, 26490, 26969, 27456, 27952].into_iter().enumerate() {
            assert_eq!(
                rates
                    .demand_rate(reported, step as u8 + 1, 100_000)
                    .unwrap()
                    .reference
                    .transport_bits
                    .get(),
                expected
            );
        }
        for rate in rates.all_candidates() {
            rate.assignment
                .validate_category(category, rate.reference.transport_bits.get())
                .unwrap();
        }
        adaptation.bind_packet(process, FeedbackTraining::Bidirectional);
        adaptation.observe_feedback(process, Some(Feedback::Nack), Some(reported));
        assert!(adaptation.offset_milli() < 2500);
        let (adjusted, excess) = adaptation.fine_position(reported);
        assert_eq!(
            rates
                .demand_rate(adjusted, excess, 100_000)
                .unwrap()
                .reference
                .transport_bits
                .get(),
            27456
        );
    }

    #[test]
    fn category24_saturated_acks_reach_42192_per_cell() {
        use crate::hsdpa_session::adaptation::LinkAdaptation;
        use umts_mac::hsdpa::{Feedback, ProcessId};
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let category = umts_types::hsdpa::DualCellCategory::new(24)
            .unwrap()
            .per_cell_category();
        let rates = Rates::new(category, *lease.codes()).unwrap();
        let reported = Cqi::new(30).unwrap();
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        assert_eq!(
            rates
                .demand_rate(reported, 0, 100_000)
                .unwrap()
                .reference
                .transport_bits
                .get(),
            38576
        );
        let process = ProcessId::new(0).unwrap();
        for _ in 0..250 {
            adaptation.bind_packet(process, FeedbackTraining::Bidirectional);
            adaptation.observe_feedback(process, Some(Feedback::Ack), Some(reported));
        }
        let (adjusted, excess) = adaptation.fine_position(reported);
        assert_eq!(excess, 5);
        let maximum = rates.demand_rate(adjusted, excess, 100_000).unwrap();
        assert_eq!(maximum.reference.transport_bits.get(), 42192);
        assert!(
            rates
                .all_candidates()
                .any(|rate| rate.reference == maximum.reference)
        );
    }

    #[test]
    fn constrained_format_nacks_train_down_without_training_demand_limited_packets() {
        use crate::hsdpa_session::adaptation::LinkAdaptation;
        use umts_mac::hsdpa::{Feedback, ProcessId};
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let rates = Rates::new(Category::new(14).unwrap(), *lease.codes()).unwrap();
        let reported = Cqi::new(30).unwrap();
        let maximum = rates.adapted_candidates(reported, 0).next().unwrap();
        let constrained = rates
            .adapted_candidates(reported, 0)
            .find(|rate| rate.reference.transport_bits.get() == 35920)
            .unwrap();
        assert!(constrained.reference.transport_bits < maximum.reference.transport_bits);
        let pending = maximum.reference.transport_bits.get();
        assert_eq!(
            rates.feedback_training(reported, 0, maximum, pending),
            FeedbackTraining::Bidirectional
        );
        let training = rates.feedback_training(reported, 0, constrained, pending);
        assert_eq!(training, FeedbackTraining::DecreaseOnly);
        assert_eq!(
            rates.feedback_training(
                reported,
                0,
                constrained,
                constrained.reference.transport_bits.get() - 1,
            ),
            FeedbackTraining::None
        );
        let process = ProcessId::new(0).unwrap();
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        adaptation.bind_packet(process, training);
        adaptation.observe_feedback(process, Some(Feedback::Ack), Some(reported));
        assert_eq!(adaptation.reported_offsets().unwrap()[30], 0);
        adaptation.bind_packet(process, training);
        adaptation.observe_feedback(process, Some(Feedback::Nack), Some(reported));
        assert_eq!(adaptation.reported_offsets().unwrap()[30], -90);
    }

    #[test]
    fn retained_payload_demand_keeps_small_retries_qpsk_without_capping_large_blocks() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let rates = Rates::new(Category::new(14).unwrap(), *lease.codes()).unwrap();
        let cqi = Cqi::new(30).unwrap();
        for bits in [2288, 40704, 41440, 42192] {
            let ceiling = rates.demand_rate(cqi, 5, bits).unwrap();
            let selected = rates
                .adapted_candidates(cqi, 5)
                .find(|rate| rate.reference.transport_bits <= ceiling.reference.transport_bits)
                .unwrap();
            assert_eq!(selected.reference.transport_bits.get(), bits);
            if bits == 2288 {
                assert_eq!(
                    selected.reference.modulation,
                    umts_dsp::hsdpa::Modulation::Qpsk
                );
                assert_eq!(selected.reference.codes, 4);
                assert_ne!(selected.assignment.transport_size_index(), 63);
            }
        }
        let reduced = Cqi::new(26).unwrap();
        let maximum = rates.adapted_candidates(reduced, 0).next().unwrap();
        assert_eq!(
            rates.demand_rate(reduced, 0, 42192).unwrap().reference,
            maximum.reference
        );
        assert!(rates.demand_rate(Cqi::new(0).unwrap(), 5, 2288).is_none());
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn spare_codes_keep_reference_tables_and_total_power_unchanged() {
        for value in 1..=LAST_CQI {
            let cqi = Cqi::new(value).unwrap();
            let reference = cqi.reference(Category::new(14).unwrap()).unwrap();
            let rate = Rate {
                cqi,
                reference,
                assignment: reference.assignment(1, 0, false).unwrap(),
            };
            let counts: Vec<_> = rate.allocation_codes(15).collect();
            if value >= 26 {
                assert_eq!(counts.first(), Some(&15));
                assert_eq!(counts.last(), Some(&reference.codes));
            } else {
                assert_eq!(counts, vec![reference.codes]);
            }
            let baseline = f64::from(reference.amplitude_per_code(0.097, 12.0).unwrap()).powi(2)
                * f64::from(reference.codes);
            for codes in counts {
                let amplitude = rate.amplitude_per_code(codes, 0.097, 12.0).unwrap();
                let actual = f64::from(amplitude).powi(2) * f64::from(codes);
                assert!((actual / baseline - 1.0).abs() < 1e-6);
            }
            assert!(rate.allocation_codes(reference.codes - 1).next().is_none());
            assert!(
                rate.amplitude_per_code(reference.codes - 1, 0.097, 12.0)
                    .is_err()
            );
            assert_eq!(rate.reference, reference);
        }
        let cqi = Cqi::new(30).unwrap();
        let reference = cqi.reference(Category::new(6).unwrap()).unwrap();
        let rate = Rate {
            cqi,
            reference,
            assignment: reference.assignment(1, 0, false).unwrap(),
        };
        assert_eq!(rate.allocation_codes(15).collect::<Vec<_>>(), vec![5]);
        assert!(rate.amplitude_per_code(15, 0.1, 12.0).is_err());
    }

    #[test]
    fn category_fourteen_excess_steps_reach_the_normative_ceiling_without_extra_power() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        let rates = Rates::new(Category::new(14).unwrap(), *lease.codes()).unwrap();
        let cqi = Cqi::new(30).unwrap();
        let baseline = rates.adapted_candidates(cqi, 0).next().unwrap();
        assert_eq!(baseline.reference.transport_bits.get(), 38576);
        let baseline_power = baseline.reference.amplitude_per_code(0.1, 3.0).unwrap();
        for (step, bits) in [39272, 39984, 40704, 41440, 42192].into_iter().enumerate() {
            let rate = rates
                .adapted_candidates(cqi, step as u8 + 1)
                .next()
                .unwrap();
            assert_eq!(rate.cqi.value(), 30);
            assert_eq!(rate.reference.transport_bits.get(), bits);
            assert_eq!(
                rate.assignment.transport_size_index(),
                FIRST_EXCESS_INDEX + step as u8
            );
            assert_eq!(
                rate.reference.amplitude_per_code(0.1, 3.0).unwrap(),
                baseline_power
            );
            assert!(rates.rate_limited(cqi, step as u8 + 1, rate, bits));
            assert!(!rates.rate_limited(cqi, step as u8 + 1, rate, bits - 1));
        }
        let sparse = rates.demand_rate(cqi, 5, 40000).unwrap();
        assert_eq!(sparse.reference.transport_bits.get(), 40704);
        assert!(!rates.rate_limited(cqi, 5, sparse, 40000));
        assert_eq!(
            rates
                .demand_rate(cqi, 5, usize::MAX)
                .unwrap()
                .reference
                .transport_bits
                .get(),
            42192
        );
        assert_eq!(
            rates
                .adapted_candidates(Cqi::new(29).unwrap(), 5)
                .next()
                .unwrap()
                .reference
                .transport_bits
                .get(),
            37224
        );
        assert!(
            rates
                .adapted_candidates(Cqi::new(0).unwrap(), 5)
                .next()
                .is_none()
        );
        let mut encoders = rates
            .encoders(
                count(150_000),
                umts_dsp::hsdpa::CrcMethod::Method1,
                count(32_000_000),
            )
            .unwrap();
        assert!(encoders.get_mut(count(42192)).is_some());
        let legacy = Rates::new(Category::new(10).unwrap(), *lease.codes()).unwrap();
        assert_eq!(
            legacy
                .adapted_candidates(cqi, 5)
                .next()
                .unwrap()
                .reference
                .transport_bits
                .get(),
            27952
        );
        let thirteen = Rates::new(Category::new(13).unwrap(), *lease.codes()).unwrap();
        assert!(thirteen.adapted_candidates(cqi, 5).all(|rate| {
            rate.reference.modulation != umts_dsp::hsdpa::Modulation::Qam64
                || rate.assignment.transport_size_index() <= 52
        }));
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn fine_steps_are_normative_transport_sizes_with_constant_power_and_full_ceiling() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::new(2).unwrap())
            .unwrap();
        for category in [13, 14, 19, 20] {
            let category = Category::new(category).unwrap();
            let rates = Rates::new(category, *lease.codes()).unwrap();
            for value in FIRST_QAM64_CQI as u8..LAST_CQI {
                let cqi = Cqi::new(value).unwrap();
                let lower = cqi.reference(category).unwrap();
                let upper = Cqi::new(value + 1).unwrap().reference(category).unwrap();
                let mut previous = lower.transport_bits.get();
                for step in 1..=INTERMEDIATE_FORMATS as u8 {
                    let rate = rates.adapted_candidates(cqi, step).next().unwrap();
                    let bits = rate.reference.transport_bits.get();
                    assert!(bits >= previous);
                    if upper.transport_bits > lower.transport_bits {
                        assert!(bits > lower.transport_bits.get());
                        assert!(bits < upper.transport_bits.get());
                    } else {
                        assert_eq!(bits, lower.transport_bits.get());
                    }
                    previous = bits;
                    assert_eq!(
                        rate.reference.table.transport_bits(
                            rate.assignment.modulation(),
                            rate.assignment.code_count(),
                            rate.assignment.transport_size_index()
                        ),
                        Ok(rate.reference.transport_bits)
                    );
                    let baseline = f64::from(lower.amplitude_per_code(0.1, 12.0).unwrap()).powi(2)
                        * f64::from(lower.codes);
                    let actual =
                        f64::from(rate.amplitude_per_code(15, 0.1, 12.0).unwrap()).powi(2) * 15.0;
                    assert!((actual / baseline - 1.0).abs() < 1e-6);
                    assert!(rates.contains(rate));
                }
            }
            assert!(
                rates
                    .all_candidates()
                    .all(|rate| rate.reference.transport_bits.get()
                        <= category.limits().maximum_transport_bits)
            );
        }
        let rates = Rates::new(Category::new(14).unwrap(), *lease.codes()).unwrap();
        for (step, bits) in [26504, 27960, 29504, 31128].into_iter().enumerate() {
            let rate = rates
                .demand_rate(Cqi::new(28).unwrap(), step as u8, usize::MAX)
                .unwrap();
            assert_eq!(rate.reference.transport_bits.get(), bits);
        }
        assert_eq!(
            rates
                .demand_rate(Cqi::new(30).unwrap(), 5, usize::MAX)
                .unwrap()
                .reference
                .transport_bits
                .get(),
            42192
        );
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn fifteen_code_category_ten_demand_reaches_the_cqi_thirty_block() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(15).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let rates = Rates::new(Category::new(10).unwrap(), *lease.codes()).unwrap();
        let top = Cqi::new(30).unwrap();
        assert_eq!(rates.demand_cqi(top, 45_941).value(), 30);
        assert_eq!(rates.demand_cqi(top, 24_000).value(), 29);
        assert_eq!(rates.demand_cqi(top, 23_000).value(), 28);
        let best = rates.candidates(top).next().unwrap();
        assert_eq!(best.reference.transport_bits.get(), 25_558);
        assert_eq!(best.reference.codes, 15);
        let mut encoders = rates
            .encoders(
                count(100_000),
                umts_dsp::hsdpa::CrcMethod::Method1,
                count(16_000_000),
            )
            .unwrap();
        assert!(encoders.get_mut(best.reference.transport_bits).is_some());
        assert!(encoders.get_mut(count(24_222)).is_some());
        resources.release_hsdpa(lease).unwrap();
    }

    #[test]
    fn shared_code_budget_limits_format_without_reinterpreting_reported_cqi() {
        let source = source(true);
        let mut resources =
            RadioResources::new(at(0).epoch, source.information(), count(2), 0..=1).unwrap();
        let lease = resources
            .reserve_hsdpa(NonZeroU8::new(5).unwrap(), NonZeroU8::MIN)
            .unwrap();
        let rates = Rates::new(Category::new(10).unwrap(), *lease.codes()).unwrap();
        let high = Cqi::new(30).unwrap();
        let sparse = rates.demand_cqi(high, 341);
        assert!(sparse.value() < high.value());
        let sparse_rate = rates.candidates(sparse).next().unwrap();
        assert!(sparse_rate.reference.transport_bits.get() >= 341);
        assert!(rates.candidates(high).all(|rate| {
            rate.reference.transport_bits.get() < 341
                || rate.reference.transport_bits.get() >= sparse_rate.reference.transport_bits.get()
        }));
        assert_eq!(rates.demand_cqi(high, usize::MAX), high);
        assert_eq!(rates.demand_cqi(Cqi::new(0).unwrap(), 341).value(), 0);
        let selected = rates.select(high, 5).unwrap();
        assert_eq!(selected.cqi.value(), 22);
        assert_eq!(selected.reference.transport_bits.get(), 7168);
        assert_eq!(selected.assignment.first_code(), lease.codes().first_data());
        assert_eq!(rates.select(high, 1).unwrap().cqi.value(), 6);
        assert!(rates.select(high, 0).is_none());
        assert!(rates.select(Cqi::new(0).unwrap(), 5).is_none());

        let qpsk = Rates::new(Category::new(12).unwrap(), *lease.codes()).unwrap();
        let selected = qpsk.select(high, 5).unwrap();
        assert_eq!(selected.cqi.value(), 30);
        assert_eq!(selected.reference.transport_bits.get(), 3319);
        assert_eq!(selected.reference.power_adjustment_db, -15);
        assert_eq!(
            selected.reference.modulation,
            umts_dsp::hsdpa::Modulation::Qpsk
        );
        resources.release_hsdpa(lease).unwrap();
    }
}
