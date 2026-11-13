// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use std::num::NonZeroU64;
use umts_types::RadioEpoch;

fn owner() -> UeKey {
    UeKey {
        epoch: RadioEpoch::new(NonZeroU64::MIN),
        slot: 3,
        generation: NonZeroU64::MIN,
    }
}
fn router() -> NasRouter {
    NasRouter::new(
        owner(),
        CoreContextId::new(7).unwrap(),
        DomainStarts {
            cs: Some(3),
            ps: Some(9),
        },
        false,
    )
}

#[test]
fn domain_merge_is_atomic_and_preserves_the_other_domain() {
    let mut router = router();
    let updates = DomainStarts {
        cs: Some(17),
        ps: None,
    };
    assert_eq!(
        router.update_starts_with(owner(), updates, || Err::<(), _>("pressure")),
        Err(NasPublishError::Downstream("pressure"))
    );
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(3),
            ps: Some(9)
        }
    );
    assert_eq!(
        router.update_starts_with(owner(), updates, || Ok::<_, ()>(42)),
        Ok(42)
    );
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(17),
            ps: Some(9)
        }
    );
    router
        .update_starts_with(
            owner(),
            DomainStarts {
                cs: None,
                ps: Some(0),
            },
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(17),
            ps: Some(0)
        }
    );
}

#[test]
fn foreign_generation_and_invalid_start_do_not_call_publication() {
    let mut router = router();
    let mut other = owner();
    other.generation = NonZeroU64::new(2).unwrap();
    assert_eq!(
        router.update_starts_with(
            other,
            DomainStarts {
                cs: Some(1),
                ps: None
            },
            || -> Result<(), ()> { panic!("foreign owner") }
        ),
        Err(NasPublishError::Procedure(NasError::Origin))
    );
    for updates in [
        DomainStarts {
            cs: Some(1 << START_BITS),
            ps: None,
        },
        DomainStarts {
            cs: Some(1),
            ps: Some(u32::MAX),
        },
    ] {
        assert_eq!(
            router.update_starts_with(owner(), updates, || -> Result<(), ()> {
                panic!("invalid START")
            }),
            Err(NasPublishError::Procedure(NasError::Message))
        );
    }
    assert_eq!(
        router.starts(),
        DomainStarts {
            cs: Some(3),
            ps: Some(9)
        }
    );
}

#[test]
fn both_domain_boundaries_are_valid_and_empty_updates_do_not_clear() {
    let mut router = router();
    let starts = DomainStarts {
        cs: Some((1 << START_BITS) - 1),
        ps: Some(0),
    };
    router
        .update_starts_with(owner(), starts, || Ok::<_, ()>(()))
        .unwrap();
    router
        .update_starts_with(owner(), DomainStarts::default(), || Ok::<_, ()>(()))
        .unwrap();
    assert_eq!(router.starts(), starts);
}
