// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, anyhow, bail};
use umts_config::Config;
use umts_dsp::{
    channel_codes::OvsfCode,
    code_blocks::ConvolutionalBlocks,
    convolutional::CodeRate,
    crc::TransportCrc,
    interleaving::Rel99Tti,
    prach_transport::{PrachFormat, PrachTransportPlan},
    scrambling::PrimaryScramblingCode,
};
use umts_rrc::{
    broadcast::{BroadcastBlock, BroadcastSchedule, BroadcastTiming},
    cell_selection::{CellAccess, CellBarred, CellSelection, SelectionMeasure, Sib3},
    common_channels::{
        DownlinkTransport, PagingIndicators, Pich, Position, SecondaryChannel,
        SecondaryChannelSettings, Sib5, Sib5Settings, TransportRole,
    },
    master_information::{ReferenceTag, compile_broadcast},
    random_access::{
        AccessServiceClass, Aich, AichTiming, MinimumSpreadingFactor, PrachInformation,
        PrachSettings, PuncturingLimit, Signatures, Subchannels,
    },
    system_information::{
        CsDomainInformation, NetworkMode, PsDomainInformation, Sib1, Sib2, Sib7, Sib11,
    },
    transport::{
        ChannelCoding, CommonFormatGroup, CommonTransportFormatSet, ConvolutionalRate, CrcSize,
        LogicalChannels, PrachMessagePower, TransportInterval,
    },
    ue_timers::UeTimerProfile,
};
use umts_rrc_codec::{FIXED_SIB_BITS, SEGMENT_BITS, SibType};
use umts_types::RadioEpoch;

mod tags;
pub(crate) use tags::BroadcastTags;

const MINIMUM_RX_LEVEL_DBM: i16 = -115;
const RESELECTION_HYSTERESIS_DB: u8 = 2;
const RESELECTION_SECONDS: u8 = 5;
const INTRA_SEARCH_DB: i16 = -32;
const INTER_SEARCH_DB: i16 = 16;
const SI_PERIOD_FRAMES: u16 = 32;
const MIB_PERIOD_FRAMES: u16 = 8;
const RACH_BLOCK_BITS: u16 = 256;
const RACH_FORMAT_INDICES: [usize; 2] = [0, 1];
const COMMON_CRC_BITS: usize = 12;
const COMMON_RATE_MATCHING_ATTRIBUTE: u16 = 256;
const AICH_CODE: u8 = 2;
const PICH_CODE: u8 = 7;
const PRACH_MESSAGE_CONTROL_GAIN: u8 = 15;
const PRACH_MESSAGE_DATA_GAIN: u8 = 15;
const PRACH_MESSAGE_OFFSET_DB: i8 = 2;
const PREAMBLE_RAMP_STEP_DB: u8 = 1;
const PREAMBLE_RETRANSMISSIONS: u8 = 64;
const MAX_RAMP_CYCLES: u8 = 32;
const MAX_BACKOFF: u8 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommonLayout {
    Sf64,
    Sf128,
}

impl CommonLayout {
    pub fn spreading_factor(self) -> u16 {
        match self {
            Self::Sf64 => 64,
            Self::Sf128 => 128,
        }
    }
    pub fn fach_code(self) -> u16 {
        match self {
            Self::Sf64 => 2,
            Self::Sf128 => 4,
        }
    }
    pub fn pch_code(self) -> u16 {
        self.fach_code() + 1
    }
    // TS 25.211 Table 18, QPSK with TFCI and no pilots: slot formats 8 and 6.
    pub fn data_bits_per_frame(self) -> usize {
        match self {
            Self::Sf64 => 72 * 15,
            Self::Sf128 => 38 * 15,
        }
    }
    pub fn transport_block_bits(self) -> u16 {
        match self {
            Self::Sf64 => 360,
            Self::Sf128 => 264,
        }
    }
}

pub(crate) struct IndicatorPowers {
    pub pich_offset_db: i8,
    pub aich_offset_db: i8,
}

pub struct ReferencePowers {
    pub cpich_tx_dbm: i8,
    pub uplink_interference_dbm: i16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RachFormatLayout {
    pub transport_blocks: u16,
    pub coded_bits: usize,
    pub spreading_factor: u16,
    pub physical_bits: usize,
}

pub struct CellInformation {
    plmn: [u8; 3],
    primary_code: PrimaryScramblingCode,
    layout: CommonLayout,
    sib1: Sib1,
    sib2: Option<Sib2>,
    timers: UeTimerProfile,
    sib3: Sib3,
    sib5: Sib5,
    sib7: Sib7,
    rach_formats: Vec<RachFormatLayout>,
}

impl CellInformation {
    pub fn plmn(&self) -> [u8; 3] {
        self.plmn
    }
    pub fn downlink_code_reservations(&self) -> [OvsfCode; 6] {
        // TS 25.213 5.2.1 fixes CPICH and P-CCPCH, the remaining codes match this cell's advertised channels.
        [
            (256, 0),
            (256, 1),
            (256, u16::from(AICH_CODE)),
            (256, u16::from(PICH_CODE)),
            (self.layout.spreading_factor(), self.layout.fach_code()),
            (self.layout.spreading_factor(), self.layout.pch_code()),
        ]
        .map(|(sf, index)| OvsfCode::new(sf, index).expect("validated common-channel code"))
    }
    pub fn primary_code(&self) -> PrimaryScramblingCode {
        self.primary_code
    }
    pub fn common_layout(&self) -> CommonLayout {
        self.layout
    }
    pub fn sib1(&self) -> &Sib1 {
        &self.sib1
    }
    pub fn sib2(&self) -> Option<&Sib2> {
        self.sib2.as_ref()
    }
    pub fn timers(&self) -> &UeTimerProfile {
        &self.timers
    }
    pub fn sib3(&self) -> &Sib3 {
        &self.sib3
    }
    pub fn sib5(&self) -> &Sib5 {
        &self.sib5
    }
    pub fn sib7(&self) -> &Sib7 {
        &self.sib7
    }
    pub fn rach_formats(&self) -> &[RachFormatLayout] {
        &self.rach_formats
    }
}

pub struct CellPlan {
    information: CellInformation,
    schedule: BroadcastSchedule,
}

impl CellPlan {
    pub const CURRENT_MIB_VALUE_TAG: u8 = tags::REFERENCE_MIB_VALUE_TAG;

