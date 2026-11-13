// SPDX-License-Identifier: AGPL-3.0-or-later

mod rekey;

use super::*;
use crate::test_support::integrity::{fixed, protected};
use crate::{
    common_tx::{CommonTx, tests::recover_current_fach},
    connection_security::{Phase, RadioPlan},
    signalling::tests::{COMPLETE, INITIAL, send_capability_report},
};
use num_complex::Complex32;
use std::time::Instant;
use umts_mac::{
    CommonHeader, CommonIdentity, FddCommonChannel, LogicalChannelId, decode_common, encode_common,
};
use umts_rlc::acknowledged::{AmEntity, SduTransmission};
use umts_rrc::{
    establishment::TransactionId,
    nas::{DownlinkPriority, SignallingBearer},
    security::{ActivationSequences, BearerLayout, IntegrityChange, SignallingSet},
};
use umts_rrc_codec::decode_downlink_dedicated;
use umts_rrc_schema::{
    class_definitions::ULDCCHMessageType, information_elements as ie, pdu_definitions as pdu,
};
use umts_security::{
    Key,
    contexts::{CipherContext, CipherMaterial, IntegrityContext, KeySet},
};
use umts_types::{
    Direction,
    bits::{BITS_PER_BYTE, Bits},
    time::{CHIPS_PER_FRAME, ChipInstant},
};

fn nas(context: &IntegrityContext, fresh: u32, count: u32) -> Vec<u8> {
    protected(
        context,
        fresh,
        3,
        count,
        ULDCCHMessageType::uplinkDirectTransfer(pdu::UplinkDirectTransfer::new(
            ie::CNDomainIdentity::cs_domain,
            ie::NASMessage(vec![9, count as u8].into()),
            None,
            None,
        )),
    )
}
fn uplink(
    radio: &mut CommonSignalling,
    key: UeKey,
    bearer: SignallingBearer,
    peer: &mut AmEntity,
    now: ChipInstant,
    bytes: &[u8],
    cipher: Option<&CipherContext>,
) {
    peer.enqueue(bytes).unwrap();
    let identity = radio.assigned(key).unwrap().crnti();
    for _ in 0..64 {
        if !peer.pending_new_data() {
            return;
        }
        peer.publish_next(now, |publication| {
            let mut bytes = publication.bytes.to_vec();
            crypt(cipher, bearer, 0, Direction::Uplink, &mut bytes);
            let mut mac = vec![0; publication.bytes.len() + 3];
            encode_common(
                FddCommonChannel::Rach,
                CommonHeader::Dedicated {
                    identity: CommonIdentity::Cell(identity),
                    channel: LogicalChannelId::new(bearer.identity()).unwrap(),
                },
                Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
                &mut mac,
            )
            .unwrap();
            radio.receive(
                key,
                now,
                decode_common(
                    FddCommonChannel::Rach,
                    Bits::new(&mac, mac.len() * BITS_PER_BYTE).unwrap(),
                )
                .unwrap(),
            )
        })
        .unwrap()
        .unwrap();
        for _ in 0..64 {
            if !radio.collect_one(key, bearer, now).unwrap() {
                break;
            }
        }
    }
    panic!("uplink SDU exceeded bounded publication work");
}
pub(crate) fn fach(
    radio: &mut CommonSignalling,
    source: &mut CommonTx,
    key: UeKey,
    bearer: SignallingBearer,
    peer: &mut AmEntity,
    cipher: Option<&CipherContext>,
) -> Vec<Vec<u8>> {
    let now = source.next_chip();
    let published = radio.publish_bearer(key, bearer, source, now, now).unwrap();
    let mut samples = vec![Complex32::default(); source.samples_per_slot()];
    source.generate_slot(&mut samples).unwrap();
    let mut result = Vec::new();
    if published {
        let bytes = recover_current_fach(source);
        let pdu = decode_common(
            FddCommonChannel::Fach,
            Bits::new(&bytes, bytes.len() * BITS_PER_BYTE).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pdu.header,
            CommonHeader::Dedicated {
                identity: CommonIdentity::Cell(radio.assigned(key).unwrap().crnti()),
                channel: LogicalChannelId::new(bearer.identity()).unwrap()
            }
        );
        let mut data = vec![0; pdu.payload.len() / BITS_PER_BYTE];
        pdu.payload.copy_to(&mut data).unwrap();
        crypt(
            cipher,
            bearer,
            if bearer == SignallingBearer::ControlAm {
                CONTROL_CIPHER_ACTIVATION
            } else {
                0
            },
            Direction::Downlink,
            &mut data,
        );
        peer.receive(now, &data).unwrap();
        for _ in 0..64 {
            if peer
                .deliver_with(|delivery| {
                    result.extend(delivery.sdus().map(<[u8]>::to_vec));
                    Ok::<_, ()>(())
                })
                .unwrap()
                .is_none()
            {
                break;
            }
        }
    }
    for _ in 1..15 {
        source.generate_slot(&mut samples).unwrap();
    }
    assert_eq!(source.next_chip().index, now.index + CHIPS_PER_FRAME);
    result
}
const CONTROL_CIPHER_ACTIVATION: u16 = 32;

