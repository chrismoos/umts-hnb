// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_dsp::{DspError, hsdpa::SecondaryCellOrder};
use umts_types::time::{CHIPS_PER_SLOT, ChipInstant};

const SUBFRAME_CHIPS: u64 = 3 * CHIPS_PER_SLOT;
const ORDER_APPLY_DELAY: u64 = 12 * CHIPS_PER_SLOT;
// Inactive cells need not send CQI, so failed quality trials have a bounded retry interval.
const REACTIVATION_COOLDOWN_CHIPS: u64 = 1500 * CHIPS_PER_SLOT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecondaryState {
    Configured,
    Activating,
    Active,
    Deactivating,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodebookExpectation {
    Single,
    Dual,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrderTransmission {
    pub generation: u64,
    pub ordinal: u8,
    pub order: SecondaryCellOrder,
    pub scch_start: ChipInstant,
    pub feedback_start: ChipInstant,
    pub effective: ChipInstant,
    pub codebook_effective: ChipInstant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderProgress {
    Idle,
    Ready,
    Waiting,
    Confirmed,
    Exhausted,
}

#[derive(Clone, Copy, Debug)]
struct Procedure {
    generation: u64,
    order: SecondaryCellOrder,
    attempts: u8,
    accepted: Option<OrderTransmission>,
    earliest_codebook: Option<ChipInstant>,
    progress: OrderProgress,
}

pub struct SecondaryOrderProcedure {
    epoch: ChipInstant,
    feedback_origin: ChipInstant,
    advanced_to: ChipInstant,
    active: bool,
    generation: u64,
    maximum_attempts: u8,
    feedback_timeout_chips: u64,
    procedure: Option<Procedure>,
    reactivation_not_before: Option<ChipInstant>,
}

impl SecondaryOrderProcedure {
    // TS 25.214 6A.1 activates a newly configured secondary at RRC activation.
    pub fn configured_by_rrc(
        activation: ChipInstant,
        feedback_origin: ChipInstant,
        maximum_attempts: u8,
        feedback_timeout_chips: u64,
    ) -> Result<Self, DspError> {
        if activation.epoch != feedback_origin.epoch
            || maximum_attempts == 0
            || feedback_timeout_chips < CHIPS_PER_SLOT
        {
            return Err(DspError::InvalidCode);
        }
        Ok(Self {
            epoch: activation,
            feedback_origin,
            advanced_to: activation,
            active: true,
            generation: 0,
            maximum_attempts,
            feedback_timeout_chips,
            procedure: None,
            reactivation_not_before: None,
        })
    }

    pub fn state(&self) -> SecondaryState {
        match self.procedure {
            Some(procedure) => match procedure.order {
                SecondaryCellOrder::Activate => SecondaryState::Activating,
                SecondaryCellOrder::Deactivate => SecondaryState::Deactivating,
            },
            None if self.active => SecondaryState::Active,
            None => SecondaryState::Configured,
        }
    }

    pub fn automatic_reactivation_allowed(&self, now: ChipInstant) -> bool {
        now.epoch == self.epoch.epoch
            && now.index >= self.advanced_to.index
            && self
                .reactivation_not_before
                .is_none_or(|at| now.index >= at.index)
    }

    pub fn progress(&self) -> OrderProgress {
        self.procedure
            .map_or(OrderProgress::Idle, |procedure| procedure.progress)
    }

    pub fn rebase_feedback_origin(&mut self, origin: ChipInstant) -> Result<(), DspError> {
        if self.procedure.is_some() {
            return Err(DspError::Capacity);
        }
        if origin.epoch != self.epoch.epoch {
            return Err(DspError::DiscontinuousTime);
        }
        self.feedback_origin = origin;
        Ok(())
    }

    pub fn request(&mut self, order: SecondaryCellOrder) -> Result<(), DspError> {
        if self.procedure.is_some() {
            return Err(DspError::Capacity);
        }
        if self.active == (order == SecondaryCellOrder::Activate) {
            return Ok(());
        }
        self.generation = self.generation.checked_add(1).ok_or(DspError::Capacity)?;
        self.procedure = Some(Procedure {
            generation: self.generation,
            order,
            attempts: 0,
            accepted: None,
            earliest_codebook: None,
            progress: OrderProgress::Ready,
        });
        Ok(())
    }

    pub fn prepare(
        &self,
        scch_start: ChipInstant,
        feedback_start: ChipInstant,
    ) -> Result<OrderTransmission, DspError> {
        let procedure = self.procedure.ok_or(DspError::InvalidCode)?;
        if procedure.progress != OrderProgress::Ready {
            return Err(DspError::Capacity);
        }
        if scch_start.epoch != self.epoch.epoch
            || feedback_start.epoch != self.epoch.epoch
            || scch_start.index < self.advanced_to.index
            || feedback_start.index <= scch_start.index
            || procedure
                .accepted
                .is_some_and(|previous| scch_start.index <= previous.scch_start.index)
            || feedback_start
                .index
                .checked_sub(self.feedback_origin.index)
                .is_none_or(|offset| !offset.is_multiple_of(SUBFRAME_CHIPS))
        {
            return Err(DspError::DiscontinuousTime);
        }
        // TS 25.214 6A.1, single uplink. The codebook changes strictly after application.
        let effective = add(scch_start, SUBFRAME_CHIPS + ORDER_APPLY_DELAY)?;
        let offset = effective
            .index
            .checked_sub(self.feedback_origin.index)
            .ok_or(DspError::DiscontinuousTime)?;
        let steps = offset / SUBFRAME_CHIPS + 1;
        let codebook_effective = add(
            self.feedback_origin,
            steps
                .checked_mul(SUBFRAME_CHIPS)
                .ok_or(DspError::DiscontinuousTime)?,
        )?;
        add(feedback_start, self.feedback_timeout_chips)?;
        if procedure.order == SecondaryCellOrder::Deactivate {
            add(codebook_effective, REACTIVATION_COOLDOWN_CHIPS)?;
        }
        Ok(OrderTransmission {
            generation: procedure.generation,
            ordinal: procedure.attempts + 1,
            order: procedure.order,
            scch_start,
            feedback_start,
            effective,
            codebook_effective,
        })
    }

    // Invoke only after the radio accepts this exact transmission. Rejection leaves Ready.
    pub fn accepted(&mut self, transmission: OrderTransmission) -> Result<(), DspError> {
        if self.prepare(transmission.scch_start, transmission.feedback_start)? != transmission {
            return Err(DspError::InvalidCode);
        }
        let procedure = self.procedure.as_mut().ok_or(DspError::InvalidCode)?;
        procedure.attempts = transmission.ordinal;
        procedure.accepted = Some(transmission);
        procedure
            .earliest_codebook
            .get_or_insert(transmission.codebook_effective);
        procedure.progress = OrderProgress::Waiting;
        if transmission.order == SecondaryCellOrder::Deactivate {
            self.reactivation_not_before = Some(add(
                transmission.codebook_effective,
                REACTIVATION_COOLDOWN_CHIPS,
            )?);
        }
        Ok(())
    }

    pub fn acknowledge(
        &mut self,
        transmission: OrderTransmission,
        feedback_start: ChipInstant,
    ) -> bool {
        let Some(procedure) = self.procedure.as_mut() else {
            return false;
        };
        if procedure.progress != OrderProgress::Waiting
            || procedure.accepted != Some(transmission)
            || feedback_start != transmission.feedback_start
        {
            return false;
        }
        procedure.progress = OrderProgress::Confirmed;
        true
    }

    pub fn advance(&mut self, now: ChipInstant) -> Result<OrderProgress, DspError> {
        if now.epoch != self.epoch.epoch || now.index < self.advanced_to.index {
            return Err(DspError::DiscontinuousTime);
        }
        self.advanced_to = now;
        let Some(procedure) = self.procedure.as_mut() else {
            return Ok(OrderProgress::Idle);
        };
        if let Some(transmission) = procedure.accepted {
            match procedure.progress {
                OrderProgress::Confirmed if now.index >= transmission.codebook_effective.index => {
                    self.active = transmission.order == SecondaryCellOrder::Activate;
                    self.procedure = None;
                    return Ok(OrderProgress::Idle);
                }
                OrderProgress::Waiting
                    if now.index
                        >= add(transmission.feedback_start, self.feedback_timeout_chips)?.index =>
                {
                    procedure.progress = if procedure.attempts < self.maximum_attempts {
                        OrderProgress::Ready
                    } else {
                        OrderProgress::Exhausted
                    };
                }
                _ => {}
            }
        }
        Ok(procedure.progress)
    }

    // Lost ACKs do not prove the UE ignored an order. Ambiguous requires both hypotheses
    // or RRC recovery, and must never be interpreted as permission to schedule carrier 2.
    #[cfg(test)]
    pub fn codebook_at(&self, at: ChipInstant) -> Result<CodebookExpectation, DspError> {
        if at.epoch != self.epoch.epoch || at.index < self.advanced_to.index {
            return Err(DspError::DiscontinuousTime);
        }
        let stable = if self.active {
            CodebookExpectation::Dual
        } else {
            CodebookExpectation::Single
        };
        let Some(procedure) = self.procedure else {
            return Ok(stable);
        };
        if procedure
            .earliest_codebook
            .is_none_or(|first| at.index < first.index)
        {
            return Ok(stable);
        }
        if procedure.progress == OrderProgress::Confirmed
            && procedure
                .accepted
                .is_some_and(|tx| at.index >= tx.codebook_effective.index)
        {
            return Ok(if procedure.order == SecondaryCellOrder::Activate {
                CodebookExpectation::Dual
            } else {
                CodebookExpectation::Single
            });
        }
        Ok(CodebookExpectation::Ambiguous)
    }
}

fn add(time: ChipInstant, chips: u64) -> Result<ChipInstant, DspError> {
    Ok(ChipInstant {
        index: time
            .index
            .checked_add(chips)
            .ok_or(DspError::DiscontinuousTime)?,
        ..time
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::at;

    #[test]
    fn deactivation_bounds_backlog_retries_by_radio_time() {
        let mut state =
            SecondaryOrderProcedure::configured_by_rrc(at(0), at(0), 2, SUBFRAME_CHIPS).unwrap();
        state.request(SecondaryCellOrder::Deactivate).unwrap();
        let tx = state.prepare(at(0), at(4 * SUBFRAME_CHIPS)).unwrap();
        state.accepted(tx).unwrap();
        assert!(state.acknowledge(tx, tx.feedback_start));
        state.advance(tx.codebook_effective).unwrap();
        let retry = add(tx.codebook_effective, REACTIVATION_COOLDOWN_CHIPS).unwrap();
        assert!(!state.automatic_reactivation_allowed(tx.codebook_effective));
        assert!(!state.automatic_reactivation_allowed(at(retry.index - 1)));
        assert!(state.automatic_reactivation_allowed(retry));
        assert!(!state.automatic_reactivation_allowed(at(0)));
        let other_epoch = ChipInstant {
            epoch: umts_types::RadioEpoch::new(std::num::NonZeroU64::new(2).unwrap()),
            ..retry
        };
        assert!(!state.automatic_reactivation_allowed(other_epoch));
        state.advance(add(retry, SUBFRAME_CHIPS).unwrap()).unwrap();
        assert!(!state.automatic_reactivation_allowed(retry));
    }

    #[test]
    fn configuration_activates_and_rejected_submission_does_not_arm_timeout() {
        let mut state =
            SecondaryOrderProcedure::configured_by_rrc(at(0), at(0), 2, SUBFRAME_CHIPS).unwrap();
        assert_eq!(state.state(), SecondaryState::Active);
        state.request(SecondaryCellOrder::Deactivate).unwrap();
        let tx = state.prepare(at(0), at(4 * SUBFRAME_CHIPS)).unwrap();
        assert_eq!(tx.effective, at(15 * CHIPS_PER_SLOT));
        assert_eq!(tx.codebook_effective, at(18 * CHIPS_PER_SLOT));
        assert_eq!(
            state.advance(at(100 * SUBFRAME_CHIPS)).unwrap(),
            OrderProgress::Ready
        );
        assert!(!state.acknowledge(tx, tx.feedback_start));
    }

    #[test]
    fn exact_ack_and_effective_boundary_are_required() {
        let mut state =
            SecondaryOrderProcedure::configured_by_rrc(at(0), at(0), 2, SUBFRAME_CHIPS).unwrap();
        state.request(SecondaryCellOrder::Deactivate).unwrap();
        let tx = state.prepare(at(0), at(4 * SUBFRAME_CHIPS)).unwrap();
        state.accepted(tx).unwrap();
        assert!(!state.acknowledge(tx, at(tx.feedback_start.index + 1)));
        assert!(state.acknowledge(tx, tx.feedback_start));
        assert_eq!(
            state
                .codebook_at(at(tx.codebook_effective.index - 1))
                .unwrap(),
            CodebookExpectation::Dual
        );
        assert_eq!(
            state.codebook_at(tx.codebook_effective).unwrap(),
            CodebookExpectation::Single
        );
        state.advance(tx.codebook_effective).unwrap();
        assert_eq!(state.state(), SecondaryState::Configured);
    }

    #[test]
    fn lost_ack_is_ambiguous_and_retry_exhaustion_requires_recovery() {
        let mut state =
            SecondaryOrderProcedure::configured_by_rrc(at(0), at(0), 1, SUBFRAME_CHIPS).unwrap();
        state.request(SecondaryCellOrder::Deactivate).unwrap();
        let tx = state.prepare(at(0), at(4 * SUBFRAME_CHIPS)).unwrap();
        state.accepted(tx).unwrap();
        assert_eq!(
            state.advance(at(10 * SUBFRAME_CHIPS)).unwrap(),
            OrderProgress::Exhausted
        );
        assert_eq!(
            state.codebook_at(at(10 * SUBFRAME_CHIPS)).unwrap(),
            CodebookExpectation::Ambiguous
        );
        assert!(!state.acknowledge(tx, tx.feedback_start));
        assert!(state.request(SecondaryCellOrder::Activate).is_err());
    }
}
