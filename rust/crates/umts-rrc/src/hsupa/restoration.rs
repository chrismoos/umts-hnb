// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{RetainedDownlink, UplinkActivation, activation::reconfiguration_message};
use crate::{
    dedicated::{ActivationMessage, DedicatedError, Rel99Configuration, ScheduledActivation},
    establishment::TransactionId,
};
use std::sync::Arc;
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};

pub struct DchRestoration {
    previous: Arc<UplinkActivation>,
    activation: ScheduledActivation,
}

impl DchRestoration {
    pub fn new(
        previous: Arc<UplinkActivation>,
        activation: ScheduledActivation,
    ) -> Result<Self, DedicatedError> {
        previous.rescheduled(activation)?;
        if activation.time().index <= previous.activation().time().index {
            return Err(DedicatedError::Activation);
        }
        Ok(Self {
            previous,
            activation,
        })
    }

    pub fn previous(&self) -> &Arc<UplinkActivation> {
        &self.previous
    }

    pub fn dedicated(&self) -> &Arc<Rel99Configuration> {
        self.previous.dedicated()
    }

    pub fn activation(&self) -> ScheduledActivation {
        self.activation
    }

    pub fn rescheduled(&self, activation: ScheduledActivation) -> Result<Self, DedicatedError> {
        Self::new(Arc::clone(&self.previous), activation)
    }

