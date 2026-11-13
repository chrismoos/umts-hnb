// SPDX-License-Identifier: AGPL-3.0-or-later

pub use super::transaction::{
    PublicationError as SetupPublicationError, ReconfigurationPhase as SetupPhase,
    ReconfigurationResponse as SetupResponse, ResponseError as SetupResponseError,
};
use super::transaction::{Response, Transaction};
use super::{DedicatedError, ExistingDchSetup, InitialDchSetup, ScheduledActivation};
use crate::establishment::TransactionId;
use std::{num::NonZeroU64, sync::Arc};
use umts_rlc::acknowledged::SduReceipt;
use umts_rrc_codec::{DecodedUplinkDedicated, EncodedRrc};
use umts_rrc_schema::{class_definitions::ULDCCHMessageType, pdu_definitions as pdu};
use umts_types::{UeKey, time::ChipInstant};

enum Construction {
    Initial {
        setup: InitialDchSetup,
        hsdpa: Option<Arc<crate::hsdpa::InitialConfiguration>>,
    },
    Existing(ExistingDchSetup),
}
pub struct RadioBearerSetupProcedure {
    construction: Construction,
    transaction: Transaction<pdu::RadioBearerSetupComplete, pdu::RadioBearerSetupFailure>,
    counters: Option<super::SetupCounters>,
}
impl RadioBearerSetupProcedure {
    pub fn initial(
        key: UeKey,
        transaction: TransactionId,
        setup: InitialDchSetup,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        Self::initial_configuration(
            key,
            transaction,
            setup,
            None,
            now,
            publish_deadline,
            response_guard,
        )
    }

    pub fn initial_with_hsdpa(
        key: UeKey,
        transaction: TransactionId,
        setup: InitialDchSetup,
        hsdpa: Arc<crate::hsdpa::InitialConfiguration>,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        Self::initial_configuration(
            key,
            transaction,
            setup,
            Some(hsdpa),
            now,
            publish_deadline,
            response_guard,
        )
    }

    fn initial_configuration(
        key: UeKey,
        transaction: TransactionId,
        setup: InitialDchSetup,
        hsdpa: Option<Arc<crate::hsdpa::InitialConfiguration>>,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        let encoded = match &hsdpa {
            Some(hs) => setup.message_with_evolved_hsdpa(transaction, hs)?,
            None => setup.message(transaction)?,
        };
        Ok(Self {
            construction: Construction::Initial { setup, hsdpa },
            transaction: Transaction::new(
                key,
                transaction,
                encoded,
                now,
                publish_deadline,
                response_guard,
                None,
            )?,
            counters: None,
        })
    }
    pub fn existing(
        key: UeKey,
        transaction: TransactionId,
        setup: ExistingDchSetup,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        let encoded = setup.message(transaction)?;
        let activation = Some(setup.activation().time());
        Ok(Self {
            construction: Construction::Existing(setup),
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
    pub fn phase(&self) -> SetupPhase {
        self.transaction.phase()
    }
    pub fn transaction(&self) -> TransactionId {
        self.transaction.transaction()
    }
    pub fn encoded(&self) -> &EncodedRrc {
        self.transaction.encoded()
    }
    pub fn configuration(&self) -> &std::sync::Arc<super::Rel99Configuration> {
        match &self.construction {
            Construction::Initial { setup, .. } => setup.configuration(),
            Construction::Existing(setup) => setup.configuration(),
        }
    }

    pub fn initial_hsdpa(&self) -> Option<&Arc<crate::hsdpa::InitialConfiguration>> {
        match &self.construction {
            Construction::Initial { hsdpa, .. } => hsdpa.as_ref(),
            Construction::Existing(_) => None,
        }
    }
    pub fn previous(&self) -> Option<&std::sync::Arc<super::Rel99Configuration>> {
        match &self.construction {
            Construction::Initial { .. } => None,
            Construction::Existing(setup) => Some(setup.previous()),
        }
    }
    pub fn activation(&self) -> Option<ScheduledActivation> {
        match &self.construction {
            Construction::Initial { .. } => None,
            Construction::Existing(setup) => Some(setup.activation()),
        }
    }
    pub fn rabs(&self) -> &[super::NewRab] {
        match &self.construction {
            Construction::Initial { setup, .. } => setup.rabs(),
            Construction::Existing(setup) => setup.rabs(),
        }
    }
    pub fn receipt(&self) -> Option<SduReceipt> {
        self.transaction.receipt()
    }
    pub fn completion(&self) -> Option<&pdu::RadioBearerSetupComplete> {
        self.transaction.completion()
    }
    pub fn completion_counters(&self) -> Option<super::SetupCounters> {
        self.counters
    }
    pub fn failure(&self) -> Option<&pdu::RadioBearerSetupFailure> {
        self.transaction.failure()
    }
    /// The caller must prepare the actual new TX/RX links and apply current RRC integrity before RLC admission.
    pub fn publish_with<E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&EncodedRrc) -> Result<SduReceipt, E>,
    ) -> Result<(), SetupPublicationError<E>> {
        self.transaction.publish_with(now, publish)
    }
    /// The single UE control owner authenticates the response before calling this method.
    pub fn response(
        &mut self,
        key: UeKey,
        time: umts_rlc::acknowledged::ReceiveTime,
        message: &DecodedUplinkDedicated,
    ) -> Result<SetupResponse, DedicatedError> {
        self.response_with(
            key,
            time,
            message,
            |_| Ok::<_, std::convert::Infallible>(()),
        )
        .map(|(response, _)| response)
        .map_err(|error| match error {
            SetupResponseError::Procedure(error) => error,
            SetupResponseError::Admission(never) => match never {},
        })
    }
    pub fn response_with<T, E>(
        &mut self,
        key: UeKey,
        time: umts_rlc::acknowledged::ReceiveTime,
        message: &DecodedUplinkDedicated,
        admit: impl FnOnce(super::SetupCounters) -> Result<T, E>,
    ) -> Result<(SetupResponse, Option<T>), SetupResponseError<E>> {
        let response = match &message.value.message {
            ULDCCHMessageType::radioBearerSetupComplete(value) => Response::Complete {
                transaction: value.rrc_transaction_identifier.0,
                value,
            },
            ULDCCHMessageType::radioBearerSetupFailure(value) => Response::Failure {
                transaction: value.rrc_transaction_identifier.0,
                value,
            },
            _ => Response::Other,
        };
        let counters = &mut self.counters;
        self.transaction
            .response_with(key, time, response, |complete| {
                let parsed = super::SetupCounters::from_complete(complete)
                    .map_err(SetupResponseError::Procedure)?;
                let admitted = admit(parsed).map_err(SetupResponseError::Admission)?;
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
