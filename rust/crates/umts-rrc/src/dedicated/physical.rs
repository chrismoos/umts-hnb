// SPDX-License-Identifier: AGPL-3.0-or-later

use super::DedicatedError;
use umts_rrc_schema::information_elements as ie;

const CHIPS_PER_FRAME: u32 = 38_400;
const FRAME_OFFSET_QUANTUM: u32 = 256;
const DEFAULT_OFFSET_QUANTUM: u32 = 512;
const MAX_DEFAULT_OFFSET: u16 = 599;
const MAX_FRAME_OFFSET: u8 = 149;
const MAX_PRIMARY_CODE: u16 = 511;
const MAX_SECONDARY_CODE: u8 = 15;
const MAX_UL_CODE: u32 = (1 << 24) - 1;
const MAX_CODES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UplinkPowerControl {
    Algorithm1 { step_db: u8 },
    Algorithm2,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalDpch {
    pub primary_scrambling: u16,
    pub secondary_scrambling: Option<u8>,
    pub uplink_scrambling: u32,
    pub uplink_minimum_sf: u16,
    pub uplink_dpdch_count: u8,
    pub uplink_tfci: bool,
    pub puncturing_percent: u8,
    pub dpcch_offset_db: i16,
    pub pc_preamble_frames: u8,
    pub srb_delay_frames: u8,
    pub power_control: UplinkPowerControl,
    pub maximum_uplink_dbm: i8,
    pub downlink_sf: u16,
    pub downlink_pilot_bits: u8,
    pub downlink_tfci: bool,
    pub downlink_codes: Vec<u16>,
    pub flexible_position: bool,
    pub pilot_offset_quarter_db: u8,
    pub default_offset_512_chips: u16,
    pub frame_offset_256_chips: u8,
}
impl NormalDpch {
    pub fn validate(&self) -> Result<(), DedicatedError> {
        if self.primary_scrambling > MAX_PRIMARY_CODE
            || self
                .secondary_scrambling
                .is_some_and(|code| !(1..=MAX_SECONDARY_CODE).contains(&code))
            || self.uplink_scrambling > MAX_UL_CODE
            || ![4, 8, 16, 32, 64, 128, 256].contains(&self.uplink_minimum_sf)
            || !(1..=6).contains(&self.uplink_dpdch_count)
            || self.uplink_dpdch_count > 1 && self.uplink_minimum_sf != 4
            || !(40..=100).contains(&self.puncturing_percent)
            || !(self.puncturing_percent - 40).is_multiple_of(4)
            || !(-164..=-6).contains(&self.dpcch_offset_db)
            || self.dpcch_offset_db % 2 != 0
            || self.pc_preamble_frames > 7
            || self.srb_delay_frames > 7
            || !(-50..=33).contains(&self.maximum_uplink_dbm)
            || self.pilot_offset_quarter_db > 24
            || self.default_offset_512_chips > MAX_DEFAULT_OFFSET
            || self.frame_offset_256_chips > MAX_FRAME_OFFSET
            || self.downlink_codes.is_empty()
            || self.downlink_codes.len() > MAX_CODES
            || self
                .downlink_codes
                .iter()
                .any(|code| *code >= self.downlink_sf)
            || self
                .downlink_codes
                .iter()
                .enumerate()
                .any(|(index, code)| self.downlink_codes[..index].contains(code))
        {
            return Err(DedicatedError::Physical);
        }
        if matches!(self.power_control, UplinkPowerControl::Algorithm1 { step_db } if step_db != 1 && step_db != 2)
        {
            return Err(DedicatedError::Physical);
        }
        self.pilot_wire()?;
        // TS 25.331 8.6.6.14 requires this exact DOFF relation for a single normal DPCH link.
        if u32::from(self.default_offset_512_chips) * DEFAULT_OFFSET_QUANTUM % CHIPS_PER_FRAME
            != u32::from(self.frame_offset_256_chips) * FRAME_OFFSET_QUANTUM
        {
            return Err(DedicatedError::Physical);
        }
        Ok(())
    }
    pub fn cfn_for_sfn(&self, sfn: u16) -> Result<u8, DedicatedError> {
        self.validate()?;
        if sfn >= 4096 {
            return Err(DedicatedError::Time);
        }
        // TS 25.331 8.5.15.1 defines CFN from actual DOFF chips, not the encoded offset index.
        let frames =
            u32::from(self.default_offset_512_chips) * DEFAULT_OFFSET_QUANTUM / CHIPS_PER_FRAME;
        Ok(((u32::from(sfn) + 256 - frames) % 256) as u8)
    }
    fn pilot_wire(&self) -> Result<ie::SF512AndPilot, DedicatedError> {
        Ok(match (self.downlink_sf, self.downlink_pilot_bits) {
            (4, 16) => ie::SF512AndPilot::sfd4(()),
            (8, 16) => ie::SF512AndPilot::sfd8(()),
            (16, 16) => ie::SF512AndPilot::sfd16(()),
            (32, 8) => ie::SF512AndPilot::sfd32(()),
            (64, 8) => ie::SF512AndPilot::sfd64(()),
            (128, 4) => ie::SF512AndPilot::sfd128(ie::PilotBits128::pb4),
            (128, 8) => ie::SF512AndPilot::sfd128(ie::PilotBits128::pb8),
            (256, 2) => ie::SF512AndPilot::sfd256(ie::PilotBits256::pb2),
            (256, 4) => ie::SF512AndPilot::sfd256(ie::PilotBits256::pb4),
            (256, 8) => ie::SF512AndPilot::sfd256(ie::PilotBits256::pb8),
            (512, 4) => ie::SF512AndPilot::sfd512(()),
            _ => return Err(DedicatedError::Physical),
        })
    }
    pub(crate) fn uplink_wire(&self) -> Result<ie::ULChannelRequirement, DedicatedError> {
        let power = self.power_wire()?;
        let spreading = match self.uplink_minimum_sf {
            4 => ie::SpreadingFactor::sf4,
            8 => ie::SpreadingFactor::sf8,
            16 => ie::SpreadingFactor::sf16,
            32 => ie::SpreadingFactor::sf32,
            64 => ie::SpreadingFactor::sf64,
            128 => ie::SpreadingFactor::sf128,
            _ => ie::SpreadingFactor::sf256,
        };
        let puncturing = match self.puncturing_percent {
            40 => ie::PuncturingLimit::pl0_40,
            44 => ie::PuncturingLimit::pl0_44,
            48 => ie::PuncturingLimit::pl0_48,
            52 => ie::PuncturingLimit::pl0_52,
            56 => ie::PuncturingLimit::pl0_56,
            60 => ie::PuncturingLimit::pl0_60,
            64 => ie::PuncturingLimit::pl0_64,
            68 => ie::PuncturingLimit::pl0_68,
            72 => ie::PuncturingLimit::pl0_72,
            76 => ie::PuncturingLimit::pl0_76,
            80 => ie::PuncturingLimit::pl0_80,
            84 => ie::PuncturingLimit::pl0_84,
            88 => ie::PuncturingLimit::pl0_88,
            92 => ie::PuncturingLimit::pl0_92,
            96 => ie::PuncturingLimit::pl0_96,
            _ => ie::PuncturingLimit::pl1,
        };
        Ok(ie::ULChannelRequirement::ul_DPCH_Info(ie::ULDPCHInfo::new(
            Some(ie::ULDPCHPowerControlInfo::fdd(power)),
            ie::ULDPCHInfoModeSpecificInfo::fdd(ie::ULDPCHInfoModeSpecificInfoFdd::new(
                ie::ScramblingCodeType::longSC,
                ie::ULScramblingCode(self.uplink_scrambling),
                ie::NumberOfDPDCH(self.uplink_dpdch_count),
                spreading,
                self.uplink_tfci,
                None,
                puncturing,
            )),
        )))
    }
    pub(crate) fn power_wire(&self) -> Result<ie::ULDPCHPowerControlInfoFdd, DedicatedError> {
        self.validate()?;
        let algorithm = match self.power_control {
            UplinkPowerControl::Algorithm1 { step_db } => {
                ie::PowerControlAlgorithm::algorithm1(ie::TPCStepSizeFDD(step_db - 1))
            }
            UplinkPowerControl::Algorithm2 => ie::PowerControlAlgorithm::algorithm2(()),
        };
        Ok(ie::ULDPCHPowerControlInfoFdd::new(
            ie::DPCCHPowerOffset((self.dpcch_offset_db / 2) as i8),
            ie::PCPreamble(self.pc_preamble_frames),
            ie::SRBDelay(self.srb_delay_frames),
            algorithm,
        ))
    }
    pub(super) fn downlink_wire(
        &self,
        initial: bool,
    ) -> Result<(ie::DLCommonInformation, ie::DLInformationPerRLList), DedicatedError> {
        self.validate()?;
        let cfn = if initial {
            ie::DLDPCHInfoCommonCfnHandling::initialise(
                ie::DLDPCHInfoCommonCfnHandlingInitialise::new(None),
            )
        } else {
            ie::DLDPCHInfoCommonCfnHandling::maintain(())
        };
        let common = ie::DLCommonInformation::new(
            Some(ie::DLDPCHInfoCommon::new(
                cfn,
                ie::DLDPCHInfoCommonModeSpecificInfo::fdd(
                    ie::DLDPCHInfoCommonModeSpecificInfoFdd::new(
                        Some(ie::DLDPCHPowerControlInfo::new(
                            ie::DLDPCHPowerControlInfoModeSpecificInfo::fdd(
                                ie::DLDPCHPowerControlInfoModeSpecificInfoFdd::new(
                                    ie::DPCMode::singleTPC,
                                ),
                            ),
                        )),
                        ie::PowerOffsetPilotPdpdch(self.pilot_offset_quarter_db),
                        None,
                        self.pilot_wire()?,
                        if self.flexible_position {
                            ie::PositionFixedOrFlexible::flexible
                        } else {
                            ie::PositionFixedOrFlexible::fixed
                        },
                        self.downlink_tfci,
                    ),
                ),
            )),
            ie::DLCommonInformationModeSpecificInfo::fdd(
                ie::DLCommonInformationModeSpecificInfoFdd::new(
                    Some(ie::DefaultDPCHOffsetValueFDD(self.default_offset_512_chips)),
                    None,
                    None,
                    None,
                ),
            ),
        );
        let codes = self
            .downlink_codes
            .iter()
            .map(|&code| {
                let code = match self.downlink_sf {
                    4 => ie::SF512AndCodeNumber::sf4(code as u8),
                    8 => ie::SF512AndCodeNumber::sf8(code as u8),
                    16 => ie::SF512AndCodeNumber::sf16(code as u8),
                    32 => ie::SF512AndCodeNumber::sf32(code as u8),
                    64 => ie::SF512AndCodeNumber::sf64(code as u8),
                    128 => ie::SF512AndCodeNumber::sf128(code as u8),
                    256 => ie::SF512AndCodeNumber::sf256(code as u8),
                    _ => ie::SF512AndCodeNumber::sf512(code),
                };
                ie::DLChannelisationCode::new(
                    self.secondary_scrambling.map(ie::SecondaryScramblingCode),
                    code,
                    None,
                )
            })
            .collect();
        let link = ie::DLInformationPerRL::new(
            ie::DLInformationPerRLModeSpecificInfo::fdd(
                ie::DLInformationPerRLModeSpecificInfoFdd::new(
                    ie::PrimaryCPICHInfo::new(ie::PrimaryScramblingCode(self.primary_scrambling)),
                    None,
                    None,
                ),
            ),
            Some(ie::DLDPCHInfoPerRL::fdd(ie::DLDPCHInfoPerRLFdd::new(
                ie::PCPICHUsageForChannelEst::mayBeUsed,
                ie::DPCHFrameOffset(self.frame_offset_256_chips),
                None,
                ie::DLChannelisationCodeList(codes),
                ie::TPCCombinationIndex(0),
                None,
                None,
            ))),
            None,
        );
        Ok((common, ie::DLInformationPerRLList(vec![link])))
    }
}
