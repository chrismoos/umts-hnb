// SPDX-License-Identifier: AGPL-3.0-or-later

use super::DedicatedError;
use crate::establishment::TransactionId;
use std::num::NonZeroU64;
use umts_rlc::acknowledged::{ReceiveTime, SduReceipt};
use umts_rrc_codec::EncodedRrc;
use umts_types::{UeKey, time::ChipInstant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconfigurationPhase {
    Prepared,
    AwaitingComplete,
    Complete,
    FailureReported,
    AbortedBeforePublication,
    RecoveryRequired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconfigurationResponse {
    Complete,
    Failure,
    Duplicate,
}
#[derive(Debug, PartialEq, Eq)]
pub enum PublicationError<E> {
    Procedure(DedicatedError),
    Publication(E),
}
#[derive(Debug, PartialEq, Eq)]
pub enum ResponseError<E> {
    Procedure(DedicatedError),
    Admission(E),
}

pub(crate) enum Response<'a, C, F> {
    Complete { transaction: u8, value: &'a C },
    Failure { transaction: u8, value: &'a F },
    Other,
}
pub(crate) struct Transaction<C, F> {
    key: UeKey,
    transaction: TransactionId,
    encoded: EncodedRrc,
    phase: ReconfigurationPhase,
    previous: ChipInstant,
    publish_deadline: ChipInstant,
    response_guard: NonZeroU64,
    response_deadline: Option<ChipInstant>,
    published_at: Option<ChipInstant>,
    activation: Option<ChipInstant>,
    receipt: Option<SduReceipt>,
    complete: Option<Box<C>>,
    failure: Option<Box<F>>,
}
impl<C: Clone + Eq, F: Clone + Eq> Transaction<C, F> {
    pub(crate) fn new(
        key: UeKey,
        transaction: TransactionId,
        encoded: EncodedRrc,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
        activation: Option<ChipInstant>,
    ) -> Result<Self, DedicatedError> {
        if activation
            .is_some_and(|time| time.epoch != key.epoch || time.index <= publish_deadline.index)
        {
            return Err(DedicatedError::Activation);
        }
        if now.epoch != key.epoch
            || publish_deadline.epoch != key.epoch
            || publish_deadline.index <= now.index
            || publish_deadline
                .index
                .checked_add(response_guard.get())
                .is_none()
        {
            return Err(DedicatedError::Time);
        }
        if activation
            .is_some_and(|time| time.index >= publish_deadline.index + response_guard.get() - 1)
        {
            return Err(DedicatedError::Time);
        }
        Ok(Self {
            key,
            transaction,
            encoded,
            phase: ReconfigurationPhase::Prepared,
            previous: now,
            publish_deadline,
            response_guard,
            response_deadline: None,
            published_at: None,
            activation,
            receipt: None,
            complete: None,
            failure: None,
        })
    }
    pub(crate) fn key(&self) -> UeKey {
        self.key
    }
    pub(crate) fn transaction(&self) -> TransactionId {
        self.transaction
    }
    pub(crate) fn encoded(&self) -> &EncodedRrc {
        &self.encoded
    }
    pub(crate) fn phase(&self) -> ReconfigurationPhase {
        self.phase
    }
    pub(crate) fn receipt(&self) -> Option<SduReceipt> {
        self.receipt
    }
    pub(crate) fn completion(&self) -> Option<&C> {
        self.complete.as_deref()
    }
    pub(crate) fn failure(&self) -> Option<&F> {
        self.failure.as_deref()
    }
    fn time(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        if now.epoch != self.key.epoch || now.index < self.previous.index {
            return Err(DedicatedError::Time);
        }
        self.previous = now;
        Ok(())
    }
    pub(crate) fn publish_with<E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&EncodedRrc) -> Result<SduReceipt, E>,
    ) -> Result<(), PublicationError<E>> {
        self.time(now).map_err(PublicationError::Procedure)?;
        if self.phase != ReconfigurationPhase::Prepared {
            return Err(PublicationError::Procedure(DedicatedError::Phase));
        }
        if now.index >= self.publish_deadline.index {
            return Err(PublicationError::Procedure(DedicatedError::Time));
        }
        let deadline = ChipInstant {
            epoch: self.key.epoch,
            index: now
                .index
                .checked_add(self.response_guard.get())
                .ok_or(PublicationError::Procedure(DedicatedError::Time))?,
        };
        if self
            .activation
            .is_some_and(|activation| activation.index >= deadline.index)
        {
            return Err(PublicationError::Procedure(DedicatedError::Time));
        }
        let receipt = publish(&self.encoded).map_err(PublicationError::Publication)?;
        self.receipt = Some(receipt);
        self.response_deadline = Some(deadline);
        self.published_at = Some(now);
        self.phase = ReconfigurationPhase::AwaitingComplete;
        Ok(())
    }
    pub(crate) fn response_with<T, E>(
        &mut self,
        key: UeKey,
        time: ReceiveTime,
        response: Response<'_, C, F>,
        admit: impl FnOnce(&C) -> Result<T, ResponseError<E>>,
    ) -> Result<(ReconfigurationResponse, Option<T>), ResponseError<E>> {
        let fail = ResponseError::Procedure;
        if key != self.key {
            return Err(fail(DedicatedError::Identity));
        }
        if time.serviced().epoch != self.key.epoch || time.serviced().index < self.previous.index {
            return Err(fail(DedicatedError::Time));
        }
        if self.phase == ReconfigurationPhase::AwaitingComplete
            && (self
                .response_deadline
                .is_some_and(|deadline| time.received().index >= deadline.index)
                || self
                    .published_at
                    .is_some_and(|published| time.received().index < published.index))
        {
            return Err(fail(DedicatedError::Time));
        }
        let result = match response {
            Response::Complete { transaction, value } => {
                if transaction != self.transaction.get() {
                    return Err(fail(DedicatedError::Transaction));
                }
                // TS 25.331 8.2.2.4 uses the new configuration for Complete, not rejection.
                if self.phase == ReconfigurationPhase::AwaitingComplete
                    && self
                        .activation
                        .is_some_and(|activation| time.received().index < activation.index)
                {
                    return Err(fail(DedicatedError::Time));
                }
                if self.phase == ReconfigurationPhase::Complete {
                    if self.complete.as_deref() != Some(value) {
                        return Err(fail(DedicatedError::Transaction));
                    }
                    (ReconfigurationResponse::Duplicate, None)
                } else {
                    if self.phase != ReconfigurationPhase::AwaitingComplete {
                        return Err(fail(DedicatedError::Phase));
                    }
                    let admitted = admit(value)?;
                    self.complete = Some(Box::new(value.clone()));
                    self.phase = ReconfigurationPhase::Complete;
                    (ReconfigurationResponse::Complete, Some(admitted))
                }
            }
            Response::Failure { transaction, value } => {
                if transaction != self.transaction.get() {
                    return Err(fail(DedicatedError::Transaction));
                }
                if self.phase == ReconfigurationPhase::FailureReported {
                    if self.failure.as_deref() != Some(value) {
                        return Err(fail(DedicatedError::Transaction));
                    }
                    (ReconfigurationResponse::Duplicate, None)
                } else {
                    if self.phase != ReconfigurationPhase::AwaitingComplete {
                        return Err(fail(DedicatedError::Phase));
                    }
                    self.failure = Some(Box::new(value.clone()));
                    self.phase = ReconfigurationPhase::FailureReported;
                    (ReconfigurationResponse::Failure, None)
                }
            }
            Response::Other => return Err(fail(DedicatedError::Transaction)),
        };
        self.previous = time.serviced();
        Ok(result)
    }
    pub(crate) fn reported_failure(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        self.time(now)?;
        if self.phase != ReconfigurationPhase::AwaitingComplete {
            return Err(DedicatedError::Phase);
        }
        self.phase = ReconfigurationPhase::FailureReported;
        Ok(())
    }
    pub(crate) fn expire(&mut self, now: ChipInstant) -> Result<bool, DedicatedError> {
        self.time(now)?;
        let next = match self.phase {
            ReconfigurationPhase::Prepared if now.index >= self.publish_deadline.index => {
                Some(ReconfigurationPhase::AbortedBeforePublication)
            }
            ReconfigurationPhase::AwaitingComplete
                if self
                    .response_deadline
                    .is_some_and(|deadline| now.index >= deadline.index) =>
            {
                Some(ReconfigurationPhase::RecoveryRequired)
            }
            _ => None,
        };
        if let Some(next) = next {
            self.phase = next;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    pub(crate) fn local_failure(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        self.time(now)?;
        self.phase = if self.receipt.is_some() {
            ReconfigurationPhase::RecoveryRequired
        } else {
            ReconfigurationPhase::AbortedBeforePublication
        };
        Ok(())
    }
}
