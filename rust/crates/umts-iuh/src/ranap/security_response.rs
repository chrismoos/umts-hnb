// SPDX-License-Identifier: AGPL-3.0-or-later

use super::{Algorithm, Criticality, Error, Ie, Limits, Outcome, Packet, Protocol, SECURITY_MODE};
use rasn::{AsnType, Decode, Encode};

const CAUSE: u16 = 4;
const CHOSEN_ENCRYPTION: u16 = 5;
const CHOSEN_INTEGRITY: u16 = 6;
const DIAGNOSTICS: u16 = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SecurityReject {
    AlgorithmsUnsupported = 12,
    Conflict = 13,
    RadioProcedureFailure = 14,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityResponse {
    Complete {
        integrity: Algorithm,
        encryption: Option<Algorithm>,
    },
    Reject(SecurityReject),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedSecurityResponse {
    pub response: SecurityResponse,
    pub notify_ies: Vec<u16>,
}

#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, value("1..=64"))]
struct Radio(u8);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, value("65..=80"))]
struct Transmission(u8);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, value("81..=96"))]
struct Nas(u8);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, value("97..=112"))]
struct ProtocolCause(u8);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, value("113..=128"))]
struct Misc(u8);
#[derive(AsnType, Decode, Encode)]
#[rasn(delegate, value("129..=256"))]
struct NonStandard(u16);
#[derive(AsnType, Decode, Encode)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
enum Cause {
    Radio(Radio),
    Transmission(Transmission),
    Nas(Nas),
    Protocol(ProtocolCause),
    Misc(Misc),
    NonStandard(NonStandard),
}

impl SecurityResponse {
    pub fn encode(self, limits: Limits) -> Result<Vec<u8>, Error> {
        let (outcome, ies) = match self {
            Self::Complete {
                integrity,
                encryption,
            } => {
                let mut ies = vec![Ie::new(CHOSEN_INTEGRITY, Criticality::Reject, &integrity)?];
                if let Some(encryption) = encryption {
                    ies.push(Ie::new(
                        CHOSEN_ENCRYPTION,
                        Criticality::Ignore,
                        &encryption,
                    )?)
                }
                (Outcome::Successful, ies)
            }
            Self::Reject(cause) => (
                Outcome::Unsuccessful,
                vec![Ie::new(
                    CAUSE,
                    Criticality::Ignore,
                    &Cause::Radio(Radio(cause as u8)),
                )?],
            ),
        };
        Packet {
            protocol: Protocol::Ranap,
            outcome,
            procedure: SECURITY_MODE,
            criticality: Criticality::Reject,
            ies,
            extensions: vec![],
        }
        .encode(limits)
    }

    pub fn decode(bytes: &[u8], limits: Limits) -> Result<DecodedSecurityResponse, Error> {
        let packet = Packet::decode(Protocol::Ranap, bytes, limits)?;
        if packet.procedure != SECURITY_MODE {
            return Err(Error::Unsupported);
        }
        match packet.outcome {
            Outcome::Successful => {
                let notify_ies =
                    packet.check(&[CHOSEN_INTEGRITY, CHOSEN_ENCRYPTION, DIAGNOSTICS], &[])?;
                Ok(DecodedSecurityResponse {
                    response: Self::Complete {
                        integrity: packet.required(CHOSEN_INTEGRITY)?.decode()?,
                        encryption: packet
                            .optional(CHOSEN_ENCRYPTION)
                            .map(Ie::decode)
                            .transpose()?,
                    },
                    notify_ies,
                })
            }
            Outcome::Unsuccessful => {
                let notify_ies = packet.check(&[CAUSE, DIAGNOSTICS], &[])?;
                let Cause::Radio(Radio(cause)) = packet.required(CAUSE)?.decode()? else {
                    return Err(Error::Unsupported);
                };
                let cause = match cause {
                    value if value == SecurityReject::AlgorithmsUnsupported as u8 => {
                        SecurityReject::AlgorithmsUnsupported
                    }
                    value if value == SecurityReject::Conflict as u8 => SecurityReject::Conflict,
                    value if value == SecurityReject::RadioProcedureFailure as u8 => {
                        SecurityReject::RadioProcedureFailure
                    }
                    _ => return Err(Error::Unsupported),
                };
                Ok(DecodedSecurityResponse {
                    response: Self::Reject(cause),
                    notify_ies,
                })
            }
            _ => Err(Error::Unsupported),
        }
    }
}

#[cfg(test)]
mod tests;
