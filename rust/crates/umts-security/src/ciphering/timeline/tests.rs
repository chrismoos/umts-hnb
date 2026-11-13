// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    Key,
    ciphering::{CipherBudget, TransparentDomain},
    contexts::{CipherMaterial, KeyOrigin, KeySet},
    counters::Start,
};
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn ue() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 1,
        generation: NonZeroU64::MIN,
    }
}

fn context(domain: CnDomain, generation: u64) -> CipherContext {
    let origin = KeyOrigin {
        ue: ue(),
        domain,
        generation: NonZeroU64::new(generation).unwrap(),
    };
    KeySet::uia1(
        origin,
        CipherMaterial::Uea1(Key::new([generation as u8; 16])),
        Key::new([0; 16]),
    )
    .split()
    .0
}

#[test]
fn transparent_counter_activation_retains_the_key_and_old_frozen_interval() {
    let key = context(CnDomain::Cs, 1);
    let mut domain = TransparentDomain::new(
        ue(),
        CnDomain::Cs,
        CountAnchor::transparent_frozen_hfn(250, Start::new(3).unwrap()).unwrap(),
        Configuration::Configured(key.clone()),
    )
    .unwrap();
    let activation = CountAnchor::from_start(
        CipherMode::Transparent,
        Position::Frame(520),
        Start::new(9).unwrap(),
    )
    .unwrap();
    assert_eq!(
        domain.activate_counter_with(ue(), Some(key.origin()), activation, 256, || Err::<(), _>(
            "full"
        )),
        Err(UpdateError::Publication("full"))
    );
    assert_eq!(
        domain.selected(ue(), 520),
        Ok(CipherUse::Data {
            origin: Some(key.origin()),
            count: 0x3008
        })
    );
    domain
        .activate_counter_with(
            ue(),
            Some(key.origin()),
            activation,
            256,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(domain.configurations(), 1);
    for (frame, count) in [
        (255, 0x30ff),
        (256, 0x3000),
        (519, 0x3007),
        (520, 0x9008),
        (768, 0x9100),
    ] {
        assert_eq!(
            domain.selected(ue(), frame),
            Ok(CipherUse::Data {
                origin: Some(key.origin()),
                count
            })
        );
    }
    domain.retire_before(ue(), 520).unwrap();
    assert_eq!(domain.selected(ue(), 519), Err(Error::Position));
    assert_eq!(domain.configurations(), 1);
}

#[test]
fn transparent_counter_activation_rejects_wrong_owner_key_mode_and_boundary() {
    let key = context(CnDomain::Cs, 1);
    let mut domain = TransparentDomain::new(
        ue(),
        CnDomain::Cs,
        CountAnchor::transparent_frozen_hfn(250, Start::new(3).unwrap()).unwrap(),
        Configuration::Configured(key.clone()),
    )
    .unwrap();
    let make = |at| {
        CountAnchor::from_start(
            CipherMode::Transparent,
            Position::Frame(at),
            Start::new(9).unwrap(),
        )
        .unwrap()
    };
    let other_ue = UeKey {
        slot: ue().slot + 1,
        ..ue()
    };
    for (owner, origin, anchor, first, error) in [
        (other_ue, Some(key.origin()), make(520), 256, Error::Owner),
        (
            ue(),
            Some(context(CnDomain::Cs, 2).origin()),
            make(520),
            256,
            Error::Owner,
        ),
        (ue(), Some(key.origin()), make(521), 256, Error::Position),
        (ue(), Some(key.origin()), make(248), 256, Error::Position),
        (ue(), Some(key.origin()), make(520), 521, Error::Position),
        (ue(), Some(key.origin()), make(520), 249, Error::Position),
        (
            ue(),
            Some(key.origin()),
            CountAnchor::transparent_frozen_hfn(520, Start::new(9).unwrap()).unwrap(),
            256,
            Error::Mode,
        ),
    ] {
        assert_eq!(
            domain.activate_counter_with(owner, origin, anchor, first, || -> Result<(), ()> {
                panic!("invalid activation published")
            }),
            Err(UpdateError::Cipher(error))
        );
    }
    domain
        .activate_counter_with(ue(), Some(key.origin()), make(520), 256, || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        domain.activate_counter_with(ue(), Some(key.origin()), make(528), 520, || Ok::<_, ()>(())),
        Err(UpdateError::Cipher(Error::Mode))
    );
}

#[test]
fn frozen_counter_intervals_do_not_consume_additional_key_configuration_budget() {
    let first = context(CnDomain::Cs, 1);
    let mut domain = TransparentDomain::new(
        ue(),
        CnDomain::Cs,
        CountAnchor::transparent_frozen_hfn(0, Start::new(0).unwrap()).unwrap(),
        Configuration::Configured(first.clone()),
    )
    .unwrap();
    let second = context(CnDomain::Cs, 2);
    domain
        .activate_counter_with(
            ue(),
            Some(first.origin()),
            CountAnchor::from_start(
                CipherMode::Transparent,
                Position::Frame(208),
                Start::new(1).unwrap(),
            )
            .unwrap(),
            0,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    domain
        .schedule_with(
            second.clone(),
            CountAnchor::from_start(
                CipherMode::Transparent,
                Position::Frame(512),
                Start::new(0).unwrap(),
            )
            .unwrap(),
            208,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(domain.configurations(), 2);
    assert_eq!(
        domain.schedule_with(
            context(CnDomain::Cs, 3),
            CountAnchor::from_start(
                CipherMode::Transparent,
                Position::Frame(768),
                Start::new(0).unwrap()
            )
            .unwrap(),
            512,
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::Capacity))
    );
    domain.retire_before(ue(), 512).unwrap();
    assert_eq!(domain.configurations(), 1);
}

#[test]
fn stopped_transparent_counter_cannot_be_protected_until_cipher_start() {
    let stopped = CountAnchor::transparent_stopped(8, Start::new(2).unwrap()).unwrap();
    assert!(matches!(
        TransparentDomain::new(
            ue(),
            CnDomain::Cs,
            stopped,
            Configuration::Configured(context(CnDomain::Cs, 1))
        ),
        Err(Error::Mode)
    ));
    let mut domain =
        TransparentDomain::new(ue(), CnDomain::Cs, stopped, Configuration::Unprotected).unwrap();
    assert_eq!(
        domain.selected(ue(), 512),
        Ok(CipherUse::Data {
            origin: None,
            count: 0x2008
        })
    );
    domain
        .schedule_with(
            context(CnDomain::Cs, 1),
            CountAnchor::from_start(
                CipherMode::Transparent,
                Position::Frame(520),
                Start::new(3).unwrap(),
            )
            .unwrap(),
            512,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        domain.selected(ue(), 519),
        Ok(CipherUse::Data {
            origin: None,
            count: 0x2008
        })
    );
    assert_eq!(
        domain.selected(ue(), 520),
        Ok(CipherUse::Data {
            origin: Some(context(CnDomain::Cs, 1).origin()),
            count: 0x3008
        })
    );
}

#[test]
fn unstarted_domain_response_reinitializes_start_without_starting_count_progression() {
    let mut domain = TransparentDomain::new(
        ue(),
        CnDomain::Cs,
        CountAnchor::transparent_stopped(8, Start::new(2).unwrap()).unwrap(),
        Configuration::Unprotected,
    )
    .unwrap();
    let position = Position::Frame(520);
    assert_eq!(
        domain.activate_counter_with(
            ue(),
            None,
            CountAnchor::from_start(CipherMode::Transparent, position, Start::new(3).unwrap())
                .unwrap(),
            256,
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::Mode))
    );
    domain
        .activate_counter_with(
            ue(),
            None,
            CountAnchor::transparent_stopped(520, Start::new(3).unwrap()).unwrap(),
            256,
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        domain.selected(ue(), 519),
        Ok(CipherUse::Data {
            origin: None,
            count: 0x2008
        })
    );
    for frame in [520, 768, u64::MAX] {
        assert_eq!(
            domain.selected(ue(), frame),
            Ok(CipherUse::Data {
                origin: None,
                count: 0x3008
            })
        );
    }
    assert_eq!(domain.configurations(), 0);
}

fn timeline(scope: Scope) -> CipherTimeline {
    let binding = Binding::new(
        ue(),
        scope,
        if scope == Scope::Signalling { 2 } else { 5 },
        CipherMode::Acknowledged,
        Direction::Downlink,
    )
    .unwrap();
    CipherTimeline::new(
        binding,
        CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(0), 0).unwrap(),
        Configuration::Unprotected,
    )
    .unwrap()
}

