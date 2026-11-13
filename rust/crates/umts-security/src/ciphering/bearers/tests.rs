// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    Key,
    contexts::{CipherMaterial, KeySet},
    counters::Start,
};
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

mod added;
mod unregister;

fn ue() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 1,
        generation: NonZeroU64::MIN,
    }
}
fn context(domain: CnDomain, generation: u64) -> CipherContext {
    KeySet::uia1(
        KeyOrigin {
            ue: ue(),
            domain,
            generation: NonZeroU64::new(generation).unwrap(),
        },
        CipherMaterial::Uea1(Key::new([generation as u8; 16])),
        Key::new([0; 16]),
    )
    .split()
    .0
}
fn anchor(mode: CipherMode, at: u64) -> CountAnchor {
    CountAnchor::from_start(
        mode,
        if mode == CipherMode::Transparent {
            Position::Frame(at)
        } else {
            Position::Sequence(at)
        },
        Start::new(0).unwrap(),
    )
    .unwrap()
}
fn register(bank: &mut BearerCiphers, bearer: u8, scope: Scope, mode: CipherMode) {
    bank.register_with(
        BearerCipherConfiguration {
            bearer,
            scope,
            transmit: anchor(mode, 0),
            receive: anchor(mode, 0),
            initial: Configuration::Unprotected,
        },
        || Ok::<_, ()>(()),
    )
    .unwrap();
}
fn activation(bearer: u8, direction: Direction, at: u64) -> CipherActivation {
    CipherActivation::Sequenced {
        bearer,
        direction,
        anchor: anchor(CipherMode::Acknowledged, at),
        first_new: Position::Sequence(0),
    }
}
fn selected(bank: &BearerCiphers, bearer: u8, direction: Direction, at: u64) -> CipherUse {
    bank.selected(ue(), bearer, direction, Position::Sequence(at))
        .unwrap()
}
fn bank() -> BearerCiphers {
    BearerCiphers::new(ue(), NonZeroUsize::new(8).unwrap()).unwrap()
}

fn user_configuration(
    bearer: u8,
    domain: CnDomain,
    anchor: CountAnchor,
    context: Option<CipherContext>,
) -> BearerCipherConfiguration {
    BearerCipherConfiguration {
        bearer,
        scope: Scope::User(domain),
        transmit: anchor,
        receive: anchor,
        initial: context
            .map(Configuration::Configured)
            .unwrap_or(Configuration::Unprotected),
    }
}

#[test]
fn initial_registration_and_transparent_counter_activation_publish_as_one_transaction() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    let configs = || {
        [user_configuration(
            5,
            CnDomain::Cs,
            CountAnchor::transparent_frozen_hfn(250, Start::new(3).unwrap()).unwrap(),
            Some(context(CnDomain::Cs, 1)),
        )]
    };
    let update = |boundary| TransparentCounterActivation {
        domain: CnDomain::Cs,
        origin: Some(context(CnDomain::Cs, 1).origin()),
        anchor: CountAnchor::from_start(
            CipherMode::Transparent,
            Position::Frame(boundary),
            Start::new(9).unwrap(),
        )
        .unwrap(),
        first_frame: 250,
    };
    assert_eq!(
        bank.register_initial_with(configs(), &[update(520)], || Err::<(), _>(7)),
        Err(BearerCipherUpdateError::Publication(7))
    );
    assert_eq!(bank.len(), 1);
    assert!(!bank.has_user_bearers(ue()).unwrap());
    assert!(
        bank.register_initial_with(configs(), &[update(519)], || -> Result<(), ()> {
            panic!("invalid CFN must not publish")
        })
        .is_err()
    );
    assert_eq!(bank.len(), 1);
    assert!(
        bank.register_initial_with(
            configs(),
            &[update(520), update(520)],
            || -> Result<(), ()> { panic!("duplicate domain must not publish") }
        )
        .is_err()
    );
    assert_eq!(bank.len(), 1);
    bank.register_initial_with(configs(), &[update(520)], || Ok::<_, ()>(()))
        .unwrap();
    assert!(bank.has_user_bearers(ue()).unwrap());
    for direction in [Direction::Uplink, Direction::Downlink] {
        for (frame, count) in [(519, 0x3007), (520, 0x9008), (768, 0x9100)] {
            assert_eq!(
                bank.selected(ue(), 5, direction, Position::Frame(frame)),
                Ok(CipherUse::Data {
                    origin: Some(context(CnDomain::Cs, 1).origin()),
                    count,
                })
            );
        }
    }
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 8),
        CipherUse::Data {
            origin: None,
            count: 8
        }
    );
}

