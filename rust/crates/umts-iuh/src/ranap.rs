// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::Decoder;

use crate::{
    ContextId, Domain, Error,
    hnbap::{AreaCode, Imsi, Plmn, Rac},
    values::{fixed, octets},
    wire::{
        Criticality, ExtensionList, InformationElement as Ie, Limits, Outcome, Packet, Protocol,
    },
};
use rasn::{
    AsnType, Decode, Encode,
    types::{FixedBitString, OctetString, SequenceOf},
};

const INITIAL_UE: u8 = 19;
const DIRECT: u8 = 20;
const COMMON_ID: u8 = 15;
const CAUSE: u16 = 4;
const SECURITY_MODE: u8 = 6;
pub const IU_RELEASE: u8 = 1;
pub const RAB_ASSIGNMENT: u8 = 0;
pub const PAGING: u8 = 14;
pub const RESET: u8 = 9;
pub const ERROR_INDICATION: u8 = 22;
pub const RAB_RELEASE_REQUEST: u8 = 10;
const RAB_RELEASE_LIST: u16 = 41;
const RAB_RELEASE_ITEM: u16 = 40;
const DIAGNOSTICS: u16 = 9;
const DOMAIN: u16 = 3;
const ENCRYPTION: u16 = 11;
const INTEGRITY: u16 = 12;
const LAI: u16 = 15;
const NAS: u16 = 16;
const PERMANENT_ID: u16 = 23;
const RAC: u16 = 55;
const SAI: u16 = 58;
const SAPI: u16 = 59;
const KEY_STATUS: u16 = 75;
const SIGNALLING_ID: u16 = 79;
const GLOBAL_RNC: u16 = 86;

pub mod paging;
pub mod rab;
pub mod release;
mod security_response;
pub use security_response::{DecodedSecurityResponse, SecurityReject, SecurityResponse};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub plmn: [u8; 3],
    pub lac: u16,
    pub rac: u8,
    pub sac: u16,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct Lai {
    plmn: Plmn,
    lac: AreaCode,
    extensions: Option<ExtensionList>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct Sai {
    plmn: Plmn,
    lac: AreaCode,
    sac: AreaCode,
    extensions: Option<ExtensionList>,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=4095"))]
struct Rnc(u16);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct GlobalRnc {
    plmn: Plmn,
    rnc: Rnc,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum Priority {
    Signalling,
    Sms,
}
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=15"))]
pub struct Algorithm(pub u8);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=16"))]
struct Algorithms(SequenceOf<Algorithm>);
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
#[non_exhaustive]
pub enum KeyStatus {
    Old,
    New,
}
#[derive(AsnType, Decode, Encode, Clone, PartialEq, Eq)]
#[rasn(automatic_tags)]
struct ProtectionInfo {
    algorithms: Algorithms,
    key: FixedBitString<128>,
    extensions: Option<ExtensionList>,
}
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; 16]);
impl Drop for Key {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}
impl Key {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
    pub fn bytes(&self) -> &[u8; 16] {
        &self.0
    }
}
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key([redacted])")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Protection {
    pub algorithms: Vec<Algorithm>,
    pub key: Key,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityCommand {
    pub integrity: Protection,
    pub encryption: Option<Protection>,
    pub key_status: KeyStatus,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("3..=8"))]
struct ImsiAsn(OctetString);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(choice, automatic_tags)]
#[non_exhaustive]
enum PermanentIdentity {
    Imsi(ImsiAsn),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    Direct { nas: Vec<u8>, priority: Priority },
    CommonIdentity(Imsi),
    IuRelease(release::Command),
    Security(SecurityCommand),
    RabAssignment(rab::Assignment),
    Paging(paging::Request),
    Procedure(Packet),
}

