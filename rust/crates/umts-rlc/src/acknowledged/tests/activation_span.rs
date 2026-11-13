// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn isolated_command_span_matches_actual_segmentation_and_deferred_delimiters() {
    for pdu_bytes in [6, 8, 16, 160] {
        for length in 1..=512 {
            let mut configuration = config(pdu_bytes);
            configuration.transmit_window = NonZeroU16::new(512).unwrap();
            configuration.initial_tx_hfn = 3;
            let mut sender = AmEntity::new(configuration, limits(), at(0)).unwrap();
            let before = sender.window_positions();
            let forecast = sender.isolated_sdu_range(length).unwrap();
            assert_eq!(forecast.first, before.next_transmit);
            assert_eq!(sender.window_positions(), before);
            let receipt = sender.enqueue_tracked(&vec![0x5a; length]).unwrap();
            assert_eq!(sender.isolated_sdu_range(1), Err(AmError::State));
            let mut actual_next = forecast.first;
            for tick in 0..512 {
                if !sender.pending_new_data() {
                    break;
                }
                sender
                    .publish_next(at(tick), |publication| {
                        assert_eq!(publication.position, Some(actual_next));
                        actual_next += 1;
                        Ok::<_, ()>(())
                    })
                    .unwrap()
                    .unwrap();
            }
            assert!(!sender.pending_new_data());
            assert_eq!(
                actual_next, forecast.next,
                "PDU bytes={pdu_bytes}, SDU bytes={length}"
            );
            assert_eq!(
                sender.transmission(receipt),
                Ok(SduTransmission::InFlight {
                    first: forecast.first,
                    last: forecast.next - 1,
                    delimited: true
                })
            );
        }
    }
}

#[test]
fn span_prediction_refuses_live_queue_and_reset_state_without_mutation() {
    let mut sender = entity(16);
    assert_eq!(sender.isolated_sdu_range(0), Err(AmError::Length));
    assert_eq!(
        sender.isolated_sdu_range(limits().maximum_sdu_bytes.get() + 1),
        Err(AmError::Capacity)
    );
    sender.enqueue(&[0x42; 14]).unwrap();
    send(&mut sender, 0).unwrap();
    assert_eq!(sender.isolated_sdu_range(1), Err(AmError::State));
    send(&mut sender, 1).unwrap();
    let next = sender.window_positions().next_transmit;
    assert_eq!(
        sender.isolated_sdu_range(1),
        Ok(NewSduRange {
            first: next,
            next: next + 1
        })
    );
    sender.initiate_reset(at(2)).unwrap();
    assert_eq!(sender.isolated_sdu_range(1), Err(AmError::State));
}