    pub fn value(
        &self,
        transaction: TransactionId,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        let configuration = self.dedicated();
        let affected = self.previous.mappings().iter().map(|mapping| {
            let bearer = configuration.bearer(mapping.bearer).ok_or(DedicatedError::Bearer)?;
            let uplink = bearer.mapping.uplink.iter().map(|old| ie::ULLogicalChannelMappingR6::new(
                ie::ULLogicalChannelMappingR6UlTrCHType::dch_rach_usch(
                    ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUsch::new(
                        ie::ULTransportChannelType::dch(ie::TransportChannelIdentity(old.transport)),
                        old.logical.map(ie::LogicalChannelIdentity),
                        ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUschRlcSizeList::allSizes(()),
                    )),
                ie::MACLogicalChannelPriority(old.priority),
            )).collect::<Vec<_>>();
            let uplink = match uplink.len() {
                1 => ie::ULLogicalChannelMappingsR6::oneLogicalChannel(uplink.into_iter().next().expect("one mapping")),
                2 => ie::ULLogicalChannelMappingsR6::twoLogicalChannels(ie::ULLogicalChannelMappingListR6::new(true, uplink)),
                _ => return Err(DedicatedError::Mapping),
            };
            let downlink = match mapping.downlink {
                RetainedDownlink::Dedicated => Some(bearer.mapping.downlink.iter().map(|old| {
                    ie::DLLogicalChannelMappingR5::new(
                        ie::DLTransportChannelTypeR5::dch(ie::TransportChannelIdentity(old.transport)),
                        old.logical.map(ie::LogicalChannelIdentity),
                    )
                }).collect::<Vec<_>>()),
                RetainedDownlink::Hsdpa { flow, logical } => Some(vec![ie::DLLogicalChannelMappingR5::new(
                    ie::DLTransportChannelTypeR5::hsdsch(ie::MACDFlowIdentity(flow)),
                    logical.map(ie::LogicalChannelIdentity),
                )]),
                RetainedDownlink::Ehs { .. } => None,
            };
            Ok(ie::RBInformationAffectedR6::new(ie::RBIdentity(mapping.bearer), ie::RBMappingInfoR6(vec![
                ie::RBMappingOptionR6::new(Some(uplink), downlink.map(ie::DLLogicalChannelMappingListR5)),
                crate::dedicated::common_mapping::CommonChannel::for_bearer(mapping.bearer).option_r6(),
            ])))
        }).collect::<Result<Vec<_>, DedicatedError>>()?;
        let added = configuration
            .uplink()
            .transports()
            .iter()
            .map(|transport| {
                Ok(ie::ULAddReconfTransChInformationR6::dch_usch(
                    ie::ULAddReconfTransChInformationR6DchUsch::new(
                        ie::ULTrCHType::dch,
                        ie::TransportChannelIdentity(transport.id),
                        transport.wire_uplink()?,
                    ),
                ))
            })
            .collect::<Result<Vec<_>, DedicatedError>>()?;
        // TS 25.331 8.6.5.7 and 8.5.28 stop E-DCH and reset HARQ when all mapped flows are deleted.
        let deleted = self
            .previous
            .parameters()
            .flows
            .iter()
            .map(|flow| {
                ie::ULTransportChannelIdentityR6::e_dch(ie::EDCHMACDFlowIdentity(flow.identity()))
            })
            .collect();
        let complete = pdu::RadioBearerReconfigurationR6IEsSpecificationModeComplete {
            rab_information_reconfig_list: None,
            rb_information_reconfig_list: None,
            rb_information_affected_list: Some(ie::RBInformationAffectedListR6(affected)),
            rb_pdcpcontext_relocation_list: None,
            pdcp_rohc_target_mode: None,
            ul_common_trans_ch_info: Some(ie::ULCommonTransChInfoR4::new(
                None,
                None,
                Some(ie::ULCommonTransChInfoR4ModeSpecificInfo::fdd(
                    ie::ULCommonTransChInfoR4ModeSpecificInfoFdd::new(
                        configuration.uplink().wire()?,
                    ),
                )),
                None,
            )),
            ul_deleted_trans_ch_info_list: Some(ie::ULDeletedTransChInfoListR6(deleted)),
            ul_add_reconf_trans_ch_info_list: Some(ie::ULAddReconfTransChInfoListR6(added)),
            dl_common_trans_ch_info: None,
            dl_deleted_trans_ch_info_list: None,
            dl_add_reconf_trans_ch_info_list: None,
        };
        let body = pdu::RadioBearerReconfigurationR6IEs {
            integrity_protection_mode_info: None,
            ciphering_mode_info: None,
            activation_time: Some(ie::ActivationTime(self.activation.cfn())),
            delay_restriction_flag: None,
            new_u_rnti: None,
            new_c_rnti: None,
            new_dsch_rnti: None,
            new_h_rnti: None,
            new_primary_e_rnti: None,
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
            ul_dpch_info: Some(restore_physical(configuration)?),
            ul_edch_information: None,
            dl_hspdsch_information: None,
            dl_common_information: None,
            dl_information_per_rl_list: None,
            mbms_pl_service_restrict_info: None,
        };
        if self
            .previous
            .mappings()
            .iter()
            .any(|mapping| matches!(mapping.downlink, RetainedDownlink::Ehs { .. }))
        {
            return super::activation::r7::reconfiguration_message(
                transaction,
                body,
                self.previous.mappings(),
                self.previous.secondary_cell_active(),
            );
        }
        Ok(reconfiguration_message(transaction, body))
    }
}

impl ActivationMessage for DchRestoration {
    fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        encode_downlink_dedicated(&self.value(transaction)?).map_err(DedicatedError::Codec)
    }
    fn activation(&self) -> ScheduledActivation {
        self.activation
    }
}

fn restore_physical(
    configuration: &Rel99Configuration,
) -> Result<ie::ULDPCHInfoR6, DedicatedError> {
    let ie::ULChannelRequirement::ul_DPCH_Info(previous) =
        configuration.physical().uplink_wire()?
    else {
        return Err(DedicatedError::Physical);
    };
    let ie::ULDPCHInfoModeSpecificInfo::fdd(previous) = previous.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    Ok(ie::ULDPCHInfoR6::new(
        None,
        ie::ULDPCHInfoR6ModeSpecificInfo::fdd(ie::ULDPCHInfoR6ModeSpecificInfoFdd::new(
            previous.scrambling_code_type,
            previous.scrambling_code,
            ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresence::present(
                ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresencePresent::new(
                    previous.number_of_dpdch,
                    previous.spreading_factor,
                    previous.tfci_existence,
                    previous.number_of_fbi_bits,
                    previous.puncturing_limit,
                ),
            ),
        )),
    ))
}