fn anchor(at: u64) -> CountAnchor {
    CountAnchor::from_start(
        CipherMode::Acknowledged,
        Position::Sequence(at),
        Start::new(0).unwrap(),
    )
    .unwrap()
}

#[test]
fn uplink_srb2_reselects_activation_and_preserves_in_flight_old_key_pdus() {
    for new_at in [13, 15] {
        let binding = Binding::new(
            ue(),
            Scope::Signalling,
            SECURITY_RESPONSE_BEARER,
            CipherMode::Acknowledged,
            Direction::Uplink,
        )
        .unwrap();
        let mut timeline =
            CipherTimeline::new(binding, anchor(0), Configuration::Unprotected).unwrap();
        let old = context(CnDomain::Cs, 1);
        let new = context(CnDomain::Ps, 1);
        timeline
            .schedule_with(old.clone(), anchor(14), Position::Sequence(0), || {
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(
            timeline.schedule_with(new.clone(), anchor(new_at), Position::Sequence(12), || {
                Err::<(), _>("queue full")
            }),
            Err(UpdateError::Publication("queue full"))
        );
        assert_eq!(
            timeline.pending_activation(ue(), Position::Sequence(12)),
            Ok(Some(Position::Sequence(14)))
        );
        timeline
            .schedule_with(new.clone(), anchor(new_at), Position::Sequence(12), || {
                Ok::<_, ()>(())
            })
            .unwrap();
        let previous_origin = (new_at > 14).then_some(old.origin());
        assert_eq!(
            timeline.selected(ue(), Position::Sequence(new_at - 1)),
            Ok(CipherUse::Data {
                origin: previous_origin,
                count: (new_at - 1) as u32,
            })
        );
        assert_eq!(
            timeline.selected(ue(), Position::Sequence(new_at)),
            Ok(CipherUse::Data {
                origin: Some(new.origin()),
                count: new_at as u32,
            })
        );
        assert_eq!(
            timeline
                .origins()
                .any(|origin| origin == Some(old.origin())),
            new_at > 14
        );
    }
}

#[test]
fn other_cipher_bindings_cannot_move_a_pending_activation() {
    for (scope, bearer, direction) in [
        (
            Scope::Signalling,
            SECURITY_RESPONSE_BEARER,
            Direction::Downlink,
        ),
        (Scope::Signalling, 3, Direction::Uplink),
        (
            Scope::User(CnDomain::Cs),
            FIRST_USER_BEARER,
            Direction::Uplink,
        ),
    ] {
        let binding =
            Binding::new(ue(), scope, bearer, CipherMode::Acknowledged, direction).unwrap();
        let mut timeline =
            CipherTimeline::new(binding, anchor(0), Configuration::Unprotected).unwrap();
        timeline
            .schedule_with(
                context(CnDomain::Cs, 1),
                anchor(14),
                Position::Sequence(0),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(
            timeline.schedule_with(
                context(CnDomain::Cs, 2),
                anchor(15),
                Position::Sequence(12),
                || { panic!("invalid update must not publish") }
            ),
            Err::<(), _>(UpdateError::<()>::Cipher(Error::PendingActivation))
        );
    }
}

fn data(sequence: u16) -> Vec<u8> {
    let mut bytes = ((sequence << 3) | 0x8001).to_be_bytes().to_vec();
    bytes.extend([0x12, 0x34, 0x56, 0x78]);
    bytes
}

#[test]
fn reset_selects_pending_configuration_with_its_original_hfn_initialization() {
    for start in [0, 0x12345] {
        let mut timeline = timeline(Scope::Signalling);
        let key = context(CnDomain::Cs, 1);
        let first = Position::Sequence(37);
        timeline
            .schedule_with(
                key.clone(),
                CountAnchor::from_start(
                    CipherMode::Acknowledged,
                    first,
                    Start::new(start).unwrap(),
                )
                .unwrap(),
                Position::Sequence(0),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(
            timeline.selected(ue(), Position::Sequence(1)),
            Ok(CipherUse::Data {
                origin: None,
                count: 1
            })
        );
        assert_eq!(
            timeline.reset_configuration(ue(), Position::Sequence(1)),
            Ok(ResetConfiguration {
                origin: Some(key.origin()),
                initial_count: Some(start << 12)
            })
        );
        assert_eq!(
            timeline.reset_configuration(ue(), first),
            Ok(ResetConfiguration {
                origin: Some(key.origin()),
                initial_count: None
            })
        );
        let reset = CountAnchor::new(
            CipherMode::Acknowledged,
            Position::Sequence(4096),
            start << 12,
        )
        .unwrap();
        timeline
            .reset_with(ue(), Some(key.origin()), reset, || Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(
            timeline.selected(ue(), Position::Sequence(4096)),
            Ok(CipherUse::Data {
                origin: Some(key.origin()),
                count: start << 12
            })
        );
        assert_eq!(
            timeline.reset_configuration(ue(), Position::Sequence(4096)),
            Ok(ResetConfiguration {
                origin: Some(key.origin()),
                initial_count: None
            })
        );
    }
}

#[test]
fn reset_selection_validates_identity_and_mode_with_the_maximum_old_hfn() {
    let binding = Binding::new(
        ue(),
        Scope::Signalling,
        2,
        CipherMode::Acknowledged,
        Direction::Downlink,
    )
    .unwrap();
    let mut timeline = CipherTimeline::new(
        binding,
        CountAnchor::new(
            CipherMode::Acknowledged,
            Position::Sequence(0),
            !super::super::count::AM_SEQUENCE_MASK,
        )
        .unwrap(),
        Configuration::Unprotected,
    )
    .unwrap();
    let key = context(CnDomain::Cs, 1);
    timeline
        .schedule_with(
            key.clone(),
            CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(4096), 0).unwrap(),
            Position::Sequence(0),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(4096)),
        Ok(CipherUse::Data {
            origin: Some(key.origin()),
            count: 0
        })
    );
    let mut wrong = ue();
    wrong.slot += 1;
    assert_eq!(
        timeline.reset_configuration(wrong, Position::Sequence(1)),
        Err(Error::Owner)
    );
    assert_eq!(
        timeline.reset_configuration(ue(), Position::Frame(1)),
        Err(Error::Mode)
    );
    assert_eq!(
        timeline.reset_configuration(ue(), Position::Sequence(1)),
        Ok(ResetConfiguration {
            origin: Some(key.origin()),
            initial_count: Some(0)
        })
    );
}

#[test]
fn activation_retains_old_pdu_keys_and_failed_publication_does_not_consume_a_generation() {
    let mut timeline = timeline(Scope::Signalling);
    let old = context(CnDomain::Cs, 1);
    let new = context(CnDomain::Ps, 1);
    assert_eq!(
        timeline.schedule_with(old.clone(), anchor(10), Position::Sequence(0), || Err::<
            (),
            _,
        >(
            7
        )),
        Err(UpdateError::Publication(7))
    );
    assert_eq!(timeline.configurations(), 0);
    timeline
        .schedule_with(old.clone(), anchor(10), Position::Sequence(0), || {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(timeline.configurations(), 1);
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(9)),
        Ok(CipherUse::Data {
            origin: None,
            count: 9
        })
    );
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(10)),
        Ok(CipherUse::Data {
            origin: Some(old.origin()),
            count: 10
        })
    );
    timeline
        .schedule_with(new.clone(), anchor(20), Position::Sequence(11), || {
            Ok::<_, ()>(())
        })
        .unwrap();
    let mut old_pdu = data(12);
    let length = old_pdu.len() * 8;
    let original = old_pdu.clone();
    timeline
        .apply(ue(), Position::Sequence(12), &mut old_pdu, length)
        .unwrap();
    let mut expected = original.clone();
    old.apply(
        old.origin(),
        12,
        1,
        Direction::Downlink,
        &mut expected[2..],
        length - 16,
    )
    .unwrap();
    assert_eq!(old_pdu, expected);
    old_pdu[1] ^= 4;
    timeline
        .apply(ue(), Position::Sequence(12), &mut old_pdu, length)
        .unwrap();
    assert_eq!(&old_pdu[2..], &original[2..]);
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(20)),
        Ok(CipherUse::Data {
            origin: Some(new.origin()),
            count: 20
        })
    );
    timeline
        .schedule_with(
            context(CnDomain::Ps, 2),
            anchor(30),
            Position::Sequence(21),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        timeline.schedule_with(
            context(CnDomain::Ps, 3),
            anchor(40),
            Position::Sequence(31),
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::Capacity))
    );
    timeline
        .retire_before(ue(), Position::Sequence(10))
        .unwrap();
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(9)),
        Err(Error::Position)
    );
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(12)),
        Ok(CipherUse::Data {
            origin: Some(old.origin()),
            count: 12
        })
    );
    timeline
        .retire_before(ue(), Position::Sequence(20))
        .unwrap();
    timeline
        .schedule_with(
            context(CnDomain::Ps, 3),
            anchor(40),
            Position::Sequence(31),
            || Ok::<_, ()>(()),
        )
        .unwrap();
}

