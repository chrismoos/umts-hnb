// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

impl InitialConfiguration {
    pub(crate) fn mapping_r7(
        &self,
        mapping: &MappedBearer,
    ) -> Result<ie::RBMappingInfoR7, DedicatedError> {
        let bearer = self
            .dedicated
            .bearer(mapping.bearer)
            .ok_or(DedicatedError::Bearer)?;
        let queue = self
            .parameters
            .queues
            .queues()
            .iter()
            .find(|queue| queue.flow() == mapping.flow)
            .ok_or(DedicatedError::Mapping)?;
        let mut result = bearer.mapping.wire_r7();
        result.0[0].dl_logical_channel_mapping_list =
            Some(ie::DLLogicalChannelMappingListR7(vec![
                ie::DLLogicalChannelMappingR7::new(
                    ie::DLTransportChannelTypeR7::hsdsch(
                        ie::DLTransportChannelTypeR7Hsdsch::mac_ehs(ie::MACEhsQueueId(queue.id())),
                    ),
                    mapping.logical.map(ie::LogicalChannelIdentity),
                ),
            ]));
        Ok(result)
    }

    pub(crate) fn body_r7(
        &self,
        activation: ScheduledActivation,
    ) -> Result<pdu::RadioBearerReconfigurationR7IEs, DedicatedError> {
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
                Ok(ie::RBInformationAffectedR7::new(
                    ie::RBIdentity(mapping.bearer),
                    self.mapping_r7(mapping)?,
                ))
            })
            .collect::<Result<Vec<_>, DedicatedError>>()?;
        let specification = pdu::RadioBearerReconfigurationR7IEsSpecificationModeComplete {
            rab_information_reconfig_list: None,
            rab_information_mbmsptp_list: None,
            rb_information_reconfig_list: None,
            rb_information_affected_list: Some(ie::RBInformationAffectedListR7(affected)),
            rb_pdcpcontext_relocation_list: None,
            pdcp_rohc_target_mode: None,
            ul_common_trans_ch_info: None,
            ul_deleted_trans_ch_info_list: None,
            ul_add_reconf_trans_ch_info_list: None,
            dl_common_trans_ch_info: None,
            dl_deleted_trans_ch_info_list: None,
            dl_add_reconf_trans_ch_info_list: Some(ie::DLAddReconfTransChInfoListR7(vec![
                ie::DLAddReconfTransChInformationR7::new(
                    ie::DLTrCHTypeId1R5::hsdsch(()),
                    ie::DLAddReconfTransChInformationR7TfsSignallingMode::hsdsch(
                        self.parameters
                            .queues
                            .initial_transport_info_r7(&self.parameters.harq),
                    ),
                    None,
                ),
            ])),
        };
        let body = pdu::RadioBearerReconfigurationR7IEs {
            integrity_protection_mode_info: None,
            ciphering_mode_info: None,
            activation_time: Some(ie::ActivationTime(activation.cfn())),
            delay_restriction_flag: None,
            new_u_rnti: None,
            new_c_rnti: None,
            new_dsch_rnti: None,
            new_h_rnti: Some(ie::HRNTI(h_rnti)),
            new_primary_e_rnti: None,
            new_secondary_e_rnti: None,
            rrc_state_indicator: ie::RRCStateIndicator::cell_DCH,
            ue_mobility_state_indicator: None,
            utran_drx_cycle_length_coeff: None,
            cn_information_info: None,
            ura_identity: None,
            support_for_change_of_ue_capability: None,
            response_to_change_of_ue_capability: None,
            specification_mode: pdu::RadioBearerReconfigurationR7IEsSpecificationMode::complete(
                specification,
            ),
            frequency_info: None,
            multi_frequency_info: None,
            dtx_drx_timing_info: None,
            dtx_drx_info: None,
            hs_scch_less_info: None,
            mimo_parameters: None,
            max_allowed_ul_tx_power: None,
            ul_dpch_info: Some(self.uplink_r7()?),
            ul_edch_information: None,
            dl_hspdsch_information: Some(self.parameters.control.downlink_evolved(
                self.parameters.measurement,
                self.parameters.harq.category().limits().qam64,
            )),
            // TS 25.331 8.6.6.27 requires reset when the MAC header type changes.
            dl_common_information: Some(ie::DLCommonInformationR7::new(
                None,
                ie::DLCommonInformationR7ModeSpecificInfo::fdd(
                    ie::DLCommonInformationR7ModeSpecificInfoFdd::new(None, None, None),
                ),
                Some(ie::DLCommonInformationR7MacHsResetIndicator::R_true),
                None,
            )),
            dl_information_per_rl_list: Some(ie::DLInformationPerRLListR7(vec![
                ie::DLInformationPerRLR7::new(
                    ie::DLInformationPerRLR7ModeSpecificInfo::fdd(
                        ie::DLInformationPerRLR7ModeSpecificInfoFdd::new(
                            ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(
                                self.dedicated.physical().primary_scrambling,
                            )),
                            true,
                            false,
                        ),
                    ),
                    None,
                    None,
                    ie::DLInformationPerRLR7ModeSpecificInfo2::fdd(
                        ie::DLInformationPerRLR7ModeSpecificInfo2Fdd::new(None, None),
                    ),
                    None,
                ),
            ])),
            mbms_pl_service_restrict_info: None,
        };
        Ok(body)
    }

    pub(super) fn value_r7(
        &self,
        transaction: TransactionId,
        activation: ScheduledActivation,
    ) -> Result<class::DLDCCHMessage, DedicatedError> {
        let body = self.body_r7(activation)?;
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

    pub(crate) fn uplink_r7(&self) -> Result<ie::ULDPCHInfoR7, DedicatedError> {
        self.uplink_r7_for(&self.dedicated)
    }

    pub(crate) fn uplink_r7_for(
        &self,
        dedicated: &Rel99Configuration,
    ) -> Result<ie::ULDPCHInfoR7, DedicatedError> {
        let ie::ULChannelRequirementR5::ul_DPCH_Info(original) = self.uplink_for(dedicated)? else {
            return Err(DedicatedError::Physical);
        };
        let ie::ULDPCHInfoR5ModeSpecificInfo::fdd(fdd) = original.mode_specific_info else {
            return Err(DedicatedError::Physical);
        };
        let Some(ie::ULDPCHPowerControlInfoR5::fdd(power)) = original.ul_dpch_power_control_info
        else {
            return Err(DedicatedError::Physical);
        };
        Ok(ie::ULDPCHInfoR7::new(
            Some(ie::ULDPCHPowerControlInfoR7::fdd(
                ie::ULDPCHPowerControlInfoR7Fdd::new(
                    power.dpcch_power_offset,
                    power.pc_preamble,
                    power.s_rb_delay,
                    power.power_control_algorithm,
                    power.delta_ack,
                    power.delta_nack,
                    power.ack_nack_repetition_factor,
                    ie::HARQPreambleMode(0),
                ),
            )),
            ie::ULDPCHInfoR7ModeSpecificInfo::fdd(ie::ULDPCHInfoR7ModeSpecificInfoFdd::new(
                fdd.scrambling_code_type,
                fdd.scrambling_code,
                ie::ULDPCHInfoR7ModeSpecificInfoFddDpdchPresence::present(
                    ie::ULDPCHInfoR7ModeSpecificInfoFddDpdchPresencePresent::new(
                        fdd.number_of_dpdch,
                        fdd.spreading_factor,
                        fdd.tfci_existence,
                        fdd.number_of_fbi_bits,
                        None,
                        fdd.puncturing_limit,
                    ),
                ),
            )),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsdpa::{FeedbackCycle, InitialActivation, QueueConfiguration};
    use std::num::{NonZeroU8, NonZeroU64, NonZeroUsize};
    use umts_types::{RadioEpoch, hsdpa::Category, time::ChipInstant};

    fn activation(
        category: u8,
        logical: Option<u8>,
        voice: bool,
    ) -> Result<InitialActivation, DedicatedError> {
        let dedicated = crate::test_support::dedicated::configuration(voice, true, 10);
        let mut sizes = [None; 8];
        sizes[0] = NonZeroUsize::new(320);
        InitialActivation::new(
            dedicated,
            ActivationParameters {
                h_rnti: 0x8421,
                harq: HarqConfiguration::implicit(
                    Category::new(category).unwrap(),
                    NonZeroU8::new(6).unwrap(),
                )
                .unwrap(),
                queues: QueueSet::new(vec![QueueConfiguration::new(1, 2, sizes, 100, 32).unwrap()])
                    .unwrap(),
                control: ControlChannels::new(&[OvsfCode::new(128, 12).unwrap()]).unwrap(),
                measurement: MeasurementFeedback::new(6, FeedbackCycle::Ms2, NonZeroU8::MIN, 8)
                    .unwrap(),
                acknowledgement: AckFeedback::new(8, 8, NonZeroU8::MIN).unwrap(),
            },
            vec![MappedBearer {
                bearer: 20,
                flow: 2,
                logical,
            }],
            ScheduledActivation::new(
                ChipInstant {
                    epoch: RadioEpoch::new(NonZeroU64::MIN),
                    index: 0,
                },
                0,
                NonZeroU8::new(4).unwrap(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn evolved_activation_round_trips_with_explicit_queue_mapping_and_reset() {
        for category in [13, 14] {
            for voice in [false, true] {
                let activation = activation(category, Some(3), voice).unwrap();
                let encoded = activation.message(TransactionId::new(2).unwrap()).unwrap();
                let decoded =
                    umts_rrc_codec::decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                        .unwrap();
                assert_eq!(
                    *decoded.value,
                    activation.value(TransactionId::new(2).unwrap()).unwrap()
                );
                let class::DLDCCHMessageType::radioBearerReconfiguration(
                    pdu::RadioBearerReconfiguration::later_than_r3(outer),
                ) = decoded.value.message
                else {
                    panic!("expected reconfiguration");
                };
                let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensions::criticalExtensions(extension) = outer.critical_extensions else {
                    panic!("expected extension");
                };
                let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensions::criticalExtensions(extension) = extension else {
                    panic!("expected post R5");
                };
                let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensions::criticalExtensions(extension) = extension else {
                    panic!("expected post R6");
                };
                let pdu::RadioBearerReconfigurationLaterThanR3CriticalExtensionsCriticalExtensionsCriticalExtensionsCriticalExtensions::r7(extension) = extension else {
                    panic!("expected R7");
                };
                let body = extension.radio_bearer_reconfiguration_r7;
                assert!(body.ciphering_mode_info.is_none());
                assert!(body.integrity_protection_mode_info.is_none());
                assert_eq!(body.activation_time.unwrap().0, 4);
                assert!(
                    body.dl_common_information
                        .unwrap()
                        .mac_hs_reset_indicator
                        .is_some()
                );
                let physical = body.dl_hspdsch_information.unwrap();
                let ie::DLHSPDSCHInformationR7ModeSpecificInfo::fdd(fdd) =
                    physical.mode_specific_info
                else {
                    panic!("expected FDD");
                };
                assert!(fdd.dl_64_qam_configured.is_some());
                let pdu::RadioBearerReconfigurationR7IEsSpecificationMode::complete(specification) =
                    body.specification_mode
                else {
                    panic!("expected complete specification");
                };
                let mapped = specification.rb_information_affected_list.unwrap().0;
                assert_eq!(mapped.len(), 1);
                assert_eq!(mapped[0].rb_identity.0, 20);
                let mapping = &mapped[0].rb_mapping_info.0[0];
                assert!(mapping.ul_logical_channel_mappings.is_some());
                let downlink = &mapping.dl_logical_channel_mapping_list.as_ref().unwrap().0[0];
                assert_eq!(downlink.logical_channel_identity.as_ref().unwrap().0, 3);
                assert!(matches!(
                    downlink.dl_transport_channel_type,
                    ie::DLTransportChannelTypeR7::hsdsch(
                        ie::DLTransportChannelTypeR7Hsdsch::mac_ehs(ie::MACEhsQueueId(1))
                    )
                ));
                let transport = specification
                    .dl_add_reconf_trans_ch_info_list
                    .unwrap()
                    .0
                    .remove(0);
                let ie::DLAddReconfTransChInformationR7TfsSignallingMode::hsdsch(info) =
                    transport.tfs_signalling_mode
                else {
                    panic!("expected HS-DSCH");
                };
                assert_eq!(
                    info.harq_info.unwrap().number_of_processes,
                    ie::HARQInfoR7NumberOfProcesses::n6
                );
                let Some(ie::HSDSCHInfoR7DlMACHeaderType::mac_ehs(queues)) =
                    info.dl_mac_header_type
                else {
                    panic!("expected MAC-ehs");
                };
                let queue = queues.mac_ehs_add_reconf_queue_list.unwrap().0.remove(0);
                assert_eq!(queue.mac_ehs_queue_id.0, 1);
                assert_eq!(queue.reordering_release_timer, ie::T1ReleaseTimer::rt100);
            }
        }
    }

    #[test]
    fn evolved_activation_requires_explicit_lcid() {
        assert!(matches!(
            activation(14, None, false),
            Err(DedicatedError::Mapping)
        ));
    }
}
