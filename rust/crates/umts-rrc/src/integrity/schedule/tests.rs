// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
mod common_recovery;
mod completion_recovery;
use crate::{
    establishment::TransactionId,
    security::{
        BearerLayout, Capabilities, Command, CommandParameters, IntegrityActivation,
        SecurityExchange,
    },
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
    contexts::{CipherMaterial, KeySet},
    counters::Start,
    negotiation::{self, KeyStatus, Support},
};
use umts_types::{Direction, RadioEpoch};

#[test]
fn repeated_new_key_commands_collect_only_unreferenced_epochs_and_preserve_wire_sequence_wrap() {
    let base = Instant::now();
    let first = context(1);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    let mut previous_control = 1_u8;
    let mut previous_nas = 0_u8;
    for generation in 2..=24 {
        let current = context(generation);
        let nas_sequence = (previous_nas + 1) & 15;
        let change = IntegrityChange::Modify(
            IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap(),
        );
        let mut exchange = exchange(&current, base, change);
        let next_dl = schedule.next_downlink_sequence(key(), 2).unwrap();
        let previous_dl = next_dl.wrapping_sub(1) & 15;
        let command_count = u32::from(next_dl) + if next_dl < previous_dl { 16 } else { 0 };
        exchange
            .publish_scheduled_with(
                base,
                current.clone(),
                CounterInitialization::NewKeys,
                &mut schedule,
                |encoded| {
                    assert_downlink(&current, 2, command_count, encoded);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        assert_eq!(schedule.collect_unused(key()), Err(ScheduleFailure::Phase));
        let next_control = (previous_control + 1) & 15;
        let complete_count = u32::from(next_control)
            + if next_control < previous_control {
                16
            } else {
                0
            };
        finish(
            &mut schedule,
            &mut exchange,
            &current,
            base,
            complete_count,
            [(generation as u8) & 15, 0, 0, nas_sequence],
        );
        let nas_count = u32::from(nas_sequence) + if nas_sequence < previous_nas { 16 } else { 0 };
        let bytes = ordinary_bytes(&current, nas_count);
        schedule
            .receive_with(
                key(),
                3,
                &bytes,
                &mut vec![0; bytes.len()],
                |_, accepted| {
                    assert_eq!(accepted.origin(), current.origin());
                    assert_eq!(accepted.count(), nas_count);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
        let before = schedule.configurations();
        let removed = schedule.collect_unused(key()).unwrap();
        assert_eq!(schedule.configurations() + removed, before);
        assert!(schedule.configurations() <= 2);
        assert_eq!(
            schedule.downlink_origin(key(), 2).unwrap(),
            Some(current.origin())
        );
        previous_control = next_control;
        previous_nas = nas_sequence;
    }
}

#[test]
fn unused_collection_rejects_wrong_owners_without_pruning_live_or_pending_timelines() {
    let first = context(1);
    let base = Instant::now();
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    let foreign = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..key()
    };
    assert_eq!(
        schedule.collect_unused(foreign),
        Err(ScheduleFailure::Owner)
    );
    assert_eq!(schedule.collect_unused(key()), Ok(0));
    assert_eq!(schedule.configurations(), 1);
    schedule
        .publish_with(key(), 3, &message(), |encoded| {
            assert_downlink(&first, 3, 0, encoded);
            Ok::<_, ()>(())
        })
        .unwrap();
}

const FRESH: u32 = 0x12345678;

#[test]
fn activation_preview_preserves_pending_boundary_instead_of_replacing_it_with_the_cursor() {
    let base = Instant::now();
    let first = context(1);
    let second = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    for count in 0..5 {
        schedule
            .publish_with(key(), 3, &message(), |encoded| {
                assert_downlink(&first, 3, count, encoded);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    let change = IntegrityChange::Modify(
        IntegrityActivation::new(SignallingSet::Base, &[0, 0, 0, 9]).unwrap(),
    );
    let mut exchange = exchange(&second, base, change);
    exchange
        .publish_scheduled_with(
            base,
            second.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(schedule.next_downlink_sequence(key(), 3), Ok(5));
    assert_eq!(schedule.next_downlink_activation(key(), 3), Ok(9));
    assert_eq!(schedule.next_downlink_activation(key(), 3), Ok(9));
    assert_eq!(schedule.next_downlink_sequence(key(), 3), Ok(5));
}

fn key() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 3,
        generation: NonZeroU64::MIN,
    }
}
fn context(generation: u64) -> IntegrityContext {
    KeySet::uia1(
        KeyOrigin {
            ue: key(),
            domain: CnDomain::Ps,
            generation: NonZeroU64::new(generation).unwrap(),
        },
        CipherMaterial::Clear,
        Key::new([generation as u8; 16]),
    )
    .split()
    .1
}
fn capabilities() -> Capabilities {
    let mut cipher = BitString::repeat(false, 16);
    cipher.set(15, true);
    let mut integrity = BitString::repeat(false, 16);
    integrity.set(14, true);
    Capabilities::new(ie::SecurityCapability::new(cipher, integrity), None).unwrap()
}
fn exchange(
    context: &IntegrityContext,
    base: Instant,
    change: IntegrityChange,
) -> SecurityExchange {
    let selection = negotiation::select(
        negotiation::Request {
            integrity: &[0],
            cipher: None,
            keys: KeyStatus::Old,
        },
        Some(capabilities().support()),
        Support::KASUMI,
        negotiation::Constraints::default(),
    )
    .unwrap();
    let command = Command::new(
        CommandParameters {
            origin: context.origin(),
            transaction: TransactionId::new(2).unwrap(),
            layout: BearerLayout::new(SignallingSet::Base, &[]).unwrap(),
            selection,
            integrity: change,
            cipher: None,
            dpch_cfn: None,
        },
        &capabilities(),
    )
    .unwrap();
    SecurityExchange::prepare(command, base, Duration::from_secs(5)).unwrap()
}
fn message() -> DLDCCHMessage {
    DLDCCHMessage::new(
        None,
        DLDCCHMessageType::downlinkDirectTransfer(pdu::DownlinkDirectTransfer::r3(
            pdu::DownlinkDirectTransferR3::new(
                pdu::DownlinkDirectTransferR3IEs::new(
                    ie::RRCTransactionIdentifier(0),
                    ie::CNDomainIdentity::cs_domain,
                    ie::NASMessage(vec![5, 0x24].into()),
                ),
                None,
            ),
        )),
    )
}
fn fixed(value: u32) -> FixedBitString<32> {
    let mut bits = FixedBitString::default();
    for index in 0..32 {
        bits.set(index, value & (1 << (31 - index)) != 0);
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
        ie::RRCMessageSequenceNumber((count & 15) as u8),
    ));
    rasn::uper::encode(&message).unwrap()
}
fn complete_bytes(context: &IntegrityContext, count: u32, sequences: [u8; 4]) -> Vec<u8> {
    protected(
        context,
        2,
        count,
        ULDCCHMessageType::securityModeComplete(pdu::SecurityModeComplete::new(
            ie::RRCTransactionIdentifier(2),
            Some(ie::IntegrityProtActivationInfo::new(
                ie::RRCMessageSequenceNumberList(
                    sequences
                        .into_iter()
                        .map(ie::RRCMessageSequenceNumber)
                        .collect(),
                ),
            )),
            None,
            None,
        )),
    )
}
fn ordinary_bytes(context: &IntegrityContext, count: u32) -> Vec<u8> {
    protected(
        context,
        3,
        count,
        ULDCCHMessageType::uplinkDirectTransfer(pdu::UplinkDirectTransfer::new(
            ie::CNDomainIdentity::cs_domain,
            ie::NASMessage(vec![5, 0x24].into()),
            None,
            None,
        )),
    )
}
fn finish(
    schedule: &mut IntegritySchedule,
    exchange: &mut SecurityExchange,
    context: &IntegrityContext,
    base: Instant,
    count: u32,
    sequences: [u8; 4],
) {
    finish_unconfirmed(schedule, exchange, context, base, count, sequences);
    schedule
        .confirm_delivery_with(key(), context.origin(), || Ok::<_, ()>(()))
        .unwrap();
}
fn finish_unconfirmed(
    schedule: &mut IntegritySchedule,
    exchange: &mut SecurityExchange,
    context: &IntegrityContext,
    base: Instant,
    count: u32,
    sequences: [u8; 4],
) {
    let bytes = complete_bytes(context, count, sequences);
    schedule
        .receive_with(
            key(),
            2,
            &bytes,
            &mut vec![0; bytes.len()],
            |admission, accepted| {
                exchange.complete_with(base, accepted, |completion| admission.complete(*completion))
            },
        )
        .unwrap();
}
fn assert_downlink(context: &IntegrityContext, bearer: u8, count: u32, encoded: &EncodedRrc) {
    let message = decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
        .unwrap()
        .value;
    let info = message.integrity_check_info.as_ref().unwrap();
    assert_eq!(info.rrc_message_sequence_number.0, (count & 15) as u8);
    let actual = info.message_authentication_code.clone();
    let mut preimage = encoded.bytes.clone();
    super::super::put(
        &mut preimage,
        super::super::MAC_OFFSET,
        super::super::MAC_BITS,
        u32::from(bearer),
    );
    super::super::put(
        &mut preimage,
        super::super::SEQUENCE_OFFSET,
        super::super::SEQUENCE_BITS,
        0,
    );
    let expected = context
        .algorithm(context.origin())
        .unwrap()
        .authenticate(
            count,
            FRESH,
            Direction::Downlink,
            &preimage,
            preimage.len() * 8,
        )
        .unwrap();
    assert_eq!(
        actual,
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(expected)))
    );
}
fn initial(schedule: &mut IntegritySchedule, context: &IntegrityContext, base: Instant) {
    let mut exchange = exchange(context, base, IntegrityChange::Start { fresh: FRESH });
    exchange
        .publish_scheduled_with(
            base,
            context.clone(),
            CounterInitialization::NewKeys,
            schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    finish(schedule, &mut exchange, context, base, 1, [0; 4]);
}

#[test]
fn initial_activation_zero_protects_the_first_uplink_sn_one_and_uses_the_received_start() {
    let base = Instant::now();
    let context = context(1);
    let start = Start::new(0x12345).unwrap();
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    let mut exchange = exchange(&context, base, IntegrityChange::Start { fresh: FRESH });
    assert!(
        exchange
            .publish_scheduled_with(
                base,
                context.clone(),
                CounterInitialization::ReusedKeys(start),
                &mut schedule,
                |_| Err::<(), _>("full")
            )
            .is_err()
    );
    assert_eq!(schedule.configurations(), 0);
    assert_eq!(schedule.pending_origin(), None);
    assert_eq!(schedule.next_downlink_sequence(key(), 2), Ok(0));
    exchange
        .publish_scheduled_with(
            base,
            context.clone(),
            CounterInitialization::ReusedKeys(start),
            &mut schedule,
            |encoded| {
                assert_downlink(&context, 2, start.initial_count(), encoded);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert!(matches!(
        schedule.publish_with(key(), 3, &message(), |_| Ok::<_, ()>(())),
        Err(ScheduleError::Schedule(ScheduleFailure::Suspended))
    ));
    assert!(schedule.allows_unprotected_uplink(key(), 3).unwrap());
    assert!(!schedule.allows_unprotected_uplink(key(), 2).unwrap());
    finish(
        &mut schedule,
        &mut exchange,
        &context,
        base,
        start.initial_count() | 1,
        [0; 4],
    );
    let bytes = ordinary_bytes(&context, start.initial_count() | 1);
    schedule
        .receive_with(
            key(),
            3,
            &bytes,
            &mut vec![0; bytes.len()],
            |_, accepted| {
                assert_eq!(accepted.origin(), context.origin());
                assert_eq!(accepted.count(), start.initial_count() | 1);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert!(!schedule.allows_unprotected_uplink(key(), 3).unwrap());
    // Independent UIA1 vector with synthetic IK 01 repeated, FRESH 12345678 and 254 authenticated bits.
    let common = [
        0xb3, 0x33, 0x9b, 0x54, 0x88, 0x04, 0x8d, 0x15, 0x9e, 0x10, 0x91, 0xa2, 0xd5, 0x0c, 0x86,
        0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    let downlink_sequence = schedule.next_downlink_sequence(key(), 1).unwrap();
    assert!(matches!(
        schedule.receive_common_response_with(key(), &common, 254, &mut [0; 32], |admission, _| {
            admission.publish_response_with(1, &message(), |_| Err::<(), _>("full"))
        }),
        Err(ScheduleError::Downstream(ScheduleError::Downstream("full")))
    ));
    assert_eq!(
        schedule.next_downlink_sequence(key(), 1),
        Ok(downlink_sequence)
    );
    schedule
        .receive_common_response_with(key(), &common, 254, &mut [0; 32], |admission, accepted| {
            assert_eq!(accepted.count(), start.initial_count() | 1);
            admission.publish_response_with(1, &message(), |_| Ok::<_, ()>(()))
        })
        .unwrap();
    schedule
        .publish_with(key(), 3, &message(), |encoded| {
            assert_downlink(&context, 3, start.initial_count(), encoded);
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn control_rekey_keeps_old_reports_until_authenticated_completion() {
    let base = Instant::now();
    let old = context(1);
    let new = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &old, base);
    let change =
        IntegrityChange::Modify(IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap());
    let mut exchange = exchange(&new, base, change);
    exchange
        .publish_scheduled_with(
            base,
            new.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let report = || {
        ULDCCHMessageType::measurementReport(pdu::MeasurementReport::new(
            ie::MeasurementIdentity(16),
            None,
            None,
            None,
            None,
            None,
        ))
    };
    let bytes = protected(&old, 2, 2, report());
    schedule
        .receive_with(
            key(),
            2,
            &bytes,
            &mut vec![0; bytes.len()],
            |_, accepted| {
                assert_eq!(accepted.origin(), old.origin());
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    let premature = protected(&new, 2, 3, report());
    assert!(
        schedule
            .receive_with(
                key(),
                2,
                &premature,
                &mut vec![0; premature.len()],
                |_, _| Ok::<_, ()>(())
            )
            .is_err()
    );
    assert_eq!(schedule.pending_origin(), Some(new.origin()));
    finish(&mut schedule, &mut exchange, &new, base, 3, [0; 4]);
    let stale = protected(&old, 2, 4, report());
    assert!(
        schedule
            .receive_with(key(), 2, &stale, &mut vec![0; stale.len()], |_, _| Ok::<
                _,
                (),
            >(
                ()
            ))
            .is_err()
    );
    let bytes = protected(&new, 2, 4, report());
    schedule
        .receive_with(
            key(),
            2,
            &bytes,
            &mut vec![0; bytes.len()],
            |_, accepted| {
                assert_eq!(accepted.origin(), new.origin());
                Ok::<_, ()>(())
            },
        )
        .unwrap();
}

#[test]
fn late_activation_after_old_sequence_wrap_does_not_add_an_extra_new_key_hfn() {
    let base = Instant::now();
    let first = context(1);
    let second = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    for count in 0..14 {
        schedule
            .publish_with(key(), 3, &message(), |encoded| {
                assert_downlink(&first, 3, count, encoded);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    for count in 1..14 {
        let bytes = ordinary_bytes(&first, count);
        schedule
            .receive_with(
                key(),
                3,
                &bytes,
                &mut vec![0; bytes.len()],
                |_, accepted| {
                    assert_eq!(accepted.count(), count);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
    }
    let change = IntegrityChange::Modify(
        IntegrityActivation::new(SignallingSet::Base, &[0, 0, 0, 2]).unwrap(),
    );
    let mut exchange = exchange(&second, base, change);
    exchange
        .publish_scheduled_with(
            base,
            second.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |encoded| {
                assert_downlink(&second, 2, 1, encoded);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    for count in 14..18 {
        schedule
            .publish_with(key(), 3, &message(), |encoded| {
                assert_downlink(&first, 3, count, encoded);
                Ok::<_, ()>(())
            })
            .unwrap();
    }
    assert!(matches!(
        schedule.publish_with(key(), 3, &message(), |_| Ok::<_, ()>(())),
        Err(ScheduleError::Schedule(ScheduleFailure::Suspended))
    ));
    finish(
        &mut schedule,
        &mut exchange,
        &second,
        base,
        2,
        [10, 1, 0, 2],
    );
    assert_eq!(
        schedule.retire(key(), first.origin()),
        Err(ScheduleFailure::Activation)
    );
    let mut premature = ordinary_bytes(&second, 2);
    let last = premature.len() - 1;
    premature[last] ^= 1;
    assert!(
        schedule
            .receive_with(
                key(),
                3,
                &premature,
                &mut vec![0; premature.len()],
                |_, _| Ok::<_, ()>(())
            )
            .is_err()
    );
    for count in 14..18 {
        let bytes = ordinary_bytes(&first, count);
        schedule
            .receive_with(
                key(),
                3,
                &bytes,
                &mut vec![0; bytes.len()],
                |_, accepted| {
                    assert_eq!(accepted.origin(), first.origin());
                    assert_eq!(accepted.count(), count);
                    Ok::<_, ()>(())
                },
            )
            .unwrap();
    }
    schedule
        .publish_with(key(), 3, &message(), |encoded| {
            assert_downlink(&second, 3, 2, encoded);
            Ok::<_, ()>(())
        })
        .unwrap();
    let bytes = ordinary_bytes(&second, 2);
    schedule
        .receive_with(
            key(),
            3,
            &bytes,
            &mut vec![0; bytes.len()],
            |_, accepted| {
                assert_eq!(accepted.origin(), second.origin());
                assert_eq!(accepted.count(), 2);
                Ok::<_, ()>(())
            },
        )
        .unwrap();
    assert_eq!(schedule.configurations(), 2);
    schedule.retire(key(), first.origin()).unwrap();
    assert_eq!(schedule.configurations(), 1);
    schedule
        .publish_with(key(), 3, &message(), |encoded| {
            assert_downlink(&second, 3, 3, encoded);
            Ok::<_, ()>(())
        })
        .unwrap();
}

#[test]
fn completion_admission_and_counter_state_roll_back_together() {
    let base = Instant::now();
    let context = context(1);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    let mut exchange = exchange(&context, base, IntegrityChange::Start { fresh: FRESH });
    exchange
        .publish_scheduled_with(
            base,
            context.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let bytes = complete_bytes(&context, 1, [0; 4]);
    assert!(
        schedule
            .receive_with(
                key(),
                2,
                &bytes,
                &mut vec![0; bytes.len()],
                |admission, accepted| {
                    exchange.complete_with(base, accepted, |completion| {
                        admission.complete(*completion)?;
                        Err::<(), _>(ScheduleFailure::Capacity)
                    })
                }
            )
            .is_err()
    );
    assert_eq!(schedule.pending_origin(), Some(context.origin()));
    finish(&mut schedule, &mut exchange, &context, base, 1, [0; 4]);
    assert_eq!(schedule.pending_origin(), None);
    assert!(matches!(
        schedule.receive_with(key(), 2, &bytes, &mut vec![0; bytes.len()], |_, _| Ok::<
            _,
            (),
        >(
            ()
        )),
        Err(ScheduleError::Stream(StreamError::Duplicate))
    ));
}

#[test]
fn abort_restores_the_old_control_count_past_the_abandoned_command() {
    let base = Instant::now();
    let first = context(1);
    let second = context(2);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    initial(&mut schedule, &first, base);
    let change =
        IntegrityChange::Modify(IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap());
    let mut exchange = exchange(&second, base, change);
    exchange
        .publish_scheduled_with(
            base,
            second.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        schedule.abort_with(key(), second.origin(), |_| Err::<(), _>("recovery blocked")),
        Err(ScheduleError::Downstream("recovery blocked"))
    );
    assert_eq!(schedule.pending_origin(), Some(second.origin()));
    schedule
        .abort_with(key(), second.origin(), |recovery| {
            assert_eq!(recovery.previous_control_origin, Some(first.origin()));
            assert_eq!(recovery.next_control_count, Some(2));
            assert!(!recovery.release_connection);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(schedule.configurations(), 1);
    schedule
        .publish_with(key(), 2, &message(), |encoded| {
            assert_downlink(&first, 2, 2, encoded);
            Ok::<_, ()>(())
        })
        .unwrap();
    assert!(
        exchange
            .publish_scheduled_with(
                base,
                second,
                CounterInitialization::NewKeys,
                &mut schedule,
                |_| Ok::<_, ()>(())
            )
            .is_err()
    );
}

#[test]
fn capacity_and_owner_checks_fail_before_command_publication() {
    let base = Instant::now();
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    let first = context(1);
    initial(&mut schedule, &first, base);
    for generation in 2..=3 {
        let context = context(generation);
        let change = IntegrityChange::Modify(
            IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap(),
        );
        let mut exchange = exchange(&context, base, change);
        exchange
            .publish_scheduled_with(
                base,
                context.clone(),
                CounterInitialization::NewKeys,
                &mut schedule,
                |_| Ok::<_, ()>(()),
            )
            .unwrap();
        finish(
            &mut schedule,
            &mut exchange,
            &context,
            base,
            generation as u32,
            [10, 1, 0, 1],
        );
    }
    let fourth = context(4);
    let mut exchange = exchange(
        &fourth,
        base,
        IntegrityChange::Modify(IntegrityActivation::new(SignallingSet::Base, &[0; 4]).unwrap()),
    );
    assert!(matches!(
        exchange.publish_scheduled_with(
            base,
            fourth,
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| -> Result<(), ()> { panic!("over-capacity command was published") }
        ),
        Err(ScheduleError::Schedule(ScheduleFailure::Capacity))
    ));
    let mut foreign = key();
    foreign.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        schedule.next_downlink_sequence(foreign, 2),
        Err(ScheduleFailure::Owner)
    );
    assert_eq!(
        schedule.allows_unprotected_uplink(key(), 0),
        Err(ScheduleFailure::Bearer)
    );
}

#[test]
fn nas_publication_uses_the_scheduled_shared_key_without_changing_cs_routing() {
    use crate::{
        establishment::DomainStarts,
        nas::{
            CoreContextId, DownlinkPriority, NasError, NasRouter, NasScheduledError,
            SignallingBearer,
        },
    };
    let base = Instant::now();
    let context = context(1);
    let mut router = NasRouter::new(
        key(),
        CoreContextId::new(9).unwrap(),
        DomainStarts::default(),
        false,
    );
    let initial_message =
        umts_rrc_codec::decode_uplink_dedicated(&[0x14, 0x10, 0, 0, 0x08, 0x29, 0x20], 56).unwrap();
    router
        .forward_with(key(), SignallingBearer::NasAm, &initial_message, |_| {
            Ok::<_, ()>(())
        })
        .unwrap();
    let connection = router.connection(CnDomain::Cs).unwrap();
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    router
        .downlink_scheduled_with(
            connection,
            DownlinkPriority::Signalling,
            &[5, 0x24],
            &mut schedule,
            |bearer, encoded| {
                assert_eq!(bearer, SignallingBearer::NasAm);
                assert!(
                    decode_downlink_dedicated(&encoded.bytes, encoded.bit_length)
                        .unwrap()
                        .value
                        .integrity_check_info
                        .is_none()
                );
                Ok::<_, &str>(())
            },
        )
        .unwrap();
    router.require_security();
    assert_eq!(
        router.downlink_scheduled_with(
            connection,
            DownlinkPriority::Signalling,
            &[5, 0x24],
            &mut schedule,
            |_, _| Ok::<_, &str>(())
        ),
        Err(NasScheduledError::Nas(NasError::SecurityRequired))
    );
    initial(&mut schedule, &context, base);
    assert_eq!(
        router.downlink_scheduled_with(
            connection,
            DownlinkPriority::Signalling,
            &[5, 0x24],
            &mut schedule,
            |_, _| Err::<(), _>("full")
        ),
        Err(NasScheduledError::Schedule(ScheduleError::Downstream(
            "full"
        )))
    );
    assert_eq!(schedule.next_downlink_sequence(key(), 3), Ok(0));
    router
        .downlink_scheduled_with(
            connection,
            DownlinkPriority::Sms,
            &[5, 0x24],
            &mut schedule,
            |bearer, encoded| {
                assert_eq!(bearer, SignallingBearer::NasAm);
                assert_downlink(&context, 3, 0, encoded);
                let decoded =
                    decode_downlink_dedicated(&encoded.bytes, encoded.bit_length).unwrap();
                let DLDCCHMessageType::downlinkDirectTransfer(pdu::DownlinkDirectTransfer::r3(
                    body,
                )) = &decoded.value.message
                else {
                    panic!("expected NAS transfer");
                };
                assert_eq!(
                    body.downlink_direct_transfer_r3.cn_domain_identity,
                    ie::CNDomainIdentity::cs_domain
                );
                assert_eq!(
                    body.downlink_direct_transfer_r3
                        .rrc_transaction_identifier
                        .0,
                    1
                );
                Ok::<_, &str>(())
            },
        )
        .unwrap();
}

#[test]
fn initial_setup_abort_requires_release_instead_of_reusing_a_fresh_count_space() {
    let base = Instant::now();
    let first = context(1);
    let mut schedule = IntegritySchedule::new(key(), FRESH, SignallingSet::Base);
    let mut exchange = exchange(&first, base, IntegrityChange::Start { fresh: FRESH });
    exchange
        .publish_scheduled_with(
            base,
            first.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(matches!(
        schedule.publish_with(key(), 2, &message(), |_| -> Result<(), ()> {
            panic!("regular SRB2 traffic escaped the pending guard")
        }),
        Err(ScheduleError::Schedule(ScheduleFailure::Suspended))
    ));
    schedule
        .abort_with(key(), first.origin(), |recovery| {
            assert!(recovery.release_connection);
            assert_eq!(recovery.previous_control_origin, None);
            exchange.abort_with(base, crate::security::AbortReason::Cancelled, |_, _| {
                Ok::<_, ()>(())
            })
        })
        .unwrap();
    assert!(schedule.release_required());
    assert_eq!(schedule.configurations(), 0);
    assert!(matches!(
        schedule.publish_with(key(), 3, &message(), |_| Ok::<_, ()>(())),
        Err(ScheduleError::Schedule(ScheduleFailure::Phase))
    ));
    let second = context(2);
    let mut another = self::exchange(&second, base, IntegrityChange::Start { fresh: FRESH });
    assert!(matches!(
        another.publish_scheduled_with(
            base,
            second,
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| -> Result<(), ()> { panic!("released integrity schedule was reused") }
        ),
        Err(ScheduleError::Schedule(ScheduleFailure::Phase))
    ));
}
