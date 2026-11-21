// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{AckFeedback, ControlChannels, HarqConfiguration, MeasurementFeedback, QueueSet};
use crate::{
    dedicated::{DedicatedError, Rel99Configuration, Rel99Rlc, ScheduledActivation},
    establishment::TransactionId,
};
use rasn::types::FixedBitString;
use std::sync::Arc;
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{CnDomain, channel_codes::OvsfCode};

const FIRST_DATA_BEARER: u8 = 5;
const MAX_LOGICAL_CHANNEL: u8 = 15;
const MAX_MACD_FLOWS: u8 = 8;
const CT_BITS: usize = 4;
const H_RNTI_BITS: usize = u16::BITS as usize;

#[path = "activation_r7.rs"]
mod r7;

#[derive(Clone, Copy, Debug)]
pub struct MappedBearer {
    pub bearer: u8,
    pub flow: u8,
    pub logical: Option<u8>,
}

#[derive(Clone)]
pub struct ActivationParameters {
    pub h_rnti: u16,
    pub harq: HarqConfiguration,
    pub queues: QueueSet,
    pub control: ControlChannels,
    pub measurement: MeasurementFeedback,
    pub acknowledgement: AckFeedback,
}

pub struct InitialConfiguration {
    pub(super) cells: super::dual_cell::ConfiguredCells,
    mac_ehs: bool,
    dedicated: Arc<Rel99Configuration>,
    parameters: ActivationParameters,
    mappings: Box<[MappedBearer]>,
}

pub struct InitialActivation {
    configuration: Arc<InitialConfiguration>,
    activation: ScheduledActivation,
}

impl InitialConfiguration {
    pub fn new(
        dedicated: Arc<Rel99Configuration>,
        parameters: ActivationParameters,
        mappings: Vec<MappedBearer>,
    ) -> Result<Self, DedicatedError> {
        let mac_ehs = parameters.harq.category().limits().qam64;
        Self::with_mac_header(dedicated, parameters, mappings, mac_ehs)
    }

    pub fn new_mac_ehs(
        dedicated: Arc<Rel99Configuration>,
        parameters: ActivationParameters,
        mappings: Vec<MappedBearer>,
    ) -> Result<Self, DedicatedError> {
        Self::with_mac_header(dedicated, parameters, mappings, true)
    }

    /// Snapshot current HS mappings for a later RRC transaction, preserving queue identities.
    pub fn retained_for(
        &self,
        dedicated: Arc<Rel99Configuration>,
        active_bearers: &[u8],
    ) -> Result<Option<Self>, DedicatedError> {
        if active_bearers.len() > self.mappings.len()
            || active_bearers.iter().enumerate().any(|(index, bearer)| {
                active_bearers[..index].contains(bearer)
                    || !self
                        .mappings
                        .iter()
                        .any(|mapping| mapping.bearer == *bearer)
            })
        {
            return Err(DedicatedError::Mapping);
        }
        if active_bearers.is_empty() {
            return Ok(None);
        }
        let mappings: Vec<_> = self
            .mappings
            .iter()
            .copied()
            .filter(|mapping| active_bearers.contains(&mapping.bearer))
            .collect();
        let mut parameters = self.parameters.clone();
        parameters.queues = QueueSet::new(
            self.parameters
                .queues
                .queues()
                .iter()
                .filter(|queue| mappings.iter().any(|mapping| mapping.flow == queue.flow()))
                .cloned()
                .collect(),
        )
        .map_err(|_| DedicatedError::Mapping)?;
        let mut configuration =
            Self::with_mac_header(dedicated, parameters, mappings, self.mac_ehs)?;
        if let Some(secondary) = self.secondary_cell() {
            configuration = configuration.with_secondary_cell(secondary.clone())?;
        }
        Ok(Some(configuration))
    }

    /// Reject releases whose shared queue cannot be attributed to one surviving bearer set.
    pub(crate) fn validate_retained_flows(
        &self,
        target: &Rel99Configuration,
    ) -> Result<(), DedicatedError> {
        for mapping in self.mappings() {
            let retained = target.bearer(mapping.bearer).is_some();
            if self.mappings().iter().any(|other| {
                other.flow == mapping.flow && target.bearer(other.bearer).is_some() != retained
            }) {
                return Err(DedicatedError::Mapping);
            }
        }
        Ok(())
    }

