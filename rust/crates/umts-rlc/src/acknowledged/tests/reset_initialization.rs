// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

const NEW_COUNTERS: ResetCounters = ResetCounters {
    transmit_hfn: Some(0),
    receive_hfn: Some(0),
};

fn receive(entity: &mut AmEntity, now: u64, bytes: &[u8], counters: ResetCounters) -> AmReceive {
    entity
        .receive_prepared_with_reset(at(now), bytes, |request| {
            assert!(matches!(
                request,
                ReceivePreparation::Reset {
                    transition: Some(_),
                    ..
                }
            ));
            Ok::<_, ()>(PreparedReceive {
                plaintext: bytes,
                reset_counters: Some(counters),
            })
        })
        .unwrap()
}

#[test]
fn pending_cipher_initialization_preserves_transport_order_and_next_reset_hfni() {
    let mut sender = entity(12);
    let mut peer = entity(12);
    sender.reseed_idle_counters(5, 9).unwrap();
    peer.reseed_idle_counters(9, 5).unwrap();
    sender.initiate_reset(at(1)).unwrap();
    let (_, reset) = send(&mut sender, 1).unwrap();
    assert_eq!(parse_reset(&reset).unwrap().hfn, 5);
    assert_eq!(
        receive(&mut peer, 1, &reset, NEW_COUNTERS),
        AmReceive::ResetApplied {
            tx_hfn: 0,
            rx_hfn: 0
        }
    );
    let positions = peer.window_positions();
    assert!(positions.next_transmit >= 10 << SN_BITS);
    assert_eq!(peer.receive_count(0), Ok(0));
    let (_, ack) = send(&mut peer, 1).unwrap();
    assert_eq!(parse_reset(&ack).unwrap().hfn, 9);
    assert_eq!(
        receive(&mut sender, 2, &ack, NEW_COUNTERS),
        AmReceive::ResetAcknowledged {
            tx_hfn: 0,
            rx_hfn: 0
        }
    );
    sender.initiate_reset(at(3)).unwrap();
    let (_, reset) = send(&mut sender, 3).unwrap();
    assert_eq!(parse_reset(&reset).unwrap().hfn, 0);
    assert_eq!(
        peer.receive(at(3), &reset),
        Ok(AmReceive::ResetApplied {
            tx_hfn: 1,
            rx_hfn: 1
        })
    );
    let (_, ack) = send(&mut peer, 3).unwrap();
    assert_eq!(parse_reset(&ack).unwrap().hfn, 0);
    sender.receive(at(4), &ack).unwrap();
    assert_eq!(sender.receive_count(0), Ok(1 << SN_BITS));
    sender.enqueue(b"new key").unwrap();
    let (kind, _) = send(&mut sender, 4).unwrap();
    assert_eq!(
        kind,
        PublicationKind::Data {
            sequence: 0,
            count: 1 << SN_BITS,
            retransmission: false
        }
    );
}

#[test]
fn simultaneous_reset_retains_pending_initialization_until_the_matching_ack() {
    let mut one = entity(12);
    let mut two = entity(12);
    one.reseed_idle_counters(5, 9).unwrap();
    two.reseed_idle_counters(9, 5).unwrap();
    one.initiate_reset(at(1)).unwrap();
    two.initiate_reset(at(1)).unwrap();
    let (_, first) = send(&mut one, 1).unwrap();
    let (_, second) = send(&mut two, 1).unwrap();
    assert_eq!(
        receive(&mut one, 2, &second, NEW_COUNTERS),
        AmReceive::ResetApplied {
            tx_hfn: 0,
            rx_hfn: 0
        }
    );
    assert_eq!(
        receive(&mut two, 2, &first, NEW_COUNTERS),
        AmReceive::ResetApplied {
            tx_hfn: 0,
            rx_hfn: 0
        }
    );
    assert_eq!(one.state(), AmState::ResetPending);
    assert_eq!(two.state(), AmState::ResetPending);
    let (_, first_ack) = send(&mut one, 2).unwrap();
    let (_, second_ack) = send(&mut two, 2).unwrap();
    assert_eq!(parse_reset(&first_ack).unwrap().hfn, 5);
    assert_eq!(parse_reset(&second_ack).unwrap().hfn, 9);
    assert_eq!(
        one.receive(at(3), &second_ack),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 0,
            rx_hfn: 0
        })
    );
    assert_eq!(
        two.receive(at(3), &first_ack),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 0,
            rx_hfn: 0
        })
    );
    assert_eq!(one.state(), AmState::Ready);
    assert_eq!(two.state(), AmState::Ready);
    assert_eq!(one.receive_count(0), Ok(0));
    assert_eq!(two.receive_count(0), Ok(0));
}