impl Inbound {
    pub fn procedure_code(&self) -> u8 {
        match self {
            Self::Direct { .. } => DIRECT,
            Self::CommonIdentity(_) => COMMON_ID,
            Self::IuRelease(_) => IU_RELEASE,
            Self::Security(_) => SECURITY_MODE,
            Self::RabAssignment(_) => RAB_ASSIGNMENT,
            Self::Paging(_) => PAGING,
            Self::Procedure(packet) => packet.procedure,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub message: Inbound,
    pub notify_ies: Vec<u16>,
}
fn location_ies(location: Location) -> Result<[Ie; 3], Error> {
    let plmn = Plmn(location.plmn.to_vec().into());
    let lac = AreaCode(location.lac.to_be_bytes().to_vec().into());
    Ok([
        Ie::new(
            LAI,
            Criticality::Ignore,
            &Lai {
                plmn: plmn.clone(),
                lac: lac.clone(),
                extensions: None,
            },
        )?,
        Ie::new(RAC, Criticality::Ignore, &Rac(vec![location.rac].into()))?,
        Ie::new(
            SAI,
            Criticality::Ignore,
            &Sai {
                plmn,
                lac,
                sac: AreaCode(location.sac.to_be_bytes().to_vec().into()),
                extensions: None,
            },
        )?,
    ])
}
fn nas_ie(nas: &[u8]) -> Result<Ie, Error> {
    if nas.is_empty() {
        return Err(Error::Length);
    }
    Ie::new(NAS, Criticality::Ignore, &OctetString::from(nas.to_vec()))
}

pub fn initial_ue(
    context: ContextId,
    domain: Domain,
    rnc: u16,
    location: Location,
    nas: &[u8],
    limits: Limits,
) -> Result<Vec<u8>, Error> {
    let [lai, rac, sai] = location_ies(location)?;
    let mut ies = vec![Ie::new(DOMAIN, Criticality::Ignore, &domain)?, lai];
    if domain == Domain::Ps {
        ies.push(rac);
    }
    ies.extend([
        sai,
        nas_ie(nas)?,
        Ie::new(
            SIGNALLING_ID,
            Criticality::Ignore,
            &fixed::<24>(&context.bytes())?,
        )?,
        Ie::new(
            GLOBAL_RNC,
            Criticality::Ignore,
            &GlobalRnc {
                plmn: Plmn(location.plmn.to_vec().into()),
                rnc: Rnc(rnc),
            },
        )?,
    ]);
    Packet::initiating(Protocol::Ranap, INITIAL_UE, Criticality::Ignore, ies).encode(limits)
}
/// TS 25.413 8.26.2.1: the RNC answers a CN RESET with RESET ACKNOWLEDGE including the CN
/// domain indicator and its Global RNC-ID once the affected connections are released.
pub fn reset_acknowledge(
    domain: Domain,
    rnc: u16,
    plmn: [u8; 3],
    limits: Limits,
) -> Result<Vec<u8>, Error> {
    let ies = vec![
        Ie::new(DOMAIN, Criticality::Reject, &domain)?,
        Ie::new(
            GLOBAL_RNC,
            Criticality::Ignore,
            &GlobalRnc {
                plmn: Plmn(plmn.to_vec().into()),
                rnc: Rnc(rnc),
            },
        )?,
    ];
    Packet::successful(Protocol::Ranap, RESET, Criticality::Reject, ies).encode(limits)
}

/// The CN domain a connectionless RESET (TS 25.413 9.1.26) applies to.
pub fn reset_domain(packet: &Packet) -> Result<Domain, Error> {
    if packet.procedure != RESET || packet.outcome != Outcome::Initiating {
        return Err(Error::Unsupported);
    }
    packet.required(DOMAIN)?.decode()
}

/// TS 25.413 8.27.2 and 10.3.4.2: ERROR INDICATION with a protocol cause and, when the
/// offending IE is known, Criticality Diagnostics naming it.
pub fn error_indication(
    cause: u16,
    procedure: Option<(u8, Criticality)>,
    ie: Option<u16>,
    limits: Limits,
) -> Result<Vec<u8>, Error> {
    let mut ies = vec![Ie::new(
        CAUSE,
        Criticality::Ignore,
        &rab::Cause::from_code(cause)?,
    )?];
    if procedure.is_some() || ie.is_some() {
        ies.push(Ie::new(
            DIAGNOSTICS,
            Criticality::Ignore,
            &CriticalityDiagnostics {
                procedure_code: procedure.map(|(code, _)| ProcedureCode(code)),
                triggering_message: procedure.map(|_| TriggeringMessage::InitiatingMessage),
                procedure_criticality: procedure.map(|(_, criticality)| criticality),
                ies: ie.map(|id| {
                    DiagnosticsList(vec![DiagnosticsItem {
                        criticality: Criticality::Reject,
                        id: ProtocolIeId(id),
                        repetition: None,
                        extensions: None,
                    }])
                }),
                extensions: None,
            },
        )?);
    }
    Packet::initiating(Protocol::Ranap, ERROR_INDICATION, Criticality::Ignore, ies).encode(limits)
}

/// TS 25.413 8.3.2: RAB RELEASE REQUEST naming each RAB with the cause of its release.
/// RAB-ReleaseItem has the same layout as RAB-FailedItem (RAB ID, Cause, extensions).
pub fn rab_release_request(rabs: &[(u8, u16)], limits: Limits) -> Result<Vec<u8>, Error> {
    if rabs.is_empty() {
        return Err(Error::Length);
    }
    let items = rabs
        .iter()
        .map(|&(id, cause)| Ok(rab::Failed::new(id, rab::Cause::from_code(cause)?)))
        .collect::<Result<Vec<_>, Error>>()?;
    let list = rab::encode_list(RAB_RELEASE_LIST, RAB_RELEASE_ITEM, &items, limits)?;
    Packet::initiating(
        Protocol::Ranap,
        RAB_RELEASE_REQUEST,
        Criticality::Ignore,
        vec![list],
    )
    .encode(limits)
}

/// The protocol cause a decode failure maps to (TS 25.413 10.2 to 10.4).
pub fn protocol_cause(error: &Error) -> u16 {
    const TRANSFER_SYNTAX_ERROR: u16 = 97;
    const SEMANTIC_ERROR: u16 = 98;
    const NOT_COMPATIBLE_WITH_RECEIVER_STATE: u16 = 99;
    const ABSTRACT_SYNTAX_ERROR_REJECT: u16 = 100;
    match error {
        Error::Decode | Error::Length | Error::Trailing | Error::Capacity => TRANSFER_SYNTAX_ERROR,
        Error::Value(_) => SEMANTIC_ERROR,
        Error::State => NOT_COMPATIBLE_WITH_RECEIVER_STATE,
        _ => ABSTRACT_SYNTAX_ERROR_REJECT,
    }
}

#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=255"))]
struct ProcedureCode(u8);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=65535"))]
struct ProtocolIeId(u16);
#[derive(AsnType, Decode, Encode, Clone, Copy, Debug, PartialEq, Eq)]
#[rasn(enumerated)]
enum TriggeringMessage {
    InitiatingMessage,
    SuccessfulOutcome,
    UnsuccessfullOutcome,
    Outcome,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct DiagnosticsItem {
    criticality: Criticality,
    id: ProtocolIeId,
    repetition: Option<RepetitionNumber>,
    extensions: Option<ExtensionList>,
}
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, value("0..=255"))]
struct RepetitionNumber(u8);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(delegate, size("1..=256"))]
struct DiagnosticsList(SequenceOf<DiagnosticsItem>);
#[derive(AsnType, Decode, Encode, Clone, Debug, PartialEq, Eq)]
#[rasn(automatic_tags)]
#[non_exhaustive]
struct CriticalityDiagnostics {
    procedure_code: Option<ProcedureCode>,
    triggering_message: Option<TriggeringMessage>,
    procedure_criticality: Option<Criticality>,
    ies: Option<DiagnosticsList>,
    extensions: Option<ExtensionList>,
}

