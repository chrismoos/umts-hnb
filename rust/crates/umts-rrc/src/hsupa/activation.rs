// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{
    ControlConfiguration, DownlinkControl, ReferencePowers, ScheduledFlow, SchedulingInformation,
    TransportConfiguration, UplinkMapping,
};
use crate::{
    dedicated::{DchTfcs, DedicatedError, Rel99Configuration, Rel99Rlc, ScheduledActivation},
    establishment::TransactionId,
};
use rasn::types::FixedBitString;
use std::sync::Arc;
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{
    CnDomain,
    channel_codes::OvsfCode,
    hsupa::{Etfci, Tti},
};

const MAX_FLOWS: usize = 8;
const MAX_MAPPINGS: usize = 15;
const ZERO_GRANT: u8 = 38;

#[cfg(test)]
mod tests;

#[path = "activation_r7.rs"]
pub(super) mod r7;

#[derive(Clone, Copy, Debug)]
pub enum RetainedDownlink {
    Dedicated,
    Hsdpa { flow: u8, logical: Option<u8> },
    Ehs { queue: u8, logical: u8 },
}

#[derive(Clone, Copy, Debug)]
pub struct MappedBearer {
    pub bearer: u8,
    pub uplink: UplinkMapping,
    pub downlink: RetainedDownlink,
}

pub struct ActivationParameters {
    pub e_rnti: u16,
    pub transport: TransportConfiguration,
    pub control: ControlConfiguration,
    pub downlink: DownlinkControl,
    pub references: ReferencePowers,
    pub minimum: Etfci,
    pub scheduling: SchedulingInformation,
    pub rv_zero_only: bool,
    pub flows: Vec<ScheduledFlow>,
}

pub struct DchCoexistenceActivation {
    pub(super) secondary_cell_active: bool,
    definition: Arc<CoexistenceDefinition>,
    activation: ScheduledActivation,
}

struct CoexistenceDefinition {
    dedicated: Arc<Rel99Configuration>,
    uplink: DchTfcs,
    deleted_uplink: Box<[u8]>,
    source_combinations: Box<[usize]>,
    parameters: ActivationParameters,
    mappings: Box<[MappedBearer]>,
    hsdpa_active: bool,
}

impl crate::dedicated::ActivationMessage for DchCoexistenceActivation {
    fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        DchCoexistenceActivation::message(self, transaction)
    }

    fn activation(&self) -> ScheduledActivation {
        DchCoexistenceActivation::activation(self)
    }
}