#[test]
fn multi_bearer_registration_commits_only_after_all_validation_and_publication() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    let configs = || {
        [
            user_configuration(
                5,
                CnDomain::Cs,
                CountAnchor::transparent_frozen_hfn(250, Start::new(3).unwrap()).unwrap(),
                Some(context(CnDomain::Cs, 1)),
            ),
            user_configuration(
                6,
                CnDomain::Cs,
                CountAnchor::transparent_frozen_hfn(256, Start::new(3).unwrap()).unwrap(),
                Some(context(CnDomain::Cs, 1)),
            ),
            user_configuration(
                7,
                CnDomain::Ps,
                anchor(CipherMode::Acknowledged, 0),
                Some(context(CnDomain::Ps, 2)),
            ),
        ]
    };
    assert_eq!(
        bank.register_batch_with(configs(), || Err::<(), _>("full")),
        Err(BearerCipherUpdateError::Publication("full"))
    );
    assert_eq!(bank.len(), 1);
    assert!(bank.layout(ue(), 5).is_err());
    bank.register_batch_with(configs(), || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(bank.len(), 4);
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 8),
        CipherUse::Data {
            origin: None,
            count: 8
        }
    );
    let activation = CountAnchor::from_start(
        CipherMode::Transparent,
        Position::Frame(520),
        Start::new(9).unwrap(),
    )
    .unwrap();
    bank.activate_transparent_counter_with(
        ue(),
        CnDomain::Cs,
        Some(context(CnDomain::Cs, 1).origin()),
        activation,
        256,
        || Err::<(), _>("full"),
    )
    .unwrap_err();
    assert_eq!(
        bank.selected(ue(), 5, Direction::Uplink, Position::Frame(520)),
        Ok(CipherUse::Data {
            origin: Some(context(CnDomain::Cs, 1).origin()),
            count: 0x3008
        })
    );
    bank.activate_transparent_counter_with(
        ue(),
        CnDomain::Cs,
        Some(context(CnDomain::Cs, 1).origin()),
        activation,
        256,
        || Ok::<_, ()>(()),
    )
    .unwrap();
    for bearer in [5, 6] {
        for direction in [Direction::Downlink, Direction::Uplink] {
            assert_eq!(
                bank.selected(ue(), bearer, direction, Position::Frame(768)),
                Ok(CipherUse::Data {
                    origin: Some(context(CnDomain::Cs, 1).origin()),
                    count: 0x9100
                })
            );
        }
    }
    assert_eq!(
        selected(&bank, 7, Direction::Uplink, 8),
        CipherUse::Data {
            origin: Some(context(CnDomain::Ps, 2).origin()),
            count: 8
        }
    );
}

