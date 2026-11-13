// SPDX-License-Identifier: AGPL-3.0-or-later

use umts_rrc_schema::information_elements as ie;
use umts_types::hsupa::{
    CodeLayout, Etfci, LayoutSet, PuncturingLimit, Rel6Category, TransportBlockTable, Tti,
};

mod activation;
mod edch_only;
mod mapping;
mod physical;
mod restoration;
mod uplink_activation;
pub use activation::{
    ActivationParameters, DchCoexistenceActivation, MappedBearer, RetainedDownlink,
};
pub use edch_only::{EdchOnlyActivation, EdchOnlyBearerPlan};
pub use mapping::{ScheduledFlow, UplinkMapping};
pub use physical::{DownlinkControl, RelativeGrant};
pub use restoration::DchRestoration;
pub use uplink_activation::UplinkActivation;
pub type ActivationProcedure = crate::dedicated::ActivationProcedure<UplinkActivation>;
pub type RestorationProcedure = crate::dedicated::ActivationProcedure<DchRestoration>;

const MAX_CONTROL_POWER: u8 = 8;
const MAX_REFERENCE_POWER: u8 = 29;
const MAX_SCHEDULING_POWER: u8 = 6;
const MAX_POWER_REFERENCES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PowerReference {
    pub format: Etfci,
    pub power_index: u8,
}

#[derive(Clone, Debug)]
pub struct ReferencePowers(Box<[PowerReference]>);

impl ReferencePowers {
    pub fn new(references: Vec<PowerReference>) -> Result<Self, ConfigurationError> {
        if references.is_empty() || references.len() > MAX_POWER_REFERENCES {
            return Err(ConfigurationError::Power);
        }
        if references
            .iter()
            .any(|reference| reference.power_index > MAX_REFERENCE_POWER)
            || references
                .windows(2)
                .any(|pair| pair[0].format.value() >= pair[1].format.value())
        {
            return Err(ConfigurationError::Power);
        }
        Ok(Self(references.into_boxed_slice()))
    }

    pub fn single(format: Etfci, power_index: u8) -> Result<Self, ConfigurationError> {
        Self::new(vec![PowerReference {
            format,
            power_index,
        }])
    }

    pub fn as_slice(&self) -> &[PowerReference] {
        &self.0
    }

