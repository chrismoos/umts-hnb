// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

const IMSI_15: [u8; 11] = [0x05, 0x19, 8, 0x09, 0x10, 0x10, 0, 0, 0, 0, 0x99];
const IMSI_14: [u8; 11] = [0x05, 0x19, 8, 0x01, 0x10, 0x10, 0, 0, 0, 0x90, 0xf9];
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
fn procedure(domain: CnDomain) -> Identification {
    Identification::new(key(), domain, at(0)).unwrap()
}
fn begin(procedure: &mut Identification) {
    let domain = procedure.domain();
    procedure
        .begin_with(at(0), |message| {
            assert_eq!(message, &request(domain));
            Ok::<_, ()>(())
        })
        .unwrap();
}
fn ps_response() -> [u8; 11] {
    let mut bytes = IMSI_15;
    bytes[0] = GMM;
    bytes[1] = GMM_RESPONSE;
    bytes
}

#[test]
fn independent_identity_wire_vectors_preserve_all_digits_and_mm_send_sequence() {
    assert_eq!(request(CnDomain::Cs), [0x05, 0x18, 0x01]);
    assert_eq!(request(CnDomain::Ps), [0x08, 0x15, 0x01]);
    for (bytes, expected) in [(IMSI_15, "001010000000099"), (IMSI_14, "00101000000099")] {
        for sequence in 0..4 {
            let mut value = bytes;
            value[1] |= sequence << SEND_SEQUENCE_SHIFT;
            assert_eq!(
                decode_response(CnDomain::Cs, &value).unwrap(),
                Some(Response {
                    identity: Identity::Imsi(Imsi::from_digits(expected).unwrap()),
                    send_sequence: Some(sequence)
                })
            );
        }
    }
    let response = decode_response(CnDomain::Ps, &ps_response())
        .unwrap()
        .unwrap();
    assert_eq!(
        response.identity,
        Identity::Imsi(Imsi::from_digits("001010000000099").unwrap())
    );
    assert_eq!(response.send_sequence, None);
}

#[test]
fn no_identity_is_domain_specific_and_cannot_be_filler_or_a_temporary_id() {
    for (domain, bytes) in [
        (CnDomain::Cs, &[5, 0x19, 1, 0][..]),
        (CnDomain::Ps, &[8, 0x16, 3, 0, 0, 0][..]),
    ] {
        assert_eq!(
            decode_response(domain, bytes).unwrap().unwrap().identity,
            Identity::Unavailable
        );
    }
    for (domain, bytes) in [
        (CnDomain::Cs, &[5, 0x19, 3, 0, 0, 0][..]),
        (CnDomain::Ps, &[8, 0x16, 1, 0][..]),
        (CnDomain::Cs, &[5, 0x19, 1, 0xf0][..]),
        (CnDomain::Cs, &[5, 0x19, 5, 0xf4, 1, 2, 3, 4][..]),
    ] {
        assert_eq!(decode_response(domain, bytes), Err(Error::IdentityType));
    }
}

#[test]
fn truncated_trailing_non_decimal_and_inconsistent_odd_even_encodings_fail() {
    for prefix in 0..IMSI_15.len() {
        assert!(decode_response(CnDomain::Cs, &IMSI_15[..prefix]).is_err());
    }
    let mut trailing = IMSI_15.to_vec();
    trailing.push(0);
    assert_eq!(decode_response(CnDomain::Cs, &trailing), Err(Error::Length));
    for (index, value) in [(0, 0x15), (3, 0x01), (4, 0xfa), (10, 0xf9)] {
        let mut bytes = IMSI_15;
        bytes[index] = value;
        assert!(decode_response(CnDomain::Cs, &bytes).is_err());
    }
    assert_eq!(
        decode_response(CnDomain::Cs, &[5, 0x31, 0x11]).unwrap(),
        None
    );
    assert_eq!(decode_response(CnDomain::Ps, &IMSI_15).unwrap(), None);
}

#[test]
fn cs_t3270_has_twelve_seconds_and_does_not_borrow_gmm_retries() {
    let mut procedure = procedure(CnDomain::Cs);
    begin(&mut procedure);
    assert_eq!(
        procedure.phase(),
        Phase::Awaiting {
            deadline: at(12 * CHIPS_PER_SECOND),
            transmissions: 1
        }
    );
    assert_eq!(
        procedure
            .poll_with::<()>(at(12 * CHIPS_PER_SECOND - 1), |_| panic!("premature retry"))
            .unwrap(),
        Poll::Waiting
    );
    assert_eq!(
        procedure
            .poll_with::<()>(at(12 * CHIPS_PER_SECOND), |_| panic!("CS retransmission"))
            .unwrap(),
        Poll::Expired
    );
    assert_eq!(procedure.phase(), Phase::Aborted(AbortReason::Timeout));
}

