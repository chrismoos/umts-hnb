// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{ActivationParameters, MappedBearer, RetainedDownlink, activation::ActivationEncoding};
use crate::{
    dedicated::{DedicatedError, Rel99Configuration, Rel99Rlc, ScheduledActivation},
    establishment::TransactionId,
};
use std::sync::Arc;
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{class_definitions as class, information_elements as ie};
use umts_types::{CnDomain, channel_codes::OvsfCode};

const FIRST_SRB: u8 = 1;
const LAST_SRB: u8 = 3;
const FIRST_USER_BEARER: u8 = 5;
const MAX_MAPPINGS: usize = 15;
const MAX_DOWNLINK_FLOWS: u8 = 8;
const MAX_LOGICAL_CHANNEL: u8 = 15;

pub struct EdchOnlyBearerPlan {
    preceding: Arc<Rel99Configuration>,
    mappings: Box<[MappedBearer]>,
}

struct EdchOnlyDefinition {
    bearers: EdchOnlyBearerPlan,
    parameters: ActivationParameters,
    hsdpa_active: bool,
}

pub struct EdchOnlyActivation {
    pub(super) secondary_cell_active: bool,
    definition: Arc<EdchOnlyDefinition>,
    activation: ScheduledActivation,
}

impl EdchOnlyActivation {
    pub fn new(
        bearers: EdchOnlyBearerPlan,
        parameters: ActivationParameters,
        activation: ScheduledActivation,
        hsdpa_active: bool,
    ) -> Result<Self, DedicatedError> {
        super::activation::r7::validate_mappings(bearers.mappings(), hsdpa_active)?;
        parameters
            .transport
            .wire_reference_powers(
                &parameters.references,
                parameters.minimum,
                parameters.scheduling,
            )
            .map_err(|_| DedicatedError::Format)?;
        if parameters.flows.is_empty() || parameters.flows.len() > usize::from(MAX_DOWNLINK_FLOWS) {
            return Err(DedicatedError::Mapping);
        }
        for (index, flow) in parameters.flows.iter().enumerate() {
            if parameters.flows[..index]
                .iter()
                .any(|old| old.identity() == flow.identity())
                || !bearers
                    .mappings()
                    .iter()
                    .any(|mapping| mapping.uplink.flow() == flow.identity())
            {
                return Err(DedicatedError::Mapping);
            }
        }
        for mapping in bearers.mappings() {
            if !parameters
                .flows
                .iter()
                .any(|flow| flow.identity() == mapping.uplink.flow())
                || (!hsdpa_active && matches!(mapping.downlink, RetainedDownlink::Hsdpa { .. }))
            {
                return Err(DedicatedError::Mapping);
            }
        }
        let physical = bearers.preceding().physical();
        for index in &physical.downlink_codes {
            let code = OvsfCode::new(physical.downlink_sf, *index)
                .map_err(|_| DedicatedError::Physical)?;
            if physical.secondary_scrambling.is_none()
                && (parameters.downlink.absolute_code().overlaps(code)
                    || parameters.downlink.indicator_code().overlaps(code))
            {
                return Err(DedicatedError::Physical);
            }
        }
        let result = Self {
            secondary_cell_active: false,
            definition: Arc::new(EdchOnlyDefinition {
                bearers,
                parameters,
                hsdpa_active,
            }),
            activation,
        };
        result.validate_activation(activation)?;
        Ok(result)
    }

    fn validate_activation(&self, activation: ScheduledActivation) -> Result<(), DedicatedError> {
        if activation.time().epoch != self.activation.time().epoch
            || [self.preceding().uplink(), self.preceding().downlink()]
                .into_iter()
                .any(|tfcs| {
                    tfcs.transports().iter().any(|transport| {
                        !usize::from(activation.cfn()).is_multiple_of(transport.frames())
                    })
                })
        {
            return Err(DedicatedError::Activation);
        }
        Ok(())
    }

    pub fn rescheduled(&self, activation: ScheduledActivation) -> Result<Self, DedicatedError> {
        self.validate_activation(activation)?;
        Ok(Self {
            secondary_cell_active: self.secondary_cell_active,
            definition: Arc::clone(&self.definition),
            activation,
        })
    }

    pub fn same_definition(&self, other: &Self) -> bool {
        self.secondary_cell_active == other.secondary_cell_active
            && Arc::ptr_eq(&self.definition, &other.definition)
    }

    pub fn preceding(&self) -> &Arc<Rel99Configuration> {
        self.definition.bearers.preceding()
    }

    pub fn mappings(&self) -> &[MappedBearer] {
        self.definition.bearers.mappings()
    }

    pub fn parameters(&self) -> &ActivationParameters {
        &self.definition.parameters
    }

    pub fn activation(&self) -> ScheduledActivation {
        self.activation
    }

    pub fn hsdpa_active(&self) -> bool {
        self.definition.hsdpa_active
    }