    pub fn compile(
        config: &Config,
        epoch: RadioEpoch,
        mib_value_tag: u8,
        powers: ReferencePowers,
    ) -> Result<Self> {
        Self::compile_tagged(
            config,
            epoch,
            BroadcastTags::reference(mib_value_tag)?,
            powers,
            IndicatorPowers {
                pich_offset_db: config.power_control.downlink.pich_power_offset_db,
                aich_offset_db: config.cell.common_channels.aich_power_offset_db,
            },
        )
    }

    pub(crate) fn compile_tagged(
        config: &Config,
        epoch: RadioEpoch,
        tags: BroadcastTags,
        powers: ReferencePowers,
        indicators: IndicatorPowers,
    ) -> Result<Self> {
        config.validate().context("canonical cell configuration")?;
        let layout = if config.hspa.downlink.compact_common_channels {
            CommonLayout::Sf128
        } else {
            CommonLayout::Sf64
        };
        let primary_code =
            PrimaryScramblingCode::new(config.cell.physical.downlink_scrambling_code)
                .map_err(|e| anyhow!("primary scrambling code: {e:?}"))?;
        let assignment = crate::radio_assignment::paired_assignment(
            config.radio.uarfcn_dl,
            config.radio.uarfcn_ul,
        )?;
        let sib1 = Sib1 {
            location_area_code: config.cell.lac,
            cs: Some(CsDomainInformation {
                periodic_update_decihours: config.rrc.periodic_lu_timer_value,
                attach_detach: true,
                drx_coefficient: config.rrc.cn_drx_cycle_length_coefficient,
            }),
            ps: config.cell.gprs_enable.then_some(PsDomainInformation {
                routing_area_code: config.cell.rac,
                network_mode: match config.cell.nmo {
                    umts_config::NetworkOperationMode::CombinedPaging => NetworkMode::I,
                    umts_config::NetworkOperationMode::SeparatePaging => NetworkMode::II,
                },
                drx_coefficient: config.rrc.cn_drx_cycle_length_coefficient,
            }),
        };
        let mut timers = UeTimerProfile::release_reference();
        if let Some(connected) = timers.connected.as_mut() {
            connected.t323_seconds = config
                .rrc
                .dormancy
                .effective_fast_dormancy_inhibit_seconds();
        }
        // TS 25.331 8.6.2.1: a UE in URA_PCH that finds no SIB2 repeats URA UPDATE.
        let sib2 = (config.rrc.dormancy.enabled()
            && config.rrc.dormancy.state == umts_config::DormancyState::UraPch)
            .then_some(Sib2 {
                ura_identity: config.rrc.ura_identity,
            });
        let sib3 = Sib3 {
            cell_identity: config.cell.cell_id,
            sib4_available: false,
            selection: CellSelection {
                measure: SelectionMeasure::EcNo {
                    hysteresis2_db: Some(RESELECTION_HYSTERESIS_DB),
                },
                q_qual_min_db: i16::from(config.cell.q_qual_min),
                q_rxlev_min_dbm: MINIMUM_RX_LEVEL_DBM,
                hysteresis1_db: RESELECTION_HYSTERESIS_DB,
                reselection_seconds: RESELECTION_SECONDS,
                max_uplink_power_dbm: i16::from(config.rrc.max_allowed_ul_tx_power_dbm),
                intra_search_db: Some(INTRA_SEARCH_DB),
                inter_search_db: Some(INTER_SEARCH_DB),
                hcs_search_db: None,
            },
            access: CellAccess {
                barred: CellBarred::NotBarred,
                reserved_for_operator: false,
                reservation_extension: false,
                barred_classes: Some([false; 16]),
            },
        };
        let access = PrachInformation::new(
            PrachSettings {
                signatures: Signatures::new(&[config.cell.access.prach.signature])
                    .map_err(|e| anyhow!("PRACH signature: {e:?}"))?,
                subchannels: Subchannels::new(&[config.cell.access.prach.subchannel])
                    .map_err(|e| anyhow!("PRACH subchannel: {e:?}"))?,
                minimum_spreading_factor: MinimumSpreadingFactor::Sf32,
                preamble_scrambling_code: config.cell.access.prach.scrambling_code_word_number,
                puncturing_limit: PuncturingLimit::from_percent(100)
                    .map_err(|e| anyhow!("PRACH puncturing limit: {e:?}"))?,
                transport_identity: 1,
                access_classes: vec![AccessServiceClass {
                    first_signature_index: 0,
                    last_signature_index: 0,
                    assigned_subchannels: 15,
                }],
                ac_to_asc: [0; 7],
                persistence_factors: None,
                primary_cpich_dbm: powers.cpich_tx_dbm,
                initial_power_constant_db: config.cell.access.prach.initial_power_constant_db,
                ramp_step_db: PREAMBLE_RAMP_STEP_DB,
                preamble_retransmissions: PREAMBLE_RETRANSMISSIONS,
                maximum_ramp_cycles: MAX_RAMP_CYCLES,
                backoff_min: 0,
                backoff_max: MAX_BACKOFF,
                aich: Aich {
                    code256: AICH_CODE,
                    transmit_diversity: false,
                    timing: AichTiming::FiveSlots,
                },
            },
            formats(
                RACH_BLOCK_BITS,
                CrcSize::Bits16,
                vec![1, 2],
                LogicalChannels::Configured,
            ),
            &RACH_FORMAT_INDICES,
            vec![
                PrachMessagePower::new(
                    PRACH_MESSAGE_CONTROL_GAIN,
                    PRACH_MESSAGE_DATA_GAIN,
                    PRACH_MESSAGE_OFFSET_DB
                )
                .map_err(|e| anyhow!("PRACH message power: {e:?}"))?;
                RACH_FORMAT_INDICES.len()
            ],
        )
        .map_err(|e| anyhow!("PRACH information: {e:?}"))?;
        let rach_formats = rach_layout(&access)?;
        let block_bits = layout.transport_block_bits();
        let common_coded =
            ConvolutionalBlocks::new(usize::from(block_bits) + COMMON_CRC_BITS, CodeRate::Half)
                .map_err(|e| anyhow!("common-channel coding: {e:?}"))?
                .coded_bits();
        if common_coded > layout.data_bits_per_frame() {
            bail!(
                "common TFS needs {common_coded} coded bits but physical channel has {}",
                layout.data_bits_per_frame()
            );
        }
        let paging = secondary(layout, TransportRole::Paging)?;
        let forward = secondary(layout, TransportRole::ForwardAccess)?;
        let sib5 = Sib5::new(
            Sib5Settings {
                frequency_band: assignment.band,
                sib6_available: false,
                pich_power_offset_db: indicators.pich_offset_db,
                aich_power_offset_db: indicators.aich_offset_db,
                primary_ccpch_transmit_diversity: false,
            },
            vec![access],
            vec![paging, forward],
        )
        .map_err(|e| anyhow!("SIB5 common-channel plan: {e:?}"))?;
        let sib7 = Sib7 {
            uplink_interference_dbm: powers.uplink_interference_dbm,
            sib5_persistence: vec![config.cell.access.prach.dynamic_persistence_level],
            sib6_persistence: None,
            expiration_time_factor: Some(1),
        };
        let sib11 = Sib11 {
            selection_measure: sib3.selection.measure,
        };
        let mut encoded = vec![
            (
                SibType::Type1,
                sib1.encode_with_timers(&timers)
                    .map_err(|e| anyhow!("SIB1: {e:?}"))?,
                ReferenceTag::Value(tags.sib1()),
            ),
            (
                SibType::Type3,
                sib3.encode().map_err(|e| anyhow!("SIB3: {e:?}"))?,
                ReferenceTag::Value(tags.sib3()),
            ),
            (
                SibType::Type5,
                sib5.encode().map_err(|e| anyhow!("SIB5: {e:?}"))?,
                ReferenceTag::Value(tags.sib5()),
            ),
            (
                SibType::Type7,
                sib7.encode().map_err(|e| anyhow!("SIB7: {e:?}"))?,
                ReferenceTag::None,
            ),
            (
                SibType::Type11,
                sib11.encode().map_err(|e| anyhow!("SIB11: {e:?}"))?,
                ReferenceTag::Value(tags.sib11()),
            ),
        ];
        // SIB2 takes the positions after the other blocks, which keep theirs in every configuration.
        if let Some(sib2) = &sib2 {
            encoded.push((
                SibType::Type2,
                sib2.encode().map_err(|e| anyhow!("SIB2: {e:?}"))?,
                ReferenceTag::Value(tags.sib2()),
            ));
        }
        let mut slots = (0..SI_PERIOD_FRAMES)
            .step_by(2)
            .filter(|frame| frame % MIB_PERIOD_FRAMES != 0);
        let mut blocks = Vec::with_capacity(encoded.len());
        for (kind, value, tag) in encoded {
            let segments = if value.bit_length <= FIXED_SIB_BITS {
                1
            } else {
                value.bit_length.div_ceil(SEGMENT_BITS)
            };
            let positions = slots.by_ref().take(segments).collect::<Vec<_>>();
            if positions.len() != segments {
                bail!("system information exceeds the available broadcast schedule");
            }
            let timing = BroadcastTiming::new(SI_PERIOD_FRAMES, positions)
                .map_err(|e| anyhow!("broadcast timing: {e:?}"))?;
            let block = BroadcastBlock::new(kind, timing, &value.bytes, value.bit_length)
                .map_err(|e| anyhow!("{kind:?} segmentation: {e:?}"))?;
            blocks.push((block, tag));
        }
        let (mcc, mnc) = config
            .cell
            .plmn
            .split_once('-')
            .context("PLMN missing separator")?;
        let digits = mcc.as_bytes();
        let mcc = [digits[0] - b'0', digits[1] - b'0', digits[2] - b'0'];
        let mnc = mnc.bytes().map(|digit| digit - b'0').collect();
        let schedule = compile_broadcast(epoch, tags.mib(), mcc, mnc, blocks)
            .map_err(|e| anyhow!("MIB and SI schedule: {e:?}"))?;
        Ok(Self {
            information: CellInformation {
                plmn: umts_config::parse_plmn(&config.cell.plmn)?,
                primary_code,
                layout,
                sib1,
                sib2,
                timers,
                sib3,
                sib5,
                sib7,
                rach_formats,
            },
            schedule,
        })
    }

