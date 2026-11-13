// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(super) fn edch(original: ie::ULEDCHInformationR6) -> ie::ULEDCHInformationR7 {
    ie::ULEDCHInformationR7::new(
        original
            .mac_es_e_reset_indicator
            .map(|_| ie::ULEDCHInformationR7MacEsEResetIndicator::R_true),
        ie::ULEDCHInformationR7ModeSpecificInfo::fdd(
            ie::ULEDCHInformationR7ModeSpecificInfoFdd::new(
                original.e_dpcch_info.map(|info| {
                    ie::EDPCCHInfoR7::new(
                        info.e_dpcch_dpcch_power_offset,
                        info.happy_bit_delay_condition,
                        None,
                        None,
                    )
                }),
                original.e_dpdch_info.map(|info| {
                    ie::EDPDCHInfoR7::new(
                        info.e_tfci_table_index,
                        info.e_dch_minimum_set_e_tfci,
                        ie::EDPDCHReferenceETFCIListR7(
                            info.reference_e_tfcis
                                .0
                                .into_iter()
                                .map(|reference| {
                                    ie::EDPDCHReferenceETFCIR7::new(
                                        reference.reference_e_tfci,
                                        reference.reference_e_tfci_po,
                                    )
                                })
                                .collect(),
                        ),
                        info.max_channelisation_codes,
                        info.pl_non_max,
                        info.scheduling_info_configuration,
                        info.three_index_step_threshold,
                        info.two_index_step_threshold,
                    )
                }),
                original.scheduling_transm_configuration,
                None,
            ),
        ),
    )
}

pub(super) fn uplink(original: ie::ULDPCHInfoR6) -> Result<ie::ULDPCHInfoR7, DedicatedError> {
    let ie::ULDPCHInfoR6ModeSpecificInfo::fdd(fdd) = original.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    let presence = match fdd.dpdch_presence {
        ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresence::present(info) => {
            ie::ULDPCHInfoR7ModeSpecificInfoFddDpdchPresence::present(
                ie::ULDPCHInfoR7ModeSpecificInfoFddDpdchPresencePresent::new(
                    info.number_of_dpdch,
                    info.spreading_factor,
                    info.tfci_existence,
                    info.number_of_fbi_bits,
                    None,
                    info.puncturing_limit,
                ),
            )
        }
        ie::ULDPCHInfoR6ModeSpecificInfoFddDpdchPresence::notPresent(info) => {
            ie::ULDPCHInfoR7ModeSpecificInfoFddDpdchPresence::notPresent(
                ie::ULDPCHInfoR7ModeSpecificInfoFddDpdchPresenceNotPresent::new(
                    info.tfci_existence,
                    info.number_of_fbi_bits,
                    None,
                ),
            )
        }
    };
    let power = original
        .ul_dpch_power_control_info
        .map(|power| {
            let ie::ULDPCHPowerControlInfoR6::fdd(power) = power else {
                return Err(DedicatedError::Physical);
            };
            Ok(ie::ULDPCHPowerControlInfoR7::fdd(
                ie::ULDPCHPowerControlInfoR7Fdd::new(
                    power.dpcch_power_offset,
                    power.pc_preamble,
                    power.s_rb_delay,
                    power.power_control_algorithm,
                    power.delta_ack,
                    power.delta_nack,
                    power.ack_nack_repetition_factor,
                    power.harq_preamble_mode,
                ),
            ))
        })
        .transpose()?;
    Ok(ie::ULDPCHInfoR7::new(
        power,
        ie::ULDPCHInfoR7ModeSpecificInfo::fdd(ie::ULDPCHInfoR7ModeSpecificInfoFdd::new(
            fdd.scrambling_code_type,
            fdd.scrambling_code,
            presence,
        )),
    ))
}

pub(super) fn link(
    original: ie::DLInformationPerRLR6,
) -> Result<ie::DLInformationPerRLR7, DedicatedError> {
    let ie::DLInformationPerRLR6ModeSpecificInfo::fdd(fdd) = original.mode_specific_info else {
        return Err(DedicatedError::Physical);
    };
    if original.dl_dpch_info.is_some() {
        return Err(DedicatedError::Physical);
    }
    Ok(ie::DLInformationPerRLR7::new(
        ie::DLInformationPerRLR7ModeSpecificInfo::fdd(
            ie::DLInformationPerRLR7ModeSpecificInfoFdd::new(
                fdd.primary_cpich_info,
                fdd.serving_hsdsch_rl_indicator,
                fdd.serving_edch_rl_indicator,
            ),
        ),
        None,
        original.e_agch_information.map(|info| {
            ie::EAGCHInformationR7::new(ie::EAGCHInformationR7ModeSpecific::fdd(
                ie::EAGCHInformationR7ModeSpecificFdd::new(info.e_agch_channelisation_code),
            ))
        }),
        ie::DLInformationPerRLR7ModeSpecificInfo2::fdd(
            ie::DLInformationPerRLR7ModeSpecificInfo2Fdd::new(
                original.e_hich_info.map(|info| match info {
                    ie::DLInformationPerRLR6EHICHInfo::e_HICH_Information(info) => {
                        ie::DLInformationPerRLR7ModeSpecificInfo2FddEHICHInfo::e_HICH_Information(
                            info,
                        )
                    }
                    ie::DLInformationPerRLR6EHICHInfo::releaseIndicator(()) => {
                        ie::DLInformationPerRLR7ModeSpecificInfo2FddEHICHInfo::releaseIndicator(())
                    }
                }),
                original.e_rgch_info.map(|info| match info {
                    ie::DLInformationPerRLR6ERGCHInfo::e_RGCH_Information(info) => {
                        ie::DLInformationPerRLR7ModeSpecificInfo2FddERGCHInfo::e_RGCH_Information(
                            info,
                        )
                    }
                    ie::DLInformationPerRLR6ERGCHInfo::releaseIndicator(()) => {
                        ie::DLInformationPerRLR7ModeSpecificInfo2FddERGCHInfo::releaseIndicator(())
                    }
                }),
            ),
        ),
        original.cell_id,
    ))
}
