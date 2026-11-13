// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    establishment::TransactionId,
    integrity::{DownlinkIntegrity, UplinkIntegrity},
};
use rasn::types::{BitString, FixedBitString};
use std::{
    num::NonZeroU64,
    time::{Duration, Instant},
};
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::{
    class_definitions::{DLDCCHMessageType, ULDCCHMessage, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_security::{
    Key,
    contexts::{CipherMaterial, IntegrityContext, KeyOrigin, KeySet},
    counters::{CipherMode, Start},
    negotiation::{self, IntegrityAlgorithm, KeyStatus, Request, Selection, Support},
};
use umts_types::{CnDomain, Direction, RadioEpoch, UeKey};

const FRESH: u32 = 0x12345678;

#[test]
fn protected_size_preview_matches_actual_publication_without_consuming_a_counter() {
    let context = context();
    for cipher in [false, true] {
        for integrity in [
            IntegrityChange::Start { fresh: FRESH },
            IntegrityChange::Modify(
                IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap(),
            ),
        ] {
            let command = Command::new(
                CommandParameters {
                    origin: context.origin(),
                    transaction: TransactionId::new(2).unwrap(),
                    layout: layout(),
                    selection: selection(cipher.then_some(&[1][..])),
                    integrity,
                    cipher: cipher.then(|| activations(layout())),
                    dpch_cfn: None,
                },
                &capabilities(),
            )
            .unwrap();
            let mut stream = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
            let bytes = command.protected_octets().unwrap();
            assert_eq!(command.protected_octets(), Ok(bytes));
            assert_eq!(stream.next_count(), Ok(0));
            assert_eq!(
                command.publish_with(&mut stream, |encoded| {
                    assert_eq!(encoded.bytes.len(), bytes);
                    Err::<(), _>(7)
                }),
                Err(crate::integrity::StreamError::Consumer(7))
            );
            assert_eq!(stream.next_count(), Ok(0));
            command
                .publish_with(&mut stream, |encoded| {
                    assert_eq!(encoded.bytes.len(), bytes);
                    Ok::<_, ()>(())
                })
                .unwrap();
            assert_eq!(stream.next_count(), Ok(1));
        }
    }
}

fn context() -> IntegrityContext {
    let origin = KeyOrigin {
        ue: UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 2,
            generation: NonZeroU64::MIN,
        },
        domain: CnDomain::Cs,
        generation: NonZeroU64::MIN,
    };
    KeySet::uia1(
        origin,
        CipherMaterial::Uea1(Key::new([0x55; 16])),
        Key::new([0xaa; 16]),
    )
    .split()
    .1
}

fn capabilities() -> Capabilities {
    let mut cipher = BitString::repeat(false, 16);
    cipher.set(14, true);
    cipher.set(15, true);
    let mut integrity = BitString::repeat(false, 16);
    integrity.set(14, true);
    Capabilities::new(ie::SecurityCapability::new(cipher, integrity), None).unwrap()
}

fn selection(cipher: Option<&[u8]>) -> Selection {
    negotiation::select(
        Request {
            integrity: &[0],
            cipher,
            keys: KeyStatus::Old,
        },
        Some(capabilities().support()),
        Support::KASUMI,
        negotiation::Constraints::default(),
    )
    .unwrap()
}

fn layout() -> BearerLayout {
    BearerLayout::new(SignallingSet::Base, &[]).unwrap()
}

fn activations(layout: BearerLayout) -> ActivationSequences {
    let mut entries = vec![(1, 7), (2, 100), (3, 42)];
    if layout.signalling() == SignallingSet::WithLowPriorityNas {
        entries.push((4, 43))
    }
    ActivationSequences::new(layout, &entries).unwrap()
}

fn command(context: &IntegrityContext, cipher: bool) -> Command {
    Command::new(
        CommandParameters {
            origin: context.origin(),
            transaction: TransactionId::new(2).unwrap(),
            layout: layout(),
            selection: selection(cipher.then_some(&[1][..])),
            integrity: IntegrityChange::Start { fresh: FRESH },
            cipher: cipher.then(|| activations(layout())),
            dpch_cfn: None,
        },
        &capabilities(),
    )
    .unwrap()
}

fn fixed(value: u32) -> FixedBitString<32> {
    let mut bits = FixedBitString::default();
    for bit in 0..32 {
        bits.set(bit, value & (1 << (31 - bit)) != 0)
    }
    bits
}

fn completed_message(transaction: u8, cipher: bool) -> pdu::SecurityModeComplete {
    pdu::SecurityModeComplete::new(
        ie::RRCTransactionIdentifier(transaction),
        Some(ie::IntegrityProtActivationInfo::new(
            ie::RRCMessageSequenceNumberList(
                [0, 0, 0, 0]
                    .into_iter()
                    .map(ie::RRCMessageSequenceNumber)
                    .collect(),
            ),
        )),
        cipher.then(|| activations(layout()).wire()),
        None,
    )
}

fn authenticated_bytes(
    context: &IntegrityContext,
    complete: pdu::SecurityModeComplete,
    count: u32,
) -> Vec<u8> {
    let mut message = ULDCCHMessage::new(
        Some(ie::IntegrityCheckInfo::new(
            ie::MessageAuthenticationCode(fixed(2)),
            ie::RRCMessageSequenceNumber(0),
        )),
        ULDCCHMessageType::securityModeComplete(complete),
    );
    let preimage = rasn::uper::encode(&message).unwrap();
    let mac = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            count,
            FRESH,
            Direction::Uplink,
            &preimage,
            preimage.len() * 8,
        )
        .unwrap();
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(mac))),
        ie::RRCMessageSequenceNumber((count & 15) as u8),
    ));
    rasn::uper::encode(&message).unwrap()
}