#[test]
fn multi_bearer_registration_rejects_duplicates_limits_and_inconsistent_domain_counters_atomically()
{
    let mut bank = BearerCiphers::new(ue(), NonZeroUsize::new(2).unwrap()).unwrap();
    let config = |bearer| {
        user_configuration(
            bearer,
            CnDomain::Cs,
            anchor(CipherMode::Transparent, 0),
            None,
        )
    };
    for (configs, error) in [
        (vec![config(5), config(5)], BearerCipherError::Duplicate),
        (
            vec![config(5), config(6), config(7)],
            BearerCipherError::Capacity,
        ),
        (
            vec![
                config(5),
                user_configuration(
                    6,
                    CnDomain::Cs,
                    CountAnchor::transparent_frozen_hfn(0, Start::new(0).unwrap()).unwrap(),
                    None,
                ),
            ],
            BearerCipherError::Layout,
        ),
        (
            vec![
                config(5),
                user_configuration(
                    6,
                    CnDomain::Cs,
                    anchor(CipherMode::Transparent, 0),
                    Some(context(CnDomain::Cs, 1)),
                ),
            ],
            BearerCipherError::Layout,
        ),
        (vec![], BearerCipherError::Layout),
    ] {
        assert_eq!(
            bank.register_batch_with(configs, || -> Result<(), ()> {
                panic!("invalid registration published")
            }),
            Err(BearerCipherUpdateError::Cipher(error))
        );
        assert!(bank.is_empty());
    }
    bank.register_batch_with([config(5), config(6)], || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(bank.len(), 2);
}

#[test]
fn batch_publication_is_atomic_across_bearers_and_directions() {
    let mut bank = bank();
    for bearer in [2, 3] {
        register(
            &mut bank,
            bearer,
            Scope::Signalling,
            CipherMode::Acknowledged,
        );
    }
    let key = context(CnDomain::Ps, 1);
    let changes = [
        activation(2, Direction::Downlink, 8),
        activation(2, Direction::Uplink, 11),
        activation(3, Direction::Downlink, 9),
    ];
    assert_eq!(
        bank.update_with(key.clone(), &changes, || Err::<(), _>(7)),
        Err(BearerCipherUpdateError::Publication(7))
    );
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 8),
        CipherUse::Data {
            origin: None,
            count: 8
        }
    );
    bank.update_with(key.clone(), &changes, || Ok::<_, ()>(()))
        .unwrap();
    for (bearer, direction, at) in [
        (2, Direction::Downlink, 8),
        (2, Direction::Uplink, 11),
        (3, Direction::Downlink, 9),
    ] {
        assert_eq!(
            selected(&bank, bearer, direction, at),
            CipherUse::Data {
                origin: Some(key.origin()),
                count: at as u32
            }
        );
        assert_eq!(
            selected(&bank, bearer, direction, at - 1),
            CipherUse::Data {
                origin: None,
                count: at as u32 - 1
            }
        );
    }
    let new = context(CnDomain::Cs, 1);
    let invalid = [
        activation(2, Direction::Downlink, 16),
        activation(31, Direction::Uplink, 16),
    ];
    assert!(
        bank.update_with(new, &invalid, || -> Result<(), ()> {
            panic!("invalid batch published")
        })
        .is_err()
    );
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 16),
        CipherUse::Data {
            origin: Some(key.origin()),
            count: 16
        }
    );
    let duplicate = [activation(2, Direction::Downlink, 8); 2];
    assert_eq!(
        bank.update_with(context(CnDomain::Cs, 2), &duplicate, || Ok::<_, ()>(())),
        Err(BearerCipherUpdateError::Cipher(
            BearerCipherError::Duplicate
        ))
    );
}

#[test]
fn aggregate_configuration_budget_cannot_be_evaded_by_using_different_bearers() {
    let mut bank = bank();
    for bearer in 1..=4 {
        register(
            &mut bank,
            bearer,
            Scope::Signalling,
            if bearer == 1 {
                CipherMode::Unacknowledged
            } else {
                CipherMode::Acknowledged
            },
        );
    }
    for (bearer, domain, generation) in [
        (1, CnDomain::Cs, 1),
        (2, CnDomain::Ps, 1),
        (3, CnDomain::Cs, 2),
    ] {
        let mode = if bearer == 1 {
            CipherMode::Unacknowledged
        } else {
            CipherMode::Acknowledged
        };
        bank.update_with(
            context(domain, generation),
            &[CipherActivation::Sequenced {
                bearer,
                direction: Direction::Downlink,
                anchor: anchor(mode, 0),
                first_new: Position::Sequence(0),
            }],
            || Ok::<_, ()>(()),
        )
        .unwrap();
    }
    assert_eq!(
        bank.update_with(
            context(CnDomain::Ps, 2),
            &[activation(4, Direction::Downlink, 0)],
            || -> Result<(), ()> { panic!("fourth signaling key escaped budget") }
        ),
        Err(BearerCipherUpdateError::Cipher(BearerCipherError::Cipher(
            Error::Capacity
        )))
    );
    assert_eq!(
        selected(&bank, 4, Direction::Downlink, 0),
        CipherUse::Data {
            origin: None,
            count: 0
        }
    );
}

