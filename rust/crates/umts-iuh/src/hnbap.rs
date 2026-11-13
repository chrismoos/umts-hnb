// SPDX-License-Identifier: AGPL-3.0-or-later

mod identity;
pub use identity::{Imsi, UeIdentity};
use rasn::Decoder;

use crate::{
    ContextId, Error,
    values::{MiscCause, ProtocolCause, TransportCause, fixed, octets},
    wire::{
        Criticality, ExtensionList, InformationElement as Ie, Limits, Outcome, Packet, Protocol,
    },
};
use rasn::{
    AsnType, Decode, Encode,
    types::{FixedBitString, OctetString},
};

const HNB_REGISTER: u8 = 1;
const HNB_DEREGISTER: u8 = 2;
const UE_REGISTER: u8 = 3;
const UE_DEREGISTER: u8 = 4;
const ERROR_INDICATION: u8 = 5;
const CAUSE: u16 = 1;
const DIAGNOSTICS: u16 = 2;
const HNB_IDENTITY: u16 = 3;
const CONTEXT: u16 = 4;
const UE_IDENTITY: u16 = 5;
const LAC: u16 = 6;
const RAC: u16 = 7;
const LOCATION: u16 = 8;
const PLMN: u16 = 9;
const SAC: u16 = 10;
const CELL: u16 = 11;
const REGISTRATION_CAUSE: u16 = 12;
const CAPABILITIES: u16 = 13;
const RNC: u16 = 14;
const CSG_ID: u16 = 15;
const BACKOFF: u16 = 16;
const ACCESS_MODE: u16 = 18;
const MUX_PORT: u16 = 19;
const MEMBERSHIP: u16 = 21;

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum RadioCause {
    Overload,
    UnauthorizedLocation,
    UnauthorizedHnb,
    ParameterMismatch,
    InvalidUeIdentity,
    UeNotAllowed,
    UeUnauthorized,
    ConnectionLost,
    RrcRelease,
    HnbNotRegistered,
    Unspecified,
    Normal,
    UeRelocated,
    RegisteredElsewhere,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
pub enum Cause {
    Radio(RadioCause),
    Transport(TransportCause),
    Protocol(ProtocolCause),
    Misc(MiscCause),
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum AccessMode {
    Closed,
    Hybrid,
    Open,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Release {
    R99,
    R4,
    R5,
    R6,
    R7,
    R8AndLater,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum CsgCapability {
    Capable,
    NotCapable,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum RegistrationCause {
    Emergency,
    Normal,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct CapabilitiesAsn {
    release: Release,
    csg: CsgCapability,
    extensions: Option<ExtensionList>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub release: Release,
    pub csg: CsgCapability,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=255"))]
struct HnbIdentityInfo(OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct HnbIdentityAsn {
    identity: HnbIdentityInfo,
    extensions: Option<ExtensionList>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("3"))]
pub(crate) struct Plmn(pub(crate) OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("2"))]
pub(crate) struct AreaCode(pub(crate) OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1"))]
pub(crate) struct Rac(pub(crate) OctetString);
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(delegate, value("1024..=65535"))]
struct MuxPort(u16);
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=3600"))]
struct Backoff(u16);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HnbParameters {
    pub identity: Vec<u8>,
    pub plmn: [u8; 3],
    pub lac: u16,
    pub rac: u8,
    pub sac: u16,
    pub cell: u32,
    pub access: AccessMode,
    pub csg: Option<u32>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    HnbRegister(HnbParameters),
    HnbAccepted {
        rnc: u16,
        mux_port: Option<u16>,
    },
    HnbRejected {
        cause: Cause,
        backoff_seconds: Option<u16>,
    },
    HnbDeregistered {
        cause: Cause,
        backoff_seconds: Option<u16>,
    },
    UeRegister {
        identity: UeIdentity,
        cause: RegistrationCause,
        capabilities: Capabilities,
    },
    UeAccepted {
        identity: UeIdentity,
        context: ContextId,
        membership: Option<crate::rua::Membership>,
    },
    UeRejected {
        identity: UeIdentity,
        cause: Cause,
    },
    UeDeregister {
        context: ContextId,
        cause: Cause,
    },
    ErrorIndication {
        cause: Option<Cause>,
        diagnostics: Option<Vec<u8>>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub message: Message,
    pub notify_ies: Vec<u16>,
}
fn cause_ie(cause: &Cause) -> Result<Ie, Error> {
    Ie::new(CAUSE, Criticality::Ignore, cause)
}
fn identity_ie(identity: &UeIdentity) -> Result<Ie, Error> {
    Ie::new(UE_IDENTITY, Criticality::Reject, &identity.to_asn()?)
}
fn context_ie(context: ContextId) -> Result<Ie, Error> {
    Ie::new(
        CONTEXT,
        Criticality::Reject,
        &fixed::<24>(&context.bytes())?,
    )
}
fn read_identity(packet: &Packet) -> Result<UeIdentity, Error> {
    UeIdentity::from_asn(packet.required(UE_IDENTITY)?.decode()?)
}
fn read_backoff(packet: &Packet, cause: &Cause) -> Result<Option<u16>, Error> {
    let value = packet
        .optional(BACKOFF)
        .map(|ie| ie.decode::<Backoff>().map(|value| value.0))
        .transpose()?;
    if *cause == Cause::Radio(RadioCause::Overload) && value.is_none() {
        return Err(Error::MissingIe(BACKOFF));
    }
    Ok(value)
}
impl Message {
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, Error> {
        let (procedure, outcome, ies, extensions) = match self {
            Self::HnbRegister(parameters) => {
                if parameters.cell >= 1 << 28
                    || parameters.csg.is_some_and(|id| id >= 1 << 27)
                    || parameters.identity.is_empty()
                    || parameters.identity.len() > 255
                {
                    return Err(Error::Identity);
                }
                if parameters.access != AccessMode::Open && parameters.csg.is_none() {
                    return Err(Error::MissingIe(CSG_ID));
                }
                let cell = (parameters.cell << 4).to_be_bytes();
                let mut ies = vec![
                    Ie::new(
                        HNB_IDENTITY,
                        Criticality::Reject,
                        &HnbIdentityAsn {
                            identity: HnbIdentityInfo(parameters.identity.clone().into()),
                            extensions: None,
                        },
                    )?,
                    // TS 25.469 HNB-Location-Information has three optional root fields. All absent encodes as one zero octet.
                    Ie {
                        id: LOCATION,
                        criticality: Criticality::Reject,
                        value: rasn::types::Any::new(vec![0]),
                    },
                    Ie::new(
                        PLMN,
                        Criticality::Reject,
                        &Plmn(parameters.plmn.to_vec().into()),
                    )?,
                    Ie::new(CELL, Criticality::Reject, &fixed::<28>(&cell)?)?,
                    Ie::new(
                        LAC,
                        Criticality::Reject,
                        &AreaCode(parameters.lac.to_be_bytes().to_vec().into()),
                    )?,
                    Ie::new(RAC, Criticality::Reject, &Rac(vec![parameters.rac].into()))?,
                    Ie::new(
                        SAC,
                        Criticality::Reject,
                        &AreaCode(parameters.sac.to_be_bytes().to_vec().into()),
                    )?,
                ];
                if let Some(id) = parameters.csg {
                    ies.push(Ie::new(
                        CSG_ID,
                        Criticality::Reject,
                        &fixed::<27>(&(id << 5).to_be_bytes())?,
                    )?);
                }
                let extensions = vec![Ie::new(
                    ACCESS_MODE,
                    Criticality::Reject,
                    &parameters.access,
                )?];
                (HNB_REGISTER, Outcome::Initiating, ies, extensions)
            }
            Self::HnbAccepted { rnc, mux_port } => (
                HNB_REGISTER,
                Outcome::Successful,
                vec![Ie::new(RNC, Criticality::Reject, rnc)?],
                mux_port
                    .map(|port| Ie::new(MUX_PORT, Criticality::Ignore, &MuxPort(port)))
                    .transpose()?
                    .into_iter()
                    .collect(),
            ),
            Self::HnbRejected {
                cause,
                backoff_seconds,
            }
            | Self::HnbDeregistered {
                cause,
                backoff_seconds,
            } => {
                if *cause == Cause::Radio(RadioCause::Overload) && backoff_seconds.is_none() {
                    return Err(Error::MissingIe(BACKOFF));
                }
                let mut ies = vec![cause_ie(cause)?];
                if let Some(seconds) = backoff_seconds {
                    ies.push(Ie::new(BACKOFF, Criticality::Reject, &Backoff(*seconds))?);
                }
                if matches!(self, Self::HnbRejected { .. }) {
                    (HNB_REGISTER, Outcome::Unsuccessful, ies, vec![])
                } else {
                    (HNB_DEREGISTER, Outcome::Initiating, ies, vec![])
                }
            }
            Self::UeRegister {
                identity,
                cause,
                capabilities,
            } => (
                UE_REGISTER,
                Outcome::Initiating,
                vec![
                    identity_ie(identity)?,
                    Ie::new(REGISTRATION_CAUSE, Criticality::Ignore, cause)?,
                    Ie::new(
                        CAPABILITIES,
                        Criticality::Reject,
                        &CapabilitiesAsn {
                            release: capabilities.release,
                            csg: capabilities.csg,
                            extensions: None,
                        },
                    )?,
                ],
                vec![],
            ),
            Self::UeAccepted {
                identity,
                context,
                membership,
            } => (
                UE_REGISTER,
                Outcome::Successful,
                vec![identity_ie(identity)?, context_ie(*context)?],
                membership
                    .map(|membership| Ie::new(MEMBERSHIP, Criticality::Reject, &membership))
                    .transpose()?
                    .into_iter()
                    .collect(),
            ),
            Self::UeRejected { identity, cause } => (
                UE_REGISTER,
                Outcome::Unsuccessful,
                vec![identity_ie(identity)?, cause_ie(cause)?],
                vec![],
            ),
            Self::UeDeregister { context, cause } => (
                UE_DEREGISTER,
                Outcome::Initiating,
                vec![context_ie(*context)?, cause_ie(cause)?],
                vec![],
            ),
            Self::ErrorIndication { cause, diagnostics } => {
                let mut ies = Vec::new();
                if let Some(cause) = cause {
                    ies.push(cause_ie(cause)?);
                }
                if let Some(bytes) = diagnostics {
                    ies.push(Ie {
                        id: DIAGNOSTICS,
                        criticality: Criticality::Ignore,
                        value: rasn::types::Any::new(bytes.clone()),
                    });
                }
                (ERROR_INDICATION, Outcome::Initiating, ies, vec![])
            }
        };
        let mut packet = Packet {
            protocol: Protocol::Hnbap,
            outcome,
            procedure,
            criticality: if procedure == HNB_REGISTER || procedure == UE_REGISTER {
                Criticality::Reject
            } else {
                Criticality::Ignore
            },
            ies,
            extensions,
        };
        if outcome != Outcome::Initiating {
            packet.criticality = Criticality::Reject;
        }
        packet.encode(limits)
    }
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<Decoded, Error> {
        let packet = Packet::decode(Protocol::Hnbap, bytes, limits)?;
        let (known, extensions): (&[u16], &[u16]) = match (packet.procedure, packet.outcome) {
            (HNB_REGISTER, Outcome::Initiating) => (
                &[HNB_IDENTITY, LOCATION, PLMN, CELL, LAC, RAC, SAC, CSG_ID],
                &[ACCESS_MODE],
            ),
            (HNB_REGISTER, Outcome::Successful) => (&[RNC], &[MUX_PORT]),
            (HNB_REGISTER, Outcome::Unsuccessful) | (HNB_DEREGISTER, Outcome::Initiating) => {
                (&[CAUSE, DIAGNOSTICS, BACKOFF], &[])
            }
            (UE_REGISTER, Outcome::Initiating) => {
                (&[UE_IDENTITY, REGISTRATION_CAUSE, CAPABILITIES], &[])
            }
            (UE_REGISTER, Outcome::Successful) => (&[UE_IDENTITY, CONTEXT], &[MEMBERSHIP]),
            (UE_REGISTER, Outcome::Unsuccessful) => (&[UE_IDENTITY, CAUSE, DIAGNOSTICS], &[]),
            (UE_DEREGISTER, Outcome::Initiating) => (&[CONTEXT, CAUSE], &[]),
            (ERROR_INDICATION, Outcome::Initiating) => (&[CAUSE, DIAGNOSTICS], &[]),
            _ => return Err(Error::Unsupported),
        };
        let notify_ies = packet.check(known, extensions)?;
        let message = match (packet.procedure, packet.outcome) {
            (HNB_REGISTER, Outcome::Initiating) => {
                let identity = packet.required(HNB_IDENTITY)?.decode::<HnbIdentityAsn>()?;
                if packet.required(LOCATION)?.value.as_bytes() != [0] {
                    return Err(Error::Unsupported);
                }
                let plmn = packet
                    .required(PLMN)?
                    .decode::<Plmn>()?
                    .0
                    .as_ref()
                    .try_into()
                    .map_err(|_| Error::Value(PLMN))?;
                let cell_bytes = octets(&packet.required(CELL)?.decode::<FixedBitString<28>>()?);
                let cell =
                    u32::from_be_bytes(cell_bytes.try_into().map_err(|_| Error::Value(CELL))?) >> 4;
                let lac = packet.required(LAC)?.decode::<AreaCode>()?;
                let sac = packet.required(SAC)?.decode::<AreaCode>()?;
                let rac = packet.required(RAC)?.decode::<Rac>()?.0[0];
                let csg = packet
                    .optional(CSG_ID)
                    .map(|ie| {
                        ie.decode::<FixedBitString<27>>().and_then(|bits| {
                            Ok(u32::from_be_bytes(
                                octets(&bits).try_into().map_err(|_| Error::Value(CSG_ID))?,
                            ) >> 5)
                        })
                    })
                    .transpose()?;
                let access = packet
                    .extensions
                    .iter()
                    .find(|ie| ie.id == ACCESS_MODE)
                    .map(Ie::decode)
                    .transpose()?
                    .unwrap_or(AccessMode::Closed);
                if access != AccessMode::Open && csg.is_none() {
                    return Err(Error::MissingIe(CSG_ID));
                }
                Self::HnbRegister(HnbParameters {
                    identity: identity.identity.0.to_vec(),
                    plmn,
                    lac: u16::from_be_bytes(
                        lac.0.as_ref().try_into().map_err(|_| Error::Value(LAC))?,
                    ),
                    rac,
                    sac: u16::from_be_bytes(
                        sac.0.as_ref().try_into().map_err(|_| Error::Value(SAC))?,
                    ),
                    cell,
                    access,
                    csg,
                })
            }
            (HNB_REGISTER, Outcome::Successful) => Self::HnbAccepted {
                rnc: packet.required(RNC)?.decode()?,
                mux_port: packet
                    .extensions
                    .iter()
                    .find(|ie| ie.id == MUX_PORT)
                    .map(|ie| ie.decode::<MuxPort>().map(|port| port.0))
                    .transpose()?,
            },
            (HNB_REGISTER, Outcome::Unsuccessful) | (HNB_DEREGISTER, Outcome::Initiating) => {
                let cause = packet.required(CAUSE)?.decode::<Cause>()?;
                let backoff_seconds = read_backoff(&packet, &cause)?;
                if packet.outcome == Outcome::Unsuccessful {
                    Self::HnbRejected {
                        cause,
                        backoff_seconds,
                    }
                } else {
                    Self::HnbDeregistered {
                        cause,
                        backoff_seconds,
                    }
                }
            }
            (UE_REGISTER, Outcome::Initiating) => {
                let capabilities = packet.required(CAPABILITIES)?.decode::<CapabilitiesAsn>()?;
                Self::UeRegister {
                    identity: read_identity(&packet)?,
                    cause: packet.required(REGISTRATION_CAUSE)?.decode()?,
                    capabilities: Capabilities {
                        release: capabilities.release,
                        csg: capabilities.csg,
                    },
                }
            }
            (UE_REGISTER, Outcome::Successful) => Self::UeAccepted {
                identity: read_identity(&packet)?,
                context: ContextId::from_bytes(&octets(
                    &packet.required(CONTEXT)?.decode::<FixedBitString<24>>()?,
                ))?,
                membership: packet
                    .extensions
                    .iter()
                    .find(|ie| ie.id == MEMBERSHIP)
                    .map(Ie::decode)
                    .transpose()?,
            },
            (UE_REGISTER, Outcome::Unsuccessful) => Self::UeRejected {
                identity: read_identity(&packet)?,
                cause: packet.required(CAUSE)?.decode()?,
            },
            (UE_DEREGISTER, Outcome::Initiating) => Self::UeDeregister {
                context: ContextId::from_bytes(&octets(
                    &packet.required(CONTEXT)?.decode::<FixedBitString<24>>()?,
                ))?,
                cause: packet.required(CAUSE)?.decode()?,
            },
            (ERROR_INDICATION, Outcome::Initiating) => Self::ErrorIndication {
                cause: packet.optional(CAUSE).map(Ie::decode).transpose()?,
                diagnostics: packet
                    .optional(DIAGNOSTICS)
                    .map(|ie| ie.value.as_bytes().to_vec()),
            },
            _ => unreachable!("validated HNBAP procedure"),
        };
        Ok(Decoded {
            message,
            notify_ies,
        })
    }
}

#[cfg(test)]
mod tests;