#[test]
fn ps_t3370_has_six_seconds_and_exactly_four_retransmissions() {
    let mut procedure = procedure(CnDomain::Ps);
    begin(&mut procedure);
    for transmission in 2..=5 {
        let Phase::Awaiting { deadline, .. } = procedure.phase() else {
            panic!("awaiting response");
        };
        assert_eq!(
            procedure
                .poll_with(deadline, |message| {
                    assert_eq!(message, &[8, 0x15, 1]);
                    Ok::<_, ()>(())
                })
                .unwrap(),
            Poll::Retransmitted
        );
        assert_eq!(
            procedure.phase(),
            Phase::Awaiting {
                deadline: at(u64::from(transmission) * 6 * CHIPS_PER_SECOND),
                transmissions: transmission
            }
        );
    }
    assert_eq!(
        procedure
            .poll_with::<()>(at(30 * CHIPS_PER_SECOND), |_| panic!("sixth transmission"))
            .unwrap(),
        Poll::Expired
    );
}

#[test]
fn failed_queue_admission_does_not_arm_or_advance_an_identity_request() {
    let mut procedure = procedure(CnDomain::Ps);
    assert_eq!(
        procedure.begin_with(at(0), |_| Err::<(), _>(7u8)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(procedure.phase(), Phase::Idle);
    begin(&mut procedure);
    let deadline = at(6 * CHIPS_PER_SECOND);
    assert_eq!(
        procedure.poll_with(deadline, |_| Err(7u8)),
        Err(PublishError::Downstream(7))
    );
    assert_eq!(
        procedure.phase(),
        Phase::Awaiting {
            deadline,
            transmissions: 1
        }
    );
    assert_eq!(
        procedure
            .poll_with(at(deadline.index + 99), |_| Ok::<_, ()>(()))
            .unwrap(),
        Poll::Retransmitted
    );
    assert_eq!(
        procedure.phase(),
        Phase::Awaiting {
            deadline: at(12 * CHIPS_PER_SECOND + 99),
            transmissions: 2
        }
    );
}

#[test]
fn receiving_identity_stops_timer_before_registration_publication() {
    let mut procedure = procedure(CnDomain::Cs);
    begin(&mut procedure);
    assert_eq!(
        procedure
            .observe_response(key(), CnDomain::Cs, at(1), &IMSI_15)
            .unwrap(),
        Observation::Ready
    );
    let before = procedure.phase();
    assert_eq!(
        procedure.publish_with(|_| Err::<(), _>(9u8)),
        Err(PublishError::Downstream(9))
    );
    assert_eq!(procedure.phase(), before);
    assert_eq!(
        procedure
            .poll_with::<()>(at(100 * CHIPS_PER_SECOND), |_| panic!(
                "timer must be stopped"
            ))
            .unwrap(),
        Poll::Inactive
    );
    let response = procedure.publish_with(Ok::<_, ()>).unwrap();
    assert_eq!(
        response.identity,
        Identity::Imsi(Imsi::from_digits("001010000000099").unwrap())
    );
    assert_eq!(procedure.phase(), Phase::Complete(response));
}

#[test]
fn unavailable_identity_stops_timer_and_remains_an_explicit_result() {
    let mut procedure = procedure(CnDomain::Ps);
    begin(&mut procedure);
    assert_eq!(
        procedure
            .observe_response(key(), CnDomain::Ps, at(1), &[8, 0x16, 3, 0, 0, 0])
            .unwrap(),
        Observation::Ready
    );
    assert_eq!(
        procedure
            .poll_with::<()>(at(60 * CHIPS_PER_SECOND), |_| panic!(
                "unavailable is a response"
            ))
            .unwrap(),
        Poll::Inactive
    );
    assert_eq!(
        procedure.publish_with(Ok::<_, ()>).unwrap().identity,
        Identity::Unavailable
    );
}

#[test]
fn peer_domain_old_generation_and_unrelated_nas_do_not_complete_identification() {
    let mut procedure = procedure(CnDomain::Cs);
    begin(&mut procedure);
    let before = procedure.phase();
    assert_eq!(
        procedure
            .observe_response(key(), CnDomain::Ps, at(1), &ps_response())
            .unwrap(),
        Observation::Unrelated
    );
    assert_eq!(
        procedure
            .observe_response(key(), CnDomain::Cs, at(1), &[5, 0x31, 0x11])
            .unwrap(),
        Observation::Unrelated
    );
    assert_eq!(
        procedure.observe_response(
            UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..key()
            },
            CnDomain::Cs,
            at(1),
            &IMSI_15
        ),
        Err(Error::Origin)
    );
    assert_eq!(procedure.phase(), before);
}

#[test]
fn duplicate_response_can_change_nsd_but_cannot_replace_the_identity() {
    let mut procedure = procedure(CnDomain::Cs);
    begin(&mut procedure);
    procedure
        .observe_response(key(), CnDomain::Cs, at(1), &IMSI_15)
        .unwrap();
    let before = procedure.phase();
    let mut duplicate = IMSI_15;
    duplicate[1] |= 0x40;
    assert_eq!(
        procedure
            .observe_response(key(), CnDomain::Cs, at(2), &duplicate)
            .unwrap(),
        Observation::Duplicate
    );
    let mut conflict = IMSI_15;
    conflict[10] = 0x98;
    assert_eq!(
        procedure.observe_response(key(), CnDomain::Cs, at(3), &conflict),
        Err(Error::ConflictingResponse)
    );
    assert_eq!(procedure.phase(), before);
}

#[test]
fn epoch_clock_overflow_and_radio_failure_are_explicit_terminal_inputs() {
    let mut procedure = procedure(CnDomain::Cs);
    begin(&mut procedure);
    assert_eq!(
        procedure.poll_with(
            ChipInstant {
                epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
                index: 1
            },
            |_| Ok::<_, ()>(())
        ),
        Err(PublishError::Procedure(Error::Time))
    );
    procedure
        .abort(key(), at(1), AbortReason::RadioFailure)
        .unwrap();
    assert_eq!(
        procedure
            .observe_response(key(), CnDomain::Cs, at(2), &IMSI_15)
            .unwrap(),
        Observation::Unrelated
    );
    assert!(matches!(
        procedure.publish_with(Ok::<_, ()>),
        Err(PublishError::Procedure(Error::Phase))
    ));
    let mut overflow = Identification::new(key(), CnDomain::Ps, at(u64::MAX)).unwrap();
    assert_eq!(
        overflow.begin_with(at(u64::MAX), |_| Ok::<_, ()>(())),
        Err(PublishError::Procedure(Error::Time))
    );
    assert_eq!(overflow.phase(), Phase::Idle);
}

#[test]
fn rrc_imsi_extraction_does_not_treat_equipment_or_temporary_identity_as_imsi() {
    use umts_rrc_schema::information_elements as ie;
    let digits = "001010000000099"
        .bytes()
        .map(|byte| ie::Digit(byte - b'0'))
        .collect();
    assert_eq!(
        rrc_imsi(&ie::InitialUEIdentity::imsi(ie::IMSIGSMMAP(digits))).unwrap(),
        Some(Imsi::from_digits("001010000000099").unwrap())
    );
    assert!(
        rrc_imsi(&ie::InitialUEIdentity::imsi(ie::IMSIGSMMAP(vec![
            ie::Digit(
                0
            );
            21
        ])))
        .is_err()
    );
    assert!(
        rrc_imsi(&ie::InitialUEIdentity::imsi(ie::IMSIGSMMAP(vec![
            ie::Digit(
                0
            );
            5
        ])))
        .is_err()
    );
    assert_eq!(
        rrc_imsi(&ie::InitialUEIdentity::imei(ie::IMEI(vec![
            ie::IMEIDigit(0);
            15
        ])))
        .unwrap(),
        None
    );
}

#[test]
fn identity_request_can_be_encoded_on_rrc_before_a_core_context_exists() {
    use crate::{establishment::TransactionId, nas::build_downlink_nas};
    use umts_rrc_codec::decode_downlink_dedicated;
    use umts_rrc_schema::{
        class_definitions::DLDCCHMessageType, pdu_definitions::DownlinkDirectTransfer,
    };
    for domain in [CnDomain::Cs, CnDomain::Ps] {
        let message =
            build_downlink_nas(TransactionId::new(0).unwrap(), domain, &request(domain)).unwrap();
        let decoded = decode_downlink_dedicated(&message.bytes, message.bit_length).unwrap();
        let DLDCCHMessageType::downlinkDirectTransfer(DownlinkDirectTransfer::r3(body)) =
            decoded.value.message
        else {
            panic!("downlink direct transfer");
        };
        assert_eq!(
            body.downlink_direct_transfer_r3.nas_message.0.as_ref(),
            &request(domain)
        );
    }
}
