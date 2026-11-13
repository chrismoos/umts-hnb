// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(crate) fn setup_body(
    base: pdu::RadioBearerSetupR7IEs,
    secondary: SecondaryCellUpdate<'_>,
) -> Result<pdu::RadioBearerSetupR8IEs, DedicatedError> {
    let pdu::RadioBearerSetupR7IEsSpecificationMode::complete(spec) = base.specification_mode
    else {
        return Err(DedicatedError::Mapping);
    };
    if base.mimo_parameters.is_some()
        || spec.srb_information_setup_list.is_some()
        || spec.rab_information_reconfig_list.is_some()
        || spec.rb_information_reconfig_list.is_some()
    {
        return Err(DedicatedError::Mapping);
    }
    let body = pdu::RadioBearerSetupR8IEs {
        integrity_protection_mode_info: base.integrity_protection_mode_info,
        ciphering_mode_info: base.ciphering_mode_info,
        sr_vcc_info: None,
        activation_time: base.activation_time,
        new_u_rnti: base.new_u_rnti,
        new_c_rnti: base.new_c_rnti,
        new_dsch_rnti: base.new_dsch_rnti,
        new_h_rnti: base.new_h_rnti,
        new_primary_e_rnti: base.new_primary_e_rnti,
        new_secondary_e_rnti: base.new_secondary_e_rnti,
        rrc_state_indicator: base.rrc_state_indicator,
        utran_drx_cycle_length_coeff: base.utran_drx_cycle_length_coeff,
        ura_identity: base.ura_identity,
        support_for_change_of_ue_capability: base.support_for_change_of_ue_capability,
        cn_information_info: base.cn_information_info,
        srb_information_setup_list: None,
        rab_information_setup_list: spec.rab_information_setup_list.map(rabs),
        rab_information_reconfig_list: None,
        rb_information_reconfig_list: None,
        rb_information_affected_list: spec.rb_information_affected_list.map(affected),
        dl_counter_synchronisation_info: spec.dl_counter_synchronisation_info,
        pdcp_rohc_target_mode: spec.pdcp_rohc_target_mode,
        ul_common_trans_ch_info: spec.ul_common_trans_ch_info,
        ul_deleted_trans_ch_info_list: spec.ul_deleted_trans_ch_info_list,
        ul_add_reconf_trans_ch_info_list: spec
            .ul_add_reconf_trans_ch_info_list
            .map(uplink)
            .transpose()?,
        dl_common_trans_ch_info: spec.dl_common_trans_ch_info,
        dl_deleted_trans_ch_info_list: spec.dl_deleted_trans_ch_info_list,
        dl_add_reconf_trans_ch_info_list: spec.dl_add_reconf_trans_ch_info_list,
        frequency_info: base.frequency_info,
        multi_frequency_info: base.multi_frequency_info,
        dtx_drx_timing_info: base.dtx_drx_timing_info,
        dtx_drx_info: base.dtx_drx_info,
        hs_scch_less_info: base.hs_scch_less_info,
        mimo_parameters: None,
        max_allowed_ul_tx_power: base.max_allowed_ul_tx_power,
        ul_dpch_info: base.ul_dpch_info,
        ul_edch_information: base.ul_edch_information.map(edch).transpose()?,
        dl_hspdsch_information: base.dl_hspdsch_information.map(hspdsch).transpose()?,
        dl_common_information: base.dl_common_information.map(common).transpose()?,
        dl_information_per_rl_list: base.dl_information_per_rl_list.map(links).transpose()?,
        dl_secondary_cell_info_fdd: secondary.wire(),
        control_channel_drxinfo_tdd128: None,
        sps_information_tdd128: None,
        mbms_pl_service_restrict_info: base.mbms_pl_service_restrict_info,
    };
    Ok(body)
}

pub(crate) fn setup(
    transaction: TransactionId,
    base: pdu::RadioBearerSetupR7IEs,
    secondary: SecondaryCellUpdate<'_>,
) -> Result<EncodedRrc, DedicatedError> {
    let body = setup_body(base, secondary)?;
    let value = class::DLDCCHMessage::new(None, class::DLDCCHMessageType::radioBearerSetup(
        pdu::RadioBearerSetup::later_than_r3(pdu::RadioBearerSetupLaterThanR3::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            pdu::RadioBearerSetupLaterThanR3CriticalExtensions::criticalExtensions(
                pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
                    pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                        pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                            pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(
                                pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsR8::new(body, None, None),
                            ),
                        ),
                    ),
                ),
            ),
        )),
    ));
    encode_downlink_dedicated(&value).map_err(DedicatedError::Codec)
}

