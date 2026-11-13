// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{connection_security::CipherPlan, signalling::SignallingError};
use umts_rlc::acknowledged::{NewSduRange, ReceiveCoverage};
use umts_rrc::{
    nas::SignallingBearer,
    security::{ActivationSequences, BearerLayout, Completion, SignallingSet},
};
use umts_security::{
    ciphering::{
        BearerCipherUpdateError, BearerCiphers, CipherActivation, CountAnchor, Position,
        next_occurrence,
    },
    counters::CipherMode,
};
use umts_types::{Direction, UeKey};

const BEARERS: [SignallingBearer; 3] = [
    SignallingBearer::Unacknowledged,
    SignallingBearer::ControlAm,
    SignallingBearer::NasAm,
];

pub(in crate::signalling) struct PendingCipher {
    plan: CipherPlan,
    downlink: [CipherActivation; BEARERS.len()],
}

impl PendingCipher {
    pub fn prepare(
        key: UeKey,
        plan: CipherPlan,
        bank: &BearerCiphers,
        transmit: [u64; BEARERS.len()],
        command: NewSduRange,
    ) -> Result<Self, SignallingError> {
        let layout = BearerLayout::new(SignallingSet::Base, &[])
            .map_err(SignallingError::SecurityProcedure)?;
        if plan.context.origin().ue != key
            || plan.downlink.layout() != layout
            || plan.dpch_cfn.is_some()
            || command.first != transmit[1]
            || command.next <= command.first
        {
            return Err(SignallingError::ConfigurationFailure);
        }
        let mut downlink = [CipherActivation::Sequenced {
            bearer: 1,
            direction: Direction::Downlink,
            anchor: CountAnchor::new(CipherMode::Unacknowledged, Position::Sequence(0), 0)
                .expect("zero UM anchor"),
            first_new: Position::Sequence(0),
        }; BEARERS.len()];
        for ((activation, bearer), next) in downlink.iter_mut().zip(BEARERS).zip(transmit) {
            let mode = layout
                .mode(bearer.identity())
                .map_err(SignallingError::SecurityProcedure)?;
            let first_new = next_occurrence(
                mode,
                plan.downlink
                    .sequence(bearer.identity())
                    .map_err(SignallingError::SecurityProcedure)?,
                Position::Sequence(next),
            )
            .map_err(|error| SignallingError::Cipher(error.into()))?;
            if bearer == SignallingBearer::ControlAm {
                if first_new
                    .get(mode)
                    .map_err(|error| SignallingError::Cipher(error.into()))?
                    < command.next
                {
                    return Err(SignallingError::ConfigurationFailure);
                }
                let first = bank
                    .selected(
                        key,
                        bearer.identity(),
                        Direction::Downlink,
                        Position::Sequence(command.first),
                    )
                    .map_err(SignallingError::Cipher)?;
                let last = bank
                    .selected(
                        key,
                        bearer.identity(),
                        Direction::Downlink,
                        Position::Sequence(command.next - 1),
                    )
                    .map_err(SignallingError::Cipher)?;
                if !matches!((first, last), (umts_security::ciphering::CipherUse::Data { origin: first, .. }, umts_security::ciphering::CipherUse::Data { origin: last, .. }) if first == last)
                {
                    return Err(SignallingError::ConfigurationFailure);
                }
            }
            *activation = CipherActivation::Sequenced {
                bearer: bearer.identity(),
                direction: Direction::Downlink,
                anchor: CountAnchor::from_start(mode, first_new, plan.counters.start())
                    .map_err(|error| SignallingError::Cipher(error.into()))?,
                first_new: Position::Sequence(next),
            };
        }
        Ok(Self { plan, downlink })
    }
    pub fn publish_with<T>(
        &self,
        bank: &mut BearerCiphers,
        publish: impl FnOnce() -> Result<T, SignallingError>,
    ) -> Result<T, SignallingError> {
        update(bank.update_with(self.plan.context.clone(), &self.downlink, publish))
    }
    pub fn admits_downlink(&self, bearer: SignallingBearer, position: u64) -> bool {
        if bearer == SignallingBearer::ControlAm {
            return true;
        }
        self.downlink
            .iter()
            .find_map(|activation| {
                let CipherActivation::Sequenced {
                    bearer: identity,
                    anchor,
                    ..
                } = activation
                else {
                    return None;
                };
                (*identity == bearer.identity()).then_some(anchor.position())
            })
            .is_some_and(|at| matches!(at, Position::Sequence(first) if position < first))
    }
    pub fn complete_with<T>(
        &self,
        key: UeKey,
        bank: &mut BearerCiphers,
        completion: Completion,
        receive: [u64; BEARERS.len()],
        coverage: ReceiveCoverage,
        admit: impl FnOnce() -> Result<T, SignallingError>,
    ) -> Result<T, SignallingError> {
        if completion.origin() != self.plan.context.origin() || completion.origin().ue != key {
            return Err(SignallingError::Origin);
        }
        let sequences = completion
            .cipher()
            .ok_or(SignallingError::ConfigurationFailure)?;
        if sequences.layout() != self.plan.downlink.layout() {
            return Err(SignallingError::ConfigurationFailure);
        }
        let mut activations = self.downlink;
        for ((activation, bearer), next) in activations.iter_mut().zip(BEARERS).zip(receive) {
            let mode = sequences
                .layout()
                .mode(bearer.identity())
                .map_err(SignallingError::SecurityProcedure)?;
            let floor = if bearer == SignallingBearer::ControlAm {
                coverage.first_position()
            } else {
                next
            };
            let first_new = boundary(sequences, bearer, mode, floor)?;
            if bearer == SignallingBearer::ControlAm
                && first_new
                    .get(mode)
                    .map_err(|error| SignallingError::Cipher(error.into()))?
                    <= coverage.last_position()
            {
                return Err(SignallingError::ConfigurationFailure);
            }
            *activation = CipherActivation::Sequenced {
                bearer: bearer.identity(),
                direction: Direction::Uplink,
                anchor: CountAnchor::from_start(mode, first_new, self.plan.counters.start())
                    .map_err(|error| SignallingError::Cipher(error.into()))?,
                first_new: Position::Sequence(next),
            };
        }
        update(bank.update_with(self.plan.context.clone(), &activations, admit))
    }
}

fn boundary(
    sequences: ActivationSequences,
    bearer: SignallingBearer,
    mode: CipherMode,
    next: u64,
) -> Result<Position, SignallingError> {
    next_occurrence(
        mode,
        sequences
            .sequence(bearer.identity())
            .map_err(SignallingError::SecurityProcedure)?,
        Position::Sequence(next),
    )
    .map_err(|error| SignallingError::Cipher(error.into()))
}

fn update<T>(
    result: Result<T, BearerCipherUpdateError<SignallingError>>,
) -> Result<T, SignallingError> {
    result.map_err(|error| match error {
        BearerCipherUpdateError::Cipher(error) => SignallingError::Cipher(error),
        BearerCipherUpdateError::Publication(error) => error,
    })
}

#[cfg(test)]
mod tests;
