// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn location() -> Location {
    Location {
        plmn: [0, 0xf1, 0x10],
        lac: 23,
        rac: 7,
        sac: 42,
    }
}

#[test]
fn common_identity_matches_independent_aper_and_preserves_the_complete_imsi() {
    let imsi = Imsi::from_digits("001010000000001").unwrap();
    let encoded = common_identity(imsi, Limits::default()).unwrap();
    // TS 25.413 Common ID APER: procedure 15, ignore, Permanent NAS UE ID 23, IMSI choice.
    assert_eq!(
        encoded,
        [
            0x00, 0x0f, 0x40, 0x10, 0x00, 0x00, 0x01, 0x00, 0x17, 0x40, 0x09, 0x50, 0x00, 0x01,
            0x01, 0x00, 0x00, 0x00, 0x00, 0xf1,
        ]
    );
    assert_eq!(
        decode(&encoded, Limits::default()).unwrap().message,
        Inbound::CommonIdentity(imsi)
    );
}

#[test]
fn uplink_direct_transfer_has_no_sapi_and_ps_includes_lai_rac_sai() {
    for domain in [Domain::Cs, Domain::Ps] {
        let bytes = uplink_direct(domain, location(), &[0x08, 0x41], Limits::default()).unwrap();
        let packet = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
        packet.check(&[NAS, LAI, RAC, SAI, SAPI], &[]).unwrap();
        assert!(packet.optional(SAPI).is_none());
        assert_eq!(
            packet.required(NAS).unwrap().value.as_bytes(),
            &[2, 0x08, 0x41]
        );
        assert_eq!(packet.optional(LAI).is_some(), domain == Domain::Ps);
        assert_eq!(packet.optional(RAC).is_some(), domain == Domain::Ps);
        assert_eq!(packet.optional(SAI).is_some(), domain == Domain::Ps);
    }
    // TS 25.413 Direct Transfer APER: procedure 20, ignore, one IE, NAS length 2.
    assert_eq!(
        uplink_direct(Domain::Cs, location(), &[0x08, 0x41], Limits::default()).unwrap(),
        [0, 20, 0x40, 10, 0, 0, 1, 0, 16, 0x40, 3, 2, 0x08, 0x41]
    );
}

#[test]
fn initial_ue_shares_context_with_rua_and_preserves_conditional_rac_and_rnc_range() {
    for domain in [Domain::Cs, Domain::Ps] {
        let context = ContextId::new(0xabcdef).unwrap();
        let bytes =
            initial_ue(context, domain, 0x123, location(), &[1], Limits::default()).unwrap();
        let packet = Packet::decode(Protocol::Ranap, &bytes, Limits::default()).unwrap();
        packet
            .check(
                &[DOMAIN, LAI, RAC, SAI, NAS, SIGNALLING_ID, GLOBAL_RNC],
                &[],
            )
            .unwrap();
        assert_eq!(
            packet.required(SIGNALLING_ID).unwrap().value.as_bytes(),
            &[0xab, 0xcd, 0xef]
        );
        assert_eq!(
            packet.required(GLOBAL_RNC).unwrap().value.as_bytes(),
            &[0, 0xf1, 0x10, 1, 0x23]
        );
        assert_eq!(
            packet.required(LAI).unwrap().value.as_bytes(),
            &[0, 0, 0xf1, 0x10, 0, 23]
        );
        assert_eq!(packet.optional(RAC).is_some(), domain == Domain::Ps);
    }
    assert!(
        initial_ue(
            ContextId::new(0).unwrap(),
            Domain::Cs,
            4096,
            location(),
            &[1],
            Limits::default()
        )
        .is_err()
    );
}

#[test]
fn missing_downlink_sapi_continues_as_signalling_and_sms_priority_is_distinct() {
    for priority in [None, Some(Priority::Signalling), Some(Priority::Sms)] {
        let bytes = downlink_direct(&[0x09, 1], priority, Limits::default()).unwrap();
        assert_eq!(
            decode(&bytes, Limits::default()).unwrap().message,
            Inbound::Direct {
                nas: vec![0x09, 1],
                priority: priority.unwrap_or(Priority::Signalling)
            }
        );
    }
}

#[test]
fn security_preserves_ordered_algorithm_ids_optional_cipher_and_all_128_key_bits() {
    for encryption in [
        None,
        Some(Protection {
            algorithms: vec![Algorithm(0), Algorithm(2), Algorithm(1)],
            key: Key::new([0x5a; 16]),
        }),
    ] {
        let command = SecurityCommand {
            integrity: Protection {
                algorithms: vec![Algorithm(1), Algorithm(0)],
                key: Key::new([0xa5; 16]),
            },
            encryption,
            key_status: KeyStatus::New,
        };
        let bytes = encode_security(&command, Limits::default()).unwrap();
        assert_eq!(
            decode(&bytes, Limits::default()).unwrap().message,
            Inbound::Security(command)
        );
    }
    assert_eq!(format!("{:?}", Key::new([0xa5; 16])), "Key([redacted])");
}
