// SPDX-License-Identifier: AGPL-3.0-or-later

use super::InitialActivation;
use crate::{
    dedicated::{ActivationMessage, DedicatedError, ScheduledActivation},
    establishment::TransactionId,
};
use umts_rrc_codec::EncodedRrc;

pub use crate::dedicated::{
    PublicationError, ReconfigurationPhase as ActivationPhase,
    ReconfigurationResponse as ActivationResponse, ResponseError,
};
pub type ActivationProcedure = crate::dedicated::ActivationProcedure<InitialActivation>;

impl ActivationMessage for InitialActivation {
    fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        InitialActivation::message(self, transaction)
    }

    fn activation(&self) -> ScheduledActivation {
        InitialActivation::activation(self)
    }
}
