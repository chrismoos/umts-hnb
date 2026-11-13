// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use umts_types::RadioEpoch;

fn origin() -> KeyOrigin {
    KeyOrigin {
        ue: UeKey {
            epoch: RadioEpoch::new(NonZeroU64::MIN),
            slot: 7,
            generation: NonZeroU64::MIN,
        },
        domain: CnDomain::Cs,
        generation: NonZeroU64::MIN,
    }
}

#[test]
fn snow_contexts_are_reentrant_and_reject_wrong_owner() {
    let origin = origin();
    let (cipher, integrity) = KeySet::new(
        origin,
        CipherMaterial::Uea2(Key::new([0x42; 16])),
        Key::new([0x18; 16]),
        IntegrityAlgorithm::Uia2,
    )
    .split();
    let mut expected = [0x5a; 16];
    Uea2Key::new(&Key::new([0x42; 16]))
        .apply(123, 3, Direction::Downlink, &mut expected, 128)
        .unwrap();
    for _ in 0..3 {
        let mut actual = [0x5a; 16];
        cipher
            .apply(origin, 123, 3, Direction::Downlink, &mut actual, 128)
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(integrity.kind(), IntegrityAlgorithm::Uia2);
        assert_eq!(
            integrity.algorithm(origin).unwrap().authenticate(
                123,
                456,
                Direction::Uplink,
                &actual,
                128
            ),
            Uia2Key::new(&Key::new([0x18; 16])).authenticate(
                123,
                456,
                Direction::Uplink,
                &actual,
                128
            )
        );
        let wrong = KeyOrigin {
            domain: CnDomain::Ps,
            ..origin
        };
        assert_eq!(
            cipher.apply(wrong, 123, 3, Direction::Downlink, &mut actual, 128),
            Err(Error::Origin)
        );
        assert!(matches!(integrity.algorithm(wrong), Err(Error::Origin)));
    }
}

#[test]
fn expanded_keys_keep_independent_domain_generation_and_ue_ownership() {
    let origin = origin();
    let (cipher, integrity) = KeySet::uia1(
        origin,
        CipherMaterial::Uea1(Key::new([0x42; 16])),
        Key::new([0x18; 16]),
    )
    .split();
    let reference_cipher = Uea1Key::new(&Key::new([0x42; 16]));
    let mut expected = [0x5a; 16];
    reference_cipher
        .apply(123, 3, Direction::Downlink, &mut expected, 128)
        .unwrap();
    let clone = cipher.clone();
    drop(cipher);
    let mut actual = [0x5a; 16];
    clone
        .apply(origin, 123, 3, Direction::Downlink, &mut actual, 128)
        .unwrap();
    assert_eq!(actual, expected);
    let expected = Uia1Key::new(&Key::new([0x18; 16]))
        .authenticate(123, 456, Direction::Uplink, &actual, 128)
        .unwrap();
    assert_eq!(
        integrity
            .algorithm(origin)
            .unwrap()
            .authenticate(123, 456, Direction::Uplink, &actual, 128)
            .unwrap(),
        expected
    );
    for wrong in [
        KeyOrigin {
            domain: CnDomain::Ps,
            ..origin
        },
        KeyOrigin {
            generation: NonZeroU64::new(2).unwrap(),
            ..origin
        },
        KeyOrigin {
            ue: UeKey {
                slot: 8,
                ..origin.ue
            },
            ..origin
        },
        KeyOrigin {
            ue: UeKey {
                generation: NonZeroU64::new(2).unwrap(),
                ..origin.ue
            },
            ..origin
        },
        KeyOrigin {
            ue: UeKey {
                epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
                ..origin.ue
            },
            ..origin
        },
    ] {
        let mut bytes = [0xa5; 16];
        assert_eq!(
            clone.apply(wrong, 123, 3, Direction::Downlink, &mut bytes, 128),
            Err(Error::Origin)
        );
        assert_eq!(bytes, [0xa5; 16]);
        assert!(matches!(integrity.algorithm(wrong), Err(Error::Origin)));
    }
    let printed = format!("{clone:?} {integrity:?}");
    assert!(!printed.contains("66, 66"));
    assert!(!printed.contains("24, 24"));
}

#[test]
fn clear_cipher_is_explicit_and_still_validates_owner_and_wire_bounds() {
    let origin = origin();
    let (cipher, _) = KeySet::uia1(origin, CipherMaterial::Clear, Key::new([0; 16])).split();
    assert!(cipher.is_clear());
    let mut bytes = [0x5a; 2];
    cipher
        .apply(origin, 123, 31, Direction::Uplink, &mut bytes, 13)
        .unwrap();
    assert_eq!(bytes, [0x5a; 2]);
    assert_eq!(
        cipher.apply(origin, 123, 32, Direction::Uplink, &mut bytes, 13),
        Err(Error::Bearer)
    );
    assert_eq!(
        cipher.apply(origin, 123, 0, Direction::Uplink, &mut bytes, 8),
        Err(Error::Length)
    );
}