#[test]
fn transparent_bearers_share_one_domain_count_across_bearers_and_directions() {
    let mut bank = bank();
    register(
        &mut bank,
        5,
        Scope::User(CnDomain::Cs),
        CipherMode::Transparent,
    );
    bank.register_with(
        BearerCipherConfiguration {
            bearer: 6,
            scope: Scope::User(CnDomain::Cs),
            transmit: anchor(CipherMode::Transparent, 8),
            receive: anchor(CipherMode::Transparent, 8),
            initial: Configuration::Unprotected,
        },
        || Ok::<_, ()>(()),
    )
    .unwrap();
    register(
        &mut bank,
        7,
        Scope::User(CnDomain::Ps),
        CipherMode::Transparent,
    );
    let cs = context(CnDomain::Cs, 1);
    bank.update_with(
        cs.clone(),
        &[CipherActivation::Transparent {
            domain: CnDomain::Cs,
            anchor: anchor(CipherMode::Transparent, 16),
            first_frame: 0,
        }],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    for bearer in [5, 6] {
        for direction in [Direction::Downlink, Direction::Uplink] {
            assert_eq!(
                bank.selected(ue(), bearer, direction, Position::Frame(16)),
                Ok(CipherUse::Data {
                    origin: Some(cs.origin()),
                    count: 16
                })
            );
            let original = [0x11, 0x20];
            let mut bytes = original;
            bank.apply(ue(), bearer, direction, Position::Frame(16), &mut bytes, 12)
                .unwrap();
            let mut expected = original;
            cs.apply(cs.origin(), 16, bearer - 1, direction, &mut expected, 12)
                .unwrap();
            assert_eq!(bytes, expected);
        }
    }
    assert_eq!(
        bank.selected(ue(), 7, Direction::Uplink, Position::Frame(16)),
        Ok(CipherUse::Data {
            origin: None,
            count: 16
        })
    );
    assert_eq!(
        bank.selected(ue(), 6, Direction::Uplink, Position::Frame(7)),
        Err(BearerCipherError::Cipher(Error::Position))
    );
    assert!(
        bank.update_with(
            context(CnDomain::Ps, 1),
            &[CipherActivation::Transparent {
                domain: CnDomain::Cs,
                anchor: anchor(CipherMode::Transparent, 24),
                first_frame: 16
            }],
            || -> Result<(), ()> { panic!("peer-domain key admitted") }
        )
        .is_err()
    );
}

#[test]
fn am_reset_rebases_both_directions_only_after_actual_reset_admission() {
    let mut bank = bank();
    for bearer in [2, 3] {
        register(
            &mut bank,
            bearer,
            Scope::Signalling,
            CipherMode::Acknowledged,
        );
    }
    let first = context(CnDomain::Cs, 1);
    bank.update_with(
        first.clone(),
        &[
            activation(2, Direction::Downlink, 0),
            activation(2, Direction::Uplink, 0),
        ],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let pending = context(CnDomain::Cs, 2);
    bank.update_with(
        pending.clone(),
        &[
            CipherActivation::Sequenced {
                bearer: 2,
                direction: Direction::Downlink,
                anchor: anchor(CipherMode::Acknowledged, 20),
                first_new: Position::Sequence(1),
            },
            CipherActivation::Sequenced {
                bearer: 2,
                direction: Direction::Uplink,
                anchor: anchor(CipherMode::Acknowledged, 21),
                first_new: Position::Sequence(1),
            },
        ],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let reset = CipherReset {
        bearer: 2,
        origin: Some(pending.origin()),
        transmit: CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(4096), 3 << 12)
            .unwrap(),
        receive: CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(4096), 4 << 12)
            .unwrap(),
    };
    assert_eq!(
        bank.reset_with(ue(), reset, || Err::<(), _>(7)),
        Err(BearerCipherUpdateError::Publication(7))
    );
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 10),
        CipherUse::Data {
            origin: Some(first.origin()),
            count: 10
        }
    );
    bank.reset_with(ue(), reset, || Ok::<_, ()>(())).unwrap();
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 4096),
        CipherUse::Data {
            origin: Some(pending.origin()),
            count: 3 << 12
        }
    );
    assert_eq!(
        selected(&bank, 2, Direction::Uplink, 4096),
        CipherUse::Data {
            origin: Some(pending.origin()),
            count: 4 << 12
        }
    );
    assert_eq!(
        selected(&bank, 3, Direction::Downlink, 0),
        CipherUse::Data {
            origin: None,
            count: 0
        }
    );
    assert_eq!(
        bank.selected(ue(), 2, Direction::Downlink, Position::Sequence(4095)),
        Err(BearerCipherError::Cipher(Error::Position))
    );
}

