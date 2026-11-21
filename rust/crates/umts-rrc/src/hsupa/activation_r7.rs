// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[path = "physical_r7.rs"]
mod physical;

pub(in crate::hsupa) fn validate_mappings(
    mappings: &[MappedBearer],
    hsdpa_active: bool,
) -> Result<(), DedicatedError> {
    let evolved = mappings
        .iter()
        .any(|mapping| matches!(mapping.downlink, RetainedDownlink::Ehs { .. }));
    if !evolved {
        return Ok(());
    }
    let mut logicals = 0_u16;
    for mapping in mappings {
        match mapping.downlink {
            RetainedDownlink::Ehs { queue, logical } => {
                if !hsdpa_active
                    || queue >= 8
                    || !(1..=15).contains(&logical)
                    || logicals & (1 << logical) != 0
                {
                    return Err(DedicatedError::Mapping);
                }
                logicals |= 1 << logical;
            }
            RetainedDownlink::Hsdpa { .. } => return Err(DedicatedError::Mapping),
            RetainedDownlink::Dedicated => {}
        }
    }
    Ok(())
}

// TS 25.331 8.6.4.8 replaces mapping options, so MAC-ehs downlink mappings remain explicit.
pub(in crate::hsupa) fn reconfiguration_message(
    transaction: TransactionId,
    body: pdu::RadioBearerReconfigurationR6IEs,
    mappings: &[MappedBearer],
    secondary_cell_active: bool,
) -> Result<class::DLDCCHMessage, DedicatedError> {
    validate_mappings(mappings, true)?;
    let pdu::RadioBearerReconfigurationR6IEsSpecificationMode::complete(specification) =
        body.specification_mode
    else {
        return Err(DedicatedError::Mapping);
    };
    let affected = specification
        .rb_information_affected_list
        .map(|list| {
            list.0
                .into_iter()
                .map(|rb| {
                    let mapping = mappings
                        .iter()
                        .find(|mapping| mapping.bearer == rb.rb_identity.0)
                        .ok_or(DedicatedError::Mapping)?;
                    // The first option is the E-DCH one. The FACH/RACH option is rebuilt below.
                    let mut options = rb
                        .rb_mapping_info
                        .0
                        .into_iter()
                        .take(1)
                        .map(|option| {
                            let downlink = match mapping.downlink {
                                RetainedDownlink::Ehs { queue, logical } => {
                                    vec![ie::DLLogicalChannelMappingR7::new(
                                        ie::DLTransportChannelTypeR7::hsdsch(
                                            ie::DLTransportChannelTypeR7Hsdsch::mac_ehs(
                                                ie::MACEhsQueueId(queue),
                                            ),
                                        ),
                                        Some(ie::LogicalChannelIdentity(logical)),
                                    )]
                                }
                                RetainedDownlink::Dedicated => option
                                    .dl_logical_channel_mapping_list
                                    .ok_or(DedicatedError::Mapping)?
                                    .0
                                    .into_iter()
                                    .map(|old| {
                                        let ie::DLTransportChannelTypeR5::dch(channel) =
                                            old.dl_transport_channel_type
                                        else {
                                            return Err(DedicatedError::Mapping);
                                        };
                                        Ok(ie::DLLogicalChannelMappingR7::new(
                                            ie::DLTransportChannelTypeR7::dch(channel),
                                            old.logical_channel_identity,
                                        ))
                                    })
                                    .collect::<Result<Vec<_>, DedicatedError>>()?,
                                RetainedDownlink::Hsdpa { .. } => {
                                    return Err(DedicatedError::Mapping);
                                }
                            };
                            Ok(ie::RBMappingOptionR7::new(
                                option.ul_logical_channel_mappings,
                                Some(ie::DLLogicalChannelMappingListR7(downlink)),
                            ))
                        })
                        .collect::<Result<Vec<_>, DedicatedError>>()?;
                    options.push(
                        crate::dedicated::common_mapping::CommonChannel::for_bearer(
                            rb.rb_identity.0,
                        )
                        .option_r7(),
                    );
                    Ok(ie::RBInformationAffectedR7::new(
                        rb.rb_identity,
                        ie::RBMappingInfoR7(options),
                    ))
                })
                .collect::<Result<Vec<_>, DedicatedError>>()
                .map(ie::RBInformationAffectedListR7)
        })
        .transpose()?;
    let complete = pdu::RadioBearerReconfigurationR7IEsSpecificationModeComplete {
        rab_information_reconfig_list: specification.rab_information_reconfig_list,
        rab_information_mbmsptp_list: None,
        rb_information_reconfig_list: None,
        rb_information_affected_list: affected,
        rb_pdcpcontext_relocation_list: specification.rb_pdcpcontext_relocation_list,
        pdcp_rohc_target_mode: specification.pdcp_rohc_target_mode,
        ul_common_trans_ch_info: specification.ul_common_trans_ch_info,
        ul_deleted_trans_ch_info_list: specification.ul_deleted_trans_ch_info_list,
        ul_add_reconf_trans_ch_info_list: specification.ul_add_reconf_trans_ch_info_list.map(
            |list| ie::ULAddReconfTransChInfoListR7(list.0.into_iter().map(transport).collect()),
        ),
        dl_common_trans_ch_info: specification.dl_common_trans_ch_info,
        dl_deleted_trans_ch_info_list: None,
        dl_add_reconf_trans_ch_info_list: None,
    };
    let body = pdu::RadioBearerReconfigurationR7IEs {
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        activation_time: body.activation_time,
        delay_restriction_flag: body.delay_restriction_flag,
        new_u_rnti: body.new_u_rnti,
        new_c_rnti: body.new_c_rnti,
        new_dsch_rnti: body.new_dsch_rnti,
        new_h_rnti: body.new_h_rnti,
        new_primary_e_rnti: body.new_primary_e_rnti,
        new_secondary_e_rnti: body.new_secondary_e_rnti,
        rrc_state_indicator: body.rrc_state_indicator,
        ue_mobility_state_indicator: None,
        utran_drx_cycle_length_coeff: None,
        cn_information_info: body.cn_information_info,
        ura_identity: body.ura_identity,
        support_for_change_of_ue_capability: None,
        response_to_change_of_ue_capability: None,
        specification_mode: pdu::RadioBearerReconfigurationR7IEsSpecificationMode::complete(
            complete,
        ),
        frequency_info: body.frequency_info,
        multi_frequency_info: None,
        dtx_drx_timing_info: None,
        dtx_drx_info: None,
        hs_scch_less_info: None,
        mimo_parameters: None,
        max_allowed_ul_tx_power: body.max_allowed_ul_tx_power,
        ul_dpch_info: body.ul_dpch_info.map(physical::uplink).transpose()?,
        ul_edch_information: body.ul_edch_information.map(physical::edch),
        // TS 25.331 10.3.6.23a: omitting this IE preserves the active 64QAM configuration.
        dl_hspdsch_information: None,
        dl_common_information: None,
        dl_information_per_rl_list: body
            .dl_information_per_rl_list
            .map(|list| {
                list.0
                    .into_iter()
                    .map(physical::link)
                    .collect::<Result<Vec<_>, _>>()
                    .map(ie::DLInformationPerRLListR7)
            })
            .transpose()?,
        mbms_pl_service_restrict_info: body.mbms_pl_service_restrict_info,
    };
    if secondary_cell_active {
        return crate::hsdpa::dual_cell::reconfigure(
            transaction,
            body,
            crate::hsdpa::SecondaryCellUpdate::Continue,
        );
    }
    Ok(class::DLDCCHMessage::new(None, class::DLDCCHMessageType::radioBearerReconfiguration(
        pdu::RadioBearerReconfiguration::later_than_r3(pdu::RadioBearerReconfigurationLaterThanR3::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(
                pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
                    pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                        pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r7(
                            pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsR7 {
                                radio_bearer_reconfiguration_r7: body, v780_non_critical_extensions: None,
                            },
                        ),
                    ),
                ),
            ),
        )),
    )))
}

