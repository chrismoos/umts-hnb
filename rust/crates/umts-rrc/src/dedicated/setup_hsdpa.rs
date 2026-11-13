// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::hsdpa::InitialConfiguration;

const H_RNTI_BITS: usize = u16::BITS as usize;

pub(super) fn body_r7(
    setup: &InitialDchSetup,
    transaction: TransactionId,
    hs: &InitialConfiguration,
) -> Result<pdu::RadioBearerSetupR7IEs, DedicatedError> {
    if !Arc::ptr_eq(setup.configuration(), hs.dedicated())
        || !hs.uses_mac_ehs()
        || setup.rabs.iter().any(|rab| rab.domain != CnDomain::Ps)
    {
        return Err(DedicatedError::Mapping);
    }
    let base = super::body_r3(
        &setup.configuration,
        &setup.retained,
        &setup.rabs,
        transaction,
        None,
        None,
    )?;
    body_r7_from_base(setup.configuration(), base, hs, true)
}

pub(super) fn body_r7_from_base(
    configuration: &Rel99Configuration,
    base: pdu::RadioBearerSetupR3IEs,
    hs: &InitialConfiguration,
    initial: bool,
) -> Result<pdu::RadioBearerSetupR7IEs, DedicatedError> {
    if !initial {
        hs.validate_retained_flows(configuration)?;
    }
    let uplink = configuration
        .uplink()
        .transports()
        .iter()
        .map(|transport| {
            Ok(ie::ULAddReconfTransChInformationR7::dch_usch(
                ie::ULAddReconfTransChInformationR7DchUsch::new(
                    ie::ULTrCHType::dch,
                    ie::TransportChannelIdentity(transport.id),
                    transport.wire_uplink()?,
                ),
            ))
        })
        .collect::<Result<Vec<_>, DedicatedError>>()?;
    let mut downlink = configuration
        .downlink()
        .transports()
        .iter()
        .map(|transport| {
            Ok(ie::DLAddReconfTransChInformationR7::new(
                ie::DLTrCHTypeId1R5::dch(ie::TransportChannelIdentity(transport.id)),
                ie::DLAddReconfTransChInformationR7TfsSignallingMode::explicit_config(
                    transport.wire_downlink()?,
                ),
                None,
            ))
        })
        .collect::<Result<Vec<_>, DedicatedError>>()?;
    if initial {
        downlink.push(ie::DLAddReconfTransChInformationR7::new(
            ie::DLTrCHTypeId1R5::hsdsch(()),
            ie::DLAddReconfTransChInformationR7TfsSignallingMode::hsdsch(
                hs.parameters()
                    .queues
                    .initial_transport_info_r7(&hs.parameters().harq),
            ),
            None,
        ));
    }
    let rabs = base
        .rab_information_setup_list
        .ok_or(DedicatedError::Bearer)?
        .0
        .into_iter()
        .map(|rab| {
            let bearers = rab
                .rb_information_setup_list
                .0
                .into_iter()
                .map(|bearer| {
                    let id = bearer.rb_identity.0;
                    let mapping = hs
                        .mappings()
                        .iter()
                        .find(|mapping| mapping.bearer == bearer.rb_identity.0);
                    if initial && mapping.is_none() {
                        return Err(DedicatedError::Mapping);
                    }
                    let pdcp = bearer
                        .pdcp_info
                        .map(|pdcp| {
                            if pdcp.header_compression_info_list.is_some() {
                                return Err(DedicatedError::Mapping);
                            }
                            Ok(ie::PDCPInfoR4::new(
                                pdcp.lossless_srns_reloc_support,
                                pdcp.pdcp_pdu_header,
                                None,
                            ))
                        })
                        .transpose()?;
                    Ok(ie::RBInformationSetupR7::new(
                        bearer.rb_identity,
                        pdcp,
                        rlc(configuration, id, bearer.rlc_info_choice)?,
                        if initial {
                            hs.mapping_r7(mapping.ok_or(DedicatedError::Mapping)?)?
                        } else {
                            configuration
                                .bearer(id)
                                .ok_or(DedicatedError::Bearer)?
                                .mapping
                                .wire_r7()
                        },
                    ))
                })
                .collect::<Result<Vec<_>, DedicatedError>>()?;
            let info = rab.rab_info;
            Ok(ie::RABInformationSetupR7::new(
                ie::RABInfoR7::new(
                    info.rab_identity,
                    None,
                    None,
                    info.cn_domain_identity,
                    info.nas_synchronisation_indicator,
                    info.re_establishment_timer,
                ),
                ie::RBInformationSetupListR7(bearers),
            ))
        })
        .collect::<Result<Vec<_>, DedicatedError>>()?;
    let affected = base
        .rb_information_affected_list
        .map(|list| {
            list.0
                .into_iter()
                .map(|old| {
                    let bearer = configuration
                        .bearer(old.rb_identity.0)
                        .ok_or(DedicatedError::Bearer)?;
                    Ok(ie::RBInformationAffectedR7::new(
                        old.rb_identity,
                        retained_mapping(configuration, hs, bearer.id)?,
                    ))
                })
                .collect::<Result<Vec<_>, DedicatedError>>()
                .map(ie::RBInformationAffectedListR7)
        })
        .transpose()?;
    let deleted_ul = base.ul_deleted_trans_ch_info_list.map(|list| {
        ie::ULDeletedTransChInfoListR6(
            list.0
                .into_iter()
                .map(|old| {
                    ie::ULTransportChannelIdentityR6::dch_usch(
                        ie::ULTransportChannelIdentityR6DchUsch::new(
                            old.ul_transport_channel_type,
                            old.ul_transport_channel_identity,
                        ),
                    )
                })
                .collect(),
        )
    });
    let mut deleted_dl = base.dl_deleted_trans_ch_info_list.map(|list| {
        ie::DLDeletedTransChInfoListR7(
            list.0
                .into_iter()
                .map(|old| {
                    ie::DLTransportChannelIdentityR7::new(ie::DLTrCHTypeId2R7::dch(
                        old.dl_transport_channel_identity,
                    ))
                })
                .collect(),
        )
    });
    if !initial {
        // TS 25.331 8.6.5.8: deleting the last MAC-ehs queue also updates
        // HS_DSCH_RECEPTION, while retained queue identifiers remain unchanged.
        for queue in hs.parameters().queues.queues() {
            let retained = hs.mappings().iter().any(|mapping| {
                mapping.flow == queue.flow() && configuration.bearer(mapping.bearer).is_some()
            });
            if !retained {
                deleted_dl
                    .get_or_insert_with(|| ie::DLDeletedTransChInfoListR7(Vec::new()))
                    .0
                    .push(ie::DLTransportChannelIdentityR7::new(
                        ie::DLTrCHTypeId2R7::hsdsch(ie::DLTrCHTypeId2R7Hsdsch::mac_ehs(
                            ie::MACEhsQueueId(queue.id()),
                        )),
                    ));
            }
        }
    }
    let specification = pdu::RadioBearerSetupR7IEsSpecificationModeComplete {
        srb_information_setup_list: None,
        rab_information_setup_list: (!rabs.is_empty())
            .then_some(ie::RABInformationSetupListR7(rabs)),
        rab_information_reconfig_list: None,
        rb_information_reconfig_list: None,
        rb_information_affected_list: affected,
        dl_counter_synchronisation_info: None,
        pdcp_rohc_target_mode: None,
        ul_common_trans_ch_info: Some(ie::ULCommonTransChInfoR4::new(
            None,
            None,
            Some(ie::ULCommonTransChInfoR4ModeSpecificInfo::fdd(
                ie::ULCommonTransChInfoR4ModeSpecificInfoFdd::new(configuration.uplink().wire()?),
            )),
            None,
        )),
        ul_deleted_trans_ch_info_list: deleted_ul,
        ul_add_reconf_trans_ch_info_list: Some(ie::ULAddReconfTransChInfoListR7(uplink)),
        dl_common_trans_ch_info: Some(ie::DLCommonTransChInfoR4::new(
            None,
            Some(ie::DLCommonTransChInfoR4ModeSpecificInfo::fdd(
                ie::DLCommonTransChInfoR4ModeSpecificInfoFdd::new(Some(
                    ie::DLCommonTransChInfoR4ModeSpecificInfoFddDlParameters::dl_DCH_TFCS(
                        ie::DLCommonTransChInfoR4ModeSpecificInfoFddDlParametersDlDCHTFCS::new(
                            Some(configuration.downlink().wire()?),
                        ),
                    ),
                )),
            )),
        )),
        dl_deleted_trans_ch_info_list: deleted_dl,
        dl_add_reconf_trans_ch_info_list: Some(ie::DLAddReconfTransChInfoListR7(downlink)),
    };
    let (common, links) = match (base.dl_common_information, base.dl_information_per_rl_list) {
        (Some(common), Some(links)) => {
            let (common, links) = physical(common, links, initial)?;
            (Some(common), Some(links))
        }
        (None, None) => (None, None),
        _ => return Err(DedicatedError::Physical),
    };
    let mut hrnti = FixedBitString::<H_RNTI_BITS>::default();
    for index in 0..H_RNTI_BITS {
        hrnti.set(
            index,
            hs.parameters().h_rnti & (1 << (H_RNTI_BITS - 1 - index)) != 0,
        );
    }
    let body = pdu::RadioBearerSetupR7IEs {
        integrity_protection_mode_info: None,
        ciphering_mode_info: None,
        // TS 25.331 8.6.3.1: initial FDD CELL_FACH-to-DCH setup uses Now.
        activation_time: base.activation_time,
        new_u_rnti: None,
        new_c_rnti: None,
        new_dsch_rnti: None,
        new_h_rnti: initial.then_some(ie::HRNTI(hrnti)),
        new_primary_e_rnti: None,
        new_secondary_e_rnti: None,
        rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
        utran_drx_cycle_length_coeff: None,
        ura_identity: None,
        support_for_change_of_ue_capability: None,
        cn_information_info: None,
        specification_mode: pdu::RadioBearerSetupR7IEsSpecificationMode::complete(specification),
        frequency_info: None,
        multi_frequency_info: None,
        dtx_drx_timing_info: None,
        dtx_drx_info: None,
        hs_scch_less_info: None,
        mimo_parameters: None,
        max_allowed_ul_tx_power: base.max_allowed_ul_tx_power,
        ul_dpch_info: if initial || base.ul_channel_requirement.is_some() {
            Some(hs.uplink_r7_for(configuration)?)
        } else {
            None
        },
        ul_edch_information: None,
        dl_hspdsch_information: initial.then_some(hs.parameters().control.downlink_evolved(
            hs.parameters().measurement,
            hs.parameters().harq.category().limits().qam64,
        )),
        dl_common_information: common,
        dl_information_per_rl_list: links,
        mbms_pl_service_restrict_info: None,
    };
    Ok(body)
}

