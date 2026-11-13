// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    Error,
    wire::{Criticality, ExtensionList, InformationElement},
};
use rasn::{AsnType, Decode, Decoder, Encode};
use std::collections::BTreeSet;

const PARAMETERS: u16 = 149;
const ROOT_RATE_MAX: u32 = 16_000_000;
const EXTENDED_RATE_MAX: u32 = 256_000_000;
const MAX_SUBFLOWS: usize = 7;
const MAX_COMBINATIONS: usize = 64;
pub const SIGNALLING_INDICATION: u16 = 116;
pub const EXTENDED_GUARANTEED: u16 = 176;
pub const EXTENDED_MAXIMUM: u16 = 177;
pub const SUPPORTED_GUARANTEED: u16 = 218;
pub const SUPPORTED_MAXIMUM: u16 = 219;

macro_rules! scalar {
    ($name:ident, $inner:ty, $constraint:literal, $minimum:expr, $maximum:expr) => {
        #[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
        #[rasn(delegate, value($constraint))]
        pub struct $name($inner);
        impl $name {
            pub fn new(value: $inner) -> Result<Self, Error> {
                if ($minimum..=$maximum).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(Error::Value(PARAMETERS))
                }
            }
            pub fn get(self) -> $inner {
                self.0
            }
        }
    };
}