impl DchCoexistenceActivation {
    pub fn new(
        dedicated: Arc<Rel99Configuration>,
        parameters: ActivationParameters,
        mappings: Vec<MappedBearer>,
        activation: ScheduledActivation,
        hsdpa_active: bool,
    ) -> Result<Self, DedicatedError> {
        let physical = dedicated.physical();
        physical.validate()?;
        // TS 25.213 Table 1C permits at most two E-DPDCHs alongside one DPDCH.
        if physical.uplink_dpdch_count != 1
            || parameters
                .transport
                .maximum_layout()
                .spreading_factors()
                .len()
                > 2
        {
            return Err(DedicatedError::Physical);
        }
        parameters
            .transport
            .wire_reference_powers(
                &parameters.references,
                parameters.minimum,
                parameters.scheduling,
            )
            .map_err(|_| DedicatedError::Format)?;
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
        if mappings.is_empty()
            || mappings.len() > MAX_MAPPINGS
            || parameters.flows.is_empty()
            || parameters.flows.len() > MAX_FLOWS
        {
            return Err(DedicatedError::Mapping);
        }
        for (index, flow) in parameters.flows.iter().enumerate() {
            if parameters.flows[..index]
                .iter()
                .any(|old| old.identity() == flow.identity())
                || !mappings
                    .iter()
                    .any(|mapping| mapping.uplink.flow() == flow.identity())
            {
                return Err(DedicatedError::Mapping);
            }
        }
        for (index, mapping) in mappings.iter().enumerate() {
            if mapping.bearer < 5
                || mappings[..index].iter().any(|old| {
                    old.bearer == mapping.bearer
                        || old.uplink.ddi() == mapping.uplink.ddi()
                        || old.uplink.logical() == mapping.uplink.logical()
                })
                || !parameters
                    .flows
                    .iter()
                    .any(|flow| flow.identity() == mapping.uplink.flow())
            {
                return Err(DedicatedError::Mapping);
            }
            let bearer = dedicated
                .bearer(mapping.bearer)
                .ok_or(DedicatedError::Bearer)?;
            if bearer.domain != Some(CnDomain::Ps)
                || matches!(bearer.rlc, Rel99Rlc::Transparent { .. })
            {
                return Err(DedicatedError::Bearer);
            }
            for old in &bearer.mapping.uplink {
                let transport = dedicated
                    .uplink()
                    .transport(old.transport)
                    .ok_or(DedicatedError::Transport)?;
                if !transport
                    .formats()
                    .any(|format| format.blocks == 0 || format.rlc_bits == 0)
                    || transport.formats().any(|format| {
                        format.blocks != 0
                            && format.rlc_bits != 0
                            && format.rlc_bits != mapping.uplink.pdu_bits()
                    })
                {
                    return Err(DedicatedError::Format);
                }
            }
            if let RetainedDownlink::Hsdpa { flow, logical } = mapping.downlink
                && (!hsdpa_active
                    || flow >= MAX_FLOWS as u8
                    || logical.is_some_and(|id| !(1..=15).contains(&id)))
            {
                return Err(DedicatedError::Mapping);
            }
        }
        r7::validate_mappings(&mappings, hsdpa_active)?;
        for tfcs in [dedicated.uplink(), dedicated.downlink()] {
            if tfcs
                .transports()
                .iter()
                .any(|transport| !usize::from(activation.cfn()).is_multiple_of(transport.frames()))
            {
                return Err(DedicatedError::Activation);
            }
        }
        let source = dedicated.uplink();
        let migrated: Vec<_> = source
            .transports()
            .iter()
            .enumerate()
            .filter_map(|(column, transport)| {
                let users: Vec<_> = dedicated
                    .bearers()
                    .iter()
                    .filter(|bearer| {
                        bearer
                            .mapping
                            .uplink
                            .iter()
                            .any(|mapping| mapping.transport == transport.id)
                    })
                    .collect();
                (!users.is_empty()
                    && users
                        .iter()
                        .all(|bearer| mappings.iter().any(|mapping| mapping.bearer == bearer.id)))
                .then_some(column)
            })
            .collect();
        let source_combinations: Vec<_> = source
            .combinations()
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                migrated
                    .iter()
                    .all(|column| {
                        source.transports()[*column]
                            .formats()
                            .nth(row.formats[*column])
                            .is_some_and(|format| format.blocks == 0)
                    })
                    .then_some(index)
            })
            .collect();
        let retained_columns: Vec<_> = (0..source.transports().len())
            .filter(|column| !migrated.contains(column))
            .collect();
        let deleted_uplink = migrated
            .iter()
            .map(|column| source.transports()[*column].id)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        // TS 25.331 8.6.5.12 assigns fresh TFCI values in complete-list order.
        let uplink = DchTfcs::new(
            source.direction(),
            retained_columns
                .iter()
                .map(|column| source.transports()[*column].clone())
                .collect(),
            source_combinations
                .iter()
                .map(|index| {
                    let mut row = source.combinations()[*index].clone();
                    row.formats = retained_columns
                        .iter()
                        .map(|column| row.formats[*column])
                        .collect();
                    row
                })
                .collect(),
        )?;
        if source.combinations().iter().any(|row| {
            !uplink.combinations().iter().any(|retained| {
                retained_columns
                    .iter()
                    .zip(&retained.formats)
                    .all(|(column, after)| row.formats[*column] == *after)
            })
        }) {
            return Err(DedicatedError::Combination);
        }
        Ok(Self {
            secondary_cell_active: false,
            definition: Arc::new(CoexistenceDefinition {
                dedicated,
                uplink,
                deleted_uplink,
                source_combinations: source_combinations.into_boxed_slice(),
                parameters,
                mappings: mappings.into_boxed_slice(),
                hsdpa_active,
            }),
            activation,
        })
    }

    pub fn rescheduled(&self, activation: ScheduledActivation) -> Result<Self, DedicatedError> {
        if activation.time().epoch != self.activation.time().epoch
            || [self.dedicated().uplink(), self.dedicated().downlink()]
                .into_iter()
                .any(|tfcs| {
                    tfcs.transports().iter().any(|transport| {
                        !usize::from(activation.cfn()).is_multiple_of(transport.frames())
                    })
                })
        {
            return Err(DedicatedError::Activation);
        }
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

    pub fn parameters(&self) -> &ActivationParameters {
        &self.definition.parameters
    }

    pub fn hsdpa_active(&self) -> bool {
        self.definition.hsdpa_active
    }
    pub fn dedicated(&self) -> &Arc<Rel99Configuration> {
        &self.definition.dedicated
    }
    pub fn mappings(&self) -> &[MappedBearer] {
        &self.definition.mappings
    }
    pub fn source_combinations(&self) -> &[usize] {
        &self.definition.source_combinations
    }
    pub fn uplink(&self) -> &DchTfcs {
        &self.definition.uplink
    }
    pub fn activation(&self) -> ScheduledActivation {
        self.activation
    }

    pub fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        encode_downlink_dedicated(&self.value(transaction)?).map_err(DedicatedError::Codec)
    }

    pub fn value(
        &self,
        transaction: TransactionId,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        ActivationEncoding {
            secondary_cell_active: self.secondary_cell_active,
            dedicated: self.dedicated(),
            parameters: self.parameters(),
            mappings: self.mappings(),
            activation: self.activation,
            hsdpa_active: self.hsdpa_active(),
            uplink_physical: None,
            deleted_uplink: (!self.definition.deleted_uplink.is_empty()).then(|| {
                ie::ULDeletedTransChInfoListR6(
                    self.definition
                        .deleted_uplink
                        .iter()
                        .map(|id| {
                            ie::ULTransportChannelIdentityR6::dch_usch(
                                ie::ULTransportChannelIdentityR6DchUsch::new(
                                    ie::ULTrCHType::dch,
                                    ie::TransportChannelIdentity(*id),
                                ),
                            )
                        })
                        .collect(),
                )
            }),
            retained_uplink: Some(self.uplink()),
        }
        .value(transaction)
    }
}

