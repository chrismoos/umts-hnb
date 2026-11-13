// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(super) fn mapping(value: ie::RBMappingInfoR7) -> ie::RBMappingInfoR8 {
    ie::RBMappingInfoR8(
        value
            .0
            .into_iter()
            .map(|value| {
                ie::RBMappingOptionR8::new(
                    value
                        .ul_logical_channel_mappings
                        .map(|uplink| match uplink {
                            ie::ULLogicalChannelMappingsR6::oneLogicalChannel(value) => {
                                ie::ULLogicalChannelMappingsR8::oneLogicalChannel(logical(value))
                            }
                            ie::ULLogicalChannelMappingsR6::twoLogicalChannels(value) => {
                                ie::ULLogicalChannelMappingsR8::twoLogicalChannels(
                                    ie::ULLogicalChannelMappingListR8::new(
                                        value.rlc_logical_channel_mapping_indicator,
                                        value
                                            .ul_logical_channel_mapping
                                            .into_iter()
                                            .map(logical)
                                            .collect(),
                                    ),
                                )
                            }
                        }),
                    value.dl_logical_channel_mapping_list,
                )
            })
            .collect(),
    )
}

fn logical(value: ie::ULLogicalChannelMappingR6) -> ie::ULLogicalChannelMappingR8 {
    let transport = match value.ul_tr_ch_type {
        ie::ULLogicalChannelMappingR6UlTrCHType::dch_rach_usch(value) => {
            let sizes = match value.rlc_size_list {
                ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUschRlcSizeList::allSizes(()) => {
                    ie::ULLogicalChannelMappingR8UlTrCHTypeDchRachUschRlcSizeList::allSizes(())
                }
                ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUschRlcSizeList::configured(()) => {
                    ie::ULLogicalChannelMappingR8UlTrCHTypeDchRachUschRlcSizeList::configured(())
                }
                ie::ULLogicalChannelMappingR6UlTrCHTypeDchRachUschRlcSizeList::explicitList(
                    sizes,
                ) => ie::ULLogicalChannelMappingR8UlTrCHTypeDchRachUschRlcSizeList::explicitList(
                    sizes,
                ),
            };
            ie::ULLogicalChannelMappingR8UlTrCHType::dch_rach_usch(
                ie::ULLogicalChannelMappingR8UlTrCHTypeDchRachUsch::new(
                    value.ul_transport_channel_type,
                    value.logical_channel_identity,
                    sizes,
                ),
            )
        }
        ie::ULLogicalChannelMappingR6UlTrCHType::e_dch(value) => {
            ie::ULLogicalChannelMappingR8UlTrCHType::e_dch(
                ie::ULLogicalChannelMappingR8UlTrCHTypeEDch::new(
                    value.logical_channel_identity,
                    value.e_dch_mac_d_flow_identity,
                    ie::ULLogicalChannelMappingR8UlTrCHTypeEDchRlcPDUSize::fixedSize(
                        ie::ULLogicalChannelMappingR8UlTrCHTypeEDchRlcPDUSizeFixedSize::new(
                            value.ddi,
                            value.rlc_pdu_size_list,
                        ),
                    ),
                    value.include_in_scheduling_info,
                ),
            )
        }
    };
    ie::ULLogicalChannelMappingR8::new(transport, value.mac_logical_channel_priority)
}

pub(super) fn affected(value: ie::RBInformationAffectedListR7) -> ie::RBInformationAffectedListR8 {
    ie::RBInformationAffectedListR8(
        value
            .0
            .into_iter()
            .map(|value| {
                ie::RBInformationAffectedR8::new(value.rb_identity, mapping(value.rb_mapping_info))
            })
            .collect(),
    )
}

pub(super) fn rabs(value: ie::RABInformationSetupListR7) -> ie::RABInformationSetupListR8 {
    ie::RABInformationSetupListR8(
        value
            .0
            .into_iter()
            .map(|value| {
                ie::RABInformationSetupR8::new(
                    value.rab_info,
                    None,
                    None,
                    ie::RBInformationSetupListR8(
                        value
                            .rb_information_setup_list
                            .0
                            .into_iter()
                            .map(|bearer| {
                                ie::RBInformationSetupR8::new(
                                    bearer.rb_identity,
                                    bearer.pdcp_info,
                                    bearer.rlc_info_choice,
                                    mapping(bearer.rb_mapping_info),
                                )
                            })
                            .collect(),
                    ),
                )
            })
            .collect(),
    )
}

