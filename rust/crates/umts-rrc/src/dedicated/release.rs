// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{DchDirection, DedicatedError, Rel99Configuration, Rel99Rlc, ScheduledActivation};
use crate::establishment::TransactionId;
use std::sync::Arc;
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};

const FIRST_USER_BEARER: u8 = 5;

pub struct ExistingDchRelease {
    retained_hsdpa: Option<Arc<crate::hsdpa::InitialConfiguration>>,
    previous: Arc<Rel99Configuration>,
    configuration: Arc<Rel99Configuration>,
    released: Vec<u8>,
    activation: ScheduledActivation,
}
impl ExistingDchRelease {
    pub fn new(
        previous: Arc<Rel99Configuration>,
        configuration: Arc<Rel99Configuration>,
        activation: ScheduledActivation,
    ) -> Result<Self, DedicatedError> {
        let released: Vec<_> = previous
            .bearers()
            .iter()
            .filter(|old| configuration.bearer(old.id).is_none())
            .map(|old| old.id)
            .collect();
        if released.is_empty()
            || released.iter().any(|id| *id < FIRST_USER_BEARER)
            || released
                .iter()
                .any(|id| previous.bearer(*id).is_none_or(|old| old.domain.is_none()))
        {
            return Err(DedicatedError::Bearer);
        }
        for target in configuration.bearers() {
            let old = previous.bearer(target.id).ok_or(DedicatedError::Bearer)?;
            if old.domain != target.domain || old.rlc != target.rlc {
                return Err(DedicatedError::Bearer);
            }
            if matches!(old.rlc, Rel99Rlc::Acknowledged { .. }) {
                for (before, after, direction) in [
                    (
                        previous.uplink(),
                        configuration.uplink(),
                        DchDirection::Uplink,
                    ),
                    (
                        previous.downlink(),
                        configuration.downlink(),
                        DchDirection::Downlink,
                    ),
                ] {
                    let bits = old
                        .mappings(direction)
                        .flat_map(|mapping| before.transport(mapping.transport))
                        .flat_map(|transport| transport.formats())
                        .next()
                        .ok_or(DedicatedError::Mapping)?
                        .rlc_bits;
                    if target
                        .mappings(direction)
                        .flat_map(|mapping| after.transport(mapping.transport))
                        .flat_map(|transport| transport.formats())
                        .any(|format| format.rlc_bits != bits)
                    {
                        return Err(DedicatedError::Format);
                    }
                }
            }
        }
        for tfcs in [
            previous.uplink(),
            previous.downlink(),
            configuration.uplink(),
            configuration.downlink(),
        ] {
            if tfcs
                .transports()
                .iter()
                .any(|transport| !usize::from(activation.cfn()).is_multiple_of(transport.frames()))
            {
                return Err(DedicatedError::Activation);
            }
        }
        Ok(Self {
            retained_hsdpa: None,
            previous,
            configuration,
            released,
            activation,
        })
    }
    pub fn previous(&self) -> &Arc<Rel99Configuration> {
        &self.previous
    }
    pub fn configuration(&self) -> &Arc<Rel99Configuration> {
        &self.configuration
    }
    pub fn released(&self) -> &[u8] {
        &self.released
    }
    pub fn activation(&self) -> ScheduledActivation {
        self.activation
    }

    pub fn with_retained_hsdpa(
        mut self,
        configuration: Option<Arc<crate::hsdpa::InitialConfiguration>>,
    ) -> Self {
        self.retained_hsdpa = configuration;
        self
    }

    pub fn retained_hsdpa(&self) -> Option<&Arc<crate::hsdpa::InitialConfiguration>> {
        self.retained_hsdpa.as_ref()
    }

