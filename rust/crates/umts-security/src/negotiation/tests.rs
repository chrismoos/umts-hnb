// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const ALL: Support = Support {
    uia1: true,
    uia2: true,
    uea0: true,
    uea1: true,
    uea2: true,
};
const KASUMI: Algorithms = Algorithms {
    integrity: IntegrityAlgorithm::Uia1,
    cipher: CipherAlternative::Uea1,
};

#[test]
fn omitted_encryption_cannot_bypass_disabled_cleartext() {
    let policy = Support { uea0: false, ..ALL };
    assert_eq!(
        select(
            request(&[0, 1], None, KeyStatus::New),
            Some(ALL),
            policy,
            Constraints::default()
        ),
        Err(Error::UnsupportedCipher)
    );
}

fn request<'a>(integrity: &'a [u8], cipher: Option<&'a [u8]>, keys: KeyStatus) -> Request<'a> {
    Request {
        integrity,
        cipher,
        keys,
    }
}

#[test]
fn preference_uses_ue_and_implementation_support_without_advertising_missing_crypto() {
    let chosen = select(
        request(&[1, 0], Some(&[2, 1, 0]), KeyStatus::Old),
        Some(ALL),
        Support::KASUMI,
        Constraints::default(),
    )
    .unwrap();
    assert_eq!(chosen.algorithms, KASUMI);
    assert_eq!(chosen.chosen_encryption(), Some(CipherAlternative::Uea1));
    let chosen = select(
        request(&[1, 0], Some(&[2, 1, 0]), KeyStatus::New),
        Some(ALL),
        ALL,
        Constraints::default(),
    )
    .unwrap();
    assert_eq!(
        chosen.algorithms,
        Algorithms {
            integrity: IntegrityAlgorithm::Uia2,
            cipher: CipherAlternative::Uea2
        }
    );
    let chosen = select(
        request(&[0, 1], Some(&[0, 1, 2]), KeyStatus::Old),
        Some(ALL),
        ALL,
        Constraints::default(),
    )
    .unwrap();
    assert_eq!(
        chosen.algorithms,
        Algorithms {
            integrity: IntegrityAlgorithm::Uia1,
            cipher: CipherAlternative::Clear
        }
    );
}

#[test]
fn missing_encryption_information_is_distinct_from_an_explicit_uea0_selection() {
    let absent = select(
        request(&[0], None, KeyStatus::Old),
        Some(ALL),
        Support::KASUMI,
        Constraints::default(),
    )
    .unwrap();
    let explicit = select(
        request(&[0], Some(&[0]), KeyStatus::Old),
        Some(ALL),
        Support::KASUMI,
        Constraints::default(),
    )
    .unwrap();
    assert_eq!(absent.algorithms, explicit.algorithms);
    assert_eq!(absent.chosen_encryption(), None);
    assert_eq!(explicit.chosen_encryption(), Some(CipherAlternative::Clear));
    assert_ne!(absent, explicit);
}

#[test]
fn other_domain_requires_the_existing_alternatives_even_if_not_first_preference() {
    let constraints = Constraints {
        same_domain: None,
        other_domain: Some(KASUMI),
    };
    let chosen = select(
        request(&[1, 0], Some(&[2, 1, 0]), KeyStatus::Old),
        Some(ALL),
        ALL,
        constraints,
    )
    .unwrap();
    assert_eq!(chosen.algorithms, KASUMI);
    for cipher in [None, Some(&[0][..]), Some(&[2][..])] {
        assert_eq!(
            select(
                request(&[0], cipher, KeyStatus::Old),
                Some(ALL),
                ALL,
                constraints
            ),
            Err(Error::Conflict)
        );
    }
    let clear = Constraints {
        other_domain: Some(Algorithms {
            cipher: CipherAlternative::Clear,
            ..KASUMI
        }),
        ..constraints
    };
    assert_eq!(
        select(
            request(&[0], Some(&[1]), KeyStatus::Old),
            Some(ALL),
            ALL,
            clear
        ),
        Err(Error::Conflict)
    );
    assert!(select(request(&[0], None, KeyStatus::Old), Some(ALL), ALL, clear).is_ok());
}

#[test]
fn rekey_requires_new_status_and_cannot_change_an_existing_alternative() {
    let constraints = Constraints {
        same_domain: Some(KASUMI),
        other_domain: None,
    };
    assert_eq!(
        select(
            request(&[0], Some(&[1]), KeyStatus::Old),
            Some(ALL),
            ALL,
            constraints
        ),
        Err(Error::Conflict)
    );
    assert_eq!(
        select(
            request(&[1], Some(&[1]), KeyStatus::New),
            Some(ALL),
            ALL,
            constraints
        ),
        Err(Error::Conflict)
    );
    assert_eq!(
        select(
            request(&[0], Some(&[2]), KeyStatus::New),
            Some(ALL),
            ALL,
            constraints
        ),
        Err(Error::Conflict)
    );
    assert_eq!(
        select(
            request(&[1, 0], Some(&[2, 1]), KeyStatus::New),
            Some(ALL),
            ALL,
            constraints
        )
        .unwrap()
        .algorithms,
        KASUMI
    );
    let inconsistent = Constraints {
        other_domain: Some(Algorithms {
            cipher: CipherAlternative::Clear,
            ..KASUMI
        }),
        ..constraints
    };
    assert_eq!(
        select(
            request(&[0], Some(&[1, 0]), KeyStatus::New),
            Some(ALL),
            ALL,
            inconsistent
        ),
        Err(Error::Conflict)
    );
}

#[test]
fn malformed_or_unsupported_lists_fail_without_clear_or_integrity_fallback() {
    assert_eq!(
        select(
            request(&[], None, KeyStatus::Old),
            Some(ALL),
            ALL,
            Constraints::default()
        ),
        Err(Error::InvalidList)
    );
    assert_eq!(
        select(
            request(&[16], None, KeyStatus::Old),
            Some(ALL),
            ALL,
            Constraints::default()
        ),
        Err(Error::InvalidList)
    );
    assert_eq!(
        select(
            request(&[0; 17], None, KeyStatus::Old),
            Some(ALL),
            ALL,
            Constraints::default()
        ),
        Err(Error::InvalidList)
    );
    assert_eq!(
        select(
            request(&[0], Some(&[]), KeyStatus::Old),
            Some(ALL),
            ALL,
            Constraints::default()
        ),
        Err(Error::InvalidList)
    );
    assert_eq!(
        select(
            request(&[0], None, KeyStatus::Old),
            None,
            ALL,
            Constraints::default()
        ),
        Err(Error::MissingCapabilities)
    );
    assert_eq!(
        select(
            request(&[1], None, KeyStatus::Old),
            Some(ALL),
            Support::KASUMI,
            Constraints::default()
        ),
        Err(Error::UnsupportedIntegrity)
    );
    assert_eq!(
        select(
            request(&[0], Some(&[2]), KeyStatus::Old),
            Some(ALL),
            Support::KASUMI,
            Constraints::default()
        ),
        Err(Error::UnsupportedCipher)
    );
    let chosen = select(
        request(&[15, 0], Some(&[15, 1]), KeyStatus::Old),
        Some(ALL),
        Support::KASUMI,
        Constraints::default(),
    )
    .unwrap();
    assert_eq!(chosen.algorithms, KASUMI);
}