#[test]
fn layouts_require_all_configured_am_um_bearers_and_separate_tm_cfn() {
    let layout = BearerLayout::new(
        SignallingSet::WithLowPriorityNas,
        &[(5, CipherMode::Transparent), (32, CipherMode::Acknowledged)],
    )
    .unwrap();
    let valid = [(1, 127), (2, 4095), (3, 0), (4, 4), (32, 1234)];
    let activation = ActivationSequences::new(layout, &valid).unwrap();
    assert_eq!(activation.sequence(32), Ok(1234));
    assert_eq!(layout.cipher_bearer(1), Ok(0));
    assert_eq!(layout.cipher_bearer(32), Ok(31));
    assert_eq!(layout.cipher_bearer(0), Err(Error::Bearer));
    assert_eq!(layout.cipher_bearer(31), Err(Error::Bearer));
    assert_eq!(activation.sequence(5), Err(Error::Activation));
    assert_eq!(resources::validate_dpch(layout, Some(248)), Ok(()));
    assert_eq!(
        resources::validate_dpch(layout, Some(249)),
        Err(Error::Activation)
    );
    assert_eq!(
        resources::validate_dpch(layout, None),
        Err(Error::Activation)
    );
    assert_eq!(
        ActivationSequences::new(layout, &valid[..4]),
        Err(Error::Activation)
    );
    for invalid in [
        vec![(1, 128), (2, 0), (3, 0), (4, 0), (32, 0)],
        vec![(1, 0), (2, 4096), (3, 0), (4, 0), (32, 0)],
        vec![(1, 0), (2, 0), (3, 0), (4, 0), (32, 0), (5, 0)],
        vec![(1, 0), (2, 0), (3, 0), (4, 0), (32, 0), (32, 1)],
    ] {
        assert!(ActivationSequences::new(layout, &invalid).is_err());
    }
    assert_eq!(
        BearerLayout::new(SignallingSet::Base, &[(4, CipherMode::Acknowledged)]),
        Err(Error::Bearer)
    );
    assert_eq!(
        BearerLayout::new(
            SignallingSet::Base,
            &[(5, CipherMode::Acknowledged), (5, CipherMode::Acknowledged)]
        ),
        Err(Error::Bearer)
    );
    assert_eq!(
        IntegrityActivation::new(SignallingSet::Base, &[0, 1, 2, 3, 4]),
        Err(Error::Activation)
    );
    assert_eq!(
        IntegrityActivation::new(SignallingSet::Base, &[0, 1, 2, 16]),
        Err(Error::Activation)
    );
}

