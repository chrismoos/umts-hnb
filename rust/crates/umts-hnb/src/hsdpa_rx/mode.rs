// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_dsp::hsdpa::{
    CellAck, CompositeCqi, DualHarqAck, decode_cqi_composite, decode_harq_ack_dual,
};

const SUBFRAME_CHIPS: u64 = 3 * CHIPS_PER_SLOT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackMode {
    Single,
    Dual,
    Ambiguous,
}

#[derive(Clone, Copy, Debug)]
pub struct FeedbackModeSchedule {
    origin: ChipInstant,
    initial: FeedbackMode,
    transition: Option<(ChipInstant, FeedbackMode)>,
}

impl FeedbackModeSchedule {
    pub fn fixed(origin: ChipInstant, mode: FeedbackMode) -> Self {
        Self {
            origin,
            initial: mode,
            transition: None,
        }
    }

    pub fn change_at(
        origin: ChipInstant,
        initial: FeedbackMode,
        effective: ChipInstant,
        mode: FeedbackMode,
    ) -> Result<Self, DspError> {
        if effective.epoch != origin.epoch
            || effective
                .index
                .checked_sub(origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(SUBFRAME_CHIPS))
        {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(Self {
            origin,
            initial,
            transition: Some((effective, mode)),
        })
    }

    pub fn mode_at(self, start: ChipInstant) -> Result<FeedbackMode, DspError> {
        if start.epoch != self.origin.epoch
            || start
                .index
                .checked_sub(self.origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(SUBFRAME_CHIPS))
        {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(self
            .transition
            .filter(|(at, _)| start.index >= at.index)
            .map_or(self.initial, |(_, mode)| mode))
    }

    pub(crate) fn window(
        self,
        start: ChipInstant,
        repetitions: u8,
    ) -> Result<FeedbackMode, DspError> {
        if !(1..=MAX_REPETITIONS).contains(&repetitions) {
            return Err(DspError::InvalidLength);
        }
        let first = self.mode_at(start)?;
        let last = ChipInstant {
            index: start
                .index
                .checked_add(u64::from(repetitions - 1) * SUBFRAME_CHIPS)
                .ok_or(DspError::DiscontinuousTime)?,
            ..start
        };
        if first != self.mode_at(last)? {
            return Err(DspError::DiscontinuousTime);
        }
        Ok(first)
    }

    pub(crate) fn matches_window(
        self,
        current: Option<Self>,
        start: ChipInstant,
        repetitions: u8,
    ) -> bool {
        let captured = self.window(start, repetitions).ok();
        captured.is_some()
            && captured == current.and_then(|mode| mode.window(start, repetitions).ok())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarrierAcknowledgements {
    Single(HarqAck),
    Dual(DualHarqAck),
}

impl CarrierAcknowledgements {
    pub fn cells(self) -> [Option<CellAck>; 2] {
        match self {
            Self::Single(HarqAck::Ack) => [Some(CellAck::Ack), None],
            Self::Single(HarqAck::Nack) => [Some(CellAck::Nack), None],
            Self::Single(_) => [None, None],
            Self::Dual(word) => word.cells(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarrierCqi {
    Single(Cqi),
    Dual(CompositeCqi),
}

impl CarrierCqi {
    pub fn cells(self) -> [Option<Cqi>; 2] {
        match self {
            Self::Single(cqi) => [Some(cqi), None],
            Self::Dual(cqi) => [Some(cqi.primary), Some(cqi.secondary)],
        }
    }
}

impl FeedbackReceiver {
    pub fn ack_order(
        &self,
        start: ChipInstant,
        schedule: FeedbackModeSchedule,
    ) -> Result<Readout<CarrierAcknowledgements>, DspError> {
        if schedule.window(start, 1)? != FeedbackMode::Ambiguous {
            return self.ack_cells(start, 1, schedule);
        }
        self.read(start, 1, |soft: &[f32; SLOT_SYMBOLS]| {
            let single = decode_harq_ack(soft)?;
            let dual = decode_harq_ack_dual(soft)?;
            // Table 15C.1 primary-only ACK is identical to the single-cell order ACK.
            let candidate = (single.candidate == Some(HarqAck::Ack)
                && dual.candidate == Some(DualHarqAck::Primary(CellAck::Ack)))
            .then_some(CarrierAcknowledgements::Single(HarqAck::Ack));
            Ok(FeedbackEvidence {
                candidate,
                normalized_correlation: single
                    .normalized_correlation
                    .min(dual.normalized_correlation),
                normalized_margin: single.normalized_margin.min(dual.normalized_margin),
                mean_soft_power: single.mean_soft_power.min(dual.mean_soft_power),
            })
        })
    }

    pub fn ack_cells(
        &self,
        start: ChipInstant,
        repetitions: u8,
        schedule: FeedbackModeSchedule,
    ) -> Result<Readout<CarrierAcknowledgements>, DspError> {
        let mode = schedule.window(start, repetitions)?;
        self.read(
            start,
            repetitions,
            |soft: &[f32; SLOT_SYMBOLS]| match mode {
                FeedbackMode::Single => decode_harq_ack(soft)
                    .map(|evidence| mapped(evidence, CarrierAcknowledgements::Single)),
                FeedbackMode::Dual => decode_harq_ack_dual(soft)
                    .map(|evidence| mapped(evidence, CarrierAcknowledgements::Dual)),
                FeedbackMode::Ambiguous => Ok(unresolved(
                    decode_harq_ack(soft)?,
                    decode_harq_ack_dual(soft)?,
                )),
            },
        )
    }

    pub fn cqi_cells(
        &self,
        start: ChipInstant,
        repetitions: u8,
        schedule: FeedbackModeSchedule,
    ) -> Result<Readout<CarrierCqi>, DspError> {
        let mode = schedule.window(start, repetitions)?;
        let cqi_start = ChipInstant {
            index: start
                .index
                .checked_add(CHIPS_PER_SLOT)
                .ok_or(DspError::DiscontinuousTime)?,
            ..start
        };
        self.read(
            cqi_start,
            repetitions,
            |soft: &[f32; CQI_SYMBOLS]| match mode {
                FeedbackMode::Single => {
                    decode_cqi(soft).map(|evidence| mapped(evidence, CarrierCqi::Single))
                }
                FeedbackMode::Dual => {
                    decode_cqi_composite(soft).map(|evidence| mapped(evidence, CarrierCqi::Dual))
                }
                FeedbackMode::Ambiguous => {
                    Ok(unresolved(decode_cqi(soft)?, decode_cqi_composite(soft)?))
                }
            },
        )
    }
}

fn mapped<A, B>(evidence: FeedbackEvidence<A>, map: impl FnOnce(A) -> B) -> FeedbackEvidence<B> {
    FeedbackEvidence {
        candidate: evidence.candidate.map(map),
        normalized_correlation: evidence.normalized_correlation,
        normalized_margin: evidence.normalized_margin,
        mean_soft_power: evidence.mean_soft_power,
    }
}

// Correlation cannot establish whether an unacknowledged order changed the UE's codebook.
fn unresolved<A, B, C>(
    single: FeedbackEvidence<A>,
    dual: FeedbackEvidence<B>,
) -> FeedbackEvidence<C> {
    FeedbackEvidence {
        candidate: None,
        normalized_correlation: single
            .normalized_correlation
            .max(dual.normalized_correlation),
        normalized_margin: 0.0,
        mean_soft_power: single.mean_soft_power,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;

    #[test]
    fn effective_boundary_selects_codebook_and_repetition_may_not_cross_it() {
        let schedule = FeedbackModeSchedule::change_at(
            at(256),
            FeedbackMode::Single,
            at(256 + SUBFRAME_CHIPS),
            FeedbackMode::Dual,
        )
        .unwrap();
        assert_eq!(schedule.mode_at(at(256)).unwrap(), FeedbackMode::Single);
        assert_eq!(
            schedule.mode_at(at(256 + SUBFRAME_CHIPS)).unwrap(),
            FeedbackMode::Dual
        );
        assert!(schedule.window(at(256), 2).is_err());
        assert!(schedule.mode_at(at(257)).is_err());
    }

    #[test]
    fn absent_secondary_is_not_a_negative_acknowledgement() {
        assert_eq!(
            CarrierAcknowledgements::Dual(DualHarqAck::Primary(CellAck::Ack)).cells(),
            [Some(CellAck::Ack), None]
        );
        let single = decode_harq_ack(&[-1.0; SLOT_SYMBOLS]).unwrap();
        let dual = decode_harq_ack_dual(&[-1.0; SLOT_SYMBOLS]).unwrap();
        let result: FeedbackEvidence<CarrierAcknowledgements> = unresolved(single, dual);
        assert!(result.candidate.is_none());
    }

    #[test]
    fn queued_schedule_is_valid_only_before_intervening_order_boundary() {
        let captured = FeedbackModeSchedule::fixed(at(256), FeedbackMode::Dual);
        let current = FeedbackModeSchedule::change_at(
            at(256),
            FeedbackMode::Dual,
            at(256 + SUBFRAME_CHIPS),
            FeedbackMode::Single,
        )
        .unwrap();
        assert!(captured.matches_window(Some(current), at(256), 1));
        assert!(!captured.matches_window(Some(current), at(256 + SUBFRAME_CHIPS), 1));
        assert!(!captured.matches_window(Some(current), at(256), 2));
        assert!(!captured.matches_window(None, at(256), 1));
    }
}