#[test]
fn registration_identity_and_staging_capacity_are_bounded() {
    assert!(matches!(
        BearerCiphers::new(ue(), NonZeroUsize::new(33).unwrap()),
        Err(BearerCipherError::Capacity)
    ));
    let mut bank = bank();
    for bearer in 1..=8 {
        register(
            &mut bank,
            bearer,
            if bearer < 5 {
                Scope::Signalling
            } else {
                Scope::User(CnDomain::Ps)
            },
            if bearer == 1 {
                CipherMode::Unacknowledged
            } else {
                CipherMode::Acknowledged
            },
        );
        assert_eq!(bank.tables.channels.capacity(), 8);
    }
    assert_eq!(bank.len(), 8);
    assert_eq!(
        bank.register_with(
            BearerCipherConfiguration {
                bearer: 9,
                scope: Scope::User(CnDomain::Cs),
                transmit: anchor(CipherMode::Acknowledged, 0),
                receive: anchor(CipherMode::Acknowledged, 0),
                initial: Configuration::Unprotected
            },
            || -> Result<(), ()> { panic!("capacity failure published") }
        ),
        Err(BearerCipherUpdateError::Cipher(BearerCipherError::Capacity))
    );
    let foreign = UeKey {
        generation: NonZeroU64::new(2).unwrap(),
        ..ue()
    };
    assert_eq!(
        bank.selected(foreign, 2, Direction::Downlink, Position::Sequence(0)),
        Err(BearerCipherError::Cipher(Error::Owner))
    );
    let mut bytes = [0x80, 0, 1, 2];
    assert_eq!(
        bank.apply(
            foreign,
            2,
            Direction::Downlink,
            Position::Sequence(0),
            &mut bytes,
            32
        ),
        Err(BearerCipherError::Cipher(Error::Owner))
    );
    assert_eq!(bytes, [0x80, 0, 1, 2]);
}

#[test]
fn retirement_is_atomic_and_old_retransmission_maps_survive_other_bearer_updates() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    let key = context(CnDomain::Cs, 1);
    bank.update_with(
        key.clone(),
        &[
            activation(2, Direction::Downlink, 8),
            activation(2, Direction::Uplink, 8),
        ],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    assert!(
        bank.retire(
            ue(),
            &[
                CipherRetirement::Sequenced {
                    bearer: 2,
                    direction: Direction::Downlink,
                    floor: Position::Sequence(8)
                },
                CipherRetirement::Sequenced {
                    bearer: 31,
                    direction: Direction::Uplink,
                    floor: Position::Sequence(8)
                }
            ]
        )
        .is_err()
    );
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 7),
        CipherUse::Data {
            origin: None,
            count: 7
        }
    );
    bank.retire(
        ue(),
        &[CipherRetirement::Sequenced {
            bearer: 2,
            direction: Direction::Downlink,
            floor: Position::Sequence(8),
        }],
    )
    .unwrap();
    assert_eq!(
        bank.selected(ue(), 2, Direction::Downlink, Position::Sequence(7)),
        Err(BearerCipherError::Cipher(Error::Position))
    );
    assert_eq!(
        selected(&bank, 2, Direction::Uplink, 7),
        CipherUse::Data {
            origin: None,
            count: 7
        }
    );
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 9),
        CipherUse::Data {
            origin: Some(key.origin()),
            count: 9
        }
    );
}

