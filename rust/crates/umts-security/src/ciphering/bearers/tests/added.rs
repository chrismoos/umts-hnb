// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

fn initialized(
    bearer: u8,
    domain: CnDomain,
    mode: CipherMode,
    start: u32,
) -> AddedBearerCipherConfiguration {
    AddedBearerCipherConfiguration::Initialized(user_configuration(
        bearer,
        domain,
        CountAnchor::from_start(mode, Position::Sequence(0), Start::new(start).unwrap()).unwrap(),
        Some(context(domain, 1)),
    ))
}

fn join(
    bearer: u8,
    domain: CnDomain,
    generation: Option<u64>,
    first_frame: u64,
) -> AddedBearerCipherConfiguration {
    AddedBearerCipherConfiguration::ExistingTransparent(ExistingTransparentBearer {
        bearer,
        domain,
        origin: generation.map(|generation| context(domain, generation).origin()),
        first_frame,
    })
}

fn first_tm(bearer: u8, protected: bool, start: u32, first: u64) -> AddedBearerCipherConfiguration {
    let start = Start::new(start).unwrap();
    let anchor = if protected {
        CountAnchor::transparent_frozen_hfn(first, start).unwrap()
    } else {
        CountAnchor::transparent_stopped(first, start).unwrap()
    };
    AddedBearerCipherConfiguration::Initialized(user_configuration(
        bearer,
        CnDomain::Cs,
        anchor,
        protected.then(|| context(CnDomain::Cs, 1)),
    ))
}

fn counter(protected: bool, first: u64, at: u64, start: u32) -> TransparentCounterActivation {
    let start = Start::new(start).unwrap();
    TransparentCounterActivation {
        domain: CnDomain::Cs,
        origin: protected.then(|| context(CnDomain::Cs, 1).origin()),
        anchor: if protected {
            CountAnchor::from_start(CipherMode::Transparent, Position::Frame(at), start).unwrap()
        } else {
            CountAnchor::transparent_stopped(at, start).unwrap()
        },
        first_frame: first,
    }
}