pub fn uplink_direct(
    domain: Domain,
    location: Location,
    nas: &[u8],
    limits: Limits,
) -> Result<Vec<u8>, Error> {
    let mut ies = vec![nas_ie(nas)?];
    if domain == Domain::Ps {
        ies.extend(location_ies(location)?);
    }
    // TS 25.413 8.23.2.2 requires uplink SAPI omission and PS LAI/RAC/SAI inclusion.
    Packet::initiating(Protocol::Ranap, DIRECT, Criticality::Ignore, ies).encode(limits)
}
pub fn downlink_direct(
    nas: &[u8],
    priority: Option<Priority>,
    limits: Limits,
) -> Result<Vec<u8>, Error> {
    let mut ies = vec![nas_ie(nas)?];
    if let Some(priority) = priority {
        ies.push(Ie::new(SAPI, Criticality::Ignore, &priority)?);
    }
    Packet::initiating(Protocol::Ranap, DIRECT, Criticality::Ignore, ies).encode(limits)
}

pub fn common_identity(imsi: Imsi, limits: Limits) -> Result<Vec<u8>, Error> {
    let identity = PermanentIdentity::Imsi(ImsiAsn(imsi.bcd().to_vec().into()));
    Packet::initiating(
        Protocol::Ranap,
        COMMON_ID,
        Criticality::Ignore,
        vec![Ie::new(PERMANENT_ID, Criticality::Ignore, &identity)?],
    )
    .encode(limits)
}