    pub fn uses_mac_ehs(&self) -> bool {
        self.mac_ehs
    }

    fn with_mac_header(
        dedicated: Arc<Rel99Configuration>,
        parameters: ActivationParameters,
        mappings: Vec<MappedBearer>,
        evolved: bool,
    ) -> Result<Self, DedicatedError> {
        dedicated.physical().validate()?;
        for &index in &dedicated.physical().downlink_codes {
            let code = OvsfCode::new(dedicated.physical().downlink_sf, index)
                .map_err(|_| DedicatedError::Physical)?;
            if dedicated.physical().secondary_scrambling.is_none()
                && parameters
                    .control
                    .codes()
                    .iter()
                    .any(|control| control.overlaps(code))
            {
                return Err(DedicatedError::Physical);
            }
        }
        if mappings.is_empty() || mappings.len() > dedicated.bearers().len() {
            return Err(DedicatedError::Mapping);
        }
        for (index, mapping) in mappings.iter().enumerate() {
            if evolved
                && (mapping.logical.is_none()
                    || mappings[..index]
                        .iter()
                        .any(|other| other.logical == mapping.logical)
                    || parameters
                        .queues
                        .queues()
                        .iter()
                        .filter(|queue| queue.flow() == mapping.flow)
                        .count()
                        != 1)
            {
                return Err(DedicatedError::Mapping);
            }
            if mapping.bearer < FIRST_DATA_BEARER
                || mapping.flow >= MAX_MACD_FLOWS
                || mapping
                    .logical
                    .is_some_and(|logical| !(1..=MAX_LOGICAL_CHANNEL).contains(&logical))
                || mappings[..index]
                    .iter()
                    .any(|other| other.bearer == mapping.bearer)
            {
                return Err(DedicatedError::Mapping);
            }
            let shared = mappings
                .iter()
                .filter(|other| other.flow == mapping.flow)
                .count()
                > 1;
            if (shared && mapping.logical.is_none())
                || mappings[..index]
                    .iter()
                    .any(|other| other.flow == mapping.flow && other.logical == mapping.logical)
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
            let queues = parameters.queues.queues();
            if !queues.iter().any(|queue| queue.flow() == mapping.flow)
                || bearer.mapping.downlink.is_empty()
            {
                return Err(DedicatedError::Mapping);
            }
            for old in &bearer.mapping.downlink {
                let transport = dedicated
                    .downlink()
                    .transport(old.transport)
                    .ok_or(DedicatedError::Transport)?;
                if !transport
                    .formats()
                    .any(|format| format.blocks == 0 || format.rlc_bits == 0)
                {
                    return Err(DedicatedError::Format);
                }
                for format in transport.formats().filter(|format| format.rlc_bits != 0) {
                    if evolved {
                        if !format.rlc_bits.is_multiple_of(8) {
                            return Err(DedicatedError::Mapping);
                        }
                        continue;
                    }
                    let bits = usize::from(format.rlc_bits)
                        + if mapping.logical.is_some() {
                            CT_BITS
                        } else {
                            0
                        };
                    if !queues
                        .iter()
                        .filter(|queue| queue.flow() == mapping.flow)
                        .any(|queue| {
                            queue
                                .sizes()
                                .iter()
                                .flatten()
                                .any(|size| size.get() == bits)
                        })
                    {
                        return Err(DedicatedError::Mapping);
                    }
                }
            }
        }
        if parameters
            .queues
            .queues()
            .iter()
            .any(|queue| !mappings.iter().any(|mapping| mapping.flow == queue.flow()))
        {
            return Err(DedicatedError::Mapping);
        }
        Ok(Self {
            cells: super::dual_cell::ConfiguredCells::Single,
            mac_ehs: evolved,
            dedicated,
            parameters,
            mappings: mappings.into_boxed_slice(),
        })
    }