#[test]
fn protected_command_echoes_capabilities_and_uses_the_exact_rel99_construction() {
    let context = context();
    let command = command(&context, true);
    assert_eq!(command.cipher_activation(), Some(activations(layout())));
    assert_eq!(command.cipher_frame(), None);
    let mut downlink = DownlinkIntegrity::new(context, 2, FRESH, 0).unwrap();
    let encoded = command
        .publish_with(&mut downlink, |encoded| Ok::<_, ()>(encoded.clone()))
        .unwrap();
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    assert_eq!(
        decoded
            .value
            .integrity_check_info
            .as_ref()
            .unwrap()
            .rrc_message_sequence_number
            .0,
        0
    );
    let DLDCCHMessageType::securityModeCommand(pdu::SecurityModeCommand::r3(body)) =
        decoded.value.message
    else {
        panic!("not the Rel99 security command")
    };
    let body = body.security_mode_command_r3;
    assert_eq!(&body.security_capability, capabilities().security());
    assert_eq!(body.rrc_transaction_identifier.0, 2);
    assert_eq!(body.cn_domain_identity, ie::CNDomainIdentity::cs_domain);
    let cipher = body.ciphering_mode_info.unwrap();
    assert_eq!(
        cipher.ciphering_mode_command,
        ie::CipheringModeCommand::startRestart(ie::CipheringAlgorithm::uea1)
    );
    assert_eq!(
        cipher.rb_dl_ciph_activation_time_info.unwrap(),
        command.cipher_activation().unwrap().wire()
    );
    assert!(cipher.activation_time_for_dpch.is_none());
    let integrity = body.integrity_protection_mode_info.unwrap();
    assert_eq!(
        integrity.integrity_protection_algorithm,
        Some(ie::IntegrityProtectionAlgorithm::uia1)
    );
    let ie::IntegrityProtectionModeCommand::startIntegrityProtection(start) =
        integrity.integrity_protection_mode_command
    else {
        panic!("not initial integrity")
    };
    assert_eq!(start.integrity_prot_init_number.0, fixed(FRESH));
    assert_eq!(downlink.next_count(), Ok(1));
}

#[test]
fn capability_bits_and_cipher_presence_are_not_algorithm_indices_or_fallbacks() {
    let mut security = capabilities().security().clone();
    security.ciphering_algorithm_cap.set(13, true);
    security.integrity_protection_algorithm_cap.set(13, true);
    let capabilities = Capabilities::new(security, None).unwrap();
    assert!(capabilities.support().uea2 && capabilities.support().uia2);
    let context = context();
    let command = command(&context, false);
    let mut wrong_fresh = DownlinkIntegrity::new(context.clone(), 2, FRESH + 1, 0).unwrap();
    assert!(
        command
            .publish_with(&mut wrong_fresh, |_| Ok::<_, ()>(()))
            .is_err()
    );
    assert_eq!(wrong_fresh.next_count(), Ok(0));
    let mut wrong_bearer = DownlinkIntegrity::new(context.clone(), 3, FRESH, 0).unwrap();
    assert!(
        command
            .publish_with(&mut wrong_bearer, |_| Ok::<_, ()>(()))
            .is_err()
    );
    let mut downlink = DownlinkIntegrity::new(context, 2, FRESH, 0).unwrap();
    let encoded = command
        .publish_with(&mut downlink, |encoded| Ok::<_, ()>(encoded.clone()))
        .unwrap();
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let DLDCCHMessageType::securityModeCommand(pdu::SecurityModeCommand::r3(body)) =
        decoded.value.message
    else {
        panic!("not Rel99")
    };
    assert!(body.security_mode_command_r3.ciphering_mode_info.is_none());
}