pub(super) struct ActivationEncoding<'a> {
    pub secondary_cell_active: bool,
    pub dedicated: &'a Rel99Configuration,
    pub parameters: &'a ActivationParameters,
    pub mappings: &'a [MappedBearer],
    pub activation: ScheduledActivation,
    pub hsdpa_active: bool,
    pub uplink_physical: Option<ie::ULDPCHInfoR6>,
    pub deleted_uplink: Option<ie::ULDeletedTransChInfoListR6>,
    pub retained_uplink: Option<&'a DchTfcs>,
}

impl ActivationEncoding<'_> {
    fn dedicated(&self) -> &Rel99Configuration {
        self.dedicated
    }

    fn parameters(&self) -> &ActivationParameters {
        self.parameters
    }

    fn mappings(&self) -> &[MappedBearer] {
        self.mappings
    }

    fn hsdpa_active(&self) -> bool {
        self.hsdpa_active
    }

    pub(super) fn value(
        &self,
        transaction: TransactionId,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        let mut rnti = FixedBitString::<16>::default();
        for bit in 0..16 {
            rnti.set(bit, self.parameters().e_rnti & (1 << (15 - bit)) != 0);
        }
        let affected = self
            .mappings()
            .iter()
            .map(|mapping| {
                let bearer = self
                    .dedicated()
                    .bearer(mapping.bearer)
                    .ok_or(DedicatedError::Bearer)?;
                let downlink = match mapping.downlink {
                    RetainedDownlink::Dedicated => Some(
                        bearer
                            .mapping
                            .downlink
                            .iter()
                            .map(|old| {
                                ie::DLLogicalChannelMappingR5::new(
                                    ie::DLTransportChannelTypeR5::dch(
                                        ie::TransportChannelIdentity(old.transport),
                                    ),
                                    old.logical.map(ie::LogicalChannelIdentity),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ),
                    RetainedDownlink::Hsdpa { flow, logical } => {
                        Some(vec![ie::DLLogicalChannelMappingR5::new(
                            ie::DLTransportChannelTypeR5::hsdsch(ie::MACDFlowIdentity(flow)),
                            logical.map(ie::LogicalChannelIdentity),
                        )])
                    }
                    RetainedDownlink::Ehs { .. } => None,
                };
                if downlink.as_ref().is_some_and(Vec::is_empty) {
                    return Err(DedicatedError::Mapping);
                }
                Ok(ie::RBInformationAffectedR6::new(
                    ie::RBIdentity(mapping.bearer),
                    ie::RBMappingInfoR6(vec![ie::RBMappingOptionR6::new(
                        Some(mapping.uplink.wire().map_err(|_| DedicatedError::Mapping)?),
                        downlink.map(ie::DLLogicalChannelMappingListR5),
                    )]),
                ))
            })
            .collect::<Result<Vec<_>, DedicatedError>>()?;
        let transport = ie::ULAddReconfTransChInformationR6::e_dch(
            ie::ULAddReconfTransChInformationR6EDch::new(
                match self.parameters().transport.interval() {
                    Tti::TwoMilliseconds => ie::EDCHTTI::tti2,
                    Tti::TenMilliseconds => ie::EDCHTTI::tti10,
                },
                if self.parameters().rv_zero_only {
                    ie::ULAddReconfTransChInformationR6EDchHarqInfo::rv0
                } else {
                    ie::ULAddReconfTransChInformationR6EDchHarqInfo::rvtable
                },
                Some(ie::EDCHAddReconfMACDFlowList(
                    self.parameters()
                        .flows
                        .iter()
                        .map(|flow| flow.wire())
                        .collect(),
                )),
            ),
        );
        let mut uplink_transports = Vec::new();
        let uplink_common = if let Some(retained) = self.retained_uplink {
            for channel in retained.transports() {
                uplink_transports.push(ie::ULAddReconfTransChInformationR6::dch_usch(
                    ie::ULAddReconfTransChInformationR6DchUsch::new(
                        ie::ULTrCHType::dch,
                        ie::TransportChannelIdentity(channel.id),
                        channel.wire_uplink()?,
                    ),
                ));
            }
            Some(ie::ULCommonTransChInfoR4::new(
                None,
                None,
                Some(ie::ULCommonTransChInfoR4ModeSpecificInfo::fdd(
                    ie::ULCommonTransChInfoR4ModeSpecificInfoFdd::new(retained.wire()?),
                )),
                None,
            ))
        } else {
            None
        };
        uplink_transports.push(transport);
        let complete = pdu::RadioBearerReconfigurationR6IEsSpecificationModeComplete {
            rab_information_reconfig_list: None,
            rb_information_reconfig_list: None,
            rb_information_affected_list: Some(ie::RBInformationAffectedListR6(affected)),
            rb_pdcpcontext_relocation_list: None,
            pdcp_rohc_target_mode: None,
            ul_common_trans_ch_info: uplink_common,
            ul_deleted_trans_ch_info_list: self.deleted_uplink.clone(),
            ul_add_reconf_trans_ch_info_list: Some(ie::ULAddReconfTransChInfoListR6(
                uplink_transports,
            )),
            dl_common_trans_ch_info: None,
            dl_deleted_trans_ch_info_list: None,
            dl_add_reconf_trans_ch_info_list: None,
        };
        let downlink = self.parameters().downlink;
        let link = ie::DLInformationPerRLR6::new(
            ie::DLInformationPerRLR6ModeSpecificInfo::fdd(
                ie::DLInformationPerRLR6ModeSpecificInfoFdd::new(
                    ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(
                        self.dedicated().physical().primary_scrambling,
                    )),
                    self.hsdpa_active(),
                    true,
                ),
            ),
            None,
            Some(downlink.absolute_wire()),
            Some(ie::DLInformationPerRLR6EHICHInfo::e_HICH_Information(
                downlink.acknowledgement_wire(),
            )),
            downlink
                .relative()
                .map(|grant| ie::DLInformationPerRLR6ERGCHInfo::e_RGCH_Information(grant.wire())),
            None,
        );
        // A fresh entity starts with zero grant until the NodeB publishes an admitted E-AGCH grant.
        let scheduling = ie::EDPDCHSchedulingTransmConfiguration::new(None, Some(ie::EDPDCHSchedulingTransmConfigurationServingGrant::new(
            ZERO_GRANT, ie::EDPDCHSchedulingTransmConfigurationServingGrantPrimarySecondaryGrantSelector::primary)));
        let uplink = ie::ULEDCHInformationR6::new(
            Some(ie::ULEDCHInformationR6MacEsEResetIndicator::R_true),
            Some(
                self.parameters()
                    .control
                    .wire()
                    .map_err(|_| DedicatedError::Physical)?,
            ),
            Some(
                self.parameters()
                    .transport
                    .wire_reference_powers(
                        &self.parameters().references,
                        self.parameters().minimum,
                        self.parameters().scheduling,
                    )
                    .map_err(|_| DedicatedError::Format)?,
            ),
            Some(scheduling),
        );
        let body = pdu::RadioBearerReconfigurationR6IEs {
            integrity_protection_mode_info: None,
            ciphering_mode_info: None,
            activation_time: Some(ie::ActivationTime(self.activation.cfn())),
            delay_restriction_flag: None,
            new_u_rnti: None,
            new_c_rnti: None,
            new_dsch_rnti: None,
            new_h_rnti: None,
            new_primary_e_rnti: Some(ie::ERNTI(rnti)),
            new_secondary_e_rnti: None,
            rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
            utran_drx_cycle_length_coeff: None,
            cn_information_info: None,
            ura_identity: None,
            specification_mode: pdu::RadioBearerReconfigurationR6IEsSpecificationMode::complete(
                complete,
            ),
            frequency_info: None,
            max_allowed_ul_tx_power: None,
            ul_dpch_info: self.uplink_physical.clone(),
            ul_edch_information: Some(uplink),
            dl_hspdsch_information: None,
            dl_common_information: None,
            dl_information_per_rl_list: Some(ie::DLInformationPerRLListR6(vec![link])),
            mbms_pl_service_restrict_info: None,
        };
        if self
            .mappings()
            .iter()
            .any(|mapping| matches!(mapping.downlink, RetainedDownlink::Ehs { .. }))
        {
            r7::validate_mappings(self.mappings(), self.hsdpa_active())?;
            return r7::reconfiguration_message(
                transaction,
                body,
                self.mappings(),
                self.secondary_cell_active,
            );
        }
        Ok(reconfiguration_message(transaction, body))
    }
}

pub(super) fn reconfiguration_message(
    transaction: TransactionId,
    body: pdu::RadioBearerReconfigurationR6IEs,
) -> class::DLDCCHMessage {
    let r6 = pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::r6(
            pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsR6::new(body, None, None));
    let r5 = pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(r6);
    class::DLDCCHMessage::new(None, class::DLDCCHMessageType::radioBearerReconfiguration(pdu::RadioBearerReconfiguration::later_than_r3(
            pdu::RadioBearerReconfigurationLaterThanR3::new(ie::RRCTransactionIdentifier(transaction.get()),
                pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(r5)))))
}
