// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{
    ContextId, Domain, Error, NasSelector,
    values::{MiscCause, ProtocolCause, TransportCause, fixed, octets},
    wire::{Criticality, InformationElement as Ie, Limits, Outcome, Packet, Protocol},
};
use rasn::{
    AsnType, Decode, Encode,
    types::{FixedBitString, OctetString},
};

const CONNECT: u8 = 1;
const DIRECT: u8 = 2;
const DISCONNECT: u8 = 3;
const CONNECTIONLESS: u8 = 4;
const ERROR_INDICATION: u8 = 5;
const CAUSE: u16 = 1;
const DIAGNOSTICS: u16 = 2;
const CONTEXT: u16 = 3;
const RANAP: u16 = 4;
const SELECTOR: u16 = 5;
const ESTABLISHMENT: u16 = 6;
const DOMAIN: u16 = 7;
const MEMBERSHIP: u16 = 9;

#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum EstablishmentCause {
    Emergency,
    Normal,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum RadioCause {
    Normal,
    ConnectFailed,
    NetworkRelease,
    Unspecified,
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
pub enum Membership {
    Member,
    NonMember,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Connect {
        domain: Domain,
        context: ContextId,
        cause: EstablishmentCause,
        selector: Option<NasSelector>,
        membership: Option<Membership>,
        ranap: Vec<u8>,
    },
    Direct {
        domain: Domain,
        context: ContextId,
        ranap: Vec<u8>,
    },
    Disconnect {
        domain: Domain,
        context: ContextId,
        cause: Cause,
        ranap: Option<Vec<u8>>,
    },
    Connectionless {
        ranap: Vec<u8>,
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
fn context_ie(context: ContextId) -> Result<Ie, Error> {
    Ie::new(
        CONTEXT,
        Criticality::Reject,
        &fixed::<24>(&context.bytes())?,
    )
}
fn domain_ie(domain: Domain) -> Result<Ie, Error> {
    Ie::new(DOMAIN, Criticality::Reject, &domain)
}
fn ranap_ie(bytes: &[u8]) -> Result<Ie, Error> {
    if bytes.is_empty() {
        return Err(Error::Length);
    }
    Ie::new(
        RANAP,
        Criticality::Reject,
        &OctetString::from(bytes.to_vec()),
    )
}
fn read_context(packet: &Packet) -> Result<ContextId, Error> {
    ContextId::from_bytes(&octets(
        &packet.required(CONTEXT)?.decode::<FixedBitString<24>>()?,
    ))
}
fn read_ranap(packet: &Packet) -> Result<Vec<u8>, Error> {
    let value = packet.required(RANAP)?.decode::<OctetString>()?.to_vec();
    if value.is_empty() {
        Err(Error::Value(RANAP))
    } else {
        Ok(value)
    }
}
impl Message {
    pub fn encode(&self, limits: Limits) -> Result<Vec<u8>, Error> {
        let (procedure, ies, extensions) = match self {
            Self::Connect {
                domain,
                context,
                cause,
                selector,
                membership,
                ranap,
            } => {
                let mut ies = vec![domain_ie(*domain)?, context_ie(*context)?];
                if let Some(selector) = selector {
                    ies.push(Ie::new(SELECTOR, Criticality::Ignore, selector)?);
                }
                ies.push(Ie::new(ESTABLISHMENT, Criticality::Reject, cause)?);
                ies.push(ranap_ie(ranap)?);
                let extensions = membership
                    .map(|membership| Ie::new(MEMBERSHIP, Criticality::Ignore, &membership))
                    .transpose()?
                    .into_iter()
                    .collect();
                (CONNECT, ies, extensions)
            }
            Self::Direct {
                domain,
                context,
                ranap,
            } => (
                DIRECT,
                vec![domain_ie(*domain)?, context_ie(*context)?, ranap_ie(ranap)?],
                vec![],
            ),
            Self::Disconnect {
                domain,
                context,
                cause,
                ranap,
            } => {
                if *cause == Cause::Radio(RadioCause::Normal) && ranap.is_none() {
                    return Err(Error::MissingIe(RANAP));
                }
                let mut ies = vec![
                    domain_ie(*domain)?,
                    context_ie(*context)?,
                    Ie::new(CAUSE, Criticality::Reject, cause)?,
                ];
                if let Some(ranap) = ranap {
                    ies.push(ranap_ie(ranap)?);
                }
                (DISCONNECT, ies, vec![])
            }
            Self::Connectionless { ranap } => (CONNECTIONLESS, vec![ranap_ie(ranap)?], vec![]),
            Self::ErrorIndication { cause, diagnostics } => {
                let mut ies = Vec::new();
                if let Some(cause) = cause {
                    ies.push(Ie::new(CAUSE, Criticality::Ignore, cause)?);
                }
                if let Some(bytes) = diagnostics {
                    ies.push(Ie {
                        id: DIAGNOSTICS,
                        criticality: Criticality::Ignore,
                        value: rasn::types::Any::new(bytes.clone()),
                    });
                }
                (ERROR_INDICATION, ies, vec![])
            }
        };
        // TS 25.468 9.3.2 assigns ignore to these class-2 procedures, independently of IE criticality.
        let mut packet = Packet::initiating(Protocol::Rua, procedure, Criticality::Ignore, ies);
        packet.extensions = extensions;
        packet.encode(limits)
    }
    pub fn decode(bytes: &[u8], limits: Limits) -> Result<Decoded, Error> {
        let packet = Packet::decode(Protocol::Rua, bytes, limits)?;
        if packet.outcome != Outcome::Initiating {
            return Err(Error::Unsupported);
        }
        let (known, known_extensions): (&[u16], &[u16]) = match packet.procedure {
            CONNECT => (
                &[DOMAIN, CONTEXT, SELECTOR, ESTABLISHMENT, RANAP],
                &[MEMBERSHIP],
            ),
            DIRECT => (&[DOMAIN, CONTEXT, RANAP], &[]),
            DISCONNECT => (&[DOMAIN, CONTEXT, CAUSE, RANAP], &[]),
            CONNECTIONLESS => (&[RANAP], &[]),
            ERROR_INDICATION => (&[CAUSE, DIAGNOSTICS], &[]),
            _ => return Err(Error::Unsupported),
        };
        let notify_ies = packet.check(known, known_extensions)?;
        let message = match packet.procedure {
            CONNECT => Self::Connect {
                domain: packet.required(DOMAIN)?.decode()?,
                context: read_context(&packet)?,
                cause: packet.required(ESTABLISHMENT)?.decode()?,
                selector: packet.optional(SELECTOR).map(Ie::decode).transpose()?,
                membership: packet
                    .extensions
                    .iter()
                    .find(|ie| ie.id == MEMBERSHIP)
                    .map(Ie::decode)
                    .transpose()?,
                ranap: read_ranap(&packet)?,
            },
            DIRECT => Self::Direct {
                domain: packet.required(DOMAIN)?.decode()?,
                context: read_context(&packet)?,
                ranap: read_ranap(&packet)?,
            },
            DISCONNECT => {
                let cause = packet.required(CAUSE)?.decode::<Cause>()?;
                let ranap = if packet.optional(RANAP).is_some() {
                    Some(read_ranap(&packet)?)
                } else {
                    None
                };
                if cause == Cause::Radio(RadioCause::Normal) && ranap.is_none() {
                    return Err(Error::MissingIe(RANAP));
                }
                Self::Disconnect {
                    domain: packet.required(DOMAIN)?.decode()?,
                    context: read_context(&packet)?,
                    cause,
                    ranap,
                }
            }
            CONNECTIONLESS => Self::Connectionless {
                ranap: read_ranap(&packet)?,
            },
            ERROR_INDICATION => Self::ErrorIndication {
                cause: packet.optional(CAUSE).map(Ie::decode).transpose()?,
                diagnostics: packet
                    .optional(DIAGNOSTICS)
                    .map(|ie| ie.value.as_bytes().to_vec()),
            },
            _ => unreachable!("validated RUA procedure"),
        };
        Ok(Decoded {
            message,
            notify_ies,
        })
    }
}

#[cfg(test)]
mod tests;