    // TS 25.214 5.1.2.5B.2.3 selects the greatest reference not above the E-TFCI, or the first.
    pub fn index_for(&self, format: Etfci) -> usize {
        self.0
            .partition_point(|reference| reference.format.value() <= format.value())
            .saturating_sub(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationError {
    Capability,
    Format,
    Power,
    Period,
    Code,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportingPeriod {
    EveryTti,
    Milliseconds(u16),
}

impl ReportingPeriod {
    fn wire(self) -> Result<ie::EDPDCHPeriodicyOfSchedInfo, ConfigurationError> {
        use ie::EDPDCHPeriodicyOfSchedInfo as W;
        Ok(match self {
            Self::EveryTti => W::everyEDCHTTI,
            Self::Milliseconds(4) => W::ms4,
            Self::Milliseconds(10) => W::ms10,
            Self::Milliseconds(20) => W::ms20,
            Self::Milliseconds(50) => W::ms50,
            Self::Milliseconds(100) => W::ms100,
            Self::Milliseconds(200) => W::ms200,
            Self::Milliseconds(500) => W::ms500,
            Self::Milliseconds(1000) => W::ms1000,
            _ => return Err(ConfigurationError::Period),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SchedulingInformation {
    pub without_grant: Option<ReportingPeriod>,
    pub with_grant: Option<ReportingPeriod>,
    pub power_offset: u8,
}

impl SchedulingInformation {
    fn wire(self) -> Result<ie::EDPDCHSchedulingInfoConfiguration, ConfigurationError> {
        if self.power_offset > MAX_SCHEDULING_POWER {
            return Err(ConfigurationError::Power);
        }
        Ok(ie::EDPDCHSchedulingInfoConfiguration::new(
            self.without_grant.map(ReportingPeriod::wire).transpose()?,
            self.with_grant.map(ReportingPeriod::wire).transpose()?,
            self.power_offset,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ControlConfiguration {
    power_index: u8,
    happy_delay_ms: u16,
}

impl ControlConfiguration {
    pub fn power_index(self) -> u8 {
        self.power_index
    }

    pub fn new(power_index: u8, happy_delay_ms: u16) -> Result<Self, ConfigurationError> {
        if power_index > MAX_CONTROL_POWER {
            return Err(ConfigurationError::Power);
        }
        let result = Self {
            power_index,
            happy_delay_ms,
        };
        result.wire()?;
        Ok(result)
    }

    pub fn wire(self) -> Result<ie::EDPCCHInfo, ConfigurationError> {
        use ie::HappyBitDelayCondition as W;
        let delay = match self.happy_delay_ms {
            2 => W::ms2,
            10 => W::ms10,
            20 => W::ms20,
            50 => W::ms50,
            100 => W::ms100,
            200 => W::ms200,
            500 => W::ms500,
            1000 => W::ms1000,
            _ => return Err(ConfigurationError::Period),
        };
        Ok(ie::EDPCCHInfo::new(
            ie::EDPCCHDPCCHPowerOffset(self.power_index),
            delay,
        ))
    }
}

pub struct TransportConfiguration {
    interval: Tti,
    category: Rel6Category,
    table: TransportBlockTable,
    maximum: CodeLayout,
    puncturing: PuncturingLimit,
    layouts: LayoutSet,
}

impl TransportConfiguration {
    pub fn new(
        interval: Tti,
        category: Rel6Category,
        table: TransportBlockTable,
        maximum: CodeLayout,
        puncturing: PuncturingLimit,
    ) -> Result<Self, ConfigurationError> {
        let allowed: Vec<_> = maximum.up_to().collect();
        let layouts = LayoutSet::new(interval, category, &allowed, puncturing)
            .map_err(|_| ConfigurationError::Capability)?;
        Ok(Self {
            interval,
            category,
            table,
            maximum,
            puncturing,
            layouts,
        })
    }

    pub fn layouts(&self) -> &LayoutSet {
        &self.layouts
    }
    pub fn maximum_layout(&self) -> CodeLayout {
        self.maximum
    }
    pub fn table(&self) -> TransportBlockTable {
        self.table
    }
    pub fn category(&self) -> Rel6Category {
        self.category
    }
    pub fn interval(&self) -> Tti {
        self.interval
    }

    pub fn wire_single_reference(
        &self,
        reference: Etfci,
        power_index: u8,
        minimum: Etfci,
        scheduling: SchedulingInformation,
    ) -> Result<ie::EDPDCHInfo, ConfigurationError> {
        self.wire_reference_powers(
            &ReferencePowers::single(reference, power_index)?,
            minimum,
            scheduling,
        )
    }

    pub fn wire_reference_powers(
        &self,
        references: &ReferencePowers,
        minimum: Etfci,
        scheduling: SchedulingInformation,
    ) -> Result<ie::EDPDCHInfo, ConfigurationError> {
        for format in references
            .as_slice()
            .iter()
            .map(|reference| reference.format)
            .chain(std::iter::once(minimum))
        {
            let bits = self
                .table
                .bits(self.interval, format)
                .ok_or(ConfigurationError::Format)?;
            if self
                .category
                .maximum_transport_bits(self.interval)
                .is_none_or(|max| bits.get() > max)
            {
                return Err(ConfigurationError::Capability);
            }
        }
        use ie::EDPDCHMaxChannelisationCodes as W;
        let maximum = match self.maximum.spreading_factors() {
            [256] => W::sf256,
            [128] => W::sf128,
            [64] => W::sf64,
            [32] => W::sf32,
            [16] => W::sf16,
            [8] => W::sf8,
            [4] => W::sf4,
            [4, 4] => W::sf4x2,
            [2, 2] => W::sf2x2,
            [2, 2, 4, 4] => W::sf4x2_and_sf2x2,
            _ => return Err(ConfigurationError::Format),
        };
        Ok(ie::EDPDCHInfo::new(
            ie::ETFCITableIndex(match self.table {
                TransportBlockTable::Zero => 0,
                TransportBlockTable::One => 1,
            }),
            Some(ie::EDCHMinimumSetETFCI(minimum.value())),
            ie::EDPDCHReferenceETFCIList(
                references
                    .as_slice()
                    .iter()
                    .map(|reference| {
                        ie::EDPDCHReferenceETFCI::new(
                            reference.format.value(),
                            reference.power_index,
                        )
                    })
                    .collect(),
            ),
            maximum,
            ie::EDPDCHPLNonMax(self.puncturing.percent() / 4),
            scheduling.wire()?,
            None,
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_selection_and_signalling_preserve_every_power_point() {
        let point = |format, power_index| PowerReference {
            format: Etfci::new(format).unwrap(),
            power_index,
        };
        let references = ReferencePowers::new(vec![point(11, 4), point(83, 17)]).unwrap();
        for (format, expected) in [(0, 0), (10, 0), (11, 0), (82, 0), (83, 1), (127, 1)] {
            assert_eq!(references.index_for(Etfci::new(format).unwrap()), expected);
        }
        assert!(ReferencePowers::new(vec![]).is_err());
        assert!(ReferencePowers::new(vec![point(11, 4), point(11, 17)]).is_err());
        assert!(ReferencePowers::new(vec![point(83, 17), point(11, 4)]).is_err());
        assert!(ReferencePowers::new(vec![point(11, 30)]).is_err());
        assert!(ReferencePowers::new((0..9).map(|format| point(format, 4)).collect()).is_err());
        let configuration = TransportConfiguration::new(
            Tti::TwoMilliseconds,
            Rel6Category::new(6).unwrap(),
            TransportBlockTable::Zero,
            CodeLayout::new(&[2, 2, 4, 4]).unwrap(),
            PuncturingLimit::new(44).unwrap(),
        )
        .unwrap();
        let wire = configuration
            .wire_reference_powers(
                &references,
                Etfci::new(0).unwrap(),
                SchedulingInformation {
                    without_grant: Some(ReportingPeriod::EveryTti),
                    with_grant: None,
                    power_offset: 0,
                },
            )
            .unwrap();
        let encoded = rasn::uper::encode(&wire).unwrap();
        let decoded: ie::EDPDCHInfo = rasn::uper::decode(&encoded).unwrap();
        let points: Vec<_> = decoded
            .reference_e_tfcis
            .0
            .iter()
            .map(|reference| (reference.reference_e_tfci, reference.reference_e_tfci_po))
            .collect();
        assert_eq!(points, [(11, 4), (83, 17)]);
    }

    #[test]
    fn wire_and_receiver_share_table_layout_and_puncturing() {
        let configuration = TransportConfiguration::new(
            Tti::TwoMilliseconds,
            Rel6Category::new(6).unwrap(),
            TransportBlockTable::Zero,
            CodeLayout::new(&[2, 2, 4, 4]).unwrap(),
            PuncturingLimit::new(44).unwrap(),
        )
        .unwrap();
        let scheduling = SchedulingInformation {
            without_grant: Some(ReportingPeriod::EveryTti),
            with_grant: Some(ReportingPeriod::Milliseconds(100)),
            power_offset: 0,
        };
        let wire = configuration
            .wire_single_reference(
                Etfci::new(50).unwrap(),
                20,
                Etfci::new(0).unwrap(),
                scheduling,
            )
            .unwrap();
        assert_eq!(wire.e_tfci_table_index.0, 0);
        assert_eq!(wire.pl_non_max.0, 11);
        assert_eq!(
            wire.max_channelisation_codes,
            ie::EDPDCHMaxChannelisationCodes::sf4x2_and_sf2x2
        );
        assert_eq!(wire.reference_e_tfcis.0[0].reference_e_tfci, 50);
        assert_eq!(wire.reference_e_tfcis.0[0].reference_e_tfci_po, 20);
        assert_eq!(configuration.layouts().interval(), Tti::TwoMilliseconds);
        assert!(
            configuration
                .wire_single_reference(
                    Etfci::new(120).unwrap(),
                    20,
                    Etfci::new(0).unwrap(),
                    scheduling
                )
                .is_err()
        );
        assert!(
            configuration
                .wire_single_reference(
                    Etfci::new(50).unwrap(),
                    30,
                    Etfci::new(0).unwrap(),
                    scheduling
                )
                .is_err()
        );
        let encoded = rasn::uper::encode(&wire).unwrap();
        let decoded: ie::EDPDCHInfo = rasn::uper::decode(&encoded).unwrap();
        assert_eq!(decoded, wire);
        assert!(ControlConfiguration::new(9, 20).is_err());
        assert!(ControlConfiguration::new(8, 4).is_err());
    }
}
