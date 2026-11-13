// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn transparent_frozen_hfn_tracks_cfn_without_counting_any_frame_wraps() {
    for initial in [0, 19, 255, 256, 100_255] {
        for start in [0, 0x12345, 0xfffff] {
            let anchor =
                CountAnchor::transparent_frozen_hfn(initial, Start::new(start).unwrap()).unwrap();
            for delta in [0, 1, 255, 256, 512, u64::MAX - initial] {
                let frame = initial + delta;
                assert_eq!(
                    anchor.count_at(Position::Frame(frame)),
                    Ok((start << 12) | (frame & 255) as u32)
                );
            }
            if initial != 0 {
                assert_eq!(
                    anchor.count_at(Position::Frame(initial - 1)),
                    Err(Error::Position)
                );
            }
            assert_eq!(
                anchor.count_at(Position::Sequence(initial)),
                Err(Error::Mode)
            );
        }
    }
}

#[test]
fn transparent_stopped_count_does_not_advance_even_the_cfn_component() {
    let anchor = CountAnchor::transparent_stopped(255, Start::new(7).unwrap()).unwrap();
    for frame in [255, 256, 512, u64::MAX] {
        assert_eq!(anchor.count_at(Position::Frame(frame)), Ok(0x70ff));
    }
    let running = CountAnchor::from_start(
        CipherMode::Transparent,
        Position::Frame(255),
        Start::new(7).unwrap(),
    )
    .unwrap();
    assert!(!anchor.agrees_with(running, Position::Frame(255)).unwrap());
}

#[test]
fn key_hfn_reinitialization_preserves_transport_sequence_and_does_not_rewind_windows() {
    for (mode, raw, base) in [
        (
            CipherMode::Acknowledged,
            Position::Sequence(0x98765009),
            0x12345009,
        ),
        (
            CipherMode::Unacknowledged,
            Position::Sequence(0x98765009),
            0x12345009,
        ),
        (
            CipherMode::Transparent,
            Position::Frame(0x98765009),
            0x12345009,
        ),
    ] {
        let anchor = CountAnchor::from_start(mode, raw, Start::new(0x12345).unwrap()).unwrap();
        assert_eq!(anchor.count_at(raw), Ok(base));
        let later = match raw {
            Position::Sequence(value) => Position::Sequence(value + 4096),
            Position::Frame(value) => Position::Frame(value + 4096),
        };
        assert_eq!(anchor.count_at(later), Ok(base + 4096));
        let new_key = CountAnchor::from_start(mode, raw, Start::new(0).unwrap()).unwrap();
        assert_eq!(new_key.count_at(raw), Ok(9));
    }
}

#[test]
fn positions_are_typed_and_count_never_wraps_or_accepts_a_different_wire_sequence() {
    assert_eq!(
        CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(1), 2),
        Err(Error::Sequence)
    );
    assert_eq!(
        CountAnchor::new(CipherMode::Acknowledged, Position::Frame(1), 1),
        Err(Error::Mode)
    );
    assert_eq!(
        CountAnchor::new(CipherMode::Transparent, Position::Sequence(1), 1),
        Err(Error::Mode)
    );
    let anchor =
        CountAnchor::new(CipherMode::Acknowledged, Position::Sequence(4095), u32::MAX).unwrap();
    assert_eq!(anchor.count_at(Position::Sequence(4095)), Ok(u32::MAX));
    assert_eq!(
        anchor.count_at(Position::Sequence(4096)),
        Err(Error::Exhausted)
    );
    assert_eq!(
        anchor.count_at(Position::Sequence(4094)),
        Err(Error::Position)
    );
    assert_eq!(anchor.count_at(Position::Frame(4095)), Err(Error::Mode));
}

#[test]
fn wire_activation_resolves_to_the_nearest_extended_future_occurrence() {
    for (mode, modulus) in [
        (CipherMode::Acknowledged, 4096),
        (CipherMode::Unacknowledged, 128),
        (CipherMode::Transparent, 256),
    ] {
        let position = |value| {
            if mode == CipherMode::Transparent {
                Position::Frame(value)
            } else {
                Position::Sequence(value)
            }
        };
        assert_eq!(
            next_occurrence(mode, 0, position(modulus - 1)),
            Ok(position(modulus))
        );
        assert_eq!(
            next_occurrence(mode, 7, position(modulus + 7)),
            Ok(position(modulus + 7))
        );
        assert_eq!(
            next_occurrence(mode, 6, position(modulus + 7)),
            Ok(position(2 * modulus + 6))
        );
        assert_eq!(
            next_occurrence(mode, modulus as u16, position(0)),
            Err(Error::Sequence)
        );
    }
    assert_eq!(
        next_occurrence(CipherMode::Acknowledged, 0, Position::Sequence(u64::MAX)),
        Err(Error::Exhausted)
    );
}
