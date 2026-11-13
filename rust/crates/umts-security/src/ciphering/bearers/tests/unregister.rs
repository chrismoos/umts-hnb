// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const AM_DATA_FLAG: u16 = 0x8000;
const AM_SEQUENCE_SHIFT: u32 = 3;
const AM_SEQUENCE_MASK: u32 = 0x0fff;
const UM_SEQUENCE_MASK: u32 = 0x7f;

fn identity(bank: &BearerCiphers, bearer: u8) -> *const SequenceCiphers {
    match channel(&bank.tables, bearer).unwrap() {
        Channel::Sequenced(value) => value.as_ref() as *const SequenceCiphers,
        Channel::Transparent { .. } => panic!("expected a sequenced bearer"),
    }
}

fn encrypted(
    bank: &BearerCiphers,
    bearer: u8,
    direction: Direction,
    position: Position,
) -> (CipherUse, Vec<u8>) {
    let selected = bank.selected(ue(), bearer, direction, position).unwrap();
    let CipherUse::Data { count, .. } = selected else {
        panic!("expected a data cipher context")
    };
    let (_, mode) = bank.layout(ue(), bearer).unwrap();
    let mut bytes = match mode {
        CipherMode::Acknowledged => {
            let header = (AM_DATA_FLAG
                | (((count & AM_SEQUENCE_MASK) as u16) << AM_SEQUENCE_SHIFT))
                .to_be_bytes();
            vec![header[0], header[1], 0x12, 0x34]
        }
        CipherMode::Unacknowledged => vec![((count & UM_SEQUENCE_MASK) as u8) << 1, 0x12, 0x34],
        CipherMode::Transparent => vec![0x12, 0x34],
    };
    let bits = bytes.len() * u8::BITS as usize;
    assert_eq!(
        bank.apply(ue(), bearer, direction, position, &mut bytes, bits),
        Ok(selected)
    );
    (selected, bytes)
}

