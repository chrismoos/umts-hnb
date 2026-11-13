// SPDX-License-Identifier: AGPL-3.0-or-later

use super::transaction::{Response, Transaction};
use super::{
    DedicatedError, ExistingDchRelease, PublicationError, ReconfigurationPhase,
    ReconfigurationResponse, ReleaseCounters, ResponseError,
};
use crate::establishment::TransactionId;
use std::{num::NonZeroU64, sync::Arc};
use umts_rlc::acknowledged::{ReceiveTime, SduReceipt};
use umts_rrc_codec::{DecodedUplinkDedicated, EncodedRrc};
use umts_rrc_schema::{class_definitions::ULDCCHMessageType, pdu_definitions as pdu};
use umts_types::{UeKey, time::ChipInstant};

pub struct RadioBearerReleaseProcedure {
    construction: ExistingDchRelease,
    transaction: Transaction<pdu::RadioBearerReleaseComplete, pdu::RadioBearerReleaseFailure>,
    counters: Option<ReleaseCounters>,
}
impl RadioBearerReleaseProcedure {
    pub fn new(
        key: UeKey,
        transaction: TransactionId,
        release: ExistingDchRelease,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        let encoded = release.message(transaction)?;
        let activation = Some(release.activation().time());
        Ok(Self {
            construction: release,
            transaction: Transaction::new(
                key,
                transaction,
                encoded,
                now,
                publish_deadline,
                response_guard,
                activation,
            )?,
            counters: None,
        })
    }
    pub fn key(&self) -> UeKey {
        self.transaction.key()
    }
    pub fn phase(&self) -> ReconfigurationPhase {
        self.transaction.phase()
    }
    pub fn transaction(&self) -> TransactionId {
        self.transaction.transaction()
    }
    pub fn encoded(&self) -> &EncodedRrc {
        self.transaction.encoded()
    }
    pub fn configuration(&self) -> &Arc<super::Rel99Configuration> {
        self.construction.configuration()
    }
    pub fn previous(&self) -> &Arc<super::Rel99Configuration> {
        self.construction.previous()
    }
    pub fn released(&self) -> &[u8] {
        self.construction.released()
    }
    pub fn activation(&self) -> super::ScheduledActivation {
        self.construction.activation()
    }
    pub fn receipt(&self) -> Option<SduReceipt> {
        self.transaction.receipt()
    }
    pub fn completion(&self) -> Option<&pdu::RadioBearerReleaseComplete> {
        self.transaction.completion()
    }
    pub fn completion_counters(&self) -> Option<ReleaseCounters> {
        self.counters
    }
    pub fn failure(&self) -> Option<&pdu::RadioBearerReleaseFailure> {
        self.transaction.failure()
    }
    /// Actual replacement links must be prepared and current integrity applied before protected RLC publication.
    pub fn publish_with<E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&EncodedRrc) -> Result<SduReceipt, E>,
    ) -> Result<(), PublicationError<E>> {
        self.transaction.publish_with(now, publish)
    }
    /// Completion is authenticated by the UE owner and does not prove old TX/RX resources have drained.
    pub fn response_with<T, E>(
        &mut self,
        key: UeKey,
        time: ReceiveTime,
        message: &DecodedUplinkDedicated,
        admit: impl FnOnce(ReleaseCounters) -> Result<T, E>,
    ) -> Result<(ReconfigurationResponse, Option<T>), ResponseError<E>> {
        let response = match &message.value.message {
            ULDCCHMessageType::radioBearerReleaseComplete(value) => Response::Complete {
                transaction: value.rrc_transaction_identifier.0,
                value,
            },
            ULDCCHMessageType::radioBearerReleaseFailure(value) => Response::Failure {
                transaction: value.rrc_transaction_identifier.0,
                value,
            },
            _ => Response::Other,
        };
        let counters = &mut self.counters;
        self.transaction
            .response_with(key, time, response, |complete| {
                let parsed =
                    ReleaseCounters::from_complete(complete).map_err(ResponseError::Procedure)?;
                let admitted = admit(parsed).map_err(ResponseError::Admission)?;
                *counters = Some(parsed);
                Ok(admitted)
            })
    }
    pub fn expire(&mut self, now: ChipInstant) -> Result<bool, DedicatedError> {
        self.transaction.expire(now)
    }
    pub fn local_failure(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        self.transaction.local_failure(now)
    }
}
