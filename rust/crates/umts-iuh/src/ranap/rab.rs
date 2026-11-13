// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Criticality, Error, Ie, Limits, Outcome, Packet, Protocol, RAB_ASSIGNMENT};
use crate::{
    Domain,
    values::{fixed, octets},
    wire::{ExtensionList, decode_value, encode_value, preflight_rab_list},
};
use rasn::Decoder;
use rasn::{
    AsnType, Decode, Encode,
    types::{Any, BitString, FixedBitString, FixedOctetString, SequenceOf},
};

mod ip_transport;
pub mod qos;
pub use ip_transport::Peer;
mod response;
pub use response::{AssignmentResponse, Failed, Queued, Released, Setup, Volume, Volumes};

const SETUP_LIST: u16 = 54;
const SETUP_ITEM: u16 = 53;
const RELEASE_LIST: u16 = 41;
const RELEASE_ITEM: u16 = 40;
const MAX_RABS: usize = 256;
const NAS_SYNCHRONISATION_BITS: usize = 4;

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate)]
pub struct RabId(FixedBitString<8>);
impl RabId {
    pub fn new(id: u8) -> Self {
        Self(fixed(&[id]).expect("eight-bit RAB ID"))
    }
    pub fn get(&self) -> u8 {
        octets(&self.0)[0]
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=160", extensible))]
pub struct Address(BitString);
impl Address {
    pub fn new(bits: BitString) -> Result<Self, Error> {
        if bits.is_empty() || bits.len() > 160 {
            return Err(Error::Length);
        }
        Ok(Self(bits))
    }
    pub fn bits(&self) -> &BitString {
        &self.0
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
pub enum Association {
    Gtp(FixedOctetString<4>),
    Binding(FixedOctetString<4>),
}
impl Association {
    pub fn gtp(teid: u32) -> Self {
        Self::Gtp(teid.to_be_bytes().into())
    }
    pub fn binding(bytes: [u8; 4]) -> Self {
        Self::Binding(bytes.into())
    }
    pub fn validate_domain(&self, domain: Domain) -> Result<(), Error> {
        match (self, domain) {
            (Self::Gtp(_), Domain::Ps) | (Self::Binding(_), Domain::Cs) => Ok(()),
            _ => Err(Error::Identity),
        }
    }
    pub fn binding_port(&self) -> Option<u16> {
        match self {
            Self::Binding(value) => Some(u16::from_be_bytes([value[0], value[1]])),
            _ => None,
        }
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Transport {
    pub address: Address,
    pub association: Association,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Transport {
    pub fn new(address: Address, association: Association) -> Self {
        Self {
            address,
            association,
            extensions: None,
        }
    }
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Mode {
    Transparent,
    PredefinedSdu,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct UserPlane {
    pub mode: Mode,
    pub versions: FixedBitString<16>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl UserPlane {
    pub fn new(mode: Mode, versions: u16) -> Result<Self, Error> {
        if versions == 0 {
            return Err(Error::Value(SETUP_ITEM));
        }
        Ok(Self {
            mode,
            versions: fixed(&versions.to_be_bytes())?,
            extensions: None,
        })
    }
    pub fn version_mask(&self) -> u16 {
        let bytes = octets(&self.versions);
        u16::from_be_bytes([bytes[0], bytes[1]])
    }
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Handover {
    Should,
    ShouldNot,
    ShallNot,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum Reporting {
    Report,
    DoNotReport,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum PdpType {
    Empty,
    Ppp,
    OspIhoss,
    Ipv4,
    Ipv6,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=2"))]
pub struct PdpTypes(SequenceOf<PdpType>);
impl PdpTypes {
    pub fn new(values: Vec<PdpType>) -> Result<Self, Error> {
        if values.is_empty() || values.len() > 2 {
            return Err(Error::Length);
        }
        Ok(Self(values))
    }
    pub fn values(&self) -> &[PdpType] {
        &self.0
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct First {
    pub id: RabId,
    pub nas_synchronization: Option<FixedBitString<4>>,
    pub parameters: Option<qos::Parameters>,
    pub user_plane: Option<UserPlane>,
    pub transport: Option<Transport>,
    pub handover: Option<Handover>,
    pub(crate) extensions: Option<ExtensionList>,
}
impl First {
    pub fn nas_synchronization_value(&self) -> Option<u8> {
        self.nas_synchronization.as_ref().map(|bits| {
            // The fixed-bit-string backing array includes padding beyond the four NAS wire bits.
            bits.iter()
                .take(NAS_SYNCHRONISATION_BITS)
                .fold(0u8, |value, bit| (value << 1) | u8::from(*bit))
        })
    }
    pub fn modification(id: u8) -> Self {
        Self {
            id: RabId::new(id),
            nas_synchronization: None,
            parameters: None,
            user_plane: None,
            transport: None,
            handover: None,
            extensions: None,
        }
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, Default, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Second {
    pub pdp_types: Option<PdpTypes>,
    pub reporting: Option<Reporting>,
    pub downlink_gtp_sequence: Option<u16>,
    pub uplink_gtp_sequence: Option<u16>,
    pub downlink_npdu_sequence: Option<u16>,
    pub uplink_npdu_sequence: Option<u16>,
    pub(crate) extensions: Option<ExtensionList>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupOrModify {
    pub first: First,
    pub second: Second,
}
impl SetupOrModify {
    pub fn merged(&self, previous: &Self) -> Result<Self, Error> {
        if self.first.id != previous.first.id {
            return Err(Error::Identity);
        }
        let mut merged = self.clone();
        macro_rules! inherit {
            ($part:ident,$($field:ident),+)=>{$(if merged.$part.$field.is_none() {merged.$part.$field.clone_from(&previous.$part.$field);})+};
        }
        inherit!(
            first,
            nas_synchronization,
            parameters,
            user_plane,
            transport,
            handover
        );
        inherit!(
            second,
            pdp_types,
            reporting,
            downlink_gtp_sequence,
            uplink_gtp_sequence,
            downlink_npdu_sequence,
            uplink_npdu_sequence
        );
        merged.first.extensions = merge_extensions(
            self.first.extensions.as_ref(),
            previous.first.extensions.as_ref(),
        )?;
        merged.second.extensions = merge_extensions(
            self.second.extensions.as_ref(),
            previous.second.extensions.as_ref(),
        )?;
        Ok(merged)
    }
    pub fn validate_new(&self, domain: Domain) -> Result<(), Error> {
        if self.first.parameters.is_none()
            || self.first.user_plane.is_none()
            || self.first.transport.is_none()
        {
            return Err(Error::MissingIe(SETUP_ITEM));
        }
        self.validate(domain)
    }
    pub fn validate(&self, domain: Domain) -> Result<(), Error> {
        if let Some(parameters) = &self.first.parameters {
            parameters.validate()?;
            if domain == Domain::Cs && parameters.signalling()? {
                return Err(Error::Value(SETUP_ITEM));
            }
            if self
                .first
                .user_plane
                .as_ref()
                .is_some_and(|plane| plane.mode == Mode::Transparent)
                && parameters
                    .subflows
                    .as_slice()
                    .iter()
                    .any(|flow| flow.formats.is_some())
            {
                return Err(Error::Value(SETUP_ITEM));
            }
        }
        if let Some(plane) = &self.first.user_plane
            && plane.version_mask() == 0
        {
            return Err(Error::Value(SETUP_ITEM));
        }
        if let Some(transport) = &self.first.transport {
            transport.association.validate_domain(domain)?
        }
        Ok(())
    }
}

macro_rules! cause_integer {
    ($name:ident,$ty:ty,$range:literal) => {
        #[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
        #[rasn(delegate, value($range))]
        pub struct $name($ty);
    };
}
cause_integer!(Radio, u8, "1..=64");
cause_integer!(Transmission, u8, "65..=80");
cause_integer!(Nas, u8, "81..=96");
cause_integer!(ProtocolCause, u8, "97..=112");
cause_integer!(Misc, u8, "113..=128");
cause_integer!(NonStandard, u16, "129..=256");
cause_integer!(RadioExtension, u16, "257..=512");
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
pub enum Cause {
    Radio(Radio),
    Transmission(Transmission),
    Nas(Nas),
    Protocol(ProtocolCause),
    Misc(Misc),
    NonStandard(NonStandard),
    #[rasn(extension_addition)]
    RadioExtension(RadioExtension),
}
impl Cause {
    pub fn from_code(code: u16) -> Result<Self, Error> {
        Ok(match code {
            1..=64 => Self::Radio(Radio(code as u8)),
            65..=80 => Self::Transmission(Transmission(code as u8)),
            81..=96 => Self::Nas(Nas(code as u8)),
            97..=112 => Self::Protocol(ProtocolCause(code as u8)),
            113..=128 => Self::Misc(Misc(code as u8)),
            129..=255 => Self::NonStandard(NonStandard(code)),
            257..=512 => Self::RadioExtension(RadioExtension(code)),
            _ => return Err(Error::Value(RELEASE_ITEM)),
        })
    }
    pub fn code(&self) -> u16 {
        match self {
            Self::Radio(v) => u16::from(v.0),
            Self::Transmission(v) => u16::from(v.0),
            Self::Nas(v) => u16::from(v.0),
            Self::Protocol(v) => u16::from(v.0),
            Self::Misc(v) => u16::from(v.0),
            Self::NonStandard(v) => v.0,
            Self::RadioExtension(v) => v.0,
        }
    }
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
pub struct Release {
    pub id: RabId,
    pub cause: Cause,
    pub(crate) extensions: Option<ExtensionList>,
}
impl Release {
    pub fn new(id: u8, cause: Cause) -> Self {
        Self {
            id: RabId::new(id),
            cause,
            extensions: None,
        }
    }
}

#[derive(AsnType, Decode, Encode)]
#[rasn(automatic_tags)]
struct Pair {
    id: u16,
    first_criticality: Criticality,
    first: Any,
    second_criticality: Criticality,
    second: Any,
}
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, size("0..=65535"))]
struct PairContainer(SequenceOf<Pair>);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, size("1..=256"))]
struct PairList(SequenceOf<PairContainer>);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, size("0..=65535"))]
struct Container(SequenceOf<Ie>);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, size("1..=256"))]
struct List(SequenceOf<Container>);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    pub setup: Vec<SetupOrModify>,
    pub release: Vec<Release>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedAssignment {
    pub assignment: Assignment,
    pub notify_ies: Vec<u16>,
}

impl Assignment {
    pub fn validate(&self) -> Result<(), Error> {
        if self.setup.is_empty() && self.release.is_empty() {
            return Err(Error::Length);
        }
        unique_ids(
            self.setup
                .iter()
                .map(|item| item.first.id.get())
                .chain(self.release.iter().map(|item| item.id.get())),
        )?;
        for item in &self.release {
            Cause::from_code(item.cause.code())?;
        }
        Ok(())
    }
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut ies = Vec::new();
        if !self.setup.is_empty() {
            let pairs = self
                .setup
                .iter()
                .map(|item| {
                    Ok(PairContainer(vec![Pair {
                        id: SETUP_ITEM,
                        first_criticality: Criticality::Reject,
                        first: Any::new(encode_value(&item.first)?),
                        second_criticality: Criticality::Ignore,
                        second: Any::new(encode_value(&item.second)?),
                    }]))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let ie = Ie::new(SETUP_LIST, Criticality::Ignore, &PairList(pairs))?;
            preflight_rab_list(ie.value.as_bytes(), true, limits)?;
            ies.push(ie);
        }
        if !self.release.is_empty() {
            ies.push(encode_list(
                RELEASE_LIST,
                RELEASE_ITEM,
                &self.release,
                limits,
            )?);
        }
        Packet::initiating(Protocol::Ranap, RAB_ASSIGNMENT, Criticality::Reject, ies).encode(limits)
    }
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<DecodedAssignment, Error> {
        Self::from_packet(Packet::decode(Protocol::Ranap, bytes, limits)?, limits)
    }
    pub(super) fn from_packet(packet: Packet, limits: Limits) -> Result<DecodedAssignment, Error> {
        if packet.procedure != RAB_ASSIGNMENT || packet.outcome != Outcome::Initiating {
            return Err(Error::Unsupported);
        }
        let mut notify_ies = packet.check(&[SETUP_LIST, RELEASE_LIST], &[])?;
        let mut setup = Vec::new();
        if let Some(ie) = packet.optional(SETUP_LIST) {
            preflight_rab_list(ie.value.as_bytes(), true, limits)?;
            let list: PairList = ie.decode()?;
            for container in list.0 {
                for pair in container.0 {
                    if pair.id != SETUP_ITEM {
                        unknown(pair.id, pair.first_criticality, &mut notify_ies)?;
                        unknown(pair.id, pair.second_criticality, &mut notify_ies)?;
                        continue;
                    }
                    let first: First = decode_value(pair.first.as_bytes())?;
                    let second: Second = decode_value(pair.second.as_bytes())?;
                    inspect_extensions(first.extensions.as_ref(), &mut notify_ies)?;
                    inspect_extensions(second.extensions.as_ref(), &mut notify_ies)?;
                    if let Some(parameters) = &first.parameters {
                        for ie in parameters.unknown_extension_ies() {
                            unknown(ie.id, ie.criticality, &mut notify_ies)?;
                        }
                    }
                    if let Some(plane) = &first.user_plane {
                        inspect_extensions(plane.extensions.as_ref(), &mut notify_ies)?
                    }
                    if let Some(transport) = &first.transport {
                        inspect_extensions(transport.extensions.as_ref(), &mut notify_ies)?
                    }
                    setup.push(SetupOrModify { first, second });
                }
            }
        }
        let release =
            decode_list::<Release>(&packet, RELEASE_LIST, RELEASE_ITEM, limits, &mut notify_ies)?;
        for item in &release {
            inspect_extensions(item.extensions.as_ref(), &mut notify_ies)?
        }
        let assignment = Self { setup, release };
        assignment.validate()?;
        Ok(DecodedAssignment {
            assignment,
            notify_ies,
        })
    }
}

fn unique_ids(ids: impl Iterator<Item = u8>) -> Result<(), Error> {
    let mut seen = [false; MAX_RABS];
    for id in ids {
        if std::mem::replace(&mut seen[usize::from(id)], true) {
            return Err(Error::Identity);
        }
    }
    Ok(())
}
fn unknown(id: u16, criticality: Criticality, notify: &mut Vec<u16>) -> Result<(), Error> {
    match criticality {
        Criticality::Reject => Err(Error::UnknownRejectIe(id)),
        Criticality::Notify => {
            if !notify.contains(&id) {
                notify.push(id)
            }
            Ok(())
        }
        Criticality::Ignore => Ok(()),
    }
}
fn inspect_extensions(list: Option<&ExtensionList>, notify: &mut Vec<u16>) -> Result<(), Error> {
    if let Some(list) = list {
        for (index, ie) in list.0.iter().enumerate() {
            if list.0[..index].iter().any(|other| other.id == ie.id) {
                return Err(Error::DuplicateIe(ie.id));
            }
            unknown(ie.id, ie.criticality, notify)?;
        }
    }
    Ok(())
}
fn merge_extensions(
    current: Option<&ExtensionList>,
    previous: Option<&ExtensionList>,
) -> Result<Option<ExtensionList>, Error> {
    for list in [current, previous].into_iter().flatten() {
        for (index, ie) in list.0.iter().enumerate() {
            if list.0[..index].iter().any(|old| old.id == ie.id) {
                return Err(Error::DuplicateIe(ie.id));
            }
        }
    }
    let Some(current) = current else {
        return Ok(previous.cloned());
    };
    let mut values = previous.map_or_else(Vec::new, |list| list.0.clone());
    for ie in &current.0 {
        if let Some(index) = values.iter().position(|old| old.id == ie.id) {
            values[index] = ie.clone();
        } else {
            values.push(ie.clone());
        }
    }
    if values.is_empty() || values.len() > usize::from(u16::MAX) {
        return Err(Error::Capacity);
    }
    Ok(Some(ExtensionList(values)))
}
pub(super) fn encode_list<T: Encode>(
    id: u16,
    item: u16,
    values: &[T],
    limits: Limits,
) -> Result<Ie, Error> {
    let list = values
        .iter()
        .map(|value| Ok(Container(vec![Ie::new(item, Criticality::Ignore, value)?])))
        .collect::<Result<Vec<_>, Error>>()?;
    let ie = Ie::new(id, Criticality::Ignore, &List(list))?;
    preflight_rab_list(ie.value.as_bytes(), false, limits)?;
    Ok(ie)
}
fn decode_list<T: Decode>(
    packet: &Packet,
    id: u16,
    item: u16,
    limits: Limits,
    notify: &mut Vec<u16>,
) -> Result<Vec<T>, Error> {
    let Some(ie) = packet.optional(id) else {
        return Ok(Vec::new());
    };
    preflight_rab_list(ie.value.as_bytes(), false, limits)?;
    let list: List = ie.decode()?;
    let mut result = Vec::new();
    for container in list.0 {
        for ie in container.0 {
            if ie.id == item {
                result.push(ie.decode()?)
            } else {
                unknown(ie.id, ie.criticality, notify)?
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod merge_tests;
#[cfg(test)]
mod tests;
