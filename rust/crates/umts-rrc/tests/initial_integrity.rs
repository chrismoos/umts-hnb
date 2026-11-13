// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::types::{BitString, FixedBitString};
use std::{
    num::NonZeroU64,
    time::{Duration, Instant},
};
use umts_rrc::{
    establishment::TransactionId,
    integrity::{
        Error as IntegrityError, IntegritySchedule, ScheduleError, ScheduleFailure, StreamError,
    },
    security::{
        BearerLayout, Capabilities, Command, CommandParameters, ExchangeError, IntegrityChange,
        SecurityExchange, SignallingSet,
    },
};
use umts_rrc_schema::{
    class_definitions::{ULDCCHMessage, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_security::{
    Key,
    contexts::{CipherMaterial, IntegrityContext, KeyOrigin, KeySet},
    counters::CounterInitialization,
    negotiation::{self, KeyStatus, Support},
};
use umts_types::{CnDomain, Direction, RadioEpoch, UeKey};

const FRESH: u32 = 0x12345678;
const MAC_BITS: usize = 32;
const SEQUENCE_MASK: u32 = 15;
const CONTROL_SRB: u8 = 2;
const NAS_SRB: u8 = 3;
const BASE_SIGNALLING_COUNT: usize = 4;
const FIRST_UPLINK_COUNT: u32 = 1;
const TRANSACTION: u8 = 2;

fn fixed(value: u32) -> FixedBitString<32> {
    let mut bits = FixedBitString::default();
    for index in 0..MAC_BITS {
        bits.set(index, value & (1 << (MAC_BITS - 1 - index)) != 0);
    }
    bits
}

fn protected(
    context: &IntegrityContext,
    bearer: u8,
    count: u32,
    body: ULDCCHMessageType,
) -> Vec<u8> {
    let mut message = ULDCCHMessage::new(
        Some(ie::IntegrityCheckInfo::new(
            ie::MessageAuthenticationCode(fixed(u32::from(bearer))),
            ie::RRCMessageSequenceNumber(0),
        )),
        body,
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
        ie::RRCMessageSequenceNumber((count & SEQUENCE_MASK) as u8),
    ));
    rasn::uper::encode(&message).unwrap()
}

#[test]
fn actual_production_schedule_enforces_initial_boundaries_mac_and_first_message() {
    for sequence in 0..=SEQUENCE_MASK as u8 {
        let key = UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 0,
            generation: NonZeroU64::MIN,
        };
        let context = KeySet::uia1(
            KeyOrigin {
                ue: key,
                domain: CnDomain::Cs,
                generation: NonZeroU64::MIN,
            },
            CipherMaterial::Clear,
            Key::new([1; 16]),
        )
        .split()
        .1;
        let mut cipher = BitString::repeat(false, 16);
        cipher.set(15, true);
        let mut integrity = BitString::repeat(false, 16);
        integrity.set(14, true);
        let caps = Capabilities::new(ie::SecurityCapability::new(cipher, integrity), None).unwrap();
        let selection = negotiation::select(
            negotiation::Request {
                integrity: &[0],
                cipher: None,
                keys: KeyStatus::Old,
            },
            Some(caps.support()),
            Support::KASUMI,
            negotiation::Constraints::default(),
        )
        .unwrap();
        let command = Command::new(
            CommandParameters {
                origin: context.origin(),
                transaction: TransactionId::new(TRANSACTION).unwrap(),
                layout: BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
                selection,
                integrity: IntegrityChange::Start { fresh: FRESH },
                cipher: None,
                dpch_cfn: None,
            },
            &caps,
        )
        .unwrap();
        let now = Instant::now();
        let mut exchange = SecurityExchange::prepare(command, now, Duration::from_secs(5)).unwrap();
        let mut schedule = IntegritySchedule::new(key, FRESH, SignallingSet::Base);
        exchange
            .publish_scheduled_with(
                now,
                context.clone(),
                CounterInitialization::NewKeys,
                &mut schedule,
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        let bytes = protected(
            &context,
            CONTROL_SRB,
            FIRST_UPLINK_COUNT,
            ULDCCHMessageType::securityModeComplete(pdu::SecurityModeComplete::new(
                ie::RRCTransactionIdentifier(TRANSACTION),
                Some(ie::IntegrityProtActivationInfo::new(
                    ie::RRCMessageSequenceNumberList(vec![
                        ie::RRCMessageSequenceNumber(sequence);
                        BASE_SIGNALLING_COUNT
                    ]),
                )),
                None,
                None,
            )),
        );
        let mut corrupt = bytes.clone();
        corrupt[1] ^= 1;
        assert!(
            schedule
                .receive_with(
                    key,
                    CONTROL_SRB,
                    &corrupt,
                    &mut vec![0; corrupt.len()],
                    |admission, accepted| exchange
                        .complete_with(now, accepted, |completion| admission.complete(*completion))
                )
                .is_err_and(|error| matches!(
                    error,
                    ScheduleError::Stream(StreamError::Protection(IntegrityError::Authentication))
                ))
        );
        assert_eq!(schedule.pending_origin(), Some(context.origin()));
        let result = schedule.receive_with(
            key,
            CONTROL_SRB,
            &bytes,
            &mut vec![0; bytes.len()],
            |admission, accepted| {
                exchange.complete_with(now, accepted, |completion| admission.complete(*completion))
            },
        );
        if u32::from(sequence) > FIRST_UPLINK_COUNT {
            assert!(
                matches!(
                    result,
                    Err(ScheduleError::Downstream(ExchangeError::Consumer(
                        ScheduleFailure::Activation
                    )))
                ),
                "activation {sequence} must not skip first SN1"
            );
            assert_eq!(schedule.pending_origin(), Some(context.origin()));
            continue;
        }
        result.unwrap();
        let body = ULDCCHMessageType::uplinkDirectTransfer(pdu::UplinkDirectTransfer::new(
            ie::CNDomainIdentity::cs_domain,
            ie::NASMessage(vec![5, 0x24].into()),
            None,
            None,
        ));
        let bytes = protected(&context, NAS_SRB, FIRST_UPLINK_COUNT, body);
        schedule
            .receive_with(
                key,
                NAS_SRB,
                &bytes,
                &mut vec![0; bytes.len()],
                |_, accepted| {
                    assert_eq!(accepted.count(), FIRST_UPLINK_COUNT);
                    assert_eq!(accepted.origin(), context.origin());
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
    }
}