    pub fn value(
        &self,
        transaction: TransactionId,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        ActivationEncoding {
            secondary_cell_active: self.secondary_cell_active,
            dedicated: self.preceding(),
            parameters: self.parameters(),
            mappings: self.mappings(),
            activation: self.activation,
            hsdpa_active: self.hsdpa_active(),
            uplink_physical: Some(self.definition.bearers.uplink_physical()),
            deleted_uplink: Some(self.definition.bearers.deleted_uplink_transports()),
            retained_uplink: None,
        }
        .value(transaction)
    }

    pub fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        encode_downlink_dedicated(&self.value(transaction)?).map_err(DedicatedError::Codec)
    }
}

impl crate::dedicated::ActivationMessage for EdchOnlyActivation {
    fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        EdchOnlyActivation::message(self, transaction)
    }

    fn activation(&self) -> ScheduledActivation {
        self.activation
    }
}

impl EdchOnlyBearerPlan {
    pub fn new(
        preceding: Arc<Rel99Configuration>,
        mappings: Vec<MappedBearer>,
    ) -> Result<Self, DedicatedError> {
        if mappings.is_empty()
            || mappings.len() > MAX_MAPPINGS
            || mappings.len() != preceding.bearers().len()
        {
            return Err(DedicatedError::Mapping);
        }
        for (index, mapping) in mappings.iter().enumerate() {
            if mappings[..index].iter().any(|old| {
                old.bearer == mapping.bearer
                    || old.uplink.ddi() == mapping.uplink.ddi()
                    || old.uplink.logical() == mapping.uplink.logical()
            }) {
                return Err(DedicatedError::Mapping);
            }
            let bearer = preceding
                .bearer(mapping.bearer)
                .ok_or(DedicatedError::Bearer)?;
            let signalling = (FIRST_SRB..=LAST_SRB).contains(&bearer.id) && bearer.domain.is_none();
            let packet = bearer.id >= FIRST_USER_BEARER && bearer.domain == Some(CnDomain::Ps);
            if (!signalling && !packet) || matches!(bearer.rlc, Rel99Rlc::Transparent { .. }) {
                return Err(DedicatedError::Bearer);
            }
            if bearer.mapping.uplink.is_empty() || bearer.mapping.downlink.is_empty() {
                return Err(DedicatedError::Mapping);
            }
            let mut has_data = false;
            for old in &bearer.mapping.uplink {
                let transport = preceding
                    .uplink()
                    .transport(old.transport)
                    .ok_or(DedicatedError::Transport)?;
                for format in transport.formats() {
                    if format.blocks == 0 || format.rlc_bits == 0 {
                        continue;
                    }
                    has_data = true;
                    if format.rlc_bits != mapping.uplink.pdu_bits() {
                        return Err(DedicatedError::Format);
                    }
                }
            }
            if !has_data {
                return Err(DedicatedError::Format);
            }
            match mapping.downlink {
                RetainedDownlink::Dedicated => (),
                RetainedDownlink::Hsdpa { flow, logical }
                    if packet
                        && flow < MAX_DOWNLINK_FLOWS
                        && logical.is_none_or(|id| (1..=MAX_LOGICAL_CHANNEL).contains(&id)) => {}
                RetainedDownlink::Hsdpa { .. } => return Err(DedicatedError::Mapping),
                RetainedDownlink::Ehs { queue, logical }
                    if packet
                        && queue < MAX_DOWNLINK_FLOWS
                        && (1..=MAX_LOGICAL_CHANNEL).contains(&logical) => {}
                RetainedDownlink::Ehs { .. } => return Err(DedicatedError::Mapping),
            }
        }
        Ok(Self {
            preceding,
            mappings: mappings.into_boxed_slice(),
        })
    }

    pub fn preceding(&self) -> &Arc<Rel99Configuration> {
        &self.preceding
    }

    pub fn mappings(&self) -> &[MappedBearer] {
        &self.mappings
    }

    pub fn uplink_physical(&self) -> ie::ULDPCHInfoR6 {
        // TS 25.331 UL-DPCH-Info-r6 represents absence as a choice, not NumberOfDPDCH=0.
        ie::ULDPCHInfoR6::new(
            None,
            ie::ULDPCHInfoR6ModeSpecificInfo::fdd(ie::ULDPCHInfoR6ModeSpecificInfoFdd::new(
                ie::ScramblingCodeType::longSC,
                ie::ULScramblingCode(self.preceding.physical().uplink_scrambling),
                ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresence::notPresent(
                    ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresenceNotPresent::new(false, None),
                ),
            )),
        )
    }

    pub fn deleted_uplink_transports(&self) -> ie::ULDeletedTransChInfoListR6 {
        ie::ULDeletedTransChInfoListR6(
            self.preceding
                .uplink()
                .transports()
                .iter()
                .map(|transport| {
                    ie::ULTransportChannelIdentityR6::dch_usch(
                        ie::ULTransportChannelIdentityR6DchUsch::new(
                            ie::ULTrCHType::dch,
                            ie::TransportChannelIdentity(transport.id),
                        ),
                    )
                })
                .collect(),
        )
    }
}