#[test]
fn completion_requires_authenticated_matching_transaction_and_all_activation_entries() {
    let context = context();
    let command = command(&context, true);
    let mut uplink =
        UplinkIntegrity::new(context.clone(), 2, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    for complete in [
        completed_message(1, true),
        completed_message(2, false),
        pdu::SecurityModeComplete::new(
            ie::RRCTransactionIdentifier(2),
            None,
            Some(activations(layout()).wire()),
            None,
        ),
    ] {
        let bytes = authenticated_bytes(&context, complete, 1);
        assert!(
            uplink
                .receive_with(
                    context.origin(),
                    &bytes,
                    &mut vec![0; bytes.len()],
                    |accepted| command.completion(accepted)
                )
                .is_err()
        );
        assert_eq!(uplink.last_sequence(), Some(0));
    }
    let bytes = authenticated_bytes(&context, completed_message(2, true), 1);
    let complete = uplink
        .receive_with(
            context.origin(),
            &bytes,
            &mut vec![0; bytes.len()],
            |accepted| command.completion(accepted),
        )
        .unwrap();
    assert_eq!(complete.count(), 1);
    assert_eq!(complete.origin(), context.origin());
    assert_eq!(complete.transaction().get(), 2);
    assert_eq!(complete.cipher(), Some(activations(layout())));
    assert_eq!(complete.integrity().sequence(0), Ok(0));
    assert_eq!(complete.integrity().sequence(3), Ok(0));
    assert_eq!(complete.integrity().sequence(4), Err(Error::Bearer));
    assert_eq!(uplink.last_sequence(), Some(1));
}

#[test]
fn exchange_commits_publication_and_completion_only_after_consumers_accept() {
    let context = context();
    let now = Instant::now();
    let mut exchange =
        SecurityExchange::prepare(command(&context, true), now, Duration::from_secs(5)).unwrap();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
    assert!(
        exchange
            .publish_with(now, &mut downlink, |_| Err::<(), _>(7))
            .is_err()
    );
    assert_eq!(exchange.phase(), ExchangePhase::Prepared);
    assert_eq!(downlink.next_count(), Ok(0));
    exchange
        .publish_with(now, &mut downlink, |_| Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(exchange.phase(), ExchangePhase::AwaitingResponse);
    let mut uplink =
        UplinkIntegrity::new(context.clone(), 2, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    let bytes = authenticated_bytes(&context, completed_message(2, true), 1);
    let mut scratch = vec![0; bytes.len()];
    assert!(
        uplink
            .receive_with(context.origin(), &bytes, &mut scratch, |accepted| exchange
                .complete_with(now, accepted, |_| Err::<(), _>(8)))
            .is_err()
    );
    assert_eq!(exchange.phase(), ExchangePhase::AwaitingResponse);
    assert_eq!(uplink.last_sequence(), Some(0));
    assert_eq!(
        uplink
            .receive_with(context.origin(), &bytes, &mut scratch, |accepted| exchange
                .complete_with(now, accepted, |_| Ok::<_, ()>(9)))
            .unwrap(),
        CompletionResult::Activated(9)
    );
    assert_eq!(exchange.phase(), ExchangePhase::Completed);
    assert_eq!(exchange.completion().unwrap().count(), 1);
    let duplicate = authenticated_bytes(&context, completed_message(2, true), 2);
    assert_eq!(
        uplink
            .receive_with(
                context.origin(),
                &duplicate,
                &mut vec![0; duplicate.len()],
                |accepted| exchange.complete_with(now, accepted, |_| -> Result<(), ()> {
                    panic!("duplicate republished core completion")
                })
            )
            .unwrap(),
        CompletionResult::Duplicate
    );
    assert_eq!(uplink.last_sequence(), Some(2));
}

#[test]
fn deadlines_start_at_cn_receipt_and_failed_rollback_is_retryable() {
    let context = context();
    let now = Instant::now();
    assert!(SecurityExchange::prepare(command(&context, false), now, Duration::ZERO).is_err());
    assert!(
        SecurityExchange::prepare(command(&context, false), now, Duration::from_secs(6)).is_err()
    );
    let mut exchange =
        SecurityExchange::prepare(command(&context, false), now, Duration::from_secs(5)).unwrap();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
    let deadline = now + Duration::from_secs(5);
    assert_eq!(
        exchange.expire_with(now + Duration::from_secs(4), |_, _| Ok::<_, ()>(())),
        Ok(None)
    );
    assert_eq!(
        exchange.publish_with(deadline, &mut downlink, |_| Ok::<_, ()>(())),
        Err(ExchangeError::Expired)
    );
    assert_eq!(downlink.next_count(), Ok(0));
    assert_eq!(
        exchange.expire_with(deadline, |origin, reason| {
            assert_eq!(origin, context.origin());
            assert_eq!(reason, AbortReason::Timeout);
            Err::<(), _>(7)
        }),
        Err(ExchangeError::Consumer(7))
    );
    assert_eq!(exchange.phase(), ExchangePhase::Prepared);
    assert_eq!(
        exchange.expire_with(deadline, |_, _| Ok::<_, ()>(8)),
        Ok(Some(8))
    );
    assert_eq!(exchange.phase(), ExchangePhase::Aborted);
    assert_eq!(exchange.abort_reason(), Some(AbortReason::Timeout));
    assert_eq!(
        exchange.expire_with(deadline, |_, _| -> Result<(), ()> {
            panic!("rollback repeated")
        }),
        Ok(None)
    );
}

#[test]
fn modify_retains_exact_per_srb_activation_order() {
    let context = context();
    let activation = IntegrityActivation::new(SignallingSet::Base, &[4, 15, 7, 0]).unwrap();
    let command = Command::new(
        CommandParameters {
            origin: context.origin(),
            transaction: TransactionId::new(3).unwrap(),
            layout: layout(),
            selection: selection(None),
            integrity: IntegrityChange::Modify(activation),
            cipher: None,
            dpch_cfn: None,
        },
        &capabilities(),
    )
    .unwrap();
    let mut downlink = DownlinkIntegrity::new(context, 2, FRESH, 7).unwrap();
    let encoded = command
        .publish_with(&mut downlink, |value| Ok::<_, ()>(value.clone()))
        .unwrap();
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let DLDCCHMessageType::securityModeCommand(pdu::SecurityModeCommand::r3(body)) =
        decoded.value.message
    else {
        panic!("not Rel99")
    };
    let mode = body
        .security_mode_command_r3
        .integrity_protection_mode_info
        .unwrap();
    let ie::IntegrityProtectionModeCommand::modify(modify) = mode.integrity_protection_mode_command
    else {
        panic!("not Modify")
    };
    assert_eq!(modify.dl_integrity_prot_activation_info, activation.wire());
}

#[test]
fn release7_uia2_publication_requires_matching_integrity_keys() {
    let mut capability = capabilities().security().clone();
    capability.ciphering_algorithm_cap.set(13, true);
    capability.integrity_protection_algorithm_cap.set(13, true);
    let capabilities = Capabilities::new(capability, None).unwrap();
    let support = capabilities.support();
    let selection = negotiation::select(
        Request {
            integrity: &[1, 0],
            cipher: Some(&[2, 1]),
            keys: KeyStatus::New,
        },
        Some(support),
        support,
        negotiation::Constraints::default(),
    )
    .unwrap();
    let context = context();
    let command = Command::new(
        CommandParameters {
            origin: context.origin(),
            transaction: TransactionId::new(1).unwrap(),
            layout: layout(),
            selection,
            integrity: IntegrityChange::Start { fresh: FRESH },
            cipher: Some(activations(layout())),
            dpch_cfn: None,
        },
        &capabilities,
    )
    .unwrap();
    let encoded = umts_rrc_codec::encode_downlink_dedicated(&command.message).unwrap();
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    let DLDCCHMessageType::securityModeCommand(pdu::SecurityModeCommand::later_than_r3(body)) =
        decoded.value.message
    else {
        panic!("not Release 7")
    };
    assert_eq!(body.rrc_transaction_identifier.0, 1);
    let pdu::SecurityModeCommandLaterThanR3CriticalExtensions::r7(body) = body.critical_extensions
    else {
        panic!("not Release 7 critical extension")
    };
    let body = body.security_mode_command_r7;
    assert_eq!(&body.security_capability, capabilities.security());
    assert_eq!(
        body.integrity_protection_mode_info
            .unwrap()
            .integrity_protection_algorithm,
        Some(ie::IntegrityProtectionAlgorithmR7::uia2)
    );
    assert_eq!(
        body.ciphering_mode_info
            .unwrap()
            .ciphering_mode_command
            .start_restart,
        ie::CipheringAlgorithmR7::uea2
    );
    let mut downlink = DownlinkIntegrity::new(context, 2, FRESH, 0).unwrap();
    assert_eq!(
        command.publish_with(&mut downlink, |_| -> Result<(), ()> {
            panic!("UIA2 was signed using UIA1")
        }),
        Err(crate::integrity::StreamError::Protection(
            crate::integrity::Error::Algorithm(umts_security::Error::UnsupportedAlgorithm)
        ))
    );
    assert_eq!(downlink.next_count(), Ok(0));
    let (_, snow) = KeySet::new(
        downlink.origin(),
        CipherMaterial::Uea2(Key::new([0x55; 16])),
        Key::new([0xaa; 16]),
        IntegrityAlgorithm::Uia2,
    )
    .split();
    let mut downlink = DownlinkIntegrity::new(snow, 2, FRESH, 0).unwrap();
    command
        .publish_with(&mut downlink, |encoded| {
            let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
            assert!(decoded.value.integrity_check_info.is_some());
            assert!(matches!(
                decoded.value.message,
                DLDCCHMessageType::securityModeCommand(_)
            ));
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(downlink.next_count(), Ok(1));
}
