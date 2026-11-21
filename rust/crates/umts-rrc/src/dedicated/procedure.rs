// SPDX-License-Identifier: AGPL-3.0-or-later

pub use super::transaction::{
    PublicationError as SetupPublicationError, ReconfigurationPhase as SetupPhase,
    ReconfigurationResponse as SetupResponse, ResponseError as SetupResponseError,
};
use super::transaction::{Response, Transaction};
use super::{DedicatedError, ExistingDchSetup, InitialDchSetup, ScheduledActivation};
use crate::establishment::TransactionId;
use crate::mobility::{CellUpdate, UpdateFailureCause};
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
type SetupExchange = Transaction<pdu::RadioBearerSetupComplete, pdu::RadioBearerSetupFailure>;
type ResumeExchange =
    Transaction<pdu::RadioBearerReconfigurationComplete, pdu::RadioBearerReconfigurationFailure>;

enum Exchange {
    Setup(SetupExchange),
    Resume(ResumeExchange),
}

macro_rules! exchange {
    ($value:expr, $transaction:ident => $body:expr) => {
        match $value {
            Exchange::Setup($transaction) => $body,
            Exchange::Resume($transaction) => $body,
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupMode {
    Setup,
    /// RADIO BEARER RECONFIGURATION for bearers the UE already holds, answered by its COMPLETE
    /// or FAILURE, or by a CELL UPDATE that reports the failure (TS 25.331 8.3.1.3).
    Resume,
}

pub struct RadioBearerSetupProcedure {
    construction: Construction,
    transaction: Exchange,
    counters: Option<super::SetupCounters>,
    reported_failure: Option<UpdateFailureCause>,
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
            transaction: Exchange::Setup(Transaction::new(
                key,
                transaction,
                encoded,
                now,
                publish_deadline,
                response_guard,
                None,
            )?),
            counters: None,
            reported_failure: None,
        })
    }

    /// Resumes a UE that already holds the bearers of `setup` and is answering a cell update.
    /// The caller re-establishes its own RLC entities for bearers 5 and above.
    pub fn resume(
        key: UeKey,
        transaction: TransactionId,
        setup: InitialDchSetup,
        now: ChipInstant,
        publish_deadline: ChipInstant,
        response_guard: NonZeroU64,
    ) -> Result<Self, DedicatedError> {
        let encoded = setup.resume_reconfiguration(transaction)?;
        Ok(Self {
            construction: Construction::Initial { setup, hsdpa: None },
            transaction: Exchange::Resume(Transaction::new(
                key,
                transaction,
                encoded,
                now,
                publish_deadline,
                response_guard,
                None,
            )?),
            counters: None,
            reported_failure: None,
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
            transaction: Exchange::Setup(Transaction::new(
                key,
                transaction,
                encoded,
                now,
                publish_deadline,
                response_guard,
                activation,
            )?),
            counters: None,
            reported_failure: None,
        })
    }
    pub fn mode(&self) -> SetupMode {
        match self.transaction {
            Exchange::Setup(_) => SetupMode::Setup,
            Exchange::Resume(_) => SetupMode::Resume,
        }
    }
    pub fn key(&self) -> UeKey {
        exchange!(&self.transaction, value => value.key())
    }
    pub fn phase(&self) -> SetupPhase {
        exchange!(&self.transaction, value => value.phase())
    }
    pub fn transaction(&self) -> TransactionId {
        exchange!(&self.transaction, value => value.transaction())
    }
    pub fn encoded(&self) -> &EncodedRrc {
        exchange!(&self.transaction, value => value.encoded())
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
        exchange!(&self.transaction, value => value.receipt())
    }
    /// Absent in resume mode, where `resume_completion` holds the response.
    pub fn completion(&self) -> Option<&pdu::RadioBearerSetupComplete> {
        match &self.transaction {
            Exchange::Setup(value) => value.completion(),
            Exchange::Resume(_) => None,
        }
    }
    pub fn resume_completion(&self) -> Option<&pdu::RadioBearerReconfigurationComplete> {
        match &self.transaction {
            Exchange::Setup(_) => None,
            Exchange::Resume(value) => value.completion(),
        }
    }
    /// In resume mode `domain_starts` is the START list of the completion when the UE sent
    /// one, and `new_bearer_start` is always absent.
    pub fn completion_counters(&self) -> Option<super::SetupCounters> {
        self.counters
    }
    pub fn failure(&self) -> Option<&pdu::RadioBearerSetupFailure> {
        match &self.transaction {
            Exchange::Setup(value) => value.failure(),
            Exchange::Resume(_) => None,
        }
    }
    pub fn resume_failure(&self) -> Option<&pdu::RadioBearerReconfigurationFailure> {
        match &self.transaction {
            Exchange::Setup(_) => None,
            Exchange::Resume(value) => value.failure(),
        }
    }
    pub fn reported_failure(&self) -> Option<UpdateFailureCause> {
        self.reported_failure
    }
    /// TS 25.331 8.3.1.3: a UE that could not apply the confirm sends CELL UPDATE again with
    /// the failure cause and the transaction identifier of the confirm. Returns whether this
    /// cell update failed the procedure.
    pub fn cell_update_failure(
        &mut self,
        key: UeKey,
        now: ChipInstant,
        update: &CellUpdate,
    ) -> Result<bool, DedicatedError> {
        let Exchange::Resume(transaction) = &mut self.transaction else {
            return Ok(false);
        };
        if key != transaction.key() {
            return Err(DedicatedError::Identity);
        }
        let Some(failure) = update
            .failure
            .filter(|failure| failure.transaction == transaction.transaction().get())
        else {
            return Ok(false);
        };
        if transaction.phase() == SetupPhase::FailureReported {
            return Ok(self.reported_failure == Some(failure.cause));
        }
        transaction.reported_failure(now)?;
        self.reported_failure = Some(failure.cause);
        Ok(true)
    }
    /// The caller must prepare the actual new TX/RX links and apply current RRC integrity before RLC admission.
    pub fn publish_with<E>(
        &mut self,
        now: ChipInstant,
        publish: impl FnOnce(&EncodedRrc) -> Result<SduReceipt, E>,
    ) -> Result<(), SetupPublicationError<E>> {
        exchange!(&mut self.transaction, value => value.publish_with(now, publish))
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
        let counters = &mut self.counters;
        match &mut self.transaction {
            Exchange::Setup(transaction) => {
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
                transaction.response_with(key, time, response, |complete| {
                    let parsed = super::SetupCounters::from_complete(complete)
                        .map_err(SetupResponseError::Procedure)?;
                    let admitted = admit(parsed).map_err(SetupResponseError::Admission)?;
                    *counters = Some(parsed);
                    Ok(admitted)
                })
            }
            Exchange::Resume(transaction) => {
                let response = match &message.value.message {
                    ULDCCHMessageType::radioBearerReconfigurationComplete(value) => {
                        Response::Complete {
                            transaction: value.rrc_transaction_identifier.0,
                            value,
                        }
                    }
                    ULDCCHMessageType::radioBearerReconfigurationFailure(value) => {
                        Response::Failure {
                            transaction: value.rrc_transaction_identifier.0,
                            value,
                        }
                    }
                    _ => Response::Other,
                };
                transaction.response_with(key, time, response, |complete| {
                    let parsed = super::SetupCounters::from_resume_complete(complete)
                        .map_err(SetupResponseError::Procedure)?;
                    let admitted = admit(parsed).map_err(SetupResponseError::Admission)?;
                    *counters = Some(parsed);
                    Ok(admitted)
                })
            }
        }
    }
    pub fn expire(&mut self, now: ChipInstant) -> Result<bool, DedicatedError> {
        exchange!(&mut self.transaction, value => value.expire(now))
    }
    pub fn local_failure(&mut self, now: ChipInstant) -> Result<(), DedicatedError> {
        exchange!(&mut self.transaction, value => value.local_failure(now))
    }
}