pub(crate) fn reconfigure(
    transaction: TransactionId,
    base: pdu::RadioBearerReconfigurationR7IEs,
    secondary: SecondaryCellUpdate<'_>,
) -> Result<class::DLDCCHMessage, DedicatedError> {
    let pdu::RadioBearerReconfigurationR7IEsSpecificationMode::complete(spec) =
        base.specification_mode
    else {
        return Err(DedicatedError::Mapping);
    };
    if base.mimo_parameters.is_some()
        || spec.rab_information_reconfig_list.is_some()
        || spec.rb_information_reconfig_list.is_some()
    {
        return Err(DedicatedError::Mapping);
    }
    let specification = pdu::RadioBearerReconfigurationR8IEsSpecificationModeComplete {
        rab_information_reconfig_list: None,
        rab_information_mbmsptp_list: spec.rab_information_mbmsptp_list,
        rb_information_reconfig_list: None,
        rb_information_affected_list: spec.rb_information_affected_list.map(affected),
        rb_pdcpcontext_relocation_list: spec.rb_pdcpcontext_relocation_list,
        pdcp_rohc_target_mode: spec.pdcp_rohc_target_mode,
        ul_common_trans_ch_info: spec.ul_common_trans_ch_info,
        ul_deleted_trans_ch_info_list: spec.ul_deleted_trans_ch_info_list,
        ul_add_reconf_trans_ch_info_list: spec
            .ul_add_reconf_trans_ch_info_list
            .map(uplink)
            .transpose()?,
        dl_common_trans_ch_info: spec.dl_common_trans_ch_info,
        dl_deleted_trans_ch_info_list: spec.dl_deleted_trans_ch_info_list,
        dl_add_reconf_trans_ch_info_list: spec.dl_add_reconf_trans_ch_info_list,
    };
    let body = pdu::RadioBearerReconfigurationR8IEs {
        integrity_protection_mode_info: base.integrity_protection_mode_info,
        ciphering_mode_info: base.ciphering_mode_info,
        activation_time: base.activation_time,
        delay_restriction_flag: base.delay_restriction_flag,
        new_u_rnti: base.new_u_rnti,
        new_c_rnti: base.new_c_rnti,
        new_dsch_rnti: base.new_dsch_rnti,
        new_h_rnti: base.new_h_rnti,
        new_primary_e_rnti: base.new_primary_e_rnti,
        new_secondary_e_rnti: base.new_secondary_e_rnti,
        rrc_state_indicator: base.rrc_state_indicator,
        ue_mobility_state_indicator: base.ue_mobility_state_indicator,
        utran_drx_cycle_length_coeff: base.utran_drx_cycle_length_coeff,
        cn_information_info: base.cn_information_info,
        ura_identity: base.ura_identity,
        support_for_change_of_ue_capability: base.support_for_change_of_ue_capability,
        response_to_change_of_ue_capability: base
            .response_to_change_of_ue_capability
            .map(|_| pdu::RadioBearerReconfigurationR8IEsResponseToChangeOfUECapability::R_true),
        dummy: None,
        specification_mode: pdu::RadioBearerReconfigurationR8IEsSpecificationMode::complete(
            specification,
        ),
        frequency_info: base.frequency_info,
        multi_frequency_info: base.multi_frequency_info,
        dtx_drx_timing_info: base.dtx_drx_timing_info,
        dtx_drx_info: base.dtx_drx_info,
        hs_scch_less_info: base.hs_scch_less_info,
        mimo_parameters: None,
        max_allowed_ul_tx_power: base.max_allowed_ul_tx_power,
        ul_dpch_info: base.ul_dpch_info,
        ul_edch_information: base.ul_edch_information.map(edch).transpose()?,
        dl_hspdsch_information: base.dl_hspdsch_information.map(hspdsch).transpose()?,
        dl_common_information: base.dl_common_information.map(common).transpose()?,
        dl_information_per_rl_list: base.dl_information_per_rl_list.map(links).transpose()?,
        dl_secondary_cell_info_fdd: secondary.wire(),
        control_channel_drxinfo_tdd128: None,
        sps_information_tdd128: None,
        mbms_pl_service_restrict_info: base.mbms_pl_service_restrict_info,
    };
    let value = class::DLDCCHMessage::new(None, class::DLDCCHMessageType::radioBearerReconfiguration(
        pdu::RadioBearerReconfiguration::later_than_r3(pdu::RadioBearerReconfigurationLaterThanR3::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(
                pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
                    pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                        pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                            pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(
                                pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsR8::new(body, None, None),
                            ),
                        ),
                    ),
                ),
            ),
        )),
    ));
    Ok(value)
}

pub(super) fn physical(
    transaction: TransactionId,
    activation: ScheduledActivation,
    secondary: SecondaryCellUpdate<'_>,
) -> Result<EncodedRrc, DedicatedError> {
    let body = pdu::PhysicalChannelReconfigurationR8IEs {
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        activation_time: Some(ie::ActivationTime(activation.cfn())),
        delay_restriction_flag: None,
        new_u_rnti: None,
        new_c_rnti: None,
        new_dsch_rnti: None,
        new_h_rnti: None,
        new_primary_e_rnti: None,
        new_secondary_e_rnti: None,
        rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
        ue_mobility_state_indicator: None,
        utran_drx_cycle_length_coeff: None,
        cn_information_info: None,
        ura_identity: None,
        support_for_change_of_ue_capability: None,
        response_to_change_of_ue_capability: None,
        dl_counter_synchronisation_info: None,
        harq_info: None,
        frequency_info: None,
        multi_frequency_info: None,
        dtx_drx_timing_info: None,
        dtx_drx_info: None,
        hs_scch_less_info: None,
        mimo_parameters: None,
        max_allowed_ul_tx_power: None,
        ul_dpch_info: None,
        ul_edch_information: None,
        dl_hspdsch_information: None,
        dl_common_information: None,
        dl_information_per_rl_list: None,
        dl_secondary_cell_info_fdd: secondary.wire(),
        control_channel_drxinfo_tdd128: None,
        sps_information_tdd128: None,
        mbms_pl_service_restrict_info: None,
    };
    let value = class::DLDCCHMessage::new(None, class::DLDCCHMessageType::physicalChannelReconfiguration(
        pdu::PhysicalChannelReconfiguration::later_than_r3(pdu::PhysicalChannelReconfigurationLaterThanR3::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(
                pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
                    pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                        pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                            pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(
                                pdu::PhysicalChannelReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsR8::new(body, None, None),
                            ),
                        ),
                    ),
                ),
            ),
        )),
    ));
    encode_downlink_dedicated(&value).map_err(DedicatedError::Codec)
}