pub fn encode_security(command: &SecurityCommand, limits: Limits) -> Result<Vec<u8>, Error> {
    let mut ies = vec![Ie::new(
        INTEGRITY,
        Criticality::Reject,
        &protection_asn(&command.integrity)?,
    )?];
    if let Some(encryption) = &command.encryption {
        ies.push(Ie::new(
            ENCRYPTION,
            Criticality::Ignore,
            &protection_asn(encryption)?,
        )?);
    }
    ies.push(Ie::new(
        KEY_STATUS,
        Criticality::Reject,
        &command.key_status,
    )?);
    Packet::initiating(Protocol::Ranap, SECURITY_MODE, Criticality::Reject, ies).encode(limits)
}
fn protection_asn(protection: &Protection) -> Result<ProtectionInfo, Error> {
    Ok(ProtectionInfo {
        algorithms: Algorithms(protection.algorithms.clone()),
        key: fixed(protection.key.bytes())?,
        extensions: None,
    })
}
fn protection(ie: &Ie) -> Result<Protection, Error> {
    let value = ie.decode::<ProtectionInfo>()?;
    if value.extensions.as_ref().is_some_and(|list| {
        list.0
            .iter()
            .any(|ie| ie.criticality != Criticality::Ignore)
    }) {
        return Err(Error::Unsupported);
    }
    Ok(Protection {
        algorithms: value.algorithms.0,
        key: Key::new(
            octets(&value.key)
                .try_into()
                .map_err(|_| Error::Value(ie.id))?,
        ),
    })
}
pub fn decode(bytes: &[u8], limits: Limits) -> Result<Decoded, Error> {
    let packet = Packet::decode(Protocol::Ranap, bytes, limits)?;
    if packet.outcome != Outcome::Initiating {
        return Ok(Decoded {
            message: Inbound::Procedure(packet),
            notify_ies: vec![],
        });
    }
    let (message, notify_ies) = match packet.procedure {
        PAGING => {
            let (request, notify) = paging::Request::from_packet(packet)?;
            (Inbound::Paging(request), notify)
        }
        IU_RELEASE => {
            let decoded = release::Command::from_packet(packet)?;
            (Inbound::IuRelease(decoded.command), decoded.notify_ies)
        }
        RAB_ASSIGNMENT => {
            let decoded = rab::Assignment::from_packet(packet, limits)?;
            (
                Inbound::RabAssignment(decoded.assignment),
                decoded.notify_ies,
            )
        }
        DIRECT => {
            let notify = packet.check(&[NAS, LAI, RAC, SAI, SAPI], &[])?;
            let nas = packet.required(NAS)?.decode::<OctetString>()?.to_vec();
            if nas.is_empty() {
                return Err(Error::Value(NAS));
            }
            // TS 25.413 8.23.3 continues the procedure when downlink SAPI is absent.
            let priority = packet
                .optional(SAPI)
                .map(Ie::decode)
                .transpose()?
                .unwrap_or(Priority::Signalling);
            (Inbound::Direct { nas, priority }, notify)
        }
        COMMON_ID => {
            let notify = packet.check(&[PERMANENT_ID], &[])?;
            let PermanentIdentity::Imsi(imsi) = packet.required(PERMANENT_ID)?.decode()?;
            (
                Inbound::CommonIdentity(Imsi::from_bcd(imsi.0.as_ref())?),
                notify,
            )
        }
        SECURITY_MODE => {
            let notify = packet.check(&[INTEGRITY, ENCRYPTION, KEY_STATUS], &[])?;
            (
                Inbound::Security(SecurityCommand {
                    integrity: protection(packet.required(INTEGRITY)?)?,
                    encryption: packet.optional(ENCRYPTION).map(protection).transpose()?,
                    key_status: packet.required(KEY_STATUS)?.decode()?,
                }),
                notify,
            )
        }
        _ => {
            return Ok(Decoded {
                message: Inbound::Procedure(packet),
                notify_ies: vec![],
            });
        }
    };
    Ok(Decoded {
        message,
        notify_ies,
    })
}

#[cfg(test)]
mod tests;