    pub fn dedicated(&self) -> &Arc<Rel99Configuration> {
        &self.dedicated
    }

    pub fn parameters(&self) -> &ActivationParameters {
        &self.parameters
    }

    pub fn mappings(&self) -> &[MappedBearer] {
        &self.mappings
    }

    fn value(
        &self,
        transaction: TransactionId,
        activation: ScheduledActivation,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        if let Some(secondary) = self.secondary_cell() {
            return self.value_with_secondary_cell(
                transaction,
                activation,
                super::dual_cell::SecondaryCellUpdate::Configure(secondary),
            );
        }
        if self.mac_ehs {
            return self.value_r7(transaction, activation);
        }
        let mut h_rnti = FixedBitString::<H_RNTI_BITS>::default();
        for index in 0..H_RNTI_BITS {
            h_rnti.set(
                index,
                self.parameters.h_rnti & (1 << (H_RNTI_BITS - 1 - index)) != 0,
            );
        }
        let affected = self
            .mappings
            .iter()
            .map(|mapping| {
                let bearer = self
                    .dedicated
                    .bearer(mapping.bearer)
                    .ok_or(DedicatedError::Bearer)?;
                let mut old = bearer.mapping.wire().0;
                let uplink = old
                    .pop()
                    .ok_or(DedicatedError::Mapping)?
                    .ul_logical_channel_mappings;
                let mut options = vec![ie::RBMappingOptionR5::new(
                    uplink,
                    Some(ie::DLLogicalChannelMappingListR5(vec![
                        ie::DLLogicalChannelMappingR5::new(
                            ie::DLTransportChannelTypeR5::hsdsch(ie::MACDFlowIdentity(
                                mapping.flow,
                            )),
                            mapping.logical.map(ie::LogicalChannelIdentity),
                        ),
                    ])),
                )];
                options.extend(bearer.common_channel().map(|common| common.option_r5()));
                Ok(ie::RBInformationAffectedR5::new(
                    ie::RBIdentity(mapping.bearer),
                    ie::RBMappingInfoR5(options),
                ))
            })
            .collect::<Result<Vec<_>, DedicatedError>>()?;
        let specification = pdu::RadioBearerReconfigurationR5IEsSpecificationModeComplete {
            rab_information_reconfig_list: None,
            rb_information_reconfig_list: None,
            rb_information_affected_list: Some(ie::RBInformationAffectedListR5(affected)),
            rb_pdcpcontext_relocation_list: None,
            ul_common_trans_ch_info: None,
            ul_deleted_trans_ch_info_list: None,
            ul_add_reconf_trans_ch_info_list: None,
            dummy: None,
            dl_common_trans_ch_info: None,
            dl_deleted_trans_ch_info_list: None,
            dl_add_reconf_trans_ch_info_list: Some(ie::DLAddReconfTransChInfoListR5(vec![
                ie::DLAddReconfTransChInformationR5::new(
                    ie::DLTrCHTypeId1R5::hsdsch(()),
                    ie::DLAddReconfTransChInformationR5TfsSignallingMode::hsdsch(
                        self.parameters
                            .queues
                            .initial_transport_info(&self.parameters.harq),
                    ),
                    None,
                ),
            ])),
        };
        let link = ie::DLInformationPerRLR5::new(
            ie::DLInformationPerRLR5ModeSpecificInfo::fdd(
                ie::DLInformationPerRLR5ModeSpecificInfoFdd::new(
                    ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(
                        self.dedicated.physical().primary_scrambling,
                    )),
                    None,
                    None,
                    true,
                ),
            ),
            None,
            None,
            None,
        );
        // Mapping-only changes preserve RLC/PDCP state and existing CS/PS security activation.
        let body = pdu::RadioBearerReconfigurationR5IEs {
            integrity_protection_mode_info: None,
            ciphering_mode_info: None,
            activation_time: Some(ie::ActivationTime(activation.cfn())),
            new_u_rnti: None,
            new_c_rnti: None,
            new_dsch_rnti: None,
            new_h_rnti: Some(ie::HRNTI(h_rnti)),
            rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
            utran_drx_cycle_length_coeff: None,
            cn_information_info: None,
            ura_identity: None,
            specification_mode: pdu::RadioBearerReconfigurationR5IEsSpecificationMode::complete(
                specification,
            ),
            frequency_info: None,
            max_allowed_ul_tx_power: None,
            ul_channel_requirement: Some(self.uplink()?),
            mode_specific_phys_ch_info:
                pdu::RadioBearerReconfigurationR5IEsModeSpecificPhysChInfo::fdd(
                    pdu::RadioBearerReconfigurationR5IEsModeSpecificPhysChInfoFdd::new(None),
                ),
            dl_hspdsch_information: Some(
                self.parameters
                    .control
                    .downlink_r5(self.parameters.measurement),
            ),
            dl_common_information: None,
            dl_information_per_rl_list: Some(ie::DLInformationPerRLListR5(vec![link])),
        };
        Ok(class::DLDCCHMessage::new(None, class::DLDCCHMessageType::radioBearerReconfiguration(
            pdu::RadioBearerReconfiguration::later_than_r3(pdu::RadioBearerReconfigurationLaterThanR3::new(
                ie::RRCTransactionIdentifier(transaction.get()),
                pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(
                    pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::r5(
                        pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsR5::new(body, None, None),
                    ),
                ),
            )),
        )))
    }

    fn uplink(&self) -> Result<ie::ULChannelRequirementR5, DedicatedError> {
        self.uplink_for(&self.dedicated)
    }

    fn uplink_for(
        &self,
        dedicated: &Rel99Configuration,
    ) -> Result<ie::ULChannelRequirementR5, DedicatedError> {
        let physical = dedicated.physical();
        let ie::ULChannelRequirement::ul_DPCH_Info(original) = physical.uplink_wire()? else {
            return Err(DedicatedError::Physical);
        };
        let ie::ULDPCHInfoModeSpecificInfo::fdd(original) = original.mode_specific_info else {
            return Err(DedicatedError::Physical);
        };
        Ok(ie::ULChannelRequirementR5::ul_DPCH_Info(
            ie::ULDPCHInfoR5::new(
                Some(self.parameters.acknowledgement.power_r5(physical)?),
                ie::ULDPCHInfoR5ModeSpecificInfo::fdd(ie::ULDPCHInfoR5ModeSpecificInfoFdd::new(
                    original.scrambling_code_type,
                    original.scrambling_code,
                    original.number_of_dpdch,
                    original.spreading_factor,
                    original.tfci_existence,
                    original.number_of_fbi_bits,
                    original.puncturing_limit,
                )),
            ),
        ))
    }
}