#[test]
fn pending_replacement_keeps_the_activation_and_other_domains_cannot_rekey_user_data() {
    let mut timeline = timeline(Scope::User(CnDomain::Cs));
    timeline
        .schedule_with(
            context(CnDomain::Cs, 1),
            anchor(10),
            Position::Sequence(0),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        timeline.schedule_with(
            context(CnDomain::Cs, 2),
            anchor(11),
            Position::Sequence(1),
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::PendingActivation))
    );
    timeline
        .schedule_with(
            context(CnDomain::Cs, 2),
            anchor(10),
            Position::Sequence(1),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(timeline.configurations(), 1);
    assert_eq!(
        timeline.schedule_with(
            context(CnDomain::Ps, 1),
            anchor(20),
            Position::Sequence(11),
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::Owner))
    );
    assert_eq!(
        timeline.schedule_with(
            context(CnDomain::Cs, 2),
            anchor(20),
            Position::Sequence(11),
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::ReusedGeneration))
    );
    timeline
        .schedule_with(
            context(CnDomain::Cs, 3),
            anchor(20),
            Position::Sequence(11),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        timeline.schedule_with(
            context(CnDomain::Cs, 4),
            anchor(30),
            Position::Sequence(21),
            || Ok::<_, ()>(())
        ),
        Err(UpdateError::Cipher(Error::Capacity))
    );
    timeline
        .retire_before(ue(), Position::Sequence(20))
        .unwrap();
    timeline
        .schedule_with(
            context(CnDomain::Cs, 4),
            anchor(30),
            Position::Sequence(21),
            || Ok::<_, ()>(()),
        )
        .unwrap();
}