scalar!(MaximumRate, u32, "1..=16000000", 1, ROOT_RATE_MAX);
scalar!(GuaranteedRate, u32, "0..=16000000", 0, ROOT_RATE_MAX);
scalar!(
    ExtendedRate,
    u32,
    "16000001..=256000000",
    ROOT_RATE_MAX + 1,
    EXTENDED_RATE_MAX
);
scalar!(MaxSduBits, u16, "0..=32768", 0, 32768);
scalar!(SubflowBits, u16, "0..=4095", 0, 4095);
scalar!(CombinationRate, u32, "0..=16000000", 0, ROOT_RATE_MAX);
scalar!(Priority, u8, "0..=15", 1, 15);
scalar!(Mantissa, u8, "1..=9", 1, 9);
scalar!(ResidualExponent, u8, "1..=8", 1, 8);
scalar!(SduExponent, u8, "1..=6", 1, 6);

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(delegate, value("1..=1000000000", extensible))]
pub struct SupportedRate(u64);
impl SupportedRate {
    pub fn new(value: u64) -> Result<Self, Error> {
        if value == 0 {
            Err(Error::Value(PARAMETERS))
        } else {
            Ok(Self(value))
        }
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

macro_rules! rates {
    ($name:ident, $scalar:ident, $inner:ty) => {
        #[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
        #[rasn(delegate, size("1..=2"))]
        pub struct $name(Vec<$scalar>);
        impl $name {
            pub fn new(values: &[$inner]) -> Result<Self, Error> {
                if !(1..=2).contains(&values.len()) {
                    return Err(Error::Value(PARAMETERS));
                }
                Ok(Self(
                    values
                        .iter()
                        .copied()
                        .map($scalar::new)
                        .collect::<Result<_, _>>()?,
                ))
            }
            pub fn values(&self) -> impl ExactSizeIterator<Item = $inner> + '_ {
                self.0.iter().map(|value| value.get())
            }
        }
    };
}
rates!(MaximumRates, MaximumRate, u32);
rates!(GuaranteedRates, GuaranteedRate, u32);
rates!(ExtendedRates, ExtendedRate, u32);
rates!(SupportedRates, SupportedRate, u64);

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum TrafficClass {
    Conversational,
    Streaming,
    Interactive,
    Background,
}
impl TrafficClass {
    pub fn is_realtime(self) -> bool {
        matches!(self, Self::Conversational | Self::Streaming)
    }
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Asymmetry {
    SymmetricBidirectional,
    Downlink,
    Uplink,
    AsymmetricBidirectional,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum DeliveryOrder {
    Requested,
    NotRequested,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum Delivery {
    Yes,
    No,
    NoDetection,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum SourceStatistics {
    Speech,
    Unknown,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Relocation {
    Lossless,
    None,
    #[rasn(extension_addition)]
    Realtime,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum PreemptionCapability {
    CannotTrigger,
    MayTrigger,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum PreemptionVulnerability {
    NotPreemptable,
    Preemptable,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum Queuing {
    NotAllowed,
    Allowed,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum SignallingIndication {
    Signalling,
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Allocation {
    pub priority: Priority,
    pub preemption_capability: PreemptionCapability,
    pub preemption_vulnerability: PreemptionVulnerability,
    pub queuing: Queuing,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Allocation {
    pub fn new(
        priority: Priority,
        preemption_capability: PreemptionCapability,
        preemption_vulnerability: PreemptionVulnerability,
        queuing: Queuing,
    ) -> Self {
        Self {
            priority,
            preemption_capability,
            preemption_vulnerability,
            queuing,
            extensions: None,
        }
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct ResidualError {
    mantissa: Mantissa,
    exponent: ResidualExponent,
    extensions: Option<ExtensionList>,
}
impl ResidualError {
    pub fn new(mantissa: u8, exponent: u8) -> Result<Self, Error> {
        Ok(Self {
            mantissa: Mantissa::new(mantissa)?,
            exponent: ResidualExponent::new(exponent)?,
            extensions: None,
        })
    }
    pub fn mantissa(&self) -> u8 {
        self.mantissa.get()
    }
    pub fn exponent(&self) -> u8 {
        self.exponent.get()
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct SduError {
    mantissa: Mantissa,
    exponent: SduExponent,
    extensions: Option<ExtensionList>,
}
impl SduError {
    pub fn new(mantissa: u8, exponent: u8) -> Result<Self, Error> {
        Ok(Self {
            mantissa: Mantissa::new(mantissa)?,
            exponent: SduExponent::new(exponent)?,
            extensions: None,
        })
    }
    pub fn mantissa(&self) -> u8 {
        self.mantissa.get()
    }
    pub fn exponent(&self) -> u8 {
        self.exponent.get()
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Format {
    pub size: Option<SubflowBits>,
    pub bitrate: Option<CombinationRate>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Format {
    pub fn new(size: Option<u16>, bitrate: Option<u32>) -> Result<Self, Error> {
        if size.is_none() && bitrate.is_none() {
            return Err(Error::Value(PARAMETERS));
        }
        Ok(Self {
            size: size.map(SubflowBits::new).transpose()?,
            bitrate: bitrate.map(CombinationRate::new).transpose()?,
            extensions: None,
        })
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=64"))]
pub struct Formats(Vec<Format>);
impl Formats {
    pub fn new(formats: Vec<Format>) -> Result<Self, Error> {
        if !(1..=MAX_COMBINATIONS).contains(&formats.len()) {
            return Err(Error::Value(PARAMETERS));
        }
        Ok(Self(formats))
    }
    pub fn as_slice(&self) -> &[Format] {
        &self.0
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Subflow {
    pub sdu_error: Option<SduError>,
    pub residual_error: ResidualError,
    pub delivery: Delivery,
    pub formats: Option<Formats>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Subflow {
    pub fn new(
        residual_error: ResidualError,
        delivery: Delivery,
        sdu_error: Option<SduError>,
        formats: Option<Formats>,
    ) -> Result<Self, Error> {
        let value = Self {
            sdu_error,
            residual_error,
            delivery,
            formats,
            extensions: None,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<(), Error> {
        if self.sdu_error.is_some() != (self.delivery != Delivery::NoDetection) {
            return Err(Error::Value(PARAMETERS));
        }
        check_extensions(&self.extensions)?;
        check_extensions(&self.residual_error.extensions)?;
        if let Some(error) = &self.sdu_error {
            check_extensions(&error.extensions)?;
        }
        if let Some(formats) = &self.formats {
            if !(1..=MAX_COMBINATIONS).contains(&formats.0.len()) {
                return Err(Error::Value(PARAMETERS));
            }
            let first = &formats.0[0];
            for format in &formats.0 {
                check_extensions(&format.extensions)?;
                if format.size.is_none() && format.bitrate.is_none()
                    || format.size.is_some() != first.size.is_some()
                    || format.bitrate.is_some() != first.bitrate.is_some()
                {
                    return Err(Error::Value(PARAMETERS));
                }
            }
        }
        Ok(())
    }
    fn extension_ies(&self) -> impl Iterator<Item = &InformationElement> {
        extension_ies(&self.extensions)
            .chain(extension_ies(&self.residual_error.extensions))
            .chain(
                self.sdu_error
                    .iter()
                    .flat_map(|error| extension_ies(&error.extensions)),
            )
            .chain(
                self.formats
                    .iter()
                    .flat_map(|formats| formats.0.iter())
                    .flat_map(|format| extension_ies(&format.extensions)),
            )
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=7"))]
pub struct Subflows(Vec<Subflow>);
impl Subflows {
    pub fn new(subflows: Vec<Subflow>) -> Result<Self, Error> {
        if !(1..=MAX_SUBFLOWS).contains(&subflows.len()) {
            return Err(Error::Value(PARAMETERS));
        }
        Ok(Self(subflows))
    }
    pub fn as_slice(&self) -> &[Subflow] {
        &self.0
    }
}

const RATE_EXTENSIONS: [u16; 4] = [
    SUPPORTED_MAXIMUM,
    SUPPORTED_GUARANTEED,
    EXTENDED_MAXIMUM,
    EXTENDED_GUARANTEED,
];

type RateExtension<'a> = (&'a InformationElement, Result<Rates, Error>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rates {
    Symmetric { both: u64 },
    Downlink(u64),
    Uplink(u64),
    Asymmetric { downlink: u64, uplink: u64 },
}
impl Rates {
    pub fn downlink(self) -> Option<u64> {
        match self {
            Self::Symmetric { both } => Some(both),
            Self::Downlink(value) => Some(value),
            Self::Asymmetric { downlink, .. } => Some(downlink),
            Self::Uplink(_) => None,
        }
    }
    pub fn uplink(self) -> Option<u64> {
        match self {
            Self::Symmetric { both } => Some(both),
            Self::Uplink(value) => Some(value),
            Self::Asymmetric { uplink, .. } => Some(uplink),
            Self::Downlink(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Combination {
    pub subflow_bits: Box<[u16]>,
    pub bitrate: Option<u32>,
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Parameters {
    pub traffic_class: TrafficClass,
    pub asymmetry: Asymmetry,
    pub maximum: MaximumRates,
    pub guaranteed: Option<GuaranteedRates>,
    pub delivery_order: DeliveryOrder,
    pub maximum_sdu_bits: MaxSduBits,
    pub subflows: Subflows,
    #[rasn(value("0..=65535"))]
    pub transfer_delay: Option<u16>,
    pub traffic_priority: Option<Priority>,
    pub allocation: Option<Allocation>,
    pub source_statistics: Option<SourceStatistics>,
    pub relocation: Option<Relocation>,
    pub(crate) extensions: Option<ExtensionList>,
}

impl Parameters {
    pub fn new(
        traffic_class: TrafficClass,
        asymmetry: Asymmetry,
        maximum: MaximumRates,
        delivery_order: DeliveryOrder,
        maximum_sdu_bits: MaxSduBits,
        subflows: Subflows,
    ) -> Self {
        Self {
            traffic_class,
            asymmetry,
            maximum,
            guaranteed: None,
            delivery_order,
            maximum_sdu_bits,
            subflows,
            transfer_delay: None,
            traffic_priority: None,
            allocation: None,
            source_statistics: None,
            relocation: None,
            extensions: None,
        }
    }

    pub fn extension_ies(&self) -> impl Iterator<Item = &InformationElement> {
        extension_ies(&self.extensions)
            .chain(
                self.allocation
                    .iter()
                    .flat_map(|allocation| extension_ies(&allocation.extensions)),
            )
            .chain(self.subflows.0.iter().flat_map(Subflow::extension_ies))
    }

    pub fn unknown_extension_ies(&self) -> impl Iterator<Item = &InformationElement> {
        extension_ies(&self.extensions)
            .filter(|ie| !Self::is_known_extension(ie.id))
            .chain(
                self.allocation
                    .iter()
                    .flat_map(|allocation| extension_ies(&allocation.extensions)),
            )
            .chain(self.subflows.0.iter().flat_map(Subflow::extension_ies))
    }

    pub fn is_known_extension(id: u16) -> bool {
        matches!(
            id,
            SIGNALLING_INDICATION
                | EXTENDED_GUARANTEED
                | EXTENDED_MAXIMUM
                | SUPPORTED_GUARANTEED
                | SUPPORTED_MAXIMUM
        )
    }

    fn extension(&self, id: u16) -> Result<Option<&InformationElement>, Error> {
        let mut matches = extension_ies(&self.extensions).filter(|ie| ie.id == id);
        let value = matches.next();
        if matches.next().is_some() {
            return Err(Error::DuplicateIe(id));
        }
        Ok(value)
    }

    pub fn signalling(&self) -> Result<bool, Error> {
        self.extension(SIGNALLING_INDICATION)?
            .map(|ie| ie.decode::<SignallingIndication>().map(|_| true))
            .transpose()
            .map(|value| value.unwrap_or(false))
    }

    fn rate_values(
        &self,
        supported: u16,
        extended: u16,
        root: impl Iterator<Item = u32>,
    ) -> Result<Rates, Error> {
        if let Some(rates) = self.applied_rate_extension(supported)? {
            return Ok(rates);
        }
        if let Some(rates) = self.applied_rate_extension(extended)? {
            return Ok(rates);
        }
        self.directional(root.map(u64::from))
    }

    fn rate_extension(&self, id: u16) -> Result<Option<RateExtension<'_>>, Error> {
        let Some(ie) = self.extension(id)? else {
            return Ok(None);
        };
        let rates = match id {
            SUPPORTED_MAXIMUM | SUPPORTED_GUARANTEED => {
                ie.decode::<SupportedRates>().and_then(|rates| {
                    if rates.values().any(|value| value == 0) {
                        return Err(Error::Value(id));
                    }
                    self.directional(rates.values())
                })
            }
            _ => ie.decode::<ExtendedRates>().and_then(|rates| {
                if !self.extended_root_permitted(id)? {
                    return Err(Error::Value(id));
                }
                self.directional(rates.values().map(u64::from))
            }),
        };
        Ok(Some((ie, rates)))
    }

    // TS 25.413 8.2.2: extended rates require saturated root rates unless a
    // supported-rate list overrides both.
    fn extended_root_permitted(&self, id: u16) -> Result<bool, Error> {
        Ok(if id == EXTENDED_MAXIMUM {
            self.extension(SUPPORTED_MAXIMUM)?.is_some()
                || self.maximum.values().all(|value| value == ROOT_RATE_MAX)
        } else {
            self.extension(SUPPORTED_GUARANTEED)?.is_some()
                || self
                    .guaranteed
                    .as_ref()
                    .is_some_and(|rates| rates.values().all(|value| value == ROOT_RATE_MAX))
        })
    }

    /// Rate extension IEs that are erroneous but ignored, so the root rates apply.
    pub fn ignored_rate_extensions(&self) -> impl Iterator<Item = (u16, Error)> + '_ {
        RATE_EXTENSIONS
            .into_iter()
            .filter_map(|id| match self.rate_extension(id) {
                Ok(Some((ie, Err(error)))) if ie.criticality == Criticality::Ignore => {
                    Some((id, error))
                }
                _ => None,
            })
    }

    fn applied_rate_extension(&self, id: u16) -> Result<Option<Rates>, Error> {
        match self.rate_extension(id)? {
            None => Ok(None),
            Some((_, Ok(rates))) => Ok(Some(rates)),
            Some((ie, Err(_))) if ie.criticality == Criticality::Ignore => Ok(None),
            Some((_, Err(error))) => Err(error),
        }
    }

    fn directional(&self, values: impl Iterator<Item = u64>) -> Result<Rates, Error> {
        let mut values = values;
        let first = values.next();
        let second = values.next();
        if values.next().is_some() {
            return Err(Error::Value(PARAMETERS));
        }
        match (self.asymmetry, first, second) {
            (Asymmetry::SymmetricBidirectional, Some(both), None) => Ok(Rates::Symmetric { both }),
            (Asymmetry::Downlink, Some(value), None) => Ok(Rates::Downlink(value)),
            (Asymmetry::Uplink, Some(value), None) => Ok(Rates::Uplink(value)),
            (Asymmetry::AsymmetricBidirectional, Some(downlink), Some(uplink)) => {
                Ok(Rates::Asymmetric { downlink, uplink })
            }
            _ => Err(Error::Value(PARAMETERS)),
        }
    }

    pub fn effective_maximum(&self) -> Result<Rates, Error> {
        self.rate_values(SUPPORTED_MAXIMUM, EXTENDED_MAXIMUM, self.maximum.values())
    }

    pub fn effective_guaranteed(&self) -> Result<Option<Rates>, Error> {
        if self.extension(SUPPORTED_GUARANTEED)?.is_none()
            && self.extension(EXTENDED_GUARANTEED)?.is_none()
            && self.guaranteed.is_none()
        {
            return Ok(None);
        }
        self.rate_values(
            SUPPORTED_GUARANTEED,
            EXTENDED_GUARANTEED,
            self.guaranteed.iter().flat_map(GuaranteedRates::values),
        )
        .map(Some)
    }

    pub fn combination_count(&self) -> usize {
        self.subflows
            .0
            .first()
            .and_then(|flow| flow.formats.as_ref())
            .map_or(1, |formats| formats.0.len())
    }

    pub fn combination(&self, index: usize) -> Result<Combination, Error> {
        if index >= self.combination_count() {
            return Err(Error::Value(PARAMETERS));
        }
        let mut bits = Vec::with_capacity(self.subflows.0.len());
        let mut bitrate = None;
        for flow in &self.subflows.0 {
            let format = flow
                .formats
                .as_ref()
                .map(|formats| formats.0.get(index).ok_or(Error::Value(PARAMETERS)))
                .transpose()?;
            let length = format.and_then(|format| format.size).map(SubflowBits::get);
            bits.push(match length {
                Some(length) => length,
                None if self.subflows.0.len() == 1 => self.maximum_sdu_bits.get(),
                None => return Err(Error::Unsupported),
            });
            if let Some(rate) = format
                .and_then(|format| format.bitrate)
                .map(CombinationRate::get)
            {
                if bitrate.is_some_and(|previous| previous != rate) {
                    return Err(Error::Value(PARAMETERS));
                }
                bitrate = Some(rate);
            }
        }
        Ok(Combination {
            subflow_bits: bits.into_boxed_slice(),
            bitrate,
        })
    }

    pub fn validate(&self) -> Result<(), Error> {
        check_extensions(&self.extensions)?;
        if !(1..=MAX_SUBFLOWS).contains(&self.subflows.0.len()) {
            return Err(Error::Value(PARAMETERS));
        }
        let realtime = self.traffic_class.is_realtime();
        let guaranteed = self.effective_guaranteed()?;
        let maximum = self.effective_maximum()?;
        if guaranteed.is_some() != realtime
            || self.transfer_delay.is_some() != realtime
            || self.source_statistics.is_some() != realtime
            || self.traffic_priority.is_some() != (self.traffic_class == TrafficClass::Interactive)
            || self
                .traffic_priority
                .is_some_and(|priority| priority.get() == 0)
        {
            return Err(Error::Value(PARAMETERS));
        }
        if let Some(guaranteed) = guaranteed
            && (guaranteed
                .downlink()
                .zip(maximum.downlink())
                .is_some_and(|(guaranteed, maximum)| guaranteed > maximum)
                || guaranteed
                    .uplink()
                    .zip(maximum.uplink())
                    .is_some_and(|(guaranteed, maximum)| guaranteed > maximum))
        {
            return Err(Error::Value(PARAMETERS));
        }
        if let Some(allocation) = &self.allocation {
            if allocation.priority.get() == 0 {
                return Err(Error::Value(PARAMETERS));
            }
            check_extensions(&allocation.extensions)?;
        }
        if self.signalling()? && self.traffic_class != TrafficClass::Interactive {
            return Err(Error::Value(PARAMETERS));
        }
        for id in RATE_EXTENSIONS {
            if let Some((ie, Err(error))) = self.rate_extension(id)?
                && ie.criticality != Criticality::Ignore
            {
                return Err(error);
            }
        }
        let combinations = self.combination_count();
        let formats_present = self.subflows.0[0].formats.is_some();
        for flow in &self.subflows.0 {
            flow.validate()?;
            if flow.formats.is_some() != formats_present
                || flow
                    .formats
                    .as_ref()
                    .is_some_and(|formats| formats.0.len() != combinations)
                || flow.formats.is_some() && !realtime
            {
                return Err(Error::Value(PARAMETERS));
            }
        }
        if formats_present {
            let mut largest = 0;
            for index in 0..combinations {
                let combination = self.combination(index)?;
                let sum: u32 = combination
                    .subflow_bits
                    .iter()
                    .map(|bits| u32::from(*bits))
                    .sum();
                largest = largest.max(sum);
                if sum > u32::from(self.maximum_sdu_bits.get())
                    || combination.bitrate.is_some_and(|rate| {
                        maximum
                            .downlink()
                            .into_iter()
                            .chain(maximum.uplink())
                            .any(|maximum| u64::from(rate) > maximum)
                    })
                {
                    return Err(Error::Value(PARAMETERS));
                }
            }
            if largest != u32::from(self.maximum_sdu_bits.get()) {
                return Err(Error::Value(PARAMETERS));
            }
        }
        Ok(())
    }
}

fn extension_ies(list: &Option<ExtensionList>) -> impl Iterator<Item = &InformationElement> {
    list.iter().flat_map(|list| list.0.iter())
}

fn check_extensions(list: &Option<ExtensionList>) -> Result<(), Error> {
    if list.as_ref().is_some_and(|list| list.0.is_empty()) {
        return Err(Error::Value(PARAMETERS));
    }
    let mut ids = BTreeSet::new();
    for ie in extension_ies(list) {
        if !ids.insert(ie.id) {
            return Err(Error::DuplicateIe(ie.id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
