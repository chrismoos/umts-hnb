// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ActivationParameters, DchCoexistenceActivation, EdchOnlyActivation, MappedBearer};
use crate::{
    dedicated::{DedicatedError, Rel99Configuration, ScheduledActivation},
    establishment::TransactionId,
};
use std::sync::Arc;
use umts_rrc_codec::EncodedRrc;
use umts_rrc_schema::class_definitions as class;

pub enum UplinkActivation {
    Coexistence(DchCoexistenceActivation),
    EdchOnly(EdchOnlyActivation),
}

impl UplinkActivation {
    pub fn with_secondary_cell(mut self, enabled: bool) -> Result<Self, DedicatedError> {
        if enabled
            && (!self.hsdpa_active()
                || !self
                    .mappings()
                    .iter()
                    .any(|mapping| matches!(mapping.downlink, super::RetainedDownlink::Ehs { .. })))
        {
            return Err(DedicatedError::Mapping);
        }
        match &mut self {
            Self::Coexistence(value) => value.secondary_cell_active = enabled,
            Self::EdchOnly(value) => value.secondary_cell_active = enabled,
        }
        Ok(self)
    }

    pub fn process_allocation(
        &self,
        transaction: TransactionId,
        processes: u8,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        super::activation::process_allocation_message(
            self.parameters(),
            transaction,
            processes,
            self.secondary_cell_active(),
        )
    }

    pub fn secondary_cell_active(&self) -> bool {
        match self {
            Self::Coexistence(value) => value.secondary_cell_active,
            Self::EdchOnly(value) => value.secondary_cell_active,
        }
    }

    pub fn coexistence(
        dedicated: Arc<Rel99Configuration>,
        parameters: ActivationParameters,
        mappings: Vec<MappedBearer>,
        activation: ScheduledActivation,
        hsdpa_active: bool,
    ) -> Result<Self, DedicatedError> {
        DchCoexistenceActivation::new(dedicated, parameters, mappings, activation, hsdpa_active)
            .map(Self::Coexistence)
    }

    pub fn maximum_dpdch(&self) -> u8 {
        match self {
            Self::Coexistence(_) => 1,
            Self::EdchOnly(_) => 0,
        }
    }

    // This is the predecessor retained for ownership and downlink, not the successor UL layout.
    pub fn dedicated(&self) -> &Arc<Rel99Configuration> {
        match self {
            Self::Coexistence(value) => value.dedicated(),
            Self::EdchOnly(value) => value.preceding(),
        }
    }

    pub fn parameters(&self) -> &ActivationParameters {
        match self {
            Self::Coexistence(value) => value.parameters(),
            Self::EdchOnly(value) => value.parameters(),
        }
    }

    pub fn mappings(&self) -> &[MappedBearer] {
        match self {
            Self::Coexistence(value) => value.mappings(),
            Self::EdchOnly(value) => value.mappings(),
        }
    }

    pub fn activation(&self) -> ScheduledActivation {
        match self {
            Self::Coexistence(value) => value.activation(),
            Self::EdchOnly(value) => value.activation(),
        }
    }

    pub fn hsdpa_active(&self) -> bool {
        match self {
            Self::Coexistence(value) => value.hsdpa_active(),
            Self::EdchOnly(value) => value.hsdpa_active(),
        }
    }

    pub fn rescheduled(&self, activation: ScheduledActivation) -> Result<Self, DedicatedError> {
        match self {
            Self::Coexistence(value) => value.rescheduled(activation).map(Self::Coexistence),
            Self::EdchOnly(value) => value.rescheduled(activation).map(Self::EdchOnly),
        }
    }

    pub fn same_definition(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Coexistence(a), Self::Coexistence(b)) => a.same_definition(b),
            (Self::EdchOnly(a), Self::EdchOnly(b)) => a.same_definition(b),
            _ => false,
        }
    }

    pub fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        match self {
            Self::Coexistence(value) => value.message(transaction),
            Self::EdchOnly(value) => value.message(transaction),
        }
    }

    pub fn value(
        &self,
        transaction: TransactionId,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        match self {
            Self::Coexistence(value) => value.value(transaction),
            Self::EdchOnly(value) => value.value(transaction),
        }
    }
}

impl crate::dedicated::ActivationMessage for UplinkActivation {
    fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        UplinkActivation::message(self, transaction)
    }

    fn activation(&self) -> ScheduledActivation {
        UplinkActivation::activation(self)
    }
}