#[test]
fn control_is_clear_and_bad_owner_mode_count_or_length_never_mutates_data() {
    let mut timeline = timeline(Scope::Signalling);
    timeline
        .schedule_with(
            context(CnDomain::Cs, 1),
            anchor(0),
            Position::Sequence(0),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let mut status = vec![0, 1, 2, 3];
    assert_eq!(
        timeline.apply(ue(), Position::Sequence(0), &mut status, 32),
        Ok(CipherUse::Control)
    );
    assert_eq!(status, [0, 1, 2, 3]);
    let mut bytes = data(7);
    let original = bytes.clone();
    let bits = bytes.len() * 8;
    assert_eq!(
        timeline.apply(ue(), Position::Sequence(8), &mut bytes, bits),
        Err(Error::Sequence)
    );
    assert_eq!(
        timeline.apply(
            UeKey { slot: 2, ..ue() },
            Position::Sequence(7),
            &mut bytes,
            bits
        ),
        Err(Error::Owner)
    );
    assert_eq!(
        timeline.apply(ue(), Position::Frame(7), &mut bytes, bits),
        Err(Error::Mode)
    );
    assert_eq!(
        timeline.apply(ue(), Position::Sequence(7), &mut bytes, bits - 1),
        Err(Error::Length)
    );
    assert_eq!(bytes, original);
    assert!(
        Binding::new(
            ue(),
            Scope::Signalling,
            2,
            CipherMode::Transparent,
            Direction::Uplink
        )
        .is_err()
    );
}

#[test]
fn am_and_um_use_the_independent_f8_vector_without_ciphering_the_clear_header() {
    let secret = [
        0x5a, 0xcb, 0x1d, 0x64, 0x4c, 0x0d, 0x51, 0x20, 0x4e, 0xa5, 0xf1, 0x45, 0x10, 0x10, 0xd8,
        0x52,
    ];
    let origin = KeyOrigin {
        ue: ue(),
        domain: CnDomain::Cs,
        generation: NonZeroU64::MIN,
    };
    let context = KeySet::uia1(
        origin,
        CipherMaterial::Uea1(Key::new(secret)),
        Key::new([0; 16]),
    )
    .split()
    .0;
    let plain = [
        0xad, 0x9c, 0x44, 0x1f, 0x89, 0x0b, 0x38, 0xc4, 0x57, 0xa4, 0x9d, 0x42, 0x14, 0x07, 0xe8,
    ];
    let expected = [
        0x9b, 0xc9, 0x2c, 0xa8, 0x03, 0xc6, 0x7b, 0x28, 0xa1, 0x1a, 0x4b, 0xee, 0x5a, 0x0c, 0x25,
    ];
    let count = 0xfa556b26;
    for (mode, clear) in [
        (CipherMode::Acknowledged, vec![0xd9, 0x31]),
        (CipherMode::Unacknowledged, vec![0x4d]),
    ] {
        let sequence = match mode {
            CipherMode::Acknowledged => 0xb26,
            _ => 0x26,
        };
        let binding = Binding::new(ue(), Scope::Signalling, 4, mode, Direction::Downlink).unwrap();
        let timeline = CipherTimeline::new(
            binding,
            CountAnchor::new(mode, Position::Sequence(sequence), count).unwrap(),
            Configuration::Configured(context.clone()),
        )
        .unwrap();
        let mut bytes = clear.clone();
        bytes.extend(plain);
        let bits = bytes.len() * 8;
        timeline
            .apply(ue(), Position::Sequence(sequence), &mut bytes, bits)
            .unwrap();
        assert_eq!(&bytes[..clear.len()], clear);
        assert_eq!(&bytes[clear.len()..], expected);
    }
}

#[test]
fn transparent_count_is_shared_across_bearers_and_directions_with_independent_stream_bits() {
    let context = context(CnDomain::Cs, 1);
    let domain = TransparentDomain::new(
        ue(),
        CnDomain::Cs,
        CountAnchor::from_start(
            CipherMode::Transparent,
            Position::Frame(248),
            Start::new(7).unwrap(),
        )
        .unwrap(),
        Configuration::Configured(context.clone()),
    )
    .unwrap();
    for frame in [248, 255, 256, 300] {
        let expected_count = 0x7000 + frame as u32;
        for bearer in [5, 6, 32] {
            for direction in [Direction::Uplink, Direction::Downlink] {
                let mut actual = [0xa5; 4];
                let mut expected = actual;
                context
                    .apply(
                        context.origin(),
                        expected_count,
                        bearer - 1,
                        direction,
                        &mut expected,
                        29,
                    )
                    .unwrap();
                assert_eq!(
                    domain.apply(ue(), bearer, direction, frame, &mut actual, 29),
                    Ok(CipherUse::Data {
                        origin: Some(context.origin()),
                        count: expected_count
                    })
                );
                assert_eq!(actual, expected);
                assert_eq!(actual[3] & 7, 5);
            }
        }
    }
}

#[test]
fn aggregate_budget_counts_distinct_configs_across_bearers_and_directions() {
    let cs1 = context(CnDomain::Cs, 1).origin();
    let cs2 = context(CnDomain::Cs, 2).origin();
    let cs3 = context(CnDomain::Cs, 3).origin();
    let ps1 = context(CnDomain::Ps, 1).origin();
    let mut budget = CipherBudget::new(ue());
    for _ in 0..32 {
        budget.admit(Scope::User(CnDomain::Cs), Some(cs1)).unwrap()
    }
    budget.admit(Scope::User(CnDomain::Cs), Some(cs2)).unwrap();
    assert_eq!(
        budget.admit(Scope::User(CnDomain::Cs), Some(cs3)),
        Err(Error::Capacity)
    );
    budget.admit(Scope::User(CnDomain::Ps), Some(ps1)).unwrap();
    assert_eq!(
        budget.admit(Scope::User(CnDomain::Ps), Some(cs1)),
        Err(Error::Owner)
    );
    for origin in [None, Some(cs1), Some(ps1)] {
        budget.admit(Scope::Signalling, origin).unwrap()
    }
    budget.admit(Scope::Signalling, Some(cs2)).unwrap();
    assert_eq!(
        budget.admit(Scope::Signalling, Some(cs3)),
        Err(Error::Capacity)
    );
    assert_eq!(
        budget.admit(
            Scope::Signalling,
            Some(KeyOrigin {
                ue: UeKey { slot: 9, ..ue() },
                ..cs1
            })
        ),
        Err(Error::Owner)
    );
}

#[test]
fn transparent_cipher_matches_a_partial_bit_spec_vector_and_activates_on_eight_frame_boundaries() {
    let hex = |value: &str| {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect::<Vec<_>>()
    };
    let origin = KeyOrigin {
        ue: ue(),
        domain: CnDomain::Ps,
        generation: NonZeroU64::MIN,
    };
    let key = Key::new(hex("D3C5D592327FB11C4035C6680AF8C6D1").try_into().unwrap());
    let cipher = KeySet::uia1(origin, CipherMaterial::Uea1(key), Key::new([0; 16]))
        .split()
        .0;
    let mut domain = TransparentDomain::new(
        ue(),
        CnDomain::Ps,
        CountAnchor::new(CipherMode::Transparent, Position::Frame(0), 0x398a5900).unwrap(),
        Configuration::Configured(cipher),
    )
    .unwrap();
    let mut bytes = hex("981BA6824C1BFB1AB485472029B71D808CE33E2CC3C0B5FC1F3DE8A6DC66B1F0");
    let expected = hex("5BB9431BB1E98BD11B93DB7C3D45136559BB86A295AA204ECBEBF6F7A5101512");
    assert_eq!(
        domain.apply(ue(), 6, Direction::Downlink, 0xb4, &mut bytes, 253),
        Ok(CipherUse::Data {
            origin: Some(origin),
            count: 0x398a59b4
        })
    );
    for bit in 0..253 {
        assert_eq!(
            bytes[bit / 8] & (1 << (7 - bit % 8)),
            expected[bit / 8] & (1 << (7 - bit % 8))
        )
    }
    assert_eq!(bytes[31] & 7, 0);
    let invalid = CountAnchor::from_start(
        CipherMode::Transparent,
        Position::Frame(257),
        Start::new(0).unwrap(),
    )
    .unwrap();
    assert_eq!(
        domain.schedule_with(
            context(CnDomain::Ps, 2),
            invalid,
            200,
            || -> Result<(), ()> { panic!("non-eight-frame activation was published") }
        ),
        Err(UpdateError::Cipher(Error::Position))
    );
    let activation = CountAnchor::from_start(
        CipherMode::Transparent,
        Position::Frame(256),
        Start::new(0).unwrap(),
    )
    .unwrap();
    assert_eq!(
        domain.schedule_with(context(CnDomain::Ps, 2), activation, 200, || Err::<(), _>(
            7
        )),
        Err(UpdateError::Publication(7))
    );
    assert_eq!(domain.configurations(), 1);
    let next = context(CnDomain::Ps, 2);
    domain
        .schedule_with(next.clone(), activation, 200, || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        domain.selected(ue(), 255),
        Ok(CipherUse::Data {
            origin: Some(origin),
            count: 0x398a59ff
        })
    );
    assert_eq!(
        domain.selected(ue(), 256),
        Ok(CipherUse::Data {
            origin: Some(next.origin()),
            count: 0
        })
    );
    assert_eq!(
        domain.selected(ue(), 257),
        Ok(CipherUse::Data {
            origin: Some(next.origin()),
            count: 1
        })
    );
    domain.retire_before(ue(), 256).unwrap();
    assert_eq!(domain.selected(ue(), 255), Err(Error::Position));
}

#[test]
fn rekey_cannot_leave_a_count_exhaustion_gap_before_its_activation() {
    let binding = Binding::new(
        ue(),
        Scope::Signalling,
        2,
        CipherMode::Acknowledged,
        Direction::Downlink,
    )
    .unwrap();
    let old = context(CnDomain::Cs, 1);
    let mut timeline = CipherTimeline::new(
        binding,
        CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(4095), u32::MAX).unwrap(),
        Configuration::Configured(old.clone()),
    )
    .unwrap();
    assert_eq!(
        timeline.schedule_with(
            context(CnDomain::Cs, 2),
            anchor(4097),
            Position::Sequence(4096),
            || -> Result<(), ()> { panic!("activation beyond old COUNT exhaustion was published") }
        ),
        Err(UpdateError::Cipher(Error::Exhausted))
    );
    assert_eq!(timeline.configurations(), 1);
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(4095)),
        Ok(CipherUse::Data {
            origin: Some(old.origin()),
            count: u32::MAX
        })
    );
    let new = context(CnDomain::Cs, 2);
    timeline
        .schedule_with(new.clone(), anchor(4096), Position::Sequence(4096), || {
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(4096)),
        Ok(CipherUse::Data {
            origin: Some(new.origin()),
            count: 0
        })
    );
}

