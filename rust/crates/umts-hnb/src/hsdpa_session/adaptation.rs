// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_dsp::hsdpa::Cqi;
use umts_mac::hsdpa::ProcessId;

const MILLI_PER_CQI: i32 = 1000;
const MAX_CQI: i32 = 30;
const HARQ_PROCESSES: usize = 8;
pub(super) const CQI_VALUES: usize = MAX_CQI as usize + 1;
const EXCESS_STEP_MILLI: i32 = 500;
const MAX_EXCESS_STEPS: i32 = 5;
const FIRST_QAM64_CQI: i32 = 26;
const FINE_STEP_MILLI: i32 = 250;
// Eight outstanding first attempts must move the loop by less than one CQI step.
const FEEDBACK_GAIN_MILLI_PER_PERCENT: i32 = 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FeedbackTraining {
    #[default]
    None,
    DecreaseOnly,
    Bidirectional,
}

#[derive(Clone, Copy, Debug)]
enum Calibration {
    Shared(i32),
    PerCqi {
        offsets: [i32; CQI_VALUES],
        last: i32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct LinkAdaptation {
    limit_milli: i32,
    up_milli: i32,
    down_milli: i32,
    calibration: Calibration,
    first_feedback: [FeedbackTraining; HARQ_PROCESSES],
}

impl LinkAdaptation {
    pub fn disabled() -> Self {
        Self::new(0, 10)
    }

    pub fn new(offset_limit: u8, target_bler_percent: u8) -> Self {
        let target = i32::from(target_bler_percent.clamp(1, 50));
        Self {
            limit_milli: i32::from(offset_limit.min(10)) * MILLI_PER_CQI,
            up_milli: target * FEEDBACK_GAIN_MILLI_PER_PERCENT,
            down_milli: (100 - target) * FEEDBACK_GAIN_MILLI_PER_PERCENT,
            calibration: Calibration::Shared(0),
            first_feedback: [FeedbackTraining::None; HARQ_PROCESSES],
        }
    }

    pub fn offset_milli(&self) -> i32 {
        match self.calibration {
            Calibration::Shared(offset) => offset,
            Calibration::PerCqi { last, .. } => last,
        }
    }

    pub fn per_cqi(offset_limit: u8, target_bler_percent: u8) -> Self {
        Self {
            calibration: Calibration::PerCqi {
                offsets: [0; CQI_VALUES],
                last: 0,
            },
            ..Self::new(offset_limit, target_bler_percent)
        }
    }

    #[cfg(test)]
    pub fn reported_offsets(&self) -> Option<[i32; CQI_VALUES]> {
        match self.calibration {
            Calibration::Shared(_) => None,
            Calibration::PerCqi { offsets, .. } => Some(offsets),
        }
    }

    fn offset_for(&self, reported: Cqi) -> i32 {
        match &self.calibration {
            Calibration::Shared(offset) => *offset,
            Calibration::PerCqi { offsets, .. } => offsets[usize::from(reported.value())],
        }
    }

    pub fn excess_steps(&self, reported: Cqi) -> u8 {
        if reported.value() == 0 {
            return 0;
        }
        let excess = i32::from(reported.value()) * MILLI_PER_CQI + self.offset_for(reported)
            - MAX_CQI * MILLI_PER_CQI;
        (excess.max(0) / EXCESS_STEP_MILLI).min(MAX_EXCESS_STEPS) as u8
    }

    pub fn fine_position(&self, reported: Cqi) -> (Cqi, u8) {
        let position = i32::from(reported.value()) * MILLI_PER_CQI + self.offset_for(reported);
        if reported.value() == 0 || position < FIRST_QAM64_CQI * MILLI_PER_CQI {
            return (self.adjust(reported), 0);
        }
        if position >= MAX_CQI * MILLI_PER_CQI {
            return (
                Cqi::new(MAX_CQI as u8).expect("maximum CQI"),
                self.excess_steps(reported),
            );
        }
        (
            Cqi::new((position / MILLI_PER_CQI) as u8).expect("64QAM CQI range"),
            ((position % MILLI_PER_CQI) / FINE_STEP_MILLI) as u8,
        )
    }

    pub fn bind_packet(&mut self, process: ProcessId, training: FeedbackTraining) {
        self.first_feedback[usize::from(process.value())] = training;
    }

    // TS 25.321 11.6.1.3 toggles NDI between packets, not between first and retry attempts.
    pub fn observe_feedback(
        &mut self,
        process: ProcessId,
        classification: Option<Feedback>,
        reported: Option<Cqi>,
    ) {
        let training = std::mem::take(&mut self.first_feedback[usize::from(process.value())]);
        if training == FeedbackTraining::None
            || (training == FeedbackTraining::DecreaseOnly
                && classification != Some(Feedback::Nack))
        {
            return;
        }
        self.observe_new_transmission(classification, reported);
    }

    fn observe_new_transmission(
        &mut self,
        classification: Option<Feedback>,
        reported: Option<Cqi>,
    ) {
        if self.limit_milli == 0 {
            return;
        }
        let step = match classification {
            Some(Feedback::Ack) => self.up_milli,
            Some(Feedback::Nack) => -self.down_milli,
            Some(Feedback::Dtx) | None => return,
        };
        match &mut self.calibration {
            Calibration::Shared(offset) => {
                *offset = (*offset + step).clamp(-self.limit_milli, self.limit_milli);
            }
            Calibration::PerCqi { offsets, last } => {
                let Some(reported) = reported.filter(|value| value.value() != 0) else {
                    return;
                };
                let offset = &mut offsets[usize::from(reported.value())];
                *offset = (*offset + step).clamp(-self.limit_milli, self.limit_milli);
                *last = *offset;
            }
        }
    }

    /// A reported CQI of zero means no transmission and is never raised.
    pub fn adjust(&self, reported: Cqi) -> Cqi {
        let value = i32::from(reported.value());
        if self.limit_milli == 0 || value == 0 {
            return reported;
        }
        let steps =
            (f64::from(self.offset_for(reported)) / f64::from(MILLI_PER_CQI)).round() as i32;
        let adjusted = (value + steps).clamp(1, MAX_CQI);
        Cqi::new(adjusted as u8).expect("adjusted CQI stays within 1..=30")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cqi(value: u8) -> Cqi {
        Cqi::new(value).unwrap()
    }

    #[test]
    fn fine_position_tracks_signed_quarters_without_raising_zero_or_capping_maximum() {
        let mut adaptation = LinkAdaptation::new(3, 10);
        for (offset, base, step) in [
            (-1, 27, 3),
            (0, 28, 0),
            (249, 28, 0),
            (250, 28, 1),
            (500, 28, 2),
            (750, 28, 3),
            (1000, 29, 0),
            (2000, 30, 0),
            (2500, 30, 1),
            (3000, 30, 2),
        ] {
            adaptation.calibration = Calibration::Shared(offset);
            assert_eq!(adaptation.fine_position(cqi(28)), (cqi(base), step));
            assert_eq!(adaptation.fine_position(cqi(0)), (cqi(0), 0));
        }
        adaptation.calibration = Calibration::Shared(-900);
        assert_eq!(
            adaptation.fine_position(cqi(20)),
            (adaptation.adjust(cqi(20)), 0)
        );
        assert_eq!(
            LinkAdaptation::disabled().fine_position(cqi(28)),
            (cqi(28), 0)
        );
    }

    #[test]
    fn excess_rates_use_raw_cqi_and_half_step_thresholds_not_clamped_cqi() {
        let mut adaptation = LinkAdaptation::new(3, 10);
        for offset in 0..=30 {
            adaptation.calibration = Calibration::Shared(offset * 100);
            assert_eq!(adaptation.excess_steps(cqi(30)), (offset / 5).min(5) as u8);
            assert_eq!(
                adaptation.excess_steps(cqi(29)),
                ((offset - 10).max(0) / 5).min(5) as u8
            );
            assert_eq!(
                adaptation.excess_steps(cqi(28)),
                ((offset - 20).max(0) / 5).min(5) as u8
            );
            assert_eq!(adaptation.excess_steps(cqi(0)), 0);
        }
        adaptation.calibration = Calibration::Shared(500);
        assert_eq!(adaptation.adjust(cqi(29)).value(), 30);
        assert_eq!(adaptation.excess_steps(cqi(29)), 0);
        adaptation.calibration = Calibration::Shared(-1000);
        assert_eq!(adaptation.excess_steps(cqi(30)), 0);
    }

    #[test]
    fn disabled_adaptation_returns_the_reported_value_and_ignores_outcomes() {
        let mut adaptation = LinkAdaptation::disabled();
        for _ in 0..100 {
            adaptation.observe_new_transmission(Some(Feedback::Ack), None);
        }
        assert_eq!(adaptation.offset_milli(), 0);
        for value in [0, 1, 28, 30] {
            assert_eq!(adaptation.adjust(cqi(value)).value(), value);
        }
    }

    #[test]
    fn ten_percent_target_balances_nine_acks_against_one_nack() {
        let mut adaptation = LinkAdaptation::new(3, 10);
        for _ in 0..9 {
            adaptation.observe_new_transmission(Some(Feedback::Ack), None);
        }
        assert_eq!(adaptation.offset_milli(), 90);
        assert_eq!(adaptation.adjust(cqi(28)).value(), 28);
        adaptation.observe_new_transmission(Some(Feedback::Nack), None);
        assert_eq!(adaptation.offset_milli(), 0);
        assert_eq!(adaptation.adjust(cqi(28)).value(), 28);
    }

    #[test]
    fn offset_is_clamped_to_the_limit_in_both_directions_and_rounds_to_whole_steps() {
        let mut adaptation = LinkAdaptation::new(2, 10);
        for _ in 0..1000 {
            adaptation.observe_new_transmission(Some(Feedback::Ack), None);
        }
        assert_eq!(adaptation.offset_milli(), 2000);
        assert_eq!(adaptation.adjust(cqi(27)).value(), 29);
        assert_eq!(adaptation.adjust(cqi(30)).value(), 30);
        for _ in 0..1000 {
            adaptation.observe_new_transmission(Some(Feedback::Nack), None);
        }
        assert_eq!(adaptation.offset_milli(), -2000);
        assert_eq!(adaptation.adjust(cqi(28)).value(), 26);
        assert_eq!(adaptation.adjust(cqi(1)).value(), 1);
        assert_eq!(adaptation.adjust(cqi(0)).value(), 0);
        let mut partial = LinkAdaptation::new(3, 10);
        for _ in 0..49 {
            partial.observe_new_transmission(Some(Feedback::Ack), None);
        }
        assert_eq!(partial.offset_milli(), 490);
        assert_eq!(partial.adjust(cqi(20)).value(), 20);
        partial.observe_new_transmission(Some(Feedback::Ack), None);
        assert_eq!(partial.adjust(cqi(20)).value(), 21);
    }

    #[test]
    fn dtx_and_missing_reports_do_not_move_the_offset() {
        let mut adaptation = LinkAdaptation::new(3, 10);
        adaptation.observe_new_transmission(Some(Feedback::Ack), None);
        adaptation.observe_new_transmission(Some(Feedback::Dtx), None);
        adaptation.observe_new_transmission(None, None);
        assert_eq!(adaptation.offset_milli(), 10);
    }

    #[test]
    fn first_feedback_counts_both_ndi_values_but_never_their_retries() {
        use crate::test_support::at;
        use std::num::NonZeroU64;
        use umts_mac::hsdpa::HarqProcess;
        use umts_types::UeKey;

        let id = ProcessId::new(0).unwrap();
        let mut process = HarqProcess::new(
            UeKey {
                epoch: at(0).epoch,
                slot: 0,
                generation: NonZeroU64::MIN,
            },
            NonZeroU64::MIN,
            id,
        );
        let mut adaptation = LinkAdaptation::new(3, 10);
        for (packet, ndi) in [false, true].into_iter().enumerate() {
            process.replace(());
            adaptation.bind_packet(id, FeedbackTraining::Bidirectional);
            let first = process
                .schedule(
                    at(packet as u64 * 80_000),
                    at(packet as u64 * 80_000 + 30_000),
                )
                .unwrap();
            assert_eq!(first.new_data, ndi);
            process.transmitted(first.key).unwrap();
            process
                .feedback(first.key, first.expected_ack, Feedback::Nack)
                .unwrap();
            adaptation.observe_feedback(id, Some(Feedback::Nack), None);
            let expected = -90 * (packet as i32 + 1);
            assert_eq!(adaptation.offset_milli(), expected);
            let retry = process
                .schedule(
                    at(packet as u64 * 80_000 + 40_000),
                    at(packet as u64 * 80_000 + 70_000),
                )
                .unwrap();
            assert_eq!(retry.new_data, ndi);
            process.transmitted(retry.key).unwrap();
            process
                .feedback(retry.key, retry.expected_ack, Feedback::Ack)
                .unwrap();
            adaptation.observe_feedback(id, Some(Feedback::Ack), None);
            assert_eq!(adaptation.offset_milli(), expected);
        }
    }

    #[test]
    fn first_feedback_is_per_process_and_missing_does_not_promote_a_retry() {
        let zero = ProcessId::new(0).unwrap();
        let one = ProcessId::new(1).unwrap();
        let mut adaptation = LinkAdaptation::new(3, 10);
        adaptation.bind_packet(zero, FeedbackTraining::Bidirectional);
        adaptation.bind_packet(one, FeedbackTraining::Bidirectional);
        adaptation.observe_feedback(zero, None, None);
        adaptation.observe_feedback(zero, Some(Feedback::Ack), None);
        assert_eq!(adaptation.offset_milli(), 0);
        adaptation.observe_feedback(one, Some(Feedback::Nack), None);
        assert_eq!(adaptation.offset_milli(), -90);
        adaptation.bind_packet(zero, FeedbackTraining::None);
        adaptation.observe_feedback(zero, Some(Feedback::Nack), None);
        assert_eq!(adaptation.offset_milli(), -90);
        adaptation.bind_packet(zero, FeedbackTraining::Bidirectional);
        adaptation.observe_feedback(zero, Some(Feedback::Ack), None);
        assert_eq!(adaptation.offset_milli(), -80);
    }

    #[test]
    fn one_outstanding_batch_cannot_cross_a_full_cqi_step() {
        for target in 1..=50 {
            for feedback in [Feedback::Ack, Feedback::Nack] {
                let mut adaptation = LinkAdaptation::new(3, target);
                for _ in 0..HARQ_PROCESSES {
                    adaptation.observe_new_transmission(Some(feedback), None);
                }
                assert!(adaptation.offset_milli().abs() < MILLI_PER_CQI);
            }
        }
    }

    #[test]
    fn per_cqi_feedback_updates_only_its_original_reported_quality_once() {
        let zero = ProcessId::new(0).unwrap();
        let one = ProcessId::new(1).unwrap();
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        adaptation.bind_packet(zero, FeedbackTraining::Bidirectional);
        adaptation.bind_packet(one, FeedbackTraining::Bidirectional);
        adaptation.observe_feedback(zero, Some(Feedback::Nack), Some(cqi(29)));
        assert_eq!(adaptation.offset_for(cqi(29)), -90);
        assert_eq!(adaptation.offset_for(cqi(30)), 0);
        adaptation.observe_feedback(zero, Some(Feedback::Ack), Some(cqi(30)));
        assert_eq!(adaptation.offset_for(cqi(30)), 0);
        adaptation.observe_feedback(one, Some(Feedback::Ack), Some(cqi(30)));
        assert_eq!(adaptation.offset_for(cqi(29)), -90);
        assert_eq!(adaptation.offset_for(cqi(30)), 10);
    }

    #[test]
    fn per_cqi_missing_tags_erasures_and_nonlimited_packets_do_not_train() {
        let id = ProcessId::new(0).unwrap();
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        for (limited, result, reported) in [
            (true, Some(Feedback::Nack), None),
            (true, Some(Feedback::Ack), Some(cqi(0))),
            (true, Some(Feedback::Dtx), Some(cqi(29))),
            (true, None, Some(cqi(30))),
            (false, Some(Feedback::Nack), Some(cqi(30))),
        ] {
            adaptation.bind_packet(
                id,
                if limited {
                    FeedbackTraining::Bidirectional
                } else {
                    FeedbackTraining::None
                },
            );
            adaptation.observe_feedback(id, result, reported);
            adaptation.observe_feedback(id, Some(Feedback::Ack), Some(cqi(30)));
        }
        assert_eq!(adaptation.reported_offsets(), Some([0; CQI_VALUES]));
    }

    #[test]
    fn constrained_first_feedback_can_only_decrease_and_is_consumed_once() {
        let process = ProcessId::new(0).unwrap();
        let reported = cqi(30);
        let mut adaptation = LinkAdaptation::per_cqi(3, 10);
        for first in [Some(Feedback::Ack), Some(Feedback::Dtx), None] {
            adaptation.bind_packet(process, FeedbackTraining::DecreaseOnly);
            adaptation.observe_feedback(process, first, Some(reported));
            adaptation.observe_feedback(process, Some(Feedback::Nack), Some(reported));
            assert_eq!(adaptation.offset_for(reported), 0);
        }
        adaptation.bind_packet(process, FeedbackTraining::DecreaseOnly);
        adaptation.observe_feedback(process, Some(Feedback::Nack), Some(reported));
        assert_eq!(adaptation.offset_for(reported), -90);
        adaptation.observe_feedback(process, Some(Feedback::Nack), Some(reported));
        adaptation.observe_feedback(process, Some(Feedback::Ack), Some(reported));
        assert_eq!(adaptation.offset_for(reported), -90);
        assert_eq!(adaptation.offset_for(cqi(29)), 0);
        adaptation.bind_packet(process, FeedbackTraining::Bidirectional);
        adaptation.observe_feedback(process, Some(Feedback::Ack), Some(reported));
        assert_eq!(adaptation.offset_for(reported), -80);
    }

    #[test]
    fn per_cqi_limits_and_rate_selection_are_independent() {
        let mut adaptation = LinkAdaptation::per_cqi(2, 10);
        for _ in 0..300 {
            adaptation.observe_new_transmission(Some(Feedback::Ack), Some(cqi(29)));
            adaptation.observe_new_transmission(Some(Feedback::Nack), Some(cqi(30)));
        }
        assert_eq!(adaptation.offset_for(cqi(29)), 2000);
        assert_eq!(adaptation.offset_for(cqi(30)), -2000);
        assert_eq!(adaptation.fine_position(cqi(29)), (cqi(30), 2));
        assert_eq!(adaptation.fine_position(cqi(30)), (cqi(28), 0));
        assert_eq!(adaptation.adjust(cqi(0)), cqi(0));
    }
}
