// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AdmissionError, AdmissionPhase, Transition, addition, recovery, release};

#[derive(Clone, Copy)]
pub(super) enum InitialPhase {
    Preparing,
    ArmingReceive,
    Prefilling,
    ArmingTransmit,
    Starting,
    AwaitingComplete,
    AwaitingCommandAck,
}

impl InitialPhase {
    fn from_phase(phase: AdmissionPhase) -> Option<Self> {
        Some(match phase {
            AdmissionPhase::Preparing => Self::Preparing,
            AdmissionPhase::ArmingReceive => Self::ArmingReceive,
            AdmissionPhase::Prefilling => Self::Prefilling,
            AdmissionPhase::ArmingTransmit => Self::ArmingTransmit,
            AdmissionPhase::Starting => Self::Starting,
            AdmissionPhase::AwaitingComplete => Self::AwaitingComplete,
            AdmissionPhase::AwaitingCommandAck => Self::AwaitingCommandAck,
            _ => return None,
        })
    }

    fn phase(self) -> AdmissionPhase {
        match self {
            Self::Preparing => AdmissionPhase::Preparing,
            Self::ArmingReceive => AdmissionPhase::ArmingReceive,
            Self::Prefilling => AdmissionPhase::Prefilling,
            Self::ArmingTransmit => AdmissionPhase::ArmingTransmit,
            Self::Starting => AdmissionPhase::Starting,
            Self::AwaitingComplete => AdmissionPhase::AwaitingComplete,
            Self::AwaitingCommandAck => AdmissionPhase::AwaitingCommandAck,
        }
    }
}

pub(super) enum Lifecycle {
    Initial(InitialPhase),
    Active(Transition),
    Stopping(Transition),
    Retired(Transition),
}

impl Lifecycle {
    pub(super) fn initial() -> Self {
        Self::Initial(InitialPhase::Preparing)
    }

    pub(super) fn phase(&self) -> AdmissionPhase {
        match self {
            Self::Initial(phase) => phase.phase(),
            Self::Active(_) => AdmissionPhase::Active,
            Self::Stopping(_) => AdmissionPhase::Stopping,
            Self::Retired(_) => AdmissionPhase::Retired,
        }
    }

    pub(super) fn advance(&mut self, next: AdmissionPhase) -> Result<(), AdmissionError> {
        if !matches!(self, Self::Initial(_)) {
            return Err(AdmissionError::Phase);
        }
        *self = if next == AdmissionPhase::Active {
            Self::Active(Transition::Idle)
        } else {
            Self::Initial(InitialPhase::from_phase(next).ok_or(AdmissionError::Phase)?)
        };
        Ok(())
    }

    fn transition(&self) -> Option<&Transition> {
        match self {
            Self::Initial(_) => None,
            Self::Active(value) | Self::Stopping(value) | Self::Retired(value) => Some(value),
        }
    }

    pub(super) fn transition_mut(&mut self) -> Option<&mut Transition> {
        match self {
            Self::Initial(_) => None,
            Self::Active(value) | Self::Stopping(value) | Self::Retired(value) => Some(value),
        }
    }

    pub(super) fn stop(&mut self) {
        let change = self
            .transition_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        *self = Self::Stopping(change);
    }

    pub(super) fn retire(&mut self) {
        let change = self
            .transition_mut()
            .map(std::mem::take)
            .unwrap_or_default();
        *self = Self::Retired(change);
    }

    pub(super) fn is_idle(&self) -> bool {
        matches!(self, Self::Active(Transition::Idle))
    }

    pub(super) fn addition(&self) -> Option<&addition::ScheduledAddition> {
        match self.transition()? {
            Transition::Addition(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn addition_mut(&mut self) -> Option<&mut addition::ScheduledAddition> {
        match self.transition_mut()? {
            Transition::Addition(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn release(&self) -> Option<&release::ScheduledRelease> {
        match self.transition()? {
            Transition::Release(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn release_mut(&mut self) -> Option<&mut release::ScheduledRelease> {
        match self.transition_mut()? {
            Transition::Release(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn recovery(&self) -> Option<&recovery::JoinedRecovery> {
        match self.transition()? {
            Transition::Recovery(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn recovery_mut(&mut self) -> Option<&mut recovery::JoinedRecovery> {
        match self.transition_mut()? {
            Transition::Recovery(value) => Some(value),
            _ => None,
        }
    }

    pub(super) fn take_addition(&mut self) -> Option<addition::ScheduledAddition> {
        let change = self.transition_mut()?;
        if !matches!(change, Transition::Addition(_)) {
            return None;
        }
        match std::mem::take(change) {
            Transition::Addition(value) => Some(*value),
            _ => unreachable!(),
        }
    }

    pub(super) fn take_release(&mut self) -> Option<release::ScheduledRelease> {
        let change = self.transition_mut()?;
        if !matches!(change, Transition::Release(_)) {
            return None;
        }
        match std::mem::take(change) {
            Transition::Release(value) => Some(*value),
            _ => unreachable!(),
        }
    }

    pub(super) fn take_recovery(&mut self) -> Option<recovery::JoinedRecovery> {
        let change = self.transition_mut()?;
        if !matches!(change, Transition::Recovery(_)) {
            return None;
        }
        match std::mem::take(change) {
            Transition::Recovery(value) => Some(*value),
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_admission_cannot_hold_a_bearer_change_or_reenter_after_activation() {
        let mut state = Lifecycle::initial();
        assert_eq!(state.phase(), AdmissionPhase::Preparing);
        assert!(state.transition_mut().is_none());
        assert!(!state.is_idle());
        state.advance(AdmissionPhase::AwaitingCommandAck).unwrap();
        state.advance(AdmissionPhase::Active).unwrap();
        assert!(state.is_idle());
        assert_eq!(
            state.advance(AdmissionPhase::Preparing),
            Err(AdmissionError::Phase)
        );
        assert_eq!(state.phase(), AdmissionPhase::Active);
        state.stop();
        assert_eq!(state.phase(), AdmissionPhase::Stopping);
        assert!(!state.is_idle());
        assert!(state.transition_mut().is_some());
        state.retire();
        assert_eq!(state.phase(), AdmissionPhase::Retired);
    }

    #[test]
    fn stopping_initial_admission_does_not_create_an_active_admission() {
        let mut state = Lifecycle::initial();
        assert_eq!(
            state.advance(AdmissionPhase::Retired),
            Err(AdmissionError::Phase)
        );
        assert_eq!(state.phase(), AdmissionPhase::Preparing);
        state.stop();
        assert_eq!(
            state.advance(AdmissionPhase::Active),
            Err(AdmissionError::Phase)
        );
        assert_eq!(state.phase(), AdmissionPhase::Stopping);
    }
}