fn crypt(
    cipher: Option<&CipherContext>,
    bearer: SignallingBearer,
    activation: u16,
    direction: Direction,
    bytes: &mut [u8],
) {
    let Some(cipher) = cipher else {
        return;
    };
    let header = umts_security::ciphering::header(
        umts_security::counters::CipherMode::Acknowledged,
        bytes,
        bytes.len() * BITS_PER_BYTE,
    )
    .unwrap();
    if let umts_security::ciphering::Header::Data {
        sequence: Some(sequence),
        clear_octets,
    } = header
        && sequence >= activation
    {
        let body = &mut bytes[clear_octets..];
        cipher
            .apply(
                cipher.origin(),
                u32::from(sequence),
                bearer.identity() - 1,
                direction,
                body,
                body.len() * BITS_PER_BYTE,
            )
            .unwrap();
    }
}
fn time(ms: u64) -> SecurityTime {
    SecurityTime {
        control: ControlTime(ms),
        monotonic: Instant::now(),
    }
}

#[test]
fn core_command_fach_delivery_authenticated_completion_and_protected_nas_are_connected() {
    exercise_integrity_admission(Failure::None, Encryption::NoAction);
}
#[test]
fn authenticated_security_completion_allows_protected_bearer_setup_without_another_nas() {
    for encryption in [Encryption::NoAction, Encryption::Clear, Encryption::Uea1] {
        exercise_security(Failure::None, encryption, PostCompleteAction::Bearer);
    }
}
#[test]
fn rejected_completion_keeps_status_held_after_the_ingress_slot_is_removed() {
    exercise_integrity_admission(Failure::Mac, Encryption::NoAction);
}
#[test]
fn cipher_command_completion_and_bidirectional_nas_follow_the_actual_transport_boundaries() {
    exercise_integrity_admission(Failure::None, Encryption::Uea1);
}
#[test]
fn cipher_maps_and_suspension_do_not_commit_from_a_rejected_completion() {
    exercise_integrity_admission(Failure::Mac, Encryption::Uea1);
}
#[test]
fn explicitly_configured_clear_ciphering_uses_the_same_activation_and_admission_path() {
    exercise_integrity_admission(Failure::None, Encryption::Clear);
}
#[test]
fn authenticated_completion_cannot_activate_ciphering_inside_its_own_rlc_coverage() {
    exercise_integrity_admission(Failure::Activation, Encryption::Uea1);
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encryption {
    NoAction,
    Clear,
    Uea1,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Failure {
    None,
    Mac,
    Activation,
}
fn exercise_integrity_admission(failure: Failure, encryption: Encryption) {
    exercise_security(failure, encryption, PostCompleteAction::None);
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PostCompleteAction {
    None,
    Bearer,
    Page,
    ConfirmDelivery,
}

#[test]
fn protected_page_does_not_wait_for_unrelated_nas_peer_progress() {
    for encryption in [Encryption::NoAction, Encryption::Clear, Encryption::Uea1] {
        exercise_security(Failure::None, encryption, PostCompleteAction::Page);
    }
}

#[test]
fn acknowledged_piggybacked_status_confirms_security_without_another_uplink_nas() {
    for encryption in [Encryption::NoAction, Encryption::Clear, Encryption::Uea1] {
        exercise_security(
            Failure::None,
            encryption,
            PostCompleteAction::ConfirmDelivery,
        );
    }
}

fn exercise_security(failure: Failure, encryption: Encryption, action: PostCompleteAction) {
    let publish_bearer = action == PostCompleteAction::Bearer;
    let ciphering = encryption != Encryption::NoAction;
    let mut source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context_id, mut nas_peer) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let mut control_peer = radio_peer(&source);
    control_peer.enqueue(&COMPLETE).unwrap();
    control_peer
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    send_capability_report(&mut radio, key, &mut control_peer);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    for _ in 0..if ciphering { 0 } else { 4 } {
        fach(
            &mut radio,
            &mut source,
            key,
            SignallingBearer::ControlAm,
            &mut control_peer,
            None,
        );
    }
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: ciphering.then(|| ranap::Protection {
            algorithms: vec![ranap::Algorithm(if encryption == Encryption::Clear {
                0
            } else {
                1
            })],
            key: ranap::Key::new([0x22; 16]),
        }),
        key_status: ranap::KeyStatus::New,
    };
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context: context_id,
        ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
    });
    drive(&mut link, &mut radio, 6);
    let fresh = radio.security(key).unwrap().fresh();
    radio
        .prepare_security(
            key,
            Domain::Cs,
            time(7),
            RadioPlan {
                transaction: TransactionId::new(2).unwrap(),
                layout: BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
                integrity: IntegrityChange::Start { fresh },
                cipher: ciphering.then(|| {
                    ActivationSequences::new(
                        BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
                        &[(1, 0), (2, CONTROL_CIPHER_ACTIVATION), (3, 0)],
                    )
                    .unwrap()
                }),
                dpch_cfn: None,
            },
        )
        .unwrap();
    let origin = radio
        .security(key)
        .unwrap()
        .pending_origin(Domain::Cs)
        .unwrap();
    let (cipher, integrity) = KeySet::uia1(
        origin,
        if encryption == Encryption::Uea1 {
            CipherMaterial::Uea1(Key::new([0x22; 16]))
        } else {
            CipherMaterial::Clear
        },
        Key::new([0x11; 16]),
    )
    .split();
    let cipher = ciphering.then_some(cipher);
    if ciphering {
        assert_eq!(
            radio.publish_security_command(key, Domain::Cs, time(8)),
            Err(SignallingError::Capacity)
        );
        assert_eq!(
            radio.security(key).unwrap().phase(Domain::Cs),
            Phase::Prepared
        );
        assert_eq!(radio.security_delivery(key).unwrap(), None);
        for _ in 0..4 {
            fach(
                &mut radio,
                &mut source,
                key,
                SignallingBearer::ControlAm,
                &mut control_peer,
                None,
            );
        }
    }
    radio
        .publish_security_command(key, Domain::Cs, time(8))
        .unwrap();
    assert_eq!(
        radio.security_delivery(key).unwrap().unwrap().command,
        SduTransmission::Queued
    );
    let mut received_command = false;
    for _ in 0..32 {
        for bytes in fach(
            &mut radio,
            &mut source,
            key,
            SignallingBearer::ControlAm,
            &mut control_peer,
            cipher.as_ref(),
        ) {
            let decoded = decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
            if matches!(
                decoded.value.message,
                umts_rrc_schema::class_definitions::DLDCCHMessageType::securityModeCommand(_)
            ) {
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
                received_command = true;
            }
        }
        if received_command {
            break;
        }
    }
    assert!(received_command);
    assert!(matches!(
        radio.security_delivery(key).unwrap().unwrap().command,
        SduTransmission::InFlight {
            delimited: true,
            ..
        }
    ));
    if !publish_bearer {
        for count in 1..=4 {
            uplink(
                &mut radio,
                key,
                SignallingBearer::NasAm,
                &mut nas_peer,
                source.next_chip(),
                &nas(&integrity, fresh, count),
                None,
            );
        }
    }
    assert_eq!(radio.process_one_at(time(9)), None);
    assert!(
        radio
            .with_bearer_access(key, |_, permission, _| matches!(
                permission,
                crate::dedicated_bearers::ResetPermission::Defer
            ))
            .unwrap()
    );
    let mut complete = protected(
        &integrity,
        fresh,
        2,
        1,
        ULDCCHMessageType::securityModeComplete(pdu::SecurityModeComplete::new(
            ie::RRCTransactionIdentifier(2),
            Some(ie::IntegrityProtActivationInfo::new(
                ie::RRCMessageSequenceNumberList(vec![ie::RRCMessageSequenceNumber(0); 4]),
            )),
            ciphering.then(|| {
                ie::RBActivationTimeInfoList(vec![
                    ie::RBActivationTimeInfo::new(ie::RBIdentity(1), ie::RLCSequenceNumber(0)),
                    ie::RBActivationTimeInfo::new(
                        ie::RBIdentity(2),
                        ie::RLCSequenceNumber(if failure == Failure::Activation {
                            control_peer.send_sequence()
                        } else {
                            CONTROL_CIPHER_ACTIVATION
                        }),
                    ),
                    ie::RBActivationTimeInfo::new(
                        ie::RBIdentity(3),
                        ie::RLCSequenceNumber(nas_peer.send_sequence()),
                    ),
                ])
            }),
            None,
        )),
    );
    if failure == Failure::Mac {
        const FIRST_MAC_BIT: u8 = 1 << 6;
        complete[0] ^= FIRST_MAC_BIT;
    }
    uplink(
        &mut radio,
        key,
        SignallingBearer::ControlAm,
        &mut control_peer,
        source.next_chip(),
        &complete,
        None,
    );
    assert_eq!(
        radio
            .security_delivery(key)
            .unwrap()
            .unwrap()
            .status_deferral,
        Some(umts_rlc::acknowledged::StatusDeferral::UpperAdmission)
    );
    let outstanding = control_peer.outstanding_pdus();
    assert!(outstanding > 0);
    assert!(
        fach(
            &mut radio,
            &mut source,
            key,
            SignallingBearer::ControlAm,
            &mut control_peer,
            cipher.as_ref()
        )
        .is_empty()
    );
    assert_eq!(control_peer.outstanding_pdus(), outstanding);
    assert_eq!(radio.process_one(), None);
    if failure != Failure::None {
        assert_eq!(
            radio.process_one_at(time(9)),
            Some(Err(if failure == Failure::Mac {
                SignallingError::Integrity(umts_rrc::integrity::ScheduleFailure::Integrity(
                    umts_rrc::integrity::Error::Authentication,
                ))
            } else {
                SignallingError::ConfigurationFailure
            }))
        );
        let delivery = radio.security_delivery(key).unwrap().unwrap();
        assert!(matches!(
            delivery.admission,
            crate::signalling::ControlAdmission::RecoveryRequired { rejected: Some(_) }
        ));
        assert!(delivery.completion.is_none());
        assert_eq!(
            delivery.status_deferral,
            Some(umts_rlc::acknowledged::StatusDeferral::UpperAdmission)
        );
        assert!(
            fach(
                &mut radio,
                &mut source,
                key,
                SignallingBearer::ControlAm,
                &mut control_peer,
                cipher.as_ref()
            )
            .is_empty()
        );
        assert_eq!(control_peer.outstanding_pdus(), outstanding);
        assert_eq!(radio.security(key).unwrap().active_origin(Domain::Cs), None);
        assert_eq!(radio.core().len(), 0);
        let mut recovery = false;
        for _ in 0..16 {
            recovery |= radio.service_security_one(time(9)).unwrap()
                == SecurityWork::RecoveryRequired { key };
        }
        assert!(recovery);
        return;
    }
    assert_eq!(radio.process_one_at(time(9)), Some(Ok(key)));
    assert_eq!(
        radio.security(key).unwrap().active_origin(Domain::Cs),
        Some(origin)
    );
    assert_eq!(
        radio.security(key).unwrap().phase(Domain::Cs),
        Phase::Reporting
    );
    let delivery = radio.security_delivery(key).unwrap().unwrap();
    assert!(delivery.completion.is_some());
    assert!(!delivery.completion_status_queued);
    assert!(
        radio
            .with_bearer_access(key, |_, permission, _| matches!(
                permission,
                crate::dedicated_bearers::ResetPermission::Settled { cs, .. }
                    if cs == Some(origin)
            ))
            .unwrap()
    );
    assert_eq!(
        radio.initiate_am_reset(key, SignallingBearer::ControlAm, source.next_chip()),
        Err(crate::signalling::SignallingError::Phase)
    );
    assert_eq!(radio.security_delivery(key).unwrap(), Some(delivery));
    assert!(
        !radio
            .bearer_setup_readiness(key)
            .unwrap()
            .protection_blocks_reconfiguration
    );
    if matches!(
        action,
        PostCompleteAction::Page | PostCompleteAction::ConfirmDelivery
    ) {
        assert!(!delivery.peer_progress_observed);
        let imsi = umts_types::identity::Imsi::from_digits("001010000000001").unwrap();
        radio
            .associate_core_identity(
                radio.connection(key, umts_types::CnDomain::Cs).unwrap(),
                imsi,
            )
            .unwrap();
        let page = umts_iuh::ranap::paging::Request {
            domain: Domain::Ps,
            imsi,
            temporary: None,
            area: None,
            cause: None,
            drx_coefficient: Some(6),
            search_other_domain: true,
        };
        radio.receive_paging(&page);
        for _ in 0..32 {
            let now = source.next_chip();
            radio.service_paging(&mut source, now, 0);
        }
        assert_eq!(radio.counters().paging.dedicated_queued, 1);
        assert_eq!(radio.security_delivery(key).unwrap(), Some(delivery));
        let mut received = false;
        for _ in 0..32 {
            for bytes in fach(
                &mut radio,
                &mut source,
                key,
                SignallingBearer::ControlAm,
                &mut control_peer,
                cipher.as_ref(),
            ) {
                let decoded =
                    decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
                if matches!(
                    decoded.value.message,
                    umts_rrc_schema::class_definitions::DLDCCHMessageType::pagingType2(_)
                ) {
                    assert!(decoded.value.integrity_check_info.is_some());
                    received = true;
                }
            }
            if received {
                break;
            }
        }
        assert!(received);
        assert!(
            !radio
                .security_delivery(key)
                .unwrap()
                .unwrap()
                .peer_progress_observed
        );
        if action == PostCompleteAction::ConfirmDelivery {
            assert!(
                !radio
                    .security_delivery(key)
                    .unwrap()
                    .unwrap()
                    .completion_status_acknowledged
            );
            let now = source.next_chip();
            control_peer
                .publish_next(now, |publication| {
                    assert_eq!(
                        publication.kind,
                        umts_rlc::acknowledged::PublicationKind::Status
                    );
                    radio.receive(
                        key,
                        now,
                        umts_mac::CommonPdu {
                            header: CommonHeader::Dedicated {
                                identity: CommonIdentity::Cell(
                                    radio.assigned(key).unwrap().crnti(),
                                ),
                                channel: LogicalChannelId::new(2).unwrap(),
                            },
                            payload: Bits::new(
                                publication.bytes,
                                publication.bytes.len() * BITS_PER_BYTE,
                            )
                            .unwrap(),
                        },
                    )
                })
                .unwrap()
                .unwrap();
            let evidence = radio.security_delivery(key).unwrap().unwrap();
            assert!(!evidence.peer_progress_observed);
            assert!(evidence.completion_status_acknowledged);
            let mut retired = false;
            for _ in 0..16 {
                retired |= radio.service_security_one(time(10)).unwrap()
                    == SecurityWork::DeliveryRetired { key, origin };
            }
            assert!(retired);
            assert_eq!(radio.security_delivery(key).unwrap(), None);
            assert_eq!(radio.security(key).unwrap().unconfirmed_origin(), None);
            assert_eq!(
                radio.security(key).unwrap().active_origin(Domain::Cs),
                Some(origin)
            );
        }
        return;
    }
    if publish_bearer {
        assert!(!delivery.completion_status_submitted && !delivery.peer_progress_observed);
        let now = source.next_chip();
        let mut procedure = crate::signalling::tests::initial_dch_procedure(&radio, key, now);
        radio.publish_bearer_setup(&mut procedure, now).unwrap();
        assert_eq!(
            procedure.phase(),
            umts_rrc::dedicated::SetupPhase::AwaitingComplete
        );
        assert_eq!(radio.security_delivery(key).unwrap(), Some(delivery));
        let mut received = false;
        for _ in 0..32 {
            for bytes in fach(
                &mut radio,
                &mut source,
                key,
                SignallingBearer::ControlAm,
                &mut control_peer,
                cipher.as_ref(),
            ) {
                let decoded =
                    decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
                if matches!(
                    decoded.value.message,
                    umts_rrc_schema::class_definitions::DLDCCHMessageType::radioBearerSetup(_)
                ) {
                    let info = decoded.value.integrity_check_info.as_ref().unwrap();
                    assert_eq!(info.rrc_message_sequence_number.0, 1);
                    let mut preimage = decoded.value.as_ref().clone();
                    preimage.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
                        ie::MessageAuthenticationCode(fixed(u32::from(
                            SignallingBearer::ControlAm.identity(),
                        ))),
                        ie::RRCMessageSequenceNumber(0),
                    ));
                    let preimage = umts_rrc_codec::encode_downlink_dedicated(&preimage).unwrap();
                    let expected = integrity
                        .algorithm(origin)
                        .unwrap()
                        .authenticate(
                            1,
                            fresh,
                            Direction::Downlink,
                            &preimage.bytes,
                            preimage.bytes.len() * BITS_PER_BYTE,
                        )
                        .unwrap();
                    for bit in 0..32 {
                        assert_eq!(
                            info.message_authentication_code.0[bit],
                            expected[bit / BITS_PER_BYTE]
                                & (1 << (BITS_PER_BYTE - 1 - bit % BITS_PER_BYTE))
                                != 0
                        );
                    }
                    received = true;
                }
            }
            if received {
                break;
            }
        }
        assert!(received);
        let retained = radio.security_delivery(key).unwrap().unwrap();
        assert_eq!(retained.origin, delivery.origin);
        assert!(!retained.peer_progress_observed);
        assert_eq!(
            radio.security(key).unwrap().active_origin(Domain::Cs),
            Some(origin)
        );
        let transaction = procedure.transaction();
        let mut start_bits = rasn::types::FixedBitString::<20>::default();
        const NEW_BEARER_START: u32 = 92;
        for bit in 0..20 {
            start_bits.set(bit, NEW_BEARER_START & (1 << (19 - bit)) != 0);
        }
        let mut complete = pdu::RadioBearerSetupComplete::new(
            ie::RRCTransactionIdentifier(transaction.get()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        complete.start_value = Some(ie::STARTValue(start_bits));
        let response = ULDCCHMessageType::radioBearerSetupComplete(complete);
        assert!(control_peer.send_sequence() < CONTROL_CIPHER_ACTIVATION);
        let received_at = source.next_chip();
        uplink(
            &mut radio,
            key,
            SignallingBearer::ControlAm,
            &mut control_peer,
            received_at,
            &protected(&integrity, fresh, 2, 2, response),
            None,
        );
        assert_eq!(
            radio.process_one_at_with_control(received_at, time(10), |received, mut security| {
                assert_eq!(security.starts().cs, Some(0));
                assert_eq!(security.starts().ps, Some(0x54321));
                let user = security.user_cipher(CnDomain::Cs)?;
                assert_eq!(user.is_some(), ciphering);
                if let Some(user) = user {
                    assert_eq!(user.context.origin(), origin);
                }
                assert!(security.user_cipher(CnDomain::Ps)?.is_none());
                assert_eq!(
                    received.bearer_setup_response(&mut procedure, &mut security)?,
                    umts_rrc::dedicated::SetupResponse::Complete
                );
                assert_eq!(security.starts().cs, Some(NEW_BEARER_START));
                assert_eq!(security.starts().ps, Some(0x54321));
                Ok(())
            }),
            Some(Ok(key))
        );
        assert_eq!(procedure.phase(), umts_rrc::dedicated::SetupPhase::Complete);
        assert_eq!(
            radio.security(key).unwrap().unconfirmed_origin(),
            Some(origin)
        );
        assert!(
            !radio
                .security_delivery(key)
                .unwrap()
                .unwrap()
                .peer_progress_observed
        );
        return;
    }
    for count in 1..=4 {
        assert_eq!(radio.process_one_at(time(9)), Some(Ok(key)));
        assert_eq!(radio.core().len(), count);
    }
    assert_eq!(radio.core().len(), 4);
    uplink(
        &mut radio,
        key,
        SignallingBearer::NasAm,
        &mut nas_peer,
        source.next_chip(),
        &nas(&integrity, fresh, 5),
        cipher.as_ref(),
    );
    assert_eq!(
        radio.process_one_at(time(9)),
        Some(Err(SignallingError::Capacity))
    );
    radio
        .core()
        .publish_one(|_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    assert_eq!(radio.process_one_at(time(9)), Some(Ok(key)));
    assert_eq!(radio.core().len(), 4);
    for _ in 0..4 {
        radio
            .core()
            .publish_one(|_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
    }
    assert!(
        radio
            .publish_security_report(key, Domain::Cs, time(10))
            .unwrap()
    );
    drive(&mut link, &mut radio, 10);
    assert!(matches!(core.last_rua(), rua::Message::Direct { .. }));
    let connection = radio.connection(key, CnDomain::Cs).unwrap();
    radio
        .downlink_nas(connection, DownlinkPriority::Sms, &[9, 1])
        .unwrap();
    let mut protected_nas = false;
    for _ in 0..8 {
        for bytes in fach(
            &mut radio,
            &mut source,
            key,
            SignallingBearer::NasAm,
            &mut nas_peer,
            cipher.as_ref(),
        ) {
            let decoded = decode_downlink_dedicated(&bytes, bytes.len() * BITS_PER_BYTE).unwrap();
            assert!(decoded.value.integrity_check_info.is_some());
            protected_nas = true;
        }
        if protected_nas {
            break;
        }
    }
    assert!(protected_nas);
    for _ in 0..8 {
        fach(
            &mut radio,
            &mut source,
            key,
            SignallingBearer::ControlAm,
            &mut control_peer,
            cipher.as_ref(),
        );
        if radio
            .security_delivery(key)
            .unwrap()
            .unwrap()
            .completion_status_queued
        {
            break;
        }
    }
    assert!(
        radio
            .security_delivery(key)
            .unwrap()
            .unwrap()
            .completion_status_queued
    );
    assert_eq!(
        radio.publish_security_command(key, Domain::Cs, time(11)),
        Err(SignallingError::Phase)
    );
    if encryption == Encryption::NoAction {
        let (mut preparation, _pump) = verify_driver_and_peer_progress(
            &mut radio,
            source,
            key,
            &mut nas_peer,
            &integrity,
            fresh,
        );
        let mut initial = INITIAL;
        initial[1] |= 0x80;
        let body = umts_rrc_codec::decode_uplink_dedicated(&initial, initial.len() * BITS_PER_BYTE)
            .unwrap()
            .value
            .message;
        uplink(
            &mut radio,
            key,
            SignallingBearer::NasAm,
            &mut nas_peer,
            preparation.source().next_chip(),
            &protected(&integrity, fresh, 3, 8, body),
            None,
        );
        assert_eq!(radio.process_one_at(time(12)), Some(Ok(key)));
        assert!(radio.connection(key, CnDomain::Ps).is_some());
        for _ in 0..4 {
            drive(&mut link, &mut radio, 20);
        }
        let command = ranap::SecurityCommand {
            integrity: ranap::Protection {
                algorithms: vec![ranap::Algorithm(0)],
                key: ranap::Key::new([0x33; 16]),
            },
            encryption: None,
            key_status: ranap::KeyStatus::New,
        };
        core.rua(rua::Message::Direct {
            domain: Domain::Ps,
            context: context_id,
            ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
        });
        drive(&mut link, &mut radio, 20);
        radio
            .prepare_security(
                key,
                Domain::Ps,
                time(21),
                RadioPlan {
                    transaction: TransactionId::new(3).unwrap(),
                    layout: BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
                    integrity: IntegrityChange::Modify(
                        umts_rrc::security::IntegrityActivation::new(SignallingSet::Base, &[0; 4])
                            .unwrap(),
                    ),
                    cipher: None,
                    dpch_cfn: None,
                },
            )
            .unwrap();
        radio
            .publish_security_command(key, Domain::Ps, time(22))
            .unwrap();
        assert_eq!(
            radio.security(key).unwrap().active_origin(Domain::Cs),
            Some(origin)
        );
        assert_eq!(
            radio.security_delivery(key).unwrap().unwrap().origin.domain,
            CnDomain::Ps
        );
        assert_eq!(
            radio.security_delivery(key).unwrap().unwrap().command,
            SduTransmission::Queued
        );
        assert!(preparation.source_mut().next_chip().index > 0);
    }
}

fn verify_driver_and_peer_progress(
    radio: &mut CommonSignalling,
    source: CommonTx,
    key: UeKey,
    nas_peer: &mut AmEntity,
    integrity: &IntegrityContext,
    fresh: u32,
) -> (crate::radio_io::TxPreparation, crate::radio_io::TxPump) {
    use crate::radio_io::{PumpProgress, prepare_tx};
    use sdr_hal::{Radio, memory::MemoryRadio};
    let old = radio.security_delivery(key).unwrap().unwrap();
    assert_eq!(old.command, SduTransmission::Acknowledged);
    assert!(!old.completion_status_submitted);
    uplink(
        radio,
        key,
        SignallingBearer::NasAm,
        nas_peer,
        source.next_chip(),
        &nas(integrity, fresh, 6),
        None,
    );
    let (_, rx_size) = crate::common_am::CommonAmBearer::sizes(&source, 1).unwrap();
    let (mut memory, peer) = MemoryRadio::new(1);
    memory.set_tx_sample_rate(7_680_000.0).unwrap();
    let (tx, _) = Box::new(memory).split().unwrap();
    let (mut preparation, mut pump) = prepare_tx(
        source,
        tx,
        crate::radio_io::TxSettings::offline(1000, count(1), count(1), count(2)),
    )
    .unwrap();
    let mut duplicate = vec![0; rx_size.get()];
    const DATA_AND_POLL: u16 = 0x8004;
    const SN_MASK: u64 = 4095;
    let header = DATA_AND_POLL | ((old.completion.unwrap().last_position() & SN_MASK) as u16) << 3;
    duplicate[..2].copy_from_slice(&header.to_be_bytes());
    radio
        .receive(
            key,
            preparation.source().next_chip(),
            umts_mac::CommonPdu {
                header: CommonHeader::Dedicated {
                    identity: CommonIdentity::Cell(radio.assigned(key).unwrap().crnti()),
                    channel: LogicalChannelId::new(2).unwrap(),
                },
                payload: Bits::new(&duplicate, duplicate.len() * BITS_PER_BYTE).unwrap(),
            },
        )
        .unwrap();
    pump.start().unwrap();
    for _ in 0..512 {
        let now = preparation.source().next_chip();
        if now.index.is_multiple_of(CHIPS_PER_FRAME) {
            radio
                .publish_bearer(
                    key,
                    SignallingBearer::ControlAm,
                    preparation.source_mut(),
                    now,
                    now,
                )
                .unwrap();
        }
        preparation.prepare_one().unwrap().unwrap();
        assert!(matches!(pump.cycle().unwrap(), PumpProgress::Submitted(_)));
        peer.output.recv().unwrap();
        if radio
            .security_delivery(key)
            .unwrap()
            .unwrap()
            .completion_status_submitted
        {
            break;
        }
    }
    assert!(
        radio
            .security_delivery(key)
            .unwrap()
            .unwrap()
            .completion_status_submitted
    );
    assert_eq!(radio.process_one_at(time(12)), Some(Ok(key)));
    assert!(
        !radio
            .security_delivery(key)
            .unwrap()
            .unwrap()
            .peer_progress_observed
    );
    uplink(
        radio,
        key,
        SignallingBearer::NasAm,
        nas_peer,
        preparation.source().next_chip(),
        &nas(integrity, fresh, 7),
        None,
    );
    assert_eq!(radio.process_one_at(time(12)), Some(Ok(key)));
    let confirmed = radio.security_delivery(key).unwrap().unwrap();
    assert!(confirmed.peer_progress_observed);
    let mut retired = false;
    for _ in 0..16 {
        retired |= radio.service_security_one(time(12)).unwrap()
            == SecurityWork::DeliveryRetired {
                key,
                origin: confirmed.origin,
            };
    }
    assert!(retired);
    assert_eq!(radio.security_delivery(key).unwrap(), None);
    (preparation, pump)
}

#[test]
fn a_cipher_boundary_inside_the_command_cannot_publish_a_falsely_protected_command() {
    let mut source = source(false);
    let mut radio = controller(&source);
    let (mut link, core) = ready(&mut radio);
    let (key, context_id, _) = register(&mut link, &core, &mut radio, &source, 0, 3);
    let mut control_peer = radio_peer(&source);
    control_peer.enqueue(&COMPLETE).unwrap();
    control_peer
        .publish_next(at(0), |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    radio
        .request_capabilities(key, at(0), TransactionId::new(2).unwrap())
        .unwrap();
    send_capability_report(&mut radio, key, &mut control_peer);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    for _ in 0..4 {
        fach(
            &mut radio,
            &mut source,
            key,
            SignallingBearer::ControlAm,
            &mut control_peer,
            None,
        );
    }
    let command = ranap::SecurityCommand {
        integrity: ranap::Protection {
            algorithms: vec![ranap::Algorithm(0)],
            key: ranap::Key::new([0x11; 16]),
        },
        encryption: Some(ranap::Protection {
            algorithms: vec![ranap::Algorithm(1)],
            key: ranap::Key::new([0x22; 16]),
        }),
        key_status: ranap::KeyStatus::New,
    };
    core.rua(rua::Message::Direct {
        domain: Domain::Cs,
        context: context_id,
        ranap: ranap::encode_security(&command, Limits::default()).unwrap(),
    });
    drive(&mut link, &mut radio, 6);
    let fresh = radio.security(key).unwrap().fresh();
    let layout = BearerLayout::new(SignallingSet::Base, &[]).unwrap();
    radio
        .prepare_security(
            key,
            Domain::Cs,
            time(7),
            RadioPlan {
                transaction: TransactionId::new(2).unwrap(),
                layout,
                integrity: IntegrityChange::Start { fresh },
                cipher: Some(
                    ActivationSequences::new(
                        layout,
                        &[(1, 0), (2, control_peer.receive_sequence()), (3, 0)],
                    )
                    .unwrap(),
                ),
                dpch_cfn: None,
            },
        )
        .unwrap();
    assert_eq!(
        radio.publish_security_command(key, Domain::Cs, time(8)),
        Err(SignallingError::ConfigurationFailure)
    );
    assert_eq!(
        radio.security(key).unwrap().phase(Domain::Cs),
        Phase::Prepared
    );
    assert_eq!(radio.security_delivery(key).unwrap(), None);
}
