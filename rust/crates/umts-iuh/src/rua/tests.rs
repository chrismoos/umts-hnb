// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn independent_connect_vector_uses_procedure_ignore_and_separate_reject_ies() {
    let message = Message::Connect {
        domain: Domain::Cs,
        context: ContextId::new(0x123456).unwrap(),
        cause: EstablishmentCause::Normal,
        selector: None,
        membership: None,
        ranap: vec![0xde, 0xad],
    };
    // TS 25.468 APER root flags, 16-bit IE count, 24-bit context and extensible normal cause.
    let literal = [
        0, 1, 0x40, 27, 0, 0, 4, 0, 7, 0, 1, 0, 0, 3, 0, 3, 0x12, 0x34, 0x56, 0, 6, 0, 1, 0x40, 0,
        4, 0, 3, 2, 0xde, 0xad,
    ];
    assert_eq!(message.encode(Limits::default()).unwrap(), literal);
    assert_eq!(
        Message::decode(&literal, Limits::default())
            .unwrap()
            .message,
        message
    );
}

#[test]
fn normal_disconnect_requires_final_ranap_but_radio_failure_does_not() {
    let context = ContextId::new(0).unwrap();
    let mut message = Message::Disconnect {
        domain: Domain::Ps,
        context,
        cause: Cause::Radio(RadioCause::Normal),
        ranap: None,
    };
    assert_eq!(
        message.encode(Limits::default()),
        Err(Error::MissingIe(RANAP))
    );
    message = Message::Disconnect {
        domain: Domain::Ps,
        context,
        cause: Cause::Radio(RadioCause::ConnectFailed),
        ranap: None,
    };
    let bytes = message.encode(Limits::default()).unwrap();
    let packet = Packet::decode(Protocol::Rua, &bytes, Limits::default()).unwrap();
    assert_eq!(packet.required(CAUSE).unwrap().value.as_bytes(), &[4]);
    assert_eq!(
        Message::decode(&bytes, Limits::default()).unwrap().message,
        message
    );
    let mut invalid = packet;
    invalid
        .ies
        .iter_mut()
        .find(|ie| ie.id == CAUSE)
        .unwrap()
        .value = rasn::types::Any::new(vec![0]);
    assert_eq!(
        Message::decode(
            &invalid.encode(Limits::default()).unwrap(),
            Limits::default()
        ),
        Err(Error::MissingIe(RANAP))
    );
}

#[test]
fn selector_membership_both_domains_and_context_range_preserve_wire_values() {
    use crate::values::{FutureSelector, SelectorVersion};
    for domain in [Domain::Cs, Domain::Ps] {
        let message = Message::Connect {
            domain,
            context: ContextId::new(0xffffff).unwrap(),
            cause: EstablishmentCause::Emergency,
            selector: Some(NasSelector {
                version: SelectorVersion::Later(FutureSelector {
                    coding: fixed::<15>(&[0xab, 0xcc]).unwrap(),
                }),
            }),
            membership: Some(Membership::Member),
            ranap: vec![1; 128],
        };
        assert_eq!(
            Message::decode(
                &message.encode(Limits::default()).unwrap(),
                Limits::default()
            )
            .unwrap()
            .message,
            message
        );
    }
    assert_eq!(ContextId::new(0x1000000), Err(Error::Identity));
}

#[test]
fn transfer_empty_payload_missing_context_and_unknown_critical_ies_are_rejected() {
    let message = Message::Direct {
        domain: Domain::Cs,
        context: ContextId::new(3).unwrap(),
        ranap: vec![],
    };
    assert_eq!(message.encode(Limits::default()), Err(Error::Length));
    let packet = Packet::initiating(
        Protocol::Rua,
        DIRECT,
        Criticality::Ignore,
        vec![domain_ie(Domain::Cs).unwrap(), ranap_ie(&[1]).unwrap()],
    );
    assert_eq!(
        Message::decode(
            &packet.encode(Limits::default()).unwrap(),
            Limits::default()
        ),
        Err(Error::MissingIe(CONTEXT))
    );
}
