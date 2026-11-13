// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::dedicated_rx::ReceiveIdentity;

#[test]
fn failed_radio_retirement_is_owned_atomic_and_peer_local() {
    let (source, mut radio, key, _) = ready(false);
    let other = establish(&mut radio, &source, 1);
    let identity = ReceiveIdentity {
        ue: key,
        configuration: std::num::NonZeroU64::MIN,
    };
    let wrong = ReceiveIdentity {
        configuration: std::num::NonZeroU64::new(2).unwrap(),
        ..identity
    };
    radio.registry.get_mut(key).unwrap().dedicated_route = Some(identity);
    assert_eq!(
        radio.retire_failed_radio(identity),
        Err(SignallingError::Phase)
    );
    radio.require_dedicated_recovery(identity).unwrap();
    assert_eq!(
        radio.retire_failed_radio(wrong),
        Err(SignallingError::Phase)
    );
    radio.core.clear();
    for _ in 0..4 {
        radio
            .core
            .push_radio_failure(
                UeKey {
                    slot: key.slot + 100,
                    ..key
                },
                Some(CoreContextId::new(999).unwrap()),
                false,
                [None; 2],
            )
            .unwrap();
    }
    let before = radio.resources();
    assert_eq!(
        radio.retire_failed_radio(identity),
        Err(SignallingError::Capacity)
    );
    assert_eq!(radio.resources(), before);
    assert_eq!(radio.dedicated_route(key), Some(identity));
    assert!(radio.phase(other).is_some());
    radio.core.clear();
    assert_eq!(radio.retire_failed_radio(identity), Ok(true));
    assert_eq!(radio.phase(key), None);
    assert!(radio.phase(other).is_some());
    assert_eq!(radio.retire_failed_radio(identity), Ok(false));
    let mut failures = 0;
    radio
        .core
        .publish_one(|event| {
            assert_eq!(event.key, key);
            assert!(matches!(event.kind, CoreEventKind::RadioFailure { .. }));
            failures += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
    assert_eq!(failures, 1);
}