#[test]
fn admitted_reset_selects_an_explicit_retained_key_without_reusing_old_transport_positions() {
    let mut timeline = timeline(Scope::Signalling);
    let old = context(CnDomain::Cs, 1);
    let pending = context(CnDomain::Ps, 1);
    timeline
        .schedule_with(old, anchor(0), Position::Sequence(0), || Ok::<_, ()>(()))
        .unwrap();
    timeline
        .schedule_with(
            pending.clone(),
            anchor(4098),
            Position::Sequence(4095),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let reset = CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(4096), 0).unwrap();
    assert_eq!(
        timeline.reset_with(ue(), Some(pending.origin()), reset, || Err::<(), _>(7)),
        Err(UpdateError::Publication(7))
    );
    assert_eq!(timeline.configurations(), 2);
    timeline
        .reset_with(ue(), Some(pending.origin()), reset, || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(timeline.configurations(), 1);
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(4095)),
        Err(Error::Position)
    );
    assert_eq!(
        timeline.selected(ue(), Position::Sequence(4096)),
        Ok(CipherUse::Data {
            origin: Some(pending.origin()),
            count: 0
        })
    );
    assert_eq!(
        timeline.reset_with(ue(), None, reset, || Ok::<_, ()>(())),
        Err(UpdateError::Cipher(Error::Owner))
    );
}