pub(super) fn uplink(
    value: ie::ULAddReconfTransChInfoListR7,
) -> Result<ie::ULAddReconfTransChInfoListR8, DedicatedError> {
    value
        .0
        .into_iter()
        .map(|value| match value {
            ie::ULAddReconfTransChInformationR7::dch_usch(value) => {
                Ok(ie::ULAddReconfTransChInformationR8::dch_usch(
                    ie::ULAddReconfTransChInformationR8DchUsch::new(
                        value.ul_transport_channel_type,
                        value.transport_channel_identity,
                        value.transport_format_set,
                    ),
                ))
            }
            ie::ULAddReconfTransChInformationR7::e_dch(value) => {
                uplink_edch(value).map(ie::ULAddReconfTransChInformationR8::e_dch)
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(ie::ULAddReconfTransChInfoListR8)
}

pub(super) fn hspdsch(
    value: ie::DLHSPDSCHInformationR7,
) -> Result<ie::DLHSPDSCHInformationR8, DedicatedError> {
    let ie::DLHSPDSCHInformationR7ModeSpecificInfo::fdd(fdd) = value.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    Ok(ie::DLHSPDSCHInformationR8::new(
        value.hs_scch_info,
        value.measurement_feedback_info,
        ie::DLHSPDSCHInformationR8ModeSpecificInfo::fdd(
            ie::DLHSPDSCHInformationR8ModeSpecificInfoFdd::new(
                fdd.dl_64_qam_configured.map(|_| {
                    ie::DLHSPDSCHInformationR8ModeSpecificInfoFddDl64QAMConfigured::R_true
                }),
                None,
            ),
        ),
    ))
}

pub(super) fn common(
    value: ie::DLCommonInformationR7,
) -> Result<ie::DLCommonInformationR8, DedicatedError> {
    let ie::DLCommonInformationR7ModeSpecificInfo::fdd(fdd) = value.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    if fdd.dpch_compressed_mode_info.is_some() {
        return Err(DedicatedError::Physical);
    }
    Ok(ie::DLCommonInformationR8::new(
        value.dl_dpch_info_common.map(|value| match value {
            ie::DLCommonInformationR7DlDpchInfoCommon::dl_DPCH_InfoCommon(value) => {
                ie::DLCommonInformationR8DlDpchInfoCommon::dl_DPCH_InfoCommon(value)
            }
            ie::DLCommonInformationR7DlDpchInfoCommon::dl_FDPCH_InfoCommon(value) => {
                ie::DLCommonInformationR8DlDpchInfoCommon::dl_FDPCH_InfoCommon(value)
            }
        }),
        ie::DLCommonInformationR8ModeSpecificInfo::fdd(
            ie::DLCommonInformationR8ModeSpecificInfoFdd::new(
                fdd.default_dpch_offset_value,
                None,
                fdd.tx_diversity_mode,
            ),
        ),
        value
            .mac_hs_reset_indicator
            .map(|_| ie::DLCommonInformationR8MacHsResetIndicator::R_true),
        value
            .post_verification_period
            .map(|_| ie::DLCommonInformationR8PostVerificationPeriod::R_true),
    ))
}

pub(super) fn links(
    value: ie::DLInformationPerRLListR7,
) -> Result<ie::DLInformationPerRLListR8, DedicatedError> {
    value.0.into_iter().map(|value| {
        let ie::DLInformationPerRLR7ModeSpecificInfo::fdd(fdd) = value.mode_specific_info else { return Err(DedicatedError::Physical) };
        let ie::DLInformationPerRLR7ModeSpecificInfo2::fdd(extra) = value.mode_specific_info2 else { return Err(DedicatedError::Physical) };
        let e_agch = value.e_agch_information.map(e_agch).transpose()?;
        Ok(ie::DLInformationPerRLR8::new(
            ie::DLInformationPerRLR8ModeSpecificInfo::fdd(ie::DLInformationPerRLR8ModeSpecificInfoFdd::new(fdd.primary_cpich_info, fdd.serving_hsdsch_rl_indicator, fdd.serving_edch_rl_indicator)),
            value.dl_dpch_info.map(|value| match value {
                ie::DLInformationPerRLR7DlDpchInfo::dl_DPCH_InfoPerRL(value) => ie::DLInformationPerRLR8DlDpchInfo::dl_DPCH_InfoPerRL(value),
                ie::DLInformationPerRLR7DlDpchInfo::dl_FDPCH_InfoPerRL(value) => ie::DLInformationPerRLR8DlDpchInfo::dl_FDPCH_InfoPerRL(value),
            }), e_agch,
            ie::DLInformationPerRLR8ModeSpecificInfo2::fdd(ie::DLInformationPerRLR8ModeSpecificInfo2Fdd::new(
                extra.e_hich_info.map(|value| match value {
                    ie::DLInformationPerRLR7ModeSpecificInfo2FddEHICHInfo::e_HICH_Information(value) => ie::DLInformationPerRLR8ModeSpecificInfo2FddEHICHInfo::e_HICH_Information(value),
                    ie::DLInformationPerRLR7ModeSpecificInfo2FddEHICHInfo::releaseIndicator(()) => ie::DLInformationPerRLR8ModeSpecificInfo2FddEHICHInfo::releaseIndicator(()),
                }),
                extra.e_rgch_info.map(|value| match value {
                    ie::DLInformationPerRLR7ModeSpecificInfo2FddERGCHInfo::e_RGCH_Information(value) => ie::DLInformationPerRLR8ModeSpecificInfo2FddERGCHInfo::e_RGCH_Information(value),
                    ie::DLInformationPerRLR7ModeSpecificInfo2FddERGCHInfo::releaseIndicator(()) => ie::DLInformationPerRLR8ModeSpecificInfo2FddERGCHInfo::releaseIndicator(()),
                }),
            )), value.cell_id,
        ))
    }).collect::<Result<Vec<_>, _>>().map(ie::DLInformationPerRLListR8)
}

pub(super) fn edch(
    value: ie::ULEDCHInformationR7,
) -> Result<ie::ULEDCHInformationR8, DedicatedError> {
    let ie::ULEDCHInformationR7ModeSpecificInfo::fdd(fdd) = value.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    Ok(ie::ULEDCHInformationR8::new(
        value
            .mac_es_e_reset_indicator
            .map(|_| ie::ULEDCHInformationR8MacEsEResetIndicator::R_true),
        ie::ULEDCHInformationR8ModeSpecificInfo::fdd(
            ie::ULEDCHInformationR8ModeSpecificInfoFdd::new(
                fdd.e_dpcch_info,
                fdd.e_dpdch_info.map(|value| ie::EDPDCHInfoR8 {
                    e_tfci_table_index: value.e_tfci_table_index,
                    e_dch_minimum_set_e_tfci: value.e_dch_minimum_set_e_tfci,
                    reference_e_tfcis: value.reference_e_tfcis,
                    min_reduced_e_dpdch_gain_factor: None,
                    max_channelisation_codes: value.max_channelisation_codes,
                    pl_non_max: value.pl_non_max,
                    scheduling_info_configuration: value.scheduling_info_configuration,
                    three_index_step_threshold: value.three_index_step_threshold,
                    two_index_step_threshold: value.two_index_step_threshold,
                }),
                fdd.scheduling_transm_configuration,
                fdd.ul_16_qam_settings,
            ),
        ),
    ))
}

fn uplink_edch(
    value: ie::ULAddReconfTransChInformationR7EDch,
) -> Result<ie::ULAddReconfTransChInformationR8EDch, DedicatedError> {
    let ie::ULAddReconfTransChInformationR7EDchModeSpecific::fdd(fdd) = value.mode_specific else {
        return Err(DedicatedError::Physical);
    };
    Ok(ie::ULAddReconfTransChInformationR8EDch::new(
        None,
        ie::ULAddReconfTransChInformationR8EDchModeSpecific::fdd(
            ie::ULAddReconfTransChInformationR8EDchModeSpecificFdd::new(fdd.tti),
        ),
        match value.harq_info {
            ie::ULAddReconfTransChInformationR7EDchHarqInfo::rv0 => {
                ie::ULAddReconfTransChInformationR8EDchHarqInfo::rv0
            }
            ie::ULAddReconfTransChInformationR7EDchHarqInfo::rvtable => {
                ie::ULAddReconfTransChInformationR8EDchHarqInfo::rvtable
            }
        },
        value.add_reconf_mac_d_flow_list,
    ))
}

fn e_agch(value: ie::EAGCHInformationR7) -> Result<ie::EAGCHInformationR8, DedicatedError> {
    let ie::EAGCHInformationR7ModeSpecific::fdd(fdd) = value.mode_specific else {
        return Err(DedicatedError::Physical);
    };
    Ok(ie::EAGCHInformationR8::new(
        ie::EAGCHInformationR8ModeSpecific::fdd(ie::EAGCHInformationR8ModeSpecificFdd::new(
            fdd.e_agch_channelisation_code,
        )),
    ))
}
