// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::integrity::UplinkIntegrity;
use rasn::types::FixedOctetString;
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::class_definitions::ULDCCHMessage;
use umts_security::{
    Key,
    contexts::{CipherMaterial, IntegrityContext, KeyOrigin, KeySet},
    counters::Start,
};
use umts_types::{CnDomain, Direction, RadioEpoch};

const FRESH: u32 = 0x12345678;

use crate::test_support::capabilities as fixtures;
use fixtures::information;

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 3,
        generation: NonZeroU64::MIN,
    }
}
fn at(index: u64) -> ChipInstant {
    ChipInstant {
        epoch: key().epoch,
        index,
    }
}
fn transaction() -> TransactionId {
    TransactionId::new(2).unwrap()
}
fn gsm(class2: [u8; 5], class3: u8) -> ie::InterRATUERadioAccessCapability {
    ie::InterRATUERadioAccessCapability::gsm(ie::InterRATUERadioAccessCapabilityGsm::new(
        ie::GSMClassmark2(FixedOctetString::new(class2)),
        ie::GSMClassmark3(vec![class3].into()),
    ))
}
fn decoded(information: pdu::UECapabilityInformation) -> DecodedUplinkDedicated {
    let message = ULDCCHMessage::new(
        None,
        ULDCCHMessageType::ueCapabilityInformation(information),
    );
    let bytes = rasn::uper::encode(&message).unwrap();
    decode_uplink_dedicated(&bytes, bytes.len() * OCTET_BITS).unwrap()
}
fn enquiry(exchange: &mut CapabilityExchange) {
    exchange
        .publish_enquiry_with(
            key(),
            at(0),
            transaction(),
            NonZeroU64::new(100).unwrap(),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
}

#[test]
fn enquiry_and_confirmation_preserve_transaction_and_real_security_capabilities() {
    let mut exchange = CapabilityExchange::new(key(), at(0)).unwrap();
    assert_eq!(
        exchange.publish_enquiry_with(
            key(),
            at(0),
            transaction(),
            NonZeroU64::new(100).unwrap(),
            |_| Err::<(), _>("full")
        ),
        Err(ExchangeError::Downstream("full"))
    );
    assert_eq!(exchange.phase(), ExchangePhase::Idle);
    exchange
        .publish_enquiry_with(
            key(),
            at(0),
            transaction(),
            NonZeroU64::new(100).unwrap(),
            |encoded| {
                assert_eq!(encoded.bit_length, 17);
                assert_eq!(encoded.bytes, [0x50, 0xb0, 0]);
                let value = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
                let DLDCCHMessageType::ueCapabilityEnquiry(pdu::UECapabilityEnquiry::r3(body)) =
                    &value.value.message
                else {
                    panic!("expected capability enquiry");
                };
                assert_eq!(
                    body.ue_capability_enquiry_r3.rrc_transaction_identifier.0,
                    2
                );
                let request = &body.ue_capability_enquiry_r3.capability_update_requirement;
                assert!(request.ue_radio_capability_fddupdate_requirement);
                assert!(!request.ue_radio_capability_tddupdate_requirement);
                assert_eq!(
                    request
                        .system_specific_cap_update_req_list
                        .as_ref()
                        .unwrap()
                        .0,
                    [ie::SystemSpecificCapUpdateReq::gsm]
                );
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    let message = decoded(information());
    assert_eq!(
        exchange.receive_with(key(), at(1), 2, &message, |_| Err::<(), _>("full")),
        Err(ExchangeError::Downstream("full"))
    );
    assert!(exchange.report().is_none());
    assert_eq!(exchange.phase(), ExchangePhase::AwaitingInformation);
    let (result, ()) = exchange
        .receive_with(key(), at(1), 2, &message, |encoded| {
            assert_eq!(encoded.bit_length, 10);
            assert_eq!(encoded.bytes, [0x54, 0x80]);
            let value = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
            let DLDCCHMessageType::ueCapabilityInformationConfirm(
                pdu::UECapabilityInformationConfirm::r3(body),
            ) = &value.value.message
            else {
                panic!("expected capability confirmation");
            };
            assert_eq!(
                body.ue_capability_information_confirm_r3
                    .rrc_transaction_identifier
                    .0,
                2
            );
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(result, ReceiveResult::Confirmed);
    assert_eq!(
        exchange.report().unwrap().information().unwrap().as_ref(),
        &information()
    );
    assert_eq!(exchange.report().unwrap().key(), key());
    let support = exchange.report().unwrap().security().support();
    assert!(support.uia1 && support.uea1 && support.uea0);
    assert!(!support.uia2 && !support.uea2);
    assert_eq!(
        exchange
            .receive_with(key(), at(2), 2, &message, |_| Ok::<_, ()>(()))
            .unwrap()
            .0,
        ReceiveResult::DuplicateConfirmed
    );
}

#[test]
fn malformed_stale_unsolicited_and_expired_reports_never_replace_capabilities() {
    let mut exchange = CapabilityExchange::new(key(), at(0)).unwrap();
    let message = decoded(information());
    assert_eq!(
        exchange.receive_with(key(), at(0), 2, &message, |_| Ok::<_, ()>(())),
        Err(ExchangeError::Transaction)
    );
    enquiry(&mut exchange);
    let mut wrong = information();
    wrong.rrc_transaction_identifier = Some(ie::RRCTransactionIdentifier(1));
    assert_eq!(
        exchange.receive_with(key(), at(1), 2, &decoded(wrong), |_| Ok::<_, ()>(())),
        Err(ExchangeError::Transaction)
    );
    let mut missing = information();
    missing.ue_radio_access_capability = None;
    assert_eq!(
        exchange.receive_with(key(), at(1), 2, &decoded(missing), |_| Ok::<_, ()>(())),
        Err(ExchangeError::MissingRadioCapability)
    );
    assert_eq!(
        exchange.receive_with(key(), at(1), 3, &message, |_| Ok::<_, ()>(())),
        Err(ExchangeError::Message)
    );
    let mut stale = key();
    stale.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        exchange.receive_with(stale, at(1), 2, &message, |_| Ok::<_, ()>(())),
        Err(ExchangeError::Origin)
    );
    assert_eq!(
        exchange.receive_with(key(), at(100), 2, &message, |_| Ok::<_, ()>(())),
        Err(ExchangeError::Expired)
    );
    assert!(!exchange.expire(key(), at(99)).unwrap());
    assert!(exchange.expire(key(), at(100)).unwrap());
    assert!(exchange.report().is_none());
    assert_eq!(exchange.phase(), ExchangePhase::Idle);
}

#[test]
fn gsm_security_echo_maps_classmarks_without_inventing_algorithms() {
    for class2_a51 in [0, GSM_A51_UNSUPPORTED] {
        for class2_tail in 0..=3 {
            for class3_algorithms in 0..=15 {
                let mut report = information();
                report.ue_ratspecific_capability =
                    Some(ie::InterRATUERadioAccessCapabilityList(vec![gsm(
                        [0x33, 3, class2_a51, 0, class2_tail],
                        class3_algorithms,
                    )]));
                let report =
                    CapabilityReport::from_information::<()>(key(), &report, 1024).unwrap();
                let security = &report.security.inter_rat().unwrap().0;
                let [ie::InterRATUESecurityCapability::gsm(gsm)] = security.as_slice() else {
                    panic!("expected GSM security echo");
                };
                let bits = &gsm.gsm_security_capability.0;
                assert_eq!(bits[6], class2_a51 == 0);
                assert_eq!(bits[5], class2_tail & 1 != 0);
                assert_eq!(bits[4], class2_tail & 2 != 0);
                for index in 0..4 {
                    assert_eq!(bits[index], class3_algorithms & (1 << (3 - index)) != 0);
                }
            }
        }
    }
    for bad in [
        gsm([0, 3, 0, 0, 0], 0),
        gsm([0x33, 2, 0, 0, 0], 0),
        gsm([0x33, 3, 0, 0, 0], 0x30),
    ] {
        let mut report = information();
        report.ue_ratspecific_capability = Some(ie::InterRATUERadioAccessCapabilityList(vec![bad]));
        assert_eq!(
            CapabilityReport::from_information::<()>(key(), &report, 1024),
            Err(ExchangeError::InterRat)
        );
    }
    let mut report = information();
    let valid = gsm([0x33, 3, 0, 0, 0], 0);
    report.ue_ratspecific_capability = Some(ie::InterRATUERadioAccessCapabilityList(vec![
        valid.clone(),
        valid,
    ]));
    assert_eq!(
        CapabilityReport::from_information::<()>(key(), &report, 1024),
        Err(ExchangeError::InterRat)
    );
}

fn context() -> IntegrityContext {
    KeySet::uia1(
        KeyOrigin {
            ue: key(),
            domain: CnDomain::Ps,
            generation: NonZeroU64::MIN,
        },
        CipherMaterial::Clear,
        Key::new([0x5a; 16]),
    )
    .split()
    .1
}
fn fixed(value: u32) -> FixedBitString<32> {
    let mut bits = FixedBitString::default();
    for index in 0..32 {
        bits.set(index, value & (1 << (31 - index)) != 0);
    }
    bits
}
fn protected(context: &IntegrityContext) -> Vec<u8> {
    protected_information(context, information())
}

fn protected_information(
    context: &IntegrityContext,
    information: pdu::UECapabilityInformation,
) -> Vec<u8> {
    let mut message = decoded(information).value;
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(2)),
        ie::RRCMessageSequenceNumber(0),
    ));
    let preimage = rasn::uper::encode(message.as_ref()).unwrap();
    let mac = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            1,
            FRESH,
            Direction::Uplink,
            &preimage,
            preimage.len() * OCTET_BITS,
        )
        .unwrap();
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(mac))),
        ie::RRCMessageSequenceNumber(1),
    ));
    rasn::uper::encode(message.as_ref()).unwrap()
}