    pub fn information(&self) -> &CellInformation {
        &self.information
    }
    pub fn schedule(&self) -> &BroadcastSchedule {
        &self.schedule
    }
    pub fn into_parts(self) -> (CellInformation, BroadcastSchedule) {
        (self.information, self.schedule)
    }
}

fn formats(
    bits: u16,
    crc: CrcSize,
    counts: Vec<u16>,
    logical: LogicalChannels,
) -> CommonTransportFormatSet {
    CommonTransportFormatSet {
        interval: TransportInterval::Ms10,
        coding: ChannelCoding::Convolutional(ConvolutionalRate::Half),
        rate_matching_attribute: COMMON_RATE_MATCHING_ATTRIBUTE,
        crc,
        groups: vec![CommonFormatGroup {
            transport_block_bits: bits,
            block_counts: counts,
            logical_channels: logical,
        }],
    }
}

fn secondary(layout: CommonLayout, role: TransportRole) -> Result<SecondaryChannel> {
    let paging = role == TransportRole::Paging;
    let index = if paging {
        layout.pch_code()
    } else {
        layout.fach_code()
    };
    let code = OvsfCode::new(layout.spreading_factor(), index)
        .map_err(|e| anyhow!("S-CCPCH code: {e:?}"))?;
    SecondaryChannel::new(
        SecondaryChannelSettings {
            code,
            secondary_scrambling_code: None,
            transmit_diversity: false,
            pilots: false,
            tfci: true,
            position: Position::Flexible,
            timing_offset_chips: 0,
            pich: paging.then_some(Pich {
                code256: PICH_CODE,
                indicators: PagingIndicators::PerFrame18,
                transmit_diversity: false,
            }),
        },
        vec![DownlinkTransport {
            role,
            identity: 1,
            formats: formats(
                layout.transport_block_bits(),
                CrcSize::Bits12,
                vec![0, 1],
                LogicalChannels::AllSizes,
            ),
        }],
        &[vec![0], vec![1]],
    )
    .map_err(|e| anyhow!("S-CCPCH transport plan: {e:?}"))
}

fn rach_layout(access: &PrachInformation) -> Result<Vec<RachFormatLayout>> {
    let transport = access.transport();
    let settings = access.settings();
    let interval = match transport.interval {
        TransportInterval::Ms10 => Rel99Tti::Ms10,
        TransportInterval::Ms20 => Rel99Tti::Ms20,
        _ => bail!("RACH requires a 10 or 20 ms transport interval"),
    };
    let rate = match transport.coding {
        ChannelCoding::Convolutional(ConvolutionalRate::Half) => CodeRate::Half,
        ChannelCoding::Convolutional(ConvolutionalRate::Third) => CodeRate::Third,
        _ => bail!("RACH requires convolutional coding"),
    };
    let crc = match transport.crc {
        CrcSize::None => TransportCrc::None,
        CrcSize::Bits8 => TransportCrc::Crc8,
        CrcSize::Bits12 => TransportCrc::Crc12,
        CrcSize::Bits16 => TransportCrc::Crc16,
        CrcSize::Bits24 => TransportCrc::Crc24,
    };
    transport
        .formats()
        .map(|(bits, blocks)| {
            let plan = PrachTransportPlan::new(
                PrachFormat {
                    block_bits: usize::from(bits),
                    blocks: usize::from(blocks),
                    crc,
                    rate,
                    interval,
                },
                settings.minimum_spreading_factor.chips(),
                settings.puncturing_limit.percent(),
            )
            .map_err(|e| anyhow!("RACH physical transport plan: {e:?}"))?;
            Ok(RachFormatLayout {
                transport_blocks: blocks,
                coded_bits: plan.coded_bits(),
                spreading_factor: plan.spreading_factor(),
                physical_bits: plan.frame_bits(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroU64, path::PathBuf};
    use umts_rrc_codec::{BchMessage, BchPayload, SHORT_SIB_BITS, SegmentPart};
    use umts_types::time::{CHIPS_PER_FRAME, ChipInstant, SYSTEM_FRAME_MODULUS};

    fn config() -> Config {
        Config::load(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../rust/test-support/fixtures/config/serving.yaml"),
        )
        .unwrap()
    }

    fn epoch() -> RadioEpoch {
        RadioEpoch::new(NonZeroU64::new(1).unwrap())
    }

    fn compile(config: &Config) -> CellPlan {
        CellPlan::compile(
            config,
            epoch(),
            CellPlan::CURRENT_MIB_VALUE_TAG,
            ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -100,
            },
        )
        .unwrap()
    }

    #[test]
    fn both_common_layouts_fit_and_match_the_advertised_transports() {
        for (compact, expected, capacity, bits, coded) in [
            (false, CommonLayout::Sf64, 1080, 360, 760),
            (true, CommonLayout::Sf128, 570, 264, 568),
        ] {
            let mut config = config();
            config.hspa.downlink.compact_common_channels = compact;
            let plan = compile(&config);
            let cell = plan.information();
            assert_eq!(cell.common_layout(), expected);
            assert_eq!(expected.data_bits_per_frame(), capacity);
            assert_eq!(
                ConvolutionalBlocks::new(usize::from(bits) + COMMON_CRC_BITS, CodeRate::Half)
                    .unwrap()
                    .coded_bits(),
                coded
            );
            assert!(coded <= capacity);
            let channels = cell.sib5().secondary_channels();
            assert_eq!(channels.len(), 2);
            for (channel, role, code) in [
                (&channels[0], TransportRole::Paging, expected.pch_code()),
                (
                    &channels[1],
                    TransportRole::ForwardAccess,
                    expected.fach_code(),
                ),
            ] {
                assert_eq!(channel.settings().code.index(), code);
                assert_eq!(
                    channel.settings().code.spreading_factor(),
                    expected.spreading_factor()
                );
                assert_eq!(
                    channel.settings().pich.is_some(),
                    role == TransportRole::Paging
                );
                assert_eq!(channel.transports()[0].role, role);
                assert_eq!(
                    channel.transports()[0].formats.groups[0].transport_block_bits,
                    bits
                );
                assert_eq!(
                    channel.transports()[0].formats.groups[0].block_counts,
                    [0, 1]
                );
            }
        }
    }

    #[test]
    fn advertised_non_sttd_pccpch_matches_both_signed_synchronization_channels() {
        use num_complex::Complex32;
        use umts_dsp::{
            bits::Bit,
            broadcast::FRAME_BITS,
            common_downlink::{CommonChannelAmplitudes, CommonDownlink},
            scrambling::downlink_sequence,
            synchronization,
        };
        use umts_rrc_schema::information_elements as ie;
        use umts_types::time::{CHIPS_PER_SLOT, SLOTS_PER_FRAME};

        const CPICH_AMPLITUDE: f32 = 0.25;
        const SCH_AMPLITUDE: f32 = 0.5;
        const CHIP_TOLERANCE: f32 = 1e-6;

        for compact in [false, true] {
            for psc in [0, 42, 511] {
                let mut config = config();
                config.hspa.downlink.compact_common_channels = compact;
                config.cell.physical.downlink_scrambling_code = psc;
                let plan = compile(&config);
                let cell = plan.information();
                assert!(!cell.sib5().settings().primary_ccpch_transmit_diversity);
                let encoded = cell.sib5().encode().unwrap();
                let advertised: ie::SysInfoType5 = rasn::uper::decode(&encoded.bytes).unwrap();
                assert!(matches!(advertised.primary_ccpch_info,
                    Some(ie::PrimaryCCPCHInfo::fdd(value)) if !value.tx_diversity_indicator));
                let mut scrambling = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
                downlink_sequence(cell.primary_code().sequence_number(), &mut scrambling).unwrap();
                let allocation =
                    synchronization::secondary_allocation(cell.primary_code().group()).unwrap();
                for primary in [true, false] {
                    let mut chips = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
                    CommonDownlink::new(
                        cell.primary_code(),
                        CommonChannelAmplitudes {
                            cpich: CPICH_AMPLITUDE,
                            pccpch: 0.125,
                            primary_sch: if primary { SCH_AMPLITUDE } else { 0.0 },
                            secondary_sch: if primary { 0.0 } else { SCH_AMPLITUDE },
                        },
                    )
                    .unwrap()
                    .compose_frame(&[Bit::Zero; FRAME_BITS], &mut chips)
                    .unwrap();
                    for (slot, &number) in
                        allocation.iter().enumerate().take(SLOTS_PER_FRAME as usize)
                    {
                        let raw = if primary {
                            synchronization::primary()
                        } else {
                            synchronization::secondary(number).unwrap()
                        };
                        assert_eq!(raw[0], Complex32::new(1.0, 1.0));
                        let start = slot * CHIPS_PER_SLOT as usize;
                        for (chip, &code) in raw.iter().enumerate() {
                            let pilot = scrambling[start + chip]
                                * Complex32::new(CPICH_AMPLITUDE * 0.5, CPICH_AMPLITUDE * 0.5);
                            let sch = chips[start + chip] - pilot;
                            // TS 25.211 5.3.3.5 assigns a=-1 to both SCHs when P-CCPCH is not STTD encoded.
                            let expected =
                                -code * (SCH_AMPLITUDE * std::f32::consts::FRAC_1_SQRT_2);
                            assert!(
                                (sch - expected).norm() < CHIP_TOLERANCE,
                                "PSC={psc}, compact={compact}, primary={primary}, slot={slot}, chip={chip}: {sch:?} != {expected:?}"
                            );
                            if chip == 0 {
                                assert!(sch.re < 0.0 && sch.im < 0.0);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rach_spreading_accounts_for_separate_terminated_code_blocks() {
        let plan = compile(&config());
        assert_eq!(
            plan.information().rach_formats(),
            &[
                RachFormatLayout {
                    transport_blocks: 1,
                    coded_bits: 560,
                    spreading_factor: 64,
                    physical_bits: 600
                },
                RachFormatLayout {
                    transport_blocks: 2,
                    coded_bits: 1120,
                    spreading_factor: 32,
                    physical_bits: 1200
                },
            ]
        );
        let access = &plan.information().sib5().prach()[0];
        assert!(
            access
                .settings()
                .signatures
                .contains(config().cell.access.prach.signature)
        );
        assert!(
            access
                .settings()
                .subchannels
                .contains(config().cell.access.prach.subchannel)
        );
        assert_eq!(access.settings().primary_cpich_dbm, 10);
    }

    #[test]
    fn prach_power_constant_uses_configured_value_and_standard_bounds() {
        for value in [-35, -20, -10] {
            let mut config = config();
            config.cell.access.prach.initial_power_constant_db = value;
            config.cell.access.prach.validate_schema().unwrap();
            let plan = compile(&config);
            assert_eq!(
                plan.information().sib5().prach()[0]
                    .settings()
                    .initial_power_constant_db,
                value
            );
        }
        for value in [-36, -9] {
            let mut config = config();
            config.cell.access.prach.initial_power_constant_db = value;
            assert!(config.cell.access.prach.validate_schema().is_err());
        }
    }

    #[test]
    fn named_network_modes_preserve_sib1_wire_values() {
        for (mode, expected) in [
            (umts_config::NetworkOperationMode::CombinedPaging, 0),
            (umts_config::NetworkOperationMode::SeparatePaging, 1),
        ] {
            let mut config = config();
            config.cell.nmo = mode;
            let plan = compile(&config);
            let ps = plan.information().sib1().ps.as_ref().unwrap();
            assert_eq!(ps.network_mode as u8, expected);
        }
    }

    #[test]
    fn canonical_identity_domains_and_selection_are_owned_snapshots() {
        let mut config = config();
        config.cell.cell_id = 0x0abc_def1;
        config.cell.nmo = umts_config::NetworkOperationMode::CombinedPaging;
        config.cell.q_qual_min = -18;
        let plan = compile(&config);
        let cell = plan.information();
        assert_eq!(cell.sib1().location_area_code, config.cell.lac);
        let cs = cell.sib1().cs.as_ref().unwrap();
        assert_eq!(
            cs.periodic_update_decihours,
            config.rrc.periodic_lu_timer_value
        );
        assert_eq!(
            cs.drx_coefficient,
            config.rrc.cn_drx_cycle_length_coefficient
        );
        let ps = cell.sib1().ps.as_ref().unwrap();
        assert_eq!(ps.routing_area_code, config.cell.rac);
        assert!(matches!(ps.network_mode, NetworkMode::I));
        assert_eq!(cell.sib3().cell_identity, 0x0abc_def1);
        assert_eq!(cell.sib3().selection.q_qual_min_db, -18);
        assert!(!cell.sib3().sib4_available);
        assert_eq!(
            cell.sib7().sib5_persistence,
            [config.cell.access.prach.dynamic_persistence_level]
        );
        assert_eq!(cell.sib7().uplink_interference_dbm, -100);
        assert!(cell.sib7().sib6_persistence.is_none());
        config.cell.lac += 1;
        config.cell.cell_id = 1;
        assert_ne!(cell.sib1().location_area_code, config.cell.lac);
        assert_ne!(cell.sib3().cell_identity, config.cell.cell_id);
    }

    #[test]
    fn disabling_packet_service_removes_only_the_ps_domain() {
        let mut config = config();
        config.cell.gprs_enable = false;
        let plan = compile(&config);
        assert!(plan.information().sib1().ps.is_none());
        assert!(plan.information().sib1().cs.is_some());
    }

    #[test]
    fn configured_band_is_shared_by_radio_assignment_and_sib5() {
        for (downlink, uplink, band) in [(4400, 4175, 5), (10637, 9687, 1), (3012, 2787, 8)] {
            let mut config = config();
            config.radio.uarfcn_dl = downlink;
            config.radio.uarfcn_ul = uplink;
            let plan = compile(&config);
            assert_eq!(
                plan.information().sib5().settings().frequency_band.number(),
                band
            );
        }
        let plan = compile(&config());
        assert_eq!(plan.information().sib5().encode().unwrap().bit_length, 476);
        let message = BchMessage::decode(
            plan.schedule()
                .message_at(ChipInstant {
                    epoch: epoch(),
                    index: 12 * CHIPS_PER_FRAME,
                })
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            message.payload,
            BchPayload::LastShort {
                kind: SibType::Type5,
                index: 2,
                data,
            } if data.bit_length() == 32
        ));
    }

    #[test]
    fn full_si_cycle_preserves_ordered_segments_and_sfn_wrap() {
        let plan = compile(&config());
        let mut actual = Vec::new();
        for frame in (0..SI_PERIOD_FRAMES).step_by(2) {
            let message = BchMessage::decode(
                plan.schedule()
                    .message_at(ChipInstant {
                        epoch: epoch(),
                        index: u64::from(frame) * CHIPS_PER_FRAME,
                    })
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(message.system_frame, frame);
            let wrapped = BchMessage::decode(
                plan.schedule()
                    .message_at(ChipInstant {
                        epoch: epoch(),
                        index: (SYSTEM_FRAME_MODULUS + u64::from(frame)) * CHIPS_PER_FRAME,
                    })
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(message, wrapped);
            match message.payload {
                BchPayload::CompleteShort { kind, data } => {
                    assert!(data.bit_length() <= SHORT_SIB_BITS);
                    if kind == SibType::Type11 {
                        assert_eq!(data.bit_length(), 10);
                        assert_eq!(data.bytes(), [0x01, 0x00]);
                    }
                    actual.push((frame, kind, None));
                }
                BchPayload::Complete { kind, .. } => {
                    panic!("unexpected fixed-size complete SIB in short-SIB fixture: {kind:?}");
                }
                BchPayload::Segment { kind, part, .. } => actual.push((frame, kind, Some(part))),
                BchPayload::LastShort { kind, index, data } => {
                    assert!(data.bit_length() <= SHORT_SIB_BITS);
                    actual.push((frame, kind, Some(SegmentPart::Last { index })));
                }
                BchPayload::NoSegment => {}
            }
        }
        let non_mib: Vec<_> = actual
            .into_iter()
            .filter(|(_, kind, _)| *kind != SibType::MasterInformationBlock)
            .collect();
        assert_eq!(
            non_mib,
            [
                (2, SibType::Type1, None),
                (4, SibType::Type3, None),
                (6, SibType::Type5, Some(SegmentPart::First { count: 3 })),
                (
                    10,
                    SibType::Type5,
                    Some(SegmentPart::Subsequent { index: 1 })
                ),
                (12, SibType::Type5, Some(SegmentPart::Last { index: 2 })),
                (14, SibType::Type7, None),
                (18, SibType::Type11, None),
            ]
        );
    }

    fn cycle(plan: &CellPlan) -> Vec<BchMessage> {
        (0..SI_PERIOD_FRAMES)
            .step_by(2)
            .map(|frame| {
                BchMessage::decode(
                    plan.schedule()
                        .message_at(ChipInstant {
                            epoch: epoch(),
                            index: u64::from(frame) * CHIPS_PER_FRAME,
                        })
                        .unwrap(),
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn dormancy_settings_change_nothing_on_air_while_parking_is_disabled() {
        let disabled = || {
            let mut config = config();
            config.rrc.dormancy.after_seconds = 0;
            config
        };
        let reference = compile(&disabled());
        assert!(reference.information().sib2().is_none());
        let connected = reference.information().timers().connected.as_ref().unwrap();
        assert_eq!(connected.t323_seconds, None);
        let mut config = disabled();
        config.rrc.dormancy.state = umts_config::DormancyState::UraPch;
        config.rrc.dormancy.drx_cycle_length_coefficient = 4;
        config.rrc.dormancy.fast_dormancy_inhibit_seconds = Some(30);
        let plan = compile(&config);
        assert!(plan.information().sib2().is_none());
        assert_eq!(cycle(&plan), cycle(&reference));
    }

    #[test]
    fn enabled_dormancy_broadcasts_the_inhibit_timer_and_sib2_only_for_ura_pch() {
        const SIB2_FRAME: u16 = 20;
        let mut config = config();
        config.rrc.dormancy.after_seconds = 0;
        let reference = cycle(&compile(&config));
        config.rrc.dormancy.after_seconds = 5;
        let cell_pch = compile(&config);
        assert!(cell_pch.information().sib2().is_none());
        let connected = cell_pch.information().timers().connected.as_ref().unwrap();
        assert_eq!(connected.t323_seconds, Some(5));
        let sib1 = cell_pch
            .information()
            .sib1()
            .encode_with_timers(cell_pch.information().timers())
            .unwrap();
        let sib1: umts_rrc_schema::information_elements::SysInfoType1 =
            rasn::uper::decode(&sib1.bytes).unwrap();
        assert_eq!(
            sib1.v3a0_non_critical_extensions
                .unwrap()
                .v860_non_critical_extentions
                .unwrap()
                .sys_info_type1_v860ext
                .ue_conn_timers_and_constants
                .t_323,
            Some(umts_rrc_schema::information_elements::T323::s5)
        );
        let parked = cycle(&cell_pch);
        for (index, (actual, expected)) in parked.iter().zip(&reference).enumerate() {
            let sib1 = matches!(
                actual.payload,
                BchPayload::CompleteShort {
                    kind: SibType::Type1,
                    ..
                } | BchPayload::Complete {
                    kind: SibType::Type1,
                    ..
                }
            );
            assert_eq!(actual == expected, !sib1, "position {index}");
        }

        config.rrc.dormancy.state = umts_config::DormancyState::UraPch;
        config.rrc.dormancy.fast_dormancy_inhibit_seconds = Some(0);
        config.rrc.ura_identity = 0xa5c3;
        let ura_pch = compile(&config);
        assert_eq!(
            ura_pch.information().sib2().map(|sib2| sib2.ura_identity),
            Some(0xa5c3)
        );
        let connected = ura_pch.information().timers().connected.as_ref().unwrap();
        assert_eq!(connected.t323_seconds, Some(0));
        let mut found = Vec::new();
        for (message, expected) in cycle(&ura_pch).iter().zip(&reference) {
            match &message.payload {
                BchPayload::CompleteShort {
                    kind: SibType::Type2,
                    data,
                } => {
                    assert_eq!(data.bit_length(), 20);
                    assert_eq!(data.bytes(), [0x0a, 0x5c, 0x30]);
                    assert!(matches!(expected.payload, BchPayload::NoSegment));
                    found.push(message.system_frame);
                }
                BchPayload::CompleteShort {
                    kind: SibType::Type1 | SibType::MasterInformationBlock,
                    ..
                }
                | BchPayload::Complete {
                    kind: SibType::Type1 | SibType::MasterInformationBlock,
                    ..
                }
                | BchPayload::Segment {
                    kind: SibType::MasterInformationBlock,
                    ..
                }
                | BchPayload::LastShort {
                    kind: SibType::MasterInformationBlock,
                    ..
                } => {}
                _ => assert_eq!(message, expected),
            }
        }
        assert_eq!(found, [SIB2_FRAME]);
    }

    #[test]
    fn advertised_paging_and_forward_channels_compose_and_recover_their_transports() {
        use num_complex::Complex32;
        use umts_dsp::{
            bits::{Bit, TxBit},
            broadcast::FRAME_BITS,
            common_downlink::{CommonChannelAmplitudes, CommonDownlink},
            convolutional::TerminatedDecoder,
            crc::TransportCrc,
            downlink_transport::{ConvolutionalDownlink, ConvolutionalFormat, FlexibleCommonFrame},
            interleaving::{Interleaver, Rel99Tti},
            rate_matching::RateMatching,
            scrambling::downlink_sequence,
            secondary_downlink::SecondaryDownlink,
            tfci::Tfci,
        };
        for compact in [false, true] {
            let mut config = config();
            config.hspa.downlink.compact_common_channels = compact;
            let plan = compile(&config);
            let cell = plan.information();
            let mut chips = vec![Complex32::default(); CHIPS_PER_FRAME as usize];
            CommonDownlink::new(
                cell.primary_code(),
                CommonChannelAmplitudes {
                    cpich: 0.2,
                    pccpch: 0.1,
                    primary_sch: 0.0,
                    secondary_sch: 0.0,
                },
            )
            .unwrap()
            .compose_frame(&[Bit::Zero; FRAME_BITS], &mut chips)
            .unwrap();
            let mut payloads = Vec::new();
            for (index, channel) in cell.sib5().secondary_channels().iter().enumerate() {
                let tfs = &channel.transports()[0].formats;
                assert!(matches!(tfs.interval, TransportInterval::Ms10));
                assert!(matches!(tfs.crc, CrcSize::Bits12));
                assert!(matches!(
                    tfs.coding,
                    ChannelCoding::Convolutional(ConvolutionalRate::Half)
                ));
                assert_eq!(channel.combinations().ctfc_values(), [0, 1]);
                let physical = SecondaryDownlink::new(
                    cell.primary_code(),
                    channel.settings().code,
                    channel.settings().tfci,
                    0.3,
                )
                .unwrap();
                let capacity = physical.format().data_bits_per_frame();
                assert_eq!(capacity, cell.common_layout().data_bits_per_frame());
                let block_bits = usize::from(tfs.groups[0].transport_block_bits);
                let payload: Vec<_> = (0..block_bits)
                    .map(|i| Bit::from((i * 7 + index * 3 + i / 5) % 11 < 5))
                    .collect();
                let mut transport = ConvolutionalDownlink::new(ConvolutionalFormat {
                    block_bits,
                    blocks: 1,
                    crc: TransportCrc::Crc12,
                    rate: CodeRate::Half,
                    interval: Rel99Tti::Ms10,
                    rate_matched_bits: capacity,
                })
                .unwrap();
                let mut matched = vec![Bit::Zero; capacity];
                transport.encode(&payload, &mut matched).unwrap();
                let mut data = vec![TxBit::Dtx; capacity];
                FlexibleCommonFrame::new(capacity)
                    .unwrap()
                    .compose(&[&matched], &mut data)
                    .unwrap();
                physical
                    .add_frame(&data, Some(Tfci::new(1).unwrap()), &mut chips)
                    .unwrap();
                payloads.push(payload);
            }
            let mut scrambling = vec![Complex32::default(); chips.len()];
            downlink_sequence(cell.primary_code().sequence_number(), &mut scrambling).unwrap();
            for (channel, payload) in cell.sib5().secondary_channels().iter().zip(payloads) {
                let sf = usize::from(channel.settings().code.spreading_factor());
                let mut ovsf = vec![0.0; sf];
                channel.settings().code.write(&mut ovsf).unwrap();
                let mut data = Vec::new();
                let mut control = Vec::new();
                let reserved = if sf >= 128 { 2 } else { 8 };
                for slot in 0..15 {
                    for symbol in 0..2560 / sf {
                        let start = slot * 2560 + symbol * sf;
                        let value = (0..sf)
                            .map(|chip| {
                                chips[start + chip] * scrambling[start + chip].conj() * ovsf[chip]
                            })
                            .sum::<Complex32>();
                        for (branch, value) in [value.re, value.im].into_iter().enumerate() {
                            if symbol * 2 + branch < reserved {
                                control.push(value);
                            } else {
                                data.push(value);
                            }
                        }
                    }
                }
                for (index, value) in control.iter().enumerate() {
                    assert_eq!(
                        *value < 0.0,
                        Tfci::new(1).unwrap().encode()[index % 32] == Bit::One
                    );
                }
                let mut matched = vec![0.0; data.len()];
                Interleaver::second(data.len())
                    .unwrap()
                    .deinterleave(&data, &mut matched)
                    .unwrap();
                let coded_bits = (payload.len() + 12 + 8) * 2;
                let mut coded = vec![0.0; coded_bits];
                RateMatching::downlink_convolutional_flexible(coded_bits, matched.len())
                    .unwrap()
                    .combine_llrs(&matched, &mut coded)
                    .unwrap();
                let mut decoded = vec![Bit::Zero; payload.len() + 12];
                TerminatedDecoder::new(CodeRate::Half)
                    .decode(&coded, &mut decoded)
                    .unwrap();
                assert_eq!(&decoded[..payload.len()], payload);
                assert!(
                    TransportCrc::Crc12
                        .verify(&decoded[..payload.len()], &decoded[payload.len()..])
                        .unwrap()
                );
            }
        }
    }

    #[test]
    fn invalid_configuration_and_uncalibrated_power_ranges_fail_closed() {
        for powers in [
            ReferencePowers {
                cpich_tx_dbm: 51,
                uplink_interference_dbm: -100,
            },
            ReferencePowers {
                cpich_tx_dbm: 10,
                uplink_interference_dbm: -111,
            },
        ] {
            assert!(CellPlan::compile(&config(), epoch(), 1, powers).is_err());
        }
        let mut invalid = config();
        invalid.cell.plmn = "broken".into();
        assert!(
            CellPlan::compile(
                &invalid,
                epoch(),
                1,
                ReferencePowers {
                    cpich_tx_dbm: 10,
                    uplink_interference_dbm: -100
                }
            )
            .is_err()
        );
    }
}
