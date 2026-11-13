// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn security_responses_match_literal_aper_fields_and_cause_ranges() {
    for (response, expected) in [
        (
            SecurityResponse::Complete {
                integrity: Algorithm(0),
                encryption: None,
            },
            vec![0x20, 6, 0, 8, 0, 0, 1, 0, 6, 0, 1, 0],
        ),
        (
            SecurityResponse::Complete {
                integrity: Algorithm(0),
                encryption: Some(Algorithm(1)),
            },
            vec![0x20, 6, 0, 13, 0, 0, 2, 0, 6, 0, 1, 0, 0, 5, 0x40, 1, 0x10],
        ),
        (
            SecurityResponse::Reject(SecurityReject::AlgorithmsUnsupported),
            vec![0x40, 6, 0, 9, 0, 0, 1, 0, 4, 0x40, 2, 2, 0xc0],
        ),
        (
            SecurityResponse::Reject(SecurityReject::Conflict),
            vec![0x40, 6, 0, 9, 0, 0, 1, 0, 4, 0x40, 2, 3, 0],
        ),
        (
            SecurityResponse::Reject(SecurityReject::RadioProcedureFailure),
            vec![0x40, 6, 0, 9, 0, 0, 1, 0, 4, 0x40, 2, 3, 0x40],
        ),
    ] {
        assert_eq!(response.encode(Limits::default()).unwrap(), expected);
        let decoded = SecurityResponse::decode(&expected, Limits::default()).unwrap();
        assert_eq!(decoded.response, response);
        assert!(decoded.notify_ies.is_empty());
    }
}

#[test]
fn absent_mandatory_duplicate_and_unsupported_security_responses_are_rejected() {
    let response = SecurityResponse::Complete {
        integrity: Algorithm(0),
        encryption: Some(Algorithm(1)),
    }
    .encode(Limits::default())
    .unwrap();
    let original = Packet::decode(Protocol::Ranap, &response, Limits::default()).unwrap();
    let mut missing = original.clone();
    missing.ies.remove(0);
    assert!(
        SecurityResponse::decode(
            &missing.encode(Limits::default()).unwrap(),
            Limits::default()
        )
        .is_err()
    );
    let mut duplicate = original.clone();
    duplicate.ies.push(duplicate.ies[0].clone());
    assert_eq!(
        duplicate.encode(Limits::default()),
        Err(Error::DuplicateIe(6))
    );
    let duplicate_wire = [
        0x20, 6, 0, 18, 0, 0, 3, 0, 6, 0, 1, 0, 0, 5, 0x40, 1, 0x10, 0, 6, 0, 1, 0,
    ];
    assert!(SecurityResponse::decode(&duplicate_wire, Limits::default()).is_err());
    let mut wrong = original.clone();
    wrong.procedure += 1;
    assert!(
        SecurityResponse::decode(&wrong.encode(Limits::default()).unwrap(), Limits::default())
            .is_err()
    );
    assert!(
        SecurityResponse::Complete {
            integrity: Algorithm(16),
            encryption: None
        }
        .encode(Limits::default())
        .is_err()
    );
    let mut notified = original;
    notified
        .ies
        .push(Ie::new(60000, Criticality::Notify, &Algorithm(0)).unwrap());
    assert_eq!(
        SecurityResponse::decode(
            &notified.encode(Limits::default()).unwrap(),
            Limits::default()
        )
        .unwrap()
        .notify_ies,
        [60000]
    );
}