#[test]
fn authenticated_exchange_commits_both_directions_only_after_confirmation_admission() {
    let context = context();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
    let mut uplink =
        UplinkIntegrity::new(context.clone(), 2, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    let mut exchange = CapabilityExchange::new(key(), at(0)).unwrap();
    exchange
        .publish_authenticated_enquiry_with(
            at(0),
            transaction(),
            NonZeroU64::new(100).unwrap(),
            &mut downlink,
            |_| Ok::<_, &str>(()),
        )
        .unwrap();
    assert_eq!(downlink.next_count().unwrap(), 1);
    assert_eq!(
        exchange.receive_with(key(), at(1), 2, &decoded(information()), |_| Ok::<_, &str>(
            ()
        )),
        Err(ExchangeError::ProtectionRequired)
    );
    let bytes = protected(&context);
    let mut scratch = vec![0; bytes.len()];
    let failed = uplink.receive_with(context.origin(), &bytes, &mut scratch, |accepted| {
        exchange
            .receive_authenticated_with(at(1), accepted, &mut downlink, |_| Err::<(), _>("full"))
    });
    assert!(failed.is_err());
    assert_eq!(uplink.last_sequence(), Some(0));
    assert_eq!(downlink.next_count().unwrap(), 1);
    assert!(exchange.report().is_none());
    uplink
        .receive_with(context.origin(), &bytes, &mut scratch, |accepted| {
            exchange.receive_authenticated_with(at(1), accepted, &mut downlink, |encoded| {
                assert!(
                    decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                        .unwrap()
                        .value
                        .integrity_check_info
                        .is_some()
                );
                Ok::<_, &str>(())
            })
        })
        .unwrap();
    assert_eq!(uplink.last_sequence(), Some(1));
    assert_eq!(downlink.next_count().unwrap(), 2);
    assert_eq!(exchange.phase(), ExchangePhase::Confirmed);
}

#[test]
fn retained_report_is_bounded_and_a_failed_refresh_preserves_accepted_capabilities() {
    let mut exchange =
        CapabilityExchange::with_limit(key(), at(0), NonZeroUsize::new(1024).unwrap()).unwrap();
    enquiry(&mut exchange);
    exchange
        .receive_with(
            key(),
            at(1),
            2,
            &decoded(information()),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let previous = exchange.report().unwrap().encoded_information().to_vec();
    exchange
        .publish_enquiry_with(
            key(),
            at(2),
            TransactionId::new(3).unwrap(),
            NonZeroU64::new(100).unwrap(),
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let mut refreshed = information();
    refreshed.rrc_transaction_identifier = Some(ie::RRCTransactionIdentifier(3));
    refreshed
        .ue_radio_access_capability
        .as_mut()
        .unwrap()
        .security_capability
        .ciphering_algorithm_cap
        .set(14, false);
    assert_eq!(
        exchange.receive_with(key(), at(3), 2, &decoded(refreshed), |_| Err::<(), _>(
            "full"
        )),
        Err(ExchangeError::Downstream("full"))
    );
    assert_eq!(exchange.report().unwrap().encoded_information(), previous);
    assert!(exchange.report().unwrap().security().support().uea1);
    assert!(exchange.expire(key(), at(102)).unwrap());
    assert_eq!(exchange.report().unwrap().encoded_information(), previous);
    let mut tiny = CapabilityExchange::with_limit(key(), at(0), NonZeroUsize::MIN).unwrap();
    enquiry(&mut tiny);
    assert_eq!(
        tiny.receive_with(
            key(),
            at(1),
            2,
            &decoded(information()),
            |_| Ok::<_, ()>(())
        ),
        Err(ExchangeError::Length)
    );
    assert!(tiny.report().is_none());
    assert!(CapabilityExchange::buffer_bytes(NonZeroUsize::new(1024).unwrap()).unwrap() >= 2048);
    let beyond_codec = NonZeroUsize::new(MAX_DEDICATED_BITS / OCTET_BITS + 1).unwrap();
    assert!(CapabilityExchange::with_limit(key(), at(0), beyond_codec).is_err());
    assert!(CapabilityExchange::buffer_bytes(beyond_codec).is_none());
}

#[test]
fn setup_complete_capabilities_are_used_only_when_the_ue_supplied_them() {
    let mut completion = pdu::RRCConnectionSetupComplete::new(
        ie::RRCTransactionIdentifier(0),
        ie::STARTList(vec![ie::STARTSingle::new(
            ie::CNDomainIdentity::cs_domain,
            ie::STARTValue(FixedBitString::default()),
        )]),
        None,
        None,
        None,
    );
    assert_eq!(setup_security_capabilities(&completion).unwrap(), None);
    completion.ue_radio_access_capability = information().ue_radio_access_capability;
    let accepted = setup_security_capabilities(&completion).unwrap().unwrap();
    assert!(accepted.support().uia1 && accepted.support().uea1);
    assert_eq!(
        accepted.security(),
        &completion
            .ue_radio_access_capability
            .as_ref()
            .unwrap()
            .security_capability
    );
    completion.ue_ratspecific_capability =
        Some(ie::InterRATUERadioAccessCapabilityList(vec![gsm(
            [0x33, 3, 8, 0, 2],
            0x0a,
        )]));
    let accepted = setup_security_capabilities(&completion).unwrap().unwrap();
    let [ie::InterRATUESecurityCapability::gsm(gsm)] = accepted.inter_rat().unwrap().0.as_slice()
    else {
        panic!("expected retained GSM security capability");
    };
    assert_eq!(
        rasn::uper::encode(&gsm.gsm_security_capability).unwrap(),
        [0b1010_1000]
    );
}

fn changed_information() -> pdu::UECapabilityInformation {
    let mut report = information();
    report
        .ue_radio_access_capability
        .as_mut()
        .unwrap()
        .security_capability
        .ciphering_algorithm_cap
        .set(14, false);
    report
}

fn assert_change_refused(encoded: &EncodedRrc) {
    let decoded = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
    if decoded.value.integrity_check_info.is_none() {
        assert_eq!(encoded.bit_length, 16);
        assert_eq!(encoded.bytes, [0x55, 0x94]);
    }
    let DLDCCHMessageType::ueCapabilityInformationConfirm(pdu::UECapabilityInformationConfirm::r3(
        body,
    )) = &decoded.value.message
    else {
        panic!("expected capability confirmation");
    };
    assert_eq!(
        body.ue_capability_information_confirm_r3
            .rrc_transaction_identifier
            .0,
        2
    );
    let extension = body.later_non_critical_extensions.as_ref().unwrap();
    assert!(
        extension
            .ue_capability_information_confirm_r3_add_ext
            .is_none()
    );
    let extension = extension.v770_non_critical_extensions.as_ref().unwrap();
    assert_eq!(
        extension
            .ue_capability_information_confirm_v770ext
            .acceptance_of_change_of_capability,
        Some(pdu::UECapabilityInformationConfirmV770extIEsAcceptanceOfChangeOfCapability::refused),
    );
    assert!(extension.non_critical_extensions.is_none());
}

#[test]
fn unsolicited_change_is_refused_without_replacing_admission_capabilities() {
    let mut exchange = CapabilityExchange::new(key(), at(0)).unwrap();
    enquiry(&mut exchange);
    exchange
        .receive_with(key(), at(1), 2, &decoded(information()), |_| {
            Ok::<_, &str>(())
        })
        .unwrap();
    let previous = exchange.report().unwrap().clone();
    let changed = decoded(changed_information());
    assert_eq!(
        exchange.receive_with(key(), at(2), 2, &changed, |_| Err::<(), _>("full")),
        Err(ExchangeError::Downstream("full")),
    );
    assert_eq!(exchange.report(), Some(&previous));
    assert_eq!(exchange.changed_at, at(1));
    for index in [2, 3] {
        let (result, ()) = exchange
            .receive_with(key(), at(index), 2, &changed, |encoded| {
                assert_change_refused(encoded);
                Ok::<_, &str>(())
            })
            .unwrap();
        assert_eq!(result, ReceiveResult::ChangeRefused);
        assert_eq!(exchange.report(), Some(&previous));
        assert_eq!(exchange.phase(), ExchangePhase::Confirmed);
    }
    let mut duplicate = information();
    duplicate.rrc_transaction_identifier = Some(ie::RRCTransactionIdentifier(3));
    assert_eq!(
        exchange
            .receive_with(key(), at(4), 2, &decoded(duplicate), |encoded| {
                let value = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
                let DLDCCHMessageType::ueCapabilityInformationConfirm(
                    pdu::UECapabilityInformationConfirm::r3(body),
                ) = &value.value.message
                else {
                    panic!("expected confirmation")
                };
                assert_eq!(
                    body.ue_capability_information_confirm_r3
                        .rrc_transaction_identifier
                        .0,
                    3
                );
                assert!(body.later_non_critical_extensions.is_none());
                Ok::<_, &str>(())
            })
            .unwrap()
            .0,
        ReceiveResult::DuplicateConfirmed
    );
    assert_eq!(exchange.report(), Some(&previous));
}

#[test]
fn protected_change_refusal_commits_integrity_only_after_publication() {
    let context = context();
    let mut downlink = DownlinkIntegrity::new(context.clone(), 2, FRESH, 0).unwrap();
    let mut uplink =
        UplinkIntegrity::new(context.clone(), 2, FRESH, Start::new(0).unwrap(), Some(0)).unwrap();
    let mut exchange = CapabilityExchange::new(key(), at(0)).unwrap();
    enquiry(&mut exchange);
    exchange
        .receive_with(key(), at(1), 2, &decoded(information()), |_| {
            Ok::<_, &str>(())
        })
        .unwrap();
    let previous = exchange.report().unwrap().clone();
    let bytes = protected_information(&context, changed_information());
    let mut scratch = vec![0; bytes.len()];
    assert!(
        uplink
            .receive_with(context.origin(), &bytes, &mut scratch, |accepted| {
                exchange.receive_authenticated_with(at(2), accepted, &mut downlink, |_| {
                    Err::<(), _>("full")
                })
            })
            .is_err()
    );
    assert_eq!(uplink.last_sequence(), Some(0));
    assert_eq!(downlink.next_count().unwrap(), 0);
    assert_eq!(exchange.report(), Some(&previous));
    assert_eq!(exchange.changed_at, at(1));
    uplink
        .receive_with(context.origin(), &bytes, &mut scratch, |accepted| {
            let result =
                exchange.receive_authenticated_with(at(2), accepted, &mut downlink, |encoded| {
                    assert_change_refused(encoded);
                    assert!(
                        decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                            .unwrap()
                            .value
                            .integrity_check_info
                            .is_some()
                    );
                    Ok::<_, &str>(())
                })?;
            assert_eq!(result.0, ReceiveResult::ChangeRefused);
            Ok::<_, ExchangeError<&str>>(())
        })
        .unwrap();
    assert_eq!(uplink.last_sequence(), Some(1));
    assert_eq!(downlink.next_count().unwrap(), 1);
    assert_eq!(exchange.report(), Some(&previous));
    assert!(exchange.integrity_required);
}

#[test]
fn solicited_refresh_cannot_replace_capabilities_without_reconfiguration() {
    let mut exchange = CapabilityExchange::new(key(), at(0)).unwrap();
    enquiry(&mut exchange);
    exchange
        .receive_with(key(), at(1), 2, &decoded(information()), |_| {
            Ok::<_, &str>(())
        })
        .unwrap();
    let previous = exchange.report().unwrap().clone();
    exchange
        .publish_enquiry_with(
            key(),
            at(2),
            transaction(),
            NonZeroU64::new(100).unwrap(),
            |_| Ok::<_, &str>(()),
        )
        .unwrap();
    let changed = decoded(changed_information());
    assert_eq!(
        exchange.receive_with(key(), at(3), 2, &changed, |_| Err::<(), _>("full")),
        Err(ExchangeError::Downstream("full"))
    );
    assert_eq!(exchange.phase(), ExchangePhase::AwaitingInformation);
    assert_eq!(exchange.pending_transaction(), Some(transaction()));
    assert_eq!(exchange.report(), Some(&previous));
    assert_eq!(
        exchange
            .receive_with(key(), at(3), 2, &changed, |encoded| {
                assert_change_refused(encoded);
                Ok::<_, &str>(())
            })
            .unwrap()
            .0,
        ReceiveResult::ChangeRefused
    );
    assert_eq!(exchange.phase(), ExchangePhase::Confirmed);
    assert_eq!(exchange.pending_transaction(), None);
    assert_eq!(exchange.report(), Some(&previous));
    exchange
        .publish_enquiry_with(
            key(),
            at(4),
            transaction(),
            NonZeroU64::new(100).unwrap(),
            |_| Ok::<_, &str>(()),
        )
        .unwrap();
    assert_eq!(
        exchange
            .receive_with(key(), at(5), 2, &decoded(information()), |_| Ok::<_, &str>(
                ()
            ))
            .unwrap()
            .0,
        ReceiveResult::Confirmed
    );
    assert_eq!(exchange.report(), Some(&previous));
}