fn mixed() -> BearerCiphers {
    let mut bank = bank();
    register(&mut bank, 2, Scope::Signalling, CipherMode::Acknowledged);
    for (bearer, domain, mode) in [
        (5, CnDomain::Cs, CipherMode::Transparent),
        (6, CnDomain::Cs, CipherMode::Transparent),
        (7, CnDomain::Ps, CipherMode::Acknowledged),
        (8, CnDomain::Ps, CipherMode::Unacknowledged),
    ] {
        register(&mut bank, bearer, Scope::User(domain), mode);
    }
    bank.update_with(
        context(CnDomain::Cs, 1),
        &[
            activation(2, Direction::Uplink, 8),
            activation(2, Direction::Downlink, 8),
            CipherActivation::Transparent {
                domain: CnDomain::Cs,
                anchor: anchor(CipherMode::Transparent, 16),
                first_frame: 0,
            },
        ],
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let mut updates = Vec::new();
    for (bearer, mode) in [
        (7, CipherMode::Acknowledged),
        (8, CipherMode::Unacknowledged),
    ] {
        for direction in [Direction::Uplink, Direction::Downlink] {
            updates.push(CipherActivation::Sequenced {
                bearer,
                direction,
                anchor: anchor(mode, 8),
                first_new: Position::Sequence(0),
            });
        }
    }
    bank.update_with(context(CnDomain::Ps, 1), &updates, || Ok::<_, ()>(()))
        .unwrap();
    bank.retire(
        ue(),
        &[CipherRetirement::Sequenced {
            bearer: 2,
            direction: Direction::Downlink,
            floor: Position::Sequence(4),
        }],
    )
    .unwrap();
    bank
}

#[test]
fn removing_middle_channels_preserves_original_cipher_objects_and_history() {
    let mut bank = mixed();
    let survivors = [2, 6, 8];
    let pointers = [identity(&bank, 2), identity(&bank, 8)];
    let before: Vec<_> = survivors
        .into_iter()
        .flat_map(|bearer| {
            [Direction::Uplink, Direction::Downlink]
                .into_iter()
                .flat_map(move |direction| {
                    [4, 7, 8, 16]
                        .into_iter()
                        .map(move |at| (bearer, direction, at))
                })
        })
        .map(|(bearer, direction, at)| {
            let position = if bearer == 6 {
                Position::Frame(at)
            } else {
                Position::Sequence(at)
            };
            (
                bearer,
                direction,
                position,
                encrypted(&bank, bearer, direction, position),
            )
        })
        .collect();
    let pending =
        bank.pending_sequence_activation(ue(), 8, Direction::Uplink, Position::Sequence(4));
    assert_eq!(
        bank.unregister_batch_with(ue(), &[7, 5], || Ok::<_, ()>(17)),
        Ok(17)
    );
    assert_eq!(bank.len(), survivors.len());
    assert_eq!([identity(&bank, 2), identity(&bank, 8)], pointers);
    assert_eq!(bank.tables.index[1], Some(0));
    assert_eq!(bank.tables.index[5], Some(1));
    assert_eq!(bank.tables.index[7], Some(2));
    for (bearer, direction, position, expected) in before {
        assert_eq!(encrypted(&bank, bearer, direction, position), expected);
    }
    assert_eq!(
        bank.pending_sequence_activation(ue(), 8, Direction::Uplink, Position::Sequence(4)),
        pending
    );
    assert_eq!(pending, Ok(Some(Position::Sequence(8))));
    assert_eq!(
        bank.selected(ue(), 2, Direction::Downlink, Position::Sequence(3)),
        Err(BearerCipherError::Cipher(Error::Position))
    );
    for bearer in [5, 7] {
        assert_eq!(
            bank.layout(ue(), bearer),
            Err(BearerCipherError::Cipher(Error::Bearer))
        );
    }
}

#[test]
fn publication_pressure_preserves_all_indices_cipher_bytes_and_pending_updates() {
    let mut bank = mixed();
    let indices = bank.tables.index;
    let pointers = [identity(&bank, 2), identity(&bank, 7), identity(&bank, 8)];
    let before = encrypted(&bank, 5, Direction::Downlink, Position::Frame(16));
    let pending =
        bank.pending_sequence_activation(ue(), 7, Direction::Downlink, Position::Sequence(0));
    for _ in 0..3 {
        assert_eq!(
            bank.unregister_batch_with(ue(), &[5, 7], || Err::<(), _>("full")),
            Err(BearerCipherUpdateError::Publication("full"))
        );
        assert_eq!(bank.len(), 5);
        assert_eq!(bank.tables.index, indices);
        assert_eq!(
            [identity(&bank, 2), identity(&bank, 7), identity(&bank, 8)],
            pointers
        );
        assert_eq!(
            encrypted(&bank, 5, Direction::Downlink, Position::Frame(16)),
            before
        );
        assert_eq!(
            bank.pending_sequence_activation(ue(), 7, Direction::Downlink, Position::Sequence(0)),
            pending
        );
    }
    bank.unregister_batch_with(ue(), &[5, 7], || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(bank.len(), 3);
}

#[test]
fn invalid_batches_never_publish_or_partially_remove_a_user_bearer() {
    let mut bank = mixed();
    for bearers in [
        &[][..],
        &[5, 5],
        &[31],
        &[0],
        &[33],
        &[2],
        &[5, 2],
        &[5, 31],
        &[5; MAX_BEARERS + 1],
    ] {
        assert!(matches!(
            bank.unregister_batch_with(ue(), bearers, || -> Result<(), ()> {
                panic!("invalid removal must not publish")
            }),
            Err(BearerCipherUpdateError::Cipher(_))
        ));
        assert_eq!(bank.len(), 5);
        assert_eq!(
            bank.layout(ue(), 5),
            Ok((Scope::User(CnDomain::Cs), CipherMode::Transparent))
        );
    }
    for foreign in [
        UeKey {
            slot: ue().slot + 1,
            ..ue()
        },
        UeKey {
            generation: NonZeroU64::new(2).unwrap(),
            ..ue()
        },
        UeKey {
            epoch: RadioEpoch::new(NonZeroU64::new(2).unwrap()),
            ..ue()
        },
    ] {
        assert_eq!(
            bank.unregister_batch_with(foreign, &[5], || -> Result<(), ()> {
                panic!("foreign or stale UE must not publish")
            }),
            Err(BearerCipherUpdateError::Cipher(BearerCipherError::Cipher(
                Error::Owner
            )))
        );
        assert_eq!(bank.len(), 5);
    }
}

#[test]
fn last_transparent_removal_keeps_domain_history_and_validates_reregistration_anchor() {
    let mut bank = mixed();
    let expected = encrypted(&bank, 5, Direction::Uplink, Position::Frame(16));
    let domain = domain_index(CnDomain::Cs);
    let origins: Vec<_> = bank.tables.transparent[domain]
        .as_ref()
        .unwrap()
        .origins()
        .collect();
    bank.unregister_batch_with(ue(), &[5, 6], || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(
        bank.tables.transparent[domain]
            .as_ref()
            .unwrap()
            .origins()
            .collect::<Vec<_>>(),
        origins
    );
    assert_eq!(
        bank.update_with(
            context(CnDomain::Cs, 1),
            &[CipherActivation::Transparent {
                domain: CnDomain::Cs,
                anchor: anchor(CipherMode::Transparent, 32),
                first_frame: 16,
            }],
            || -> Result<(), ()> {
                panic!("removed RBs must not reset domain key generation history")
            }
        ),
        Err(BearerCipherUpdateError::Cipher(BearerCipherError::Cipher(
            Error::ReusedGeneration
        )))
    );
    let wrong_anchor = CountAnchor::from_start(
        CipherMode::Transparent,
        Position::Frame(0),
        Start::new(1).unwrap(),
    )
    .unwrap();
    for configuration in [
        user_configuration(5, CnDomain::Cs, wrong_anchor, None),
        user_configuration(
            5,
            CnDomain::Cs,
            anchor(CipherMode::Transparent, 0),
            Some(context(CnDomain::Cs, 2)),
        ),
    ] {
        assert!(
            bank.register_with(configuration, || -> Result<(), ()> {
                panic!("mismatched retained domain must not publish")
            })
            .is_err()
        );
        assert_eq!(bank.len(), 3);
    }
    bank.register_with(
        user_configuration(5, CnDomain::Cs, anchor(CipherMode::Transparent, 0), None),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    assert_eq!(
        encrypted(&bank, 5, Direction::Uplink, Position::Frame(16)),
        expected
    );
    assert_eq!(
        bank.selected(ue(), 5, Direction::Downlink, Position::Frame(7)),
        Ok(CipherUse::Data {
            origin: None,
            count: 7
        })
    );
}

#[test]
fn removal_reuses_bounded_capacity_without_reusing_removed_sequence_context() {
    let mut bank = BearerCiphers::new(ue(), NonZeroUsize::new(2).unwrap()).unwrap();
    let first = CountAnchor::from_start(
        CipherMode::Acknowledged,
        Position::Sequence(0),
        Start::new(3).unwrap(),
    )
    .unwrap();
    bank.register_with(
        user_configuration(5, CnDomain::Ps, first, Some(context(CnDomain::Ps, 1))),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    register(
        &mut bank,
        6,
        Scope::User(CnDomain::Ps),
        CipherMode::Unacknowledged,
    );
    let retained = identity(&bank, 6);
    assert!(matches!(
        bank.register_with(user_configuration(7, CnDomain::Ps, first, None), || Ok::<
            _,
            (),
        >(
            ()
        )),
        Err(BearerCipherUpdateError::Cipher(BearerCipherError::Capacity))
    ));
    bank.unregister_batch_with(ue(), &[5], || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(identity(&bank, 6), retained);
    let next = CountAnchor::from_start(
        CipherMode::Acknowledged,
        Position::Sequence(64),
        Start::new(9).unwrap(),
    )
    .unwrap();
    bank.register_with(
        user_configuration(5, CnDomain::Ps, next, Some(context(CnDomain::Ps, 2))),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    assert_eq!(bank.len(), 2);
    assert_eq!(
        bank.selected(ue(), 5, Direction::Uplink, Position::Sequence(63)),
        Err(BearerCipherError::Cipher(Error::Position))
    );
    assert_eq!(
        bank.selected(ue(), 5, Direction::Uplink, Position::Sequence(64)),
        Ok(CipherUse::Data {
            origin: Some(context(CnDomain::Ps, 2).origin()),
            count: next.count_at(Position::Sequence(64)).unwrap(),
        })
    );
    bank.unregister_batch_with(ue(), &[6, 5], || Ok::<_, ()>(()))
        .unwrap();
    assert!(bank.is_empty());
    assert!(!bank.has_user_bearers(ue()).unwrap());
}
