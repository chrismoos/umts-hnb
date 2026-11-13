// SPDX-License-Identifier: AGPL-3.0-or-later

use super::*;

#[test]
fn bounded_decode_keeps_old_format_while_next_tti_and_feedback_can_advance() {
    let config = configuration();
    let mut receiver = session(0, 4, Arc::clone(&config));
    receive(&mut receiver, 0, 0, 1, 2, 7).unwrap();
    for _ in 0..2 {
        receiver
            .service_with_work(&mut DecodeWork::new(count(257)))
            .unwrap();
    }
    assert!(receiver.pending_completed_ttis() > 0);
    assert_eq!(receiver.queued(), 0);
    receive(&mut receiver, 1, 1, 1, 1, 7).unwrap();
    for expected in [1, 0, 1, 0] {
        receiver
            .service_with_work(&mut DecodeWork::new(count(257)))
            .unwrap();
        assert_eq!(receiver.decode_cursor, expected);
    }
    let mut deliveries = Vec::new();
    for _ in 0..100 {
        let mut work = DecodeWork::new(count(96 * 1024));
        receiver.service_with_work(&mut work).unwrap();
        assert!(work.remaining() <= 96 * 1024);
        deliveries.extend(consume(&mut receiver));
        if receiver.pending_completed_ttis() == 0 {
            break;
        }
    }
    assert_eq!(receiver.pending_completed_ttis(), 0);
    let packets: Vec<_> = deliveries
        .iter()
        .filter(|(decoded, _)| decoded.transport == id(5))
        .collect();
    assert_eq!(packets.len(), 2);
    for ((decoded, actual), (format, frame, seed)) in
        packets.into_iter().zip([(2, 0, 7), (1, 1, 7)])
    {
        assert_eq!(decoded.identity, identity(0, 1));
        assert_eq!(decoded.format, format);
        assert_eq!(decoded.start, at(frame));
        assert_eq!(*actual, payload(&config, id(5), format, seed));
    }
    assert_eq!(receiver.counters().failed_crc, 0);
}

#[test]
fn retiring_partial_decode_counts_and_cancels_original_owned_jobs_once() {
    let config = configuration();
    let mut receiver = session(0, 4, config);
    receive(&mut receiver, 0, 0, 1, 2, 7).unwrap();
    for _ in 0..2 {
        receiver
            .service_with_work(&mut DecodeWork::new(count(257)))
            .unwrap();
    }
    assert_eq!(receiver.queued(), 0);
    assert_eq!(receiver.pending_completed_ttis(), 1);
    receiver.retire();
    assert_eq!(receiver.pending_completed_ttis(), 0);
    assert_eq!(receiver.counters().retired_completed_ttis, 1);
    receiver.retire();
    assert_eq!(receiver.counters().retired_completed_ttis, 1);
    assert_eq!(receiver.service(), Err(ReceiveError::Retired));
}