#[test]
fn pending_activation_queries_are_read_only_and_do_not_evaluate_an_exhausted_old_count() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    bank.update_with(
        context(CnDomain::Cs, 1),
        &[activation(2, Direction::Downlink, 12)],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            bank.pending_sequence_activation(ue(), 2, Direction::Downlink, Position::Sequence(0)),
            Ok(Some(Position::Sequence(12)))
        );
    }
    assert_eq!(
        bank.pending_sequence_activation(ue(), 2, Direction::Downlink, Position::Sequence(12)),
        Ok(None)
    );
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 0),
        CipherUse::Data {
            origin: None,
            count: 0
        }
    );
    assert!(
        bank.pending_sequence_activation(ue(), 2, Direction::Downlink, Position::Frame(0))
            .is_err()
    );
    let mut exhausted = BearerCiphers::new(ue(), std::num::NonZeroUsize::MIN).unwrap();
    let anchor = CountAnchor::new(
        CipherMode::Acknowledged,
        Position::Sequence(0),
        !((1_u32 << CipherMode::Acknowledged.sequence_bits()) - 1),
    )
    .unwrap();
    exhausted
        .register_with(
            BearerCipherConfiguration {
                bearer: 2,
                scope: Scope::Signalling,
                transmit: anchor,
                receive: anchor,
                initial: Configuration::Unprotected,
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert!(
        exhausted
            .selected(ue(), 2, Direction::Downlink, Position::Sequence(4096))
            .is_err()
    );
    assert_eq!(
        exhausted.pending_sequence_activation(
            ue(),
            2,
            Direction::Downlink,
            Position::Sequence(4096)
        ),
        Ok(None)
    );
}

#[test]
fn repeated_cipher_rekeys_keep_unacknowledged_am_maps_and_collect_only_settled_windows() {
    let mut bank = bank();
    for (bearer, mode) in [
        (1, CipherMode::Unacknowledged),
        (2, CipherMode::Acknowledged),
        (3, CipherMode::Acknowledged),
    ] {
        register(&mut bank, bearer, Scope::Signalling, mode);
    }
    let mut previous = None;
    for generation in 1..=24 {
        let start = (generation - 1) * 8;
        let current = context(
            if generation % 2 == 0 {
                CnDomain::Ps
            } else {
                CnDomain::Cs
            },
            generation,
        );
        let mut updates = Vec::new();
        for (bearer, mode) in [
            (1, CipherMode::Unacknowledged),
            (2, CipherMode::Acknowledged),
            (3, CipherMode::Acknowledged),
        ] {
            for direction in [Direction::Downlink, Direction::Uplink] {
                let at = if bearer == 1 {
                    0
                } else if bearer == 2 {
                    start + 2
                } else {
                    start
                };
                updates.push(CipherActivation::Sequenced {
                    bearer,
                    direction,
                    anchor: anchor(mode, at),
                    first_new: Position::Sequence(if bearer == 1 { 0 } else { start }),
                });
            }
        }
        bank.update_with(current.clone(), &updates, || Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(
            selected(&bank, 2, Direction::Downlink, start),
            CipherUse::Data {
                origin: previous,
                count: start as u32
            }
        );
        let floors = |downlink: u64| {
            let mut result = Vec::new();
            for bearer in 1..=3 {
                for direction in [Direction::Downlink, Direction::Uplink] {
                    result.push(CipherRetirement::Sequenced {
                        bearer,
                        direction,
                        floor: Position::Sequence(if bearer == 1 {
                            0
                        } else if bearer == 2 {
                            if direction == Direction::Downlink {
                                downlink
                            } else {
                                start + 2
                            }
                        } else {
                            start
                        }),
                    });
                }
            }
            result
        };
        bank.retire(ue(), &floors(start)).unwrap();
        assert_eq!(
            selected(&bank, 2, Direction::Downlink, start),
            CipherUse::Data {
                origin: previous,
                count: start as u32
            }
        );
        bank.retire(ue(), &floors(start + 2)).unwrap();
        assert_eq!(
            bank.selected(ue(), 2, Direction::Downlink, Position::Sequence(start)),
            Err(BearerCipherError::Cipher(Error::Position))
        );
        assert_eq!(
            selected(&bank, 2, Direction::Downlink, start + 2),
            CipherUse::Data {
                origin: Some(current.origin()),
                count: (start + 2) as u32
            }
        );
        previous = Some(current.origin());
    }
}
