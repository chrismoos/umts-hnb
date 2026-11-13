// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    EncodedInformation,
    bits::BitWriter,
    transport::{
        ChannelCoding, CommonTransportFormatSet, PrachMessagePower, TransportCombinations,
        TransportError, TransportInterval,
    },
};
use umts_rrc_codec::{CodecError, RrcValueKind, validate_rrc_value};

const SIGNATURE_COUNT: u8 = 16;
const SUBCHANNEL_COUNT: u8 = 12;
const MAX_ASC: usize = 8;
const MAPPING_COUNT: usize = 7;
const MAX_PERSISTENCE_FACTORS: usize = 6;
const SIGNATURE_BITS: usize = 16;
const SUBCHANNEL_BITS: usize = 12;
const PREAMBLE_CODE_BITS: usize = 4;
const SF_BITS: usize = 2;
const PUNCTURING_BITS: usize = 4;
const TRCH_ID_BITS: usize = 5;
const ASC_COUNT_BITS: usize = 3;
const SIGNATURE_INDEX_BITS: usize = 4;
const ASSIGNMENT_BITS: usize = 4;
const ASC_MAPPING_BITS: usize = 3;
const PERSISTENCE_BITS: usize = 3;
const CPICH_POWER_BITS: usize = 6;
const CONSTANT_BITS: usize = 5;
const POWER_STEP_BITS: usize = 3;
const RETRANSMISSION_BITS: usize = 6;
const RAMP_CYCLES_BITS: usize = 5;
const BACKOFF_BITS: usize = 6;
const AICH_CODE_BITS: usize = 8;
const THREE_SLOT_CHIPS: u32 = 7680;
const FIVE_SLOT_CHIPS: u32 = 12800;
const THREE_SLOT_MESSAGE_CHIPS: u32 = 15360;
const FIVE_SLOT_MESSAGE_CHIPS: u32 = 20480;
const MAX_COMBINATIONS: usize = 1024;
const MAX_TRCH_ID: u8 = 32;
const MIN_CPICH_DBM: i8 = -10;
const MAX_CPICH_DBM: i8 = 50;
const MIN_POWER_CONSTANT_DB: i8 = -35;
const MAX_POWER_CONSTANT_DB: i8 = -10;
const MAX_RAMP_STEP_DB: u8 = 8;
const MAX_PREAMBLE_RETRANSMISSIONS: u8 = 64;
const MAX_RAMP_CYCLES: u8 = 32;
const MAX_BACKOFF: u8 = 50;
const FIRST_NON_BROADCAST_CODE: u8 = 2;
const FDD_OPTIONAL_FIELDS: usize = 5;
const MIN_PUNCTURING_PERCENT: u8 = 40;
const MAX_PUNCTURING_PERCENT: u8 = 100;
const PUNCTURING_STEP_PERCENT: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessError {
    Signatures,
    Subchannels,
    PreambleCode,
    TransportIdentity,
    TransportProfile,
    Partition,
    Mapping,
    Persistence,
    Power,
    Retransmission,
    AichCode,
    Transport(TransportError),
    Codec(CodecError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signatures(u16);

impl Signatures {
    pub fn new(numbers: &[u8]) -> Result<Self, AccessError> {
        checked_mask(numbers, SIGNATURE_COUNT)
            .map(Self)
            .ok_or(AccessError::Signatures)
    }

    pub fn contains(self, number: u8) -> bool {
        number < SIGNATURE_COUNT && self.0 & (1 << number) != 0
    }

    pub fn count(self) -> u8 {
        self.0.count_ones() as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subchannels(u16);

impl Subchannels {
    pub fn new(numbers: &[u8]) -> Result<Self, AccessError> {
        checked_mask(numbers, SUBCHANNEL_COUNT)
            .map(Self)
            .ok_or(AccessError::Subchannels)
    }

    pub fn contains(self, number: u8) -> bool {
        number < SUBCHANNEL_COUNT && self.0 & (1 << number) != 0
    }
}

fn checked_mask(numbers: &[u8], maximum: u8) -> Option<u16> {
    if numbers.is_empty() || numbers.len() > usize::from(maximum) {
        return None;
    }
    let mut mask = 0;
    for &number in numbers {
        if number >= maximum || mask & (1 << number) != 0 {
            return None;
        }
        mask |= 1 << number;
    }
    Some(mask)
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum MinimumSpreadingFactor {
    Sf32,
    Sf64,
    Sf128,
    Sf256,
}

impl MinimumSpreadingFactor {
    pub fn chips(self) -> u16 {
        match self {
            Self::Sf32 => 32,
            Self::Sf64 => 64,
            Self::Sf128 => 128,
            Self::Sf256 => 256,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PuncturingLimit(u8);

impl PuncturingLimit {
    pub fn percent(self) -> u8 {
        MIN_PUNCTURING_PERCENT + self.0 * PUNCTURING_STEP_PERCENT
    }
    pub fn from_percent(percent: u8) -> Result<Self, AccessError> {
        if !(MIN_PUNCTURING_PERCENT..=MAX_PUNCTURING_PERCENT).contains(&percent)
            || !(percent - MIN_PUNCTURING_PERCENT).is_multiple_of(PUNCTURING_STEP_PERCENT)
        {
            return Err(AccessError::TransportProfile);
        }
        Ok(Self(
            (percent - MIN_PUNCTURING_PERCENT) / PUNCTURING_STEP_PERCENT,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AichTiming {
    ThreeSlots,
    FiveSlots,
}

impl AichTiming {
    // TS 25.211 7.3 defines the PRACH-to-AICH start separation in chips.
    pub fn preamble_to_indicator_chips(self) -> u32 {
        match self {
            Self::ThreeSlots => THREE_SLOT_CHIPS,
            Self::FiveSlots => FIVE_SLOT_CHIPS,
        }
    }

    // TS 25.211 7.3 defines tau_p-m separately from the preamble-to-AI distance.
    pub fn preamble_to_message_chips(self) -> u32 {
        match self {
            Self::ThreeSlots => THREE_SLOT_MESSAGE_CHIPS,
            Self::FiveSlots => FIVE_SLOT_MESSAGE_CHIPS,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Aich {
    pub code256: u8,
    pub transmit_diversity: bool,
    pub timing: AichTiming,
}

#[derive(Clone, Copy, Debug)]
pub struct AccessServiceClass {
    pub first_signature_index: u8,
    pub last_signature_index: u8,
    pub assigned_subchannels: u8,
}

impl AccessServiceClass {
    // TS 25.331 8.6.6.29 repeats b2..b0 four times for e0, b3..b0 three times for e1.
    pub fn available_subchannels(
        self,
        available: Subchannels,
        timing: AichTiming,
    ) -> Result<Subchannels, AccessError> {
        if self.assigned_subchannels >= 1 << ASSIGNMENT_BITS {
            return Err(AccessError::Partition);
        }
        let width = match timing {
            AichTiming::ThreeSlots => 3,
            AichTiming::FiveSlots => 4,
        };
        let pattern = u16::from(self.assigned_subchannels) & ((1 << width) - 1);
        let repeated = (0..SUBCHANNEL_BITS)
            .step_by(width)
            .fold(0, |mask, shift| mask | pattern << shift);
        let mask = available.0 & repeated;
        if mask == 0 {
            return Err(AccessError::Partition);
        }
        Ok(Subchannels(mask))
    }

    // ASC signature indices address the ascending available list, not physical signature numbers.
    pub fn available_signatures(self, available: Signatures) -> Result<Signatures, AccessError> {
        if self.first_signature_index > self.last_signature_index
            || self.last_signature_index >= available.count()
        {
            return Err(AccessError::Partition);
        }
        let mask = (0..SIGNATURE_COUNT)
            .filter(|&number| available.contains(number))
            .enumerate()
            .filter(|(index, _)| {
                (usize::from(self.first_signature_index)..=usize::from(self.last_signature_index))
                    .contains(index)
            })
            .fold(0, |mask, (_, number)| mask | 1 << number);
        Ok(Signatures(mask))
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub enum PersistenceFactor {
    Point9,
    Point8,
    Point7,
    Point6,
    Point5,
    Point4,
    Point3,
    Point2,
}

pub struct PrachSettings {
    pub signatures: Signatures,
    pub subchannels: Subchannels,
    pub minimum_spreading_factor: MinimumSpreadingFactor,
    pub preamble_scrambling_code: u8,
    pub puncturing_limit: PuncturingLimit,
    pub transport_identity: u8,
    pub access_classes: Vec<AccessServiceClass>,
    pub ac_to_asc: [u8; MAPPING_COUNT],
    pub persistence_factors: Option<Vec<PersistenceFactor>>,
    pub primary_cpich_dbm: i8,
    pub initial_power_constant_db: i8,
    pub ramp_step_db: u8,
    pub preamble_retransmissions: u8,
    pub maximum_ramp_cycles: u8,
    pub backoff_min: u8,
    pub backoff_max: u8,
    pub aich: Aich,
}

pub struct PrachInformation {
    settings: PrachSettings,
    transport: CommonTransportFormatSet,
    combinations: TransportCombinations,
    message_powers: Vec<PrachMessagePower>,
}

impl PrachInformation {
    pub fn new(
        settings: PrachSettings,
        transport: CommonTransportFormatSet,
        format_indices: &[usize],
        message_powers: Vec<PrachMessagePower>,
    ) -> Result<Self, AccessError> {
        settings.validate()?;
        if format_indices.is_empty() || format_indices.len() > MAX_COMBINATIONS {
            return Err(AccessError::Transport(TransportError::Combination));
        }
        if message_powers.len() != format_indices.len() {
            return Err(AccessError::Transport(TransportError::PowerOffset));
        }
        if !matches!(
            transport.interval,
            TransportInterval::Ms10 | TransportInterval::Ms20
        ) || !matches!(transport.coding, ChannelCoding::Convolutional(_))
        {
            return Err(AccessError::TransportProfile);
        }
        let indices: Vec<_> = format_indices.iter().map(|&index| vec![index]).collect();
        let combinations =
            TransportCombinations::new(&[&transport], &indices).map_err(AccessError::Transport)?;
        Ok(Self {
            settings,
            transport,
            combinations,
            message_powers,
        })
    }

    pub fn settings(&self) -> &PrachSettings {
        &self.settings
    }
    pub fn transport(&self) -> &CommonTransportFormatSet {
        &self.transport
    }
    pub fn combinations(&self) -> &TransportCombinations {
        &self.combinations
    }

    pub fn encode(&self) -> Result<EncodedInformation, AccessError> {
        let mut output = BitWriter::default();
        self.write(&mut output)?;
        validate_rrc_value(
            RrcValueKind::PrachSystemInformation,
            &output.bytes,
            output.length,
        )
        .map_err(AccessError::Codec)?;
        Ok(EncodedInformation {
            bytes: output.bytes,
            bit_length: output.length,
        })
    }

    pub(crate) fn write(&self, output: &mut BitWriter) -> Result<(), AccessError> {
        let settings = &self.settings;
        for present in [
            true,
            true,
            true,
            settings.persistence_factors.is_some(),
            true,
        ] {
            output.flag(present);
        }
        output.flag(false);
        // TS 25.331 AvailableSignatures and AvailableSubChannelNumbers place the highest number first.
        output.write(u64::from(settings.signatures.0), SIGNATURE_BITS);
        output.write(settings.minimum_spreading_factor as u64, SF_BITS);
        output.write(
            u64::from(settings.preamble_scrambling_code),
            PREAMBLE_CODE_BITS,
        );
        output.write(u64::from(settings.puncturing_limit.0), PUNCTURING_BITS);
        output.write(u64::from(settings.subchannels.0), SUBCHANNEL_BITS);
        output.write(u64::from(settings.transport_identity - 1), TRCH_ID_BITS);
        self.transport
            .write(output)
            .map_err(AccessError::Transport)?;
        self.combinations
            .write_prach(output, &self.message_powers)
            .map_err(AccessError::Transport)?;
        output.flag(false);
        output.write((settings.access_classes.len() - 1) as u64, ASC_COUNT_BITS);
        for asc in &settings.access_classes {
            output.flag(true);
            output.write(u64::from(asc.first_signature_index), SIGNATURE_INDEX_BITS);
            output.write(u64::from(asc.last_signature_index), SIGNATURE_INDEX_BITS);
            output.write(u64::from(asc.assigned_subchannels), ASSIGNMENT_BITS);
        }
        if let Some(factors) = &settings.persistence_factors {
            output.write((factors.len() - 1) as u64, ASC_COUNT_BITS);
            for &factor in factors {
                output.write(factor as u64, PERSISTENCE_BITS);
            }
        }
        for &asc in &settings.ac_to_asc {
            output.write(u64::from(asc), ASC_MAPPING_BITS);
        }
        output.flag(false);
        for _ in 0..FDD_OPTIONAL_FIELDS {
            output.flag(true);
        }
        output.write(
            (i16::from(settings.primary_cpich_dbm) - i16::from(MIN_CPICH_DBM)) as u64,
            CPICH_POWER_BITS,
        );
        output.write(
            (i16::from(settings.initial_power_constant_db) - i16::from(MIN_POWER_CONSTANT_DB))
                as u64,
            CONSTANT_BITS,
        );
        output.write(u64::from(settings.ramp_step_db - 1), POWER_STEP_BITS);
        output.write(
            u64::from(settings.preamble_retransmissions - 1),
            RETRANSMISSION_BITS,
        );
        output.write(
            u64::from(settings.maximum_ramp_cycles - 1),
            RAMP_CYCLES_BITS,
        );
        output.write(u64::from(settings.backoff_min), BACKOFF_BITS);
        output.write(u64::from(settings.backoff_max), BACKOFF_BITS);
        output.write(u64::from(settings.aich.code256), AICH_CODE_BITS);
        output.flag(settings.aich.transmit_diversity);
        output.write(settings.aich.timing as u64, 1);
        Ok(())
    }
}

impl PrachSettings {
    fn validate(&self) -> Result<(), AccessError> {
        if self.preamble_scrambling_code >= SIGNATURE_COUNT {
            return Err(AccessError::PreambleCode);
        }
        if !(1..=MAX_TRCH_ID).contains(&self.transport_identity) {
            return Err(AccessError::TransportIdentity);
        }
        if self.access_classes.is_empty() || self.access_classes.len() > MAX_ASC {
            return Err(AccessError::Partition);
        }
        for &asc in &self.access_classes {
            asc.available_signatures(self.signatures)?;
            asc.available_subchannels(self.subchannels, self.aich.timing)?;
        }
        if self
            .ac_to_asc
            .iter()
            .any(|&asc| usize::from(asc) >= self.access_classes.len())
        {
            return Err(AccessError::Mapping);
        }
        if self
            .persistence_factors
            .as_ref()
            .is_some_and(|list| list.is_empty() || list.len() > MAX_PERSISTENCE_FACTORS)
        {
            return Err(AccessError::Persistence);
        }
        if !(MIN_CPICH_DBM..=MAX_CPICH_DBM).contains(&self.primary_cpich_dbm)
            || !(MIN_POWER_CONSTANT_DB..=MAX_POWER_CONSTANT_DB)
                .contains(&self.initial_power_constant_db)
            || !(1..=MAX_RAMP_STEP_DB).contains(&self.ramp_step_db)
        {
            return Err(AccessError::Power);
        }
        if !(1..=MAX_PREAMBLE_RETRANSMISSIONS).contains(&self.preamble_retransmissions)
            || !(1..=MAX_RAMP_CYCLES).contains(&self.maximum_ramp_cycles)
            || self.backoff_min > self.backoff_max
            || self.backoff_max > MAX_BACKOFF
        {
            return Err(AccessError::Retransmission);
        }
        if self.aich.code256 < FIRST_NON_BROADCAST_CODE {
            return Err(AccessError::AichCode);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{CommonFormatGroup, ConvolutionalRate, CrcSize, LogicalChannels};

    fn settings() -> PrachSettings {
        PrachSettings {
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
            ac_to_asc: [0; MAPPING_COUNT],
            persistence_factors: None,
            primary_cpich_dbm: 10,
            initial_power_constant_db: -10,
            ramp_step_db: 1,
            preamble_retransmissions: 64,
            maximum_ramp_cycles: 32,
            backoff_min: 0,
            backoff_max: 50,
            aich: Aich {
                code256: 2,
                transmit_diversity: false,
                timing: AichTiming::FiveSlots,
            },
        }
    }

    fn transport() -> CommonTransportFormatSet {
        CommonTransportFormatSet {
            interval: TransportInterval::Ms20,
            coding: ChannelCoding::Convolutional(ConvolutionalRate::Half),
            rate_matching_attribute: 256,
            crc: CrcSize::Bits16,
            groups: vec![CommonFormatGroup {
                transport_block_bits: 168,
                block_counts: vec![0, 1],
                logical_channels: LogicalChannels::AllSizes,
            }],
        }
    }

    #[test]
    fn asc_indices_are_relative_to_ascending_available_signatures() {
        let available = Signatures::new(&[15, 0, 10, 5]).unwrap();
        let asc = AccessServiceClass {
            first_signature_index: 1,
            last_signature_index: 2,
            assigned_subchannels: 15,
        };
        let selected = asc.available_signatures(available).unwrap();
        assert_eq!(selected, Signatures::new(&[5, 10]).unwrap());
        assert_eq!(
            AccessServiceClass {
                last_signature_index: 4,
                ..asc
            }
            .available_signatures(available),
            Err(AccessError::Partition)
        );
        assert_eq!(
            AccessServiceClass {
                first_signature_index: 3,
                ..asc
            }
            .available_signatures(available),
            Err(AccessError::Partition)
        );
    }

    #[test]
    fn aich_timing_changes_subchannel_pattern_not_mask_numbering() {
        let available = Subchannels::new(&(0..SUBCHANNEL_COUNT).collect::<Vec<_>>()).unwrap();
        let asc = AccessServiceClass {
            first_signature_index: 0,
            last_signature_index: 0,
            assigned_subchannels: 9,
        };
        assert_eq!(
            asc.available_subchannels(available, AichTiming::ThreeSlots)
                .unwrap(),
            Subchannels::new(&[0, 3, 6, 9]).unwrap()
        );
        assert_eq!(
            asc.available_subchannels(available, AichTiming::FiveSlots)
                .unwrap(),
            Subchannels::new(&[0, 3, 4, 7, 8, 11]).unwrap()
        );
        assert_eq!(
            asc.available_subchannels(Subchannels::new(&[1, 2]).unwrap(), AichTiming::FiveSlots),
            Err(AccessError::Partition)
        );
        let ignored_high_bit = AccessServiceClass {
            assigned_subchannels: 8,
            ..asc
        };
        assert_eq!(
            ignored_high_bit.available_subchannels(available, AichTiming::ThreeSlots),
            Err(AccessError::Partition)
        );
        assert_eq!(AichTiming::ThreeSlots.preamble_to_indicator_chips(), 7680);
        assert_eq!(AichTiming::FiveSlots.preamble_to_indicator_chips(), 12800);
    }

    #[test]
    fn available_sets_and_puncturing_are_exact_not_rounded() {
        assert_eq!(Signatures::new(&[]), Err(AccessError::Signatures));
        assert_eq!(Signatures::new(&[0, 0]), Err(AccessError::Signatures));
        assert_eq!(Signatures::new(&[16]), Err(AccessError::Signatures));
        assert_eq!(Subchannels::new(&[12]), Err(AccessError::Subchannels));
        assert!(!Signatures::new(&[0]).unwrap().contains(16));
        assert!(!Subchannels::new(&[0]).unwrap().contains(12));
        for percent in 0..=u8::MAX {
            assert_eq!(
                PuncturingLimit::from_percent(percent).is_ok(),
                (40..=100).contains(&percent) && (percent - 40).is_multiple_of(4)
            );
        }
    }

    #[test]
    fn full_prach_information_passes_generated_constraints_and_reencoding() {
        let information =
            PrachInformation::new(settings(), transport(), &[0, 1], powers()).unwrap();
        let encoded = information.encode().unwrap();
        assert_eq!(encoded.bit_length, 236);
        assert_eq!(
            encoded.bytes,
            [
                0xea, 0x00, 0x07, 0x0f, 0x80, 0x10, 0x48, 0x03, 0xc2, 0x22, 0xff, 0x60, 0x00, 0x32,
                0x3f, 0xde, 0xc7, 0xfb, 0x84, 0x07, 0xc0, 0x00, 0x00, 0xfa, 0x99, 0x1f, 0xfc, 0x0c,
                0x80, 0x90,
            ]
        );
        assert_eq!(information.combinations().ctfc_values(), &[0, 1]);
    }

    fn powers() -> Vec<PrachMessagePower> {
        vec![PrachMessagePower::new(15, 15, 2).unwrap(); 2]
    }

    #[test]
    fn prach_requires_explicit_power_for_every_transport_combination() {
        for powers in [Vec::new(), vec![PrachMessagePower::new(15, 15, 2).unwrap()]] {
            assert_eq!(
                PrachInformation::new(settings(), transport(), &[0, 1], powers).err(),
                Some(AccessError::Transport(TransportError::PowerOffset))
            );
        }
    }

    #[test]
    fn power_mapping_and_access_reject_unusable_advertisements() {
        type InvalidSetting = (fn(&mut PrachSettings), AccessError);
        let cases: &[InvalidSetting] = &[
            (
                |s| s.preamble_scrambling_code = 16,
                AccessError::PreambleCode,
            ),
            (|s| s.transport_identity = 0, AccessError::TransportIdentity),
            (|s| s.ac_to_asc[0] = 1, AccessError::Mapping),
            (|s| s.access_classes.clear(), AccessError::Partition),
            (
                |s| s.access_classes[0].assigned_subchannels = 16,
                AccessError::Partition,
            ),
            (|s| s.primary_cpich_dbm = -11, AccessError::Power),
            (|s| s.initial_power_constant_db = -9, AccessError::Power),
            (|s| s.ramp_step_db = 0, AccessError::Power),
            (
                |s| s.preamble_retransmissions = 65,
                AccessError::Retransmission,
            ),
            (|s| s.maximum_ramp_cycles = 0, AccessError::Retransmission),
            (
                |s| {
                    s.backoff_min = 10;
                    s.backoff_max = 9;
                },
                AccessError::Retransmission,
            ),
            (|s| s.aich.code256 = 1, AccessError::AichCode),
            (
                |s| s.persistence_factors = Some(vec![]),
                AccessError::Persistence,
            ),
        ];
        for (change, expected) in cases {
            let mut config = settings();
            change(&mut config);
            assert_eq!(
                PrachInformation::new(config, transport(), &[0, 1], powers()).err(),
                Some(expected.clone())
            );
        }
    }

    #[test]
    fn optional_persistence_and_boundary_fields_encode_for_both_timings() {
        for timing in [AichTiming::ThreeSlots, AichTiming::FiveSlots] {
            for factor in [
                PersistenceFactor::Point9,
                PersistenceFactor::Point8,
                PersistenceFactor::Point7,
                PersistenceFactor::Point6,
                PersistenceFactor::Point5,
                PersistenceFactor::Point4,
                PersistenceFactor::Point3,
                PersistenceFactor::Point2,
            ] {
                let mut config = settings();
                config.aich.timing = timing;
                config.aich.code256 = 255;
                config.aich.transmit_diversity = true;
                config.transport_identity = 32;
                config.primary_cpich_dbm = -10;
                config.initial_power_constant_db = -35;
                config.ramp_step_db = 8;
                config.preamble_retransmissions = 1;
                config.maximum_ramp_cycles = 1;
                config.persistence_factors = Some(vec![factor; MAX_PERSISTENCE_FACTORS]);
                PrachInformation::new(config, transport(), &[0, 1], powers())
                    .unwrap()
                    .encode()
                    .unwrap();
            }
        }
    }

    #[test]
    fn rach_transport_rejects_illegal_tti_coding_and_indices() {
        for interval in [TransportInterval::Ms40, TransportInterval::Ms80] {
            let mut tfs = transport();
            tfs.interval = interval;
            assert_eq!(
                PrachInformation::new(settings(), tfs, &[0, 1], powers()).err(),
                Some(AccessError::TransportProfile)
            );
        }
        let mut tfs = transport();
        tfs.coding = ChannelCoding::Turbo;
        assert_eq!(
            PrachInformation::new(settings(), tfs, &[0, 1], powers()).err(),
            Some(AccessError::TransportProfile)
        );
        for indices in [vec![], vec![2], vec![0, 0], vec![0; MAX_COMBINATIONS + 1]] {
            assert!(PrachInformation::new(settings(), transport(), &indices, powers()).is_err());
        }
        let mut tfs = transport();
        tfs.interval = TransportInterval::Ms10;
        tfs.coding = ChannelCoding::Convolutional(ConvolutionalRate::Third);
        PrachInformation::new(settings(), tfs, &[0, 1], powers())
            .unwrap()
            .encode()
            .unwrap();
    }
}