#[test]
fn reset_triggered_before_peer_reset_uses_current_hfni_at_its_first_publication() {
    let mut local = entity(12);
    local.reseed_idle_counters(7, 9).unwrap();
    local.initiate_reset(at(1)).unwrap();
    let request = reset(12, false, false, 9);
    receive(&mut local, 2, &request, NEW_COUNTERS);
    let (kind, ack) = send(&mut local, 2).unwrap();
    assert_eq!(kind, PublicationKind::ResetAck);
    assert_eq!(parse_reset(&ack).unwrap().hfn, 7);
    let (kind, request) = send(&mut local, 2).unwrap();
    assert_eq!(kind, PublicationKind::Reset);
    let message = parse_reset(&request).unwrap();
    assert_eq!(message.hfn, 0);
    let response = reset(12, message.sequence, true, 0);
    assert_eq!(
        local.receive(at(3), &response),
        Ok(AmReceive::ResetAcknowledged {
            tx_hfn: 1,
            rx_hfn: 1
        })
    );
}

#[test]
fn reset_counter_overrides_are_directional_and_cannot_change_data_or_duplicate_controls() {
    let mut receiver = entity(12);
    let reset = reset(12, false, false, 7);
    let before = receiver.window_positions();
    assert_eq!(
        receiver.receive_prepared_with_reset(at(1), &reset, |_| Ok::<_, ()>(PreparedReceive {
            plaintext: &reset,
            reset_counters: Some(ResetCounters {
                transmit_hfn: Some(MAX_HFN + 1),
                receive_hfn: None
            }),
        })),
        Err(ReceiveError::Rlc(AmError::CountExhausted))
    );
    assert_eq!(receiver.window_positions(), before);
    assert_eq!(
        receive(
            &mut receiver,
            1,
            &reset,
            ResetCounters {
                transmit_hfn: Some(0),
                receive_hfn: None
            }
        ),
        AmReceive::ResetApplied {
            tx_hfn: 0,
            rx_hfn: 8
        }
    );
    let before = receiver.window_positions();
    assert_eq!(
        receiver.receive_prepared_with_reset(at(2), &reset, |_| Ok::<_, ()>(PreparedReceive {
            plaintext: &reset,
            reset_counters: Some(NEW_COUNTERS),
        })),
        Err(ReceiveError::Rlc(AmError::State))
    );
    assert_eq!(receiver.window_positions(), before);
    let mut sender = entity(12);
    let mut receiver = entity(12);
    sender.enqueue(b"data").unwrap();
    let (_, data) = send(&mut sender, 1).unwrap();
    let before = receiver.window_positions();
    assert_eq!(
        receiver.receive_prepared_with_reset(at(1), &data, |_| Ok::<_, ()>(PreparedReceive {
            plaintext: &data,
            reset_counters: Some(NEW_COUNTERS),
        })),
        Err(ReceiveError::Rlc(AmError::State))
    );
    assert_eq!(receiver.window_positions(), before);
    receiver.receive(at(1), &data).unwrap();
    assert_eq!(drain(&mut receiver), [b"data".to_vec()]);
}
