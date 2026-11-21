// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{FIRST_USER_BEARER, InitialDchSetup, TransportContent};
use crate::{dedicated::DedicatedError, establishment::TransactionId};
use umts_rrc_codec::{EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions as class, information_elements as ie, pdu_definitions as pdu,
};
use umts_types::CnDomain;

impl InitialDchSetup {
    /// RADIO BEARER RECONFIGURATION that moves a UE holding these bearers from CELL_FACH to
    /// CELL_DCH. The bearers keep their RLC parameters and only their mapping is listed.
    pub fn resume_reconfiguration(
        &self,
        transaction: TransactionId,
    ) -> Result<EncodedRrc, DedicatedError> {
        let configuration = &self.configuration;
        if configuration
            .bearers()
            .iter()
            .any(|bearer| bearer.id >= FIRST_USER_BEARER && bearer.domain != Some(CnDomain::Ps))
        {
            return Err(DedicatedError::Bearer);
        }
        let transport = TransportContent::new(configuration)?;
        let (common, links) = configuration.physical().downlink_wire(true)?;
        let (user, signalling): (Vec<_>, Vec<_>) = configuration
            .bearers()
            .iter()
            .partition(|bearer| bearer.id >= FIRST_USER_BEARER);
        if user.is_empty() {
            return Err(DedicatedError::Bearer);
        }
        let downlink_channels = transport
            .downlink_channels
            .0
            .into_iter()
            .map(|channel| {
                ie::DLAddReconfTransChInformation2::new(
                    channel.dl_transport_channel_type,
                    channel.dl_transport_channel_identity,
                    match channel.tfs_signalling_mode {
                        ie::DLAddReconfTransChInformationTfsSignallingMode::explicit_config(
                            formats,
                        ) => ie::DLAddReconfTransChInformation2TfsSignallingMode::explicit_config(
                            formats,
                        ),
                        ie::DLAddReconfTransChInformationTfsSignallingMode::sameAsULTrCH(
                            uplink,
                        ) => ie::DLAddReconfTransChInformation2TfsSignallingMode::sameAsULTrCH(
                            uplink,
                        ),
                    },
                    channel.dch_quality_target,
                )
            })
            .collect();
        // TS 25.331 8.6.3.1 requires activation time "now" for a UE outside CELL_DCH, and
        // 8.2.2.3 leaves the UE behaviour unspecified without the Default DPCH Offset Value.
        let ies = pdu::RadioBearerReconfigurationR3IEs {
            rrc_transaction_identifier: ie::RRCTransactionIdentifier(transaction.get()),
            integrity_protection_mode_info: None,
            ciphering_mode_info: None,
            activation_time: None,
            new_u_rnti: None,
            new_c_rnti: None,
            rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
            utran_drx_cycle_length_coeff: None,
            cn_information_info: None,
            ura_identity: None,
            rab_information_reconfig_list: None,
            rb_information_reconfig_list: ie::RBInformationReconfigList(
                user.into_iter()
                    .map(|bearer| {
                        ie::RBInformationReconfig::new(
                            ie::RBIdentity(bearer.id),
                            None,
                            None,
                            None,
                            Some(bearer.mapping_info()),
                            None,
                        )
                    })
                    .collect(),
            ),
            rb_information_affected_list: Some(ie::RBInformationAffectedList(
                signalling
                    .into_iter()
                    .map(|bearer| {
                        ie::RBInformationAffected::new(
                            ie::RBIdentity(bearer.id),
                            bearer.mapping_info(),
                        )
                    })
                    .collect(),
            )),
            ul_common_trans_ch_info: Some(transport.uplink_common),
            ul_deleted_trans_ch_info_list: None,
            ul_add_reconf_trans_ch_info_list: Some(transport.uplink_channels),
            dummy: None,
            dl_common_trans_ch_info: Some(transport.downlink_common),
            dl_deleted_trans_ch_info_list: None,
            dl_add_reconf_trans_ch_info_list: Some(ie::DLAddReconfTransChInfo2List(
                downlink_channels,
            )),
            frequency_info: None,
            max_allowed_ul_tx_power: Some(ie::MaxAllowedULTXPower(
                configuration.physical().maximum_uplink_dbm,
            )),
            ul_channel_requirement: Some(configuration.physical().uplink_wire()?),
            mode_specific_phys_ch_info:
                pdu::RadioBearerReconfigurationR3IEsModeSpecificPhysChInfo::fdd(
                    pdu::RadioBearerReconfigurationR3IEsModeSpecificPhysChInfoFdd::new(None),
                ),
            dl_common_information: Some(common),
            dl_information_per_rl_list: links,
        };
        encode_downlink_dedicated(&class::DLDCCHMessage::new(
            None,
            class::DLDCCHMessageType::radioBearerReconfiguration(
                pdu::RadioBearerReconfiguration::r3(pdu::RadioBearerReconfigurationR3::new(
                    ies, None,
                )),
            ),
        ))
        .map_err(DedicatedError::Codec)
    }
}