pub(super) fn message(
    setup: &InitialDchSetup,
    transaction: TransactionId,
    hs: &InitialConfiguration,
) -> Result<EncodedRrc, DedicatedError> {
    let body = body_r7(setup, transaction, hs)?;
    let value = class::DLDCCHMessage::new(None,
        class::DLDCCHMessageType::radioBearerSetup(pdu::RadioBearerSetup::later_than_r3(
            pdu::RadioBearerSetupLaterThanR3::new(ie::RRCTransactionIdentifier(transaction.get()),
                pdu::RadioBearerSetupLaterThanR3CriticalExtensions::criticalExtensions(
                    pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
                        pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                            pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r7(
                                pdu::RadioBearerSetupLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsR7::new(body, None, None),
                            ),
                        ),
                    ),
                ),
            ),
        )),
    );
    encode_downlink_dedicated(&value).map_err(DedicatedError::Codec)
}

fn rlc(
    configuration: &Rel99Configuration,
    id: u8,
    original: ie::RLCInfoChoice,
) -> Result<ie::RLCInfoChoiceR7, DedicatedError> {
    let ie::RLCInfoChoice::rlc_Info(info) = original else {
        return Err(DedicatedError::Bearer);
    };
    let downlink = match info.dl_rlc_mode {
        Some(ie::DLRLCMode::dl_AM_RLC_Mode(value)) => value,
        other => {
            let mode = other.map(|mode| match mode {
                ie::DLRLCMode::dl_TM_RLC_Mode(value) => ie::DLRLCModeR7::dl_TM_RLC_Mode(value),
                ie::DLRLCMode::dl_UM_RLC_Mode(()) => ie::DLRLCModeR7::dl_UM_RLC_Mode(
                    ie::DLUMRLCModeR6::new(ie::DLUMRLCLISize::size7, None),
                ),
                ie::DLRLCMode::dl_AM_RLC_Mode(_) => unreachable!(),
            });
            return Ok(ie::RLCInfoChoiceR7::rlc_Info(ie::RLCInfoR7::new(
                info.ul_rlc_mode,
                mode,
                false,
                None,
                None,
            )));
        }
    };
    let bearer = configuration.bearer(id).ok_or(DedicatedError::Bearer)?;
    let mut sizes = bearer
        .mapping
        .downlink
        .iter()
        .map(|mapping| {
            configuration
                .downlink()
                .transport(mapping.transport)
                .ok_or(DedicatedError::Transport)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|transport| transport.formats())
        .map(|format| format.rlc_bits)
        .filter(|bits| *bits != 0);
    let bits = sizes.next().ok_or(DedicatedError::Format)?;
    if sizes.any(|other| other != bits) {
        return Err(DedicatedError::Format);
    }
    let size = crate::transport::octet_rlc_size(bits).map_err(|_| DedicatedError::Format)?;
    Ok(ie::RLCInfoChoiceR7::rlc_Info(ie::RLCInfoR7::new(
        info.ul_rlc_mode,
        Some(ie::DLRLCModeR7::dl_AM_RLC_Mode(ie::DLAMRLCModeR7::new(
            ie::DLAMRLCModeR7DlRLCPDUSize::fixedSize(size),
            downlink.in_sequence_delivery,
            downlink.receiving_window_size,
            downlink.dl_rlc_status_info,
        ))),
        false,
        None,
        None,
    )))
}

fn physical(
    common: ie::DLCommonInformation,
    links: ie::DLInformationPerRLList,
    initial: bool,
) -> Result<(ie::DLCommonInformationR7, ie::DLInformationPerRLListR7), DedicatedError> {
    let dpch = common.dl_dpch_info_common.ok_or(DedicatedError::Physical)?;
    let ie::DLDPCHInfoCommonModeSpecificInfo::fdd(fdd) = dpch.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    let ie::DLCommonInformationModeSpecificInfo::fdd(timing) = common.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    let common = ie::DLCommonInformationR7::new(
        Some(
            ie::DLCommonInformationR7DlDpchInfoCommon::dl_DPCH_InfoCommon(
                ie::DLDPCHInfoCommonR6::new(
                    if initial {
                        ie::DLDPCHInfoCommonR6CfnHandling::initialise(())
                    } else {
                        ie::DLDPCHInfoCommonR6CfnHandling::maintain(
                            ie::DLDPCHInfoCommonR6CfnHandlingMaintain::new(None),
                        )
                    },
                    ie::DLDPCHInfoCommonR6ModeSpecificInfo::fdd(
                        ie::DLDPCHInfoCommonR6ModeSpecificInfoFdd::new(
                            fdd.dl_dpch_power_control_info,
                            fdd.power_offset_pilot_pdpdch,
                            fdd.dl_rate_matching_restriction,
                            fdd.spreading_factor_and_pilot,
                            fdd.position_fixed_or_flexible,
                            fdd.tfci_existence,
                        ),
                    ),
                    None,
                ),
            ),
        ),
        ie::DLCommonInformationR7ModeSpecificInfo::fdd(
            ie::DLCommonInformationR7ModeSpecificInfoFdd::new(
                timing.default_dpch_offset_value,
                timing.dpch_compressed_mode_info,
                timing.tx_diversity_mode,
            ),
        ),
        initial.then_some(ie::DLCommonInformationR7MacHsResetIndicator::R_true),
        None,
    );
    let links = links
        .0
        .into_iter()
        .map(|link| {
            let ie::DLInformationPerRLModeSpecificInfo::fdd(fdd) = link.mode_specific_info else {
                return Err(DedicatedError::Physical);
            };
            let Some(ie::DLDPCHInfoPerRL::fdd(dpch)) = link.dl_dpch_info_per_rl else {
                return Err(DedicatedError::Physical);
            };
            if dpch.dummy.is_some() {
                return Err(DedicatedError::Physical);
            }
            Ok(ie::DLInformationPerRLR7::new(
                ie::DLInformationPerRLR7ModeSpecificInfo::fdd(
                    ie::DLInformationPerRLR7ModeSpecificInfoFdd::new(
                        fdd.primary_cpich_info,
                        true,
                        false,
                    ),
                ),
                Some(ie::DLInformationPerRLR7DlDpchInfo::dl_DPCH_InfoPerRL(
                    ie::DLDPCHInfoPerRLR7::fdd(ie::DLDPCHInfoPerRLR7Fdd::new(
                        dpch.p_cpich_usage_for_channel_est,
                        dpch.dpch_frame_offset,
                        dpch.secondary_cpich_info,
                        dpch.dl_channelisation_code_list,
                        dpch.tpc_combination_index,
                        None,
                        dpch.closed_loop_timing_adj_mode,
                    )),
                )),
                None,
                ie::DLInformationPerRLR7ModeSpecificInfo2::fdd(
                    ie::DLInformationPerRLR7ModeSpecificInfo2Fdd::new(None, None),
                ),
                None,
            ))
        })
        .collect::<Result<Vec<_>, DedicatedError>>()?;
    Ok((common, ie::DLInformationPerRLListR7(links)))
}

// A replacement mapping replaces both directions. Preserve the committed MAC-ehs
// downlink while taking the successor DCH uplink mapping from the new plan.
pub(super) fn retained_mapping(
    configuration: &Rel99Configuration,
    hs: &InitialConfiguration,
    id: u8,
) -> Result<ie::RBMappingInfoR7, DedicatedError> {
    let mut mapping = configuration
        .bearer(id)
        .ok_or(DedicatedError::Bearer)?
        .mapping
        .wire_r7();
    if let Some(retained) = hs.mappings().iter().find(|mapping| mapping.bearer == id) {
        let mut previous = hs.mapping_r7(retained)?;
        mapping.0[0].dl_logical_channel_mapping_list =
            previous.0[0].dl_logical_channel_mapping_list.take();
    }
    Ok(mapping)
}