pub(super) fn process_allocation(
    transaction: TransactionId,
    uplink: ie::ULEDCHInformationR6,
) -> Result<class::DLDCCHMessage, DedicatedError> {
    crate::hsdpa::dual_cell::process_allocation(transaction, physical::edch(uplink))
}

fn transport(original: ie::ULAddReconfTransChInformationR6) -> ie::ULAddReconfTransChInformationR7 {
    match original {
        ie::ULAddReconfTransChInformationR6::dch_usch(dch) => ie::ULAddReconfTransChInformationR7::dch_usch(
            ie::ULAddReconfTransChInformationR7DchUsch::new(dch.ul_transport_channel_type, dch.transport_channel_identity, dch.transport_format_set),
        ),
        ie::ULAddReconfTransChInformationR6::e_dch(edch) => ie::ULAddReconfTransChInformationR7::e_dch(
            ie::ULAddReconfTransChInformationR7EDch::new(
                ie::ULAddReconfTransChInformationR7EDchModeSpecific::fdd(ie::ULAddReconfTransChInformationR7EDchModeSpecificFdd::new(edch.tti)),
                match edch.harq_info {
                    ie::ULAddReconfTransChInformationR6EDchHarqInfo::rv0 => ie::ULAddReconfTransChInformationR7EDchHarqInfo::rv0,
                    ie::ULAddReconfTransChInformationR6EDchHarqInfo::rvtable => ie::ULAddReconfTransChInformationR7EDchHarqInfo::rvtable,
                },
                edch.add_reconf_mac_d_flow_list.map(|list| ie::EDCHAddReconfMACDFlowListR7(list.0.into_iter().map(|flow| {
                    ie::EDCHAddReconfMACDFlowR7::new(
                        flow.mac_d_flow_identity, flow.mac_d_flow_power_offset, flow.mac_d_flow_max_retrans,
                        None, flow.mac_d_flow_multiplexing_list,
                        flow.transmission_grant_type.map(|grant| match grant {
                            ie::EDCHAddReconfMACDFlowTransmissionGrantType::scheduledTransmissionGrantInfo(()) => ie::EDCHAddReconfMACDFlowR7TransmissionGrantType::scheduledTransmissionGrantInfo(()),
                            ie::EDCHAddReconfMACDFlowTransmissionGrantType::non_ScheduledTransGrantInfo(info) => {
                                ie::EDCHAddReconfMACDFlowR7TransmissionGrantType::non_ScheduledTransGrantInfo(
                                    ie::EDCHAddReconfMACDFlowR7TransmissionGrantTypeNonScheduledTransGrantInfo::new(
                                        ie::EDCHAddReconfMACDFlowR7TransmissionGrantTypeNonScheduledTransGrantInfoModeSpecificInfo::fdd(
                                            ie::EDCHAddReconfMACDFlowR7TransmissionGrantTypeNonScheduledTransGrantInfoModeSpecificInfoFdd::new(info.max_mac_e_pducontents, info.ms2_non_sched_transm_grant_harqalloc),
                                        ),
                                    ),
                                )
                            }
                        }),
                    )
                }).collect())),
            ),
        ),
    }
}
