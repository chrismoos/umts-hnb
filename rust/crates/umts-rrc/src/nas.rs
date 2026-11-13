// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod deferred;
pub mod identification;
pub mod pending;
mod protected;
mod signalling_release;
pub use protected::{NasProtectedError, NasScheduledError};
pub use signalling_release::{SignallingRelease, inspect_signalling_release};

use crate::establishment::{DomainStarts, TransactionId};
use umts_rrc_codec::{CodecError, DecodedUplinkDedicated, EncodedRrc, encode_downlink_dedicated};
use umts_rrc_schema::{
    class_definitions::{DLDCCHMessage, DLDCCHMessageType, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_types::{CnDomain, ConnectionKey, UeKey};

const CONTEXT_BITS: usize = 24;
const START_BITS: usize = 20;
const TRANSACTION_COUNT: u8 = 4;
pub const MAX_NAS_BYTES: usize = 4095;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoreContextId(u32);
impl CoreContextId {
    pub fn new(value: u32) -> Result<Self, NasError> {
        if value >= 1 << CONTEXT_BITS {
            Err(NasError::Context)
        } else {
            Ok(Self(value))
        }
    }
    pub fn get(self) -> u32 {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CoreConnection {
    pub key: ConnectionKey,
    pub context: CoreContextId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignallingBearer {
    Unacknowledged,
    ControlAm,
    NasAm,
    LowPriorityNasAm,
}
impl SignallingBearer {
    pub fn identity(self) -> u8 {
        match self {
            Self::Unacknowledged => 1,
            Self::ControlAm => 2,
            Self::NasAm => 3,
            Self::LowPriorityNasAm => 4,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownlinkPriority {
    Signalling,
    Sms,
}
impl DownlinkPriority {
    pub fn from_sapi(sapi: u8) -> Result<Self, NasError> {
        match sapi {
            0 => Ok(Self::Signalling),
            3 => Ok(Self::Sms),
            _ => Err(NasError::Priority),
        }
    }
    pub fn bearer(self, fourth_available: bool) -> SignallingBearer {
        match (self, fourth_available) {
            (Self::Sms, true) => SignallingBearer::LowPriorityNasAm,
            _ => SignallingBearer::NasAm,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NasError {
    Origin,
    Context,
    Domain,
    Bearer,
    Message,
    SecurityRequired,
    Length,
    Priority,
    Unsupported,
    Codec(CodecError),
}
#[derive(Debug, PartialEq, Eq)]
pub enum NasPublishError<E> {
    Procedure(NasError),
    Downstream(E),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialNasMetadata {
    pub selector: ie::IntraDomainNasNodeSelector,
    pub start: Option<u32>,
    pub plmn: Option<ie::PLMNIdentity>,
    pub establishment_cause: Option<ie::EstablishmentCause>,
    pub supports_csg: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreNasKind {
    Initial,
    Direct,
}

// TS 25.413 8.23.2.2 forbids SAPI in uplink Direct Transfer. Downlink priority is modeled separately.
pub struct CoreNas<'a> {
    pub connection: CoreConnection,
    pub kind: CoreNasKind,
    pub nas: &'a [u8],
    pub initial: Option<InitialNasMetadata>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForwardedNas {
    pub connection: CoreConnection,
    pub created: bool,
}

pub struct InspectedNas<'a> {
    domain: CnDomain,
    nas: &'a [u8],
    initial: Option<InitialNasMetadata>,
}
impl<'a> InspectedNas<'a> {
    pub fn domain(&self) -> CnDomain {
        self.domain
    }
    pub fn nas(&self) -> &'a [u8] {
        self.nas
    }
    pub fn initial(&self) -> Option<&InitialNasMetadata> {
        self.initial.as_ref()
    }
}
pub fn inspect_uplink_nas(
    bearer: SignallingBearer,
    message: &DecodedUplinkDedicated,
    fourth_available: bool,
) -> Result<InspectedNas<'_>, NasError> {
    if message.value.integrity_check_info.is_some() {
        return Err(NasError::SecurityRequired);
    }
    inspect_message(bearer, message, fourth_available)
}

fn inspect_message(
    bearer: SignallingBearer,
    message: &DecodedUplinkDedicated,
    fourth_available: bool,
) -> Result<InspectedNas<'_>, NasError> {
    let (domain, nas, initial) = match &message.value.message {
        ULDCCHMessageType::initialDirectTransfer(body) => {
            if bearer != SignallingBearer::NasAm {
                return Err(NasError::Bearer);
            }
            let extensions = body.v3a0_non_critical_extensions.as_ref();
            let start = extensions
                .and_then(|extensions| {
                    extensions
                        .initial_direct_transfer_v3a0ext
                        .start_value
                        .as_ref()
                })
                .map(|value| {
                    value.0[..START_BITS]
                        .iter()
                        .fold(0u32, |word, bit| (word << 1) | u32::from(*bit))
                });
            let later = extensions
                .and_then(|extensions| extensions.later_non_critical_extensions.as_ref())
                .and_then(|extensions| extensions.v590_non_critical_extensions.as_ref());
            let plmn = later
                .and_then(|extensions| extensions.v690_non_critical_extensions.as_ref())
                .and_then(|extensions| {
                    extensions
                        .initial_direct_transfer_v690ext
                        .plmn_identity
                        .clone()
                });
            let metadata = InitialNasMetadata {
                selector: body.intra_domain_nas_node_selector.clone(),
                start,
                plmn,
                establishment_cause: later.and_then(|extensions| {
                    extensions
                        .initial_direct_transfer_v590ext
                        .establishment_cause
                }),
                supports_csg: later
                    .and_then(|extensions| extensions.v690_non_critical_extensions.as_ref())
                    .and_then(|extensions| extensions.v770_non_critical_extensions.as_ref())
                    .and_then(|extensions| extensions.v860_non_critical_extensions.as_ref())
                    .is_some_and(|extensions| {
                        extensions
                            .initial_direct_transfer_v860ext
                            .support_of_csg
                            .is_some()
                    }),
            };
            (
                domain(body.cn_domain_identity),
                body.nas_message.0.as_ref(),
                Some(metadata),
            )
        }
        ULDCCHMessageType::uplinkDirectTransfer(body) => {
            if bearer != SignallingBearer::NasAm
                && !(fourth_available && bearer == SignallingBearer::LowPriorityNasAm)
            {
                return Err(NasError::Bearer);
            }
            (
                domain(body.cn_domain_identity),
                body.nas_message.0.as_ref(),
                None,
            )
        }
        _ => return Err(NasError::Message),
    };
    if nas.is_empty() || nas.len() > MAX_NAS_BYTES {
        return Err(NasError::Length);
    }
    Ok(InspectedNas {
        domain,
        nas,
        initial,
    })
}

pub struct NasRouter {
    key: UeKey,
    context: CoreContextId,
    links: [Option<CoreConnection>; 2],
    starts: DomainStarts,
    transaction: u8,
    fourth_available: bool,
    security_required: bool,
}
impl NasRouter {
    pub fn new(
        key: UeKey,
        context: CoreContextId,
        starts: DomainStarts,
        fourth_available: bool,
    ) -> Self {
        Self {
            key,
            context,
            links: [None; 2],
            starts,
            transaction: 0,
            fourth_available,
            security_required: false,
        }
    }
    pub fn starts(&self) -> DomainStarts {
        self.starts
    }
    pub fn update_starts_with<T, E>(
        &mut self,
        key: UeKey,
        updates: DomainStarts,
        publish: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, NasPublishError<E>> {
        if key != self.key {
            return Err(NasPublishError::Procedure(NasError::Origin));
        }
        if [updates.cs, updates.ps]
            .into_iter()
            .flatten()
            .any(|value| value >= (1 << START_BITS))
        {
            return Err(NasPublishError::Procedure(NasError::Message));
        }
        let next = DomainStarts {
            cs: updates.cs.or(self.starts.cs),
            ps: updates.ps.or(self.starts.ps),
        };
        let value = publish().map_err(NasPublishError::Downstream)?;
        self.starts = next;
        Ok(value)
    }
    pub fn connection(&self, domain: CnDomain) -> Option<CoreConnection> {
        self.links[index(domain)]
    }
    pub fn connections(&self) -> [Option<CoreConnection>; 2] {
        self.links
    }
    pub fn require_security(&mut self) {
        self.security_required = true;
    }

    pub fn forward_with<T, E>(
        &mut self,
        key: UeKey,
        bearer: SignallingBearer,
        message: &DecodedUplinkDedicated,
        publish: impl FnOnce(CoreNas<'_>) -> Result<T, E>,
    ) -> Result<(ForwardedNas, T), NasPublishError<E>> {
        let fail = NasPublishError::Procedure;
        if key != self.key {
            return Err(fail(NasError::Origin));
        }
        if self.security_required || message.value.integrity_check_info.is_some() {
            return Err(fail(NasError::SecurityRequired));
        }
        let InspectedNas {
            domain,
            nas,
            initial,
        } = inspect_uplink_nas(bearer, message, self.fourth_available).map_err(fail)?;
        self.forward_parts(key, domain, nas, initial, publish)
    }
    fn forward_parts<T, E>(
        &mut self,
        key: UeKey,
        domain: CnDomain,
        nas: &[u8],
        initial: Option<InitialNasMetadata>,
        publish: impl FnOnce(CoreNas<'_>) -> Result<T, E>,
    ) -> Result<(ForwardedNas, T), NasPublishError<E>> {
        if key != self.key {
            return Err(NasPublishError::Procedure(NasError::Origin));
        }
        if self.security_required {
            return Err(NasPublishError::Procedure(NasError::SecurityRequired));
        }
        self.forward_admitted(key, domain, nas, initial, publish)
    }

    fn forward_admitted<T, E>(
        &mut self,
        key: UeKey,
        domain: CnDomain,
        nas: &[u8],
        initial: Option<InitialNasMetadata>,
        publish: impl FnOnce(CoreNas<'_>) -> Result<T, E>,
    ) -> Result<(ForwardedNas, T), NasPublishError<E>> {
        let fail = NasPublishError::Procedure;
        if key != self.key {
            return Err(fail(NasError::Origin));
        }
        if nas.is_empty() || nas.len() > MAX_NAS_BYTES {
            return Err(fail(NasError::Length));
        }
        let slot = index(domain);
        let created = self.links[slot].is_none();
        if created && initial.is_none() {
            return Err(fail(NasError::Domain));
        }
        let connection = self.links[slot].unwrap_or(CoreConnection {
            key: ConnectionKey {
                ue: self.key,
                domain,
            },
            // TS 25.468 9.2.2 uses one gateway-assigned UE context for both CN domains.
            context: self.context,
        });
        let start = initial.as_ref().and_then(|metadata| metadata.start);
        let result = publish(CoreNas {
            connection,
            kind: if created {
                CoreNasKind::Initial
            } else {
                CoreNasKind::Direct
            },
            nas,
            initial,
        })
        .map_err(NasPublishError::Downstream)?;
        self.links[slot] = Some(connection);
        if let Some(start) = start {
            match domain {
                CnDomain::Cs => self.starts.cs = Some(start),
                CnDomain::Ps => self.starts.ps = Some(start),
            }
        }
        Ok((
            ForwardedNas {
                connection,
                created,
            },
            result,
        ))
    }

    pub fn downlink_with<T, E>(
        &mut self,
        connection: CoreConnection,
        priority: DownlinkPriority,
        nas: &[u8],
        publish: impl FnOnce(SignallingBearer, &EncodedRrc) -> Result<T, E>,
    ) -> Result<T, NasPublishError<E>> {
        let fail = NasPublishError::Procedure;
        if connection.key.ue != self.key {
            return Err(fail(NasError::Origin));
        }
        if self.links[index(connection.key.domain)] != Some(connection) {
            return Err(fail(NasError::Context));
        }
        if self.security_required {
            return Err(fail(NasError::SecurityRequired));
        }
        let transaction =
            TransactionId::new(self.transaction).map_err(|_| fail(NasError::Message))?;
        let message = build_downlink_nas(transaction, connection.key.domain, nas).map_err(fail)?;
        let bearer = priority.bearer(self.fourth_available);
        let result = publish(bearer, &message).map_err(NasPublishError::Downstream)?;
        self.transaction = (self.transaction + 1) % TRANSACTION_COUNT;
        Ok(result)
    }

    pub fn retire_with<T, E>(
        &mut self,
        publish: impl FnOnce([Option<CoreConnection>; 2]) -> Result<T, E>,
    ) -> Result<T, E> {
        let result = publish(self.links)?;
        self.links = [None; 2];
        Ok(result)
    }
}

pub fn build_downlink_nas(
    transaction: TransactionId,
    cn: CnDomain,
    nas: &[u8],
) -> Result<EncodedRrc, NasError> {
    encode_downlink_dedicated(&downlink_nas_message(transaction, cn, nas)?).map_err(NasError::Codec)
}

pub fn signalling_connection_release_message(
    transaction: TransactionId,
    cn: CnDomain,
) -> DLDCCHMessage {
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::signallingConnectionRelease(pdu::SignallingConnectionRelease::r3(
            pdu::SignallingConnectionReleaseR3::new(
                pdu::SignallingConnectionReleaseR3IEs::new(
                    ie::RRCTransactionIdentifier(transaction.get()),
                    wire_domain(cn),
                ),
                None,
            ),
        )),
    )
}

fn downlink_nas_message(
    transaction: TransactionId,
    cn: CnDomain,
    nas: &[u8],
) -> Result<DLDCCHMessage, NasError> {
    if nas.is_empty() || nas.len() > MAX_NAS_BYTES {
        return Err(NasError::Length);
    }
    let body = pdu::DownlinkDirectTransferR3IEs::new(
        ie::RRCTransactionIdentifier(transaction.get()),
        wire_domain(cn),
        ie::NASMessage(nas.to_vec().into()),
    );
    Ok(DLDCCHMessage::new(
        None,
        DLDCCHMessageType::downlinkDirectTransfer(pdu::DownlinkDirectTransfer::r3(
            pdu::DownlinkDirectTransferR3::new(body, None),
        )),
    ))
}
fn index(domain: CnDomain) -> usize {
    match domain {
        CnDomain::Cs => 0,
        CnDomain::Ps => 1,
    }
}
fn domain(value: ie::CNDomainIdentity) -> CnDomain {
    match value {
        ie::CNDomainIdentity::cs_domain => CnDomain::Cs,
        ie::CNDomainIdentity::ps_domain => CnDomain::Ps,
    }
}
fn wire_domain(value: CnDomain) -> ie::CNDomainIdentity {
    match value {
        CnDomain::Cs => ie::CNDomainIdentity::cs_domain,
        CnDomain::Ps => ie::CNDomainIdentity::ps_domain,
    }
}

#[cfg(test)]
mod start_updates;
#[cfg(test)]
mod tests;