fn with_tm() -> BearerCiphers {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    bank.register_added_with(
        ue(),
        [first_tm(5, true, 3, 250)],
        &[counter(true, 250, 520, 9)],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    bank
}

fn tm_selected(bank: &BearerCiphers, bearer: u8, frame: u64) -> CipherUse {
    bank.selected(ue(), bearer, Direction::Uplink, Position::Frame(frame))
        .unwrap()
}

#[test]
fn added_am_and_um_use_new_start_without_mutating_existing_directional_epochs() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    bank.register_added_with(
        ue(),
        [initialized(5, CnDomain::Ps, CipherMode::Acknowledged, 1)],
        &[],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let next = context(CnDomain::Ps, 2);
    let activation = |direction| CipherActivation::Sequenced {
        bearer: 5,
        direction,
        anchor: CountAnchor::from_start(
            CipherMode::Acknowledged,
            Position::Sequence(4096),
            Start::new(2).unwrap(),
        )
        .unwrap(),
        first_new: Position::Sequence(0),
    };
    bank.update_with(
        next,
        &[
            activation(Direction::Uplink),
            activation(Direction::Downlink),
        ],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let before = [
        selected(&bank, 5, Direction::Uplink, 4095),
        selected(&bank, 5, Direction::Downlink, 4097),
    ];
    let mut ciphertext = [0x80, 0x08, 0x5a, 0xa5];
    bank.apply(
        ue(),
        5,
        Direction::Downlink,
        Position::Sequence(4097),
        &mut ciphertext,
        32,
    )
    .unwrap();
    let additions = || [initialized(6, CnDomain::Ps, CipherMode::Unacknowledged, 9)];
    assert_eq!(
        bank.register_added_with(ue(), additions(), &[], || Err::<(), _>("full")),
        Err(BearerCipherUpdateError::Publication("full"))
    );
    assert_eq!(bank.len(), 2);
    assert!(bank.layout(ue(), 6).is_err());
    bank.register_added_with(ue(), additions(), &[], || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(bank.len(), 3);
    assert_eq!(
        before,
        [
            selected(&bank, 5, Direction::Uplink, 4095),
            selected(&bank, 5, Direction::Downlink, 4097)
        ]
    );
    let mut after = [0x80, 0x08, 0x5a, 0xa5];
    bank.apply(
        ue(),
        5,
        Direction::Downlink,
        Position::Sequence(4097),
        &mut after,
        32,
    )
    .unwrap();
    assert_eq!(after, ciphertext);
    assert_eq!(
        bank.pending_sequence_activation(ue(), 5, Direction::Uplink, Position::Sequence(12))
            .unwrap(),
        Some(Position::Sequence(4096))
    );
    for direction in [Direction::Uplink, Direction::Downlink] {
        assert_eq!(
            selected(&bank, 6, direction, 0),
            CipherUse::Data {
                origin: Some(context(CnDomain::Ps, 1).origin()),
                count: Start::new(9).unwrap().initial_count(),
            }
        );
    }
    assert_eq!(
        selected(&bank, 2, Direction::Downlink, 40),
        CipherUse::Data {
            origin: None,
            count: 40
        }
    );
}

#[test]
fn added_tm_joins_original_domain_history_and_future_key_activation_without_new_start() {
    let mut bank = with_tm();
    bank.update_with(
        context(CnDomain::Cs, 2),
        &[CipherActivation::Transparent {
            domain: CnDomain::Cs,
            anchor: CountAnchor::from_start(
                CipherMode::Transparent,
                Position::Frame(800),
                Start::new(11).unwrap(),
            )
            .unwrap(),
            first_frame: 600,
        }],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let before = [
        tm_selected(&bank, 5, 519),
        tm_selected(&bank, 5, 520),
        tm_selected(&bank, 5, 799),
        tm_selected(&bank, 5, 800),
    ];
    assert_eq!(
        bank.register_added_with(ue(), [join(6, CnDomain::Cs, Some(1), 768)], &[], || Err::<
            (),
            _,
        >(
            7
        )),
        Err(BearerCipherUpdateError::Publication(7))
    );
    assert_eq!(bank.len(), 2);
    assert!(bank.layout(ue(), 6).is_err());
    bank.register_added_with(ue(), [join(6, CnDomain::Cs, Some(1), 768)], &[], || {
        Ok::<_, ()>(())
    })
    .unwrap();
    assert_eq!(
        before,
        [
            tm_selected(&bank, 5, 519),
            tm_selected(&bank, 5, 520),
            tm_selected(&bank, 5, 799),
            tm_selected(&bank, 5, 800)
        ]
    );
    for direction in [Direction::Uplink, Direction::Downlink] {
        for frame in [768, 799, 800, 1024] {
            assert_eq!(
                bank.selected(ue(), 6, direction, Position::Frame(frame)),
                bank.selected(ue(), 5, direction, Position::Frame(frame))
            );
        }
    }
    assert!(
        bank.selected(ue(), 6, Direction::Uplink, Position::Frame(767))
            .is_err()
    );
    assert_eq!(
        bank.tables.transparent[domain_index(CnDomain::Cs)]
            .as_ref()
            .unwrap()
            .configurations(),
        2
    );
}

#[test]
fn first_tm_domain_requires_response_counter_and_commits_multiple_new_bearers_atomically() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    let additions = || {
        [
            first_tm(5, true, 3, 250),
            first_tm(6, true, 3, 256),
            initialized(7, CnDomain::Ps, CipherMode::Acknowledged, 4),
        ]
    };
    assert!(!bank.has_transparent_domain(ue(), CnDomain::Cs).unwrap());
    assert!(
        bank.register_added_with(ue(), additions(), &[], || -> Result<(), ()> {
            panic!("missing authenticated counter must not publish")
        })
        .is_err()
    );
    assert!(
        bank.register_added_with(
            ue(),
            additions(),
            &[counter(true, 250, 520, 9), counter(true, 250, 520, 9)],
            || -> Result<(), ()> { panic!("duplicate domain counter must not publish") }
        )
        .is_err()
    );
    assert_eq!(bank.len(), 1);
    assert_eq!(
        bank.register_added_with(ue(), additions(), &[counter(true, 250, 520, 9)], || Err::<
            (),
            _,
        >(
            "full"
        )),
        Err(BearerCipherUpdateError::Publication("full"))
    );
    assert_eq!(bank.len(), 1);
    assert!(!bank.has_transparent_domain(ue(), CnDomain::Cs).unwrap());
    bank.register_added_with(ue(), additions(), &[counter(true, 250, 520, 9)], || {
        Ok::<_, ()>(())
    })
    .unwrap();
    assert_eq!(bank.len(), 4);
    assert!(bank.has_transparent_domain(ue(), CnDomain::Cs).unwrap());
    assert!(!bank.has_transparent_domain(ue(), CnDomain::Ps).unwrap());
    assert_eq!(
        tm_selected(&bank, 5, 519),
        CipherUse::Data {
            origin: Some(context(CnDomain::Cs, 1).origin()),
            count: 0x3007
        }
    );
    assert_eq!(
        tm_selected(&bank, 6, 520),
        CipherUse::Data {
            origin: Some(context(CnDomain::Cs, 1).origin()),
            count: 0x9008
        }
    );
    assert_eq!(
        selected(&bank, 2, Direction::Uplink, 20),
        CipherUse::Data {
            origin: None,
            count: 20
        }
    );
}

#[test]
fn stopped_unprotected_domain_remains_stopped_when_another_tm_bearer_joins() {
    let mut bank = bank();
    bank.register_added_with(
        ue(),
        [first_tm(5, false, 3, 250)],
        &[counter(false, 250, 520, 9)],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    bank.register_added_with(ue(), [join(6, CnDomain::Cs, None, 768)], &[], || {
        Ok::<_, ()>(())
    })
    .unwrap();
    for bearer in [5, 6] {
        for frame in [768, 1024, 4096] {
            assert_eq!(
                tm_selected(&bank, bearer, frame),
                CipherUse::Data {
                    origin: None,
                    count: 0x9008
                }
            );
        }
    }
}

#[test]
fn existing_tm_domain_rejects_reinitialization_counter_reset_wrong_origin_and_retired_frame() {
    let mut bank = with_tm();
    let before = tm_selected(&bank, 5, 900);
    assert!(
        bank.register_added_with(
            ue(),
            [first_tm(6, true, 13, 768)],
            &[counter(true, 768, 1024, 14)],
            || -> Result<(), ()> { panic!("existing domain cannot reset") }
        )
        .is_err()
    );
    assert!(
        bank.register_added_with(
            ue(),
            [join(6, CnDomain::Cs, Some(1), 768)],
            &[counter(true, 768, 1024, 14)],
            || -> Result<(), ()> { panic!("existing counter update cannot publish") }
        )
        .is_err()
    );
    assert!(
        bank.register_added_with(
            ue(),
            [join(6, CnDomain::Cs, Some(2), 768)],
            &[],
            || -> Result<(), ()> { panic!("wrong key origin cannot join") }
        )
        .is_err()
    );
    assert_eq!(bank.len(), 2);
    assert_eq!(tm_selected(&bank, 5, 900), before);
    bank.retire(
        ue(),
        &[CipherRetirement::Transparent {
            domain: CnDomain::Cs,
            first_frame: 760,
        }],
    )
    .unwrap();
    assert!(
        bank.register_added_with(
            ue(),
            [join(6, CnDomain::Cs, Some(1), 759)],
            &[],
            || -> Result<(), ()> { panic!("retired history cannot join") }
        )
        .is_err()
    );
    assert_eq!(bank.len(), 2);
    assert_eq!(tm_selected(&bank, 5, 900), before);
}

#[test]
fn removal_of_last_tm_bearer_requires_fresh_first_domain_initialization_not_stale_join() {
    let mut bank = with_tm();
    bank.unregister_batch_with(ue(), &[5], || Ok::<_, ()>(()))
        .unwrap();
    assert!(!bank.has_transparent_domain(ue(), CnDomain::Cs).unwrap());
    assert!(
        bank.register_added_with(
            ue(),
            [join(6, CnDomain::Cs, Some(1), 1024)],
            &[],
            || -> Result<(), ()> { panic!("no established TM bearer remains") }
        )
        .is_err()
    );
    let old_domain = bank.tables.transparent[domain_index(CnDomain::Cs)]
        .as_ref()
        .unwrap()
        .selected(ue(), 1280)
        .unwrap();
    assert!(
        bank.register_added_with(
            ue(),
            [first_tm(6, true, 13, 1024)],
            &[counter(true, 1024, 1280, 14)],
            || Err::<(), _>("full")
        )
        .is_err()
    );
    assert_eq!(
        bank.tables.transparent[domain_index(CnDomain::Cs)]
            .as_ref()
            .unwrap()
            .selected(ue(), 1280)
            .unwrap(),
        old_domain
    );
    bank.register_added_with(
        ue(),
        [first_tm(6, true, 13, 1024)],
        &[counter(true, 1024, 1280, 14)],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    assert_eq!(
        tm_selected(&bank, 6, 1279),
        CipherUse::Data {
            origin: Some(context(CnDomain::Cs, 1).origin()),
            count: 0xd0ff
        }
    );
    assert_eq!(
        tm_selected(&bank, 6, 1280),
        CipherUse::Data {
            origin: Some(context(CnDomain::Cs, 1).origin()),
            count: 0xe000
        }
    );
}

#[test]
fn added_registration_rejects_foreign_owner_signalling_rb_duplicate_capacity_and_bad_context() {
    let mut bank = with_tm();
    let mut foreign = ue();
    foreign.generation = NonZeroU64::new(2).unwrap();
    assert!(bank.has_transparent_domain(foreign, CnDomain::Cs).is_err());
    assert!(
        bank.register_added_with(
            foreign,
            [join(6, CnDomain::Cs, Some(1), 768)],
            &[],
            || -> Result<(), ()> { panic!("foreign owner cannot publish") }
        )
        .is_err()
    );
    assert!(
        bank.register_added_with(
            ue(),
            [join(5, CnDomain::Cs, Some(1), 768)],
            &[],
            || -> Result<(), ()> { panic!("duplicate cannot publish") }
        )
        .is_err()
    );
    let mut configuration = user_configuration(
        6,
        CnDomain::Ps,
        anchor(CipherMode::Acknowledged, 0),
        Some(context(CnDomain::Cs, 1)),
    );
    assert!(
        bank.register_added_with(
            ue(),
            [AddedBearerCipherConfiguration::Initialized(configuration)],
            &[],
            || -> Result<(), ()> { panic!("wrong domain context cannot publish") }
        )
        .is_err()
    );
    configuration = user_configuration(3, CnDomain::Cs, anchor(CipherMode::Acknowledged, 0), None);
    configuration.scope = Scope::Signalling;
    assert!(
        bank.register_added_with(
            ue(),
            [AddedBearerCipherConfiguration::Initialized(configuration)],
            &[],
            || -> Result<(), ()> { panic!("existing signaling must not be recreated") }
        )
        .is_err()
    );
    assert_eq!(bank.len(), 2);
    let mut full = BearerCiphers::new(ue(), NonZeroUsize::MIN).unwrap();
    register(&mut full, 2, Scope::Signalling, CipherMode::Acknowledged);
    assert!(
        full.register_added_with(
            ue(),
            [initialized(5, CnDomain::Ps, CipherMode::Acknowledged, 1)],
            &[],
            || -> Result<(), ()> { panic!("capacity cannot publish") }
        )
        .is_err()
    );
    assert_eq!(full.len(), 1);
}

#[test]
fn added_sequence_registration_requires_new_rlc_zero_and_preserves_context_budget_on_rejection() {
    let mut bank = bank();
    bank.register_added_with(
        ue(),
        [initialized(5, CnDomain::Ps, CipherMode::Acknowledged, 1)],
        &[],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    bank.update_with(
        context(CnDomain::Ps, 2),
        &[
            activation(5, Direction::Uplink, 4096),
            activation(5, Direction::Downlink, 4096),
        ],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let before = selected(&bank, 5, Direction::Uplink, 4097);
    let bad = user_configuration(
        6,
        CnDomain::Ps,
        anchor(CipherMode::Unacknowledged, 1),
        Some(context(CnDomain::Ps, 1)),
    );
    assert!(
        bank.register_added_with(
            ue(),
            [AddedBearerCipherConfiguration::Initialized(bad)],
            &[],
            || -> Result<(), ()> { panic!("nonzero initial RLC sequence cannot publish") }
        )
        .is_err()
    );
    let excess = user_configuration(
        6,
        CnDomain::Ps,
        anchor(CipherMode::Acknowledged, 0),
        Some(context(CnDomain::Ps, 3)),
    );
    assert!(
        bank.register_added_with(
            ue(),
            [AddedBearerCipherConfiguration::Initialized(excess)],
            &[],
            || -> Result<(), ()> { panic!("third retained configuration cannot publish") }
        )
        .is_err()
    );
    assert_eq!(bank.len(), 1);
    assert_eq!(selected(&bank, 5, Direction::Uplink, 4097), before);
}

#[test]
fn nonzero_start_extended_rlc_positions_are_atomic_and_keep_on_air_sequence_zero() {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    let start = Start::new(0x12345).unwrap();
    let initial = u64::from(start.initial_count());
    let additions = || {
        [
            (5, CipherMode::Acknowledged),
            (6, CipherMode::Unacknowledged),
        ]
        .map(|(bearer, mode)| {
            AddedBearerCipherConfiguration::Initialized(user_configuration(
                bearer,
                CnDomain::Ps,
                CountAnchor::from_start(mode, Position::Sequence(initial), start).unwrap(),
                Some(context(CnDomain::Ps, 1)),
            ))
        })
    };
    assert_eq!(
        bank.register_added_with(ue(), additions(), &[], || Err::<(), _>("full")),
        Err(BearerCipherUpdateError::Publication("full"))
    );
    assert_eq!(bank.len(), 1);
    bank.register_added_with(ue(), additions(), &[], || Ok::<_, ()>(()))
        .unwrap();
    for (bearer, mode) in [
        (5, CipherMode::Acknowledged),
        (6, CipherMode::Unacknowledged),
    ] {
        assert_eq!(initial & ((1 << mode.sequence_bits()) - 1), 0);
        for direction in [Direction::Uplink, Direction::Downlink] {
            assert_eq!(
                selected(&bank, bearer, direction, initial + 1),
                CipherUse::Data {
                    origin: Some(context(CnDomain::Ps, 1).origin()),
                    count: start.initial_count() + 1,
                }
            );
        }
        let unrelated = user_configuration(
            7,
            CnDomain::Ps,
            CountAnchor::from_start(mode, Position::Sequence(initial + 4096), start).unwrap(),
            Some(context(CnDomain::Ps, 1)),
        );
        assert!(
            bank.register_added_with(
                ue(),
                [AddedBearerCipherConfiguration::Initialized(unrelated)],
                &[],
                || -> Result<(), ()> {
                    panic!("an unrelated SN-zero epoch cannot initialize this START")
                }
            )
            .is_err()
        );
    }
    assert_eq!(bank.len(), 3);
    assert_eq!(
        selected(&bank, 2, Direction::Uplink, 20),
        CipherUse::Data {
            origin: None,
            count: 20
        }
    );
}
