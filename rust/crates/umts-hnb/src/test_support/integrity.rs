// SPDX-License-Identifier: AGPL-3.0-or-later

use rasn::types::{BitString, FixedBitString};
use std::time::{Duration, Instant};
use umts_rrc::{
    establishment::TransactionId,
    integrity::IntegritySchedule,
    security::{
        BearerLayout, Capabilities, Command, CommandParameters, IntegrityChange, SecurityExchange,
        SignallingSet,
    },
};
use umts_rrc_schema::{
    class_definitions::{ULDCCHMessage, ULDCCHMessageType},
    information_elements as ie, pdu_definitions as pdu,
};
use umts_security::{
    contexts::IntegrityContext,
    counters::CounterInitialization,
    negotiation::{self, KeyStatus, Support},
};
use umts_types::{Direction, bits::BITS_PER_BYTE};

pub(crate) fn fixed(value: u32) -> FixedBitString<32> {
    let mut result = FixedBitString::default();
    for index in 0..32 {
        result.set(index, value & (1 << (31 - index)) != 0);
    }
    result
}
pub(crate) fn protected(
    context: &IntegrityContext,
    fresh: u32,
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
            fresh,
            Direction::Uplink,
            &preimage,
            preimage.len() * BITS_PER_BYTE,
        )
        .unwrap();
    message.integrity_check_info = Some(ie::IntegrityCheckInfo::new(
        ie::MessageAuthenticationCode(fixed(u32::from_be_bytes(mac))),
        ie::RRCMessageSequenceNumber((count & 15) as u8),
    ));
    rasn::uper::encode(&message).unwrap()
}
pub(crate) fn initial_schedule(context: &IntegrityContext, fresh: u32) -> IntegritySchedule {
    let mut encryption = BitString::repeat(false, 16);
    encryption.set(15, true);
    let mut integrity = BitString::repeat(false, 16);
    integrity.set(14, true);
    let capabilities =
        Capabilities::new(ie::SecurityCapability::new(encryption, integrity), None).unwrap();
    let selection = negotiation::select(
        negotiation::Request {
            integrity: &[0],
            cipher: None,
            keys: KeyStatus::Old,
        },
        Some(capabilities.support()),
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
            integrity: IntegrityChange::Start { fresh },
            cipher: None,
            dpch_cfn: None,
        },
        &capabilities,
    )
    .unwrap();
    let now = Instant::now();
    let mut exchange = SecurityExchange::prepare(command, now, Duration::from_secs(5)).unwrap();
    let key = context.origin().ue;
    let mut schedule = IntegritySchedule::new(key, fresh, SignallingSet::Base);
    exchange
        .publish_scheduled_with(
            now,
            context.clone(),
            CounterInitialization::NewKeys,
            &mut schedule,
            |_| Ok::<_, ()>(()),
        )
        .unwrap();
    let complete = protected(
        context,
        fresh,
        2,
        1,
        ULDCCHMessageType::securityModeComplete(pdu::SecurityModeComplete::new(
            ie::RRCTransactionIdentifier(2),
            Some(ie::IntegrityProtActivationInfo::new(
                ie::RRCMessageSequenceNumberList(vec![ie::RRCMessageSequenceNumber(0); 4]),
            )),
            None,
            None,
        )),
    );
    schedule
        .receive_with(
            key,
            2,
            &complete,
            &mut vec![0; complete.len()],
            |admission, accepted| {
                exchange.complete_with(now, accepted, |completion| admission.complete(*completion))
            },
        )
        .unwrap();
    schedule
        .confirm_delivery_with(key, context.origin(), || Ok::<_, ()>(()))
        .unwrap();
    schedule
}
