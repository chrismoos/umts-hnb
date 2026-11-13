// SPDX-License-Identifier: AGPL-3.0-or-later

pub use crate::dedicated::transaction::{
    PublicationError, ReconfigurationPhase as ActivationPhase,
    ReconfigurationResponse as ActivationResponse, ResponseError,
};
use crate::{
    dedicated::{
        DedicatedError, ReconfigurationCounters,
        transaction::{Response, Transaction},
    },
    establishment::TransactionId,
};
use std::{num::NonZeroU64, sync::Arc};
use umts_rlc::acknowledged::{ReceiveTime, SduReceipt};
use umts_rrc_codec::{DecodedUplinkDedicated, EncodedRrc};
use umts_rrc_schema::{class_definitions::ULDCCHMessageType, pdu_definitions as pdu};
use umts_types::{UeKey, time::ChipInstant};

pub trait ActivationMessage {
    fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError>;
    fn activation(&self) -> super::ScheduledActivation;
}

pub struct ActivationProcedure<C: ActivationMessage> {
    construction: Arc<C>,
    transaction: Transaction<
        pdu::RadioBearerReconfigurationComplete,
        pdu::RadioBearerReconfigurationFailure,
    >,
}

impl<C: ActivationMessage> ActivationProcedure<C> {
    pub fn new(
        key: UeKey,
        transaction: TransactionId,
        construction: Arc<C>,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        let encoded = construction.message(transaction)?;
        let activation = Some(construction.activation().time());
        Ok(Self {
            construction,
            transaction: Transaction::new(
                key,
                transaction,
                encoded,
                now,
                publish_deadline,
                response_guard,
                activation,
            )?,
        })
    }

    pub fn key(&self) -> UeKey {
        self.transaction.key()
    }

    pub fn phase(&self) -> ActivationPhase {
        self.transaction.phase()
    }

    pub fn construction(&self) -> &Arc<C> {
        &self.construction
    }

    pub fn transaction(&self) -> TransactionId {
        self.transaction.transaction()
    }

    pub fn encoded(&self) -> &EncodedRrc {
        self.transaction.encoded()
    }

    pub fn receipt(&self) -> Option<SduReceipt> {
        self.transaction.receipt()
    }

    pub fn completion(&self) -> Option<&pdu::RadioBearerReconfigurationComplete> {
        self.transaction.completion()
    }

    pub fn failure(&self) -> Option<&pdu::RadioBearerReconfigurationFailure> {
        self.transaction.failure()
    }

    /// Radio preparation and current RRC integrity must precede RLC admission.
    pub fn publish_with<E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&EncodedRrc) -> Result<SduReceipt, E>,
    ) -> Result<(), PublicationError<E>> {
        self.transaction.publish_with(now, publish)
    }

    /// The UE control owner authenticates the response and admits its counter fields before committing.
    pub fn response_with<T, E>(
        &mut self,
        key: UeKey,
        time: ReceiveTime,
        message: &DecodedUplinkDedicated,
        admit: impl FnOnce(ReconfigurationCounters) -> Result<T, E>,
    ) -> Result<(ActivationResponse, Option<T>), ResponseError<E>> {
        let response = match &message.value.message {
            ULDCCHMessageType::radioBearerReconfigurationComplete(value) => Response::Complete {
                transaction: value.rrc_transaction_identifier.0,
                value,
            },
            ULDCCHMessageType::radioBearerReconfigurationFailure(value) => Response::Failure {
                transaction: value.rrc_transaction_identifier.0,
                value,
            },
            _ => Response::Other,
        };
        self.transaction
            .response_with(key, time, response, |complete| {
                let counters = ReconfigurationCounters::from_complete(complete)
                    .map_err(ResponseError::Procedure)?;
                admit(counters).map_err(ResponseError::Admission)
            })
    }

    pub fn expire(&mut self, now: ChipInstant) -> Result<bool, DedicatedError> {
        self.transaction.expire(now)
    }

    pub fn local_failure(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        self.transaction.local_failure(now)
    }
}
