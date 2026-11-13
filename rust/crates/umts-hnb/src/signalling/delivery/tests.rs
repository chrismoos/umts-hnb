// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;
use crate::{
    signalling::tests::{
        INITIAL, controller, establish, finish_registration, initial_dch_procedure, peer, send,
    },
    test_support::{at, source},
};

#[test]
fn reservation_and_um_abort_are_per_ue_and_abort_publication_is_idempotent() {
    let source = source(false);
    let mut radio = controller(&source);
    let key = establish(&mut radio, &source, 0);
    send(
        &mut radio,
        key,
        SignallingBearer::NasAm,
        &mut peer(&source),
        &INITIAL,
    );
    finish_registration(&mut radio, key);
    assert_eq!(radio.process_one(), Some(Ok(key)));
    radio
        .core()
        .publish_one(|_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let other = establish(&mut radio, &source, 1);
    let identity = crate::dedicated_rx::ReceiveIdentity {
        ue: key,
        configuration: NonZeroU64::new(1).unwrap(),
    };
    let other_identity = crate::dedicated_rx::ReceiveIdentity {
        ue: other,
        configuration: NonZeroU64::new(1).unwrap(),
    };
    let mut procedure = initial_dch_procedure(&radio, key, at(0));
    radio
        .publish_bearer_setup_on(&mut procedure, identity, at(0))
        .unwrap();
    radio.registry.get_mut(key).unwrap().dedicated_route = Some(identity);
    radio.reserve_transition_delivery(identity).unwrap();
    assert_eq!(
        radio.reserved_signalling(identity),
        Some(SignallingBearer::ControlAm)
    );
    assert_eq!(radio.reserved_signalling(other_identity), None);
    radio.abort_transition_delivery(identity).unwrap();
    assert_eq!(
        radio.reserved_signalling(identity),
        Some(SignallingBearer::Unacknowledged)
    );
    assert!(radio.registry.get(key).unwrap().um.publication_ready());
    assert!(!radio.registry.get(other).unwrap().um.publication_ready());
    let transaction = radio
        .registry
        .get(key)
        .unwrap()
        .transactions
        .next(transactions::MessageType::RrcConnectionRelease);
    radio.abort_transition_delivery(identity).unwrap();
    assert_eq!(
        radio
            .registry
            .get(key)
            .unwrap()
            .transactions
            .next(transactions::MessageType::RrcConnectionRelease),
        transaction
    );
}
