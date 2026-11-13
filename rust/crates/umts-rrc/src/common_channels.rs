// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    EncodedInformation,
    random_access::{AccessError, PrachInformation},
    transport::{
        ChannelCoding, CommonTransportFormatSet, ConvolutionalRate, TransportCombinations,
        TransportError, TransportInterval,
    },
};
use rasn::Decode;
use umts_rrc_codec::{CodecError, SibType};
use umts_rrc_schema::information_elements as ie;
use umts_types::channel_codes::OvsfCode;

const MAX_SCCPCH: usize = 16;
const MAX_PRACH: usize = 16;
const MAX_FACH_PCH: usize = 8;
const MAX_TRCH_ID: u8 = 32;
const MAX_COMBINATIONS: usize = 1024;
const FIXED_SF: u16 = 256;
const MIN_SF: u16 = 4;
const TIMING_STEP_CHIPS: u16 = 256;
const MAX_TIMING_CHIPS: u16 = 38144;
const MAX_SECONDARY_SCRAMBLING_CODE: u8 = 15;
const MIN_PICH_OFFSET_DB: i8 = -10;
const MAX_PICH_OFFSET_DB: i8 = 5;
const MIN_AICH_OFFSET_DB: i8 = -22;
const MAX_AICH_OFFSET_DB: i8 = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommonChannelError {
    Count,
    Code,
    CodeCollision,
    ScramblingCode,
    Timing,
    TransportIdentity,
    PagingOrder,
    PagingProfile,
    PichAssociation,
    Power,
    FrequencyBand,
    Access(AccessError),
    Transport(TransportError),
    Codec(CodecError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportRole {
    Paging,
    ForwardAccess,
}

pub struct DownlinkTransport {
    pub role: TransportRole,
    pub identity: u8,
    pub formats: CommonTransportFormatSet,
}

#[derive(Clone, Copy, Debug)]
pub enum Position {
    Fixed,
    Flexible,
}

#[derive(Clone, Copy, Debug)]
pub enum PagingIndicators {
    PerFrame18,
    PerFrame36,
    PerFrame72,
    PerFrame144,
}

pub struct Pich {
    pub code256: u8,
    pub indicators: PagingIndicators,
    pub transmit_diversity: bool,
}

pub struct SecondaryChannelSettings {
    pub code: OvsfCode,
    pub secondary_scrambling_code: Option<u8>,
    pub transmit_diversity: bool,
    pub pilots: bool,
    pub tfci: bool,
    pub position: Position,
    pub timing_offset_chips: u16,
    pub pich: Option<Pich>,
}

pub struct SecondaryChannel {
    settings: SecondaryChannelSettings,
    transports: Vec<DownlinkTransport>,
    combinations: TransportCombinations,
}

impl SecondaryChannel {
    pub fn new(
        settings: SecondaryChannelSettings,
        transports: Vec<DownlinkTransport>,
        tuples: &[Vec<usize>],
    ) -> Result<Self, CommonChannelError> {
        if transports.is_empty()
            || transports.len() > MAX_FACH_PCH
            || tuples.is_empty()
            || tuples.len() > MAX_COMBINATIONS
        {
            return Err(CommonChannelError::Count);
        }
        if settings.code.spreading_factor() < MIN_SF || settings.code.spreading_factor() > FIXED_SF
        {
            return Err(CommonChannelError::Code);
        }
        if settings.timing_offset_chips > MAX_TIMING_CHIPS
            || !settings
                .timing_offset_chips
                .is_multiple_of(TIMING_STEP_CHIPS)
        {
            return Err(CommonChannelError::Timing);
        }
        if settings
            .secondary_scrambling_code
            .is_some_and(|code| !(1..=MAX_SECONDARY_SCRAMBLING_CODE).contains(&code))
        {
            return Err(CommonChannelError::ScramblingCode);
        }
        let mut identities = std::collections::BTreeSet::new();
        for (position, transport) in transports.iter().enumerate() {
            if !(1..=MAX_TRCH_ID).contains(&transport.identity)
                || !identities.insert(transport.identity)
            {
                return Err(CommonChannelError::TransportIdentity);
            }
            if transport.role == TransportRole::Paging {
                // TS 25.331 10.3.6.72 requires PCH first in its FACH/PCH list.
                if position != 0 {
                    return Err(CommonChannelError::PagingOrder);
                }
                // TS 25.212 Table 1 and 4.2.13.7 specify PCH rate 1/2 and 10 ms TTI.
                if !matches!(transport.formats.interval, TransportInterval::Ms10)
                    || !matches!(
                        transport.formats.coding,
                        ChannelCoding::Convolutional(ConvolutionalRate::Half)
                    )
                {
                    return Err(CommonChannelError::PagingProfile);
                }
                if settings.secondary_scrambling_code.is_some() {
                    return Err(CommonChannelError::ScramblingCode);
                }
            }
        }
        let paging = transports[0].role == TransportRole::Paging;
        if settings.pich.is_some() && !paging {
            return Err(CommonChannelError::PichAssociation);
        }
        let formats: Vec<_> = transports
            .iter()
            .map(|transport| &transport.formats)
            .collect();
        let combinations =
            TransportCombinations::new(&formats, tuples).map_err(CommonChannelError::Transport)?;
        Ok(Self {
            settings,
            transports,
            combinations,
        })
    }

    pub fn settings(&self) -> &SecondaryChannelSettings {
        &self.settings
    }
    pub fn transports(&self) -> &[DownlinkTransport] {
        &self.transports
    }
    pub fn combinations(&self) -> &TransportCombinations {
        &self.combinations
    }
    pub fn carries_paging(&self) -> bool {
        self.transports[0].role == TransportRole::Paging
    }

    fn generated(&self) -> Result<ie::SCCPCHSystemInformation, CommonChannelError> {
        let settings = &self.settings;
        let index = settings.code.index() as u8;
        let sf_and_code_number = match settings.code.spreading_factor() {
            4 => ie::SF256AndCodeNumber::sf4(index),
            8 => ie::SF256AndCodeNumber::sf8(index),
            16 => ie::SF256AndCodeNumber::sf16(index),
            32 => ie::SF256AndCodeNumber::sf32(index),
            64 => ie::SF256AndCodeNumber::sf64(index),
            128 => ie::SF256AndCodeNumber::sf128(index),
            256 => ie::SF256AndCodeNumber::sf256(index),
            _ => return Err(CommonChannelError::Code),
        };
        let transports = self
            .transports
            .iter()
            .map(|transport| {
                Ok(ie::FACHPCHInformation {
                    transport_format_set: decoded(
                        &transport
                            .formats
                            .encode()
                            .map_err(CommonChannelError::Transport)?,
                    )?,
                    transport_channel_identity: ie::TransportChannelIdentity(transport.identity),
                    ctch_indicator: false,
                })
            })
            .collect::<Result<Vec<_>, CommonChannelError>>()?;
        Ok(ie::SCCPCHSystemInformation {
            secondary_ccpch_info: ie::SecondaryCCPCHInfo {
                mode_specific_info: ie::SecondaryCCPCHInfoModeSpecificInfo::fdd(
                    ie::SecondaryCCPCHInfoModeSpecificInfoFdd {
                        dummy1: ie::PCPICHUsageForChannelEst::mayBeUsed,
                        dummy2: None,
                        secondary_scrambling_code: settings
                            .secondary_scrambling_code
                            .map(ie::SecondaryScramblingCode),
                        sttd_indicator: settings.transmit_diversity,
                        sf_and_code_number,
                        pilot_symbol_existence: settings.pilots,
                        tfci_existence: settings.tfci,
                        position_fixed_or_flexible: match settings.position {
                            Position::Fixed => ie::PositionFixedOrFlexible::fixed,
                            Position::Flexible => ie::PositionFixedOrFlexible::flexible,
                        },
                        timing_offset: ie::TimingOffset(
                            (settings.timing_offset_chips / TIMING_STEP_CHIPS) as u8,
                        ),
                    },
                ),
            },
            tfcs: Some(decoded(
                &self
                    .combinations
                    .encode()
                    .map_err(CommonChannelError::Transport)?,
            )?),
            fach_pch_information_list: Some(ie::FACHPCHInformationList(transports)),
            pich_info: settings.pich.as_ref().map(|pich| {
                ie::PICHInfo::fdd(ie::PICHInfoFdd {
                    channelisation_code256: ie::ChannelisationCode256(pich.code256),
                    pi_count_per_frame: match pich.indicators {
                        PagingIndicators::PerFrame18 => ie::PICountPerFrame::e18,
                        PagingIndicators::PerFrame36 => ie::PICountPerFrame::e36,
                        PagingIndicators::PerFrame72 => ie::PICountPerFrame::e72,
                        PagingIndicators::PerFrame144 => ie::PICountPerFrame::e144,
                    },
                    sttd_indicator: pich.transmit_diversity,
                })
            }),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FddBand {
    I = 1,
    II = 2,
    III = 3,
    IV = 4,
    V = 5,
    VI = 6,
    VII = 7,
    VIII = 8,
    IX = 9,
    X = 10,
    XI = 11,
    XII = 12,
    XIII = 13,
    XIV = 14,
    XIX = 19,
    XX = 20,
    XXI = 21,
}

impl FddBand {
    pub fn new(number: u8) -> Result<Self, CommonChannelError> {
        match number {
            1 => Ok(Self::I),
            2 => Ok(Self::II),
            3 => Ok(Self::III),
            4 => Ok(Self::IV),
            5 => Ok(Self::V),
            6 => Ok(Self::VI),
            7 => Ok(Self::VII),
            8 => Ok(Self::VIII),
            9 => Ok(Self::IX),
            10 => Ok(Self::X),
            11 => Ok(Self::XI),
            12 => Ok(Self::XII),
            13 => Ok(Self::XIII),
            14 => Ok(Self::XIV),
            19 => Ok(Self::XIX),
            20 => Ok(Self::XX),
            21 => Ok(Self::XXI),
            _ => Err(CommonChannelError::FrequencyBand),
        }
    }

    pub fn number(self) -> u8 {
        self as u8
    }

    fn indicators(
        self,
    ) -> (
        ie::RadioFrequencyBandFDD,
        Option<ie::RadioFrequencyBandFDD2>,
    ) {
        use ie::{RadioFrequencyBandFDD as First, RadioFrequencyBandFDD2 as Second};
        let second = match self {
            Self::I => return (First::fdd2100, None),
            Self::II => return (First::fdd1900, None),
            Self::III => return (First::fdd1800, None),
            Self::IV => return (First::bandIV, None),
            Self::V => return (First::bandV, None),
            Self::VI => return (First::bandVI, None),
            Self::VII => return (First::bandVII, None),
            Self::VIII => Second::bandVIII,
            Self::IX => Second::bandIX,
            Self::X => Second::bandX,
            Self::XI => Second::bandXI,
            Self::XII => Second::bandXII,
            Self::XIII => Second::bandXIII,
            Self::XIV => Second::bandXIV,
            Self::XIX => Second::bandXIX,
            Self::XX => Second::bandXX,
            Self::XXI => Second::bandXXI,
        };
        (First::extension_indicator, Some(second))
    }

    fn extensions(self) -> ie::SysInfoType5V4b0NonCriticalExtensions {
        let (first, second) = self.indicators();
        ie::SysInfoType5V4b0NonCriticalExtensions {
            sys_info_type5_v4b0ext: Some(ie::SysInfoType5V4b0extIEs {
                p_nbsch_allocation_r4: None,
                open_loop_power_control_ipdl_tdd: None,
                prach_system_information_list_lcr_r4: None,
                tdd128_specific_info: None,
                frequency_band_indicator: Some(first),
            }),
            v590_non_critical_extensions: second.map(|second| {
                ie::SysInfoType5V4b0NonCriticalExtensionsV590NonCriticalExtensions {
                    sys_info_type5_v590ext: None,
                    v650_non_critical_extensions: Some(
                        ie::SysInfoType5V4b0NonCriticalExtensionsV590NonCriticalExtensionsV650NonCriticalExtensions {
                            sys_info_type5_v650ext: Some(ie::SysInfoType5V650extIEs {
                                frequency_band_indicator2: second,
                            }),
                            v680_non_critical_extensions: None,
                        },
                    ),
                }
            }),
        }
    }
}

pub struct Sib5Settings {
    pub frequency_band: FddBand,
    pub sib6_available: bool,
    pub pich_power_offset_db: i8,
    pub aich_power_offset_db: i8,
    pub primary_ccpch_transmit_diversity: bool,
}

pub struct Sib5 {
    settings: Sib5Settings,
    prach: Vec<PrachInformation>,
    secondary: Vec<SecondaryChannel>,
}

impl Sib5 {
    pub fn new(
        settings: Sib5Settings,
        prach: Vec<PrachInformation>,
        secondary: Vec<SecondaryChannel>,
    ) -> Result<Self, CommonChannelError> {
        if prach.is_empty()
            || prach.len() > MAX_PRACH
            || secondary.is_empty()
            || secondary.len() > MAX_SCCPCH
        {
            return Err(CommonChannelError::Count);
        }
        if !(MIN_PICH_OFFSET_DB..=MAX_PICH_OFFSET_DB).contains(&settings.pich_power_offset_db)
            || !(MIN_AICH_OFFSET_DB..=MAX_AICH_OFFSET_DB).contains(&settings.aich_power_offset_db)
        {
            return Err(CommonChannelError::Power);
        }
        let mut seen_nonpaging = false;
        let mut codes = vec![
            (None, OvsfCode::new(FIXED_SF, 0).unwrap()),
            (None, OvsfCode::new(FIXED_SF, 1).unwrap()),
        ];
        let mut aich_codes = std::collections::BTreeMap::new();
        for access in &prach {
            let aich = access.settings().aich;
            if let Some(&diversity) = aich_codes.get(&aich.code256) {
                if diversity != aich.transmit_diversity {
                    return Err(CommonChannelError::CodeCollision);
                }
            } else {
                reserve_code(
                    &mut codes,
                    None,
                    OvsfCode::new(FIXED_SF, u16::from(aich.code256)).unwrap(),
                )?;
                aich_codes.insert(aich.code256, aich.transmit_diversity);
            }
        }
        for channel in &secondary {
            // TS 25.331 10.3.6.72 lists all S-CCPCHs mapped to PCH before FACH-only channels.
            if channel.carries_paging() && seen_nonpaging {
                return Err(CommonChannelError::PagingOrder);
            }
            seen_nonpaging |= !channel.carries_paging();
            reserve_code(
                &mut codes,
                channel.settings.secondary_scrambling_code,
                channel.settings.code,
            )?;
            if let Some(pich) = &channel.settings.pich {
                reserve_code(
                    &mut codes,
                    None,
                    OvsfCode::new(FIXED_SF, u16::from(pich.code256)).unwrap(),
                )?;
            }
        }
        Ok(Self {
            settings,
            prach,
            secondary,
        })
    }

    pub fn prach(&self) -> &[PrachInformation] {
        &self.prach
    }
    pub fn settings(&self) -> &Sib5Settings {
        &self.settings
    }
    pub fn secondary_channels(&self) -> &[SecondaryChannel] {
        &self.secondary
    }

    pub fn encode(&self) -> Result<EncodedInformation, CommonChannelError> {
        let prach = self
            .prach
            .iter()
            .map(|access| decoded(&access.encode().map_err(CommonChannelError::Access)?))
            .collect::<Result<Vec<_>, _>>()?;
        let secondary = self
            .secondary
            .iter()
            .map(SecondaryChannel::generated)
            .collect::<Result<Vec<_>, _>>()?;
        let value = ie::SysInfoType5 {
            sib6indicator: self.settings.sib6_available,
            pich_power_offset: ie::PICHPowerOffset(self.settings.pich_power_offset_db),
            mode_specific_info: ie::SysInfoType5ModeSpecificInfo::fdd(
                ie::SysInfoType5ModeSpecificInfoFdd {
                    aich_power_offset: ie::AICHPowerOffset(self.settings.aich_power_offset_db),
                },
            ),
            primary_ccpch_info: Some(ie::PrimaryCCPCHInfo::fdd(ie::PrimaryCCPCHInfoFdd {
                tx_diversity_indicator: self.settings.primary_ccpch_transmit_diversity,
            })),
            prach_system_information_list: ie::PRACHSystemInformationList(prach),
            s_ccpch_system_information_list: ie::SCCPCHSystemInformationList(secondary),
            cbs_drx_level1_information: None,
            // TS 25.331 8.1.1.6.5 leaves Band IV and later unspecified without a band indicator.
            v4b0_non_critical_extensions: Some(self.settings.frequency_band.extensions()),
        };
        crate::generated_information::encode(SibType::Type5, &value)
            .map_err(CommonChannelError::Codec)
    }
}

fn reserve_code(
    codes: &mut Vec<(Option<u8>, OvsfCode)>,
    scrambling: Option<u8>,
    code: OvsfCode,
) -> Result<(), CommonChannelError> {
    if codes.iter().any(|&(existing_scrambling, existing)| {
        existing_scrambling == scrambling && existing.overlaps(code)
    }) {
        return Err(CommonChannelError::CodeCollision);
    }
    codes.push((scrambling, code));
    Ok(())
}

fn decoded<T: Decode>(encoded: &EncodedInformation) -> Result<T, CommonChannelError> {
    crate::generated_information::decode(encoded).map_err(CommonChannelError::Codec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bits::BitWriter,
        random_access::{
            AccessServiceClass, Aich, AichTiming, MinimumSpreadingFactor, PrachSettings,
            PuncturingLimit, Signatures, Subchannels,
        },
        transport::{CommonFormatGroup, CrcSize, LogicalChannels, PrachMessagePower},
    };

    fn formats() -> CommonTransportFormatSet {
        CommonTransportFormatSet {
            interval: TransportInterval::Ms10,
            coding: ChannelCoding::Convolutional(ConvolutionalRate::Half),
            rate_matching_attribute: 256,
            crc: CrcSize::Bits16,
            groups: vec![CommonFormatGroup {
                transport_block_bits: 168,
                block_counts: vec![0, 1],
                logical_channels: LogicalChannels::Configured,
            }],
        }
    }

    fn access() -> PrachInformation {
        access_with_aich(Aich {
            code256: 2,
            transmit_diversity: false,
            timing: AichTiming::FiveSlots,
        })
    }

    fn access_with_aich(aich: Aich) -> PrachInformation {
        let settings = PrachSettings {
            signatures: Signatures::new(&[0, 15]).unwrap(),
            subchannels: Subchannels::new(&[0, 11]).unwrap(),
            minimum_spreading_factor: MinimumSpreadingFactor::Sf256,
            preamble_scrambling_code: 0,
            puncturing_limit: PuncturingLimit::from_percent(100).unwrap(),
            transport_identity: 1,
            access_classes: vec![AccessServiceClass {
                first_signature_index: 0,
                last_signature_index: 1,
                assigned_subchannels: 15,
            }],
            ac_to_asc: [0; 7],
            persistence_factors: None,
            primary_cpich_dbm: 10,
            initial_power_constant_db: -10,
            ramp_step_db: 1,
            preamble_retransmissions: 64,
            maximum_ramp_cycles: 32,
            backoff_min: 0,
            backoff_max: 50,
            aich,
        };
        let mut transport = formats();
        transport.interval = TransportInterval::Ms20;
        transport.groups[0].logical_channels = LogicalChannels::AllSizes;
        PrachInformation::new(
            settings,
            transport,
            &[0, 1],
            vec![PrachMessagePower::new(15, 15, 2).unwrap(); 2],
        )
        .unwrap()
    }

    fn physical(paging: bool, index: u16) -> SecondaryChannelSettings {
        SecondaryChannelSettings {
            code: OvsfCode::new(64, index).unwrap(),
            secondary_scrambling_code: None,
            transmit_diversity: false,
            pilots: false,
            tfci: true,
            position: Position::Flexible,
            timing_offset_chips: 0,
            pich: paging.then_some(Pich {
                code256: 3,
                indicators: PagingIndicators::PerFrame18,
                transmit_diversity: false,
            }),
        }
    }

    fn channel(paging: bool, index: u16) -> SecondaryChannel {
        SecondaryChannel::new(
            physical(paging, index),
            vec![DownlinkTransport {
                role: if paging {
                    TransportRole::Paging
                } else {
                    TransportRole::ForwardAccess
                },
                identity: 1,
                formats: formats(),
            }],
            &[vec![0], vec![1]],
        )
        .unwrap()
    }

    fn settings() -> Sib5Settings {
        Sib5Settings {
            frequency_band: FddBand::V,
            sib6_available: false,
            pich_power_offset_db: -5,
            aich_power_offset_db: -10,
            primary_ccpch_transmit_diversity: false,
        }
    }

    #[test]
    fn sib5_matches_independently_assembled_base_fdd_fields() {
        let encoded = Sib5::new(settings(), vec![access()], vec![channel(true, 4)])
            .unwrap()
            .encode()
            .unwrap();
        let mut expected = BitWriter::default();
        for (value, width) in [(5, 3), (0, 1), (5, 4), (0, 1), (12, 5), (0, 2), (0, 4)] {
            expected.write(value, width);
        }
        let prach: [u8; 30] = [
            0xea, 0x00, 0x07, 0x0f, 0x80, 0x10, 0x48, 0x03, 0xc2, 0x22, 0xff, 0x60, 0x00, 0x32,
            0x3f, 0xde, 0xc7, 0xfb, 0x84, 0x07, 0xc0, 0x00, 0x00, 0xfa, 0x99, 0x1f, 0xfc, 0x0c,
            0x80, 0x90,
        ];
        for bit in 0..236 {
            expected.write(u64::from((prach[bit / 8] >> (7 - bit % 8)) & 1), 1);
        }
        for (value, width) in [
            (0, 4),
            (7, 3),
            (0, 1),
            (0, 3),
            (0, 1),
            (0, 1),
            (4, 3),
            (4, 6),
            (0, 1),
            (1, 1),
            (1, 1),
        ] {
            expected.write(value, width);
        }
        let tfcs: [u8; 3] = [0x00, 0x01, 0x04];
        for bit in 0..22 {
            expected.write(u64::from((tfcs[bit / 8] >> (7 - bit % 8)) & 1), 1);
        }
        expected.write(0, 3);
        let tfs: [u8; 6] = [0x80, 0x07, 0x84, 0x55, 0xfe, 0xc0];
        for bit in 0..42 {
            expected.write(u64::from((tfs[bit / 8] >> (7 - bit % 8)) & 1), 1);
        }
        for (value, width) in [(0, 5), (0, 1), (0, 1), (3, 8), (0, 2), (0, 1)] {
            expected.write(value, width);
        }
        for (value, width) in [(2, 2), (1, 5), (5, 3)] {
            expected.write(value, width);
        }
        assert_eq!(encoded.bit_length, 376);
        assert_eq!(encoded.bit_length, expected.length);
        assert_eq!(encoded.bytes, expected.bytes);
    }

    #[test]
    fn frequency_band_indicators_preserve_actual_asn_wire_ordinals() {
        // TS 25.331 ASN.1 places Band VI before IV and V in the first indicator.
        for (number, first, second) in [
            (1, 0, None),
            (2, 1, None),
            (3, 2, None),
            (4, 4, None),
            (5, 5, None),
            (6, 3, None),
            (7, 6, None),
            (8, 7, Some(0)),
            (9, 7, Some(1)),
            (10, 7, Some(2)),
            (11, 7, Some(3)),
            (12, 7, Some(4)),
            (13, 7, Some(5)),
            (14, 7, Some(6)),
            (19, 7, Some(11)),
            (20, 7, Some(12)),
            (21, 7, Some(13)),
        ] {
            let mut settings = settings();
            settings.frequency_band = FddBand::new(number).unwrap();
            assert_eq!(settings.frequency_band.number(), number);
            let encoded = Sib5::new(settings, vec![access()], vec![channel(true, 4)])
                .unwrap()
                .encode()
                .unwrap();
            let value: ie::SysInfoType5 = decoded(&encoded).unwrap();
            let extension = value.v4b0_non_critical_extensions.unwrap();
            let band = extension.sys_info_type5_v4b0ext.unwrap();
            assert!(band.p_nbsch_allocation_r4.is_none());
            assert!(band.open_loop_power_control_ipdl_tdd.is_none());
            assert!(band.prach_system_information_list_lcr_r4.is_none());
            assert!(band.tdd128_specific_info.is_none());
            assert_eq!(band.frequency_band_indicator.unwrap() as u8, first);
            let decoded_second = extension.v590_non_critical_extensions.map(|later| {
                assert!(later.sys_info_type5_v590ext.is_none());
                let later = later.v650_non_critical_extensions.unwrap();
                assert!(later.v680_non_critical_extensions.is_none());
                later
                    .sys_info_type5_v650ext
                    .unwrap()
                    .frequency_band_indicator2 as u8
            });
            assert_eq!(decoded_second, second);

            let mut expected = BitWriter::default();
            expected.write(if second.is_some() { 3 } else { 2 }, 2);
            expected.write(1, 5);
            expected.write(u64::from(first), 3);
            if let Some(second) = second {
                expected.write(1, 2);
                expected.write(2, 2);
                expected.write(u64::from(second), 4);
            }
            let base_bits = 366;
            assert_eq!(encoded.bit_length, base_bits + expected.length);
            assert_eq!(encoded.bytes[0] >> 5, 5);
            for bit in 0..expected.length {
                let actual = base_bits + bit;
                assert_eq!(
                    (encoded.bytes[actual / 8] >> (7 - actual % 8)) & 1,
                    (expected.bytes[bit / 8] >> (7 - bit % 8)) & 1,
                    "band {number}, extension bit {bit}"
                );
            }
        }
    }

    #[test]
    fn frequency_band_rejects_every_unsupported_number() {
        for number in 0..=u8::MAX {
            let supported = (1..=14).contains(&number) || (19..=21).contains(&number);
            assert_eq!(FddBand::new(number).is_ok(), supported);
            if !supported {
                assert_eq!(FddBand::new(number), Err(CommonChannelError::FrequencyBand));
            }
        }
    }

    #[test]
    fn paging_transport_and_physical_lists_keep_their_specified_order() {
        let transports = vec![
            DownlinkTransport {
                role: TransportRole::Paging,
                identity: 9,
                formats: formats(),
            },
            DownlinkTransport {
                role: TransportRole::ForwardAccess,
                identity: 1,
                formats: formats(),
            },
        ];
        let combined = SecondaryChannel::new(
            physical(true, 4),
            transports,
            &[vec![0, 0], vec![0, 1], vec![1, 1]],
        )
        .unwrap();
        assert_eq!(combined.combinations().ctfc_values(), &[0, 2, 3]);
        let encoded = Sib5::new(
            settings(),
            vec![access()],
            vec![combined, channel(false, 5)],
        )
        .unwrap()
        .encode()
        .unwrap();
        let decoded: ie::SysInfoType5 = rasn::uper::decode(&encoded.bytes).unwrap();
        assert_eq!(decoded.s_ccpch_system_information_list.0.len(), 2);
        let transports = &decoded.s_ccpch_system_information_list.0[0]
            .fach_pch_information_list
            .as_ref()
            .unwrap()
            .0;
        assert_eq!(
            transports
                .iter()
                .map(|t| t.transport_channel_identity.0)
                .collect::<Vec<_>>(),
            [9, 1]
        );
        assert_eq!(
            Sib5::new(
                settings(),
                vec![access()],
                vec![channel(false, 5), channel(true, 4)]
            )
            .err(),
            Some(CommonChannelError::PagingOrder)
        );
        let transports = vec![
            DownlinkTransport {
                role: TransportRole::ForwardAccess,
                identity: 1,
                formats: formats(),
            },
            DownlinkTransport {
                role: TransportRole::Paging,
                identity: 2,
                formats: formats(),
            },
        ];
        assert_eq!(
            SecondaryChannel::new(physical(true, 4), transports, &[vec![0, 0]]).err(),
            Some(CommonChannelError::PagingOrder)
        );
    }

    #[test]
    fn paging_tti_coding_and_scrambling_are_not_generic_fach_settings() {
        for (interval, coding) in [
            (
                TransportInterval::Ms20,
                ChannelCoding::Convolutional(ConvolutionalRate::Half),
            ),
            (
                TransportInterval::Ms10,
                ChannelCoding::Convolutional(ConvolutionalRate::Third),
            ),
            (TransportInterval::Ms10, ChannelCoding::Turbo),
        ] {
            let mut tfs = formats();
            tfs.interval = interval;
            tfs.coding = coding;
            assert_eq!(
                SecondaryChannel::new(
                    physical(true, 4),
                    vec![DownlinkTransport {
                        role: TransportRole::Paging,
                        identity: 1,
                        formats: tfs
                    }],
                    &[vec![0]]
                )
                .err(),
                Some(CommonChannelError::PagingProfile)
            );
        }
        let mut config = physical(true, 4);
        config.secondary_scrambling_code = Some(1);
        assert_eq!(
            SecondaryChannel::new(
                config,
                vec![DownlinkTransport {
                    role: TransportRole::Paging,
                    identity: 1,
                    formats: formats()
                }],
                &[vec![0]]
            )
            .err(),
            Some(CommonChannelError::ScramblingCode)
        );
        let config = physical(true, 4);
        assert_eq!(
            SecondaryChannel::new(
                config,
                vec![DownlinkTransport {
                    role: TransportRole::ForwardAccess,
                    identity: 1,
                    formats: formats()
                }],
                &[vec![0]]
            )
            .err(),
            Some(CommonChannelError::PichAssociation)
        );
    }

    #[test]
    fn code_tree_admission_reserves_broadcast_aich_pich_and_sccpch() {
        for code in [
            OvsfCode::new(64, 0).unwrap(),
            OvsfCode::new(256, 2).unwrap(),
            OvsfCode::new(128, 1).unwrap(),
        ] {
            let mut config = physical(false, 4);
            config.code = code;
            let ch = SecondaryChannel::new(
                config,
                vec![DownlinkTransport {
                    role: TransportRole::ForwardAccess,
                    identity: 1,
                    formats: formats(),
                }],
                &[vec![0]],
            )
            .unwrap();
            assert_eq!(
                Sib5::new(settings(), vec![access()], vec![ch]).err(),
                Some(CommonChannelError::CodeCollision)
            );
        }
        assert_eq!(
            Sib5::new(
                settings(),
                vec![access()],
                vec![channel(true, 4), channel(false, 4)]
            )
            .err(),
            Some(CommonChannelError::CodeCollision)
        );
        let mut config = physical(false, 4);
        config.secondary_scrambling_code = Some(1);
        let ch = SecondaryChannel::new(
            config,
            vec![DownlinkTransport {
                role: TransportRole::ForwardAccess,
                identity: 1,
                formats: formats(),
            }],
            &[vec![0]],
        )
        .unwrap();
        Sib5::new(settings(), vec![access()], vec![channel(true, 4), ch])
            .unwrap()
            .encode()
            .unwrap();
    }

    #[test]
    fn explicit_timing_and_all_legal_spreading_factor_branches_encode() {
        for sf in [4, 8, 16, 32, 64, 128, 256] {
            let mut config = physical(false, 4);
            config.code = OvsfCode::new(sf, sf - 1).unwrap();
            config.timing_offset_chips = MAX_TIMING_CHIPS;
            config.position = Position::Fixed;
            config.pilots = true;
            let ch = SecondaryChannel::new(
                config,
                vec![DownlinkTransport {
                    role: TransportRole::ForwardAccess,
                    identity: 1,
                    formats: formats(),
                }],
                &[vec![0], vec![1]],
            )
            .unwrap();
            Sib5::new(settings(), vec![access()], vec![ch])
                .unwrap()
                .encode()
                .unwrap();
        }
        for offset in [1, 255, MAX_TIMING_CHIPS + 1] {
            let mut config = physical(false, 4);
            config.timing_offset_chips = offset;
            assert_eq!(
                SecondaryChannel::new(
                    config,
                    vec![DownlinkTransport {
                        role: TransportRole::ForwardAccess,
                        identity: 1,
                        formats: formats()
                    }],
                    &[vec![0]]
                )
                .err(),
                Some(CommonChannelError::Timing)
            );
        }
    }

    #[test]
    fn sib5_segments_survive_the_real_broadcast_schedule() {
        use crate::{
            broadcast::{BroadcastBlock, BroadcastTiming},
            master_information::{ReferenceTag, compile_broadcast},
        };
        use std::num::NonZeroU64;
        use umts_rrc_codec::{BchMessage, BchPayload, SegmentPart};
        use umts_types::{
            RadioEpoch,
            time::{CHIPS_PER_FRAME, ChipInstant},
        };
        let encoded = Sib5::new(settings(), vec![access()], vec![channel(true, 4)])
            .unwrap()
            .encode()
            .unwrap();
        let epoch = RadioEpoch::new(NonZeroU64::new(1).unwrap());
        let block = BroadcastBlock::new(
            SibType::Type5,
            BroadcastTiming::new(32, vec![4, 6]).unwrap(),
            &encoded.bytes,
            encoded.bit_length,
        )
        .unwrap();
        let schedule = compile_broadcast(
            epoch,
            1,
            [0, 0, 1],
            vec![0, 1],
            vec![(block, ReferenceTag::Value(1))],
        )
        .unwrap();
        for repetition in [0_u64, 32, 4096] {
            let mut reassembled = BitWriter::default();
            for (frame, part) in [
                (4, SegmentPart::First { count: 2 }),
                (6, SegmentPart::Last { index: 1 }),
            ] {
                let message = BchMessage::decode(
                    schedule
                        .message_at(ChipInstant {
                            epoch,
                            index: (frame + repetition) * CHIPS_PER_FRAME,
                        })
                        .unwrap(),
                )
                .unwrap();
                let (kind, actual, bytes, length) = match &message.payload {
                    BchPayload::Segment { kind, part, data } => (
                        *kind,
                        *part,
                        data.bytes().as_slice(),
                        umts_rrc_codec::SEGMENT_BITS,
                    ),
                    BchPayload::LastShort { kind, index, data } => (
                        *kind,
                        SegmentPart::Last { index: *index },
                        data.bytes(),
                        data.bit_length(),
                    ),
                    _ => panic!("missing SIB5 segment"),
                };
                assert_eq!(kind, SibType::Type5);
                assert_eq!(actual, part);
                if matches!(message.payload, BchPayload::LastShort { .. }) {
                    assert_eq!(length, encoded.bit_length - reassembled.length);
                }
                for bit in 0..length {
                    if reassembled.length == encoded.bit_length {
                        break;
                    }
                    reassembled.write(u64::from((bytes[bit / 8] >> (7 - bit % 8)) & 1), 1);
                }
            }
            assert_eq!(reassembled.length, encoded.bit_length);
            assert_eq!(reassembled.bytes, encoded.bytes);
        }
    }

    #[test]
    fn lists_identities_power_and_scrambling_are_bounded_before_encoding() {
        assert_eq!(
            Sib5::new(settings(), vec![], vec![channel(true, 4)]).err(),
            Some(CommonChannelError::Count)
        );
        assert_eq!(
            Sib5::new(settings(), vec![access()], vec![]).err(),
            Some(CommonChannelError::Count)
        );
        for (pich, aich) in [(-11, -10), (6, -10), (-5, -23), (-5, 6)] {
            let mut config = settings();
            config.pich_power_offset_db = pich;
            config.aich_power_offset_db = aich;
            assert_eq!(
                Sib5::new(config, vec![access()], vec![channel(true, 4)]).err(),
                Some(CommonChannelError::Power)
            );
        }
        for identity in [0, 33] {
            assert_eq!(
                SecondaryChannel::new(
                    physical(false, 4),
                    vec![DownlinkTransport {
                        role: TransportRole::ForwardAccess,
                        identity,
                        formats: formats()
                    }],
                    &[vec![0]]
                )
                .err(),
                Some(CommonChannelError::TransportIdentity)
            );
        }
        let transports = (0..2)
            .map(|_| DownlinkTransport {
                role: TransportRole::ForwardAccess,
                identity: 1,
                formats: formats(),
            })
            .collect();
        assert_eq!(
            SecondaryChannel::new(physical(false, 4), transports, &[vec![0, 0]]).err(),
            Some(CommonChannelError::TransportIdentity)
        );
        for scrambling in [0, 16] {
            let mut config = physical(false, 4);
            config.secondary_scrambling_code = Some(scrambling);
            assert_eq!(
                SecondaryChannel::new(
                    config,
                    vec![DownlinkTransport {
                        role: TransportRole::ForwardAccess,
                        identity: 1,
                        formats: formats()
                    }],
                    &[vec![0]]
                )
                .err(),
                Some(CommonChannelError::ScramblingCode)
            );
        }
        let mut config = physical(true, 4);
        config.pich.as_mut().unwrap().code256 = 2;
        let ch = SecondaryChannel::new(
            config,
            vec![DownlinkTransport {
                role: TransportRole::Paging,
                identity: 1,
                formats: formats(),
            }],
            &[vec![0], vec![1]],
        )
        .unwrap();
        assert_eq!(
            Sib5::new(settings(), vec![access()], vec![ch]).err(),
            Some(CommonChannelError::CodeCollision)
        );
    }

    #[test]
    fn paging_indicators_optional_pich_and_shared_aich_are_explicit() {
        for indicators in [
            PagingIndicators::PerFrame18,
            PagingIndicators::PerFrame36,
            PagingIndicators::PerFrame72,
            PagingIndicators::PerFrame144,
        ] {
            let mut config = physical(true, 4);
            config.pich.as_mut().unwrap().indicators = indicators;
            config.pich.as_mut().unwrap().transmit_diversity = true;
            let ch = SecondaryChannel::new(
                config,
                vec![DownlinkTransport {
                    role: TransportRole::Paging,
                    identity: 1,
                    formats: formats(),
                }],
                &[vec![0], vec![1]],
            )
            .unwrap();
            let mut base = settings();
            base.sib6_available = true;
            base.primary_ccpch_transmit_diversity = true;
            Sib5::new(base, vec![access()], vec![ch])
                .unwrap()
                .encode()
                .unwrap();
        }
        let mut config = physical(true, 4);
        config.pich = None;
        let ch = SecondaryChannel::new(
            config,
            vec![DownlinkTransport {
                role: TransportRole::Paging,
                identity: 1,
                formats: formats(),
            }],
            &[vec![0], vec![1]],
        )
        .unwrap();
        Sib5::new(settings(), vec![access()], vec![ch])
            .unwrap()
            .encode()
            .unwrap();
        let other = access_with_aich(Aich {
            code256: 2,
            transmit_diversity: false,
            timing: AichTiming::ThreeSlots,
        });
        Sib5::new(settings(), vec![access(), other], vec![channel(true, 4)])
            .unwrap()
            .encode()
            .unwrap();
        let other = access_with_aich(Aich {
            code256: 2,
            transmit_diversity: true,
            timing: AichTiming::FiveSlots,
        });
        assert_eq!(
            Sib5::new(settings(), vec![access(), other], vec![channel(true, 4)]).err(),
            Some(CommonChannelError::CodeCollision)
        );
    }
}
