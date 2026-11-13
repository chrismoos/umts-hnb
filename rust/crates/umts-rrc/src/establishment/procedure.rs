// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    AssignedRntis, CellFachSetup, DomainStarts, EstablishmentError, TransactionId,
    build_cell_fach_setup,
};
use std::num::NonZeroU64;
use umts_rrc_codec::{DecodedUplinkCommon, DecodedUplinkDedicated, EncodedRrc};
use umts_rrc_schema::{
    class_definitions::{ULCCCHMessageType, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{UeKey, time::ChipInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstablishmentPhase {
    Prepared,
    AwaitingComplete,
    Established,
    Closed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Timeout,
    PublicationCancelled,
    LowerLayerFailure,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Established,
    Duplicate,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestMatch {
    DifferentIdentity,
    Retransmission,
    ConfigurationFailure,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcedureError {
    Origin,
    Time,
    Phase,
    Message,
    Transaction,
    ConflictingCompletion,
    MessageConstruction(EstablishmentError),
}
#[derive(Debug, PartialEq, Eq)]
pub enum PublishError<E> {
    Procedure(ProcedureError),
    Downstream(E),
}

enum State {
    Prepared,
    Awaiting {
        deadline: ChipInstant,
    },
    Established {
        starts: DomainStarts,
        setup: Box<pdu::RRCConnectionSetupComplete>,
    },
    Closed {
        reason: CloseReason,
    },
}

pub struct Establishment {
    key: UeKey,
    changed_at: ChipInstant,
    identity: ie::InitialUEIdentity,
    assigned: AssignedRntis,
    transaction: TransactionId,
    message: EncodedRrc,
    state: State,
}

impl Establishment {
    // Assigned RNTIs and the UE generation must remain reserved until completion or a returned release action.
    pub fn prepare(
        key: UeKey,
        now: ChipInstant,
        request: &DecodedUplinkCommon,
        parameters: &CellFachSetup,
    ) -> Result<Self, ProcedureError> {
        if now.epoch != key.epoch {
            return Err(ProcedureError::Origin);
        }
        if request.value.integrity_check_info.is_some() {
            return Err(ProcedureError::Message);
        }
        let ULCCCHMessageType::rrcConnectionRequest(request) = &request.value.message else {
            return Err(ProcedureError::Message);
        };
        let message = build_cell_fach_setup(&request.initial_ue_identity, parameters)
            .map_err(ProcedureError::MessageConstruction)?;
        Ok(Self {
            key,
            changed_at: now,
            identity: request.initial_ue_identity.clone(),
            assigned: parameters.assigned,
            transaction: parameters.transaction,
            message,
            state: State::Prepared,
        })
    }

    pub fn phase(&self) -> EstablishmentPhase {
        match self.state {
            State::Prepared => EstablishmentPhase::Prepared,
            State::Awaiting { .. } => EstablishmentPhase::AwaitingComplete,
            State::Established { .. } => EstablishmentPhase::Established,
            State::Closed { .. } => EstablishmentPhase::Closed,
        }
    }
    pub fn key(&self) -> UeKey {
        self.key
    }
    pub fn assigned(&self) -> AssignedRntis {
        self.assigned
    }
    pub fn response(&self) -> &EncodedRrc {
        &self.message
    }
    pub fn starts(&self) -> Option<DomainStarts> {
        if let State::Established { starts, .. } = self.state {
            Some(starts)
        } else {
            None
        }
    }
    pub fn close_reason(&self) -> Option<CloseReason> {
        if let State::Closed { reason } = self.state {
            Some(reason)
        } else {
            None
        }
    }

    pub fn setup_complete(&self) -> Option<&pdu::RRCConnectionSetupComplete> {
        match &self.state {
            State::Established { setup, .. } => Some(setup),
            _ => None,
        }
    }

    pub fn classify_request(&self, request: &DecodedUplinkCommon) -> RequestMatch {
        match &request.value.message {
            ULCCCHMessageType::rrcConnectionRequest(request)
                if request.initial_ue_identity == self.identity =>
            {
                if request.protocol_error_indicator == ie::ProtocolErrorIndicator::errorOccurred {
                    RequestMatch::ConfigurationFailure
                } else {
                    RequestMatch::Retransmission
                }
            }
            _ => RequestMatch::DifferentIdentity,
        }
    }

    fn time(&self, now: ChipInstant) -> Result<(), ProcedureError> {
        if now.epoch != self.key.epoch {
            return Err(ProcedureError::Origin);
        }
        if now.index < self.changed_at.index {
            return Err(ProcedureError::Time);
        }
        Ok(())
    }

    // This is a UTRAN resource guard supplied by its owner, not the UE's standardized T300.
    pub fn publish_with<T, E>(
        &mut self,
        now: ChipInstant,
        response_guard_chips: NonZeroU64,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.time(now).map_err(PublishError::Procedure)?;
        if !matches!(self.state, State::Prepared) {
            return Err(PublishError::Procedure(ProcedureError::Phase));
        }
        let deadline = ChipInstant {
            index: now
                .index
                .checked_add(response_guard_chips.get())
                .ok_or(PublishError::Procedure(ProcedureError::Time))?,
            ..now
        };
        let result = publish(&self.message).map_err(PublishError::Downstream)?;
        self.state = State::Awaiting { deadline };
        self.changed_at = now;
        Ok(result)
    }

    pub fn retransmit_with<T, E>(
        &self,
        now: ChipInstant,
        publish: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, PublishError<E>> {
        self.time(now).map_err(PublishError::Procedure)?;
        let State::Awaiting { deadline } = self.state else {
            return Err(PublishError::Procedure(ProcedureError::Phase));
        };
        if now.index >= deadline.index {
            return Err(PublishError::Procedure(ProcedureError::Time));
        }
        publish(&self.message).map_err(PublishError::Downstream)
    }

    pub fn complete(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        message: &DecodedUplinkDedicated,
    ) -> Result<Completion, ProcedureError> {
        if key != self.key {
            return Err(ProcedureError::Origin);
        }
        self.time(now)?;
        if message.value.integrity_check_info.is_some() {
            return Err(ProcedureError::Message);
        }
        let ULDCCHMessageType::rrcConnectionSetupComplete(message) = &message.value.message else {
            return Err(ProcedureError::Message);
        };
        if message.rrc_transaction_identifier.0 != self.transaction.get() {
            return Err(ProcedureError::Transaction);
        }
        let starts = DomainStarts::from_wire(&message.start_list)
            .map_err(ProcedureError::MessageConstruction)?;
        match self.state {
            State::Awaiting { deadline } if now.index < deadline.index => {
                self.state = State::Established {
                    starts,
                    setup: Box::new(message.clone()),
                };
                self.changed_at = now;
                Ok(Completion::Established)
            }
            State::Established {
                starts: previous,
                ref setup,
            } if previous == starts && setup.as_ref() == message => Ok(Completion::Duplicate),
            State::Established { .. } => Err(ProcedureError::ConflictingCompletion),
            State::Awaiting { .. } => Err(ProcedureError::Time),
            _ => Err(ProcedureError::Phase),
        }
    }

    pub fn expire(&mut self, now: ChipInstant) -> Result<Option<UeKey>, ProcedureError> {
        self.time(now)?;
        if let State::Awaiting { deadline } = self.state
            && now.index >= deadline.index
        {
            self.state = State::Closed {
                reason: CloseReason::Timeout,
            };
            self.changed_at = now;
            return Ok(Some(self.key));
        }
        Ok(None)
    }

    pub fn abort(&mut self, reason: CloseReason) -> Result<UeKey, ProcedureError> {
        if !matches!(self.state, State::Prepared | State::Awaiting { .. }) {
            return Err(ProcedureError::Phase);
        }
        self.state = State::Closed { reason };
        Ok(self.key)
    }
}