    pub fn message(&self, transaction: TransactionId) -> Result<EncodedRrc, DedicatedError> {
        let configuration = &self.configuration;
        if let Some(hs) = &self.retained_hsdpa {
            hs.validate_retained_flows(configuration)?;
        }
        let uplink = configuration
            .uplink()
            .transports()
            .iter()
            .map(|transport| {
                Ok(ie::ULAddReconfTransChInformation::new(
                    ie::ULTrCHType::dch,
                    ie::TransportChannelIdentity(transport.id),
                    transport.wire_uplink()?,
                ))
            })
            .collect::<Result<_, DedicatedError>>()?;
        // TS 25.331 10.2.30 uses DL-AddReconfTransChInformation2, not the Setup list.
        let downlink = configuration
            .downlink()
            .transports()
            .iter()
            .map(|transport| {
                Ok(ie::DLAddReconfTransChInformation2::new(
                    ie::DLTrCHType::dch,
                    ie::TransportChannelIdentity(transport.id),
                    ie::DLAddReconfTransChInformation2TfsSignallingMode::explicit_config(
                        transport.wire_downlink()?,
                    ),
                    None,
                ))
            })
            .collect::<Result<_, DedicatedError>>()?;
        let deleted_ul: Vec<_> = self
            .previous
            .uplink()
            .transports()
            .iter()
            .filter(|old| configuration.uplink().transport(old.id).is_none())
            .map(|old| {
                ie::ULTransportChannelIdentity::new(
                    ie::ULTrCHType::dch,
                    ie::TransportChannelIdentity(old.id),
                )
            })
            .collect();
        let deleted_dl: Vec<_> = self
            .previous
            .downlink()
            .transports()
            .iter()
            .filter(|old| configuration.downlink().transport(old.id).is_none())
            .map(|old| {
                ie::DLTransportChannelIdentity::new(
                    ie::DLTrCHType::dch,
                    ie::TransportChannelIdentity(old.id),
                )
            })
            .collect();
        let physical_changed = self.previous.physical() != configuration.physical();
        let (common, links) = if physical_changed {
            let (common, links) = configuration.physical().downlink_wire(false)?;
            (Some(common), Some(links))
        } else {
            (None, None)
        };
        // TS 25.331 8.6.4.8 replaces all stored mappings, including a retained HS-DSCH route.
        let affected: Vec<_> = configuration
            .bearers()
            .iter()
            .filter(|bearer| {
                self.previous
                    .bearer(bearer.id)
                    .is_none_or(|previous| previous.mapping != bearer.mapping)
            })
            .map(|bearer| {
                ie::RBInformationAffected::new(ie::RBIdentity(bearer.id), bearer.mapping.wire())
            })
            .collect();
        let body = pdu::RadioBearerReleaseR3IEs {
            rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
            integrity_protection_mode_info: None,
            ciphering_mode_info: None,
            activation_time: Some(ie::ActivationTime(self.activation.cfn())),
            new_u_rnti: None,
            new_c_rnti: None,
            rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
            utran_drx_cycle_length_coeff: None,
            cn_information_info: None,
            signalling_connection_rel_indication: None,
            ura_identity: None,
            rab_information_reconfig_list: None,
            rb_information_release_list: ie::RBInformationReleaseList(
                self.released.iter().copied().map(ie::RBIdentity).collect(),
            ),
            rb_information_affected_list: (!affected.is_empty())
                .then_some(ie::RBInformationAffectedList(affected)),
            dl_counter_synchronisation_info: None,
            ul_common_trans_ch_info: Some(ie::ULCommonTransChInfo::new(
                None,
                None,
                Some(ie::ULCommonTransChInfoModeSpecificInfo::fdd(
                    ie::ULCommonTransChInfoModeSpecificInfoFdd::new(configuration.uplink().wire()?),
                )),
            )),
            ul_deleted_trans_ch_info_list: (!deleted_ul.is_empty())
                .then_some(ie::ULDeletedTransChInfoList(deleted_ul)),
            ul_add_reconf_trans_ch_info_list: Some(ie::ULAddReconfTransChInfoList(uplink)),
            dummy: None,
            dl_common_trans_ch_info: Some(ie::DLCommonTransChInfo::new(
                None,
                ie::DLCommonTransChInfoModeSpecificInfo::fdd(
                    ie::DLCommonTransChInfoModeSpecificInfoFdd::new(Some(
                        ie::DLCommonTransChInfoModeSpecificInfoFddDlParameters::dl_DCH_TFCS(
                            configuration.downlink().wire()?,
                        ),
                    )),
                ),
            )),
            dl_deleted_trans_ch_info_list: (!deleted_dl.is_empty())
                .then_some(ie::DLDeletedTransChInfoList(deleted_dl)),
            dl_add_reconf_trans_ch_info_list: Some(ie::DLAddReconfTransChInfo2List(downlink)),
            frequency_info: None,
            max_allowed_ul_tx_power: physical_changed.then_some(ie::MaxAllowedULTXPower(
                configuration.physical().maximum_uplink_dbm,
            )),
            ul_channel_requirement: if physical_changed {
                Some(configuration.physical().uplink_wire()?)
            } else {
                None
            },
            mode_specific_phys_ch_info: pdu::RadioBearerReleaseR3IEsModeSpecificPhysChInfo::fdd(
                pdu::RadioBearerReleaseR3IEsModeSpecificPhysChInfoFdd::new(None),
            ),
            dl_common_information: common,
            dl_information_per_rl_list: links,
        };
        if let Some(hs) = self
            .retained_hsdpa
            .as_ref()
            .filter(|hs| hs.secondary_cell().is_some())
        {
            let retained = hs
                .mappings()
                .iter()
                .any(|mapping| configuration.bearer(mapping.bearer).is_some());
            let update = if retained {
                crate::hsdpa::SecondaryCellUpdate::Continue
            } else {
                crate::hsdpa::SecondaryCellUpdate::Remove
            };
            let base = super::setup::retained_release_r7(
                configuration,
                &self.previous,
                self.activation,
                transaction,
                body.rb_information_affected_list,
                hs,
            )?;
            let base = crate::hsdpa::dual_cell::setup_body(base, update)?;
            let release = pdu::RadioBearerReleaseR8IEs {
                integrity_protection_mode_info: base.integrity_protection_mode_info,
                ciphering_mode_info: base.ciphering_mode_info,
                activation_time: base.activation_time,
                new_u_rnti: base.new_u_rnti,
                new_c_rnti: base.new_c_rnti,
                new_dsch_rnti: base.new_dsch_rnti,
                new_h_rnti: base.new_h_rnti,
                new_primary_e_rnti: base.new_primary_e_rnti,
                new_secondary_e_rnti: base.new_secondary_e_rnti,
                rrc_state_indicator: base.rrc_state_indicator,
                ue_mobility_state_indicator: None,
                utran_drx_cycle_length_coeff: base.utran_drx_cycle_length_coeff,
                cn_information_info: base.cn_information_info,
                signalling_connection_rel_indication: body.signalling_connection_rel_indication,
                ura_identity: base.ura_identity,
                support_for_change_of_ue_capability: base.support_for_change_of_ue_capability,
                rab_information_reconfig_list: base.rab_information_reconfig_list,
                rb_information_release_list: body.rb_information_release_list,
                rb_information_reconfig_list: base.rb_information_reconfig_list,
                rb_information_affected_list: base.rb_information_affected_list,
                dl_counter_synchronisation_info: base.dl_counter_synchronisation_info,
                ul_common_trans_ch_info: base.ul_common_trans_ch_info,
                ul_deleted_trans_ch_info_list: base.ul_deleted_trans_ch_info_list,
                ul_add_reconf_trans_ch_info_list: base.ul_add_reconf_trans_ch_info_list,
                dl_common_trans_ch_info: base.dl_common_trans_ch_info,
                dl_deleted_trans_ch_info_list: base.dl_deleted_trans_ch_info_list,
                dl_add_reconf_trans_ch_info_list: base.dl_add_reconf_trans_ch_info_list,
                frequency_info: base.frequency_info,
                multi_frequency_info: base.multi_frequency_info,
                dtx_drx_timing_info: base.dtx_drx_timing_info,
                dtx_drx_info: base.dtx_drx_info,
                hs_scch_less_info: base.hs_scch_less_info,
                mimo_parameters: base.mimo_parameters,
                max_allowed_ul_tx_power: base.max_allowed_ul_tx_power,
                ul_dpch_info: base.ul_dpch_info,
                ul_edch_information: base.ul_edch_information,
                dl_hspdsch_information: base.dl_hspdsch_information,
                dl_common_information: base.dl_common_information,
                dl_information_per_rl_list: base.dl_information_per_rl_list,
                dl_secondary_cell_info_fdd: base.dl_secondary_cell_info_fdd,
                control_channel_drxinfo_tdd128: None,
                sps_information_tdd128: None,
                mbms_pl_service_restrict_info: None,
                mbms_rb_list_released_to_change_transfer_mode: None,
            };
            let value = class::DLDCCHMessage::new(None, class::DLDCCHMessageType::radioBearerRelease(
                pdu::RadioBearerRelease::later_than_r3(pdu::RadioBearerReleaseLaterThanR3::new(
                    ie::RRCTransactionIdentifier(transaction.get()),
                    pdu::RadioBearerReleaseLaterThanR3CriticalExtensions::criticalExtensions(
                        pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(
                            pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                                pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(
                                    pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r8(
                                        pdu::RadioBearerReleaseLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensionsR8::new(release, None, None),
                                    ),
                                ),
                            ),
                        ),
                    ),
                )),
            ));
            return encode_downlink_dedicated(&value).map_err(DedicatedError::Codec);
        }
        encode_downlink_dedicated(&class::DLDCCHMessage::new(
            None,
            class::DLDCCHMessageType::radioBearerRelease(pdu::RadioBearerRelease::r3(
                pdu::RadioBearerReleaseR3::new(body, None),
            )),
        ))
        .map_err(DedicatedError::Codec)
    }
}
