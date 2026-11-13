// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::Error;
use rasn::Decoder;
use rasn::{AsnType, Decode, Encode, types::FixedBitString};
use umts_types::CnDomain;

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
pub enum Domain {
    Cs,
    Ps,
}
impl From<CnDomain> for Domain {
    fn from(value: CnDomain) -> Self {
        match value {
            CnDomain::Cs => Self::Cs,
            CnDomain::Ps => Self::Ps,
        }
    }
}
impl From<Domain> for CnDomain {
    fn from(value: Domain) -> Self {
        match value {
            Domain::Cs => Self::Cs,
            Domain::Ps => Self::Ps,
        }
    }
}

pub(crate) fn fixed<const N: usize>(bytes: &[u8]) -> Result<FixedBitString<N>, Error> {
    if bytes.len() != N.div_ceil(8) {
        return Err(Error::Length);
    }
    let mut bits = FixedBitString::<N>::default();
    for index in 0..N {
        bits.set(index, bytes[index / 8] & (1 << (7 - index % 8)) != 0);
    }
    Ok(bits)
}
pub(crate) fn octets<const N: usize>(bits: &FixedBitString<N>) -> Vec<u8> {
    let mut bytes = vec![0; N.div_ceil(8)];
    for index in 0..N {
        if bits[index] {
            bytes[index / 8] |= 1 << (7 - index % 8);
        }
    }
    bytes
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
pub enum RoutingBasis {
    LocalPtmsi(FixedBitString<10>),
    TmsiSamePlmn(FixedBitString<10>),
    TmsiOtherPlmn(FixedBitString<10>),
    ImsiPaging(FixedBitString<10>),
    ImsiUeEvent(FixedBitString<10>),
    Imei(FixedBitString<10>),
    Spare2(FixedBitString<10>),
    Spare1(FixedBitString<10>),
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct GsmSelector {
    pub routing: RoutingBasis,
    pub dummy: bool,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
pub enum SelectorCn {
    Gsm(GsmSelector),
    Ansi41(FixedBitString<14>),
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct Release99Selector {
    pub cn: SelectorCn,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct FutureSelector {
    pub coding: FixedBitString<15>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
pub enum SelectorVersion {
    Release99(Release99Selector),
    Later(FutureSelector),
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
pub struct NasSelector {
    pub version: SelectorVersion,
}

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum TransportCause {
    ResourceUnavailable,
    Unspecified,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum ProtocolCause {
    TransferSyntax,
    AbstractSyntaxReject,
    AbstractSyntaxIgnoreNotify,
    IncompatibleState,
    Semantic,
    Unspecified,
    FalselyConstructed,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum MiscCause {
    ProcessingOverload,
    HardwareFailure,
    OmIntervention,
    Unspecified,
}
