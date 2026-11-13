// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::types::FixedBitString;
use umts_rrc_codec::{EncodedRrc, decode_uplink_dedicated};
use umts_rrc_schema::{
    class_definitions::{DLDCCHMessage, DLDCCHMessageType, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_security::{
    contexts::KeyOrigin,
    negotiation::{CipherAlternative, IntegrityAlgorithm, Selection, Support},
};
use umts_types::CnDomain;

use super::{
    ActivationSequences, BearerLayout, Error, IntegrityActivation, resources::validate_dpch,
};
use crate::{
    establishment::TransactionId,
    integrity::{AcceptedUplink, DownlinkIntegrity, StreamError},
};

const SECURITY_CAPABILITY_BITS: usize = 16;
const SECOND_ALGORITHM_BIT: usize = 13;
const FIRST_ALGORITHM_BIT: usize = 14;
const CLEAR_CIPHER_BIT: usize = 15;
const CONTROL_SRB: u8 = 2;
const OCTET_BITS: usize = u8::BITS as usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    security: ie::SecurityCapability,
    inter_rat: Option<ie::InterRATUESecurityCapList>,
}

impl Capabilities {
    pub fn security(&self) -> &ie::SecurityCapability {
        &self.security
    }
    pub fn inter_rat(&self) -> Option<&ie::InterRATUESecurityCapList> {
        self.inter_rat.as_ref()
    }

    pub fn new(
        security: ie::SecurityCapability,
        inter_rat: Option<ie::InterRATUESecurityCapList>,
    ) -> Result<Self, Error> {
        if security.ciphering_algorithm_cap.len() != SECURITY_CAPABILITY_BITS
            || security.integrity_protection_algorithm_cap.len() != SECURITY_CAPABILITY_BITS
        {
            return Err(Error::Capabilities);
        }
        Ok(Self {
            security,
            inter_rat,
        })
    }

    pub fn support(&self) -> Support {
        // TS 25.331 10.3.3.37 places UEA2/UIA2 at bit 13, not RANAP algorithm index 1.
        Support {
            uia1: self.security.integrity_protection_algorithm_cap[FIRST_ALGORITHM_BIT],
            uia2: self.security.integrity_protection_algorithm_cap[SECOND_ALGORITHM_BIT],
            uea0: self.security.ciphering_algorithm_cap[CLEAR_CIPHER_BIT],
            uea1: self.security.ciphering_algorithm_cap[FIRST_ALGORITHM_BIT],
            uea2: self.security.ciphering_algorithm_cap[SECOND_ALGORITHM_BIT],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityChange {
    Start { fresh: u32 },
    Modify(IntegrityActivation),
}

pub struct Command {
    origin: KeyOrigin,
    transaction: TransactionId,
    layout: BearerLayout,
    selection: Selection,
    integrity: IntegrityChange,
    cipher: Option<ActivationSequences>,
    dpch_cfn: Option<u8>,
    pub(super) message: DLDCCHMessage,
}

pub struct CommandParameters {
    pub origin: KeyOrigin,
    pub transaction: TransactionId,
    pub layout: BearerLayout,
    pub selection: Selection,
    pub integrity: IntegrityChange,
    pub cipher: Option<ActivationSequences>,
    pub dpch_cfn: Option<u8>,
}

impl Command {
    pub fn protected_octets(&self) -> Result<usize, Error> {
        let encoded =
            umts_rrc_codec::encode_downlink_dedicated(&self.message).map_err(Error::Codec)?;
        crate::integrity::protected_bit_length(encoded.bit_length)
            .map(|bits| bits.div_ceil(OCTET_BITS))
            .map_err(|_| Error::Message)
    }
    pub fn new(parameters: CommandParameters, capabilities: &Capabilities) -> Result<Self, Error> {
        let CommandParameters {
            origin,
            transaction,
            layout,
            selection,
            integrity,
            cipher,
            dpch_cfn,
        } = parameters;
        if let IntegrityChange::Modify(activation) = integrity
            && activation.signalling() != layout.signalling()
        {
            return Err(Error::Activation);
        }
        if cipher.is_some_and(|cipher| cipher.layout() != layout) {
            return Err(Error::Bearer);
        }
        if cipher.is_some() {
            validate_dpch(layout, dpch_cfn)?
        } else if dpch_cfn.is_some() {
            return Err(Error::Activation);
        }
        if selection.chosen_encryption().is_some() != cipher.is_some() {
            return Err(Error::Activation);
        }
        validate_algorithms(selection, capabilities.support())?;
        let mode = match integrity {
            IntegrityChange::Start { fresh } => {
                ie::IntegrityProtectionModeCommand::startIntegrityProtection(
                    ie::IntegrityProtectionModeCommandStartIntegrityProtection::new(
                        ie::IntegrityProtInitNumber(fixed(fresh)),
                    ),
                )
            }
            IntegrityChange::Modify(activation) => ie::IntegrityProtectionModeCommand::modify(
                ie::IntegrityProtectionModeCommandModify::new(activation.wire()),
            ),
        };
        let domain = match origin.domain {
            CnDomain::Cs => ie::CNDomainIdentity::cs_domain,
            CnDomain::Ps => ie::CNDomainIdentity::ps_domain,
        };
        let body = if selection.algorithms.integrity == IntegrityAlgorithm::Uia2
            || selection.algorithms.cipher == CipherAlternative::Uea2
        {
            release7(
                transaction,
                capabilities,
                selection,
                mode,
                domain,
                cipher,
                dpch_cfn,
            )
        } else {
            release99(
                transaction,
                capabilities,
                selection,
                mode,
                domain,
                cipher,
                dpch_cfn,
            )?
        };
        Ok(Self {
            origin,
            transaction,
            layout,
            selection,
            integrity,
            cipher,
            dpch_cfn,
            message: DLDCCHMessage::new(None, DLDCCHMessageType::securityModeCommand(body)),
        })
    }

    pub fn origin(&self) -> KeyOrigin {
        self.origin
    }
    pub fn transaction(&self) -> TransactionId {
        self.transaction
    }
    pub fn selection(&self) -> Selection {
        self.selection
    }
    pub fn integrity_change(&self) -> IntegrityChange {
        self.integrity
    }
    pub fn layout(&self) -> BearerLayout {
        self.layout
    }
    pub fn cipher_activation(&self) -> Option<ActivationSequences> {
        self.cipher
    }
    pub fn cipher_frame(&self) -> Option<u8> {
        self.dpch_cfn
    }

    pub fn publish_with<T, E>(
        &self,
        integrity: &mut DownlinkIntegrity,
        publish_old_cipher: impl FnOnce(&EncodedRrc) -> Result<T, E>,
    ) -> Result<T, StreamError<E>> {
        if integrity.bearer() != CONTROL_SRB {
            return Err(StreamError::Protection(crate::integrity::Error::Bearer));
        }
        if self.selection.algorithms.integrity != integrity.algorithm() {
            return Err(StreamError::Protection(crate::integrity::Error::Algorithm(
                umts_security::Error::UnsupportedAlgorithm,
            )));
        }
        if let IntegrityChange::Start { fresh } = self.integrity
            && integrity.fresh() != fresh
        {
            return Err(StreamError::Protection(
                crate::integrity::Error::Authentication,
            ));
        }
        // TS 25.331 8.1.12.2 requires new integrity but old ciphering for this command.
        integrity.publish_with(self.origin, &self.message, publish_old_cipher)
    }

    pub fn completion(&self, accepted: AcceptedUplink<'_>) -> Result<Completion, Error> {
        if accepted.origin() != self.origin || accepted.bearer() != CONTROL_SRB {
            return Err(Error::Origin);
        }
        let decoded =
            decode_uplink_dedicated(accepted.bytes(), accepted.bytes().len() * OCTET_BITS)
                .map_err(Error::Codec)?;
        let ULDCCHMessageType::securityModeComplete(complete) = &decoded.value.message else {
            return Err(Error::Message);
        };
        if complete.rrc_transaction_identifier.0 != self.transaction.get() {
            return Err(Error::Transaction);
        }
        let integrity = IntegrityActivation::from_wire(
            self.layout.signalling(),
            complete
                .ul_integ_prot_activation_info
                .as_ref()
                .ok_or(Error::Activation)?,
        )?;
        let cipher = match (
            &complete.rb_ul_ciph_activation_time_info,
            self.selection.chosen_encryption(),
        ) {
            (Some(value), Some(_)) => Some(ActivationSequences::from_wire(self.layout, value)?),
            (None, None) => None,
            _ => return Err(Error::Activation),
        };
        Ok(Completion {
            origin: self.origin,
            transaction: self.transaction,
            integrity,
            cipher,
            count: accepted.count(),
            selection: self.selection,
        })
    }
}

fn validate_algorithms(selection: Selection, support: Support) -> Result<(), Error> {
    let integrity = match selection.algorithms.integrity {
        IntegrityAlgorithm::Uia1 => support.uia1,
        IntegrityAlgorithm::Uia2 => support.uia2,
    };
    let cipher = match selection.algorithms.cipher {
        CipherAlternative::Clear => support.uea0,
        CipherAlternative::Uea1 => support.uea1,
        CipherAlternative::Uea2 => support.uea2,
    };
    if integrity && cipher {
        Ok(())
    } else {
        Err(Error::Algorithm)
    }
}

fn release99(
    transaction: TransactionId,
    capabilities: &Capabilities,
    selection: Selection,
    mode: ie::IntegrityProtectionModeCommand,
    domain: ie::CNDomainIdentity,
    activations: Option<ActivationSequences>,
    dpch: Option<u8>,
) -> Result<pdu::SecurityModeCommand, Error> {
    let cipher = selection
        .chosen_encryption()
        .map(|algorithm| {
            let algorithm = match algorithm {
                CipherAlternative::Clear => ie::CipheringAlgorithm::uea0,
                CipherAlternative::Uea1 => ie::CipheringAlgorithm::uea1,
                CipherAlternative::Uea2 => return Err(Error::Algorithm),
            };
            Ok(ie::CipheringModeInfo::new(
                ie::CipheringModeCommand::startRestart(algorithm),
                dpch.map(ie::ActivationTime),
                activations.map(ActivationSequences::wire),
            ))
        })
        .transpose()?;
    let integrity =
        ie::IntegrityProtectionModeInfo::new(mode, Some(ie::IntegrityProtectionAlgorithm::uia1));
    Ok(pdu::SecurityModeCommand::r3(
        pdu::SecurityModeCommandR3::new(
            pdu::SecurityModeCommandR3IEs::new(
                ie::RRCTransactionIdentifier(transaction.get()),
                capabilities.security.clone(),
                cipher,
                Some(integrity),
                domain,
                capabilities.inter_rat.clone(),
            ),
            None,
        ),
    ))
}

fn release7(
    transaction: TransactionId,
    capabilities: &Capabilities,
    selection: Selection,
    mode: ie::IntegrityProtectionModeCommand,
    domain: ie::CNDomainIdentity,
    activations: Option<ActivationSequences>,
    dpch: Option<u8>,
) -> pdu::SecurityModeCommand {
    let cipher = selection.chosen_encryption().map(|algorithm| {
        ie::CipheringModeInfoR7::new(
            ie::CipheringModeCommandR7::new(match algorithm {
                CipherAlternative::Clear => ie::CipheringAlgorithmR7::uea0,
                CipherAlternative::Uea1 => ie::CipheringAlgorithmR7::uea1,
                CipherAlternative::Uea2 => ie::CipheringAlgorithmR7::uea2,
            }),
            dpch.map(ie::ActivationTime),
            activations.map(ActivationSequences::wire),
        )
    });
    let integrity = ie::IntegrityProtectionModeInfoR7::new(
        mode,
        Some(match selection.algorithms.integrity {
            IntegrityAlgorithm::Uia1 => ie::IntegrityProtectionAlgorithmR7::uia1,
            IntegrityAlgorithm::Uia2 => ie::IntegrityProtectionAlgorithmR7::uia2,
        }),
    );
    pdu::SecurityModeCommand::later_than_r3(pdu::SecurityModeCommandLaterThanR3::new(
        ie::RRCTransactionIdentifier(transaction.get()),
        pdu::SecurityModeCommandLaterThanR3CriticalExtensions::r7(
            pdu::SecurityModeCommandLaterThanR3CriticalExtensionsR7::new(
                pdu::SecurityModeCommandR7IEs::new(
                    capabilities.security.clone(),
                    cipher,
                    Some(integrity),
                    domain,
                    capabilities.inter_rat.clone(),
                ),
                None,
                None,
            ),
        ),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    origin: KeyOrigin,
    transaction: TransactionId,
    integrity: IntegrityActivation,
    cipher: Option<ActivationSequences>,
    count: u32,
    selection: Selection,
}

impl Completion {
    pub fn origin(self) -> KeyOrigin {
        self.origin
    }
    pub fn transaction(self) -> TransactionId {
        self.transaction
    }
    pub fn integrity(self) -> IntegrityActivation {
        self.integrity
    }
    pub fn cipher(self) -> Option<ActivationSequences> {
        self.cipher
    }
    pub fn count(self) -> u32 {
        self.count
    }
    pub fn selection(self) -> Selection {
        self.selection
    }
}

fn fixed<const N: usize>(value: u32) -> FixedBitString<N> {
    let mut bits = FixedBitString::default();
    for bit in 0..N {
        bits.set(bit, value & (1 << (N - bit - 1)) != 0)
    }
    bits
}