impl InitialActivation {
    pub fn new(
        dedicated: Arc<Rel99Configuration>,
        parameters: ActivationParameters,
        mappings: Vec<MappedBearer>,
        activation: ScheduledActivation,
    ) -> Result<Self, DedicatedError> {
        Self::schedule(
            Arc::new(InitialConfiguration::new(dedicated, parameters, mappings)?),
            activation,
        )
    }

    pub fn schedule(
        configuration: Arc<InitialConfiguration>,
        activation: ScheduledActivation,
    ) -> Result<Self, DedicatedError> {
        let dedicated = configuration.dedicated();
        for tfcs in [dedicated.uplink(), dedicated.downlink()] {
            if tfcs
                .transports()
                .iter()
                .any(|transport| !usize::from(activation.cfn()).is_multiple_of(transport.frames()))
            {
                return Err(DedicatedError::Activation);
            }
        }
        Ok(Self {
            configuration,
            activation,
        })
    }

    pub fn configuration(&self) -> &Arc<InitialConfiguration> {
        &self.configuration
    }

    pub fn dedicated(&self) -> &Arc<Rel99Configuration> {
        self.configuration.dedicated()
    }

    pub fn parameters(&self) -> &ActivationParameters {
        self.configuration.parameters()
    }

    pub fn mappings(&self) -> &[MappedBearer] {
        self.configuration.mappings()
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
        self.configuration.value(transaction, self.activation)
    }
}
