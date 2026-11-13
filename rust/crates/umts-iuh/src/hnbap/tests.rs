// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

pub(crate) fn parameters() -> HnbParameters {
    HnbParameters {
        identity: b"Rust HNB".to_vec(),
        plmn: [0, 0xf1, 0x10],
        lac: 23,
        rac: 7,
        sac: 42,
        cell: 0x1234567,
        access: AccessMode::Open,
        csg: None,
    }
}
fn identity() -> UeIdentity {
    UeIdentity::Imsi(Imsi::from_digits("001010000000099").unwrap())
}

#[test]
fn register_request_carries_access_extension_and_exact_canonical_location_fields() {
    let parameters = parameters();
    let message = Message::HnbRegister(parameters.clone());
    let encoded = message.encode(Limits::default()).unwrap();
    let packet = Packet::decode(Protocol::Hnbap, &encoded, Limits::default()).unwrap();
    assert_eq!(packet.ies.len(), 7);
    assert_eq!(packet.required(LOCATION).unwrap().value.as_bytes(), &[0]);
    assert_eq!(
        packet.required(PLMN).unwrap().value.as_bytes(),
        &parameters.plmn
    );
    assert_eq!(
        packet.required(CELL).unwrap().value.as_bytes(),
        &[0x12, 0x34, 0x56, 0x70]
    );
    assert_eq!(packet.extensions[0].id, ACCESS_MODE);
    assert_eq!(
        Message::decode(&encoded, Limits::default())
            .unwrap()
            .message,
        message
    );
}

#[test]
fn imsi_bcd_has_only_terminal_filler_and_all_standard_identity_choices_roundtrip() {
    let imsi = Imsi::from_digits("001010000000099").unwrap();
    assert_eq!(
        imsi.bcd(),
        &[0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x90, 0xf9]
    );
    for bytes in [
        &[0x0a, 0, 0][..],
        &[0xf0, 0, 0],
        &[0, 0, 0x10, 0, 0, 0, 0, 0],
    ] {
        assert!(Imsi::from_bcd(bytes).is_err());
    }
    for value in [
        identity(),
        UeIdentity::Tmsi {
            tmsi: [1, 2, 3, 4],
            plmn: [0, 0xf1, 0x10],
            lac: 23,
        },
        UeIdentity::Ptmsi {
            ptmsi: [4, 3, 2, 1],
            plmn: [0, 0xf1, 0x10],
            lac: 24,
            rac: 7,
        },
        UeIdentity::Imei([0; 8]),
        UeIdentity::Esn([1; 4]),
        UeIdentity::ImsiDs41(vec![1; 5]),
        UeIdentity::ImsiEsn {
            imsi: vec![2; 7],
            esn: [3; 4],
        },
        UeIdentity::TmsiDs41(vec![4; 17]),
    ] {
        let message = Message::UeRegister {
            identity: value,
            cause: RegistrationCause::Normal,
            capabilities: Capabilities {
                release: Release::R8AndLater,
                csg: CsgCapability::NotCapable,
            },
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
}

#[test]
fn overload_backoff_is_conditional_and_mux_port_has_1024_offset() {
    let overload = Cause::Radio(RadioCause::Overload);
    assert_eq!(
        Message::HnbRejected {
            cause: overload.clone(),
            backoff_seconds: None
        }
        .encode(Limits::default()),
        Err(Error::MissingIe(BACKOFF))
    );
    let rejected = Message::HnbRejected {
        cause: overload,
        backoff_seconds: Some(42),
    };
    assert_eq!(
        Message::decode(
            &rejected.encode(Limits::default()).unwrap(),
            Limits::default()
        )
        .unwrap()
        .message,
        rejected
    );
    let accepted = Message::HnbAccepted {
        rnc: 0x123,
        mux_port: Some(1024),
    };
    let bytes = accepted.encode(Limits::default()).unwrap();
    let packet = Packet::decode(Protocol::Hnbap, &bytes, Limits::default()).unwrap();
    assert_eq!(packet.extensions[0].value.as_bytes(), &[0, 0]);
    assert_eq!(
        Message::decode(&bytes, Limits::default()).unwrap().message,
        accepted
    );
    assert!(
        Message::HnbAccepted {
            rnc: 1,
            mux_port: Some(1023)
        }
        .encode(Limits::default())
        .is_err()
    );
}

#[test]
fn accepted_rejected_and_deregistered_contexts_remain_checked_24_bit_values() {
    for message in [
        Message::UeAccepted {
            identity: identity(),
            context: ContextId::new(0x123456).unwrap(),
            membership: Some(crate::rua::Membership::Member),
        },
        Message::UeRejected {
            identity: identity(),
            cause: Cause::Radio(RadioCause::InvalidUeIdentity),
        },
        Message::UeDeregister {
            context: ContextId::new(0xffffff).unwrap(),
            cause: Cause::Radio(RadioCause::ConnectionLost),
        },
    ] {
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
}
